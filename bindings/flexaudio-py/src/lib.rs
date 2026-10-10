//! flexaudio-py — Python bindings (PyO3 + maturin) that link directly to flexaudio.
//!
//! Bindings for using flexaudio in-process from Python applications. Like flexaudio-napi
//! (for Node), adapted to PyO3.
//!
//! Design:
//! - `open(...)` calls `flexaudio::open` and then `start()` before returning [`Stream`]
//!   (like napi `open_stream`, open also starts the stream).
//! - Polling APIs (`poll_chunk` / `poll_event`) are fast and non-blocking, so they do not release the GIL.
//!   Since pyclass methods run with the GIL held, concurrent access to the internal `flexaudio::Stream`
//!   does not occur (there is no bridge thread as in napi). Integrated VAD / denoise processing
//!   also runs when poll_chunk is called, while the GIL is held.
//! - Chunk `data` contains interleaved `f32` as raw little-endian bytes (`bytes`).
//!   numpy users can read it with `np.frombuffer(chunk.data, dtype=np.float32)`.
//!
//! # Module layout
//! Keep responsibilities in separate files to avoid a god class:
//! - This file (`lib.rs`): shared helpers (error and enum conversion), `devices()`, and pymodule registration.
//! - `marshal`: data types passed to Python (AudioChunk / StreamEvent / DeviceInfo / VadEvent /
//!   DeviceEvent) and their conversions.
//! - `config`: convert and validate Python arguments as core configs (StreamConfig / VadConfig).
//! - `stream`: recording stream [`Stream`], `open()`, and integrated VAD / denoise.
//! - `vad` / `denoise` / `encode`: standalone addons ([`Vad`] / [`Denoiser`] / [`FlacEncoder`]).
//! - `watcher`: device hotplug monitoring ([`DeviceWatcher`] and `watch_devices()`).
//!
//! No network communication occurs at runtime (PyO3 is only a Python extension bridge; embedded VAD
//! model, FLAC encoder, and RNNoise model require no files or network access).

use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;

// Alias the dependency crate `flexaudio` as `fa`. This cdylib uses the same name for `[lib] name` and
// `#[pymodule] fn flexaudio`, so bare `flexaudio::` could be ambiguous between the
// crate and module. The alias avoids that ambiguity.
use ::flexaudio as fa;
use fa::{ProcessMode, SourceKind};

mod config;
mod denoise;
mod encode;
mod errors;
mod marshal;
mod stream;
mod vad;
mod watcher;
mod whisper_buffer;
mod whisper_marshal;
mod whisper_vad;

use marshal::{device_info_to_py, process_info_to_py};

// ---------------------------------------------------------------------------
// Error conversion
// ---------------------------------------------------------------------------

pub(crate) use errors::to_py_err;

/// Convert VadError to a Python exception. Invalid configuration becomes `ValueError`; model loading and inference failures
/// become `RuntimeError`.
pub(crate) fn vad_err_to_py(err: flexaudio_vad::VadError) -> PyErr {
    let msg = err.to_string();
    match err {
        flexaudio_vad::VadError::InvalidConfig(_) | flexaudio_vad::VadError::InvalidFormat(_) => {
            PyValueError::new_err(msg)
        }
        flexaudio_vad::VadError::ModelLoad(_)
        | flexaudio_vad::VadError::Inference(_)
        | flexaudio_vad::VadError::Reset(_)
        | flexaudio_vad::VadError::Resample(_) => PyRuntimeError::new_err(msg),
    }
}

/// Convert DenoiseError to a Python exception. Invalid channel count or length is an argument error, so use `ValueError`.
pub(crate) fn denoise_err_to_py(err: flexaudio_denoise::DenoiseError) -> PyErr {
    PyValueError::new_err(err.to_string())
}

/// Convert EncodeError to a Python exception. Unsupported parameters become `ValueError`, I/O errors become `OSError`,
/// and encoder errors become `RuntimeError`. Future variants (`#[non_exhaustive]`) default to
/// `RuntimeError`.
pub(crate) fn encode_err_to_py(err: flexaudio_encode::EncodeError) -> PyErr {
    use flexaudio_encode::EncodeError;
    let msg = err.to_string();
    match err {
        EncodeError::Unsupported(_) => PyValueError::new_err(msg),
        EncodeError::Io(_) => pyo3::exceptions::PyOSError::new_err(msg),
        _ => PyRuntimeError::new_err(msg),
    }
}

// ---------------------------------------------------------------------------
// Enum-to-string conversion helpers
// ---------------------------------------------------------------------------

/// Convert [`SourceKind`] to a Python string ("mic"|"system"|"process"|"mix").
pub(crate) fn source_kind_str(k: SourceKind) -> &'static str {
    match k {
        SourceKind::Mic => "mic",
        SourceKind::SystemLoopback => "system",
        SourceKind::ProcessLoopback => "process",
        SourceKind::Mix => "mix",
    }
}

