//! Shared implementation for the Process Tap chain. Creates a `CATapDescription` → process tap →
//! private aggregate device → IOProc(block) → start. [`TapChain`] drops them in reverse order.
//!
//! Both the system and process backends use [`build_tap_chain`] and select INCLUDE/EXCLUDE with
//! `TapKind`. The chain itself is shared.
//!
//! # Teardown order
//! `AudioDeviceStop` → `AudioDeviceDestroyIOProcID` →
//! Destroy `AudioHardwareDestroyAggregateDevice` → `AudioHardwareDestroyProcessTap`, then drop
//! the block (`RcBlock`) and `CATapDescription` (`Retained`). The [`TapChain`] field declaration
//! order and `Drop` implementation enforce this sequence.

use std::cell::RefCell;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::AnyThread;
use objc2_core_audio::{
    kAudioAggregateDeviceIsPrivateKey, kAudioAggregateDeviceIsStackedKey,
    kAudioAggregateDeviceNameKey, kAudioAggregateDeviceTapAutoStartKey,
    kAudioAggregateDeviceTapListKey, kAudioAggregateDeviceUIDKey, kAudioSubTapDriftCompensationKey,
    kAudioSubTapUIDKey, AudioDeviceCreateIOProcIDWithBlock, AudioDeviceDestroyIOProcID,
    AudioDeviceIOProcID, AudioDeviceStart, AudioDeviceStop, AudioHardwareCreateAggregateDevice,
    AudioHardwareCreateProcessTap, AudioHardwareDestroyAggregateDevice,
    AudioHardwareDestroyProcessTap, AudioObjectID, CATapDescription,
};
use objc2_core_audio_types::{AudioBufferList, AudioTimeStamp};
use objc2_core_foundation::CFDictionary;
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSObject, NSString};

use flexaudio_core::backend::RawSink;
use flexaudio_core::types::Error;

use crate::common::{
    map_os_status, now_ns, tap_format_is_float, tap_native_format, FALLBACK_FORMAT, NO_ERR,
};

/// Tap kind. INCLUDE = mixdown of the selected processes / EXCLUDE = everything except them.
pub(crate) enum TapKind {
    /// Stereo mixdown including the selected objects (process loopback INCLUDE).
    /// An empty vector is invalid (the caller returns DeviceNotFound).
    IncludeProcesses(Vec<AudioObjectID>),
    /// All system audio (default output) except the selected objects. An empty vector means the
    /// entire system.
    ExcludeProcesses(Vec<AudioObjectID>),
    /// Audio sent to a specific output device, excluding the selected objects. `device_uid` is
    /// that device's UID. Unlike `ExcludeProcesses`, which uses the default output, this variant
    /// restricts capture to one output device.
    ExcludeProcessesOnDevice {
        /// Process objects to exclude (empty means all system audio sent to that device).
        ids: Vec<AudioObjectID>,
        /// UID of the target output device (`kAudioDevicePropertyDeviceUID`).
        device_uid: String,
    },
}

/// A constructed tap chain. `Drop` tears it down in reverse order.
///
/// Match Rust's field drop order (declaration order), and use `Drop` to explicitly release OS
/// resources in Stop → IOProc → aggregate → tap order before dropping `RcBlock` /
/// `Retained<CATapDescription>`.
// `_block`'s `RcBlock<dyn Fn(...)>` mirrors CoreAudio's five-argument IOProc block signature,
// which is complex. A type alias would not improve readability, so allow this lint here, as in
// the Linux backend.
#[allow(clippy::type_complexity)]
pub(crate) struct TapChain {
    /// Aggregate device ID used by the IOProc.
    aggregate_id: AudioObjectID,
    /// Registered IOProc ID (block-driven).
    io_proc_id: AudioDeviceIOProcID,
    /// Process tap ID.
    tap_id: AudioObjectID,
    /// IOProc stop gate. Set `stopped=true` (Release) before calling `AudioDeviceStop`, then load
    /// it with `Acquire` at the start of the IOProc block and return immediately if set. This
    /// fail-safe closes the window where a late in-flight callback could access `RefCell<RawSink>`
    /// after `AudioDeviceStop` returns. Apple does not document that CoreAudio calls the IOProc
    /// on one thread without reentrancy, so guard against it. Shared with the block via `Arc`.
    stopped: Arc<AtomicBool>,
    /// Block passed to the IOProc (must live until `DestroyIOProcID`). Dropped last.
    _block: RcBlock<
        dyn Fn(
            NonNull<AudioTimeStamp>,
            NonNull<AudioBufferList>,
            NonNull<AudioTimeStamp>,
            NonNull<AudioBufferList>,
            NonNull<AudioTimeStamp>,
        ),
    >,
    /// Tap description (kept alive while the aggregate exists). Dropped after the block.
    _desc: Retained<CATapDescription>,
}

