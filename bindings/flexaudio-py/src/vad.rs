//! Standalone VAD (speech segment detection) class [`Vad`].
//!
//! Python binding for the addon ([`flexaudio_vad`]) that runs Silero VAD offline with ONNX.
//! Feed recording chunks (interleaved f32 in any format) directly to [`Vad::process`];
//! it converts them to mono, resamples to the VAD rate, and detects speech boundaries.

use pyo3::prelude::*;

use flexaudio_vad::Vad as CoreVad;

use crate::config::make_vad_config;
use crate::marshal::{vad_event_to_py, PyVadEvent};
use crate::vad_err_to_py;

/// Streaming VAD. Each instance holds one ONNX session.
///
/// Feed recording chunks in any format to [`process`](Vad::process) to get confirmed speech boundaries
/// as a list of [`VadEvent`](PyVadEvent). `at_sample` is relative to the VAD’s internal rate.
// rubato’s resampler (the conversion before VAD) is !Sync, so this pyclass cannot meet the default Send+Sync
// bounds. Python use is single-threaded and poll-based (under the GIL), so mark it unsendable and pin it to
// the thread where it was created.
#[pyclass(module = "flexaudio", name = "Vad", unsendable)]
pub struct Vad {
    inner: CoreVad,
}

#[pymethods]
impl Vad {
    /// Create a VAD with the given settings. Defaults match Silero VAD’s `get_speech_timestamps`
    /// (`VadConfig::default` in [`flexaudio_vad::VadConfig`]). `sample_rate` must be
    /// 8000 or 16000 (otherwise `ValueError`). `neg_threshold` is the lower (silence-side) threshold
    /// for silence detection; when `None`, it is `max(threshold - 0.15, 0.01)` (Silero-compatible).
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

    /// Process samples in any format (interleaved f32 with `input_sample_rate` / `input_channels`)
    /// and return a list of confirmed [`VadEvent`](PyVadEvent).
    ///
    /// Partial frames are carried forward internally, so chunks can be split at any point and passed
    /// consecutively without gaps. `samples` can be a list, array.array, or NumPy array.
    fn process(
        &mut self,
        samples: Vec<f32>,
        input_sample_rate: u32,
        input_channels: u16,
    ) -> PyResult<Vec<PyVadEvent>> {
        Ok(self
            .inner
            .process_pcm(&samples, input_sample_rate, input_channels)
            .map_err(vad_err_to_py)?
            .into_iter()
            .map(vad_event_to_py)
            .collect())
    }

    /// Reset all state, the remainder buffer, and the resampler (so another stream can be processed).
    fn reset(&mut self) -> PyResult<()> {
        self.inner.reset().map_err(vad_err_to_py)
    }
}

#[cfg(test)]
mod repro_tests {
    use super::*;
    #[test]
    #[ignore = "repro: C F42"]
    fn repro_p10_f42_standalone_vad_has_flush() {
        Python::initialize();
        Python::attach(|py| {
            let mut vad = Vad::new(0.0, 0, 0, 0, 0, 16_000, Some(0.0)).unwrap();
            assert!(vad
                .process(vec![0.1; 16_000], 16_000, 1)
                .unwrap()
                .is_empty());
            let vad = Py::new(py, vad).unwrap();
            assert!(
                vad.bind(py).hasattr("flush").unwrap(),
                "active standalone VAD segment has no Python flush method"
            );
        });
    }
    #[test]
    fn repro_p10_control_f39_invalid_vad_format_raises() {
        Python::initialize();
        Python::attach(|py| {
            let mut vad = Vad::new(0.5, 250, 100, 30, 0, 16_000, None).unwrap();
            let error = match vad.process(vec![0.0; 512], 16_000, 0) {
                Err(error) => error,
                Ok(_) => panic!("zero channels must fail"),
            };
            assert!(error.is_instance_of::<pyo3::exceptions::PyValueError>(py));
            assert_eq!(vad.inner.converted_sample_position(), 0);
            assert!(vad.process(vec![0.0; 512], 16_000, 1).unwrap().is_empty());
        });
    }
    #[test]
    fn repro_p10_control_core_vad_flush_closes_speech() {
        let mut vad = CoreVad::new(make_vad_config(0.0, Some(0.0), 0, 0, 0, 0, 16_000)).unwrap();
        assert!(vad
            .process_pcm(&vec![0.1; 16_000], 16_000, 1)
            .unwrap()
            .is_empty());
        assert_eq!(vad.flush().unwrap().len(), 2);
    }
}
