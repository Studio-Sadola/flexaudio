//! One intake generation's startup confirmation, native clock and DSP ownership.
use super::*;
use flexaudio_core::startup::{StartupOutput, StartupSamples};
use std::time::Instant;

#[derive(Default)]
pub(super) struct InputProgress {
    pub pushed: bool,
    pub discontinuity: bool,
}

pub(super) enum GenerationEnd {
    Replaced,
    Stopped,
}

pub(super) struct IntakeGeneration {
    pub number: u64,
    pub normalizer: Normalizer,
    pub overflows: u64,
    pub minimum_pts: i64,
    native: (u32, u16),
    clock: ClockNormalizer,
    startup: Option<StartupSamples>,
    started: Instant,
    origin: Option<i64>,
    external_gap_ns: i64,
}

impl IntakeGeneration {
    pub fn new(
        shared: &SharedState,
        snapshot: &RawSnapshot,
        formats: (OutputFormat, Option<OutputFormat>),
        minimum_pts: i64,
    ) -> Result<Self> {
        let (rate, channels) = snapshot.native_format;
        Ok(Self {
            number: snapshot.generation,
            normalizer: build_normalizer(
                shared,
                rate,
                channels,
                formats.0,
                formats.1,
                snapshot.denoise_enabled,
            )?,
            overflows: 0,
            minimum_pts,
            native: (rate, channels),
            clock: ClockNormalizer::new(),
            startup: snapshot
                .microphone
                .then(|| StartupSamples::new(rate, channels)),
            started: Instant::now(),
            origin: None,
            external_gap_ns: 0,
        })
    }

    pub fn rebuild_after_loss(
        &mut self,
        shared: &SharedState,
        denoise_enabled: bool,
        formats: (OutputFormat, Option<OutputFormat>),
    ) -> Result<()> {
        advance_frame_index(
            &shared.capture_frame_index,
            self.normalizer.buffered_capture_frames() as u64,
            48_000,
        )?;
        self.normalizer = build_normalizer(
            shared,
            self.native.0,
            self.native.1,
            formats.0,
            formats.1,
            denoise_enabled,
        )?;
        self.clock = ClockNormalizer::new();
        Ok(())
    }

    pub fn external_drop(&mut self, shared: &SharedState, frames: u64, rate: u32) -> Result<()> {
        shared.advance_startup_gap(frames, rate)?;
        self.external_gap_ns = native_frames_ns(frames, rate);
        shared.push_event(Event::StartupSamplesDropped {
            frames,
            reason: flexaudio_core::StartupDropReason::OutOfRange,
        });
        Ok(())
    }

    pub fn check_deadline(&mut self, shared: &SharedState) -> Result<()> {
        if let Some(error) = self
            .startup
            .as_ref()
            .and_then(|guard| guard.check_deadline(self.started.elapsed()).err())
        {
            self.reject_pending(shared)?;
            return Err(error);
        }
        Ok(())
    }

    pub fn accept(
        &mut self,
        shared: &SharedState,
        samples: &[f32],
        reanchor: bool,
    ) -> Result<InputProgress> {
        let norm_pts = self
            .clock
            .normalize(monotonic_now_ns())
            .max(self.minimum_pts);
        let mut origin = *self.origin.get_or_insert(norm_pts);
        let accepted = match self.startup.as_mut() {
            Some(guard) => match guard.push(samples, self.started.elapsed()) {
                Ok(accepted) => accepted,
                Err(error) => {
                    self.reject_pending(shared)?;
                    return Err(error);
                }
            },
            None => StartupOutput {
                samples: std::borrow::Cow::Borrowed(samples),
                first_frame: 0,
                event: None,
            },
        };
        if reanchor {
            origin = norm_pts.saturating_sub(native_frames_ns(accepted.first_frame, self.native.0));
            self.origin = Some(origin);
        }
        let discontinuity = self.report_drop(shared, accepted.event)?;
        if self.external_gap_ns > 0 {
            Self::anchor_epoch(shared, norm_pts.saturating_sub(self.external_gap_ns));
            self.external_gap_ns = 0;
        }
        let pts = if self.startup.is_some() && !accepted.samples.is_empty() {
            origin.saturating_add(native_frames_ns(accepted.first_frame, self.native.0))
        } else {
            norm_pts
        };
        let pushed = !accepted.samples.is_empty();
        if pushed {
            self.normalizer.push(&accepted.samples, pts)?;
        }
        Ok(InputProgress {
            pushed,
            discontinuity,
        })
    }

