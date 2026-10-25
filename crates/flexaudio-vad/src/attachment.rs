//! Shared acceptance and failure retention for capture attachments.
use crate::{AttachedVadEvent, VadTapError, VadTapFailure};
use flexaudio_core::{terminal_causes::retain_terminal_cause, Error, ErrorContext, Operation};

/// Operation that produced an attachment result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VadAttachmentStage {
    /// Canonical capture processing.
    Process,
    /// Explicit epoch flush and reinitialization.
    RuntimeFlush,
    /// Final epoch flush during capture shutdown.
    StopFlush,
}

impl VadAttachmentStage {
    /// Binding-independent operation context.
    pub fn operation(self) -> Operation {
        match self {
            Self::Process => Operation::Normalize,
            Self::RuntimeFlush | Self::StopFlush => Operation::Flush,
        }
    }
}

/// One observed attachment failure, independent of capture's primary outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VadAttachmentFailure {
    /// Original typed attachment cause.
    pub error: VadTapError,
    /// Operation that observed the failure.
    pub stage: VadAttachmentStage,
}

impl VadAttachmentFailure {
    /// Uniform shutdown and notification projection, including operation context.
    pub fn audio_error(&self) -> Error {
        self.error
            .audio_error()
            .with_context(ErrorContext::new(self.stage.operation()))
    }
}

/// Retained attachment outcomes shared by all capture bindings.
#[derive(Debug, Default)]
pub struct VadAttachmentState {
    latest: Option<VadTapError>,
    failures: Vec<VadAttachmentFailure>,
}

impl VadAttachmentState {
    /// Most recently observed active failure, for explicit flush settlement.
    pub fn latest_error(&self) -> Option<&VadTapError> {
        self.latest.as_ref()
    }

    /// Distinct failures in observation order, retained across successful recovery.
    pub fn failures(&self) -> &[VadAttachmentFailure] {
        &self.failures
    }

    /// Uniform typed cleanup causes for a shutdown report.
    pub fn cleanup_errors(&self) -> impl Iterator<Item = Error> + '_ {
        self.failures.iter().map(VadAttachmentFailure::audio_error)
    }

    /// Aggregate retained failures using the core retention rule.
    pub fn retained_error(&self) -> Option<Error> {
        let mut retained = None;
        for error in self.cleanup_errors() {
            retain_terminal_cause(&mut retained, error);
        }
        retained
    }

    /// Accept one owner result and return ordered events plus any failure notification.
    /// `owner_failed` comes from the capture owner's lifecycle, so an empty successful
    /// batch clears the active failure only after the owner actually recovered.
    pub fn accept(
        &mut self,
        result: Result<Vec<AttachedVadEvent>, VadTapFailure>,
        stage: VadAttachmentStage,
        owner_failed: bool,
    ) -> (Vec<AttachedVadEvent>, Option<VadAttachmentFailure>) {
        match result {
            Ok(events) => {
                if stage != VadAttachmentStage::StopFlush && !owner_failed {
                    self.latest = None;
                }
                (events, None)
            }
            Err(failure) => {
                // FailedSession only reports the already latched lifecycle state;
                // it is not a new processing/reset failure.
                if failure.error == VadTapError::FailedSession && self.latest.is_some() {
                    return (failure.terminal_events, None);
                }
                self.latest = Some(failure.error.clone());
                let observed = VadAttachmentFailure {
                    error: failure.error,
                    stage,
                };
                let previous = self.retained_error();
                let mut retained = previous.clone();
                retain_terminal_cause(&mut retained, observed.audio_error());
                if retained != previous {
                    self.failures.push(observed.clone());
                }
                // Deduplication affects the report, not the caller's observation
                // of an actual failed operation after recovery or explicit retry.
                (failure.terminal_events, Some(observed))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_equal_failure_after_recovery_still_reaches_the_caller() {
        let mut state = VadAttachmentState::default();
        let failure = VadTapError::Vad(crate::VadError::Inference);
        for _ in 0..2 {
            assert!(state
                .accept(
                    Err(failure.clone().into()),
                    VadAttachmentStage::Process,
                    true,
                )
                .1
                .is_some());
            state.accept(Ok(Vec::new()), VadAttachmentStage::RuntimeFlush, false);
            assert!(state.latest_error().is_none());
        }
        assert_eq!(state.failures().len(), 1);
    }

    #[test]
    fn later_reset_failure_is_retained_and_is_the_flush_result() {
        let first = VadTapError::Vad(crate::VadError::Inference);
        let second = VadTapError::Cause {
            primary: Box::new(VadTapError::Vad(crate::VadError::Inference)),
            source: crate::InferenceError::Reset("reset failed".into()),
        };
        let expected_first = VadAttachmentFailure {
            error: first.clone(),
            stage: VadAttachmentStage::Process,
        }
        .audio_error();
        let expected_second = VadAttachmentFailure {
            error: second.clone(),
            stage: VadAttachmentStage::RuntimeFlush,
        }
        .audio_error();
        flexaudio_core::test_support::assert_distinct_failures_retained(
            expected_first,
            expected_second,
            |_, _| {
                let mut state = VadAttachmentState::default();
                state.accept(Err(first.clone().into()), VadAttachmentStage::Process, true);
                state.accept(
                    Err(second.clone().into()),
                    VadAttachmentStage::RuntimeFlush,
                    true,
                );
                state.accept(
                    Err(second.clone().into()),
                    VadAttachmentStage::RuntimeFlush,
                    true,
                );
                assert_eq!(state.latest_error(), Some(&second));
                assert_eq!(state.failures().len(), 2);
                state.retained_error().unwrap()
            },
        );
    }
}
