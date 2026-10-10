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
    /// Capture transport or conversion failed; the epoch is incomplete and cannot produce
    /// successful EOF finals. Includes missing/reordered canonical capture frames.
    Conversion,
    /// Capture intake has already stopped.
    Stopped,
    /// A fatal failure requires successful reinitialization through flush.
    FailedSession,
}

impl std::fmt::Display for WhisperVadTapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "whisper VAD tap error: {self:?}")
    }
}
impl std::error::Error for WhisperVadTapError {}

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
