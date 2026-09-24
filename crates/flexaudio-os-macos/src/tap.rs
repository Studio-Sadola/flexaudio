//! Shared implementation of the Process Tap chain. Builds `CATapDescription` → process tap →
//! private aggregate device → IOProc(block) → start; teardown is done in reverse order by
//! the `Drop` of [`TapChain`].
//!
//! The system / process backends both just call this [`build_tap_chain`], switching
//! INCLUDE/EXCLUDE with `TapKind`. The chain itself is shared.
//!
//! # Teardown order
//! Cleans up in the order `AudioDeviceStop` → `AudioDeviceDestroyIOProcID` →
//! `AudioHardwareDestroyAggregateDevice` → `AudioHardwareDestroyProcessTap`, and finally the
//! block (`RcBlock`) and the `CATapDescription` (`Retained`) are dropped. This order is
//! enforced by the field declaration order of [`TapChain`] and its `Drop` impl.

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

/// Tap kind. INCLUDE = a mixdown of the given processes / EXCLUDE = everything except the
/// given processes.
pub(crate) enum TapKind {
    /// Stereo mixdown including the given objects (process loopback INCLUDE).
    /// An empty vec is invalid (the caller returns DeviceNotFound).
    IncludeProcesses(Vec<AudioObjectID>),
    /// All system audio except the given objects (default output). An empty vec means the
    /// whole system.
    ExcludeProcesses(Vec<AudioObjectID>),
    /// Audio going to a specific output device, except the given objects. `device_uid` is
    /// that device's UID. Unlike `ExcludeProcesses`, which is for the default output, this
    /// narrows the output destination to one device.
    ExcludeProcessesOnDevice {
        /// Process objects to exclude (if empty, all system audio going to that device).
        ids: Vec<AudioObjectID>,
        /// UID of the target output device (`kAudioDevicePropertyDeviceUID`).
        device_uid: String,
    },
}

/// A built tap chain. Torn down in reverse order on `Drop`.
///
/// The field declaration order matches Rust's drop order (declaration order), and the `Drop`
/// impl explicitly cleans up the OS resources in the order Stop → IOProc → aggregate → tap
/// before letting `RcBlock` / `Retained<CATapDescription>` drop.
// The `RcBlock<dyn Fn(...)>` of `_block` mirrors CoreAudio's IOProc block signature (5
// arguments) as is, so it is complex. A type alias would not make it more readable, so, as
// in the Linux backend, the lint is allowed only here.
#[allow(clippy::type_complexity)]
pub(crate) struct TapChain {
    /// ID of the aggregate device the IOProc runs on.
    aggregate_id: AudioObjectID,
    /// Registered IOProc ID (block-driven).
    io_proc_id: AudioDeviceIOProcID,
    /// Process tap ID.
    tap_id: AudioObjectID,
    /// IOProc stop gate. Set to `stopped=true` (Release) before calling `AudioDeviceStop`; the
    /// IOProc block does an `Acquire` load at its start and returns immediately if it is set.
    /// A failsafe that closes the window in which a late callback still in flight after
    /// `AudioDeviceStop` returns could touch the `RefCell<RawSink>`. Apple does not document
    /// that CoreAudio calls the IOProc on a single thread without reentrancy, so this is
    /// belt-and-braces. Shared with the block via `Arc`.
    stopped: Arc<AtomicBool>,
    /// The block passed to the IOProc (must stay alive until `DestroyIOProcID`). Dropped
    /// last.
    _block: RcBlock<
        dyn Fn(
            NonNull<AudioTimeStamp>,
            NonNull<AudioBufferList>,
            NonNull<AudioTimeStamp>,
            NonNull<AudioBufferList>,
            NonNull<AudioTimeStamp>,
        ),
    >,
    /// Tap description (kept while the aggregate is alive). Dropped after the block.
    _desc: Retained<CATapDescription>,
}

// SAFETY: The ids held by TapChain are u32 and Send. `RcBlock` / `Retained<CATapDescription>`
// are created and dropped within the owning thread (the backend's dedicated thread) and are
// never shared across a thread boundary. The backends themselves
// (`MacSystemBackend`/`MacProcessBackend`) are `Send`, and TapChain itself is designed never
// to cross threads. Therefore no Send/Sync is declared for TapChain.

impl Drop for TapChain {
    fn drop(&mut self) {
        // Late-callback guard. Set the stop flag (Release) before calling `AudioDeviceStop`.
        // Even if an IOProc that was in flight runs after `AudioDeviceStop` returns, the
        // `Acquire` load at the start of the block sees this store and returns immediately
        // without touching the `RefCell<RawSink>`.
        self.stopped.store(true, Ordering::Release);
        // Teardown order: Stop → DestroyIOProcID → DestroyAggregateDevice → DestroyProcessTap.
        // Failures are ignored (best-effort cleanup).
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
        // Leaving here drops _block → _desc in declaration order.
    }
}

