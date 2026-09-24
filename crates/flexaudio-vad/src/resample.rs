//! The front end of [`crate::Vad::process_pcm`]. Downmixes interleaved PCM of any format to
//! mono and resamples it to the VAD's operating rate (16000 or 8000), putting it in a form
//! that can be passed to the existing 16k/mono path.
//!
//! The conventions follow flexaudio-core's normalizer. Downmixing is a simple average of the
//! channels (the L/R average for stereo), and resampling is a rubato sinc with anti-aliasing.
//! The resampler carries its internal delay and leftovers across calls, so no seams appear
//! even when the input is fed in small pieces.

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{
    Async, FixedAsync, Indexing, Resampler, SincInterpolationParameters, SincInterpolationType,
    WindowFunction,
};

/// Format descriptor of the input PCM accepted by [`crate::Vad::process_pcm`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PcmFormat {
    /// Input sample rate (Hz).
    pub sample_rate: u32,
    /// Input channel count (the number of interleaved channels).
    pub channels: u16,
}

/// Front-end converter that downmixes interleaved audio with any channel count to mono and
/// resamples it to the VAD rate.
///
/// The input format ([`PcmFormat`]) and target rate are fixed at creation. When the input
/// rate equals the target, it only downmixes and holds no resampler.
pub(crate) struct PcmConverter {
    format: PcmFormat,
    channels: usize,
    /// SR converter to the target rate. `None` if the input rate matches the target
    /// (downmix only).
    resampler: Option<MonoResampler>,
    /// Leftover interleaved samples that do not fill a frame. Carried over by prepending them
    /// to the next input.
    remainder: Vec<f32>,
    /// Scratch buffer for the downmixed result (reuses the allocation).
    mono: Vec<f32>,
}

impl PcmConverter {
    /// Creates a converter from the input format and the target rate (the VAD's operating
    /// rate).
    ///
    /// Building rubato can fail, e.g. for extreme rate ratios, in which case an error string is
    /// returned (so the caller can handle it without panicking).
    pub(crate) fn new(format: PcmFormat, target_rate: u32) -> Result<Self, String> {
        Self::new_with_resampler_chunk(format, target_rate, None)
    }

    /// Creates a dedicated converter that turns a VAD 8 kHz frame (256 samples) into 512
    /// samples at 16 kHz while preserving continuity.
    pub(crate) fn new_8k_to_16k_frame_resampler() -> Result<Self, String> {
        let mut converter = Self::new_with_resampler_chunk(
            PcmFormat {
                sample_rate: 8_000,
                channels: 1,
            },
            16_000,
            Some(256),
        )?;

        // rubato's sinc waits for the future-side taps at startup, so only the first real frame
        // comes out as 508 samples. Run a 256-sample silent pre-roll through first and discard
        // the corresponding output. After that every real frame yields 512 samples, with no
        // zero padding or frame splitting at the boundaries on the real-audio side.
        let mut discarded = Vec::new();
        converter.convert(&[0.0; 256], &mut discarded)?;
        Ok(converter)
    }

    fn new_with_resampler_chunk(
        format: PcmFormat,
        target_rate: u32,
        resampler_chunk_in_frames: Option<usize>,
    ) -> Result<Self, String> {
        let channels = usize::from(format.channels.max(1));
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
        })
    }

    /// Whether this is the input format this converter targets.
    pub(crate) fn matches(&self, format: PcmFormat) -> bool {
        self.format == format
    }

    /// Downmixes interleaved input to mono, resamples it if needed, and appends mono samples
    /// at the target rate to `out`.
    ///
    /// A remainder short of a frame boundary (a multiple of channels) is carried over
    /// internally, so splitting the input at any position gives the same result as passing it
    /// all at once.
    pub(crate) fn convert(
        &mut self,
        interleaved: &[f32],
        out: &mut Vec<f32>,
    ) -> Result<(), String> {
        // Append this call's input to the previous remainder and downmix only complete frames.
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

/// Downmixes interleaved `channels`-channel audio to mono by the per-frame channel average and
/// pushes it to `out`.
///
/// Stereo is the L/R average; more channels use the average of all channels. `channels <= 1`
/// is copied as-is. The length of `src` must be a multiple of `channels` (the caller removes
/// partial frames beforehand).
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

/// rubato sinc resampler for mono (1ch) only. Runs with a fixed input chunk
/// (`FixedAsync::Input`) and carries leftovers and internal delay across calls.
///
/// The parameters are the same as flexaudio-core's normalizer (sinc_len=128 /
/// BlackmanHarris2, etc.).
struct MonoResampler {
    inner: Async<f32>,
    /// Number of input frames rubato requires per `process` call (fixed).
    chunk_in_frames: usize,
    /// Maximum number of output frames one `process` call can produce.
    max_out_frames: usize,
    /// Unprocessed input mono samples.
    in_accum: Vec<f32>,
    /// Output scratch for rubato (reused to avoid allocations).
    out_scratch: Vec<f32>,
}

impl MonoResampler {
    fn new(in_sr: u32, out_sr: u32, input_chunk_frames: Option<usize>) -> Result<Self, String> {
        let ratio = out_sr as f64 / in_sr as f64;
        // The fixed input chunk is 20ms worth of input frames (rubato keeps the remainder
        // internally).
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
            1.0, // the ratio is fixed
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

    /// Accumulates mono input, resamples as much as possible in `chunk_in_frames` units, and
    /// appends to `out`. A remainder that does not fill a unit stays in `in_accum` for the next
    /// call.
    fn push(&mut self, mono: &[f32], out: &mut Vec<f32>) -> Result<(), String> {
        self.in_accum.extend_from_slice(mono);
        let step = self.chunk_in_frames; // mono, so frame count = sample count.

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

            out.extend_from_slice(&self.out_scratch[..out_written]); // mono.
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
        // Fully out-of-phase gives 0; in-phase gives the original value.
        let src = [0.5, -0.5, 0.3, 0.3, 1.0, 0.0];
        let mut out = Vec::new();
        downmix_to_mono(&src, 2, &mut out);
        assert_eq!(out, vec![0.0, 0.3, 0.5]);
    }

    #[test]
    fn downmix_quad_is_channel_average() {
        let src = [1.0, 2.0, 3.0, 4.0]; // 1 frame of 4ch → average 2.5.
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

    /// Resampling a sine wave 48k→16k preserves frequency (zero crossings) and amplitude (RMS).
    /// Measured only in the middle to avoid transients.
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
        // Push 2 seconds, 441 samples at a time (also checks that small pieces cause no seams).
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
            "at least 1 second of output is required: {}",
            out.len()
        );

        // Discard the transients (0.25 seconds = 4000 samples at each of the start and end) and
        // measure the middle 1 second.
        let mid = &out[4_000..4_000 + 16_000];

        // Frequency: zero crossings in 16000 samples ≈ 2*440 = 880.
        let crossings = zero_crossings(mid);
        assert!(
            (876..=884).contains(&crossings),
            "frequency drifted after 16k resampling: crossings={crossings}"
        );

        // Amplitude: the RMS of a sine is amp/√2 ≈ 0.3536.
        let got = rms(mid);
        let expect = amp / std::f32::consts::SQRT_2;
        assert!(
            (got - expect).abs() < 0.02,
            "RMS drifted after 16k resampling: got={got} expect={expect}"
        );
    }

    /// When the SR matches the target, no resampler is held (downmix only) and the downmixed
    /// values come out as-is.
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

    /// Splitting with remainders short of a frame boundary (a multiple of ch) in between still
    /// gives the same mono sequence as bulk.
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

        // Feed split at an odd length (straddling frame boundaries).
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
