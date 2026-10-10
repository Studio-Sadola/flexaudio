//! Sample-aligned capture conversion over the shared legacy DSP implementation.

use crate::resample::PcmConverter;

pub(crate) const CAPTURE_BLOCK: usize = 960;
pub(crate) const STARTUP_TRIM: usize = 21;

pub(crate) struct CaptureConverter {
    converter: PcmConverter,
    trim: usize,
    input_frames: u64,
    output_frames: u64,
    scratch: Vec<f32>,
}

impl CaptureConverter {
    pub fn new() -> Result<Self, ()> {
        Ok(Self {
            converter: PcmConverter::new_capture_48k().map_err(|_| ())?,
            trim: STARTUP_TRIM,
            input_frames: 0,
            output_frames: 0,
            scratch: Vec::new(),
        })
    }

    pub fn input_frames(&self) -> u64 {
        self.input_frames
    }

    #[cfg(test)]
    pub fn fail_conversion(&mut self) {
        self.converter.fail_conversion();
    }

    pub fn push(&mut self, stereo: &[f32]) -> Result<Vec<f32>, ()> {
        self.input_frames = self
            .input_frames
            .checked_add(u64::try_from(stereo.len() / 2).map_err(|_| ())?)
            .ok_or(())?;
        self.scratch.clear();
        self.converter
            .convert(stereo, &mut self.scratch)
            .map_err(|_| ())?;
        Ok(self.take_valid())
    }

    /// Zero extension recovers filter output, never contributes to the physical source length.
    /// Retained j has source coordinate 3*j, so exactly ceil(N/3) outputs are valid.
    pub fn drain(&mut self) -> Result<Vec<f32>, ()> {
        let mut out = Vec::new();
        if self.input_frames == 0 {
            return Ok(out);
        }
        let remainder = usize::try_from(self.input_frames % 960).map_err(|_| ())?;
        let mut padding = CAPTURE_BLOCK - remainder;
        while self.output_frames < self.input_frames.div_ceil(3) {
            self.scratch.clear();
            self.converter
                .convert(&vec![0.0; padding * 2], &mut self.scratch)
                .map_err(|_| ())?;
            out.extend(self.take_valid());
            padding = CAPTURE_BLOCK;
        }
        Ok(out)
    }

    fn take_valid(&mut self) -> Vec<f32> {
        let skip = self.trim.min(self.scratch.len());
        self.trim -= skip;
        let remaining = self.input_frames.div_ceil(3) - self.output_frames;
        let take = self
            .scratch
            .len()
            .saturating_sub(skip)
            .min(usize::try_from(remaining).unwrap_or(usize::MAX));
        self.output_frames += u64::try_from(take).expect("bounded output length");
        self.scratch[skip..skip + take].to_vec()
    }
}

#[cfg(test)]
#[path = "whisper_capture_tests.rs"]
mod tests;
