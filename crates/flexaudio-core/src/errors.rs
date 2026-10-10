//! Root failure kinds and safe default error messages.
use crate::error_context::{ErrorContext, ErrorGroup};
use crate::types::Permission;
use std::fmt;

/// Stable root failure classification, preserved through context and related errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorKind {
    /// Invalid argument.
    InvalidArg,
    /// Invalid lifecycle state.
    InvalidState,
    /// Device lookup completed without a match.
    DeviceNotFound,
    /// Recording permission denied.
    PermissionDenied,
    /// Unsupported OS version.
    UnsupportedOsVersion,
    /// Active device lost.
    DeviceLost,
    /// Backend failure.
    Backend,
    /// Unsupported PCM format.
    UnsupportedFormat,
    /// Negotiated native format changed.
    NativeFormatChanged,
    /// Unsupported operation.
    Unsupported,
    /// Device selection is ambiguous.
    AmbiguousDeviceName,
}

/// Typed failure with library-authored explanations and redacted private fields.
///
/// Message-bearing variants must contain safe, library-authored explanations, which may
/// include OS error text, never device names, paths, or raw environment diagnostics.
/// Display retains those explanations. Permission details and context call labels remain
/// available only through structured access.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// Invalid argument with a safe validation explanation.
    InvalidArg(String),
    /// Invalid state with a safe lifecycle explanation.
    InvalidState(String),
    /// Device lookup completed without a match.
    DeviceNotFound,
    /// Permission denied with explicit structured detail.
    PermissionDenied {
        /// Denied permission.
        permission: Permission,
        /// Raw diagnostic detail, opt-in only.
        detail: String,
    },
    /// Unsupported OS version.
    UnsupportedOsVersion,
    /// Capture device lost.
    DeviceLost,
    /// Backend failure with a safe explanation.
    Backend(String),
    /// Unsupported format with a safe validation explanation.
    UnsupportedFormat(String),
    /// Native format mismatch, retaining numeric pairs.
    NativeFormatChanged {
        /// Advertised rate/channels.
        advertised: (u32, u16),
        /// Actual rate/channels.
        actual: (u32, u16),
    },
    /// Unsupported operation.
    Unsupported,
    /// Selection matched multiple devices.
    AmbiguousDeviceName,
    /// Context preserving the source kind.
    Context {
        /// Underlying failure.
        source: Box<Error>,
        /// Structured operation/lane/native status.
        context: ErrorContext,
    },
    /// Primary with a nonempty list of related failures.
    Multiple(ErrorGroup),
}
impl Error {
    /// Root kind, recursively preserving the primary cause.
    pub fn kind(&self) -> ErrorKind {
        match self {
            Self::InvalidArg(_) => ErrorKind::InvalidArg,
            Self::InvalidState(_) => ErrorKind::InvalidState,
            Self::DeviceNotFound => ErrorKind::DeviceNotFound,
            Self::PermissionDenied { .. } => ErrorKind::PermissionDenied,
            Self::UnsupportedOsVersion => ErrorKind::UnsupportedOsVersion,
            Self::DeviceLost => ErrorKind::DeviceLost,
            Self::Backend(_) => ErrorKind::Backend,
            Self::UnsupportedFormat(_) => ErrorKind::UnsupportedFormat,
            Self::NativeFormatChanged { .. } => ErrorKind::NativeFormatChanged,
            Self::Unsupported => ErrorKind::Unsupported,
            Self::AmbiguousDeviceName => ErrorKind::AmbiguousDeviceName,
            Self::Context { source, .. } => source.kind(),
            Self::Multiple(group) => group.primary().kind(),
        }
    }
    /// Add context using one typed construction path.
    pub fn with_context(self, context: ErrorContext) -> Self {
        Self::Context {
            source: Box::new(self),
            context,
        }
    }
    /// Primary root for explicit structured projections and permission checks.
    pub fn root(&self) -> &Self {
        match self {
            Self::Context { source, .. } => source.root(),
            Self::Multiple(group) => group.primary().root(),
            error => error,
        }
    }
    /// Recording permission represented by the primary cause.
    pub fn permission(&self) -> Option<Permission> {
        match self.root() {
            Self::PermissionDenied { permission, .. } => Some(*permission),
            _ => None,
        }
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidArg(message) => write!(f, "invalid argument: {message}"),
            Self::InvalidState(message) => write!(f, "invalid state: {message}"),
            Self::DeviceNotFound => f.write_str("device not found"),
            Self::PermissionDenied { permission, .. } => write!(f, "{permission} recording permission denied or not granted. {}", permission.guidance()),
            Self::UnsupportedOsVersion => f.write_str("unsupported OS version"),
            Self::DeviceLost => f.write_str("device lost"),
            Self::Backend(message) => write!(f, "backend error: {message}"),
            Self::UnsupportedFormat(message) => write!(f, "unsupported output format: {message}"),
            Self::NativeFormatChanged { advertised, actual } => write!(f, "native input format changed from {} Hz/{} channels to {} Hz/{} channels; recreate the stream", advertised.0, advertised.1, actual.0, actual.1),
            Self::Unsupported => f.write_str("unsupported"),
            Self::AmbiguousDeviceName => f.write_str("device selection is ambiguous; select a unique device"),
            Self::Context { source, context } => write!(f, "{source} {context}"),
            Self::Multiple(group) => {
                write!(f, "{}", group.primary())?;
                for error in group.secondary() { write!(f, "; related failure: {error}")?; }
                Ok(())
            }
        }
    }
}
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Context { source, .. } => Some(source.as_ref()),
            Self::Multiple(group) => Some(group.primary()),
            _ => None,
        }
    }
}
