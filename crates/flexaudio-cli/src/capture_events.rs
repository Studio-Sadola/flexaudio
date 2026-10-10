//! Typed capture diagnostics shared by WAV and stdout recording.
use flexaudio::core::{AudioLoss, AudioPath, Error, LossReason, MixLane, OutputTap};
use flexaudio::{Event, Stream};

#[derive(Default)]
struct CaptureEventReporter {
    losses: Vec<LossSummary>,
}

struct LossSummary {
    loss: AudioLoss,
    samples: Option<u64>,
}

impl CaptureEventReporter {
    fn report(&mut self, event: Event) -> Result<(), Error> {
        match event {
            Event::TerminalError { error } | Event::ShutdownError { error } => Err(error),
            Event::Error(_) => Err(Error::Backend("legacy capture failure".into())),
            Event::PermissionDenied { permission, detail } => {
                Err(Error::PermissionDenied { permission, detail })
            }
            Event::RecoverableError { error } => {
                eprintln!("Warning: {error}; capture and retries may continue");
                Ok(())
            }
            Event::AudioLoss { loss } => {
                self.add_loss(loss);
                Ok(())
            }
            Event::Clipped => {
                eprintln!("Warning: upstream audio was clipped; exact output chunk attribution is unavailable");
                Ok(())
            }
            Event::PermissionGranted => {
                eprintln!("Microphone recording permission granted");
                Ok(())
            }
            Event::PermissionPending { detail, .. }
            | Event::SilenceWhileSourceActive { detail } => {
                eprintln!("Warning: {detail}");
                Ok(())
            }
            Event::ChunkDropped { count } => {
                eprintln!("Warning: primary output dropped {count} chunks in total");
                Ok(())
            }
            Event::StreamStalled => {
                eprintln!("Warning: capture stalled; waiting for recovery");
                Ok(())
            }
            Event::StreamRecovered => {
                eprintln!("Capture recovered");
                Ok(())
            }
            Event::DeviceLost => {
                eprintln!("Warning: capture device lost; waiting for recovery");
                Ok(())
            }
            _ => {
                eprintln!("Unknown capture event");
                Ok(())
            }
        }
    }

    fn add_loss(&mut self, loss: AudioLoss) {
        if let Some(summary) = self.losses.iter_mut().find(|summary| {
            summary.loss.path() == loss.path()
                && summary.loss.reason() == loss.reason()
                && summary.loss.sample_rate() == loss.sample_rate()
                && summary.loss.channels() == loss.channels()
        }) {
            summary.samples = summary
                .samples
                .zip(loss.samples())
                .and_then(|(total, count)| total.checked_add(count.get()));
        } else {
            self.losses.push(LossSummary {
                loss,
                samples: loss.samples().map(|count| count.get()),
            });
        }
    }

    fn flush(&mut self) {
        for summary in self.losses.drain(..) {
            eprintln!("{}", summary.message());
        }
    }
}

impl LossSummary {
    fn message(&self) -> String {
        let path = match self.loss.path() {
            AudioPath::Capture { lane: None } => "capture",
            AudioPath::Capture { lane: Some(lane) } => match lane {
                MixLane::Microphone => "microphone capture",
                MixLane::SystemAudio => "system audio capture",
                _ => "capture (unknown lane)",
            },
            AudioPath::MixFifo { lane } => match lane {
                MixLane::Microphone => "microphone mix FIFO",
                MixLane::SystemAudio => "system audio mix FIFO",
                _ => "mix FIFO (unknown lane)",
            },
            AudioPath::Output { tap } => match tap {
                OutputTap::Primary => "primary output",
                OutputTap::Secondary => "secondary output",
                _ => "output (unknown tap)",
            },
            _ => "unknown audio path",
        };
        let reason = match self.loss.reason() {
            LossReason::RawOverflow => "raw capture overflow",
            LossReason::MixFifoOverflow => "mix FIFO overflow",
            LossReason::CorruptBuffer => "corrupt buffer",
            LossReason::MalformedBuffer => "malformed buffer",
            LossReason::CallbackRejected => "callback rejected",
            LossReason::OutputOverflow => "output overflow",
            _ => "unknown loss reason",
        };
        let count = self.samples.map_or_else(
            || "unknown scalar sample count".into(),
            |samples| format!("{samples} scalar interleaved samples"),
        );
        format!(
            "Warning: audio loss at {path}: {reason}, {count}, {} Hz/{} channels",
            self.loss.sample_rate(),
            self.loss.channels(),
        )
    }
}

#[cfg(test)]
pub(super) fn report_capture_event(event: Event) -> Result<(), Error> {
    let mut reporter = CaptureEventReporter::default();
    let result = reporter.report(event);
    reporter.flush();
    result
}

/// Drain diagnostics even after failure, then inspect the retained checked outcome.
pub(super) fn drain_capture_events(stream: &mut Stream) -> Result<(), String> {
    let mut reporter = CaptureEventReporter::default();
    let mut failure = None;
    loop {
        let Some(event) = stream.poll_event() else {
            if failure.is_none() {
                if let Some(error) = stream.terminal_error() {
                    failure = Some(error);
                    let _ = stream.stop_checked();
                    // Teardown may append diagnostics even without a terminal event.
                    continue;
                }
            }
            break;
        };
        if let Err(error) = reporter.report(event) {
            if failure.is_none() {
                failure = Some(error);
                // Stop once, then continue through events produced by teardown.
                let _ = stream.stop_checked();
            } else {
                eprintln!("Related failure: {}", super::describe_error(error));
            }
        }
    }
    reporter.flush();
    if let Some(error) = stream.terminal_error() {
        failure = Some(error);
    }
    if let Some(report) = stream.shutdown_report() {
        if let Err(error) = report.result() {
            return Err(super::describe_error(error));
        }
    }
    failure.map_or(Ok(()), |error| Err(super::describe_error(error)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU64;

    #[test]
    fn loss_coalescing_preserves_path_format_and_unknown_counts() {
        let mut reporter = CaptureEventReporter::default();
        for (samples, rate) in [(Some(5), 48_000), (Some(7), 48_000), (Some(3), 16_000)] {
            reporter.add_loss(
                AudioLoss::raw_overflow(None, samples.and_then(NonZeroU64::new), rate, 2).unwrap(),
            );
        }
        assert_eq!(reporter.losses.len(), 2);
        assert_eq!(reporter.losses[0].samples, Some(12));
        assert!(reporter.losses[0]
            .message()
            .contains("12 scalar interleaved samples"));
        reporter.add_loss(AudioLoss::raw_overflow(None, None, 48_000, 2).unwrap());
        assert_eq!(reporter.losses[0].samples, None);
        reporter.add_loss(
            AudioLoss::output_overflow(OutputTap::Secondary, NonZeroU64::new(3), 48_000, 2)
                .unwrap(),
        );
        assert_eq!(reporter.losses.len(), 3);
        reporter.flush();
        assert!(reporter.losses.is_empty());
    }

    #[test]
    fn loss_count_overflow_is_unknown() {
        let mut reporter = CaptureEventReporter::default();
        for samples in [u64::MAX, 1] {
            reporter.add_loss(
                AudioLoss::raw_overflow(None, NonZeroU64::new(samples), 48_000, 2).unwrap(),
            );
        }
        assert_eq!(reporter.losses[0].samples, None);
        assert!(reporter.losses[0]
            .message()
            .contains("unknown scalar sample count"));
    }
}
