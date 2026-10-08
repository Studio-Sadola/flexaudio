//! Own-process-only private tap and timestamped, preallocated capture evidence.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::AnyThread;
use objc2_core_audio::{
    kAudioObjectPropertyScopeGlobal, kAudioTapPropertyFormat, AudioHardwareCreateProcessTap,
    AudioHardwareDestroyAggregateDevice, AudioHardwareDestroyProcessTap, AudioObjectID,
    CATapDescription,
};
use objc2_core_audio_types::{
    AudioBufferList, AudioStreamBasicDescription, AudioTimeStamp, AudioTimeStampFlags,
};
use objc2_foundation::{NSArray, NSNumber, NSString};

use super::io::{buffers, check, failed, float_rate, read_property, IoRegistration};
use crate::probe::{ProbeControl, ProbeOutcome};
use crate::probe_signal::CapturedFrame;
use crate::tap::create_aggregate_device;

struct AtomicFrame {
    time: AtomicU64,
    sample: AtomicU32,
    exact_zero: AtomicBool,
}

pub(super) struct CaptureState {
    frames: Box<[AtomicFrame]>,
    count: AtomicUsize,
    next_host: AtomicU64,
    busy: AtomicBool,
    pub(super) invalid: AtomicBool,
    pub(super) rate: u32,
    ticks_per_second: f64,
    enabled: Arc<AtomicBool>,
    pub(super) gate: Arc<AtomicBool>,
    cancellation: Arc<AtomicBool>,
}

impl CaptureState {
    /// Published end of the most recent complete callback, in Mach host-clock ticks.
    pub(super) fn observed_end(&self) -> Option<u64> {
        if self.count.load(Ordering::Acquire) == 0 {
            return None;
        }
        Some(self.next_host.load(Ordering::Acquire))
    }

    /// # Safety
    /// list/time must be the private tap's live Core Audio callback data, verified stereo f32.
    unsafe fn record(&self, list: *const AudioBufferList, time: &AudioTimeStamp) {
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
            // SAFETY: native callback preconditions forwarded to the strictly validated buffer adapter.
            let buffers = unsafe { buffers(list)? };
            if buffers
                .iter()
                .map(|buffer| buffer.mNumberChannels as usize)
                .sum::<usize>()
                != 2
            {
                return None;
            }
            let frames =
                buffers[0].mDataByteSize as usize / (4 * buffers[0].mNumberChannels as usize);
            let offset = self.count.load(Ordering::Relaxed);
            if frames > self.frames.len().saturating_sub(offset) {
                return None;
            }
            let step = self.ticks_per_second / f64::from(self.rate);
            let previous = self.next_host.load(Ordering::Relaxed);
            if previous != 0 && time.mHostTime.abs_diff(previous) as f64 > step * 2.0 + 2.0 {
                return None;
            }
            for frame in 0..frames {
                let mut first = None;
                let mut exact_zero = true;
                for buffer in buffers {
                    let channels = buffer.mNumberChannels as usize;
                    // SAFETY: validated f32 buffer has frames*channels aligned samples for this cycle.
                    let samples = unsafe {
                        std::slice::from_raw_parts(buffer.mData.cast::<f32>(), frames * channels)
                    };
                    let samples = &samples[frame * channels..(frame + 1) * channels];
                    if first.is_none() {
                        first = samples.first().copied();
                    }
                    exact_zero &= samples.iter().all(|sample| sample.to_bits() == 0);
                }
                let first = first?;
                let destination = &self.frames[offset + frame];
                destination.time.store(
                    time.mHostTime
                        .saturating_add((frame as f64 * step).round() as u64),
                    Ordering::Relaxed,
                );
                destination.sample.store(first.to_bits(), Ordering::Relaxed);
                destination.exact_zero.store(exact_zero, Ordering::Relaxed);
            }
            self.next_host.store(
                time.mHostTime
                    .saturating_add((frames as f64 * step).round() as u64),
                Ordering::Relaxed,
            );
            self.count.store(offset + frames, Ordering::Release);
            Some(())
        })();
        if result.is_none() {
            self.invalid.store(true, Ordering::Release);
        }
        self.busy.store(false, Ordering::Release);
    }

    /// Read only after synchronous IOProc stop and destruction have completed.
    pub(super) fn snapshot(&self) -> Vec<CapturedFrame> {
        self.frames[..self.count.load(Ordering::Acquire).min(self.frames.len())]
            .iter()
            .map(|frame| CapturedFrame {
                host_time: frame.time.load(Ordering::Relaxed),
                sample: f32::from_bits(frame.sample.load(Ordering::Relaxed)),
                all_exact_zero: frame.exact_zero.load(Ordering::Relaxed),
            })
            .collect()
    }
}

struct CaptureResources {
    registration: Option<IoRegistration>,
    aggregate: AudioObjectID,
    tap: AudioObjectID,
    _description: Retained<CATapDescription>,
}