/// Convert "mic"|"system"|"process"|"mix" to [`SourceKind`]. Invalid values raise `ValueError`.
pub(crate) fn parse_source_kind(s: &str) -> PyResult<SourceKind> {
    match s {
        "mic" => Ok(SourceKind::Mic),
        "system" => Ok(SourceKind::SystemLoopback),
        "process" => Ok(SourceKind::ProcessLoopback),
        "mix" => Ok(SourceKind::Mix),
        other => Err(PyValueError::new_err(format!(
            "unknown kind: {other:?} (expected mic|system|process|mix)"
        ))),
    }
}

/// Convert "include"|"exclude" to [`ProcessMode`] (process only). The default is Include.
pub(crate) fn parse_process_mode(s: &str) -> PyResult<ProcessMode> {
    match s {
        "include" => Ok(ProcessMode::Include),
        "exclude" => Ok(ProcessMode::Exclude),
        other => Err(PyValueError::new_err(format!(
            "unknown mode: {other:?} (expected include|exclude)"
        ))),
    }
}

/// Format bool as Python-style "True"/"False" (for __repr__).
pub(crate) fn bool_repr(b: bool) -> &'static str {
    if b {
        "True"
    } else {
        "False"
    }
}

// ---------------------------------------------------------------------------
// Module functions
// ---------------------------------------------------------------------------

/// List the complete device inventory. An empty complete inventory is valid.
/// Incomplete or failed discovery raises a typed exception, including when the
/// PipeWire daemon is unreachable on Linux.
#[pyfunction]
fn devices() -> PyResult<Vec<marshal::PyDeviceInfo>> {
    let list = fa::devices().map_err(to_py_err)?;
    Ok(list.into_iter().map(device_info_to_py).collect())
}

/// List processes that can be targeted for per-process capture (`open("process", process_id=...)`)
/// and have audio output. The calling process is not included.
///
/// Results are ordered by active output first, then display name, then pid. An empty list means
/// the feature works but has no candidates. Raise `RuntimeError` if per-process capture is
/// unavailable (PipeWire missing on Linux, macOS older than 14.4, Windows is older than build 20348
/// (Windows 11 / Windows Server 2022 or later is required), or an unsupported OS), if the OS does
/// not respond within 3 seconds, or if the previous query is still in progress.
#[pyfunction]
fn processes(py: Python<'_>) -> PyResult<Vec<marshal::PyProcessInfo>> {
    let list = py.detach(fa::processes).map_err(to_py_err)?;
    Ok(list.into_iter().map(process_info_to_py).collect())
}

// ---------------------------------------------------------------------------
// Module definition
// ---------------------------------------------------------------------------

/// Python module `flexaudio`.
#[pymodule]
fn flexaudio(m: &Bound<'_, PyModule>) -> PyResult<()> {
    errors::register(m)?;
    // Functions.
    m.add_function(wrap_pyfunction!(devices, m)?)?;
    m.add_function(wrap_pyfunction!(processes, m)?)?;
    m.add_function(wrap_pyfunction!(stream::open, m)?)?;
    m.add_function(wrap_pyfunction!(watcher::watch_devices, m)?)?;

    // Stream and its returned data types.
    m.add_class::<stream::Stream>()?;
    m.add_class::<marshal::PyAudioChunk>()?;
    m.add_class::<marshal::PyStreamEvent>()?;
    m.add_class::<marshal::PyDeviceInfo>()?;
    m.add_class::<marshal::PyProcessInfo>()?;
    m.add_class::<marshal::PyVadEvent>()?;
    m.add_class::<marshal::PyDeviceEvent>()?;

    // Standalone addons and monitoring.
    whisper_vad::register(m)?;
    m.add_class::<vad::Vad>()?;
    m.add_class::<denoise::Denoiser>()?;
    m.add_class::<encode::FlacEncoder>()?;
    m.add_class::<watcher::DeviceWatcher>()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Test the pure parts of shared helpers (enum and error conversion) without a Python runtime.
    //!
    //! Paths that create pyclasses (such as PyBytes) or require PyDict need a Python host,
    //! so test only Python-independent pure conversions here. The config, marshal, and encode
    //! modules also have tests for their pure logic.
    //!
    //! Import smoke test (run by CI with a Python host):
    //! ```sh
    //! cargo build -p flexaudio-py --features extension-module
    //! cp target/debug/libflexaudio.so /tmp/fa/flexaudio.so
    //! python3 - <<'PY'
    //! import sys; sys.path.insert(0, "/tmp/fa")
    //! import flexaudio as fa
    //! assert fa.Vad().process([0.0]*16000, 16000, 1) == []      # Silence contains no speech.
    //! d = fa.Denoiser(1); assert len(d.process([0.0]*1000)) == 1000; assert len(d.flush()) == 480
    //! fa.watch_devices().poll_event()                           # None or DeviceEvent.
    //! try: fa.open("mic", denoise=True, output_rate=16000); assert False
    //! except ValueError: pass                                   # denoise supports only 48 kHz.
    //! PY
    //! ```
    //! (If maturin is available, `maturin develop` can do the same.)

    use super::*;

    #[test]
    fn source_kind_roundtrips() {
        for (s, k) in [
            ("mic", SourceKind::Mic),
            ("system", SourceKind::SystemLoopback),
            ("process", SourceKind::ProcessLoopback),
            ("mix", SourceKind::Mix),
        ] {
            assert_eq!(parse_source_kind(s).unwrap(), k);
            assert_eq!(source_kind_str(k), s);
        }
    }

    #[test]
    fn parse_source_kind_rejects_unknown() {
        assert!(parse_source_kind("bogus").is_err());
    }

    #[test]
    fn parse_process_mode_defaults_and_explicit() {
        assert_eq!(parse_process_mode("include").unwrap(), ProcessMode::Include);
        assert_eq!(parse_process_mode("exclude").unwrap(), ProcessMode::Exclude);
        assert!(parse_process_mode("nope").is_err());
    }

    #[test]
    fn bool_repr_matches_python() {
        assert_eq!(bool_repr(true), "True");
        assert_eq!(bool_repr(false), "False");
    }

    #[test]
    fn to_py_err_maps_variants_without_panic() {
        Python::initialize();
        // Type selection needs no Python runtime (match only). The message comes from Display.
        let err = fa::Error::DeviceNotFound;
        assert!(err.to_string().contains("device not found"));
        // Verify only that conversion does not panic (PyErr contents require Python).
        let _ = to_py_err(fa::Error::DeviceNotFound);
        let _ = to_py_err(fa::Error::InvalidArg("x".to_string()));
    }

    #[test]
    fn addon_err_maps_without_panic() {
        // Also verify addon error conversion does not panic (only the type branch is tested).
        let _ = vad_err_to_py(flexaudio_vad::VadError::InvalidConfig("x".into()));
        let _ = vad_err_to_py(flexaudio_vad::VadError::ModelLoad("x".into()));
        let _ = denoise_err_to_py(flexaudio_denoise::DenoiseError::InvalidChannels(3));
        let _ = encode_err_to_py(flexaudio_encode::EncodeError::Unsupported("x".into()));
    }
}

