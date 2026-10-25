//! Allocation-free decoding and failure latches for the native IOProc.
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use flexaudio_core::backend::RawSink;
use flexaudio_core::{CaptureDiagnostics, Error, ErrorContext, Operation};
use objc2_core_audio_types::AudioBufferList;

use crate::capture_health::SampleMailbox;
use crate::common::now_ns;

pub(super) struct BufferReport {
    diagnostics: CaptureDiagnostics,
    advertised: (u32, u16),
    actual_channels: AtomicU32,
    reported: AtomicBool,
    busy: AtomicBool,
}

pub(super) struct CallbackGuard<'a>(&'a AtomicBool);
impl Drop for CallbackGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl BufferReport {
    pub(super) fn new(sink: &RawSink) -> Self {
        Self {
            diagnostics: sink.diagnostics(),
            advertised: (sink.native_rate(), sink.native_channels()),
            actual_channels: AtomicU32::new(0),
            reported: AtomicBool::new(false),
            busy: AtomicBool::new(false),
        }
    }

    pub(super) fn enter(&self) -> Option<CallbackGuard<'_>> {
        if self.busy.swap(true, Ordering::AcqRel) {
            self.callback_rejected();
            None
        } else {
            Some(CallbackGuard(&self.busy))
        }
    }

    pub(super) fn callback_rejected(&self) {
        self.diagnostics.record_callback_rejected(None);
    }

    fn malformed(&self, observations: &SampleMailbox) {
        self.diagnostics.record_malformed_buffer(None);
        observations.invalidate();
    }

    fn channels_changed(&self, channels: u32) {
        // The serialized callback stops decoding after this mismatch; no later format is queried.
        let _first_mismatch = self
            .actual_channels
            .compare_exchange(0, channels, Ordering::AcqRel, Ordering::Acquire)
            .is_ok();
    }

    pub(super) fn failed(&self) -> bool {
        self.actual_channels.load(Ordering::Acquire) != 0
    }

    /// Only the owner/control thread materializes an allocated error.
    pub(super) fn take_error(&self) -> Option<Error> {
        let channels = self.actual_channels.load(Ordering::Acquire);
        if channels == 0 || self.reported.swap(true, Ordering::AcqRel) {
            None
        } else {
            let error = if channels > 2 {
                Error::UnsupportedFormat("native input above two channels is unsupported".into())
            } else {
                Error::NativeFormatChanged {
                    advertised: self.advertised,
                    actual: (
                        self.advertised.0,
                        u16::try_from(channels).expect("supported channels"),
                    ),
                }
            };
            Some(error.with_context(ErrorContext::new(Operation::Normalize)))
        }
    }
}

/// Decode only complete mono/stereo f32 frames. All rejection reporting is atomic.
///
/// # Safety
/// `list` and its declared trailing array must be live native buffers. Non-null data
/// pointers must reference at least `mDataByteSize` readable bytes for this call.
pub(super) unsafe fn push_buffer_list(
    sink: &mut RawSink,
    scratch: &mut Vec<f32>,
    list: *const AudioBufferList,
    observations: &SampleMailbox,
    report: &BufferReport,
) {
    // A latched format failure ends decoding, even while its owner drains the error.
    if report.failed() {
        observations.invalidate();
        return;
    }
    if list.is_null() {
        report.malformed(observations);
        return;
    }
    let count = (*list).mNumberBuffers as usize;
    if count == 0 {
        // No native data is an ordinary idle observation, not sample loss.
        observations.invalidate();
        return;
    }
    let buffers = std::slice::from_raw_parts((*list).mBuffers.as_ptr(), count);
    let Some(channels) = buffers
        .iter()
        .try_fold(0u32, |sum, buffer| sum.checked_add(buffer.mNumberChannels))
    else {
        report.malformed(observations);
        return;
    };
    if channels > 2 || channels != u32::from(sink.native_channels()) {
        if channels == 0 {
            report.malformed(observations);
        } else {
            report.channels_changed(channels);
            observations.invalidate();
        }
        return;
    }

    let mut frames = None;
    for buffer in buffers {
        let Some(frame_bytes) = usize::try_from(buffer.mNumberChannels)
            .ok()
            .and_then(|channels| channels.checked_mul(4))
            .filter(|bytes| *bytes != 0)
        else {
            report.malformed(observations);
            return;
        };
        let bytes = buffer.mDataByteSize as usize;
        if bytes % frame_bytes != 0
            || (bytes != 0 && (buffer.mData.is_null() || !buffer.mData.cast::<f32>().is_aligned()))
        {
            report.malformed(observations);
            return;
        }
        let current = bytes / frame_bytes;
        if frames.is_some_and(|previous| previous != current) {
            report.malformed(observations);
            return;
        }
        frames = Some(current);
    }
    let frames = frames.unwrap_or(0);
    if frames == 0 {
        observations.invalidate();
        return;
    }
    let Some(total) = frames.checked_mul(channels as usize) else {
        report.malformed(observations);
        return;
    };
    if count == 1 {
        let samples = std::slice::from_raw_parts(buffers[0].mData.cast::<f32>(), total);
        let delivered = sink.push(samples, now_ns());
        observations.observe(samples, delivered);
        return;
    }
    // A supported two-plane layout must be one mono plane per channel.
    if count != 2 || buffers.iter().any(|buffer| buffer.mNumberChannels != 1) {
        report.malformed(observations);
        return;
    }
    if total > scratch.capacity() {
        report.malformed(observations);
        return;
    }
    scratch.resize(total, 0.0);
    for (channel, buffer) in buffers.iter().enumerate() {
        let samples = std::slice::from_raw_parts(buffer.mData.cast::<f32>(), frames);
        for (frame, sample) in samples.iter().enumerate() {
            scratch[frame * 2 + channel] = *sample;
        }
    }
    let delivered = sink.push(&scratch[..total], now_ns());
    observations.observe(&scratch[..total], delivered);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draining_a_format_failure_keeps_later_native_buffers_unexamined() {
        let (producer, _consumer) = flexaudio_core::raw_ring(1);
        let mut sink = RawSink::new(producer, 48_000, 2);
        let report = BufferReport::new(&sink);
        let observations = SampleMailbox::default();
        report.channels_changed(1);
        assert!(report.take_error().is_some());
        assert!(
            report.failed(),
            "draining must not reopen the callback gate"
        );
        assert!(report.diagnostics.drain().unwrap().is_empty());
        // SAFETY: The failure gate returns before dereferencing the pointer.
        unsafe {
            push_buffer_list(
                &mut sink,
                &mut Vec::new(),
                std::ptr::null(),
                &observations,
                &report,
            );
        }
        assert!(report.diagnostics.drain().unwrap().is_empty());
        assert!(report.take_error().is_none());
    }
}
