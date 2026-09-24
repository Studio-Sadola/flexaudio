//! Error and result types of flexaudio-encode.

/// Errors that flexaudio-encode operations can produce.
///
/// `#[non_exhaustive]` so that variants can be added in the future (external matches need
/// `_ =>`).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum EncodeError {
    /// File I/O failure.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// Unsupported parameter (channel count, sample rate, chunk length, etc.).
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// Internal FLAC encoder error (with a description).
    #[error("encoder error: {0}")]
    Encoder(String),
}

/// Result type used throughout flexaudio-encode.
pub type Result<T> = std::result::Result<T, EncodeError>;
