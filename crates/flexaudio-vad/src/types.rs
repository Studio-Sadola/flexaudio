//! Typed contracts for the standalone streaming VAD.

/// A final half-open speech interval, rounded to the pinned 10 ms grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpeechSegment {
    /// Inclusive epoch-relative start in milliseconds.
    pub start_ms: u64,
    /// Exclusive epoch-relative end in milliseconds; may exceed physical EOF.
    pub end_ms: u64,
}

/// Probabilities inferred by the most recent successful call, including an EOF tail.
#[derive(Debug, Clone, Copy)]
pub struct FrameProbabilities<'a> {
    /// Index of the first frame in this batch. Each frame advances 32 ms.
    pub first_frame_index: u64,
    /// Borrowed probabilities, invalidated by the next mutation.
    pub values: &'a [f32],
}

/// Optional preview policy. Final segmentation is independent of this setting.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct VadRuntimeOptions {
    /// Emit immediate hysteresis hints and deterministic pieces of at most 30 seconds.
    pub provisional: bool,
}

/// Why an open preview hint closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewCloseReason {
    /// A probability fell below the derived lower threshold.
    Hysteresis,
    /// Successful EOF.
    Finish,
    /// Explicit successful reset.
    Reset,
    /// Fatal inference or reset failure.
    Error,
}

/// Why an immutable preview piece was emitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewCutReason {
    /// The fixed 30-second deadline.
    Limit,
    /// A probability fell below the derived lower threshold.
    Hysteresis,
    /// Successful EOF.
    Finish,
    /// Explicit successful reset.
    Reset,
    /// Fatal inference or reset failure.
    Error,
}

/// Why an epoch ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EpochEndReason {
    /// Successful EOF.
    Finish,
    /// Explicit successful reset.
    Reset,
    /// Fatal failure; pending canonical intervals were discarded.
    Error,
}

/// An ordered event in a standalone session. There is no capture-clock origin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VadEvent {
    /// Independent timeline, incremented only by successful reset.
    pub epoch: u32,
    /// Contiguous identity within the epoch, starting at zero.
    pub seq: u64,
    /// Typed event payload.
    pub kind: VadEventKind,
}

/// Final segmentation and optional provisional hints use distinct variants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VadEventKind {
    /// Immutable canonical segment, possibly delivered after unbounded lookahead.
    Segment(SpeechSegment),
    /// An immediate speech hint opened.
    ProvisionalSpeechStart {
        /// Epoch-relative milliseconds.
        at_ms: u64,
    },
    /// The matching speech hint closed.
    ProvisionalSpeechEnd {
        /// Epoch-relative milliseconds.
        at_ms: u64,
        /// Closing cause.
        reason: PreviewCloseReason,
    },
    /// A positive, immutable, nonoverlapping preview piece.
    ProvisionalCut {
        /// Inclusive epoch-relative milliseconds.
        start_ms: u64,
        /// Exclusive epoch-relative milliseconds.
        end_ms: u64,
        /// Cut cause.
        reason: PreviewCutReason,
    },
    /// The last event of an epoch.
    EpochEnd {
        /// Terminal cause.
        reason: EpochEndReason,
    },
}

/// Validated parameter identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VadParameter {
    /// Speech onset threshold.
    Threshold,
    /// First and second duration filters.
    MinSpeechMs,
    /// Silence confirmation duration.
    MinSilenceMs,
    /// Maximum raw interval budget in integer milliseconds.
    MaxSpeechMs,
    /// Boundary padding duration.
    SpeechPadMs,
    /// Unsupported source sample rate.
    InputSampleRate,
    /// Zero source channels.
    InputChannels,
    /// Changed source format within an epoch, or incomplete source frame at EOF.
    InputFormat,
}

/// Parameter rejection cause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VadParameterReason {
    /// NaN or infinity.
    NotFinite,
    /// Outside the strict pinned arithmetic domain.
    OutOfRange,
}

/// Checked operation that exceeded the supported domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VadOperation {
    /// Logical frame count times 512 exceeds INT_MAX.
    LogicalSamples,
    /// Physical PCM count overflow.
    InputSamples,
    /// Event sequence identity overflow.
    EventSequence,
    /// Epoch identity overflow.
    Epoch,
    /// Result or scratch storage cannot be reserved.
    Capacity,
}

/// Fail-closed errors. Diagnostics never contain PCM or probability values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VadError {
    /// Existing public failure identity with its original typed model/converter cause.
    Cause {
        /// Public failure identity, preserved for compatibility.
        primary: Box<VadError>,
        /// Original addon diagnostic, exposed only through structured access.
        source: crate::InferenceError,
    },
    /// An invalid construction-time parameter.
    InvalidParameter {
        /// Rejected field.
        field: VadParameter,
        /// Rejection cause.
        reason: VadParameterReason,
    },
    /// Invalid probability in the complete incoming feed; state is preserved.
    InvalidProbability {
        /// Absolute frame index.
        frame: u64,
    },
    /// Invalid normalized PCM in the complete incoming feed; state is preserved.
    InvalidPcm {
        /// Absolute sample index.
        sample: u64,
    },
    /// Checked arithmetic or storage preflight failed; state is preserved.
    Overflow {
        /// Operation that failed.
        operation: VadOperation,
    },
    /// Process was called after EOF.
    SessionFinished,
    /// Inference or model reset failed.
    Inference,
    /// Embedded model construction failed.
    ModelLoad,
    /// A previous fatal failure requires a successful reset.
    FailedSession,
}

impl std::fmt::Display for VadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "VAD error: {:?}", self.root())
    }
}
impl VadError {
    pub(crate) fn with_cause(self, source: crate::InferenceError) -> Self {
        Self::Cause {
            primary: Box::new(self),
            source,
        }
    }
    /// Shared standalone error projection, preserving validation/lifecycle classification
    /// and keeping model diagnostics as an opt-in related addon cause.
    pub fn audio_error(&self) -> flexaudio_core::Error {
        use flexaudio_core::{Error, ErrorGroup};
        let primary = match self.root() {
            Self::InvalidParameter { .. }
            | Self::InvalidProbability { .. }
            | Self::InvalidPcm { .. }
            | Self::Overflow { .. } => Error::InvalidArg(self.to_string()),
            Self::SessionFinished | Self::FailedSession => Error::InvalidState(self.to_string()),
            _ => Error::Vad {
                code: crate::VadTapError::Vad(self.clone()).code(),
            },
        };
        match self.cause() {
            Some(source) => {
                Error::Multiple(ErrorGroup::new(primary, source.audio_error(), Vec::new()))
            }
            None => primary,
        }
    }

    /// Original failure identity, ignoring retained diagnostic wrappers.
    pub fn root(&self) -> &Self {
        match self {
            Self::Cause { primary, .. } => primary.root(),
            error => error,
        }
    }
    /// Original typed addon cause, when native model or converter work failed.
    pub fn cause(&self) -> Option<&crate::InferenceError> {
        match self {
            Self::Cause { source, .. } => Some(source),
            _ => None,
        }
    }
}
impl std::error::Error for VadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.cause().map(|source| source as &dyn std::error::Error)
    }
}

/// Runtime errors carry exactly-once closure events even when a call fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VadFailure {
    /// Typed cause.
    pub error: VadError,
    /// Drain these through the normal event consumer; empty for validation errors.
    pub terminal_events: Vec<VadEvent>,
}
impl std::fmt::Display for VadFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}
impl std::error::Error for VadFailure {}
impl From<VadError> for VadFailure {
    fn from(error: VadError) -> Self {
        Self {
            error,
            terminal_events: Vec::new(),
        }
    }
}