#[cfg(test)]
mod repro_tests {
    use super::*;
    #[test]
    fn repro_p10_f44_device_errors_keep_distinct_kinds() {
        Python::initialize();
        Python::attach(|py| {
            let missing = to_py_err(fa::Error::DeviceNotFound);
            let lost = to_py_err(fa::Error::DeviceLost);
            assert!(
                !missing.get_type(py).is(lost.get_type(py)),
                "DeviceNotFound and DeviceLost both map to RuntimeError with no typed kind"
            );
        });
    }
    #[test]
    fn repro_p10_control_invalid_arg_is_value_error() {
        Python::initialize();
        Python::attach(|py| {
            let error = to_py_err(fa::Error::InvalidArg("benign validation".into()));
            assert!(error.is_instance_of::<PyValueError>(py));
            assert!(error.to_string().contains("benign validation"));
            let payload = error.value(py).getattr("audio_error").unwrap();
            let payload = payload
                .extract::<PyRef<'_, crate::errors::AudioError>>()
                .unwrap();
            assert!(
                matches!(&payload.0, fa::Error::InvalidArg(detail) if detail == "benign validation")
            );
        });
    }
}

#[cfg(test)]
mod boundary_tests {
    use super::*;
    use pyo3::types::PyDict;

    #[test]
    fn non_twenty_chunk_duration_rejects_before_addons_or_device_access() {
        Python::initialize();
        Python::attach(|py| {
            let module = PyModule::new(py, "flexaudio").unwrap();
            flexaudio(&module).unwrap();
            let locals = PyDict::new(py);
            locals.set_item("flexaudio", module).unwrap();
            py.run(
                pyo3::ffi::c_str!(
                    r#"
failures = []
for duration in (0, 10, 40, 4294967295):
    try:
        flexaudio.open('mic', chunk_ms=duration, denoise=True, output_rate=16000,
                       vad={'threshold': 'invalid'})
    except flexaudio.InvalidArgumentError as error:
        assert error.audio_error.kind == 'invalidArg'
        assert 'chunk_ms' in str(error)
        failures.append(error.audio_error)
    else:
        raise AssertionError('unsupported chunk duration was accepted')
"#
                ),
                None,
                Some(&locals),
            )
            .unwrap();
            for payload in locals
                .get_item("failures")
                .unwrap()
                .unwrap()
                .try_iter()
                .unwrap()
            {
                let payload = payload.unwrap();
                let payload = payload
                    .extract::<PyRef<'_, crate::errors::AudioError>>()
                    .unwrap();
                assert!(
                    matches!(&payload.0, fa::Error::InvalidArg(detail) if detail.contains("chunk_ms"))
                );
            }
        });
    }
}
