//! flexaudio-py — Python binding (PyO3 + maturin). Links flexaudio directly.
//!
//! The binding that lets Python apps use flexaudio in-process. It translates the conventions
//! of flexaudio-napi (for Node) to PyO3.
//!
//! Design:
//! - `open(...)` calls `flexaudio::open`, goes straight through `start()`, and returns a
//!   [`Stream`] (like napi's `open_stream`, open does everything up to start).
//! - The poll APIs (`poll_chunk` / `poll_event`) are non-blocking and fast, so they do not
//!   release the GIL. pyclass methods are called with the GIL held, so concurrent access to
//!   the inner `flexaudio::Stream` cannot happen (there is no bridge thread as in napi). The
//!   integrated VAD / denoise processing also runs on the spot where poll_chunk is called
//!   (under the GIL).
//! - A chunk's `data` is interleaved `f32` passed as raw little-endian bytes (`bytes`).
//!   numpy users read it with `np.frombuffer(chunk.data, dtype=np.float32)`.
//!
//! # Module layout
//! Files are split by responsibility to avoid a god class:
//! - This file (`lib.rs`): shared helpers (error conversion, enum conversion), `devices()`,
//!   and pymodule registration.
//! - `marshal`: data types passed to Python (AudioChunk / StreamEvent / DeviceInfo / VadEvent /
//!   DeviceEvent) and their conversions.
//! - `config`: conversion and validation from Python arguments to the core's configs
//!   (StreamConfig / VadConfig).
//! - `stream`: the recording stream [`Stream`] and `open()`, plus VAD / denoise integration.
//! - `vad` / `denoise` / `encode`: standalone add-ons ([`Vad`] / [`Denoiser`] / [`FlacEncoder`]).
//! - `watcher`: device hotplug watch ([`DeviceWatcher`] and `watch_devices()`).
//!
//! No network communication happens at runtime (PyO3 is only the Python extension bridge; the
//! embedded VAD model, FLAC encoder, and RNNoise model need no files or network either).

use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;

// Refer to the dependency crate `flexaudio` as `fa`. This cdylib's `[lib] name` and
// `#[pymodule] fn flexaudio` both create the name `flexaudio`, so a bare `flexaudio::` can
// collide between the crate and the module. The alias removes the ambiguity.
use ::flexaudio as fa;
use fa::{ProcessMode, SourceKind};

mod config;
mod denoise;
mod encode;
mod marshal;
mod stream;
mod vad;
mod watcher;

use marshal::{device_info_to_py, process_info_to_py};

// ---------------------------------------------------------------------------
// Error conversion
// ---------------------------------------------------------------------------

/// flexaudio::Error → Python exception. Argument errors become `ValueError`, everything else
/// `RuntimeError`. Both keep the message (Display).
pub(crate) fn to_py_err(err: fa::Error) -> PyErr {
    let msg = err.to_string();
    match err {
        fa::Error::InvalidArg(_) | fa::Error::UnsupportedFormat(_) => PyValueError::new_err(msg),
        _ => PyRuntimeError::new_err(msg),
    }
}

/// VadError → Python exception. Invalid config becomes `ValueError`; model load / inference
/// failures become `RuntimeError`.
pub(crate) fn vad_err_to_py(err: flexaudio_vad::VadError) -> PyErr {
    let msg = err.to_string();
    match err {
        flexaudio_vad::VadError::InvalidConfig(_) => PyValueError::new_err(msg),
        flexaudio_vad::VadError::ModelLoad(_) | flexaudio_vad::VadError::Inference(_) => {
            PyRuntimeError::new_err(msg)
        }
    }
}

/// DenoiseError → Python exception. Invalid channel counts and lengths are both argument
/// errors, so they become `ValueError`.
pub(crate) fn denoise_err_to_py(err: flexaudio_denoise::DenoiseError) -> PyErr {
    PyValueError::new_err(err.to_string())
}

/// EncodeError → Python exception. Unsupported parameters become `ValueError`, I/O becomes
/// `OSError`, and internal encoder errors become `RuntimeError`. Variants added in the future
/// (`#[non_exhaustive]`) fall back to `RuntimeError`.
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
// Enum ↔ string conversion helpers
// ---------------------------------------------------------------------------

/// Converts a [`SourceKind`] to its Python-facing string ("mic"|"system"|"process"|"mix").
pub(crate) fn source_kind_str(k: SourceKind) -> &'static str {
    match k {
        SourceKind::Mic => "mic",
        SourceKind::SystemLoopback => "system",
        SourceKind::ProcessLoopback => "process",
        SourceKind::Mix => "mix",
    }
}

/// Converts "mic"|"system"|"process"|"mix" to a [`SourceKind`]. Invalid values raise
/// `ValueError`.
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

