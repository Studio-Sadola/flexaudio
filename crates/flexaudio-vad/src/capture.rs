//! Shared legacy VAD gap finalization and processing for capture bindings.

use crate::{Vad, VadError, VadEvent};

/// VAD state required to finalize a gap and anchor the next input.
/// Implementations own their sample timeline and reset it at a discontinuity.
pub trait VadCaptureEngine {
    /// Converted sample position before the next input.
    fn position(&self) -> u64;
    /// Finalize pre-gap speech, returning events on the old sample timeline.
    fn flush(&mut self) -> Result<Vec<VadEvent>, VadError>;
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
/// Reset is attempted even after a flush error; the original flush error wins and
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
        let reset = engine.reset();
        match result {
            Ok(events) => flushed = events,
            Err(error) => {
                return VadCaptureOutcome {
                    flushed,
                    process_start_sample: None,
                    processed: Err(error),
                };
            }
        }
        if let Err(error) = reset {
            return VadCaptureOutcome {
                flushed,
                process_start_sample: None,
                processed: Err(error),
            };
        }
    }
    VadCaptureOutcome {
        flushed,
        process_start_sample: Some(engine.position()),
        processed: process(engine),
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
