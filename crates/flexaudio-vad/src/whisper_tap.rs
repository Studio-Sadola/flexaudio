//! One serialized capture owner; bindings only select the carrier and marshal events.

use crate::whisper_capture::CaptureConverter;
use crate::whisper_params::MAX_FRAMES;
use crate::{
    AttachedWhisperVadEvent as Attached, WhisperVad, WhisperVadEvent, WhisperVadFailure,
    WhisperVadOptions, WhisperVadParams, WhisperVadTapError as Error,
    WhisperVadTapFailure as Failure,
};

const MAX_SAFE_PTS: i64 = 9_007_199_254_740_991;

#[derive(Clone, Copy)]
struct Origin {
    capture_sample: u64,
    pts_ns: i64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Running,
    Failed,
    Stopped,
}

/// Binding-independent canonical capture attachment over [`WhisperVad`].
///
/// Feed 48 kHz stereo interleaved f32 after the producer's shared denoise/gain snapshot,
/// before output-tap conversion/quantization. The capture index counts frames before fanout
/// or drops, never delivered stereo scalars. Only valid capture frames may be supplied;
/// transport padding is excluded. Bindings own tap selection, carrier delivery, and exclusion
/// against legacy `Vad` (the design excludes simultaneous legacy/new VAD on the entire stream).
///
/// Construct and call this owner on a serialized non-realtime worker/poll consumer. Conversion,
/// inference and event vectors allocate off-callback; never call it from a realtime callback.
/// It adds no locks. The callback remains a preallocated nonblocking raw-ring producer.
///
/// Shared rubato 3.0.0 sinc DSP averages channels and resamples at 1/3. Silent phase alignment
/// establishes raw source phase -63 frames, independently of the reported 21-output delay.
/// Trimming 21 outputs retains source frame zero; every retained j maps to origin+3*j.
/// A 960-frame block produces 320 raw outputs (299 retained at startup). EOF recovers all
/// ceil(N/3) valid outputs, discards flush zeros, then pads only the VAD's final 512-sample frame.
/// Thus an event at t integer ms maps exactly to capture_sample+t*48 and pts_ns+t*1_000_000.
/// Delivery latency, filter availability and model framing never shift those timestamps.
pub struct WhisperVadTap {
    vad: WhisperVad,
    converter: CaptureConverter,
    origin: Option<Origin>,
    started: bool,
    state: State,
}

impl WhisperVadTap {
    /// Snapshot shared parameters/options and construct DSP/model off the capture thread.
    pub fn new(params: WhisperVadParams, options: WhisperVadOptions) -> Result<Self, Error> {
        params.validate().map_err(Error::Vad)?;
        let converter = CaptureConverter::new().map_err(|_| Error::UnsupportedConversionClock)?;
        let vad = WhisperVad::new(params, options).map_err(Error::Vad)?;
        Ok(Self::with_vad(vad, converter))
    }

    fn with_vad(vad: WhisperVad, converter: CaptureConverter) -> Self {
        Self {
            vad,
            converter,
            origin: None,
            started: false,
            state: State::Running,
        }
    }

