//! Low-amplitude default-output renderer with callback-content and host-time evidence.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use block2::RcBlock;
use objc2_core_audio::{
    kAudioDevicePropertyStreams, kAudioHardwarePropertyDefaultOutputDevice,
    kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyScopeOutput, kAudioObjectSystemObject,
    kAudioStreamPropertyVirtualFormat, AudioObjectID,
};
use objc2_core_audio_types::{
    AudioBufferList, AudioStreamBasicDescription, AudioTimeStamp, AudioTimeStampFlags,
};

use super::io::{buffers, check, failed, float_rate, read_objects, read_property, IoRegistration};
use crate::probe::{ProbeControl, ProbeOutcome};
use crate::probe_signal::{ProbeSignal, RenderEvidence};

pub(super) fn default_device() -> Result<AudioObjectID, ProbeOutcome> {
    let mut device = 0;
    // The SDK system-object constant is the positive object ID 1, typed as an i32 constant.
    read_property(
        kAudioObjectSystemObject as AudioObjectID,
        kAudioHardwarePropertyDefaultOutputDevice,
        kAudioObjectPropertyScopeGlobal,
        &mut device,
    )?;
    if device == 0 {
        return Err(failed("no default output device", -1));
    }
    Ok(device)
}

pub(super) struct OutputState {
    pub(super) enabled: Arc<AtomicBool>,
    pub(super) gate: Arc<AtomicBool>,
    pub(super) invalid: AtomicBool,
    pub(super) rate: u32,
    samples: Box<[f32]>,
    cancellation: Arc<AtomicBool>,
    ticks_per_second: f64,
    channels: usize,
    next_frame: AtomicUsize,
    next_host: AtomicU64,
    start: AtomicU64,
    end: AtomicU64,
    nonzero: AtomicUsize,
    busy: AtomicBool,
}

impl OutputState {
    /// Core Audio output memory is pre-zeroed. A closed/cancelled gate leaves it untouched.
    ///
    /// # Safety
    /// The list/time come from this registration's live Core Audio callback, with validated f32 format.
    unsafe fn fill(&self, list: *const AudioBufferList, time: &AudioTimeStamp) {
        if self.gate.load(Ordering::Acquire)
            || self.cancellation.load(Ordering::Acquire)
            || !self.enabled.load(Ordering::Acquire)
        {
            return;
        }
        if self.busy.swap(true, Ordering::AcqRel) {
            self.invalid.store(true, Ordering::Release);
            return;
        }
        let result = (|| {
            if !time.mFlags.contains(AudioTimeStampFlags::HostTimeValid) || time.mHostTime == 0 {
                return None;
            }
            // SAFETY: forwarded from the native callback under the documented f32 format check.
            let buffers = unsafe { buffers(list)? };
            if buffers
                .iter()
                .map(|buffer| buffer.mNumberChannels as usize)
                .sum::<usize>()
                != self.channels
            {
                return None;
            }
            let frames =
                buffers[0].mDataByteSize as usize / (4 * buffers[0].mNumberChannels as usize);
            let offset = self.next_frame.load(Ordering::Relaxed);
            if offset >= self.samples.len() {
                return Some(());
            }
            let count = frames.min(self.samples.len() - offset);
            let step = self.ticks_per_second / f64::from(self.rate);
            let previous = self.next_host.load(Ordering::Relaxed);
            if previous != 0 && time.mHostTime.abs_diff(previous) as f64 > step * 2.0 + 2.0 {
                return None;
            }
            let mut nonzero = 0;
            for buffer in buffers {
                let channels = buffer.mNumberChannels as usize;
                // SAFETY: buffers verified aligned f32 storage and exactly frames * channels entries.
                let destination = unsafe {
                    std::slice::from_raw_parts_mut(buffer.mData.cast::<f32>(), frames * channels)
                };
                for (frame, destination) in destination
                    .chunks_exact_mut(channels)
                    .take(count)
                    .enumerate()
                {
                    destination.fill(self.samples[offset + frame]);
                }
            }
            for sample in &self.samples[offset..offset + count] {
                nonzero += usize::from(sample.to_bits() != 0);
            }
            self.start
                .compare_exchange(0, time.mHostTime, Ordering::AcqRel, Ordering::Relaxed)
                .ok();
            let end = time
                .mHostTime
                .saturating_add((count as f64 * step).round() as u64);
            self.end.store(end, Ordering::Release);
            self.next_host.store(
                time.mHostTime
                    .saturating_add((frames as f64 * step).round() as u64),
                Ordering::Relaxed,
            );
            self.nonzero.fetch_add(nonzero, Ordering::Release);
            self.next_frame.store(offset + count, Ordering::Release);
            Some(())
        })();
        if result.is_none() {
            self.invalid.store(true, Ordering::Release);
        }
        self.busy.store(false, Ordering::Release);
    }

