//! flexaudio-denoise — an offline noise-suppression add-on based on RNNoise (nnnoiseless).
//!
//! It does not depend on `flexaudio-core` and takes only ±1.0-normalized, 48kHz interleaved
//! `&[f32]`. The model weights are embedded in the nnnoiseless crate (BSD-3-Clause), so
//! neither a model file nor a network is needed at runtime. It is intended to reduce
//! stationary noise in microphone recordings (fans, air conditioning, keyboard typing, etc.).
//!
//! # Latency and carry-over semantics
//!
//! RNNoise can only process fixed frames of 480 samples (10ms at 48kHz), so
//! [`Denoiser::process`] splits the input into frames internally and carries the remainder
//! over to the next call. The design has a fixed latency independent of call granularity,
//! and the output is always "the input delayed by exactly [`FRAME_SIZE`] samples/ch":
//!
//! - `process` returns the same length as the given buffer, in place. The first
//!   [`FRAME_SIZE`] samples/ch of the stream are delay padding (silence, 0.0).
//! - [`Denoiser::flush`] returns the trailing [`FRAME_SIZE`] samples/ch and closes the
//!   stream. So total output = total input + [`FRAME_SIZE`] samples/ch.
//!
//! The output is bit-identical however the input is chunked.
//!
//! # Example
//! ```
//! use flexaudio_denoise::{Denoiser, FRAME_SIZE};
//!
//! let mut dn = Denoiser::new(1).unwrap();
//! let mut chunk = vec![0.0f32; 1000]; // ±1.0-normalized mono 48kHz
//! dn.process(&mut chunk).unwrap();    // in place (the first 480 samples are latency silence)
//! let tail = dn.flush();              // the remaining 480 samples/ch
//! assert_eq!(tail.len(), FRAME_SIZE);
//! ```

#![warn(missing_docs)]

use std::collections::VecDeque;

use nnnoiseless::DenoiseState;

/// Samples per channel in one RNNoise frame (10ms at 48kHz). The processing latency is this
/// fixed value too.
pub const FRAME_SIZE: usize = DenoiseState::FRAME_SIZE;

/// nnnoiseless expects f32 scaled to the i16 range (±32768), so this factor converts to and
/// from flexaudio's ±1.0 normalization.
const I16_SCALE: f32 = 32768.0;

/// Error type for noise suppression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenoiseError {
    /// Channel count out of range (only 1..=2 is supported).
    InvalidChannels(u16),
    /// The interleaved length is not a multiple of the channel count.
    InvalidLength {
        /// The length of the given slice.
        len: usize,
        /// The channel count at construction.
        channels: u16,
    },
}

impl std::fmt::Display for DenoiseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DenoiseError::InvalidChannels(c) => {
                write!(f, "invalid channel count {c} (expected 1 or 2)")
            }
            DenoiseError::InvalidLength { len, channels } => {
                write!(
                    f,
                    "interleaved length {len} is not a multiple of channel count {channels}"
                )
            }
        }
    }
}

impl std::error::Error for DenoiseError {}

/// Streaming noise suppressor. Stereo is processed with 2 independent RNNoise state
/// instances, one per channel (no crosstalk between channels).
///
/// See the [crate-level documentation](crate) for details on latency and carry-over.
pub struct Denoiser {
    channels: usize,
    /// Per-channel RNNoise state. nnnoiseless has no reset, so resetting is done by
    /// recreating the instance (the model is the default embedded one; deterministic).
    states: Vec<Box<DenoiseState<'static>>>,
    /// Per-channel unprocessed input (kept at ±1.0 scale). Pre-filling FRAME_SIZE of silence
    /// at construction lets the delay line always emit as many samples as it receives.
    in_buf: Vec<Vec<f32>>,
    /// Processed but not yet emitted interleaved output (±1.0 scale, clamped).
    out_buf: VecDeque<f32>,
    /// Input scratch for frame processing (i16 scale, 480 samples).
    frame_in: Vec<f32>,
    /// Output scratch for frame processing (per channel, 480 samples).
    frame_out: Vec<Vec<f32>>,
}

