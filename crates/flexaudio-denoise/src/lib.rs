//! flexaudio-denoise — An offline noise-suppression addon using RNNoise (nnnoiseless).
//!
//! It does not depend on `flexaudio-core`; it accepts only normalized ±1.0, 48 kHz interleaved
//! `&[f32]`. Model weights are embedded in the nnnoiseless crate (BSD-3-Clause), so no runtime
//! model file or network access is needed. It targets steady noise in microphone recordings
//! (fans, air conditioning, keyboard clicks, and similar sounds).
//!
//! # Latency and carry-over semantics
//!
//! RNNoise processes only fixed 480-sample frames (10 ms at 48 kHz), so [`Denoiser::process`]
//! splits input into frames internally and carries any remainder into the next call. The fixed
//! latency does not depend on call size. Output is always the input delayed by exactly
//! [`FRAME_SIZE`] samples per channel:
//!
//! - `process` returns the same-length buffer in place. The first [`FRAME_SIZE`] samples per
//!   channel are latency padding (silence, 0.0).
//! - [`Denoiser::flush`] returns the final [`FRAME_SIZE`] samples per channel and closes the
//!   stream. Total output = total input + [`FRAME_SIZE`] samples per channel.
//!
//! The output is bit-identical regardless of how the input is chunked.
//!
//! # Example
//! ```
//! use flexaudio_denoise::{Denoiser, FRAME_SIZE};
//!
//! let mut dn = Denoiser::new(1).unwrap();
//! let mut chunk = vec![0.0f32; 1000]; // normalized ±1.0, mono, 48 kHz
//! dn.process(&mut chunk).unwrap();    // In place (the first 480 samples are latency silence)
//! let tail = dn.flush();              // Remaining 480 samples per channel
//! assert_eq!(tail.len(), FRAME_SIZE);
//! ```

#![warn(missing_docs)]

use std::collections::VecDeque;

use nnnoiseless::DenoiseState;

/// Samples per RNNoise frame per channel (10 ms at 48 kHz). Processing latency is also this fixed value.
pub const FRAME_SIZE: usize = DenoiseState::FRAME_SIZE;

/// nnnoiseless expects f32 scaled to the i16 range (±32768), so use this factor to convert to and
/// from flexaudio's normalized ±1.0 range.
const I16_SCALE: f32 = 32768.0;