    /// Generation changes and graceful stop close through the same operation.
    pub fn finish(&mut self, shared: &SharedState, end: GenerationEnd) -> Result<InputProgress> {
        if shared.terminal.is_failed() {
            return self
                .reject_pending(shared)
                .map(|discontinuity| InputProgress {
                    pushed: false,
                    discontinuity,
                });
        }
        let mut progress = InputProgress::default();
        if let Some(end) = self.finish_startup() {
            progress.discontinuity = self.report_drop(shared, end.event)?;
            if !end.samples.is_empty() {
                let pts = self
                    .origin
                    .unwrap_or_else(monotonic_now_ns)
                    .max(self.minimum_pts);
                if let Err(error) = self.normalizer.push(&end.samples, pts) {
                    self.account_rejected(shared, &end.samples)?;
                    return Err(error);
                }
                self.minimum_pts = self.minimum_pts.max(pts.saturating_add(native_frames_ns(
                    u64::try_from(end.samples.len() / usize::from(self.native.1)).map_err(
                        |_| {
                            Error::InvalidState(
                                "startup clock exhausted; reopen the capture stream".into(),
                            )
                        },
                    )?,
                    self.native.0,
                )));
                progress.pushed = true;
            }
        }
        if matches!(end, GenerationEnd::Replaced) && !progress.pushed {
            // Established DSP replacement discards its canonical remainder. Only
            // newly released startup audio changes that existing contract.
            advance_frame_index(
                &shared.capture_frame_index,
                self.normalizer.buffered_capture_frames() as u64,
                48_000,
            )?;
            return Ok(progress);
        }
        if let Err(error) = self.normalizer.flush() {
            shared.record_cleanup(error.with_context(ErrorContext::new(Operation::Flush)));
        }
        Ok(progress)
    }

    /// Terminal delivery is closed; retain the original format and account valid PCM.
    pub fn reject_pending(&mut self, shared: &SharedState) -> Result<bool> {
        let Some(end) = self.finish_startup() else {
            return Ok(false);
        };
        let mut discontinuity = self.report_drop(shared, end.event)?;
        if !end.samples.is_empty() {
            self.account_rejected(shared, &end.samples)?;
            discontinuity = true;
        }
        Ok(discontinuity)
    }

    fn finish_startup(&mut self) -> Option<StartupOutput<'static>> {
        self.startup
            .as_mut()
            .filter(|guard| guard.is_pending())
            .map(StartupSamples::finish)
    }

    fn report_drop(&mut self, shared: &SharedState, event: Option<Event>) -> Result<bool> {
        let Some(event) = event else {
            return Ok(false);
        };
        if let Event::StartupSamplesDropped { frames, .. } = &event {
            shared.advance_startup_gap(*frames, self.native.0)?;
            if let Some(origin) = self.origin {
                Self::anchor_epoch(shared, origin);
                self.minimum_pts = origin.saturating_add(native_frames_ns(*frames, self.native.0));
            }
        }
        shared.push_event(event);
        Ok(true)
    }

    fn account_rejected(&mut self, shared: &SharedState, samples: &[f32]) -> Result<()> {
        let Some(loss) =
            flexaudio_core::startup::discarded_startup_loss(samples, self.native.0, self.native.1)?
        else {
            return Ok(());
        };
        let count = loss.samples().map_or(0, NonZeroU64::get);
        shared.advance_startup_gap(count / u64::from(self.native.1), self.native.0)?;
        shared.discontinuity_pending.store(true, Ordering::SeqCst);
        shared.push_event(Event::AudioLoss { loss });
        Ok(())
    }

    fn anchor_epoch(shared: &SharedState, origin: i64) {
        shared
            .recording_epoch_ns
            .compare_exchange(i64::MIN, origin, Ordering::SeqCst, Ordering::SeqCst)
            .ok();
    }
}

fn native_frames_ns(frames: u64, rate: u32) -> i64 {
    let nanos = u128::from(frames) * 1_000_000_000 / u128::from(rate.max(1));
    i64::try_from(nanos).unwrap_or(i64::MAX)
}
