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
        if super::lifecycle_json::project(&event).is_some() {
            return Ok(());
        }
        match event {
            Event::TerminalError { error } | Event::ShutdownError { error } => Err(error),
            Event::Error(detail) => {
                Err(Error::Backend("legacy capture failure".into()).with_diagnostic_detail(detail))
            }
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
            Event::StartupSamplesDropped { frames, reason } => {
                eprintln!(
                    "Warning: suppressed {frames} leading microphone frames ({})",
                    reason.as_str()
                );
                Ok(())
            }
            Event::Clipped => {
                eprintln!("Warning: upstream audio was clipped; exact output chunk attribution is unavailable");
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
            LossReason::GenerationEnded => "terminal generation closure",
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
pub(super) fn drain_capture_events(
    stream: &mut Stream,
    lifecycle_events: bool,
) -> Result<(), String> {
    let mut reporter = CaptureEventReporter::default();
    let mut failure = None;
    let mut stopped = false;
    loop {
        let Some(event) = stream.poll_event() else {
            if !stopped {
                if let Some(error) = stream.terminal_error() {
                    retain_failure(&mut failure, error);
                    if let Err(error) = stream.stop_checked() {
                        retain_failure(&mut failure, error);
                    }
                    stopped = true;
                    // Teardown may append diagnostics even without a terminal event.
                    continue;
                }
            }
            break;
        };
        match super::lifecycle_json::route(&event, lifecycle_events, &mut std::io::stderr().lock())
        {
            Ok(true) => continue,
            Ok(false) => {}
            Err(error) => retain_failure(
                &mut failure,
                Error::Backend("failed to write lifecycle JSON".into())
                    .with_diagnostic_detail(error.to_string()),
            ),
        }
        if let Err(error) = reporter.report(event) {
            retain_failure(&mut failure, error);
            if !stopped {
                // Stop once, then continue through events produced by teardown.
                if let Err(error) = stream.stop_checked() {
                    retain_failure(&mut failure, error);
                }
                stopped = true;
            }
        }
    }
    reporter.flush();
    if let Some(error) = stream.terminal_error() {
        retain_failure(&mut failure, error);
    }
    if let Some(report) = stream.shutdown_report() {
        if let Err(error) = report.result() {
            retain_failure(&mut failure, error);
        }
    }
    failure.map_or(Ok(()), |error| Err(failure_message(error)))
}

fn retain_failure(retained: &mut Option<Error>, error: Error) {
    if let Error::Multiple(group) = error {
        for error in std::iter::once(group.primary()).chain(group.secondary()) {
            retain_failure(retained, error.clone());
        }
    } else {
        flexaudio::core::terminal_causes::retain_terminal_cause(retained, error);
    }
}

fn failure_message(error: Error) -> String {
    if let Error::Multiple(group) = error {
        std::iter::once(group.primary())
            .chain(group.secondary())
            .map(|error| failure_message(error.clone()))
            .collect::<Vec<_>>()
            .join("\nRelated failure: ")
    } else {
        super::describe_error(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU64;

    #[test]
    fn distinct_reporter_and_shutdown_failures_are_both_returned() {
        flexaudio_core::test_support::assert_distinct_failures_retained(
            Error::DeviceLost,
            Error::Backend("later shutdown failure".into()),
            |first, second| {
                let mut retained = None;
                retain_failure(&mut retained, first);
                retain_failure(&mut retained, second.clone());
                retain_failure(
                    &mut retained,
                    Error::Multiple(flexaudio::ErrorGroup::new(
                        Error::DeviceLost,
                        second,
                        vec![],
                    )),
                );
                let error = retained.unwrap();
                let message = failure_message(error.clone());
                assert!(message.contains("input device was lost"));
                assert!(message.contains("later shutdown failure"));
                error
            },
        );
    }

    #[test]
    fn capture_drain_returns_capture_and_later_stop_failure() {
        struct FailsOnStop {
            event: Option<Event>,
        }
        impl flexaudio::CaptureBackend for FailsOnStop {
            fn native_format(&self) -> (u32, u16) {
                (48_000, 2)
            }
            fn start(&mut self, _sink: flexaudio::core::RawSink) -> Result<(), Error> {
                Ok(())
            }
            fn stop(&mut self) {}
            fn stop_checked(&mut self) -> Result<(), Error> {
                Err(Error::Backend("later checked shutdown failure".into()))
            }
            fn poll_event(&mut self) -> Option<Event> {
                self.event.take()
            }
        }
        let mut stream = Stream::open(
            flexaudio::StreamConfig::default(),
            Box::new(FailsOnStop {
                event: Some(Event::TerminalError {
                    error: Error::DeviceLost,
                }),
            }),
        )
        .unwrap();
        stream.start().unwrap();
        // Reconcile the native mailbox synchronously before testing the final CLI drain.
        stream.stop();
        let message = drain_capture_events(&mut stream, false).unwrap_err();
        assert!(message.contains("input device was lost"), "{message}");
        assert!(
            message.contains("later checked shutdown failure"),
            "{message}"
        );
    }

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
