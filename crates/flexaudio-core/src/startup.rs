//! Sample-based microphone startup confirmation, shared by facade capture paths.
use crate::{AudioLoss, Error, Event, Result};
use std::borrow::Cow;
use std::num::NonZeroU64;
use std::time::Duration;

/// Account valid startup PCM that terminal failure prevents from being delivered.
/// Counts retain the native interleaved format; empty input produces no notice.
pub fn discarded_startup_loss(
    samples: &[f32],
    rate: u32,
    channels: u16,
) -> Result<Option<AudioLoss>> {
    if rate == 0 || channels == 0 || samples.len() % usize::from(channels) != 0 {
        return Err(Error::UnsupportedFormat("startup loss requires a nonzero native rate and whole interleaved frames; recreate the stream with a valid input format".into()));
    }
    let count = u64::try_from(samples.len()).map_err(|_| {
        Error::InvalidState("startup loss count exhausted; reopen the capture stream".into())
    })?;
    NonZeroU64::new(count)
        .map(|count| AudioLoss::generation_ended(Some(count), rate, channels))
        .transpose()
}

/// Why leading microphone frames were suppressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupDropReason {
    /// At least one channel was non-finite or outside [-1, 1].
    OutOfRange,
}
impl StartupDropReason {
    /// Stable binding discriminant.
    pub fn as_str(self) -> &'static str {
        "outOfRange"
    }
}
/// One generation's leading microphone validation, before DSP or gain.
/// Confirm 10 ms of valid native frames and replay them from the beginning.
pub struct StartupSamples {
    rate: u32,
    channels: u16,
    finished: bool,
    seen: u64,
    dropped: u64,
    pending: Vec<f32>,
    confirmation_frames: usize,
}
/// Accepted input and its original native-frame position.
pub struct StartupOutput<'a> {
    /// Includes held confirmation samples, never an invalid leading frame.
    pub samples: Cow<'a, [f32]>,
    /// Native frame offset at the start of accepted samples.
    pub first_frame: u64,
    /// Emitted once when a dropped generation becomes valid.
    pub event: Option<Event>,
}
impl StartupSamples {
    /// Create state for a validated nonzero mono/stereo native format.
    pub fn new(rate: u32, channels: u16) -> Self {
        Self {
            rate,
            channels,
            finished: false,
            seen: 0,
            dropped: 0,
            pending: Vec::new(),
            confirmation_frames: (u64::from(rate).div_ceil(100)) as usize,
        }
    }
    /// Whether this generation is still validating its leading samples.
    pub fn is_pending(&self) -> bool {
        !self.finished
    }
    /// Number of suppressed native frames, including interrupted confirmation windows.
    pub fn dropped_frames(&self) -> u64 {
        self.dropped
    }
    /// The single report for this generation, also usable on timeout/early shutdown.
    pub fn dropped_event(&self) -> Option<Event> {
        (self.dropped > 0).then_some(Event::StartupSamplesDropped {
            frames: self.dropped,
            reason: StartupDropReason::OutOfRange,
        })
    }
    /// Reject persistent invalid startup at the wall-clock deadline, even without more input.
    pub fn check_deadline(&self, elapsed: Duration) -> Result<()> {
        if !self.finished && self.dropped > 0 && elapsed >= Duration::from_secs(2) {
            return Err(Self::timeout());
        }
        Ok(())
    }
    fn timeout() -> Error {
        Error::Backend("microphone input keeps returning out-of-range or non-finite startup samples; valid input was not confirmed within 2 seconds. Reconnect the input, check its driver and recording settings, then reopen the stream".into())
    }
    /// Finish a generation. A wholly valid short input is released unchanged;
    /// an unconfirmed candidate following invalid input is suppressed and counted.
    pub fn finish(&mut self) -> StartupOutput<'static> {
        if self.finished {
            return StartupOutput {
                samples: Cow::Owned(Vec::new()),
                first_frame: self.seen,
                event: None,
            };
        }
        self.finished = true;
        if self.dropped == 0 {
            return StartupOutput {
                samples: Cow::Owned(std::mem::take(&mut self.pending)),
                first_frame: 0,
                event: None,
            };
        }
        self.dropped = self
            .dropped
            .saturating_add((self.pending.len() / usize::from(self.channels.max(1))) as u64);
        self.pending.clear();
        StartupOutput {
            samples: Cow::Owned(Vec::new()),
            first_frame: self.seen,
            event: self.dropped_event(),
        }
    }
    /// Feed whole native frames off callback. Mid-stream samples are never modified.
    pub fn push<'a>(&mut self, samples: &'a [f32], elapsed: Duration) -> Result<StartupOutput<'a>> {
        let channels = usize::from(self.channels);
        if self.rate == 0 || channels == 0 || samples.len() % channels != 0 {
            return Err(Error::UnsupportedFormat("microphone startup validation requires a nonzero native rate and whole interleaved frames; recreate the stream with a valid input format".into()));
        }
        self.check_deadline(elapsed)?;
        let first_frame = self.seen;
        if self.finished {
            self.seen = self.seen.saturating_add((samples.len() / channels) as u64);
            return Ok(StartupOutput {
                samples: Cow::Borrowed(samples),
                first_frame,
                event: None,
            });
        }
        for (index, frame) in samples.chunks_exact(channels).enumerate() {
            self.seen = self.seen.saturating_add(1);
            let valid = frame
                .iter()
                .all(|sample| sample.is_finite() && sample.abs() <= 1.0);
            if !valid {
                self.dropped = self
                    .dropped
                    .saturating_add(1 + (self.pending.len() / channels) as u64);
                self.pending.clear();
            } else {
                self.pending.extend_from_slice(frame);
                if self.pending.len() / channels >= self.confirmation_frames {
                    self.finished = true;
                    self.pending
                        .extend_from_slice(&samples[(index + 1) * channels..]);
                    self.seen = first_frame.saturating_add((samples.len() / channels) as u64);
                    return Ok(StartupOutput {
                        samples: Cow::Owned(std::mem::take(&mut self.pending)),
                        first_frame: self.dropped,
                        event: self.dropped_event(),
                    });
                }
            }
            if self.seen >= u64::from(self.rate) * 2 {
                return Err(Self::timeout());
            }
        }
        Ok(StartupOutput {
            samples: Cow::Borrowed(&[]),
            first_frame: self.seen,
            event: None,
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn startup_invalid_prefix_is_dropped_and_confirmation_is_replayed() -> Result<()> {
        let mut guard = StartupSamples::new(48_000, 1);
        assert!(guard
            .push(&[3.058; 480], Duration::ZERO)?
            .samples
            .is_empty());
        assert!(guard
            .push(&[0.0; 240], Duration::from_millis(10))?
            .samples
            .is_empty());
        let result = guard.push(&[0.25; 240], Duration::from_millis(15))?;
        assert_eq!(result.first_frame, 480);
        assert_eq!(&result.samples[..240], &[0.0; 240]);
        assert_eq!(&result.samples[240..], &[0.25; 240]);
        assert!(result.event.is_some());
        assert!(guard
            .push(&[3.0], Duration::from_millis(20))?
            .event
            .is_none());
        Ok(())
    }
    #[test]
    fn startup_valid_zero_prefix_is_unchanged_without_drops() -> Result<()> {
        let mut guard = StartupSamples::new(48_000, 2);
        let samples: Vec<f32> = [0.0, -0.0, 0.25, -0.25].repeat(240);
        let result = guard.push(&samples, Duration::ZERO)?;
        assert_eq!(&*result.samples, &samples);
        assert_eq!(result.first_frame, 0);
        assert!(result.event.is_none());
        Ok(())
    }
    #[test]
    fn startup_invalid_input_fails_at_two_seconds() -> Result<()> {
        let mut guard = StartupSamples::new(48_000, 1);
        let _ = guard.push(&[f32::NAN; 480], Duration::ZERO)?;
        let result = guard.push(&[3.0; 480], Duration::from_secs(2));
        assert!(
            result.is_err(),
            "invalid input must fail rather than be delivered"
        );
        Ok(())
    }
}

#[cfg(test)]
mod boundary_tests {
    use super::*;
    #[test]
    fn interrupted_confirmation_discards_whole_stereo_frames_and_reports_once() -> Result<()> {
        let mut guard = StartupSamples::new(44_100, 2);
        let _ = guard.push(&[0.5, f32::INFINITY], Duration::ZERO)?;
        let _ = guard.push(&[0.0; 440], Duration::from_millis(1))?;
        let _ = guard.push(&[f32::NAN, 0.5], Duration::from_millis(5))?;
        let valid = vec![0.25; 882];
        let result = guard.push(&valid, Duration::from_millis(6))?;
        assert_eq!(result.first_frame, 222);
        assert_eq!(&*result.samples, &valid);
        assert_eq!(
            result.event,
            Some(Event::StartupSamplesDropped {
                frames: 222,
                reason: StartupDropReason::OutOfRange
            })
        );
        let next = guard.push(&[2.0, f32::NAN], Duration::from_secs(3))?;
        assert_eq!(next.samples[0], 2.0);
        assert!(next.samples[1].is_nan());
        assert!(next.event.is_none());
        Ok(())
    }
    #[test]
    fn confirmed_window_disables_filter_for_midstream_out_of_range() -> Result<()> {
        let mut guard = StartupSamples::new(48_000, 1);
        let _ = guard.push(&[0.0; 480], Duration::ZERO)?;
        let samples = [0.0, 3.058, f32::NAN];
        let output = guard.push(&samples, Duration::ZERO)?;
        assert_eq!(output.samples[0], 0.0);
        assert_eq!(output.samples[1], 3.058);
        assert!(output.samples[2].is_nan());
        assert!(output.event.is_none());
        assert_eq!(guard.dropped_frames(), 0);
        Ok(())
    }
    #[test]
    fn invalid_input_deadline_does_not_require_another_callback() -> Result<()> {
        let mut guard = StartupSamples::new(48_000, 1);
        let _ = guard.push(&[2.0], Duration::ZERO)?;
        assert!(guard.check_deadline(Duration::from_millis(1999)).is_ok());
        let error = guard
            .check_deadline(Duration::from_secs(2))
            .expect_err("deadline must fail");
        assert!(error.to_string().contains("out-of-range"));
        assert!(error.to_string().contains("reopen"));
        Ok(())
    }
}

#[cfg(test)]
mod leading_window_tests {
    use super::*;
    #[test]
    fn valid_first_sample_does_not_release_an_invalid_startup_window() -> Result<()> {
        let mut guard = StartupSamples::new(48_000, 1);
        let mut samples = vec![0.0, 3.058];
        samples.extend([0.25; 480]);
        let output = guard.push(&samples, Duration::ZERO)?;
        assert_eq!(&*output.samples, &[0.25; 480]);
        assert_eq!(output.first_frame, 2);
        assert_eq!(
            output.event,
            Some(Event::StartupSamplesDropped {
                frames: 2,
                reason: StartupDropReason::OutOfRange
            })
        );
        Ok(())
    }
}

#[cfg(test)]
mod finish_tests {
    use super::*;
    #[test]
    fn short_valid_zero_capture_is_delivered_on_finish_without_a_drop() -> Result<()> {
        let mut guard = StartupSamples::new(48_000, 2);
        let zeros = [0.0; 26];
        assert!(guard.push(&zeros, Duration::ZERO)?.samples.is_empty());
        let tail = guard.finish();
        assert_eq!(&*tail.samples, &zeros);
        assert!(tail.event.is_none());
        assert!(guard.finish().samples.is_empty());
        Ok(())
    }
    #[test]
    fn discarded_startup_loss_retains_native_scalar_count_and_validates_frames() -> Result<()> {
        let loss = discarded_startup_loss(&[0.25; 26], 44_100, 2)?
            .expect("positive discarded startup prefix");
        assert_eq!(loss.samples().map(NonZeroU64::get), Some(26));
        assert_eq!((loss.sample_rate(), loss.channels()), (44_100, 2));
        assert_eq!(loss.reason(), crate::LossReason::GenerationEnded);
        assert!(discarded_startup_loss(&[], 48_000, 2)?.is_none());
        for (samples, rate, channels) in [
            (&[0.25][..], 48_000, 2),
            (&[][..], 0, 2),
            (&[][..], 48_000, 0),
        ] {
            assert!(matches!(
                discarded_startup_loss(samples, rate, channels),
                Err(Error::UnsupportedFormat(_))
            ));
        }
        Ok(())
    }
}
