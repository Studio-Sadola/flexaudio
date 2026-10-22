//! Scoped Core Audio IOProc registration and strictly validated float-buffer adapters.

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use block2::RcBlock;
use objc2_core_audio::{
    kAudioObjectPropertyElementMain, AudioDeviceCreateIOProcIDWithBlock,
    AudioDeviceDestroyIOProcID, AudioDeviceIOProcID, AudioDeviceStart, AudioDeviceStop,
    AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize, AudioObjectID,
    AudioObjectPropertyAddress,
};
use objc2_core_audio_types::{
    kAudioFormatFlagIsAlignedHigh, kAudioFormatFlagIsBigEndian, kAudioFormatFlagIsFloat,
    kAudioFormatFlagIsNonInterleaved, kAudioFormatFlagIsPacked, kAudioFormatLinearPCM, AudioBuffer,
    AudioBufferList, AudioStreamBasicDescription, AudioTimeStamp,
};

use crate::cleanup::{native_cleanup, NativeCleanup};
use crate::probe::{ProbeCleanup, ProbeControl, ProbeOutcome};

pub(super) type DeviceBlock = RcBlock<
    dyn Fn(
        NonNull<AudioTimeStamp>,
        NonNull<AudioBufferList>,
        NonNull<AudioTimeStamp>,
        NonNull<AudioBufferList>,
        NonNull<AudioTimeStamp>,
    ),
>;

pub(super) fn check(control: &ProbeControl) -> Result<(), ProbeOutcome> {
    if control.cancelled() {
        return Err(ProbeOutcome::Cancelled);
    }
    // Leave cooperative time for synchronous native teardown. No worker is detached to
    // hide an OS call that blocks beyond the deadline; native calls cannot be preempted.
    if control.remaining() <= Duration::from_millis(100)
        || control
            .deadline()
            .saturating_duration_since(std::time::Instant::now())
            <= Duration::from_millis(100)
    {
        return Err(ProbeOutcome::TimedOut);
    }
    Ok(())
}

pub(super) fn failed(context: &'static str, status: i32) -> ProbeOutcome {
    let mut error_context = flexaudio_core::ErrorContext::new(flexaudio_core::Operation::Start);
    if status != 0 {
        error_context = error_context.with_native_status(flexaudio_core::NativeStatus::OsStatus {
            call: context,
            value: status,
        });
    }
    ProbeOutcome::Failed {
        error: flexaudio_core::Error::Backend(format!(
            "self-probe {context} failed; retry capture"
        ))
        .with_context(error_context),
    }
}

pub(super) fn address(selector: u32, scope: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMain,
    }
}

/// The caller supplies initialized, correctly typed property storage. Unknown properties fail.
pub(super) fn read_property<T>(
    object: AudioObjectID,
    selector: u32,
    scope: u32,
    value: &mut T,
) -> Result<(), ProbeOutcome> {
    let address = address(selector, scope);
    let mut bytes =
        u32::try_from(std::mem::size_of::<T>()).map_err(|_| failed("property size", -1))?;
    // SAFETY: address/bytes/value are live writable locals of the advertised size; no qualifier.
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            NonNull::from(&address),
            0,
            std::ptr::null(),
            NonNull::from(&mut bytes),
            NonNull::from(value).cast::<c_void>(),
        )
    };
    if status != 0 || bytes as usize != std::mem::size_of::<T>() {
        return Err(failed("property read", status));
    }
    Ok(())
}