    /// Consume one canonical chunk and return its ordered attached events.
    ///
    /// `capture_sample`/`pts_ns` locate the first frame, independently of event delivery.
    /// PTS uses the chunk's signed safe-integer domain. Within an epoch, project PTS with
    /// floor(delta_frames*1e9/48000), rather than multiplying by truncated ns per frame.
    /// A discontinuity, unexpected capture index, or PTS reanchor closes the prefix before
    /// resetting and accepting the new origin. Invalid complete feeds leave state unchanged.
    pub fn process(
        &mut self,
        stereo_48k: &[f32],
        capture_sample: u64,
        pts_ns: i64,
        discontinuity: bool,
    ) -> Result<Vec<Attached>, Failure> {
        self.require_running()?;
        if stereo_48k.len() % 2 != 0 {
            return Err(Error::InvalidStereoLength.into());
        }
        for (sample, value) in stereo_48k.iter().enumerate() {
            if !value.is_finite() || !(-1.0..=1.0).contains(value) {
                return Err(Error::InvalidPcm { sample }.into());
            }
        }
        let frames =
            u64::try_from(stereo_48k.len() / 2).map_err(|_| Error::CaptureSampleOverflow)?;
        capture_sample
            .checked_add(frames)
            .ok_or(Error::CaptureSampleOverflow)?;
        project_pts(pts_ns, 0)?;
        project_pts(pts_ns, frames)?;
        let input_frames = self.converter.input_frames();
        let clock_break = if frames != 0 {
            self.origin.is_some_and(|origin| {
                origin.capture_sample.checked_add(input_frames) != Some(capture_sample)
                    || project_pts(origin.pts_ns, input_frames).ok() != Some(pts_ns)
            })
        } else {
            false
        };
        let split = discontinuity || clock_break;
        let total = if split { 0 } else { input_frames }
            .checked_add(frames)
            .ok_or(Error::CaptureSampleOverflow)?;
        if total.div_ceil(3).div_ceil(512) > MAX_FRAMES {
            return Err(Error::Vad(crate::WhisperVadError::Overflow {
                operation: crate::WhisperVadOperation::LogicalSamples,
            })
            .into());
        }
        // Reserve the eventual source PTS domain before conversion can advance any state.
        if !split {
            if let Some(origin) = self.origin {
                project_pts(origin.pts_ns, total)?;
            }
        }
        let mut events = Vec::new();
        if split && self.origin.is_some() {
            self.finish_epoch(&mut events)?;
            self.reinitialize(&mut events)?;
        }
        if frames == 0 {
            return Ok(events);
        }
        self.origin.get_or_insert(Origin {
            capture_sample,
            pts_ns,
        });
        let mono = match self.converter.push(stereo_48k) {
            Ok(mono) => mono,
            Err(()) => return Err(self.conversion_failure(events)),
        };
        self.feed_mono(&mono, &mut events)?;
        Ok(events)
    }

    /// Drain/finish the prefix, reset both DSP and model, and await a new capture origin.
    /// Empty/repeated flush is a no-op. After fatal failure, flush attempts reinitialization.
    /// Bindings must deliver this batch even when no subsequent audio chunk is available.
    pub fn flush(&mut self) -> Result<Vec<Attached>, Failure> {
        if self.state == State::Stopped {
            return Err(Error::Stopped.into());
        }
        let mut events = Vec::new();
        if self.state == State::Failed {
            self.reinitialize(&mut events)?;
        } else if self.origin.is_some() {
            self.finish_epoch(&mut events)?;
            self.reinitialize(&mut events)?;
        }
        Ok(events)
    }

    /// Drain valid PCM and finish exactly once, without starting another epoch.
    /// The caller stops intake/drains queued canonical chunks first and delivers this batch
    /// before disposing the worker. Stop before any valid PCM returns no epoch events.
    pub fn stop(&mut self) -> Result<Vec<Attached>, Failure> {
        if self.state == State::Stopped {
            return Ok(Vec::new());
        }
        self.require_running()?;
        let mut events = Vec::new();
        self.finish_epoch(&mut events)?;
        self.state = State::Stopped;
        Ok(events)
    }

    fn finish_epoch(&mut self, events: &mut Vec<Attached>) -> Result<(), Failure> {
        if self.origin.is_none() {
            return Ok(());
        }
        let mono = match self.converter.drain() {
            Ok(mono) => mono,
            Err(()) => return Err(self.conversion_failure(std::mem::take(events))),
        };
        self.feed_mono(&mono, events)?;
        if self.started {
            match self.vad.finish() {
                Ok(batch) => Self::append(events, batch),
                Err(failure) => return Err(self.vad_failure(failure, std::mem::take(events))),
            }
        }
        self.origin = None;
        self.started = false;
        Ok(())
    }

