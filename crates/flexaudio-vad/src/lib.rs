//! Offline voice activity detection with an embedded Silero v6 model.
//!
//! One streaming session and one probability segmenter provide final millisecond
//! intervals, optional provisional hints, and explicit epoch boundaries.
//!
//! ```no_run
//! use flexaudio_vad::{Vad, VadOptions, VadRuntimeOptions};
//! let mut vad = Vad::new(VadOptions::default(), VadRuntimeOptions::default())?;
//! let mut events = vad.process(&[0.0; 16000])?;
//! events.extend(vad.finish()?);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

#![warn(missing_docs)]

mod attachment;
mod capture_converter;
mod infer;
mod input_converter;
mod params;
mod preview;
mod resample;
mod stream;
mod tap;
mod tap_types;
#[cfg(feature = "test-faults")]
#[doc(hidden)]
pub mod test_faults;
mod types;
mod whisper_cpp_segments;

pub use attachment::{VadAttachmentFailure, VadAttachmentStage, VadAttachmentState};
pub use flexaudio_core::VadErrorCode;
pub use params::VadOptions;
pub use resample::PcmFormat;
pub use stream::{speech_segments, Vad};
pub use tap::VadTap;
pub use tap_types::*;
pub use types::*;
pub use whisper_cpp_segments::VadPostProcessor;

/// VAD errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InferenceError {
    /// Original failure with another independently failed operation.
    Multiple {
        /// First observed failure.
        primary: Box<InferenceError>,
        /// Additional failure, retained with its typed identity.
        secondary: Box<InferenceError>,
    },
    /// Failed to load the model.
    ModelLoad(String),
    /// Inference failed.
    Inference(String),
    /// Invalid configuration.
    InvalidConfig(String),
    /// Invalid input PCM format; no state was changed.
    InvalidFormat(String),
    /// PCM conversion failed.
    Resample(String),
    /// Reset failed; the instance cannot process until reset succeeds.
    Reset(String),
}

impl std::fmt::Display for InferenceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InferenceError::Multiple { primary, secondary } => {
                write!(f, "{primary}; related failure: {secondary}")
            }
            InferenceError::ModelLoad(m) => write!(f, "model load error: {m}"),
            InferenceError::Inference(m) => write!(f, "inference error: {m}"),
            InferenceError::InvalidConfig(m) => write!(f, "invalid config: {m}"),
            InferenceError::InvalidFormat(m) => write!(f, "invalid PCM format: {m}"),
            InferenceError::Resample(m) => write!(f, "resampling error: {m}"),
            InferenceError::Reset(m) => write!(f, "reset error: {m}"),
        }
    }
}

impl std::error::Error for InferenceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Multiple { primary, .. } => Some(primary.as_ref()),
            _ => None,
        }
    }
}

impl InferenceError {
    /// One shared typed projection for standalone, capture and shutdown binding failures.
    /// The original model diagnostic stays available as structured data, never safe Display.
    pub fn audio_error(&self) -> flexaudio_core::Error {
        use flexaudio_core::{AddonErrorKind as Kind, Error, ErrorGroup};
        let (kind, message) = match self {
            Self::Multiple { primary, secondary } => {
                return Error::Multiple(ErrorGroup::new(
                    primary.audio_error(),
                    secondary.audio_error(),
                    Vec::new(),
                ))
            }
            Self::ModelLoad(message) => (Kind::VadModelLoad, message),
            Self::Inference(message) => (Kind::VadInference, message),
            Self::InvalidConfig(message) => (Kind::VadInvalidConfig, message),
            Self::InvalidFormat(message) => (Kind::VadInvalidFormat, message),
            Self::Resample(message) => (Kind::VadResample, message),
            Self::Reset(message) => (Kind::VadReset, message),
        };
        Error::Addon {
            kind,
            message: message.clone(),
        }
    }
}

trait InferenceBackend: Send {
    fn infer(&mut self, frame: &[f32]) -> Result<f32, InferenceError>;
    fn reset(&mut self) -> Result<(), InferenceError>;
}
