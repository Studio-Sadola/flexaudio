//! Immutable checked parameters for whisper.cpp pin 85a69493.

use crate::{VadError, VadParameter as Field, VadParameterReason as Reason};

pub(crate) const FRAME: u64 = 512;
pub(crate) const MAX_FRAMES: u64 = i32::MAX as u64 / FRAME;

/// The five segmentation fields from whisper.cpp; literal zero stays zero.
#[derive(Debug, Clone, PartialEq)]
pub struct VadOptions {
    /// Finite onset threshold in [0, 1]. Default 0.5.
    pub threshold: f32,
    /// Minimum unpadded speech duration, in 0..=134217 ms. Default 250.
    pub min_speech_ms: u32,
    /// Silence confirmation duration, in 0..=134217 ms. Default 100.
    pub min_silence_ms: u32,
    /// Maximum split budget in integer milliseconds; zero is unbounded.
    /// A positive value must exceed 32 + 2 * speech_pad_ms. The raw split
    /// budget is max_speech_ms * 16 - 512 - 2 * speech_pad_ms * 16 samples.
    /// Merging can undo splits, so this does not bound final duration or latency.
    pub max_speech_ms: u32,
    /// Boundary padding, in 0..=134217 ms. Default 30.
    pub speech_pad_ms: u32,
}

impl Default for VadOptions {
    fn default() -> Self {
        Self {
            threshold: 0.5,
            min_speech_ms: 250,
            min_silence_ms: 100,
            max_speech_ms: 0,
            speech_pad_ms: 30,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct DerivedParams {
    pub threshold: f32,
    pub negative: f32,
    pub min_speech: u64,
    pub min_silence: u64,
    pub pad: u64,
    pub max_speech: u64,
}

impl VadOptions {
    /// Validate all fields against the strict signed-int arithmetic domain of the pin.
    pub fn validate(&self) -> Result<(), VadError> {
        self.derive().map(|_| ())
    }

    pub(crate) fn derive(&self) -> Result<DerivedParams, VadError> {
        let invalid = |field, reason| VadError::InvalidParameter { field, reason };
        if !self.threshold.is_finite() {
            return Err(invalid(Field::Threshold, Reason::NotFinite));
        }
        if !(0.0..=1.0).contains(&self.threshold) {
            return Err(invalid(Field::Threshold, Reason::OutOfRange));
        }
        for (field, value) in [
            (Field::MinSpeechMs, self.min_speech_ms),
            (Field::MinSilenceMs, self.min_silence_ms),
            (Field::SpeechPadMs, self.speech_pad_ms),
        ] {
            if value > 134_217 {
                return Err(invalid(field, Reason::OutOfRange));
            }
        }
        let pad = u64::from(self.speech_pad_ms) * 16;
        let sentinel = u64::try_from(i32::MAX / 2).expect("positive constant");
        let max_speech = if self.max_speech_ms == 0 {
            sentinel
        } else {
            (u64::from(self.max_speech_ms) * 16)
                .checked_sub(FRAME + 2 * pad)
                .filter(|budget| *budget > 0)
                .ok_or_else(|| invalid(Field::MaxSpeechMs, Reason::OutOfRange))?
        };
        Ok(DerivedParams {
            threshold: self.threshold,
            negative: (self.threshold - 0.15_f32).max(0.01_f32),
            min_speech: u64::from(self.min_speech_ms) * 16,
            min_silence: u64::from(self.min_silence_ms) * 16,
            pad,
            max_speech,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn defaults_and_literal_zero() {
        let p = VadOptions::default().derive().unwrap();
        assert_eq!(
            (p.min_speech, p.min_silence, p.pad, p.max_speech),
            (4000, 1600, 480, 1073741823)
        );
        let p = VadOptions {
            threshold: 0.0,
            min_speech_ms: 0,
            min_silence_ms: 0,
            speech_pad_ms: 0,
            max_speech_ms: 0,
        }
        .derive()
        .unwrap();
        assert_eq!(
            (p.negative, p.min_speech, p.min_silence, p.pad, p.max_speech),
            (0.01, 0, 0, 0, 1073741823)
        );
    }
    #[test]
    fn checked_parameter_domain() {
        for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -0.1, 1.0001] {
            assert!(VadOptions {
                threshold: value,
                ..Default::default()
            }
            .validate()
            .is_err());
        }
        for field in 0..3 {
            let mut p = VadOptions::default();
            match field {
                0 => p.min_speech_ms = 134218,
                1 => p.min_silence_ms = 134218,
                _ => p.speech_pad_ms = 134218,
            }
            assert!(matches!(
                p.validate(),
                Err(VadError::InvalidParameter {
                    reason: Reason::OutOfRange,
                    ..
                })
            ));
            match field {
                0 => p.min_speech_ms = 134217,
                1 => p.min_silence_ms = 134217,
                _ => p.speech_pad_ms = 134217,
            }
            p.validate().unwrap();
        }
    }
    #[test]
    fn exact_integer_maximum_and_rejected_budgets() {
        let budget = |ms| {
            VadOptions {
                max_speech_ms: ms,
                ..Default::default()
            }
            .derive()
            .map(|params| params.max_speech)
        };
        assert_eq!(budget(0).unwrap(), 1_073_741_823);
        assert_eq!(budget(900).unwrap(), 12_928);
        assert_eq!(budget(1000).unwrap(), 14_528);
        assert_eq!(budget(30_900).unwrap(), 492_928);
        assert_eq!(budget(100_000_000).unwrap(), 1_599_998_528);
        assert_eq!(budget(100_001_000).unwrap(), 1_600_014_528);
        for ms in [1, 31, 32, 91, 92] {
            assert!(matches!(
                budget(ms),
                Err(VadError::InvalidParameter {
                    field: Field::MaxSpeechMs,
                    reason: Reason::OutOfRange,
                })
            ));
        }
        assert_eq!(budget(93).unwrap(), 16);
    }
}