impl Denoiser {
    /// Constructs with the given channel count (1 = mono, 2 = stereo interleaved).
    pub fn new(channels: u16) -> Result<Denoiser, DenoiseError> {
        if !(1..=2).contains(&channels) {
            return Err(DenoiseError::InvalidChannels(channels));
        }
        let ch = channels as usize;
        let mut dn = Denoiser {
            channels: ch,
            states: (0..ch).map(|_| DenoiseState::new()).collect(),
            in_buf: vec![Vec::new(); ch],
            out_buf: VecDeque::new(),
            frame_in: vec![0.0; FRAME_SIZE],
            frame_out: vec![vec![0.0; FRAME_SIZE]; ch],
        };
        dn.prime_delay();
        Ok(dn)
    }

    /// Noise-suppresses interleaved samples of any length (±1.0-normalized, 48kHz) in place.
    /// The length must be a multiple of the channel count (an empty slice is a no-op).
    ///
    /// Internally the input is split into frames of [`FRAME_SIZE`] samples/ch and processed,
    /// and the remainder is carried over to the next call. The output is the input delayed by
    /// [`FRAME_SIZE`] samples/ch, and the latency at the start of the stream is silence (0.0).
    /// The output does not change however the calls are split. Output samples are clamped to
    /// ±1.0.
    pub fn process(&mut self, interleaved: &mut [f32]) -> Result<(), DenoiseError> {
        if interleaved.len() % self.channels != 0 {
            return Err(DenoiseError::InvalidLength {
                len: interleaved.len(),
                channels: self.channels as u16,
            });
        }

        // Deinterleave and push into the carry-over buffers.
        for frame in interleaved.chunks_exact(self.channels) {
            for (ch, &s) in frame.iter().enumerate() {
                self.in_buf[ch].push(s);
            }
        }

        self.process_ready_frames();

        // Emit as many samples as were input from the delay line. Thanks to the FRAME_SIZE
        // of pre-filled silence, "processed ≥ emitted + this call's amount" always holds (the
        // leading silence becomes the latency).
        for s in interleaved.iter_mut() {
            *s = self
                .out_buf
                .pop_front()
                .expect("delay line must hold enough processed samples");
        }
        Ok(())
    }

    /// Zero-pads the carried-over remainder into one frame and processes it, returns the
    /// trailing [`FRAME_SIZE`] samples/ch of latency as interleaved, and closes the stream.
    ///
    /// This makes total output = total input + [`FRAME_SIZE`] samples/ch (the leading silence
    /// padding). Afterwards it is back in the same initial state as after
    /// [`Denoiser::reset`], so a new stream can be processed right away.
    pub fn flush(&mut self) -> Vec<f32> {
        if !self.in_buf[0].is_empty() {
            for buf in &mut self.in_buf {
                buf.resize(FRAME_SIZE, 0.0);
            }
            self.process_ready_frames();
        }
        let take = FRAME_SIZE * self.channels;
        let mut out = Vec::with_capacity(take);
        for _ in 0..take {
            // After the zero-padded processing above, the delay line always holds at least
            // take samples.
            out.push(self.out_buf.pop_front().unwrap_or(0.0));
        }
        self.reset();
        out
    }

    /// Resets the RNN state, the carry-over buffers, and the delay line.
    ///
    /// After reset the state is the same as right after construction, and the same input
    /// yields the same output.
    pub fn reset(&mut self) {
        for st in &mut self.states {
            *st = DenoiseState::new();
        }
        for buf in &mut self.in_buf {
            buf.clear();
        }
        self.out_buf.clear();
        self.prime_delay();
    }

    /// The channel count given at construction.
    pub fn channels(&self) -> u16 {
        self.channels as u16
    }

    /// Pre-fills the delay line. Pushes FRAME_SIZE of silence into each channel's input
    /// buffer. Processing this silent frame yields exactly 0.0, so the first FRAME_SIZE
    /// samples/ch of the output stream become "silence padding".
    fn prime_delay(&mut self) {
        for buf in &mut self.in_buf {
            buf.resize(FRAME_SIZE, 0.0);
        }
    }

