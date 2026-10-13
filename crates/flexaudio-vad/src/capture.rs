//! Shared legacy VAD gap finalization and processing for capture bindings.

use crate::{Vad, VadError, VadEvent};

/// VAD state required to finalize a gap and anchor the next input.
/// Implementations own their sample timeline and reset it at a discontinuity.
pub trait VadCaptureEngine {
    /// Converted sample position before the next input.
    fn position(&self) -> u64;
    /// Finalize pre-gap speech, returning events on the old sample timeline.
    fn flush(&mut self) -> Result<Vec<VadEvent>, VadError>;
    /// Drain finalized events retained by a failed flush/reset operation.
    fn take_pending_events(&mut self) -> Vec<VadEvent> {
        Vec::new()
    }
    /// Recover fresh state, including after a failed flush.
    fn reset(&mut self) -> Result<(), VadError>;
}

impl VadCaptureEngine for Vad {
    fn position(&self) -> u64 {
        self.converted_sample_position()
    }
    fn flush(&mut self) -> Result<Vec<VadEvent>, VadError> {
        self.flush()
    }
    fn take_pending_events(&mut self) -> Vec<VadEvent> {
        self.take_pending_events()
    }
    fn reset(&mut self) -> Result<(), VadError> {
        self.reset()
    }
}

/// Finalized events and the independently fallible processing result.
///
/// A destructive successful flush must remain deliverable even when resetting,
/// preparing PCM or processing the next input fails. Callers must retain or
/// deliver `flushed` before propagating `processed`'s error. They must map flushed
/// timestamps using the pre-gap anchor, before installing `process_start_sample`.
#[must_use = "deliver or retain flushed events before reporting the processing result"]
pub struct VadCaptureOutcome {
    /// Events on the pre-gap sample timeline; never changed by later failures.
    pub flushed: Vec<VadEvent>,
    /// Position at which the processing step starts, absent if flush/reset failed.
    pub process_start_sample: Option<u64>,
    /// New events on the current timeline, or the original processing failure.
    pub processed: Result<Vec<VadEvent>, VadError>,
}

/// Finalize a discontinuity, reset, then prepare/process one input in one shared path.
///
/// `process` may prepare PCM (such as denoising) before calling `Vad::process_pcm`.
/// Both callbacks are injectable for deterministic tests without real audio.
/// Reset is attempted even after a flush error; both typed failures are retained and
/// the affected input is not processed. Successful flushed events survive either
/// a later reset error or a processing error.
pub fn process_capture_chunk<E: VadCaptureEngine>(
    engine: &mut E,
    discontinuity: bool,
    flush: impl FnOnce(&mut E) -> Result<Vec<VadEvent>, VadError>,
    process: impl FnOnce(&mut E) -> Result<Vec<VadEvent>, VadError>,
) -> VadCaptureOutcome {
    let mut flushed = Vec::new();
    if discontinuity {
        let result = flush(engine);
        flushed = engine.take_pending_events();
        let reset = engine.reset();
        match (result, reset) {
            (Ok(events), Ok(())) => flushed.extend(events),
            (Ok(events), Err(error)) => {
                flushed.extend(events);
                return VadCaptureOutcome {
                    flushed,
                    process_start_sample: None,
                    processed: Err(error),
                };
            }
            (Err(primary), reset) => {
                let error = match reset {
                    Ok(()) => primary,
                    Err(secondary) => VadError::Multiple {
                        primary: Box::new(primary),
                        secondary: Box::new(secondary),
                    },
                };
                return VadCaptureOutcome {
                    flushed,
                    process_start_sample: None,
                    processed: Err(error),
                };
            }
        }
    }
    VadCaptureOutcome {
        flushed,
        process_start_sample: Some(engine.position()),
        processed: process(engine),
    }
}