// SAFETY: The IDs held by TapChain are `u32` and `Send`. `RcBlock` / `Retained<CATapDescription>`
// are created and dropped on the owner thread (the backend's dedicated thread) and are not
// shared across thread boundaries. The backend (`MacSystemBackend`/`MacProcessBackend`) is `Send`,
// so TapChain itself is designed not to cross threads. Therefore TapChain does not implement
// Send/Sync.

impl Drop for TapChain {
    fn drop(&mut self) {
        // Late-callback guard. Set the stop flag (Release) before calling `AudioDeviceStop`.
        // Even if an in-flight IOProc runs after `AudioDeviceStop` returns, the Acquire load at
        // the start of the block sees this store and returns without touching `RefCell<RawSink>`.
        self.stopped.store(true, Ordering::Release);
        // Teardown order: Stop → DestroyIOProcID → DestroyAggregateDevice → DestroyProcessTap.
        // Ignore failures (best-effort cleanup).
        unsafe {
            if self.io_proc_id.is_some() {
                let _ = AudioDeviceStop(self.aggregate_id, self.io_proc_id);
                let _ = AudioDeviceDestroyIOProcID(self.aggregate_id, self.io_proc_id);
            }
            if self.aggregate_id != 0 {
                let _ = AudioHardwareDestroyAggregateDevice(self.aggregate_id);
            }
            if self.tap_id != 0 {
                let _ = AudioHardwareDestroyProcessTap(self.tap_id);
            }
        }
        // On exit, `_block` → `_desc` are dropped in declaration order.
    }
}

/// Convert `AudioObjectID`s to `NSArray<NSNumber>` (u32 values).
fn object_ids_to_nsarray(ids: &[AudioObjectID]) -> Retained<NSArray<NSNumber>> {
    let numbers: Vec<Retained<NSNumber>> = ids
        .iter()
        .map(|&id| NSNumber::numberWithUnsignedInt(id))
        .collect();
    NSArray::from_retained_slice(&numbers)
}

/// Convert a `&CStr` key (`kAudio…Key` exported by objc2-core-audio) to an `NSString` key.
fn cstr_key(key: &std::ffi::CStr) -> Retained<NSString> {
    NSString::from_str(key.to_str().unwrap_or(""))
}