/// Converts `AudioObjectID`s into an `NSArray<NSNumber>` (u32 values).
fn object_ids_to_nsarray(ids: &[AudioObjectID]) -> Retained<NSArray<NSNumber>> {
    let numbers: Vec<Retained<NSNumber>> = ids
        .iter()
        .map(|&id| NSNumber::numberWithUnsignedInt(id))
        .collect();
    NSArray::from_retained_slice(&numbers)
}

/// Converts a `&CStr` key (a `kAudio…Key` exported by objc2-core-audio) into an `NSString`
/// key.
fn cstr_key(key: &std::ffi::CStr) -> Retained<NSString> {
    NSString::from_str(key.to_str().unwrap_or(""))
}

/// Builds process tap → aggregate device → IOProc → start.
///
/// `sink` is moved into the IOProc block and [`RawSink::push`] is called from the RT
/// callback. On success, returns a [`TapChain`] (dropping it tears down all resources in
/// reverse order). On failure, tears down the resources built so far on the spot and then
/// returns an [`Error`].
///
/// # Safety
/// Calls CoreAudio. Transfers ownership of `sink` to the block. Uses `RefCell` interior
/// mutability on the assumption that the block is called only from a single RT thread.
pub(crate) unsafe fn build_tap_chain(
    kind: TapKind,
    name: &str,
    sink: RawSink,
) -> Result<TapChain, Error> {
    // 1) CATapDescription (INCLUDE = mixdown / EXCLUDE = global-but-exclude /
    //    ExcludeOnDevice = exclude, targeting a specific output device).
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
            // stream 0 = the device's first output stream. The tap format follows this stream.
            // Narrow the output destination to the device_uid device and mix down the audio
            // going to that device, excluding ids.
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
    // The tap's UUID string, used as the aggregate's sub-tap UID.
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

    // Debug-print the tap's native format (rate/channels). The Stream's native_format uses
    // the backend's fallback value at construction, so this read is informational only.
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

    // Check the ASBD's float bit. The IOProc reads mData as *const f32, so non-float samples
    // could cause UB. Reject with a Backend error only when the float bit is confirmed unset.
    // If the ASBD cannot be obtained (None), it cannot be decided, so continue assuming float
    // (taps on real hardware are always float).
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

    // 4) Create the IOProc block. Move sink into the block (interior mutability via RefCell).
    //    Assumes the block is called only from a single RT thread.
    let sink_cell = RefCell::new(sink);

    // Allocate the scratch for planar → interleaved at the maximum expected length during
    // setup (non-RT). This avoids first-time/growth heap allocations inside the IOProc. The
    // capacity allows for ~100 ms of frames × ch based on the tap's native format (FALLBACK if
    // unavailable). IOProc buffers are normally 10–20 ms, so in steady state resize is an
    // in-capacity no-op (exceeding it just grows the buffer, which is safe). It is moved in as
    // a `RefCell<Vec<f32>>` owned solely by the block, and thread_local is not used, so that
    // it lives correctly together with the block even if the owning thread and the RT thread
    // differ.
    let (native_rate, native_ch) = tap_native_format(tap_id).unwrap_or(FALLBACK_FORMAT);
    let max_scratch = ((native_rate as usize / 10).max(1)) * (native_ch as usize).max(1);
    let scratch_cell = RefCell::new({
        let mut v: Vec<f32> = Vec::new();
        v.reserve_exact(max_scratch);
        v
    });

    // Stop flag for the late-callback guard (shared by the block and TapChain). stop/Drop
    // sets it to true (Release) before `AudioDeviceStop`, and the block does an Acquire load
    // at its start.
    let stopped = Arc::new(AtomicBool::new(false));
    let stopped_for_block = stopped.clone();

    let block = RcBlock::new(
        move |_in_now: NonNull<AudioTimeStamp>,
              in_input: NonNull<AudioBufferList>,
              _in_input_time: NonNull<AudioTimeStamp>,
              _out: NonNull<AudioBufferList>,
              _out_time: NonNull<AudioTimeStamp>| {
            // This block is an FFI-boundary callback invoked by CoreAudio. A panic crossing the
            // boundary is UB, so the whole body is wrapped in catch_unwind so that even if
            // RawSink::push or similar ever panics, the unwind does not propagate into
            // CoreAudio.
            let _ = catch_unwind(AssertUnwindSafe(|| {
                // Late-callback guard. Acquire-load the stop flag that stop/Drop set before
                // `AudioDeviceStop`. If it is set, return immediately without touching the
                // `RefCell<RawSink>`, closing the window in which an in-flight callback after
                // AudioDeviceStop returns could touch the sink.
                if stopped_for_block.load(Ordering::Acquire) {
                    return;
                }
                // RT callback. If borrowing fails (reentrancy), do nothing.
                if let Ok(mut sink) = sink_cell.try_borrow_mut() {
                    if let Ok(mut scratch) = scratch_cell.try_borrow_mut() {
                        // SAFETY: in_input is a valid AudioBufferList (supplied by CoreAudio).
                        unsafe { push_buffer_list(&mut sink, &mut scratch, in_input.as_ptr()) };
                    }
                }
            }));
        },
    );

    // 5) Register the IOProc (queue=None uses the device's default RT thread).
    let mut io_proc_id: AudioDeviceIOProcID = None;
    let status = AudioDeviceCreateIOProcIDWithBlock(
        NonNull::from(&mut io_proc_id),
        aggregate_id,
        None,
        // AudioDeviceIOBlock = *mut DynBlock<...>. Pass the RcBlock as a raw DynBlock pointer.
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

/// Creates a private aggregate device and returns its `AudioObjectID`.
///
/// The dictionary passed:
/// `{ Name, UID(generated UUID), IsPrivate:true, IsStacked:false, TapAutoStart:true,
///    TapList:[{SubTapUID: tap UUID, SubTapDriftCompensation:true}] }`.
/// Built as an NSDictionary and passed as a `&CFDictionary` via toll-free bridging.
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

    // The aggregate's own UID (a unique UUID string).
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

    // SAFETY: NSDictionary and CFDictionary are toll-free bridged (the same ObjC object), so
    // the pointer can be read as a &CFDictionary. dict lives until the end of this function,
    // and the pointer is valid during that time.
    let cf: &CFDictionary = unsafe { &*(Retained::as_ptr(&dict) as *const CFDictionary) };

    let mut device_id: AudioObjectID = 0;
    // SAFETY: cf is a valid CFDictionary, and device_id is a valid local.
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

/// Generates a unique UUID string (for the aggregate's UID).
fn new_uuid_string() -> String {
    use objc2_foundation::NSUUID;
    NSUUID::new().UUIDString().to_string()
}

/// Feeds the `AudioBufferList` passed to the IOProc to [`RawSink::push`] as interleaved f32.
///
/// - interleaved (`mNumberBuffers == 1`): pushed as is.
/// - planar (`mNumberBuffers >= 2`): interleaved per frame into L,R,L,R… and pushed (reuses
///   the preallocated `scratch` Vec to avoid allocation).
/// - size 0 / null is treated as silence and not pushed.
///
/// `scratch` is a Vec allocated by the block during setup (non-RT). In steady state `resize`
/// is an in-capacity no-op, so no heap allocation happens on the RT path (exceeding the
/// capacity just grows it).
///
/// # Safety
/// `list` must point to a valid `AudioBufferList` (supplied by CoreAudio to the IOProc).
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
    // Debug-print interleaved vs. planar once (when FLEXAUDIO_DEBUG is set).
    log_buffer_shape_once(num_buffers);
    // mBuffers is the start of a variable-length array. Read num_buffers entries as a
    // slice.
    let buffers = std::slice::from_raw_parts((*list).mBuffers.as_ptr(), num_buffers);

    if num_buffers == 1 {
        // interleaved: push as f32 as is.
        let buf = &buffers[0];
        let n = buf.mDataByteSize as usize / core::mem::size_of::<f32>();
        if n == 0 || buf.mData.is_null() {
            return;
        }
        let slice = std::slice::from_raw_parts(buf.mData as *const f32, n);
        sink.push(slice, now_ns());
        return;
    }

    // planar: each buffer = 1 ch. The frame count follows the smallest buffer.
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

    // Interleave by reusing the preallocated scratch (avoids allocation on the RT path).
    // channels == num_buffers == buffers.len(), so enumerate buffers directly.
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
    /// Flag that limits the buffer-shape debug output to once (RT thread-local).
    static LOGGED_SHAPE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// On the first IOProc callback, prints once to stderr whether the buffers are interleaved
/// (mNumberBuffers==1) / planar (>=2), when `FLEXAUDIO_DEBUG` is set.
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

    /// object_ids_to_nsarray preserves the element count.
    #[test]
    fn object_ids_array_preserves_count() {
        let arr = object_ids_to_nsarray(&[1, 2, 3]);
        assert_eq!(arr.count(), 3);
    }
}
