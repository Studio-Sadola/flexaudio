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
    /// Checked outcome preserving the primary and every cleanup failure.
    pub fn result(&self) -> Result<()> {
        let mut causes = self.primary.iter().chain(self.cleanup.iter()).cloned();
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