/// Converts "include"|"exclude" to a [`ProcessMode`] (process only). The default is Include.
pub(crate) fn parse_process_mode(s: &str) -> PyResult<ProcessMode> {
    match s {
        "include" => Ok(ProcessMode::Include),
        "exclude" => Ok(ProcessMode::Exclude),
        other => Err(PyValueError::new_err(format!(
            "unknown mode: {other:?} (expected include|exclude)"
        ))),
    }
}

/// Renders a bool as "True"/"False", Python style (for __repr__).
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

/// Enumerates the available devices. In a headless environment an empty list is not an error.
#[pyfunction]
fn devices() -> PyResult<Vec<marshal::PyDeviceInfo>> {
    let list = fa::devices().map_err(to_py_err)?;
    Ok(list.into_iter().map(device_info_to_py).collect())
}

/// Enumerates the processes with audio output that can be targeted by per-process capture
/// (`open("process", process_id=...)`). The calling process itself is not included.
///
/// Ordered by currently-outputting first → display name → pid. An empty list means supported
/// but no candidates. Raises `RuntimeError` when per-process capture is unavailable in this
/// environment (PipeWire absent on Linux, macOS older than 14.4, Windows not build 20348 or
/// later (Windows 11, Windows Server 2022), or an unsupported OS), when the OS does not respond
/// within 3 seconds, or when a previous query has not finished yet. Releases the GIL during
/// enumeration (up to 3 seconds).
#[pyfunction]
fn processes(py: Python<'_>) -> PyResult<Vec<marshal::PyProcessInfo>> {
    let list = py.detach(fa::processes).map_err(to_py_err)?;
    Ok(list.into_iter().map(process_info_to_py).collect())
}

// ---------------------------------------------------------------------------
// Module definition
// ---------------------------------------------------------------------------

/// The Python module `flexaudio`.
#[pymodule]
fn flexaudio(m: &Bound<'_, PyModule>) -> PyResult<()> {
    // Functions.
    m.add_function(wrap_pyfunction!(devices, m)?)?;
    m.add_function(wrap_pyfunction!(processes, m)?)?;
    m.add_function(wrap_pyfunction!(stream::open, m)?)?;
    m.add_function(wrap_pyfunction!(watcher::watch_devices, m)?)?;

    // The stream and the data types it returns.
    m.add_class::<stream::Stream>()?;
    m.add_class::<marshal::PyAudioChunk>()?;
    m.add_class::<marshal::PyStreamEvent>()?;
    m.add_class::<marshal::PyDeviceInfo>()?;
    m.add_class::<marshal::PyProcessInfo>()?;
    m.add_class::<marshal::PyVadEvent>()?;
    m.add_class::<marshal::PyDeviceEvent>()?;

    // Standalone add-ons and watch.
    m.add_class::<vad::Vad>()?;
    m.add_class::<denoise::Denoiser>()?;
    m.add_class::<encode::FlacEncoder>()?;
    m.add_class::<watcher::DeviceWatcher>()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Verifies the pure parts of the shared helpers (enum conversion, error conversion)
    //! without a Python runtime.
    //!
    //! Paths that create pyclasses (PyBytes etc.) or need a PyDict require a Python host, so
    //! only the Python-independent pure conversions are checked here. The config / marshal /
    //! encode modules each have tests for their own pure logic as well.
    //!
    //! Real-import smoke test (on the Python host side, for CI):
    //! ```sh
    //! cargo build -p flexaudio-py --features extension-module
    //! cp target/debug/libflexaudio.so /tmp/fa/flexaudio.so
    //! python3 - <<'PY'
    //! import sys; sys.path.insert(0, "/tmp/fa")
    //! import flexaudio as fa
    //! assert fa.Vad().process([0.0]*16000, 16000, 1) == []      # silence has no speech
    //! d = fa.Denoiser(1); assert len(d.process([0.0]*1000)) == 1000; assert len(d.flush()) == 480
    //! fa.watch_devices().poll_event()                           # None or DeviceEvent
    //! try: fa.open("mic", denoise=True, output_rate=16000); assert False
    //! except ValueError: pass                                   # denoise is 48k-only
    //! PY
    //! ```
    //! (With maturin available, `maturin develop` does the same.)

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
        // Kind classification needs no Python runtime (match only). The message comes from
        // Display.
        let err = fa::Error::DeviceNotFound;
        assert!(err.to_string().contains("device not found"));
        // Only check that the conversion itself does not panic (PyErr contents need Python).
        let _ = to_py_err(fa::Error::DeviceNotFound);
        let _ = to_py_err(fa::Error::InvalidArg("x".to_string()));
    }

    #[test]
    fn addon_err_maps_without_panic() {
        // The add-on error conversions must not panic either (kind branching only).
        let _ = vad_err_to_py(flexaudio_vad::VadError::InvalidConfig("x".into()));
        let _ = vad_err_to_py(flexaudio_vad::VadError::ModelLoad("x".into()));
        let _ = denoise_err_to_py(flexaudio_denoise::DenoiseError::InvalidChannels(3));
        let _ = encode_err_to_py(flexaudio_encode::EncodeError::Unsupported("x".into()));
    }
}