impl CaptureResources {
    fn stop(&mut self) -> bool {
        self.registration.as_mut().is_none_or(IoRegistration::stop)
    }

    fn close(&mut self) -> bool {
        let mut clean = self.stop();
        if self.aggregate != 0 {
            // SAFETY: uniquely owned private aggregate; IOProc stop/destruction were attempted.
            // HAL's copied block keeps Arc context alive if removal fails; its gate is closed.
            let status = unsafe { AudioHardwareDestroyAggregateDevice(self.aggregate) };
            clean &= status == 0;
            if status == 0 {
                self.aggregate = 0;
            }
        }
        if self.tap != 0 {
            // SAFETY: this owner created the tap and attempted to tear down all dependents first.
            let status = unsafe { AudioHardwareDestroyProcessTap(self.tap) };
            clean &= status == 0;
            if status == 0 {
                self.tap = 0;
            }
        }
        clean
    }
}

impl Drop for CaptureResources {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

pub(super) struct Capture {
    resources: CaptureResources,
    pub(super) state: Arc<CaptureState>,
}

impl Capture {
    pub(super) fn new(
        own_object: AudioObjectID,
        ticks_per_second: f64,
        enabled: Arc<AtomicBool>,
        control: &ProbeControl,
    ) -> Result<Self, ProbeOutcome> {
        check(control)?;
        let objects = NSArray::from_retained_slice(&[NSNumber::numberWithUnsignedInt(own_object)]);
        // SAFETY: objects contains this process's verified Core Audio process object; no exclusions.
        let description = unsafe {
            CATapDescription::initStereoMixdownOfProcesses(CATapDescription::alloc(), &objects)
        };
        // SAFETY: these public setters operate on a newly initialized, privately owned description.
        unsafe {
            description.setPrivate(true);
            description.setName(&NSString::from_str("flexaudio-private-permission-probe"));
        }
        let mut resources = CaptureResources {
            registration: None,
            aggregate: 0,
            tap: 0,
            _description: description,
        };
        // SAFETY: description and output tap-ID storage remain owned on this thread.
        let status = unsafe {
            AudioHardwareCreateProcessTap(Some(&resources._description), &mut resources.tap)
        };
        if status != 0 || resources.tap == 0 {
            return Err(failed("private tap creation", status));
        }
        check(control)?;
        // SAFETY: ASBD is numeric POD; zero is valid initialized output-property storage.
        let mut format: AudioStreamBasicDescription = unsafe { std::mem::zeroed() };
        read_property(
            resources.tap,
            kAudioTapPropertyFormat,
            kAudioObjectPropertyScopeGlobal,
            &mut format,
        )?;
        let rate = float_rate(&format)?;
        if format.mChannelsPerFrame != 2 {
            return Err(failed("private tap is not stereo", -1));
        }
        // SAFETY: description is valid and retained until tap/aggregate destruction; UUID is owned.
        let uuid = unsafe { resources._description.UUID() }.UUIDString();
        resources.aggregate = create_aggregate_device("flexaudio-private-permission-probe", &uuid)
            .map_err(|error| ProbeOutcome::Failed {
                detail: format!("self-probe aggregate creation: {error}"),
            })?;
        check(control)?;
        let state = Arc::new(CaptureState {
            frames: (0..rate as usize * 2)
                .map(|_| AtomicFrame {
                    time: AtomicU64::new(0),
                    sample: AtomicU32::new(0),
                    exact_zero: AtomicBool::new(false),
                })
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            count: AtomicUsize::new(0),
            next_host: AtomicU64::new(0),
            busy: AtomicBool::new(false),
            invalid: AtomicBool::new(false),
            rate,
            ticks_per_second,
            enabled,
            gate: Arc::new(AtomicBool::new(false)),
            cancellation: control.cancellation_flag(),
        });
        let callback = state.clone();
        let block = RcBlock::new(
            move |_now: NonNull<AudioTimeStamp>,
                  input: NonNull<AudioBufferList>,
                  input_time: NonNull<AudioTimeStamp>,
                  _output: NonNull<AudioBufferList>,
                  _output_time: NonNull<AudioTimeStamp>| {
                let result = catch_unwind(AssertUnwindSafe(|| {
                    // SAFETY: Core Audio supplies this private tap's live buffers and acquisition time.
                    unsafe { callback.record(input.as_ptr(), input_time.as_ref()) };
                }));
                if result.is_err() {
                    callback.invalid.store(true, Ordering::Release);
                }
            },
        );
        resources.registration = Some(IoRegistration::start(
            resources.aggregate,
            block,
            state.gate.clone(),
            control,
        )?);
        Ok(Self { resources, state })
    }

    pub(super) fn close(&mut self) -> bool {
        self.resources.close()
    }
}