pub(super) fn read_objects(
    object: AudioObjectID,
    selector: u32,
    scope: u32,
) -> Result<Vec<AudioObjectID>, ProbeOutcome> {
    let address = address(selector, scope);
    let mut bytes = 0;
    // SAFETY: address and bytes are valid locals; this array property takes no qualifier.
    let status = unsafe {
        AudioObjectGetPropertyDataSize(
            object,
            NonNull::from(&address),
            0,
            std::ptr::null(),
            NonNull::from(&mut bytes),
        )
    };
    if status != 0 || bytes as usize % std::mem::size_of::<AudioObjectID>() != 0 || bytes > 65_536 {
        return Err(failed("array property size", status));
    }
    if bytes == 0 {
        return Ok(Vec::new());
    }
    let capacity = bytes;
    let mut objects = vec![0; bytes as usize / std::mem::size_of::<AudioObjectID>()];
    // SAFETY: objects is aligned writable storage for capacity bytes; native changes fail safely.
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            NonNull::from(&address),
            0,
            std::ptr::null(),
            NonNull::from(&mut bytes),
            NonNull::new(objects.as_mut_ptr().cast::<c_void>())
                .ok_or_else(|| failed("array storage", -1))?,
        )
    };
    if status != 0 || bytes > capacity || bytes as usize % std::mem::size_of::<AudioObjectID>() != 0
    {
        return Err(failed("array property read", status));
    }
    objects.truncate(bytes as usize / std::mem::size_of::<AudioObjectID>());
    Ok(objects)
}

pub(super) fn float_rate(format: &AudioStreamBasicDescription) -> Result<u32, ProbeOutcome> {
    if format.mFormatID != kAudioFormatLinearPCM
        || format.mFormatFlags & kAudioFormatFlagIsFloat == 0
        || format.mFormatFlags & kAudioFormatFlagIsPacked == 0
        || format.mFormatFlags & kAudioFormatFlagIsAlignedHigh != 0
        || (format.mFormatFlags & kAudioFormatFlagIsBigEndian != 0) != cfg!(target_endian = "big")
        || format.mBitsPerChannel != 32
        || format.mChannelsPerFrame == 0
        || format.mChannelsPerFrame > 64
        || !(8_000.0..=192_000.0).contains(&format.mSampleRate)
        || format.mSampleRate.fract() != 0.0
        || format.mFramesPerPacket != 1
        || format.mBytesPerPacket != format.mBytesPerFrame
        || format.mBytesPerFrame
            != 4 * if format.mFormatFlags & kAudioFormatFlagIsNonInterleaved != 0 {
                1
            } else {
                format.mChannelsPerFrame
            }
    {
        return Err(failed("unsupported or unknown native float format", -1));
    }
    Ok(format.mSampleRate as u32)
}

#[derive(Clone, Copy)]
pub(super) enum IoRole {
    Capture,
    Output,
}

impl IoRole {
    fn cleanup_calls(self) -> (&'static str, &'static str) {
        match self {
            Self::Capture => ("capture IOProc stop", "capture IOProc destruction"),
            Self::Output => ("output IOProc stop", "output IOProc destruction"),
        }
    }
}

/// HAL explicitly Block_copy's the callback and retains that copy until DestroyIOProcID.
/// All captured context is Arc-owned, so even a failed native destroy cannot free live
/// callback data. The gate stays closed; failure makes the probe inconclusive, and Drop
/// retries teardown. An OS refusing destruction can retain native resources; Rust cannot
/// force their release or preempt a synchronous native hang.
pub(super) struct IoRegistration {
    cleanup: Arc<ProbeCleanup>,
    role: IoRole,
    device: AudioObjectID,
    id: AudioDeviceIOProcID,
    gate: Arc<AtomicBool>,
    _block: DeviceBlock,
}

impl IoRegistration {
    #[cfg(any(test, feature = "test-support"))]
    pub(super) fn fixture(control: &ProbeControl, role: IoRole) -> Self {
        Self {
            cleanup: control.cleanup_diagnostics(),
            role,
            device: 1,
            id: Some(crate::test_support::unused_io_proc),
            gate: Arc::new(AtomicBool::new(false)),
            _block: RcBlock::new(|_, _, _, _, _| {}),
        }
    }

    pub(super) fn start(
        device: AudioObjectID,
        block: DeviceBlock,
        gate: Arc<AtomicBool>,
        control: &ProbeControl,
        role: IoRole,
    ) -> Result<Self, ProbeOutcome> {
        check(control)?;
        let mut owner = Self {
            cleanup: control.cleanup_diagnostics(),
            role,
            device,
            id: None,
            gate,
            _block: block,
        };
        // SAFETY: owner retains the block and ID storage until Stop/DestroyIOProcID finishes.
        let status = unsafe {
            AudioDeviceCreateIOProcIDWithBlock(
                NonNull::from(&mut owner.id),
                device,
                None,
                RcBlock::as_ptr(&owner._block),
            )
        };
        if status != 0 || owner.id.is_none() {
            return Err(failed("IOProc creation", status));
        }
        check(control)?;
        // SAFETY: owner holds a registered IOProc for this device, including its callback context.
        let status = unsafe { AudioDeviceStart(device, owner.id) };
        if status != 0 {
            return Err(failed("IOProc start", status));
        }
        check(control)?;
        Ok(owner)
    }

