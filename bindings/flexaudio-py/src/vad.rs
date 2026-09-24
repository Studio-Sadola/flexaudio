//! Standalone VAD (voice activity detection) class [`Vad`].
//!
//! Python exposure of the add-on that runs silero-VAD offline via ONNX ([`flexaudio_vad`]).
//! Feeding recorded chunks (interleaved f32 in any format) directly to [`Vad::process`]
//! downmixes them to mono and resamples to the VAD rate internally, then detects speech
//! boundaries.

use pyo3::prelude::*;

use flexaudio_vad::Vad as CoreVad;

use crate::config::make_vad_config;
use crate::marshal::{vad_event_to_py, PyVadEvent};
use crate::vad_err_to_py;

/// Streaming VAD. Each instance owns one ONNX session.
///
/// Feeding recorded chunks of any format to [`process`](Vad::process) returns the finalized
/// speech boundaries as a list of [`VadEvent`](PyVadEvent). `at_sample` is in terms of the
/// VAD's internal rate.
// The rubato resampler (the conversion stage in front of the VAD) is !Sync, so the pyclass
// default of Send+Sync cannot be met. Python usage is assumed to be poll-style and
// single-threaded (under the GIL), so the class is made unsendable and pinned to the thread
// that created it.
#[pyclass(module = "flexaudio", name = "Vad", unsendable)]
pub struct Vad {
    inner: CoreVad,
}

#[pymethods]
impl Vad {
    /// Constructs a VAD with the given settings. The defaults match silero-VAD's
    /// `get_speech_timestamps` (= [`VadConfig::default`](flexaudio_vad::VadConfig)).
    /// `sample_rate` must be 8000 or 16000 (anything else raises `ValueError`).
    /// `neg_threshold` is the negative-side threshold for silence detection; `None` means
    /// `max(threshold - 0.15, 0.01)` (following silero).
    #[new]
    #[pyo3(signature = (
        threshold = 0.5,
        min_speech_ms = 250,
        min_silence_ms = 100,
        speech_pad_ms = 30,
        max_speech_ms = 0,
        sample_rate = 16_000,
        neg_threshold = None,
    ))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        threshold: f32,
        min_speech_ms: u32,
        min_silence_ms: u32,
        speech_pad_ms: u32,
        max_speech_ms: u32,
        sample_rate: u32,
        neg_threshold: Option<f32>,
    ) -> PyResult<Self> {
        let config = make_vad_config(
            threshold,
            neg_threshold,
            min_speech_ms,
            min_silence_ms,
            speech_pad_ms,
            max_speech_ms,
            sample_rate,
        );
        let inner = CoreVad::new(config).map_err(vad_err_to_py)?;
        Ok(Vad { inner })
    }

    /// Processes samples of any format (interleaved f32 at `input_sample_rate` /
    /// `input_channels`) and returns a list of the finalized [`VadEvent`](PyVadEvent)s.
    ///
    /// Partial frames are carried over internally, so the input may be split at any position
    /// and passed consecutively (no seams appear). `samples` may be a list, array.array, or
    /// numpy array.
    fn process(
        &mut self,
        samples: Vec<f32>,
        input_sample_rate: u32,
        input_channels: u16,
    ) -> Vec<PyVadEvent> {
        self.inner
            .process_pcm(&samples, input_sample_rate, input_channels)
            .into_iter()
            .map(vad_event_to_py)
            .collect()
    }

    /// Resets all state, the partial-frame buffer, and the resampler (a different stream can
    /// be processed next).
    fn reset(&mut self) {
        self.inner.reset();
    }
}
