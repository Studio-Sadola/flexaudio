//! Stable typed addon causes shared by all binding projections.
use crate::ErrorKind;

/// Addon failure identity, independent of native or model diagnostic messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AddonErrorKind {
    /// VAD model loading failed; recreate the VAD session and retry.
    VadModelLoad,
    /// VAD inference failed; reset the VAD session and retry.
    VadInference,
    /// VAD configuration is invalid; correct the settings and retry.
    VadInvalidConfig,
    /// VAD input format is invalid; use a sample rate within 8000..=192000 Hz and nonzero channels.
    VadInvalidFormat,
    /// VAD resampling failed; reset the VAD session and retry.
    VadResample,
    /// VAD reset failed; recreate the VAD session and retry.
    VadReset,
    /// FLAC output I/O failed; check output access and available space, then retry.
    FlacIo,
    /// FLAC input is unsupported; correct the format and writer state, then retry.
    FlacUnsupported,
    /// FLAC encoding failed; recreate the writer and retry.
    FlacEncoder,
}
impl AddonErrorKind {
    /// Root core classification.
    pub fn error_kind(self) -> ErrorKind {
        match self {
            Self::VadModelLoad => ErrorKind::Backend,
            Self::VadInference => ErrorKind::Backend,
            Self::VadInvalidConfig => ErrorKind::InvalidArg,
            Self::VadInvalidFormat => ErrorKind::InvalidArg,
            Self::VadResample => ErrorKind::Backend,
            Self::VadReset => ErrorKind::Backend,
            Self::FlacIo => ErrorKind::Backend,
            Self::FlacUnsupported => ErrorKind::InvalidArg,
            Self::FlacEncoder => ErrorKind::Backend,
        }
    }
    /// Stable binding spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::VadModelLoad => "vadModelLoad",
            Self::VadInference => "vadInference",
            Self::VadInvalidConfig => "vadInvalidConfig",
            Self::VadInvalidFormat => "vadInvalidFormat",
            Self::VadResample => "vadResample",
            Self::VadReset => "vadReset",
            Self::FlacIo => "flacIo",
            Self::FlacUnsupported => "flacUnsupported",
            Self::FlacEncoder => "flacEncoder",
        }
    }
    /// Safe library-owned explanation and recovery guidance.
    pub fn explanation(self) -> &'static str {
        match self {
            Self::VadModelLoad => "VAD model loading failed; recreate the VAD session and retry.",
            Self::VadInference => "VAD inference failed; reset the VAD session and retry.",
            Self::VadInvalidConfig => {
                "VAD configuration is invalid; correct the settings and retry."
            }
            Self::VadInvalidFormat => {
                "VAD input format is invalid; use a sample rate within 8000..=192000 Hz and nonzero channels."
            }
            Self::VadResample => "VAD resampling failed; reset the VAD session and retry.",
            Self::VadReset => "VAD reset failed; recreate the VAD session and retry.",
            Self::FlacIo => {
                "FLAC output I/O failed; check output access and available space, then retry."
            }
            Self::FlacUnsupported => {
                "FLAC input is unsupported; correct the format and writer state, then retry."
            }
            Self::FlacEncoder => "FLAC encoding failed; recreate the writer and retry.",
        }
    }
}
