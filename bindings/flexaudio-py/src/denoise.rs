//! Standalone noise-suppression class [`Denoiser`].
//!
//! Python exposure of the offline noise-suppression add-on based on RNNoise (nnnoiseless)
//! ([`flexaudio_denoise`]).

use pyo3::prelude::*;

use flexaudio_denoise::Denoiser as CoreDenoiser;

use crate::denoise_err_to_py;

/// Streaming noise suppressor.
///
/// **Assumes 48kHz**: input must be ±1.0-normalized, 48kHz, interleaved f32 (because RNNoise
/// only runs on fixed 48kHz frames). Stereo channels are processed independently.
///
/// Latency: the output is the input delayed by 480 samples/ch (10ms at 48kHz). The first
/// 480 samples/ch of the stream are delay padding (silence), and the remaining tail is returned
/// by [`flush`](Denoiser::flush).
#[pyclass(module = "flexaudio", name = "Denoiser")]
pub struct Denoiser {
    inner: CoreDenoiser,
}

#[pymethods]
impl Denoiser {
    /// Constructs with the given channel count (1 = mono / 2 = stereo interleaved). Anything
    /// outside 1..=2 raises `ValueError`.
    #[new]
    fn new(channels: u16) -> PyResult<Self> {
        let inner = CoreDenoiser::new(channels).map_err(denoise_err_to_py)?;
        Ok(Denoiser { inner })
    }

    /// Noise-suppresses interleaved samples of any length (±1.0-normalized, 48kHz) and returns
    /// them.
    ///
    /// The length must be a multiple of the channel count (otherwise `ValueError`). Leftovers
    /// carry over to the next call, so the input may be split at any position and passed
    /// consecutively. `samples` may be a list, array.array, or numpy array. Returns the
    /// noise-suppressed samples of the same length (the head is the latency silence).
    fn process(&mut self, mut samples: Vec<f32>) -> PyResult<Vec<f32>> {
        self.inner
            .process(&mut samples)
            .map_err(denoise_err_to_py)?;
        Ok(samples)
    }

    /// Processes the carried-over leftovers, returns the trailing 480 samples/ch of latency,
    /// and closes the stream. Afterwards it is back in the same initial state as after
    /// [`reset`](Denoiser::reset) (it can be reused right away).
    fn flush(&mut self) -> Vec<f32> {
        self.inner.flush()
    }

    /// Resets all state, the carry-over buffer, and the delay line (the same input yields the
    /// same output).
    fn reset(&mut self) {
        self.inner.reset();
    }

    /// The channel count given at construction.
    fn channels(&self) -> u16 {
        self.inner.channels()
    }
}