    /// Processes every complete frame and pushes the result into the delay line (out_buf).
    fn process_ready_frames(&mut self) {
        // Every channel holds the same count, so checking the first channel's length suffices.
        while self.in_buf[0].len() >= FRAME_SIZE {
            for ch in 0..self.channels {
                // Scale ±1.0 up to the i16 range and process one frame.
                for (dst, &src) in self.frame_in.iter_mut().zip(&self.in_buf[ch][..FRAME_SIZE]) {
                    *dst = src * I16_SCALE;
                }
                self.states[ch].process_frame(&mut self.frame_out[ch], &self.frame_in);
                self.in_buf[ch].drain(..FRAME_SIZE);
            }
            // Scale the i16 range back to ±1.0, interleave, and queue for emission.
            for i in 0..FRAME_SIZE {
                for out_ch in &self.frame_out {
                    self.out_buf
                        .push_back((out_ch[i] / I16_SCALE).clamp(-1.0, 1.0));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic pseudo-random numbers (LCG, Knuth's MMIX constants). Avoids depending on
    /// rand.
    struct Lcg(u64);

    impl Lcg {
        /// Uniform random number in [0, 1). Uses the upper 24 bits.
        fn next_unit(&mut self) -> f32 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((self.0 >> 40) as f32) / (1u32 << 24) as f32
        }
    }

    /// Uniform white noise of amplitude ±amp (fixed seed, deterministic).
    fn white_noise(n: usize, amp: f32) -> Vec<f32> {
        let mut lcg = Lcg(0x5EED_1234_5678_9ABC);
        (0..n)
            .map(|_| (lcg.next_unit() * 2.0 - 1.0) * amp)
            .collect()
    }

    /// First-order IIR low-pass (y += a * (x - y)). Used to mimic low-frequency-heavy
    /// stationary noise such as fans and air conditioning (a=0.1 corresponds to a cutoff of
    /// ~800Hz; deterministic).
    fn lowpass(xs: &[f32], a: f32) -> Vec<f32> {
        let mut y = 0.0f32;
        xs.iter()
            .map(|&x| {
                y += a * (x - y);
                y
            })
            .collect()
    }

    fn sine(n: usize, freq: f32, amp: f32) -> Vec<f32> {
        (0..n)
            .map(|i| (2.0 * std::f32::consts::PI * freq * i as f32 / 48_000.0).sin() * amp)
            .collect()
    }

    fn rms(xs: &[f32]) -> f64 {
        let sum: f64 = xs.iter().map(|&x| (x as f64) * (x as f64)).sum();
        (sum / xs.len() as f64).sqrt()
    }

    /// Processes in one shot and returns the output "aligned to the input with the latency
    /// removed" (the same length as the input).
    fn run_aligned(channels: u16, input: &[f32]) -> Vec<f32> {
        let mut dn = Denoiser::new(channels).unwrap();
        let mut buf = input.to_vec();
        dn.process(&mut buf).unwrap();
        buf.extend_from_slice(&dn.flush());
        // The first FRAME_SIZE samples/ch (FRAME_SIZE*ch interleaved) are latency silence.
        buf.split_off(FRAME_SIZE * channels as usize)
    }

    /// For measuring to decide thresholds (not normally run): prints the RMS ratio of each
    /// test signal. Run with `cargo test -p flexaudio-denoise -- --ignored --nocapture measure`.
    #[test]
    #[ignore]
    fn measure_rms_ratios() {
        let white = white_noise(96_000, 0.3);
        let fan = lowpass(&white, 0.1);
        let tone = sine(96_000, 440.0, 0.5);
        for (name, input) in [
            ("white noise amp=0.3", &white),
            ("lowpass(a=0.1) noise", &fan),
            ("440Hz sine amp=0.5", &tone),
        ] {
            let output = run_aligned(1, input);
            println!(
                "{name}: in_rms={:.6} out_rms={:.6} ratio={:.6}",
                rms(input),
                rms(&output),
                rms(&output) / rms(input)
            );
        }
    }

    #[test]
    fn stationary_noise_rms_strongly_reduced() {
        // 2 seconds @48k mono of low-frequency-heavy stationary noise (first-order low-pass of
        // LCG white noise). Close to fans / air conditioning, the intended target of this
        // crate. Measured by measure_rms_ratios: ratio = 0.0556 (in_rms 0.0399 → out_rms
        // 0.0022; 0.006 for the last 1 second alone). The threshold is 25%, a margin of about
        // 4.5x over the measurement.
        let input = lowpass(&white_noise(96_000, 0.3), 0.1);
        let output = run_aligned(1, &input);
        let (in_rms, out_rms) = (rms(&input), rms(&output));
        assert!(
            out_rms < in_rms * 0.25,
            "stationary noise must be strongly attenuated: \
             in_rms={in_rms:.4} out_rms={out_rms:.4}"
        );
    }

    #[test]
    fn white_noise_rms_reduced() {
        // 2 seconds @48k mono of full-band white noise (amplitude ±0.3). Synthetic noise far
        // from the training distribution, so RNNoise suppresses it only weakly: measured
        // ratio = 0.7883 (effectiveness on stationary noise is covered by
        // stationary_noise_rms_strongly_reduced). Here we only check "not amplified, with some
        // reduction", at 90% = the measurement plus headroom.
        let input = white_noise(96_000, 0.3);
        let output = run_aligned(1, &input);
        let (in_rms, out_rms) = (rms(&input), rms(&output));
        assert!(
            out_rms < in_rms * 0.90,
            "white noise must not be amplified: in_rms={in_rms:.4} out_rms={out_rms:.4}"
        );
    }

    #[test]
    fn sine_output_sane() {
        // 440Hz sine (amplitude 0.5), 2 seconds. Measured ratio = 0.9999, nearly passthrough
        // (RNNoise treats periodic signals as voiced). Still, how pure tones are handled
        // depends on the model, so the assertions are limited to sanity (no NaN, within ±1.0)
        // and "does not vanish" = a 50% floor, half the measurement.
        let input = sine(96_000, 440.0, 0.5);
        let output = run_aligned(1, &input);
        assert!(
            output.iter().all(|x| x.is_finite()),
            "output must not contain NaN/inf"
        );
        assert!(
            output.iter().all(|&x| (-1.0..=1.0).contains(&x)),
            "output must stay within +/-1.0"
        );
        let (in_rms, out_rms) = (rms(&input), rms(&output));
        assert!(
            out_rms > in_rms * 0.50,
            "sine must pass through mostly intact: in_rms={in_rms:.4} out_rms={out_rms:.4}"
        );
    }

    #[test]
    fn first_delay_block_is_silence() {
        // The first FRAME_SIZE samples/ch of the output stream are latency padding and exactly
        // 0.0.
        let mut dn = Denoiser::new(1).unwrap();
        let mut buf = white_noise(FRAME_SIZE * 2, 0.3);
        dn.process(&mut buf).unwrap();
        assert!(
            buf[..FRAME_SIZE].iter().all(|&x| x == 0.0),
            "first FRAME_SIZE output samples must be exactly zero"
        );
    }

    #[test]
    fn chunked_equals_oneshot() {
        // Feeding in 1000-sample steps (not a multiple of 480) yields output bit-identical to
        // one-shot processing (verifies independence from call granularity). Also checks that
        // the total sample counts are consistent.
        let input = white_noise(96_000, 0.3);

        let mut oneshot = input.clone();
        let mut dn1 = Denoiser::new(1).unwrap();
        dn1.process(&mut oneshot).unwrap();
        let tail1 = dn1.flush();

        let mut chunked = Vec::with_capacity(input.len());
        let mut dn2 = Denoiser::new(1).unwrap();
        for chunk in input.chunks(1000) {
            let mut buf = chunk.to_vec();
            dn2.process(&mut buf).unwrap();
            assert_eq!(
                buf.len(),
                chunk.len(),
                "process must emit in place, same length"
            );
            chunked.extend_from_slice(&buf);
        }
        let tail2 = dn2.flush();

        assert_eq!(
            chunked.len(),
            input.len(),
            "total process output == total input"
        );
        assert_eq!(
            tail1.len(),
            FRAME_SIZE,
            "flush must emit exactly FRAME_SIZE per channel"
        );
        assert_eq!(
            oneshot, chunked,
            "chunk granularity must not change the output"
        );
        assert_eq!(tail1, tail2, "flush residue must also match");
    }

    #[test]
    fn stereo_keeps_channels_independent_and_interleaved() {
        // Interleaved stereo with L = noise, R = silence. The R output stays exactly 0, and the
        // L output is bit-identical to processing the same signal as mono (verifies both that
        // interleaving is preserved and that channels are independent).
        let n = 48_000; // 1 second/ch. Fed in 1000-sample steps (=500/ch, not a multiple of 480).
        let left = white_noise(n, 0.3);
        let mut stereo = Vec::with_capacity(n * 2);
        for &l in &left {
            stereo.push(l);
            stereo.push(0.0);
        }

        let mut dn = Denoiser::new(2).unwrap();
        let mut stereo_out = Vec::with_capacity(stereo.len());
        for chunk in stereo.chunks(1000) {
            let mut buf = chunk.to_vec();
            dn.process(&mut buf).unwrap();
            stereo_out.extend_from_slice(&buf);
        }
        let tail = dn.flush();
        assert_eq!(
            tail.len(),
            FRAME_SIZE * 2,
            "stereo flush is FRAME_SIZE per channel"
        );
        stereo_out.extend_from_slice(&tail);

        let left_out: Vec<f32> = stereo_out.iter().step_by(2).copied().collect();
        let right_out: Vec<f32> = stereo_out.iter().skip(1).step_by(2).copied().collect();
        assert!(
            right_out.iter().all(|&x| x == 0.0),
            "silent right channel must stay exactly zero (no crosstalk)"
        );

        let mut mono_ref = left.clone();
        let mut dn_mono = Denoiser::new(1).unwrap();
        dn_mono.process(&mut mono_ref).unwrap();
        mono_ref.extend_from_slice(&dn_mono.flush());
        assert_eq!(
            left_out, mono_ref,
            "stereo left must equal the mono reference"
        );
    }

    #[test]
    fn reset_restores_initial_state() {
        // Feeding the same input after reset yields the same output (bit for bit). The same
        // holds for flush's automatic reset.
        let input = white_noise(10_000, 0.3);

        let mut dn = Denoiser::new(1).unwrap();
        let mut first = input.clone();
        dn.process(&mut first).unwrap();

        dn.reset();
        let mut second = input.clone();
        dn.process(&mut second).unwrap();
        assert_eq!(first, second, "reset must restore the initial state");

        // flush closes the stream and then returns to the same initial state as reset.
        dn.flush();
        let mut third = input.clone();
        dn.process(&mut third).unwrap();
        assert_eq!(first, third, "flush must leave the denoiser reusable");
    }

    #[test]
    fn rejects_invalid_channels() {
        assert_eq!(
            Denoiser::new(0).err(),
            Some(DenoiseError::InvalidChannels(0))
        );
        assert_eq!(
            Denoiser::new(3).err(),
            Some(DenoiseError::InvalidChannels(3))
        );
    }

    #[test]
    fn rejects_misaligned_length() {
        let mut dn = Denoiser::new(2).unwrap();
        let mut buf = vec![0.0f32; 999]; // not a multiple of 2ch
        let err = dn.process(&mut buf).unwrap_err();
        assert_eq!(
            err,
            DenoiseError::InvalidLength {
                len: 999,
                channels: 2
            }
        );
        // On error the buffer is not touched.
        assert!(buf.iter().all(|&x| x == 0.0));
    }

    #[test]
    fn empty_process_and_bare_flush() {
        let mut dn = Denoiser::new(1).unwrap();
        let mut empty: [f32; 0] = [];
        dn.process(&mut empty).unwrap(); // empty is a no-op
        assert_eq!(dn.channels(), 1);

        // Flushing with zero input still returns exactly the latency amount (silence).
        let tail = dn.flush();
        assert_eq!(tail.len(), FRAME_SIZE);
        assert!(tail.iter().all(|&x| x == 0.0));
    }
}