    pub(super) fn finished(&self) -> bool {
        self.next_frame.load(Ordering::Acquire) >= self.samples.len()
    }

    pub(super) fn evidence(&self, capture_rate: u32, valid: bool) -> RenderEvidence {
        RenderEvidence {
            start: self.start.load(Ordering::Acquire),
            end: self.end.load(Ordering::Acquire),
            ticks_per_second: self.ticks_per_second,
            nonzero_frames: self.nonzero.load(Ordering::Acquire),
            rate: self.rate,
            capture_rate,
            valid: valid && !self.invalid.load(Ordering::Acquire) && self.finished(),
        }
    }
}

pub(super) struct Output {
    pub(super) device: AudioObjectID,
    pub(super) state: Arc<OutputState>,
    registration: IoRegistration,
}

impl Output {
    pub(super) fn new(
        signal: &ProbeSignal,
        ticks_per_second: f64,
        control: &ProbeControl,
    ) -> Result<Self, ProbeOutcome> {
        check(control)?;
        let device = default_device()?;
        let streams = read_objects(
            device,
            kAudioDevicePropertyStreams,
            kAudioObjectPropertyScopeOutput,
        )?;
        let mut rate = None;
        let mut channels = 0usize;
        for stream in streams {
            check(control)?;
            // SAFETY: ASBD is a C numeric POD; all-zero initialization is valid property storage.
            let mut format: AudioStreamBasicDescription = unsafe { std::mem::zeroed() };
            read_property(
                stream,
                kAudioStreamPropertyVirtualFormat,
                kAudioObjectPropertyScopeGlobal,
                &mut format,
            )?;
            let stream_rate = float_rate(&format)?;
            if rate.is_some_and(|rate| rate != stream_rate) {
                return Err(failed("output streams have differing rates", -1));
            }
            rate = Some(stream_rate);
            channels += format.mChannelsPerFrame as usize;
        }
        let rate = rate.ok_or_else(|| failed("no output streams", -1))?;
        if channels == 0 || channels > 64 {
            return Err(failed("unsupported output channel count", -1));
        }
        let state = Arc::new(OutputState {
            enabled: Arc::new(AtomicBool::new(false)),
            gate: Arc::new(AtomicBool::new(false)),
            invalid: AtomicBool::new(false),
            rate,
            samples: signal.samples(rate).into_boxed_slice(),
            cancellation: control.cancellation_flag(),
            ticks_per_second,
            channels,
            next_frame: AtomicUsize::new(0),
            next_host: AtomicU64::new(0),
            start: AtomicU64::new(0),
            end: AtomicU64::new(0),
            nonzero: AtomicUsize::new(0),
            busy: AtomicBool::new(false),
        });
        let callback = state.clone();
        let block = RcBlock::new(
            move |_now: NonNull<AudioTimeStamp>,
                  _input: NonNull<AudioBufferList>,
                  _input_time: NonNull<AudioTimeStamp>,
                  output: NonNull<AudioBufferList>,
                  output_time: NonNull<AudioTimeStamp>| {
                // Never unwind over the native callback boundary. The block owns an Arc copied by HAL.
                let result = catch_unwind(AssertUnwindSafe(|| {
                    // SAFETY: Core Audio supplies live output buffers and a timestamp for this IO cycle.
                    unsafe { callback.fill(output.as_ptr(), output_time.as_ref()) };
                }));
                if result.is_err() {
                    callback.invalid.store(true, Ordering::Release);
                }
            },
        );
        let registration = IoRegistration::start(device, block, state.gate.clone(), control)?;
        Ok(Self {
            device,
            state,
            registration,
        })
    }

    pub(super) fn stop(&mut self) -> bool {
        self.registration.stop()
    }
}
