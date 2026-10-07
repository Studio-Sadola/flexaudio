//! Preprocessing for [`crate::Vad::process_pcm`]. Converts interleaved PCM in any format to mono,
//! resamples it to the VAD operating rate (16000 or 8000), and prepares it for the existing
//! 16k/mono path.
//!
//! It follows the flexaudio-core normalizer: mono conversion uses a simple average across
//! channels (L/R average for stereo), and resampling uses rubato sinc with anti-aliasing. The
//! resampler carries its internal delay and remainder across calls, so split input has no seams.

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{
    Async, FixedAsync, Indexing, Resampler, SincInterpolationParameters, SincInterpolationType,
    WindowFunction,
};

/// Input PCM format descriptor accepted by [`crate::Vad::process_pcm`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PcmFormat {
    /// Input sample rate (Hz).
    pub sample_rate: u32,
    /// Number of input channels (channels in interleaved data).
    pub channels: u16,
}

impl PcmFormat {
    /// Validate supported PCM input rates (8,000–192,000 Hz) and nonzero channels.
    pub fn validate(self) -> Result<(), crate::VadError> {
        if self.channels == 0 {
            return Err(crate::VadError::InvalidFormat(
                "channels must be nonzero".into(),
            ));
        }
        if !(8_000..=192_000).contains(&self.sample_rate) {
            return Err(crate::VadError::InvalidFormat(format!(
                "sample rate must be within 8000..=192000 Hz, got {}",
                self.sample_rate
            )));
        }
        Ok(())
    }
}

/// Preprocessor that downmixes any number of interleaved channels to mono and resamples to the VAD
/// rate.
///
/// The input format ([`PcmFormat`]) and target rate are fixed at construction. If the input rate
/// matches the target, only mono conversion is performed and no resampler is used.
pub(crate) struct PcmConverter {
    format: PcmFormat,
    channels: usize,
    /// Sample-rate converter for the target rate. `None` if the input rate matches (mono
    /// conversion only).
    resampler: Option<MonoResampler>,
    /// Remainder of interleaved samples that does not make a full frame. Carry it over and prepend
    /// it to the next input.
    remainder: Vec<f32>,
    /// Scratch buffer for mono-converted samples (reuse allocations).
    mono: Vec<f32>,
    #[cfg(test)]
    fail_conversion: bool,
}

impl PcmConverter {
    /// Create a converter from the input format and target rate (the VAD operating rate).
    ///
    /// rubato construction can fail for extreme rate ratios, so return an error string for the
    /// caller to handle without panicking.
    pub(crate) fn new(format: PcmFormat, target_rate: u32) -> Result<Self, String> {
        Self::new_with_resampler_chunk(format, target_rate, None)
    }

    /// Create a dedicated converter that maps VAD's 8 kHz frames (256 samples) to 16 kHz frames
    /// (512 samples) while preserving continuity.
    pub(crate) fn new_8k_to_16k_frame_resampler() -> Result<Self, String> {
        let mut converter = Self::new_with_resampler_chunk(
            PcmFormat {
                sample_rate: 8_000,
                channels: 1,
            },
            16_000,
            Some(256),
        )?;

        // rubato's sinc waits for future taps at startup, so the first real frame would produce
        // only 508 samples. Feed 256 samples of silent pre-roll first and discard its output.
        // After that, each real frame produces 512 samples without zero-padding or splitting
        // frames at real audio boundaries.
        let mut discarded = Vec::new();
        converter.convert(&[0.0; 256], &mut discarded)?;
        Ok(converter)
    }

    fn new_with_resampler_chunk(
        format: PcmFormat,
        target_rate: u32,
        resampler_chunk_in_frames: Option<usize>,
    ) -> Result<Self, String> {
        format.validate().map_err(|e| e.to_string())?;
        let channels = usize::from(format.channels);
        let resampler = if format.sample_rate == target_rate {
            None
        } else {
            Some(MonoResampler::new(
                format.sample_rate,
                target_rate,
                resampler_chunk_in_frames,
            )?)
        };
        Ok(PcmConverter {
            format,
            channels,
            resampler,
            remainder: Vec::new(),
            mono: Vec::new(),
            #[cfg(test)]
            fail_conversion: false,
        })
    }

    #[cfg(test)]
    pub(crate) fn fail_conversion(&mut self) {
        self.fail_conversion = true;
    }

    /// Whether this converter handles the given input format.
    pub(crate) fn matches(&self, format: PcmFormat) -> bool {
        self.format == format
    }

