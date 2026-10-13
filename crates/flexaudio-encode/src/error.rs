//! Error and result types for flexaudio-encode.

/// Errors that can occur during flexaudio-encode operations.
///
/// Marked `#[non_exhaustive]` so variants can be added later (external matches need `_ =>`).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum EncodeError {
    /// File I/O failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// An unsupported parameter, such as channel count, sample rate, or chunk length.
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// An internal FLAC encoder error with a description.
    #[error("encoder error: {0}")]
    Encoder(String),
}

/// Result type used throughout flexaudio-encode.
pub type Result<T> = std::result::Result<T, EncodeError>;

impl EncodeError {
    /// Shared typed binding projection with opt-in original diagnostics.
    /// Safe Display never includes output paths or third-party encoder messages.
    pub fn audio_error(&self) -> flexaudio_core::Error {
        use flexaudio_core::{AddonErrorKind as Kind, Error};
        let kind = match self {
            Self::Io(_) => Kind::FlacIo,
            Self::Unsupported(_) => Kind::FlacUnsupported,
            Self::Encoder(_) => Kind::FlacEncoder,
        };
        Error::Addon {
            kind,
            message: self.to_string(),
        }
    }
}
