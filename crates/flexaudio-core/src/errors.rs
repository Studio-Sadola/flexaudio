//! Root failure kinds and safe default error messages.
use crate::error_context::{ErrorContext, ErrorGroup};
use crate::types::Permission;
use crate::VadErrorCode;
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
    /// Strict validation rejected requested settings that would be ignored.
    IgnoredSettings,
}

/// Typed failure with library-authored explanations and redacted private fields.
///
/// Message-bearing variants must contain safe, library-authored explanations, which may
/// include OS error text, never device names, paths, or raw environment diagnostics.
/// Display retains those explanations. Permission details and context call labels remain
/// available only through structured access.
#[derive(Debug, Clone)]
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
    /// Addon failure with its stable identity and opt-in diagnostic message.
    Addon {
        /// Stable addon cause.
        kind: crate::AddonErrorKind,
        /// Original diagnostic, available through structured access only.
        message: String,
    },
    /// Optional VAD processor failure; capture remains usable.
    Vad {
        /// Stable typed processor cause.
        code: VadErrorCode,
    },
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
    /// Opt-in native diagnostic detail, excluded from safe display.
    Diagnostic {
        /// Typed underlying failure.
        source: Box<Error>,
        /// Native diagnostic text, available only through structured access.
        detail: String,
    },
    /// Context preserving the source kind.
    Context {
        /// Underlying failure.
        source: Box<Error>,
        /// Structured operation/lane/native status.
        context: ErrorContext,
    },
    /// Primary with a nonempty list of related failures.
    Multiple(ErrorGroup),
    /// Strict validation rejected the complete candidate settings snapshot.
    IgnoredSettings {
        /// All candidate settings, including every ignored request.
        report: crate::SettingsReport,
    },
}
impl Error {
    /// Root kind, recursively preserving the primary cause.
    pub fn kind(&self) -> ErrorKind {
        match self {
            Self::IgnoredSettings { .. } => ErrorKind::IgnoredSettings,
            Self::InvalidArg(_) => ErrorKind::InvalidArg,
            Self::InvalidState(_) => ErrorKind::InvalidState,
            Self::DeviceNotFound => ErrorKind::DeviceNotFound,
            Self::PermissionDenied { .. } => ErrorKind::PermissionDenied,
            Self::UnsupportedOsVersion => ErrorKind::UnsupportedOsVersion,
            Self::DeviceLost => ErrorKind::DeviceLost,
            Self::Backend(_) => ErrorKind::Backend,
            Self::Addon { kind, .. } => kind.error_kind(),
            Self::Vad { .. } => ErrorKind::Backend,
            Self::UnsupportedFormat(_) => ErrorKind::UnsupportedFormat,
            Self::NativeFormatChanged { .. } => ErrorKind::NativeFormatChanged,
            Self::Unsupported => ErrorKind::Unsupported,
            Self::AmbiguousDeviceName => ErrorKind::AmbiguousDeviceName,
            Self::Context { source, .. } | Self::Diagnostic { source, .. } => source.kind(),
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
    /// Retain opt-in native text without exposing it in the default message.
    pub fn with_diagnostic_detail(self, detail: String) -> Self {
        Self::Diagnostic {
            source: Box::new(self),
            detail,
        }
    }
    /// Native diagnostic text along the primary cause chain, outermost first.
    pub fn diagnostic_details(&self) -> Vec<&str> {
        match self {
            Self::Diagnostic { source, detail } => {
                let mut details = vec![detail.as_str()];
                details.extend(source.diagnostic_details());
                details
            }
            Self::Context { source, .. } => source.diagnostic_details(),
            Self::Multiple(group) => group.primary().diagnostic_details(),
            _ => Vec::new(),
        }
    }
    /// Primary root for explicit structured projections and permission checks.
    pub fn root(&self) -> &Self {
        match self {
            Self::Context { source, .. } | Self::Diagnostic { source, .. } => source.root(),
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
    /// Candidate settings retained through primary context and diagnostic wrappers.
    pub fn ignored_settings(&self) -> Option<&crate::SettingsReport> {
        match self.root() {
            Self::IgnoredSettings { report } => Some(report),
            _ => None,
        }
    }
    /// Addon cause retained through primary context wrappers.
    pub fn addon_kind(&self) -> Option<crate::AddonErrorKind> {
        match self.root() {
            Self::Addon { kind, .. } => Some(*kind),
            _ => None,
        }
    }
    /// Optional VAD cause retained through primary context wrappers.
    pub fn vad_code(&self) -> Option<VadErrorCode> {
        match self.root() {
            Self::Vad { code } => Some(*code),
            _ => None,
        }
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IgnoredSettings { .. } => f.write_str("requested capture settings would be ignored"),
            Self::InvalidArg(message) => write!(f, "invalid argument: {message}"),
            Self::InvalidState(message) => write!(f, "invalid state: {message}"),
            Self::DeviceNotFound => f.write_str("device not found"),
            Self::PermissionDenied { permission, .. } => write!(f, "{permission} recording permission denied or not granted. {}", permission.guidance()),
            Self::UnsupportedOsVersion => f.write_str("unsupported OS version"),
            Self::DeviceLost => f.write_str("device lost"),
            Self::Backend(message) => write!(f, "backend error: {message}"),
            Self::Addon { kind, .. } => f.write_str(kind.explanation()),
            Self::Vad { code } => write!(f, "VAD failed ({}): {}", code.as_str(), code.explanation()),
            Self::UnsupportedFormat(message) => write!(f, "unsupported output format: {message}"),
            Self::NativeFormatChanged { advertised, actual } => write!(f, "native input format changed from {} Hz/{} channels to {} Hz/{} channels; recreate the stream", advertised.0, advertised.1, actual.0, actual.1),
            Self::Unsupported => f.write_str("unsupported"),
            Self::AmbiguousDeviceName => f.write_str("device selection is ambiguous; select a unique device"),
            Self::Diagnostic { source, .. } => source.fmt(f),
            Self::Context { source, context } => write!(f, "{source} {context}"),
            Self::Multiple(group) => {
                write!(f, "{}", group.primary())?;
                // Isolated VAD causes remain in the structured group and shutdown cleanup
                // report. They do not change the capture primary's diagnostic message.
                for error in group.secondary().filter(|error| error.vad_code().is_none()) { write!(f, "; related failure: {error}")?; }
                Ok(())
            }
        }
    }
}
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Context { source, .. } | Self::Diagnostic { source, .. } => Some(source.as_ref()),
            Self::Multiple(group) => Some(group.primary()),
            _ => None,
        }
    }
}