    /// Downmix interleaved input to mono, resample if needed, and append target-rate mono samples
    /// to `out`.
    ///
    /// Any remainder that does not reach a frame boundary (a multiple of channels) is carried
    /// internally, so input split at any position produces the same result as a single input.
    pub(crate) fn convert(
        &mut self,
        interleaved: &[f32],
        out: &mut Vec<f32>,
    ) -> Result<(), String> {
        #[cfg(test)]
        if self.fail_conversion {
            return Err("injected conversion failure".into());
        }
        // Append this input to the previous remainder and downmix only complete frames.
        self.remainder.extend_from_slice(interleaved);
        let frames = self.remainder.len() / self.channels;
        let used = frames * self.channels;

        self.mono.clear();
        downmix_to_mono(&self.remainder[..used], self.channels, &mut self.mono);
        self.remainder.drain(..used);

        match &mut self.resampler {
            None => out.extend_from_slice(&self.mono),
            Some(rs) => rs.push(&self.mono, out)?,
        }
        Ok(())
    }
}

/// Downmix `channels` interleaved channels to mono by averaging each frame's channels, then push
/// the result to `out`.
///
/// Stereo uses the L/R average; higher channel counts use the average of all channels. Copy as-is
/// when `channels <= 1`. `src` must have a length divisible by `channels` (the caller removes any
/// incomplete frame).
fn downmix_to_mono(src: &[f32], channels: usize, out: &mut Vec<f32>) {
    if channels <= 1 {
        out.extend_from_slice(src);
        return;
    }
    let inv = 1.0 / channels as f32;
    for frame in src.chunks_exact(channels) {
        let sum: f32 = frame.iter().sum();
        out.push(sum * inv);
    }
}

/// rubato sinc resampler for mono (one channel). Uses fixed input chunks (`FixedAsync::Input`) and
/// carries remainders and internal delay across calls.
///
/// Parameters match the flexaudio-core normalizer (sinc_len=128 / BlackmanHarris2, etc.).
struct MonoResampler {
    inner: Async<f32>,
    /// Fixed number of input frames rubato requires for one `process` call.
    chunk_in_frames: usize,
    /// Maximum output frames that one `process` call can produce.
    max_out_frames: usize,
    /// Unprocessed mono input samples.
    in_accum: Vec<f32>,
    /// Output scratch for rubato (reused to avoid allocations).
    out_scratch: Vec<f32>,
}

impl MonoResampler {
    fn new(in_sr: u32, out_sr: u32, input_chunk_frames: Option<usize>) -> Result<Self, String> {
        let ratio = out_sr as f64 / in_sr as f64;
        // A fixed input chunk corresponds to 20 ms of input frames (rubato retains any remainder).
        let chunk_in_frames = input_chunk_frames.unwrap_or_else(|| (in_sr as usize / 50).max(64));

        let params = SincInterpolationParameters {
            sinc_len: 128,
            f_cutoff: 0.95,
            interpolation: SincInterpolationType::Linear,
            oversampling_factor: 128,
            window: WindowFunction::BlackmanHarris2,
        };

        let inner = Async::<f32>::new_sinc(
            ratio,
            1.0, // Fixed ratio.
            &params,
            chunk_in_frames,
            1, // mono
            FixedAsync::Input,
        )
        .map_err(|e| format!("rubato sinc resampler construction failed: {e}"))?;

        let max_out_frames = inner.output_frames_max();

        Ok(MonoResampler {
            inner,
            chunk_in_frames,
            max_out_frames,
            in_accum: Vec::with_capacity(chunk_in_frames * 4),
            out_scratch: vec![0.0; max_out_frames],
        })
    }