/// Build the process tap → aggregate device → IOProc → start chain.
///
/// `sink` is moved into the IOProc block and receives [`RawSink::push`] calls from the RT
/// callback. On success, returns [`TapChain`] (drop releases all resources in reverse order). On
/// failure, releases any resources created so far before returning [`Error`].
///
/// # Safety
/// Calls CoreAudio and transfers ownership of `sink` to the block. Uses `RefCell` for interior
/// mutability, assuming the block is called from a single RT thread.
pub(crate) unsafe fn build_tap_chain(
    kind: TapKind,
    name: &str,
    sink: RawSink,
) -> Result<TapChain, Error> {
    // 1) CATapDescription (INCLUDE = mixdown / EXCLUDE = global-but-exclude /
    //    ExcludeOnDevice = exclude audio sent to a specific output device).
    let desc: Retained<CATapDescription> = match &kind {
        TapKind::IncludeProcesses(ids) => {
            let arr = object_ids_to_nsarray(ids);
            CATapDescription::initStereoMixdownOfProcesses(CATapDescription::alloc(), &arr)
        }
        TapKind::ExcludeProcesses(ids) => {
            let arr = object_ids_to_nsarray(ids);
            CATapDescription::initStereoGlobalTapButExcludeProcesses(
                CATapDescription::alloc(),
                &arr,
            )
        }
        TapKind::ExcludeProcessesOnDevice { ids, device_uid } => {
            let arr = object_ids_to_nsarray(ids);
            let uid = NSString::from_str(device_uid);
            // Stream 0 is the device's first output stream. The tap format follows this stream.
            // Restrict the destination to the device with `device_uid` and mix down its audio,
            // excluding `ids`.
            CATapDescription::initExcludingProcesses_andDeviceUID_withStream(
                CATapDescription::alloc(),
                &arr,
                &uid,
                0,
            )
        }
    };
    desc.setName(&NSString::from_str(name));
    desc.setPrivate(true);
    // Tap UUID string used for the aggregate's sub-tap UID.
    let uuid_str: Retained<NSString> = desc.UUID().UUIDString();

    // 2) Create the process tap.
    let mut tap_id: AudioObjectID = 0;
    let status = AudioHardwareCreateProcessTap(Some(&desc), &mut tap_id as *mut AudioObjectID);
    if status != NO_ERR {
        return Err(map_os_status("AudioHardwareCreateProcessTap", status));
    }
    if tap_id == 0 {
        return Err(Error::Backend(
            "AudioHardwareCreateProcessTap returned null tap id".into(),
        ));
    }

    // Log the tap's native format (rate/channels) for debugging. Stream's native_format uses the
    // backend fallback during construction, so this read is informational only.
    if std::env::var_os("FLEXAUDIO_DEBUG").is_some() {
        match tap_native_format(tap_id) {
            Some((rate, ch)) => eprintln!(
                "[flexaudio-os-macos] tap ASBD: rate={rate} channels={ch} (fallback would be {FALLBACK_FORMAT:?})"
            ),
            None => eprintln!(
                "[flexaudio-os-macos] tap ASBD unavailable; using fallback {FALLBACK_FORMAT:?}"
            ),
        }
    }

    // Check the ASBD float bit. The IOProc reads mData as *const f32, so non-float samples could
    // cause UB. Reject with a Backend error only when the float bit is confirmed absent.
    // If the ASBD cannot be read (None), the format is unknown, so assume float and continue
    // (real hardware taps are always float).
    if let Some(false) = tap_format_is_float(tap_id) {
        let _ = unsafe { AudioHardwareDestroyProcessTap(tap_id) };
        return Err(Error::Backend(
            "tap format is not float (kAudioFormatFlagIsFloat unset); IOProc reads f32".into(),
        ));
    }

    // 3) Create the private aggregate device. On failure, destroy the tap before returning.
    let aggregate_id = match create_aggregate_device(name, &uuid_str) {
        Ok(id) => id,
        Err(e) => {
            let _ = AudioHardwareDestroyProcessTap(tap_id);
            return Err(e);
        }
    };

    // 4) Create the IOProc block. Move `sink` into the block (interior mutability via RefCell).
    //    The block is assumed to be called from a single RT thread.
    let sink_cell = RefCell::new(sink);

    // Preallocate the largest expected planar→interleaved scratch buffer during setup (outside
    // RT). This avoids the first or growth heap allocation in the IOProc. Estimate capacity from
    // the tap's native format (or FALLBACK if unavailable) for about 100 ms of frames × channels.
    // IOProc buffers are usually 10–20 ms, so resize is a no-op within capacity during steady
    // state (and remains safe if it grows beyond that). Move it into a `RefCell<Vec<f32>>` owned
    // only by the block; do not use thread_local, so it stays alive with the block even when the
    // owner and RT threads differ.
    let (native_rate, native_ch) = tap_native_format(tap_id).unwrap_or(FALLBACK_FORMAT);
    let max_scratch = ((native_rate as usize / 10).max(1)) * (native_ch as usize).max(1);
    let scratch_cell = RefCell::new({
        let mut v: Vec<f32> = Vec::new();
        v.reserve_exact(max_scratch);
        v
    });

    // Stop flag for the late-callback guard (shared by the block and TapChain). stop/Drop sets it
    // to true (Release) before `AudioDeviceStop`; the block loads it with Acquire at entry.
    let stopped = Arc::new(AtomicBool::new(false));
    let stopped_for_block = stopped.clone();

    let block = RcBlock::new(
        move |_in_now: NonNull<AudioTimeStamp>,
              in_input: NonNull<AudioBufferList>,
              _in_input_time: NonNull<AudioTimeStamp>,
              _out: NonNull<AudioBufferList>,
              _out_time: NonNull<AudioTimeStamp>| {
            // CoreAudio calls this block as an FFI boundary callback. A panic crossing the
            // boundary is UB, so wrap the body in catch_unwind to prevent a panic from
            // RawSink::push or elsewhere from unwinding into CoreAudio.
            let _ = catch_unwind(AssertUnwindSafe(|| {
                // Late-callback guard. Load with Acquire the stop flag set by stop/Drop before
                // `AudioDeviceStop`. If set, return without touching `RefCell<RawSink>` to prevent
                // an in-flight callback from accessing the sink after `AudioDeviceStop` returns.
                if stopped_for_block.load(Ordering::Acquire) {
                    return;
                }
                // RT callback. If borrowing fails (reentrancy), do nothing.
                if let Ok(mut sink) = sink_cell.try_borrow_mut() {
                    if let Ok(mut scratch) = scratch_cell.try_borrow_mut() {
                        // SAFETY: `in_input` is a valid AudioBufferList provided by CoreAudio.
                        unsafe { push_buffer_list(&mut sink, &mut scratch, in_input.as_ptr()) };
                    }
                }
            }));
        },
    );

    // 5) Register the IOProc (`queue=None` uses the device's default RT thread).
    let mut io_proc_id: AudioDeviceIOProcID = None;
    let status = AudioDeviceCreateIOProcIDWithBlock(
        NonNull::from(&mut io_proc_id),
        aggregate_id,
        None,
        // AudioDeviceIOBlock = *mut DynBlock<...>. Pass RcBlock as a raw DynBlock pointer.
        RcBlock::as_ptr(&block),
    );
    if status != NO_ERR || io_proc_id.is_none() {
        let _ = AudioHardwareDestroyAggregateDevice(aggregate_id);
        let _ = AudioHardwareDestroyProcessTap(tap_id);
        return Err(map_os_status("AudioDeviceCreateIOProcIDWithBlock", status));
    }

    // 6) Start.
    let status = AudioDeviceStart(aggregate_id, io_proc_id);
    if status != NO_ERR {
        // Mark stopped before teardown so a late IO callback becomes a no-op (ported from rodrigoaddor/flexaudio@671d294).
        stopped.store(true, Ordering::Release);
        let _ = AudioDeviceDestroyIOProcID(aggregate_id, io_proc_id);
        let _ = AudioHardwareDestroyAggregateDevice(aggregate_id);
        let _ = AudioHardwareDestroyProcessTap(tap_id);
        return Err(map_os_status("AudioDeviceStart", status));
    }

    Ok(TapChain {
        aggregate_id,
        io_proc_id,
        tap_id,
        stopped,
        _block: block,
        _desc: desc,
    })
}