/// Process retained addon audio, then close/reset its old epoch through the shared gap path.
/// Tail events remain deliverable even if flush or reset subsequently fails. A tail failure
/// still attempts gap recovery and retains every independent typed failure.
pub fn finish_capture_gap<E: VadCaptureEngine>(
    engine: &mut E,
    process_tail: impl FnOnce(&mut E) -> Result<Vec<VadEvent>, VadError>,
) -> VadCaptureOutcome {
    let tail = process_capture_chunk(engine, false, |_| Ok(Vec::new()), process_tail);
    let gap = process_capture_chunk(engine, true, VadCaptureEngine::flush, |_| Ok(Vec::new()));
    let mut flushed = tail.flushed;
    let tail_error = match tail.processed {
        Ok(events) => {
            flushed.extend(events);
            None
        }
        Err(error) => Some(error),
    };
    flushed.extend(gap.flushed);
    let processed = match (tail_error, gap.processed) {
        (None, processed) => processed,
        (Some(primary), Ok(_)) => Err(primary),
        (Some(primary), Err(secondary)) if primary == secondary => Err(primary),
        (Some(primary), Err(secondary)) => Err(VadError::Multiple {
            primary: Box::new(primary),
            secondary: Box::new(secondary),
        }),
    };
    VadCaptureOutcome {
        flushed,
        process_start_sample: if processed.is_ok() {
            gap.process_start_sample
        } else {
            None
        },
        processed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DestructiveEngine {
        position: u64,
        open: bool,
        fail_process: bool,
        fail_reset: bool,
    }
    impl VadCaptureEngine for DestructiveEngine {
        fn position(&self) -> u64 {
            self.position
        }
        fn flush(&mut self) -> Result<Vec<VadEvent>, VadError> {
            let events = if std::mem::take(&mut self.open) {
                vec![VadEvent::SpeechEnd {
                    at_sample: self.position,
                }]
            } else {
                Vec::new()
            };
            self.position = 0;
            Ok(events)
        }
        fn reset(&mut self) -> Result<(), VadError> {
            self.position = 0;
            if std::mem::take(&mut self.fail_reset) {
                Err(VadError::Reset("injected one-shot reset failure".into()))
            } else {
                Ok(())
            }
        }
    }
    impl DestructiveEngine {
        fn process(&mut self) -> Result<Vec<VadEvent>, VadError> {
            if std::mem::take(&mut self.fail_process) {
                return Err(VadError::Inference(
                    "injected one-shot processing failure".into(),
                ));
            }
            self.position += 320;
            Ok(Vec::new())
        }
    }

    #[test]
    fn tail_events_survive_a_later_gap_reset_failure() {
        let mut engine = DestructiveEngine {
            position: 512,
            open: true,
            fail_process: false,
            fail_reset: true,
        };
        let outcome = finish_capture_gap(&mut engine, |_| {
            Ok(vec![VadEvent::SpeechStart { at_sample: 0 }])
        });
        assert_eq!(
            outcome.flushed,
            [
                VadEvent::SpeechStart { at_sample: 0 },
                VadEvent::SpeechEnd { at_sample: 512 }
            ]
        );
        assert!(matches!(outcome.processed, Err(VadError::Reset(_))));
    }

    #[test]
    fn flush_and_reset_failures_both_survive_gap() {
        let mut engine = DestructiveEngine {
            position: 0,
            open: false,
            fail_process: false,
            fail_reset: true,
        };
        let outcome = process_capture_chunk(
            &mut engine,
            true,
            |_| Err(VadError::Inference("injected flush cause".into())),
            |_| panic!("processing must not run"),
        );
        let error = outcome.processed.unwrap_err();
        let message = error.to_string();
        assert!(message.contains("injected flush cause"));
        assert!(message.contains("injected one-shot reset failure"));
        match error {
            VadError::Multiple { primary, secondary } => {
                assert!(matches!(*primary, VadError::Inference(_)));
                assert!(matches!(*secondary, VadError::Reset(_)));
            }
            _ => panic!("independent failures must retain their typed identity"),
        }
    }

    #[test]
    fn destructive_flush_survives_processing_failure_and_control() {
        for fail_process in [true, false] {
            let mut engine = DestructiveEngine {
                position: 1_024,
                open: true,
                fail_process,
                fail_reset: false,
            };
            let first = process_capture_chunk(
                &mut engine,
                true,
                DestructiveEngine::flush,
                DestructiveEngine::process,
            );
            assert_eq!(first.flushed, [VadEvent::SpeechEnd { at_sample: 1_024 }]);
            assert_eq!(first.process_start_sample, Some(0));
            assert_eq!(
                first.processed.is_err(),
                fail_process,
                "original processing error must remain visible"
            );
            let second = process_capture_chunk(
                &mut engine,
                true,
                DestructiveEngine::flush,
                DestructiveEngine::process,
            );
            assert!(
                second.flushed.is_empty(),
                "destructive flush events are delivered once"
            );
            assert!(
                second.processed.is_ok(),
                "one-shot failure permits later processing"
            );
        }
    }

    #[test]
    fn destructive_flush_survives_reset_failure() {
        let mut engine = DestructiveEngine {
            position: 512,
            open: true,
            fail_process: false,
            fail_reset: true,
        };
        let outcome = process_capture_chunk(
            &mut engine,
            true,
            DestructiveEngine::flush,
            DestructiveEngine::process,
        );
        assert_eq!(outcome.flushed, [VadEvent::SpeechEnd { at_sample: 512 }]);
        assert_eq!(outcome.process_start_sample, None);
        assert!(matches!(outcome.processed, Err(VadError::Reset(_))));
        assert_eq!(
            engine.position, 0,
            "processing must not run after failed reset"
        );
    }
}