    fn feed_mono(&mut self, mono: &[f32], events: &mut Vec<Attached>) -> Result<(), Failure> {
        if mono.is_empty() {
            return Ok(());
        }
        // Filter ringing can exceed normalized PCM. Do not silently clip or pass it to inference.
        if mono
            .iter()
            .any(|sample| !sample.is_finite() || !(-1.0..=1.0).contains(sample))
        {
            return Err(self.conversion_failure(std::mem::take(events)));
        }
        if !self.started {
            let origin = self.origin.expect("valid mono requires capture input");
            events.push(Attached::EpochStart {
                epoch: self.vad.epoch(),
                seq: 0,
                capture_sample: origin.capture_sample,
                pts_ns: origin.pts_ns,
            });
            self.started = true;
        }
        match self.vad.process(mono) {
            Ok(batch) => Self::append(events, batch),
            Err(failure) => return Err(self.vad_failure(failure, std::mem::take(events))),
        }
        Ok(())
    }

    fn reinitialize(&mut self, events: &mut Vec<Attached>) -> Result<(), Failure> {
        let converter = match CaptureConverter::new() {
            Ok(converter) => converter,
            Err(()) => {
                self.state = State::Failed;
                return Err(Failure {
                    error: Error::UnsupportedConversionClock,
                    terminal_events: std::mem::take(events),
                });
            }
        };
        match self.vad.reset() {
            Ok(_) => {
                self.converter = converter;
                self.origin = None;
                self.started = false;
                self.state = State::Running;
                Ok(())
            }
            Err(failure) => Err(self.vad_failure(failure, std::mem::take(events))),
        }
    }

    fn append(events: &mut Vec<Attached>, batch: Vec<WhisperVadEvent>) {
        events.extend(batch.into_iter().map(|mut event| {
            // The strict frame domain bounds seq far below u64::MAX.
            event.seq = event
                .seq
                .checked_add(1)
                .expect("bounded VAD event sequence");
            Attached::Vad(event)
        }));
    }

    fn vad_failure(&mut self, failure: WhisperVadFailure, mut events: Vec<Attached>) -> Failure {
        if self.started {
            if failure.terminal_events.is_empty() {
                // The capture converter has already advanced. Even a shared-session
                // preflight rejection now invalidates this epoch; close published hints.
                // abort also suppresses duplicate terminals if the session already failed.
                if let Ok(closure) = self.vad.abort() {
                    Self::append(&mut events, closure.terminal_events);
                }
            } else {
                Self::append(&mut events, failure.terminal_events);
            }
        }
        self.state = State::Failed;
        Failure {
            error: Error::Vad(failure.error),
            terminal_events: events,
        }
    }

    fn conversion_failure(&mut self, mut events: Vec<Attached>) -> Failure {
        let error = match self.vad.abort() {
            Ok(failure) => {
                if self.started {
                    Self::append(&mut events, failure.terminal_events);
                }
                Error::Conversion
            }
            Err(error) => Error::Vad(error),
        };
        self.state = State::Failed;
        Failure {
            error,
            terminal_events: events,
        }
    }

    fn require_running(&self) -> Result<(), Error> {
        match self.state {
            State::Running => Ok(()),
            State::Failed => Err(Error::FailedSession),
            State::Stopped => Err(Error::Stopped),
        }
    }
}

fn project_pts(origin: i64, delta_frames: u64) -> Result<i64, Error> {
    let value = i128::from(origin) + i128::from(delta_frames) * 1_000_000_000 / 48_000;
    if !(-i128::from(MAX_SAFE_PTS)..=i128::from(MAX_SAFE_PTS)).contains(&value) {
        return Err(Error::PtsOutOfRange);
    }
    i64::try_from(value).map_err(|_| Error::PtsOutOfRange)
}

#[cfg(test)]
#[path = "whisper_tap_tests.rs"]
mod tests;
