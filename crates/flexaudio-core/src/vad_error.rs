//! Stable optional-processor failure codes, independent of inference dependencies.

/// Typed cause of an isolated VAD failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VadErrorCode {
    /// A segmentation parameter is outside the supported domain.
    InvalidParameter,
    /// An inference probability is not finite or normalized.
    InvalidProbability,
    /// PCM is not finite or normalized.
    InvalidPcm,
    /// Checked arithmetic or storage capacity was exceeded.
    Overflow,
    /// Processing followed a completed session without reset.
    SessionFinished,
    /// Inference or model reset failed.
    Inference,
    /// The embedded model could not be constructed.
    ModelLoad,
    /// A previous failure requires a successful reset.
    FailedSession,
    /// Capture input did not contain complete stereo frames.
    InvalidStereoLength,
    /// A capture sample count exceeded the supported domain.
    CaptureSampleOverflow,
    /// The capture timestamp exceeded its supported domain.
    PtsOutOfRange,
    /// Conversion could not establish an exact sample-aligned clock.
    UnsupportedConversionClock,
    /// Capture conversion failed.
    Conversion,
    /// Processing followed stopped capture intake.
    Stopped,
}

impl VadErrorCode {
    /// Stable code spelling shared by language bindings.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidParameter => "InvalidParameter",
            Self::InvalidProbability => "InvalidProbability",
            Self::InvalidPcm => "InvalidPcm",
            Self::Overflow => "Overflow",
            Self::SessionFinished => "SessionFinished",
            Self::Inference => "Inference",
            Self::ModelLoad => "ModelLoad",
            Self::FailedSession => "FailedSession",
            Self::InvalidStereoLength => "InvalidStereoLength",
            Self::CaptureSampleOverflow => "CaptureSampleOverflow",
            Self::PtsOutOfRange => "PtsOutOfRange",
            Self::UnsupportedConversionClock => "UnsupportedConversionClock",
            Self::Conversion => "Conversion",
            Self::Stopped => "Stopped",
        }
    }

    /// Safe explanation of why this processor failed; contains no input or device details.
    pub fn explanation(self) -> &'static str {
        match self {
            Self::InvalidParameter => "a segmentation parameter is outside the supported domain",
            Self::InvalidProbability => "an inference probability is not finite or normalized",
            Self::InvalidPcm => "PCM contains a nonfinite or out-of-range sample",
            Self::Overflow => "checked arithmetic or storage exceeds the supported domain",
            Self::SessionFinished => {
                "the completed session requires a successful reset before processing"
            }
            Self::Inference => "model inference or reset could not complete",
            Self::ModelLoad => "the embedded model could not be constructed",
            Self::FailedSession => "a previous processor failure requires a successful reset",
            Self::InvalidStereoLength => "capture input does not contain complete stereo frames",
            Self::CaptureSampleOverflow => "the capture sample count exceeds the supported domain",
            Self::PtsOutOfRange => "the capture timestamp exceeds the supported domain",
            Self::UnsupportedConversionClock => {
                "conversion cannot establish an exact sample-aligned clock"
            }
            Self::Conversion => "capture conversion could not preserve the valid sample timeline",
            Self::Stopped => "capture intake has already stopped",
        }
    }
}
