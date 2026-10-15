//! Delay-compensated arbitrary-format input using the shared capture DSP.

use crate::resample::PcmConverter;
use crate::{InferenceError, PcmFormat};

pub(crate) struct InputConverter {
    converter: PcmConverter,
    format: PcmFormat,
    input_scalars: u64,
    output_samples: u64,
    trim: usize,
    drained: bool,
}

impl InputConverter {
    pub fn new(format: PcmFormat) -> Result<Self, InferenceError> {
        let converter = PcmConverter::new_aligned(format).map_err(InferenceError::Resample)?;
        Ok(Self {
            trim: converter.output_delay(),
            converter,
            format,
            input_scalars: 0,
            output_samples: 0,
            drained: false,
        })
    }

    pub fn push(&mut self, samples: &[f32]) -> Result<Vec<f32>, InferenceError> {
        self.input_scalars = self
            .input_scalars
            .checked_add(u64::try_from(samples.len()).map_err(|_| overflow())?)
            .ok_or_else(overflow)?;
        let mut converted = Vec::new();
        self.converter
            .convert(samples, &mut converted)
            .map_err(InferenceError::Resample)?;
        self.retain_valid(&converted)
    }

    pub fn has_partial_frame(&self) -> bool {
        !self
            .input_scalars
            .is_multiple_of(u64::from(self.format.channels))
    }

    pub fn drain(&mut self) -> Result<Vec<f32>, InferenceError> {
        if self.drained {
            return Ok(Vec::new());
        }
        let mut output = Vec::new();
        let padding = vec![0.0; self.converter.input_block() * usize::from(self.format.channels)];
        // At most one source block plus filter history is pending. Zero extension
        // recovers that history, but only physical source samples are retained.
        while self.output_samples < self.expected_samples() {
            let mut converted = Vec::new();
            self.converter
                .convert(&padding, &mut converted)
                .map_err(InferenceError::Resample)?;
            output.extend(self.retain_valid(&converted)?);
        }
        self.drained = true;
        Ok(output)
    }

    fn expected_samples(&self) -> u64 {
        let frames = self.input_scalars / u64::from(self.format.channels);
        // The session's checked logical-sample limit bounds this conversion.
        u64::try_from((u128::from(frames) * 16_000).div_ceil(u128::from(self.format.sample_rate)))
            .expect("validated input sample domain")
    }

    fn retain_valid(&mut self, samples: &[f32]) -> Result<Vec<f32>, InferenceError> {
        if samples.iter().any(|sample| !sample.is_finite()) {
            return Err(InferenceError::Resample(
                "input conversion produced nonfinite PCM".into(),
            ));
        }
        let skip = self.trim.min(samples.len());
        self.trim -= skip;
        let remaining = self.expected_samples() - self.output_samples;
        let take = samples
            .len()
            .saturating_sub(skip)
            .min(usize::try_from(remaining).unwrap_or(usize::MAX));
        self.output_samples += u64::try_from(take).expect("bounded output length");
        // Sinc interpolation can overshoot valid full-scale source PCM. Keep
        // standalone model input normalized without changing the shared DSP or
        // the capture adapter's existing validation policy.
        Ok(samples[skip..skip + take]
            .iter()
            .map(|sample| sample.clamp(-1.0, 1.0))
            .collect())
    }
}

fn overflow() -> InferenceError {
    InferenceError::Resample("input converter sample count exceeds its supported domain".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn converter() -> InputConverter {
        let mut converter = InputConverter::new(PcmFormat {
            sample_rate: 16_000,
            channels: 2,
        })
        .unwrap();
        converter.input_scalars = 8;
        converter
    }

    #[test]
    fn finite_output_clips_both_signs_and_preserves_in_range_samples() {
        let mut converter = converter();
        assert_eq!(
            converter.retain_valid(&[-1.1, -0.5, 0.5, 1.1]).unwrap(),
            [-1.0, -0.5, 0.5, 1.0]
        );
        assert_eq!(converter.output_samples, 4);
    }

    #[test]
    fn nonfinite_output_retains_a_typed_cause_without_advancing_counters() {
        let mut converter = converter();
        converter.trim = 1;
        for invalid in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert!(matches!(
                converter.retain_valid(&[invalid, 0.5]),
                Err(InferenceError::Resample(_))
            ));
            assert_eq!(converter.trim, 1);
            assert_eq!(converter.output_samples, 0);
        }
    }
}
