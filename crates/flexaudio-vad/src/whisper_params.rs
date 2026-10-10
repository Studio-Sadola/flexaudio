//! Immutable checked parameters for whisper.cpp pin 85a69493.

use crate::{WhisperVadError, WhisperVadParameter as Field, WhisperVadParameterReason as Reason};

pub(crate) const FRAME: u64 = 512;
pub(crate) const MAX_FRAMES: u64 = i32::MAX as u64 / FRAME;

/// The five segmentation fields from whisper.cpp; literal zero stays zero.
#[derive(Debug, Clone, PartialEq)]
pub struct WhisperVadParams {
    /// Finite onset threshold in [0, 1]. Default 0.5.
    pub threshold: f32,
    /// Minimum unpadded speech duration, in 0..=134217 ms. Default 250.
    pub min_speech_duration_ms: u32,
    /// Silence confirmation duration, in 0..=134217 ms. Default 100.
    pub min_silence_duration_ms: u32,
    /// Finite nonnegative seconds, truncated before multiplication. Default f32::MAX.
    /// Merging can undo splits, so this does not bound final duration or latency.
    pub max_speech_duration_s: f32,
    /// Boundary padding, in 0..=134217 ms. Default 30.
    pub speech_pad_ms: u32,
}

impl Default for WhisperVadParams {
    fn default() -> Self {
        Self {
            threshold: 0.5,
            min_speech_duration_ms: 250,
            min_silence_duration_ms: 100,
            max_speech_duration_s: f32::MAX,
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

impl WhisperVadParams {
    /// Validate all fields against the strict signed-int arithmetic domain of the pin.
    pub fn validate(&self) -> Result<(), WhisperVadError> {
        self.derive().map(|_| ())
    }

    pub(crate) fn derive(&self) -> Result<DerivedParams, WhisperVadError> {
        let invalid = |field, reason| WhisperVadError::InvalidParameter { field, reason };
        for (field, value, upper) in [
            (Field::Threshold, self.threshold, 1.0),
            (
                Field::MaxSpeechDurationS,
                self.max_speech_duration_s,
                f32::MAX,
            ),
        ] {
            if !value.is_finite() {
                return Err(invalid(field, Reason::NotFinite));
            }
            if value < 0.0 || value > upper {
                return Err(invalid(field, Reason::OutOfRange));
            }
        }
        for (field, value) in [
            (Field::MinSpeechDurationMs, self.min_speech_duration_ms),
            (Field::MinSilenceDurationMs, self.min_silence_duration_ms),
            (Field::SpeechPadMs, self.speech_pad_ms),
        ] {
            if value > 134_217 {
                return Err(invalid(field, Reason::OutOfRange));
            }
        }
        let pad = u64::from(self.speech_pad_ms) * 16;
        let sentinel = u64::try_from(i32::MAX / 2).expect("positive constant");
        let max_speech = if self.max_speech_duration_s > 100_000.0 {
            sentinel
        } else {
            // The validated bound makes this truncating float conversion exact to the pin.
            let seconds = self.max_speech_duration_s.trunc() as i64;
            let budget = 16_000 * seconds - 512 - 2 * i64::try_from(pad).expect("bounded pad");
            if budget < 0 || budget > i64::from(i32::MAX) {
                sentinel
            } else {
                u64::try_from(budget).expect("nonnegative budget")
            }
        };
        Ok(DerivedParams {
            threshold: self.threshold,
            negative: (self.threshold - 0.15_f32).max(0.01_f32),
            min_speech: u64::from(self.min_speech_duration_ms) * 16,
            min_silence: u64::from(self.min_silence_duration_ms) * 16,
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
        let p = WhisperVadParams::default().derive().unwrap();
        assert_eq!(
            (p.min_speech, p.min_silence, p.pad, p.max_speech),
            (4000, 1600, 480, 1073741823)
        );
        let p = WhisperVadParams {
            threshold: 0.0,
            min_speech_duration_ms: 0,
            min_silence_duration_ms: 0,
            speech_pad_ms: 0,
            max_speech_duration_s: 0.0,
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
            assert!(WhisperVadParams {
                threshold: value,
                ..Default::default()
            }
            .validate()
            .is_err());
        }
        for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -0.1] {
            assert!(WhisperVadParams {
                max_speech_duration_s: value,
                ..Default::default()
            }
            .validate()
            .is_err());
        }
        for field in 0..3 {
            let mut p = WhisperVadParams::default();
            match field {
                0 => p.min_speech_duration_ms = 134218,
                1 => p.min_silence_duration_ms = 134218,
                _ => p.speech_pad_ms = 134218,
            }
            assert!(matches!(
                p.validate(),
                Err(WhisperVadError::InvalidParameter {
                    reason: Reason::OutOfRange,
                    ..
                })
            ));
            match field {
                0 => p.min_speech_duration_ms = 134217,
                1 => p.min_silence_duration_ms = 134217,
                _ => p.speech_pad_ms = 134217,
            }
            p.validate().unwrap();
        }
    }
    #[test]
    fn max_truncation_and_sentinels() {
        let derive = |s| {
            WhisperVadParams {
                max_speech_duration_s: s,
                ..Default::default()
            }
            .derive()
            .unwrap()
            .max_speech
        };
        assert_eq!(derive(30.9), derive(30.0));
        assert_eq!(derive(0.9), derive(f32::MAX));
        assert_eq!(derive(100001.0), 1073741823);
        assert_eq!(derive(100000.0), 1_599_998_528);
        assert_eq!(derive(1.0), 14_528);
    }
}
