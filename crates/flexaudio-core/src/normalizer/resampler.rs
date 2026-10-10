//! Rubato conversion and final input drain.
use super::*;
impl ResamplerState {
    /// Create a fixed-ratio resampler with `channels` channels from `in_sr` to `out_sr`.
    ///
    /// Returns [`Error::Backend`] if rubato construction fails (avoids a panic silently stopping
    /// the thread).
    pub(super) fn new(in_sr: u32, out_sr: u32, channels: usize) -> Result<Self> {
        let ratio = out_sr as f64 / in_sr as f64;
        // Fixed input chunk of about 20ms worth of input frames (rubato retains any remainder internally).
        let chunk_in_frames = (in_sr as usize / 50).max(64);

        let params = SincInterpolationParameters {
            sinc_len: 128,
            f_cutoff: 0.95,
            interpolation: SincInterpolationType::Linear,
            oversampling_factor: 128,
            window: WindowFunction::BlackmanHarris2,
        };

        let inner = Async::<f32>::new_sinc(
            ratio,
            1.0, // Fixed ratio (no variable resampling needed)
            &params,
            chunk_in_frames,
            channels,
            FixedAsync::Input,
        )
        .map_err(|e| Error::Backend(format!("rubato sinc resampler construction failed: {e}")))?;

        let max_out_frames = inner.output_frames_max();

        Ok(Self {
            inner,
            channels,
            chunk_in_frames,
            max_out_frames,
            in_accum: Vec::with_capacity(chunk_in_frames * channels * 4),
            out_scratch: vec![0.0; max_out_frames * channels],
        })
    }

    /// Resample as much of `in_accum` as possible in `chunk_in_frames` units and append the
    /// resulting interleaved samples to `out_buf`. Returns the number of output frames generated.
    ///
    /// Returns [`Error::Backend`] if constructing a rubato adapter or calling `process_into_buffer`
    /// fails (avoids a panic silently stopping the capture thread).
    pub(super) fn drain_into(&mut self, out_buf: &mut Vec<f32>) -> Result<u64> {
        let step = self.chunk_in_frames * self.channels;
        let mut produced = 0u64;

        while self.in_accum.len() >= step {
            let in_adapter =
                InterleavedSlice::new(&self.in_accum[..step], self.channels, self.chunk_in_frames)
                    .map_err(|e| {
                        Error::Backend(format!("rubato interleaved input adapter failed: {e}"))
                    })?;

            let mut out_adapter = InterleavedSlice::new_mut(
                &mut self.out_scratch[..],
                self.channels,
                self.max_out_frames,
            )
            .map_err(|e| {
                Error::Backend(format!("rubato interleaved output adapter failed: {e}"))
            })?;

            let indexing = Indexing {
                input_offset: 0,
                output_offset: 0,
                partial_len: None,
                active_channels_mask: None,
            };

            let (_in_used, out_written) = self
                .inner
                .process_into_buffer(&in_adapter, &mut out_adapter, Some(&indexing))
                .map_err(|e| Error::Backend(format!("rubato process_into_buffer failed: {e}")))?;

            let n_samples = out_written * self.channels;
            out_buf.extend_from_slice(&self.out_scratch[..n_samples]);
            produced += out_written as u64;

            // Remove consumed input (`FixedAsync::Input` always consumes `chunk_in_frames`).
            self.in_accum.drain(..step);
        }
        Ok(produced)
    }

