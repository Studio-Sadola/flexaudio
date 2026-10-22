//! Typed failure context and retained shutdown outcomes.
use crate::types::MixLane;
use crate::{Error, Result};
use std::fmt;

/// Operation that failed, independent of diagnostic strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Operation {
    /// Device inventory query.
    Enumerate,
    /// Capture startup.
    Start,
    /// Audio normalization.
    Normalize,
    /// Processing tail drain.
    Flush,
    /// Automatic capture reopen.
    Reopen,
    /// Restore the previous capture source.
    Rollback,
    /// Producer shutdown.
    Stop,
    /// Worker join.
    Join,
    /// Native audio routing.
    Link,
}
impl fmt::Display for Operation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Enumerate => "enumerate",
            Self::Start => "start",
            Self::Normalize => "normalize",
            Self::Flush => "flush",
            Self::Reopen => "reopen",
            Self::Rollback => "rollback",
            Self::Stop => "stop",
            Self::Join => "join",
            Self::Link => "link",
        })
    }
}

/// Native status; call labels are available only through explicit structured access.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum NativeStatus {
    /// Windows HRESULT bit pattern.
    HResult {
        /// Library-owned call identifier.
        call: &'static str,
        /// Unsigned HRESULT bits.
        bits: u32,
    },
    /// Apple OSStatus value.
    OsStatus {
        /// Library-owned call identifier.
        call: &'static str,
        /// Signed OSStatus.
        value: i32,
    },
}

/// Context attached without replacing the root failure kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ErrorContext {
    operation: Operation,
    lane: Option<MixLane>,
    native_status: Option<NativeStatus>,
}
impl ErrorContext {
    /// Construct operation-only context.
    pub fn new(operation: Operation) -> Self {
        Self {
            operation,
            lane: None,
            native_status: None,
        }
    }
    /// Attach the failing Mix lane.
    pub fn with_lane(mut self, lane: MixLane) -> Self {
        self.lane = Some(lane);
        self
    }
    /// Attach a structured native status.
    pub fn with_native_status(mut self, status: NativeStatus) -> Self {
        self.native_status = Some(status);
        self
    }
    /// Failed operation.
    pub fn operation(&self) -> Operation {
        self.operation
    }
    /// Failing Mix lane, if any.
    pub fn lane(&self) -> Option<MixLane> {
        self.lane
    }
    /// Native status, if any.
    pub fn native_status(&self) -> Option<NativeStatus> {
        self.native_status
    }
}
impl fmt::Display for ErrorContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "during {}", self.operation)?;
        if let Some(lane) = self.lane {
            f.write_str(match lane {
                MixLane::Microphone => " (microphone)",
                MixLane::SystemAudio => " (system audio)",
            })?;
        }
        match self.native_status {
            Some(NativeStatus::HResult { bits, .. }) => write!(f, " (HRESULT 0x{bits:08X})"),
            Some(NativeStatus::OsStatus { value, .. }) => write!(f, " (OSStatus {value})"),
            None => Ok(()),
        }
    }
}

/// A primary failure with at least one ordered related failure.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ErrorGroup {
    primary: Box<Error>,
    first_secondary: Box<Error>,
    remaining_secondary: Vec<Error>,
}
impl ErrorGroup {
    /// Construct a nonempty group.
    pub fn new(primary: Error, first_secondary: Error, remaining_secondary: Vec<Error>) -> Self {
        Self {
            primary: Box::new(primary),
            first_secondary: Box::new(first_secondary),
            remaining_secondary,
        }
    }
    /// Primary failure.
    pub fn primary(&self) -> &Error {
        &self.primary
    }
    /// Related failures in observation order.
    pub fn secondary(&self) -> impl Iterator<Item = &Error> {
        std::iter::once(self.first_secondary.as_ref()).chain(self.remaining_secondary.iter())
    }
}

/// Completed teardown; capture primary and cleanup causes remain distinct.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ShutdownReport {
    primary: Option<Error>,
    cleanup: Vec<Error>,
}