    pub(super) fn record_cleanup(&mut self) -> bool {
        self.gate.store(true, Ordering::Release);
        let Some(id) = self.id else {
            return true;
        };
        // SAFETY: ID belongs to device; callback block/state remain retained until both calls return.
        let stop = native_cleanup(NativeCleanup::ProbeStop, || unsafe {
            AudioDeviceStop(self.device, Some(id))
        });
        // SAFETY: this is this owner's registered IOProc, not shared with another capture.
        let destroy = native_cleanup(NativeCleanup::ProbeIoProc, || unsafe {
            AudioDeviceDestroyIOProcID(self.device, Some(id))
        });
        let (stop_call, destroy_call) = self.role.cleanup_calls();
        self.cleanup.record_native(stop_call, stop);
        self.cleanup.record_native(destroy_call, destroy);
        if destroy == 0 {
            self.id = None;
        }
        stop == 0 && destroy == 0
    }
}

impl Drop for IoRegistration {
    fn drop(&mut self) {
        self.record_cleanup();
    }
}

/// Validate a native float AudioBufferList before constructing any sample slices.
///
/// # Safety
/// `list` must be a valid live Core Audio callback buffer list. Native format was verified f32.
pub(super) unsafe fn buffers<'a>(list: *const AudioBufferList) -> Option<&'a [AudioBuffer]> {
    // SAFETY: caller provides the callback's live list, including its flexible trailing array.
    let list = unsafe { list.as_ref()? };
    let count = list.mNumberBuffers as usize;
    if count == 0 || count > 64 {
        return None;
    }
    // SAFETY: Core Audio guarantees mNumberBuffers entries following the callback list header.
    let buffers = unsafe { std::slice::from_raw_parts(list.mBuffers.as_ptr(), count) };
    if buffers.iter().any(|buffer| {
        buffer.mData.is_null()
            || buffer.mData as usize % std::mem::align_of::<f32>() != 0
            || buffer.mNumberChannels == 0
            || buffer.mDataByteSize == 0
            || buffer.mDataByteSize as usize
                % (std::mem::size_of::<f32>() * buffer.mNumberChannels as usize)
                != 0
    }) {
        return None;
    }
    let frames = buffers[0].mDataByteSize as usize / (4 * buffers[0].mNumberChannels as usize);
    buffers
        .iter()
        .all(|buffer| {
            buffer.mDataByteSize as usize / (4 * buffer.mNumberChannels as usize) == frames
        })
        .then_some(buffers)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cleanup::{inject, CleanupLedger};
    use std::time::Instant;

    #[test]
    fn capture_and_output_cleanup_failures_keep_distinct_roles() {
        for operation in [NativeCleanup::ProbeStop, NativeCleanup::ProbeIoProc] {
            let ledger = Arc::new(CleanupLedger::default());
            let control = ProbeControl::new(
                Instant::now() + Duration::from_secs(1),
                Arc::new(AtomicBool::new(false)),
            )
            .with_cleanup_ledger(ledger.clone());
            inject(operation, || {
                drop(IoRegistration::fixture(&control, IoRole::Capture));
                drop(IoRegistration::fixture(&control, IoRole::Output));
            });
            drop(control);
            let errors = ledger.take();
            assert_eq!(errors.len(), 2);
            assert_ne!(errors[0], errors[1]);
            let calls = if operation == NativeCleanup::ProbeStop {
                ("capture IOProc stop", "output IOProc stop")
            } else {
                ("capture IOProc destruction", "output IOProc destruction")
            };
            for (error, call) in errors.iter().zip([calls.0, calls.1]) {
                assert!(error.to_string().contains(call));
            }
        }
    }
}