    /// On stop, process the less-than-one-chunk remainder in `in_accum` using `partial_len`, append
    /// the resulting interleaved samples to `out_buf`, and return the number of output frames.
    ///
    /// This drains the remaining input (just under 20ms at most), leaving `in_accum` empty. It does
    /// not flush the resampler's internal filter group delay (a few ms).
    pub(super) fn flush_into(&mut self, out_buf: &mut Vec<f32>) -> Result<u64> {
        let remaining = self.in_accum.len() / self.channels;
        if remaining == 0 {
            return Ok(0);
        }
        // Pad input with silence to one full chunk and pass only the valid length via `partial_len`.
        self.in_accum
            .resize(self.chunk_in_frames * self.channels, 0.0);

        let in_adapter = InterleavedSlice::new(
            &self.in_accum[..self.chunk_in_frames * self.channels],
            self.channels,
            self.chunk_in_frames,
        )
        .map_err(|e| Error::Backend(format!("rubato interleaved input adapter failed: {e}")))?;

        let mut out_adapter = InterleavedSlice::new_mut(
            &mut self.out_scratch[..],
            self.channels,
            self.max_out_frames,
        )
        .map_err(|e| Error::Backend(format!("rubato interleaved output adapter failed: {e}")))?;

        let indexing = Indexing {
            input_offset: 0,
            output_offset: 0,
            partial_len: Some(remaining),
            active_channels_mask: None,
        };

        let (_in_used, out_written) = self
            .inner
            .process_into_buffer(&in_adapter, &mut out_adapter, Some(&indexing))
            .map_err(|e| Error::Backend(format!("rubato flush process_into_buffer failed: {e}")))?;

        let n_samples = out_written * self.channels;
        out_buf.extend_from_slice(&self.out_scratch[..n_samples]);
        self.in_accum.clear();
        Ok(out_written as u64)
    }
}

impl OutputStage {
    /// Create the output stage for the output sample rate and channel count.
    ///
    /// If `out_sample_rate == 48000`, sample-rate conversion is bypassed (only channel conversion
    /// is performed). rubato construction failures propagate as [`Error::Backend`].
    pub(super) fn new(out_sample_rate: u32, out_channels: usize) -> Result<Self> {
        let resampler = if out_sample_rate == SAMPLE_RATE {
            None
        } else {
            // Convert from the 48000-Hz internal canonical form to `out_sample_rate` with `out_channels` channels.
            Some(ResamplerState::new(
                SAMPLE_RATE,
                out_sample_rate,
                out_channels,
            )?)
        };
        Ok(Self {
            out_channels,
            resampler,
            ch_scratch: Vec::with_capacity(CHUNK_FRAMES * out_channels),
        })
    }

    /// Process the internal canonical form (48k/stereo interleaved, any length) and append
    /// interleaved samples in the output format to `out_buf`. Length must be a multiple of
    /// `INNER_CH` (stereo).
    pub(super) fn process_inner(
        &mut self,
        inner_stereo: &[f32],
        out_buf: &mut Vec<f32>,
    ) -> Result<()> {
        let frames = inner_stereo.len() / INNER_CH;
        if frames == 0 {
            return Ok(());
        }

        // Channel conversion: stereo → out_channels.
        self.ch_scratch.clear();
        match self.out_channels {
            1 => {
                // stereo → mono (L/R average).
                for f in 0..frames {
                    let l = inner_stereo[f * 2];
                    let r = inner_stereo[f * 2 + 1];
                    self.ch_scratch.push((l + r) * 0.5);
                }
            }
            2 => {
                self.ch_scratch
                    .extend_from_slice(&inner_stereo[..frames * 2]);
            }
            _ => unreachable!("output channel count validated by constructor"),
        }

        // Sample-rate conversion: 48000 → out_sample_rate. On passthrough, copy ch_scratch directly to output.
        match &mut self.resampler {
            None => {
                out_buf.extend_from_slice(&self.ch_scratch);
            }
            Some(rs) => {
                rs.in_accum.extend_from_slice(&self.ch_scratch);
                rs.drain_into(out_buf)?;
            }
        }
        Ok(())
    }

    /// On stop, drain any sample-rate resampler remainder into `out_buf` (a passthrough stage has
    /// no remainder, so this is a no-op).
    pub(super) fn flush_into(&mut self, out_buf: &mut Vec<f32>) -> Result<()> {
        if let Some(rs) = self.resampler.as_mut() {
            rs.flush_into(out_buf)?;
        }
        Ok(())
    }
}