    /// Accumulate mono input, resample as many `chunk_in_frames` chunks as possible, and append
    /// them to `out`. Carry any incomplete remainder in `in_accum` to the next call.
    fn push(&mut self, mono: &[f32], out: &mut Vec<f32>) -> Result<(), String> {
        self.in_accum.extend_from_slice(mono);
        let step = self.chunk_in_frames; // For mono, frame count equals sample count.

        while self.in_accum.len() >= step {
            let in_adapter = InterleavedSlice::new(&self.in_accum[..step], 1, self.chunk_in_frames)
                .map_err(|e| format!("rubato interleaved input adapter failed: {e}"))?;
            let mut out_adapter =
                InterleavedSlice::new_mut(&mut self.out_scratch[..], 1, self.max_out_frames)
                    .map_err(|e| format!("rubato interleaved output adapter failed: {e}"))?;

            let indexing = Indexing {
                input_offset: 0,
                output_offset: 0,
                partial_len: None,
                active_channels_mask: None,
            };

            let (_in_used, out_written) = self
                .inner
                .process_into_buffer(&in_adapter, &mut out_adapter, Some(&indexing))
                .map_err(|e| format!("rubato process_into_buffer failed: {e}"))?;

            out.extend_from_slice(&self.out_scratch[..out_written]); // Mono.
            self.in_accum.drain(..step);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;

    #[test]
    fn downmix_stereo_is_lr_average() {
        // Perfectly inverted phase yields 0; in-phase channels retain their value.
        let src = [0.5, -0.5, 0.3, 0.3, 1.0, 0.0];
        let mut out = Vec::new();
        downmix_to_mono(&src, 2, &mut out);
        assert_eq!(out, vec![0.0, 0.3, 0.5]);
    }

    #[test]
    fn downmix_quad_is_channel_average() {
        let src = [1.0, 2.0, 3.0, 4.0]; // One 4-channel frame → average 2.5.
        let mut out = Vec::new();
        downmix_to_mono(&src, 4, &mut out);
        assert_eq!(out, vec![2.5]);
    }

    #[test]
    fn downmix_mono_is_copy() {
        let src = [0.1, -0.2, 0.3];
        let mut out = Vec::new();
        downmix_to_mono(&src, 1, &mut out);
        assert_eq!(out, src.to_vec());
    }

    /// Resampling a sine wave from 48k to 16k preserves its frequency (zero crossings) and
    /// amplitude (RMS). Measure only the middle to avoid transients.
    #[test]
    fn resample_48k_to_16k_preserves_tone() {
        let mut conv = PcmConverter::new(
            PcmFormat {
                sample_rate: 48_000,
                channels: 1,
            },
            16_000,
        )
        .unwrap();

        let freq = 440.0_f32;
        let amp = 0.5_f32;
        let mut out = Vec::new();
        // Push two seconds of audio in 441-sample chunks (also checks for seams across chunks).
        let total = 48_000 * 2;
        let mut i = 0usize;
        while i < total {
            let take = 441.min(total - i);
            let block: Vec<f32> = (0..take)
                .map(|k| (2.0 * PI * freq * ((i + k) as f32) / 48_000.0).sin() * amp)
                .collect();
            conv.convert(&block, &mut out).unwrap();
            i += take;
        }
        assert!(
            out.len() >= 16_000,
            "at least one second of output is required: {}",
            out.len()
        );

        // Discard 0.25 seconds at each end (4000 samples) to measure the middle second.
        let mid = &out[4_000..4_000 + 16_000];

        // Frequency: about 2*440 = 880 zero crossings in 16000 samples.
        let crossings = zero_crossings(mid);
        assert!(
            (876..=884).contains(&crossings),
            "frequency shifted after resampling to 16k: crossings={crossings}"
        );

        // Amplitude: a sine wave's RMS is amp/√2 ≈ 0.3536.
        let got = rms(mid);
        let expect = amp / std::f32::consts::SQRT_2;
        assert!(
            (got - expect).abs() < 0.02,
            "RMS shifted after resampling to 16k: got={got} expect={expect}"
        );
    }

    /// When the sample rate matches the target, no resampler is used (only mono conversion), and
    /// the downmixed values are returned unchanged.
    #[test]
    fn same_rate_stereo_only_downmixes() {
        let mut conv = PcmConverter::new(
            PcmFormat {
                sample_rate: 16_000,
                channels: 2,
            },
            16_000,
        )
        .unwrap();
        assert!(conv.resampler.is_none());
        let mut out = Vec::new();
        conv.convert(&[0.5, -0.5, 0.2, 0.2], &mut out).unwrap();
        assert_eq!(out, vec![0.0, 0.2]);
    }

    /// Splitting input around a remainder that does not reach a frame boundary (a multiple of
    /// channels) produces the same mono sequence as processing it all at once.
    #[test]
    fn split_across_partial_frame_matches_bulk() {
        let fmt = PcmFormat {
            sample_rate: 16_000,
            channels: 2,
        };
        let interleaved: Vec<f32> = (0..2000).map(|i| (i as f32) * 1e-3).collect();

        let mut bulk = Vec::new();
        PcmConverter::new(fmt, 16_000)
            .unwrap()
            .convert(&interleaved, &mut bulk)
            .unwrap();

        // Process chunks of odd length, splitting across frame boundaries.
        let mut split = Vec::new();
        let mut conv = PcmConverter::new(fmt, 16_000).unwrap();
        for chunk in interleaved.chunks(777) {
            conv.convert(chunk, &mut split).unwrap();
        }
        assert_eq!(bulk, split);
    }

    #[test]
    fn frame_resampler_returns_one_16k_frame_per_8k_frame() {
        let mut converter = PcmConverter::new_8k_to_16k_frame_resampler().unwrap();
        let mut output = Vec::new();
        let input: Vec<f32> = (0..(256 * 4))
            .map(|i| (2.0 * PI * 440.0 * i as f32 / 8_000.0).sin() * 0.5)
            .collect();

        for frame in input.as_chunks::<256>().0 {
            let before = output.len();
            converter.convert(frame, &mut output).unwrap();
            assert_eq!(
                output.len() - before,
                512,
                "8 kHz frame must yield exactly one 16 kHz model frame"
            );
        }
    }

    fn rms(samples: &[f32]) -> f32 {
        let sum_sq: f64 = samples.iter().map(|&x| (x as f64) * (x as f64)).sum();
        (sum_sq / samples.len() as f64).sqrt() as f32
    }

    fn zero_crossings(samples: &[f32]) -> usize {
        let mut n = 0;
        for w in samples.windows(2) {
            if (w[0] < 0.0 && w[1] >= 0.0) || (w[0] >= 0.0 && w[1] < 0.0) {
                n += 1;
            }
        }
        n
    }
}