/// Internal field inventory and cardinalities, with no stability guarantee.
/// The exhaustive destructure makes new fields a compile error until represented.
#[cfg(any(test, feature = "test-support"))]
pub fn shutdown_fields(report: &ShutdownReport) -> [(&'static str, usize); 2] {
    let ShutdownReport { primary, cleanup } = report;
    [
        ("primary", usize::from(primary.is_some())),
        ("cleanup", cleanup.len()),
    ]
}
impl ShutdownReport {
    /// Construct a completed report, including a clean empty outcome.
    pub fn new(primary: Option<Error>, cleanup: Vec<Error>) -> Self {
        Self { primary, cleanup }
    }
    /// First capture failure; cleanup-only failures do not become capture failures.
    pub fn primary(&self) -> Option<&Error> {
        self.primary.as_ref()
    }
    /// Cleanup failures in observation order.
    pub fn cleanup(&self) -> &[Error] {
        &self.cleanup
    }
    /// Checked capture outcome. Isolated VAD cleanup causes remain in the report but do not
    /// fail stop; a capture or other cleanup failure still retains all related causes.
    pub fn result(&self) -> Result<()> {
        let first = match self.primary.as_ref() {
            Some(primary) => primary,
            None => match self.cleanup.iter().find(|error| error.vad_code().is_none()) {
                Some(error) => error,
                None => return Ok(()),
            },
        };
        let mut causes = std::iter::once(first.clone()).chain(
            self.cleanup
                .iter()
                .filter(|error| !std::ptr::eq(*error, first))
                .cloned(),
        );
        match (causes.next(), causes.next()) {
            (None, _) => Ok(()),
            (Some(error), None) => Err(error),
            (Some(primary), Some(first)) => Err(Error::Multiple(ErrorGroup::new(
                primary,
                first,
                causes.collect(),
            ))),
        }
    }
}

#[cfg(test)]
mod vad_tests {
    use super::*;
    use crate::VadErrorCode;

    #[test]
    fn vad_only_shutdown_succeeds_and_retains_the_typed_cause() {
        let cause = Error::Vad {
            code: VadErrorCode::Inference,
        }
        .with_context(ErrorContext::new(Operation::Flush));
        let report = ShutdownReport::new(None, vec![cause.clone()]);
        assert!(report.result().is_ok());
        assert!(report.primary().is_none());
        assert_eq!(report.cleanup(), &[cause]);
        assert_eq!(
            report.cleanup()[0].vad_code(),
            Some(VadErrorCode::Inference)
        );
        assert!(report.cleanup()[0]
            .to_string()
            .contains("model inference or reset could not complete"));
        assert!(ShutdownReport::new(None, Vec::new()).result().is_ok());
    }

    #[test]
    fn capture_shutdown_message_excludes_isolated_vad_related_causes() {
        let capture = Error::DeviceLost
            .with_context(ErrorContext::new(Operation::Stop))
            .with_context(ErrorContext::new(Operation::Stop));
        for operation in [Operation::Normalize, Operation::Flush] {
            let vad = Error::Vad {
                code: VadErrorCode::Inference,
            }
            .with_context(ErrorContext::new(operation));
            let report = ShutdownReport::new(Some(capture.clone()), vec![vad.clone()]);
            let error = report
                .result()
                .expect_err("capture loss must still fail stop");
            assert_eq!(error.to_string(), "device lost during stop during stop");
            assert_eq!(error.root(), &Error::DeviceLost);
            assert_eq!(report.cleanup(), std::slice::from_ref(&vad));
            match error {
                Error::Multiple(group) => {
                    assert_eq!(group.secondary().collect::<Vec<_>>(), [&vad]);
                    assert_eq!(
                        group.secondary().next().and_then(Error::vad_code),
                        Some(VadErrorCode::Inference)
                    );
                }
                _ => panic!("the rejecting error must retain typed related failures"),
            }
        }
    }

    #[test]
    fn capture_shutdown_message_retains_non_vad_related_causes() {
        let capture = Error::DeviceLost.with_context(ErrorContext::new(Operation::Stop));
        let cleanup = Error::Backend("the backend could not finish its shutdown".into())
            .with_context(ErrorContext::new(Operation::Join));
        let vad = Error::Vad {
            code: VadErrorCode::Inference,
        }
        .with_context(ErrorContext::new(Operation::Flush));
        let report = ShutdownReport::new(Some(capture), vec![vad, cleanup]);
        let error = report
            .result()
            .expect_err("capture loss must still fail stop");
        assert_eq!(error.to_string(), "device lost during stop; related failure: backend error: the backend could not finish its shutdown during join");
        assert!(matches!(error, Error::Multiple(group) if group.secondary().count() == 2));
    }

    #[test]
    fn real_shutdown_failures_remain_primary_and_retain_vad_related_causes() {
        let vad = Error::Vad {
            code: VadErrorCode::Conversion,
        };
        let capture = Error::DeviceLost;
        let report = ShutdownReport::new(Some(capture.clone()), vec![vad.clone()]);
        let error = report
            .result()
            .expect_err("capture loss must still fail stop");
        assert_eq!(error.root(), &capture);
        assert!(
            matches!(error, Error::Multiple(ref group) if group.secondary().next() == Some(&vad))
        );

        // A real cleanup failure is the rejecting cause, even if VAD failed first.
        let cleanup = Error::Backend("the backend could not finish its shutdown".into());
        let report = ShutdownReport::new(None, vec![vad.clone(), cleanup.clone()]);
        let error = report
            .result()
            .expect_err("backend teardown failure must still fail stop");
        assert_eq!(error.root(), &cleanup);
        assert!(
            matches!(error, Error::Multiple(ref group) if group.secondary().next() == Some(&vad))
        );
    }
}
