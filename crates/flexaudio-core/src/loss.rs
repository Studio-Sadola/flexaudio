//! Validated interval loss reports in scalar interleaved samples.
use crate::{Error, Result};
use std::num::{NonZeroU16, NonZeroU32, NonZeroU64};

/// Lane of a composite capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MixLane {
    /// Microphone child.
    Microphone,
    /// System audio child.
    SystemAudio,
}
/// Output tap that discarded audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum OutputTap {
    /// Primary output.
    Primary,
    /// Secondary output.
    Secondary,
}
/// Location at which samples were discarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AudioPath {
    /// Capture transport, native or canonical, optionally attributed to a Mix child.
    Capture {
        /// Child lane, if applicable.
        lane: Option<MixLane>,
    },
    /// A Mix child's canonical FIFO.
    MixFifo {
        /// Child owning the FIFO.
        lane: MixLane,
    },
    /// Finished output queue.
    Output {
        /// Output owning the queue.
        tap: OutputTap,
    },
}
/// Reason samples were discarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LossReason {
    /// Capture PCM ring capacity exceeded, before native or canonical processing intake.
    RawOverflow,
    /// Mix child FIFO capacity exceeded.
    MixFifoOverflow,
    /// Native buffer marked corrupt.
    CorruptBuffer,
    /// Native buffer layout invalid.
    MalformedBuffer,
    /// Realtime callback rejected.
    CallbackRejected,
    /// Finished output ring capacity exceeded.
    OutputOverflow,
    /// Valid buffered capture could not be delivered after terminal generation closure.
    GenerationEnded,
}

/// Rejections that can occur at the native capture boundary.
#[derive(Clone, Copy)]
#[non_exhaustive]
pub(crate) enum CaptureRejection {
    CorruptBuffer,
    MalformedBuffer,
    CallbackRejected,
}

/// Validated interval loss; unknown is distinct from a positive count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct AudioLoss {
    path: AudioPath,
    reason: LossReason,
    samples: Option<NonZeroU64>,
    sample_rate: NonZeroU32,
    channels: NonZeroU16,
}
impl AudioLoss {
    fn new(
        path: AudioPath,
        reason: LossReason,
        samples: Option<NonZeroU64>,
        rate: u32,
        channels: u16,
    ) -> Result<Self> {
        Ok(Self {
            path,
            reason,
            samples,
            sample_rate: NonZeroU32::new(rate)
                .ok_or_else(|| Error::InvalidArg("loss sample rate is zero, so discarded audio cannot be timed; report the positive sample rate of the discarded buffer".into()))?,
            channels: NonZeroU16::new(channels)
                .ok_or_else(|| Error::InvalidArg("loss channel count is zero, so discarded audio cannot be counted in frames; report the positive channel count of the discarded buffer".into()))?,
        })
    }
    /// Capture PCM ring overflow with the discard-point format and exact scalar count,
    /// or unknown. The canonical capture queue reports its own 48 kHz stereo format.
    pub fn raw_overflow(
        lane: Option<MixLane>,
        samples: Option<NonZeroU64>,
        rate: u32,
        channels: u16,
    ) -> Result<Self> {
        Self::new(
            AudioPath::Capture { lane },
            LossReason::RawOverflow,
            samples,
            rate,
            channels,
        )
    }
    /// Valid buffered capture discarded because terminal failure closed delivery.
    /// Counts use the buffered native format, before normalization.
    pub fn generation_ended(samples: Option<NonZeroU64>, rate: u32, channels: u16) -> Result<Self> {
        Self::new(
            AudioPath::Capture { lane: None },
            LossReason::GenerationEnded,
            samples,
            rate,
            channels,
        )
    }
    /// Canonical Mix FIFO loss (48 kHz stereo).
    pub fn mix_fifo_overflow(lane: MixLane, samples: Option<NonZeroU64>) -> Self {
        Self {
            path: AudioPath::MixFifo { lane },
            reason: LossReason::MixFifoOverflow,
            samples,
            sample_rate: NonZeroU32::new(48_000).expect("positive canonical rate"),
            channels: NonZeroU16::new(2).expect("positive canonical channels"),
        }
    }
    /// Finished output loss in this tap's output format.
    pub fn output_overflow(
        tap: OutputTap,
        samples: Option<NonZeroU64>,
        rate: u32,
        channels: u16,
    ) -> Result<Self> {
        Self::new(
            AudioPath::Output { tap },
            LossReason::OutputOverflow,
            samples,
            rate,
            channels,
        )
    }
    pub(crate) fn rejected(
        rejection: CaptureRejection,
        samples: Option<NonZeroU64>,
        rate: u32,
        channels: u16,
    ) -> Result<Self> {
        Self::new(
            AudioPath::Capture { lane: None },
            match rejection {
                CaptureRejection::CorruptBuffer => LossReason::CorruptBuffer,
                CaptureRejection::MalformedBuffer => LossReason::MalformedBuffer,
                CaptureRejection::CallbackRejected => LossReason::CallbackRejected,
            },
            samples,
            rate,
            channels,
        )
    }
    /// Attach a lane only to an already validated capture loss.
    pub fn with_capture_lane(mut self, lane: MixLane) -> Result<Self> {
        match &mut self.path {
            AudioPath::Capture { lane: slot @ None } => {
                *slot = Some(lane);
                Ok(self)
            }
            _ => Err(Error::InvalidArg(
                "only unattributed capture loss can acquire a Mix lane".into(),
            )),
        }
    }
    /// Discard location.
    pub fn path(&self) -> AudioPath {
        self.path
    }
    /// Discard reason.
    pub fn reason(&self) -> LossReason {
        self.reason
    }
    /// Scalar interleaved samples; None means unknown.
    pub fn samples(&self) -> Option<NonZeroU64> {
        self.samples
    }
    /// Replace a coalesced count while retaining the validated path and format.
    /// None represents an unknown count, including arithmetic overflow.
    pub fn with_sample_count(mut self, samples: Option<NonZeroU64>) -> Self {
        self.samples = samples;
        self
    }
    /// Sample rate at discard point.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate.get()
    }
    /// Channel count at discard point.
    pub fn channels(&self) -> u16 {
        self.channels.get()
    }
}