/// Noise-suppression errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenoiseError {
    /// Channel count is out of range (only 1..=2 are supported).
    InvalidChannels(u16),
    /// Interleaved length is not a multiple of the channel count.
    InvalidLength {
        /// Length of the supplied slice.
        len: usize,
        /// Channel count at construction.
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

/// Streaming noise suppressor. Stereo uses two independent RNNoise states, one per channel
/// (no crosstalk between channels).
///
/// See the [crate-level documentation](crate) for details on latency and carry-over.
pub struct Denoiser {
    channels: usize,
    /// Per-channel RNNoise state. nnnoiseless has no reset, so reset by recreating the instance
    /// (the default embedded model is deterministic).
    states: Vec<Box<DenoiseState<'static>>>,
    /// Unprocessed input per channel (kept at ±1.0 scale). Prepending FRAME_SIZE of silence at
    /// construction lets the delay line always emit as many samples as it receives.
    in_buf: Vec<Vec<f32>>,
    /// Processed interleaved output awaiting emission (±1.0 scale, clamped).
    out_buf: VecDeque<f32>,
    /// Frame-processing input scratch buffer (i16 scale, 480 samples).
    frame_in: Vec<f32>,
    /// Frame-processing output scratch buffer (480 samples per channel).
    frame_out: Vec<Vec<f32>>,
}

impl Denoiser {
    /// Create with a channel count (1 = mono, 2 = interleaved stereo).
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

    /// Apply noise suppression in place to any-length interleaved samples (normalized ±1.0, 48 kHz).
    /// Length must be a multiple of the channel count (an empty slice is a no-op).
    ///
    /// Internally splits input into frames of [`FRAME_SIZE`] samples per channel and carries any
    /// remainder to the next call. Output is delayed by [`FRAME_SIZE`] samples per channel, with
    /// silence (0.0) at the start of the stream. Chunk boundaries do not change the output sequence.
    /// Output samples are clamped to ±1.0.
    pub fn process(&mut self, interleaved: &mut [f32]) -> Result<(), DenoiseError> {
        if interleaved.len() % self.channels != 0 {
            return Err(DenoiseError::InvalidLength {
                len: interleaved.len(),
                channels: self.channels as u16,
            });
        }

        // Deinterleave and append to the carry-over buffers.
        for frame in interleaved.chunks_exact(self.channels) {
            for (ch, &s) in frame.iter().enumerate() {
                self.in_buf[ch].push(s);
            }
        }

        self.process_ready_frames();

        // Emit as many samples as input from the delay line. The prepended FRAME_SIZE of silence
        // ensures "processed >= emitted + current input" always holds (the initial silence is latency).
        for s in interleaved.iter_mut() {
            *s = self
                .out_buf
                .pop_front()
                .expect("delay line must hold enough processed samples");
        }
        Ok(())
    }

    /// Zero-pad the carried remainder to one frame and process it, then return the final delayed
    /// [`FRAME_SIZE`] samples per channel as interleaved output and close the stream.
    ///
    /// Total output is now total input + [`FRAME_SIZE`] samples per channel (initial silence padding).
    /// After this call, the denoiser returns to the same initial state as [`Denoiser::reset`] and
    /// can process a new stream.
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
            // After the zero-padding above, the delay line always contains at least `take` samples.
            out.push(self.out_buf.pop_front().unwrap_or(0.0));
        }
        self.reset();
        out
    }

    /// Reset the RNN state, carry-over buffers, and delay line.
    ///
    /// After reset, the state matches a newly created denoiser, so the same input produces the same output.
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

    /// Channel count specified at construction.
    pub fn channels(&self) -> u16 {
        self.channels as u16
    }

    /// Preload the delay line by adding FRAME_SIZE of silence to each channel's input buffer.
    /// Processing this silent frame yields exactly 0.0, so the first FRAME_SIZE output samples per
    /// channel are silence padding.
    fn prime_delay(&mut self) {
        for buf in &mut self.in_buf {
            buf.resize(FRAME_SIZE, 0.0);
        }
    }

    /// Process all complete frames and append them to the delay line (out_buf).
    fn process_ready_frames(&mut self) {
        // All channels contain the same number of samples, so checking the first channel is enough.
        while self.in_buf[0].len() >= FRAME_SIZE {
            for ch in 0..self.channels {
                // Scale from ±1.0 to the i16 range and process one frame.
                for (dst, &src) in self.frame_in.iter_mut().zip(&self.in_buf[ch][..FRAME_SIZE]) {
                    *dst = src * I16_SCALE;
                }
                self.states[ch].process_frame(&mut self.frame_out[ch], &self.frame_in);
                self.in_buf[ch].drain(..FRAME_SIZE);
            }
            // Scale back from the i16 range to ±1.0, interleave, and queue for output.
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

    /// Deterministic pseudorandom generator (LCG with Knuth's MMIX constants). Avoids a rand dependency.
    struct Lcg(u64);

    impl Lcg {
        /// Uniform random value in [0, 1), using the top 24 bits.
        fn next_unit(&mut self) -> f32 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((self.0 >> 40) as f32) / (1u32 << 24) as f32
        }
    }

    /// Uniform white noise with amplitude ±amp (fixed seed, deterministic).
    fn white_noise(n: usize, amp: f32) -> Vec<f32> {
        let mut lcg = Lcg(0x5EED_1234_5678_9ABC);
        (0..n)
            .map(|_| (lcg.next_unit() * 2.0 - 1.0) * amp)
            .collect()
    }

    /// First-order IIR low-pass filter (y += a * (x - y)). Used to simulate steady, low-frequency
    /// noise such as fans or air conditioning (a=0.1 corresponds to a cutoff near 800 Hz; deterministic).
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

    /// Process the whole input and return output aligned to the input after removing latency (same length).
    fn run_aligned(channels: u16, input: &[f32]) -> Vec<f32> {
        let mut dn = Denoiser::new(channels).unwrap();
        let mut buf = input.to_vec();
        dn.process(&mut buf).unwrap();
        buf.extend_from_slice(&dn.flush());
        // The first FRAME_SIZE samples per channel (FRAME_SIZE*ch interleaved) are latency silence.
        buf.split_off(FRAME_SIZE * channels as usize)
    }

    /// Measurement helper for choosing thresholds (not normally run): prints the RMS ratio for
    /// each test signal. Run with `cargo test -p flexaudio-denoise -- --ignored --nocapture measure`.
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
        // Two seconds of low-frequency steady noise at 48 kHz mono (low-pass filtered LCG white noise).
        // This is the crate's target, similar to fan or air-conditioning noise. Measurement with
        // measure_rms_ratios gave ratio = 0.0556 (in_rms 0.0399 → out_rms 0.0022; 0.006 for the last second).
        // Set the threshold to 25%, about 4.5 times the measured ratio.
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
        // Two seconds of full-band white noise at 48 kHz mono (amplitude ±0.3). This synthetic
        // noise is far from the training distribution, so RNNoise suppression is weak; the measured
        // ratio is 0.7883 (stationary_noise_rms_strongly_reduced covers steady-noise effectiveness).
        // Here, verify only that it is not amplified and is reduced by at least 10% from measurement.
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
        // Two seconds of a 440 Hz sine wave (amplitude 0.5). Measurement gave ratio = 0.9999,
        // nearly unchanged (RNNoise treats periodic signals as voiced). Since pure-tone handling
        // depends on the model, assert only valid output (no NaN, within ±1.0) and that the tone
        // is not erased (a 50% floor, half the measured level).
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
        // The first FRAME_SIZE samples per channel of output are exactly 0.0 latency padding.
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
        // Streaming in 1000-sample chunks (not a multiple of 480) must match batch output bit for
        // bit, regardless of call size. Also verify the total sample count.
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
        // Interleaved stereo with noise on L and silence on R. R output remains exactly 0, while
        // L output matches processing the same signal as mono bit for bit (checks interleaving and
        // channel independence).
        let n = 48_000; // 1 second per channel, streamed in 1000-sample chunks (=500/ch, not a multiple of 480).
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
        // The same input after reset produces the same output bit for bit. flush also resets automatically.
        let input = white_noise(10_000, 0.3);

        let mut dn = Denoiser::new(1).unwrap();
        let mut first = input.clone();
        dn.process(&mut first).unwrap();

        dn.reset();
        let mut second = input.clone();
        dn.process(&mut second).unwrap();
        assert_eq!(first, second, "reset must restore the initial state");

        // After closing the stream, flush returns to the same initial state as reset.
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
        let mut buf = vec![0.0f32; 999]; // Not a multiple of 2 channels.
        let err = dn.process(&mut buf).unwrap_err();
        assert_eq!(
            err,
            DenoiseError::InvalidLength {
                len: 999,
                channels: 2
            }
        );
        // Do not modify the buffer on error.
        assert!(buf.iter().all(|&x| x == 0.0));
    }

    #[test]
    fn empty_process_and_bare_flush() {
        let mut dn = Denoiser::new(1).unwrap();
        let mut empty: [f32; 0] = [];
        dn.process(&mut empty).unwrap(); // Empty input is a no-op.
        assert_eq!(dn.channels(), 1);

        // Flushing without input still returns exactly the latency padding (silence).
        let tail = dn.flush();
        assert_eq!(tail.len(), FRAME_SIZE);
        assert!(tail.iter().all(|&x| x == 0.0));
    }
}