/// Create a private aggregate device and return its `AudioObjectID`.
///
/// Dictionary passed:
/// `{ Name, UID(generated UUID), IsPrivate:true, IsStacked:false, TapAutoStart:true,
///    TapList:[{SubTapUID: tap UUID, SubTapDriftCompensation:true}] }`.
/// Build it with NSDictionary and pass it as `&CFDictionary` via toll-free bridging.
fn create_aggregate_device(name: &str, sub_tap_uid: &NSString) -> Result<AudioObjectID, Error> {
    // Sub-tap dictionary: { uid: <tap uuid>, drift: true }.
    let drift_true = NSNumber::numberWithBool(true);
    let sub_tap: Retained<NSDictionary<NSString, NSObject>> = NSDictionary::from_slices::<NSString>(
        &[
            &cstr_key(kAudioSubTapUIDKey),
            &cstr_key(kAudioSubTapDriftCompensationKey),
        ],
        &[sub_tap_uid.as_ref(), drift_true.as_ref()],
    );
    let tap_list: Retained<NSArray<NSObject>> =
        NSArray::from_retained_slice(&[Retained::into_super(sub_tap)]);

    // Aggregate's own UID (a unique UUID string).
    let agg_uid = NSString::from_str(&new_uuid_string());
    let agg_name = NSString::from_str(name);
    let is_private = NSNumber::numberWithBool(true);
    let is_stacked = NSNumber::numberWithBool(false);
    let tap_auto_start = NSNumber::numberWithBool(true);

    let keys: [&NSString; 6] = [
        &cstr_key(kAudioAggregateDeviceNameKey),
        &cstr_key(kAudioAggregateDeviceUIDKey),
        &cstr_key(kAudioAggregateDeviceIsPrivateKey),
        &cstr_key(kAudioAggregateDeviceIsStackedKey),
        &cstr_key(kAudioAggregateDeviceTapAutoStartKey),
        &cstr_key(kAudioAggregateDeviceTapListKey),
    ];
    let values: [&NSObject; 6] = [
        agg_name.as_ref(),
        agg_uid.as_ref(),
        is_private.as_ref(),
        is_stacked.as_ref(),
        tap_auto_start.as_ref(),
        tap_list.as_ref(),
    ];
    let dict: Retained<NSDictionary<NSString, NSObject>> =
        NSDictionary::from_slices::<NSString>(&keys, &values);

    // SAFETY: NSDictionary and CFDictionary are toll-free bridged (the same ObjC object), so the
    // pointer can be read as `&CFDictionary`. `dict` lives to the end of this function, keeping
    // the pointer valid.
    let cf: &CFDictionary = unsafe { &*(Retained::as_ptr(&dict) as *const CFDictionary) };

    let mut device_id: AudioObjectID = 0;
    // SAFETY: `cf` is a valid CFDictionary and `device_id` is a valid local.
    let status = unsafe { AudioHardwareCreateAggregateDevice(cf, NonNull::from(&mut device_id)) };
    if status != NO_ERR {
        return Err(map_os_status("AudioHardwareCreateAggregateDevice", status));
    }
    if device_id == 0 {
        return Err(Error::Backend(
            "AudioHardwareCreateAggregateDevice returned null device id".into(),
        ));
    }
    Ok(device_id)
}

