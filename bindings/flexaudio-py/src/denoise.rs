//! Standalone noise suppression class [`Denoiser`].
//!
//! Python binding for the offline noise suppression addon ([`flexaudio_denoise`]) using
//! RNNoise (`nnnoiseless`).

use pyo3::prelude::*;

use flexaudio_denoise::Denoiser as CoreDenoiser;

use crate::denoise_err_to_py;

/// Streaming noise suppressor.
///
/// **Requires 48 kHz**: Input must be interleaved f32 normalized to ±1.0 at 48 kHz because
/// RNNoise only supports fixed frames at 48 kHz. Stereo channels are processed independently.
///
/// Latency: Output trails input by 480 samples per channel (10 ms at 48 kHz). The first 480
/// samples per channel are silence padding; [`flush`](Denoiser::flush) returns the remaining tail.
#[pyclass(module = "flexaudio", name = "Denoiser")]
pub struct Denoiser {
    inner: CoreDenoiser,
}

#[pymethods]
impl Denoiser {
    /// Create a suppressor with the given channel count (1 = mono, 2 = interleaved stereo).
    /// Values outside 1..=2 raise `ValueError`.
    #[new]
    fn new(channels: u16) -> PyResult<Self> {
        let inner = CoreDenoiser::new(channels).map_err(denoise_err_to_py)?;
        Ok(Denoiser { inner })
    }

    /// Suppress noise in any-length interleaved samples (normalized to ±1.0 at 48 kHz) and return them.
    ///
    /// Length must be a multiple of the channel count, or `ValueError` is raised. Partial frames
    /// carry over to the next call, so input can be split and passed in consecutive chunks.
    /// `samples` can be a list, array.array, or NumPy array. Returns noise-suppressed samples of
    /// the same length, with latency silence at the start.
    fn process(&mut self, mut samples: Vec<f32>) -> PyResult<Vec<f32>> {
        self.inner
            .process(&mut samples)
            .map_err(denoise_err_to_py)?;
        Ok(samples)
    }

    /// Process any carried-over partial frame, return the 480-sample-per-channel latency tail,
    /// and close the stream. Resets to the same initial state as [`reset`](Denoiser::reset), so
    /// the instance can be reused.
    fn flush(&mut self) -> Vec<f32> {
        self.inner.flush()
    }

    /// Reset the state, carry-over buffer, and delay line (identical input then produces identical output).
    fn reset(&mut self) {
        self.inner.reset();
    }

    /// Channel count specified at construction.
    fn channels(&self) -> u16 {
        self.inner.channels()
    }
}
