//! Binding-independent attached VAD events and capture validation errors.

use crate::{WhisperVadError, WhisperVadEvent};

/// Attached epochs introduce their capture origin before all standalone VAD payloads.
/// Standalone event types and sequence numbers are unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachedWhisperVadEvent {
    /// First event of a nonempty attached epoch, always sequence zero.
    EpochStart {
        /// Independent VAD epoch.
        epoch: u32,
        /// Always zero.
        seq: u64,
        /// Canonical 48 kHz frame index of the first retained mono16k sample.
        capture_sample: u64,
        /// Recording-zero nanoseconds at exactly that sample, independent of delivery time.
        pts_ns: i64,
    },
    /// Existing VAD payload, with sequence shifted by one for the attached origin event.
    Vad(WhisperVadEvent),
}

impl AttachedWhisperVadEvent {
    /// Independent epoch identity.
    pub fn epoch(&self) -> u32 {
        match self {
            Self::EpochStart { epoch, .. } => *epoch,
            Self::Vad(event) => event.epoch,
        }
    }

    /// Contiguous identity within the attached epoch.
    pub fn seq(&self) -> u64 {
        match self {
            Self::EpochStart { seq, .. } => *seq,
            Self::Vad(event) => event.seq,
        }
    }
}

/// Capture adapter errors, separate from the standalone canonical session contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WhisperVadTapError {
    /// Existing public capture failure identity with its original converter cause.
    Cause {
        /// Existing public capture failure identity.
        primary: Box<WhisperVadTapError>,
        /// Original typed converter cause.
        source: crate::VadError,
    },
    /// Shared model, parameter, inference or session failure.
    Vad(WhisperVadError),
    /// A capture feed must contain complete stereo frames.
    InvalidStereoLength,
    /// Canonical normalized PCM must be finite and within [-1,1].
    InvalidPcm {
        /// Stereo scalar index within the rejected feed.
        sample: usize,
    },
    /// A capture frame index or physical source count overflowed.
    CaptureSampleOverflow,
    /// PTS or its projected physical endpoint exceeded the chunk's safe integer domain.
    PtsOutOfRange,
    /// The locked converter could not establish its exact sample-aligned clock.
    UnsupportedConversionClock,
    /// Capture conversion failed; the epoch is incomplete and cannot produce successful EOF
    /// finals, because lost transport cannot be recovered from a fresh converter.
    Conversion,
    /// Capture intake has already stopped.
    Stopped,
    /// A fatal failure requires reinitialization through flush or the next discontinuity.
    FailedSession,
}

impl std::fmt::Display for WhisperVadTapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.audio_error().fmt(f)
    }
}
impl std::error::Error for WhisperVadTapError {}

impl WhisperVadTapError {
    /// Existing public failure identity, ignoring retained converter diagnostics.
    pub fn root(&self) -> &Self {
        match self {
            Self::Cause { primary, .. } => primary.root(),
            error => error,
        }
    }
    /// Original typed model or converter cause, available as structured data.
    pub fn cause(&self) -> Option<&crate::VadError> {
        match self {
            Self::Cause { source, .. } => Some(source),
            Self::Vad(error) => error.cause(),
            _ => None,
        }
    }
    /// One shared projection into a retained processor cleanup cause; capture stays independent.
    pub fn code(&self) -> flexaudio_core::WhisperCode {
        use flexaudio_core::WhisperCode;
        match self {
            Self::Cause { primary, .. } => primary.code(),
            Self::Vad(error) => match error.root() {
                WhisperVadError::InvalidParameter { .. } => WhisperCode::InvalidParameter,
                WhisperVadError::InvalidProbability { .. } => WhisperCode::InvalidProbability,
                WhisperVadError::InvalidPcm { .. } => WhisperCode::InvalidPcm,
                WhisperVadError::Overflow { .. } => WhisperCode::Overflow,
                WhisperVadError::SessionFinished => WhisperCode::SessionFinished,
                WhisperVadError::Inference => WhisperCode::Inference,
                WhisperVadError::ModelLoad => WhisperCode::ModelLoad,
                WhisperVadError::FailedSession => WhisperCode::FailedSession,
                WhisperVadError::Cause { primary, .. } => Self::Vad((**primary).clone()).code(),
            },
            Self::InvalidStereoLength => WhisperCode::InvalidStereoLength,
            Self::InvalidPcm { .. } => WhisperCode::InvalidPcm,
            Self::CaptureSampleOverflow => WhisperCode::CaptureSampleOverflow,
            Self::PtsOutOfRange => WhisperCode::PtsOutOfRange,
            Self::UnsupportedConversionClock => WhisperCode::UnsupportedConversionClock,
            Self::Conversion => WhisperCode::Conversion,
            Self::Stopped => WhisperCode::Stopped,
            Self::FailedSession => WhisperCode::FailedSession,
        }
    }

    /// Retain the typed processor failure independently of capture's outcome.
    pub fn audio_error(&self) -> flexaudio_core::Error {
        let primary = flexaudio_core::Error::WhisperVad { code: self.code() };
        match self.cause() {
            Some(source) => flexaudio_core::Error::Multiple(flexaudio_core::ErrorGroup::new(
                primary,
                source.audio_error(),
                Vec::new(),
            )),
            None => primary,
        }
    }
}

/// Drain the ordered event batch even on failure, before reporting the typed cause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhisperVadTapFailure {
    /// Typed cause; diagnostics contain no PCM.
    pub error: WhisperVadTapError,
    /// Already completed epoch events and exactly-once fatal closure, in publication order.
    pub terminal_events: Vec<AttachedWhisperVadEvent>,
}

impl std::fmt::Display for WhisperVadTapFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}
impl std::error::Error for WhisperVadTapFailure {}
impl From<WhisperVadTapError> for WhisperVadTapFailure {
    fn from(error: WhisperVadTapError) -> Self {
        Self {
            error,
            terminal_events: Vec::new(),
        }
    }
}

#[cfg(test)]
mod cause_tests {
    use super::*;
    #[test]
    fn nested_model_cause_is_retained_without_failing_capture_shutdown() {
        let cause = crate::VadError::Inference("injected model inference cause".into());
        let error = WhisperVadTapError::Vad(WhisperVadError::Cause {
            primary: Box::new(WhisperVadError::Inference),
            source: cause.clone(),
        });
        let projected = error.audio_error();
        assert_eq!(
            projected.whisper_code(),
            Some(flexaudio_core::WhisperCode::Inference)
        );
        match &projected {
            flexaudio_core::Error::Multiple(group) => {
                assert_eq!(group.secondary().next(), Some(&cause.audio_error()))
            }
            _ => panic!("original model cause must remain structured"),
        }
        assert!(flexaudio_core::ShutdownReport::new(None, vec![projected])
            .result()
            .is_ok());
    }
}