/// Generate a unique UUID string (for the aggregate UID).
fn new_uuid_string() -> String {
    use objc2_foundation::NSUUID;
    NSUUID::new().UUIDString().to_string()
}

/// Send the IOProc's `AudioBufferList` to [`RawSink::push`] as interleaved f32.
///
/// - Interleaved (`mNumberBuffers == 1`): push as is.
/// - Planar (`mNumberBuffers >= 2`): interleave each frame as L,R,L,R… and push (reuse the
///   preallocated `scratch` Vec to avoid allocations).
/// - Treat size 0 / null as silence and do not push.
///
/// `scratch` is a Vec allocated by the block during setup (outside RT). During steady state,
/// `resize` is a no-op within capacity, avoiding heap allocations on the RT path (it only grows
/// if capacity is exceeded).
///
/// # Safety
/// `list` must point to a valid `AudioBufferList` (provided by CoreAudio to the IOProc).
unsafe fn push_buffer_list(
    sink: &mut RawSink,
    scratch: &mut Vec<f32>,
    list: *const AudioBufferList,
) {
    if list.is_null() {
        return;
    }
    let num_buffers = (*list).mNumberBuffers as usize;
    if num_buffers == 0 {
        return;
    }
    // Log whether buffers are interleaved or planar once (when FLEXAUDIO_DEBUG is set).
    log_buffer_shape_once(num_buffers);
    // mBuffers is the start of a variable-length array. Read `num_buffers` entries as a slice.
    let buffers = std::slice::from_raw_parts((*list).mBuffers.as_ptr(), num_buffers);

    if num_buffers == 1 {
        // Interleaved: push as f32 without conversion.
        let buf = &buffers[0];
        let n = buf.mDataByteSize as usize / core::mem::size_of::<f32>();
        if n == 0 || buf.mData.is_null() {
            return;
        }
        let slice = std::slice::from_raw_parts(buf.mData as *const f32, n);
        sink.push(slice, now_ns());
        return;
    }

    // Planar: each buffer holds one channel. Use the shortest buffer's frame count.
    let channels = num_buffers;
    let mut min_frames = usize::MAX;
    for b in buffers.iter() {
        if b.mData.is_null() {
            return;
        }
        let frames = b.mDataByteSize as usize / core::mem::size_of::<f32>();
        min_frames = min_frames.min(frames);
    }
    if min_frames == 0 || min_frames == usize::MAX {
        return;
    }

    // Reuse preallocated scratch to interleave (avoiding allocations on the RT path).
    // Since channels == num_buffers == buffers.len(), enumerate `buffers` directly.
    let total = min_frames * channels;
    scratch.resize(total, 0.0);
    for (ch, buf) in buffers.iter().enumerate() {
        let src = std::slice::from_raw_parts(buf.mData as *const f32, min_frames);
        let mut idx = ch;
        for &s in src.iter() {
            scratch[idx] = s;
            idx += channels;
        }
    }
    sink.push(&scratch[..total], now_ns());
}

thread_local! {
    /// RT-thread-local flag to log the buffer layout only once.
    static LOGGED_SHAPE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// On the first IOProc callback only, log to stderr whether buffers are interleaved
/// (mNumberBuffers==1) or planar (>=2), when `FLEXAUDIO_DEBUG` is set.
fn log_buffer_shape_once(num_buffers: usize) {
    if std::env::var_os("FLEXAUDIO_DEBUG").is_none() {
        return;
    }
    LOGGED_SHAPE.with(|c| {
        if !c.get() {
            c.set(true);
            let kind = if num_buffers == 1 {
                "interleaved"
            } else {
                "planar"
            };
            eprintln!(
                "[flexaudio-os-macos] IOProc buffer shape: mNumberBuffers={num_buffers} ({kind})"
            );
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `new_uuid_string` returns a 36-character UUID (8-4-4-4-12).
    #[test]
    fn uuid_string_has_expected_shape() {
        let s = new_uuid_string();
        assert_eq!(s.len(), 36);
        assert_eq!(s.matches('-').count(), 4);
    }

    /// `object_ids_to_nsarray` preserves the element count.
    #[test]
    fn object_ids_array_preserves_count() {
        let arr = object_ids_to_nsarray(&[1, 2, 3]);
        assert_eq!(arr.count(), 3);
    }
}
