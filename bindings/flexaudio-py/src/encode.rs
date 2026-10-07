//! FLAC streaming writer class [`FlacEncoder`].
//!
//! Python interface for the [`flexaudio_encode`] add-on, which compresses recording chunks
//! into a FLAC file as they arrive. With `split_seconds>0`, it rotates through numbered files
//! (`name-001.flac`, `name-002.flac`, ...); rotation is based on frame count, and chunks are
//! never split or dropped.

use std::path::{Path, PathBuf};

use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;

use flexaudio_encode::FlacWriter;

use crate::encode_err_to_py;

/// Maximum sample rate (Hz) allowed by flacenc's FLAC validation; same as [`FlacWriter::create`].
const MAX_SAMPLE_RATE: u32 = 96_000;

/// Build the path for the `index`th split recording file (1-based, pure function).
///
/// For `rec.flac`, insert a zero-padded three-digit sequence before the extension, as in
/// `rec-001.flac, rec-002.flac, ...`. Indices from 1000 onward grow naturally. For a path
/// without an extension (`rec`), append the sequence (`rec-001`). The parent directory is
/// preserved. This follows flexaudio-cli's `split_file_path` convention.
fn split_file_path(base: &Path, index: u64) -> PathBuf {
    let stem = base
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = match base.extension() {
        Some(ext) => format!("{stem}-{index:03}.{}", ext.to_string_lossy()),
        None => format!("{stem}-{index:03}"),
    };
    base.with_file_name(name)
}

enum EncoderState {
    Open,
    Finalized,
    Failed(PyErr),
}

/// Encoder that streams recording chunks to FLAC files.
///
/// Pass interleaved f32 samples (the same format as flexaudio's `AudioChunk.data`) to
/// [`write_chunk`](FlacEncoder::write_chunk), then finalize the header with
/// [`finalize`](FlacEncoder::finalize). It also supports the context manager (`with`) protocol
/// and finalizes when leaving the `with` block.
///
/// With `split_seconds>0`, the current file is finalized and the next numbered file is opened
/// whenever the written frame count reaches `split_seconds × sample_rate`. Each file may be up
/// to one chunk longer than the requested duration. Chunks are never split or dropped.
///
/// Files are opened only when the first chunk arrives (lazy creation). If finalized without
/// writing anything and without splitting, no empty file is created. If dropped without
/// finalize, the underlying `FlacWriter` attempts to close the file; call finalize to reliably
/// detect errors.
#[pyclass(module = "flexaudio", name = "FlacEncoder")]
pub struct FlacEncoder {
    // Base path corresponding to `--out` (numbered-file prefix when splitting; used as-is otherwise).
    base: PathBuf,
    sample_rate: u32,
    channels: u16,
    // Frame threshold per file (split_seconds × sample_rate). 0 means no splitting.
    frames_per_file: u64,
    // Writer for the current file (created lazily; None after rotation or before writing).
    writer: Option<FlacWriter>,
    // Frames written to the current file; reset to 0 on rotation.
    frames_in_current: u64,
    // Number of files opened so far, used to choose the next sequence number.
    files_opened: u64,
    state: EncoderState,
}

impl FlacEncoder {
    /// Retain the first failure, including its Python exception identity.
    fn fail(&mut self, error: PyErr) -> PyErr {
        Python::attach(|py| match &self.state {
            EncoderState::Failed(first) => first.clone_ref(py),
            _ => {
                self.state = EncoderState::Failed(error.clone_ref(py));
                error
            }
        })
    }

    fn check_open(&self) -> PyResult<()> {
        match &self.state {
            EncoderState::Open => Ok(()),
            EncoderState::Finalized => Err(PyRuntimeError::new_err(
                "FlacEncoder is already finalized; cannot write more chunks",
            )),
            EncoderState::Failed(error) => Python::attach(|py| Err(error.clone_ref(py))),
        }
    }

    /// Path of the next file to open. Uses the base path without splitting, or a 1-based sequence when splitting.
    fn next_path(&self) -> PyResult<PathBuf> {
        if self.frames_per_file > 0 {
            let index = self
                .files_opened
                .checked_add(1)
                .ok_or_else(|| PyRuntimeError::new_err("split file sequence overflow"))?;
            Ok(split_file_path(&self.base, index))
        } else {
            Ok(self.base.clone())
        }
    }

    /// Open the output file if it is not already open (lazy creation).
    fn ensure_writer(&mut self) -> PyResult<()> {
        if self.writer.is_none() {
            let path = self.next_path().map_err(|error| self.fail(error))?;
            let writer = FlacWriter::create(&path, self.sample_rate, self.channels)
                .map_err(encode_err_to_py)
                .map_err(|error| self.fail(error))?;
            self.writer = Some(writer);
            self.files_opened += 1;
        }
        Ok(())
    }

    /// Finalize and close the current file, if open.
    fn finalize_current(&mut self) -> PyResult<()> {
        if let Some(writer) = self.writer.take() {
            writer
                .finalize()
                .map_err(encode_err_to_py)
                .map_err(|error| self.fail(error))?;
        }
        Ok(())
    }
}

#[pymethods]
impl FlacEncoder {
    /// Create an encoder that writes 16-bit FLAC to `path`. `channels` must be 1..=2 and
    /// `sample_rate` 1..=96000 Hz (out-of-range values raise `ValueError`). Set
    /// `split_seconds>0` to rotate through numbered files.
    ///
    /// The file is not opened here; it is opened on the first `write_chunk` call.
    #[new]
    #[pyo3(signature = (path, sample_rate, channels, split_seconds = 0))]
    fn new(path: PathBuf, sample_rate: u32, channels: u16, split_seconds: u64) -> PyResult<Self> {
        // Validate the same ranges as FlacWriter::create before creating a file. Lazy creation
        // ensures invalid parameters do not leave an empty file behind.
        if !(1..=2).contains(&channels) {
            return Err(pyo3::exceptions::PyValueError::new_err(format!(
                "channels must be 1 or 2, got {channels}"
            )));
        }
        if !(1..=MAX_SAMPLE_RATE).contains(&sample_rate) {
            return Err(pyo3::exceptions::PyValueError::new_err(format!(
                "sample rate must be 1..={MAX_SAMPLE_RATE} Hz, got {sample_rate}"
            )));
        }
        if path.is_dir() || path.file_stem().is_none_or(|stem| stem.is_empty()) {
            return Err(PyValueError::new_err(
                "destination must have a nonempty file stem and must not be a directory",
            ));
        }
        let frames_per_file = split_seconds
            .checked_mul(u64::from(sample_rate))
            .ok_or_else(|| PyValueError::new_err("split duration is too large"))?;
        Ok(FlacEncoder {
            base: path,
            sample_rate,
            channels,
            frames_per_file,
            writer: None,
            frames_in_current: 0,
            files_opened: 0,
            state: EncoderState::Open,
        })
    }

    /// Append interleaved f32 samples. The length must be a multiple of `channels` (otherwise
    /// `ValueError` is raised). `samples` can be a list, array.array, or NumPy array. Empty input is a no-op.
    ///
    /// If the frame count reaches `split_seconds × sample_rate` after writing, the current file
    /// is finalized and rotation to the next numbered file happens immediately.
    fn write_chunk(&mut self, samples: Vec<f32>) -> PyResult<()> {
        self.check_open()?;
        // Validate before lazy creation: invalid input must not truncate an existing file.
        if !samples.len().is_multiple_of(usize::from(self.channels)) {
            return Err(PyValueError::new_err(
                "sample count must be a multiple of channels",
            ));
        }
        if samples.is_empty() {
            return Ok(());
        }
        let frames = u64::try_from(samples.len() / usize::from(self.channels))
            .map_err(|_| PyValueError::new_err("sample count is too large"))?;
        let next_frames = self
            .frames_in_current
            .checked_add(frames)
            .ok_or_else(|| PyValueError::new_err("frame count overflow"))?;
        self.ensure_writer()?;
        let result = self
            .writer
            .as_mut()
            .expect("writer was just opened")
            .write_chunk(&samples)
            .map_err(encode_err_to_py);
        result.map_err(|error| self.fail(error))?;
        self.frames_in_current = next_frames;

        if self.frames_per_file > 0 && self.frames_in_current >= self.frames_per_file {
            // Finalize the current file and advance to the next sequence number; the next write_chunk opens it.
            self.finalize_current()?;
            self.frames_in_current = 0;
        }
        Ok(())
    }

    /// Write any remaining samples, finalize the header, and close the current file. Safe to
    /// call repeatedly; subsequent calls are no-ops.
    fn finalize(&mut self) -> PyResult<()> {
        if matches!(self.state, EncoderState::Finalized) {
            return Ok(());
        }
        self.check_open()?;
        self.finalize_current()?;
        self.state = EncoderState::Finalized;
        Ok(())
    }

    /// Context manager support; use as `with flexaudio.FlacEncoder(...) as enc:`.
    fn __enter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    /// Finalize when leaving a `with` block. Propagate finalize errors if no exception occurred
    /// in the block; attach any close failure to an active body exception.
    fn __exit__(
        &mut self,
        exc_type: Option<Bound<'_, PyAny>>,
        exc_value: Option<Bound<'_, PyAny>>,
        _traceback: Option<Bound<'_, PyAny>>,
    ) -> PyResult<bool> {
        let result = self.finalize();
        if let Err(error) = result {
            if exc_type.is_none() {
                return Err(error);
            }
            if let Some(body_error) = exc_value {
                let note = format!("FlacEncoder close also failed: {error}");
                if body_error.call_method1("add_note", (note,)).is_err() {
                    // Python <3.11 has no add_note. Keep the body exception primary.
                    let py = body_error.py();
                    if !body_error.is(error.value(py)) {
                        PyErr::from_value(body_error).set_context(py, Some(error));
                    }
                }
            }
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "flexaudio-py-encode-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn invalid_lazy_chunk_preserves_existing_destination() {
        let dir = TestDir::new();
        let path = dir.0.join("existing.flac");
        std::fs::write(&path, b"existing contents").unwrap();
        let mut encoder = FlacEncoder::new(path.clone(), 48_000, 2, 0).unwrap();
        assert!(encoder.write_chunk(vec![0.0]).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"existing contents");
        assert!(encoder.writer.is_none());
        encoder.finalize().unwrap();
    }

    #[test]
    fn finalized_encoder_cannot_reopen_destination() {
        let dir = TestDir::new();
        let path = dir.0.join("recording.flac");
        let mut encoder = FlacEncoder::new(path.clone(), 48_000, 1, 0).unwrap();
        encoder.write_chunk(vec![0.0; 16]).unwrap();
        encoder.finalize().unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert!(encoder.write_chunk(vec![0.0; 16]).is_err());
        encoder.finalize().unwrap();
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }

    #[test]
    fn rotation_open_failure_is_sticky_even_if_destination_is_fixed() {
        Python::initialize();
        let dir = TestDir::new();
        let path = dir.0.join("recording.flac");
        let blocked = split_file_path(&path, 2);
        let mut encoder = FlacEncoder::new(path.clone(), 1, 1, 1).unwrap();
        encoder.write_chunk(vec![0.0]).unwrap();
        let first_bytes = std::fs::read(split_file_path(&path, 1)).unwrap();
        std::fs::create_dir(&blocked).unwrap();
        let first_error = encoder.write_chunk(vec![0.0]).unwrap_err();
        std::fs::remove_dir(&blocked).unwrap();
        let second_error = encoder.write_chunk(vec![0.0]).unwrap_err();
        let finalize_error = encoder.finalize().unwrap_err();
        Python::attach(|py| {
            assert!(first_error.value(py).is(second_error.value(py)));
            assert!(first_error.value(py).is(finalize_error.value(py)));
        });
        assert!(!blocked.exists());
        assert_eq!(
            std::fs::read(split_file_path(&path, 1)).unwrap(),
            first_bytes
        );
    }

    #[test]
    fn invalid_paths_and_split_overflow_are_rejected() {
        let dir = TestDir::new();
        for path in [
            dir.0.clone(),
            PathBuf::new(),
            PathBuf::from("."),
            PathBuf::from("/"),
        ] {
            assert!(FlacEncoder::new(path, 48_000, 1, 1).is_err());
        }
        assert!(FlacEncoder::new(dir.0.join("rec.flac"), 48_000, 1, u64::MAX).is_err());
    }

    #[test]
    fn context_manager_attaches_close_error_without_replacing_body() {
        Python::initialize();
        Python::attach(|py| {
            let mut encoder = FlacEncoder::new(PathBuf::from("unused.flac"), 48_000, 1, 0).unwrap();
            let close_error = PyRuntimeError::new_err("injected close failure");
            encoder.fail(close_error);
            let body_error = PyValueError::new_err("body failure");
            let value = body_error.value(py);
            assert!(!encoder
                .__exit__(
                    Some(value.get_type().into_any()),
                    Some(value.clone().into_any()),
                    None
                )
                .unwrap());
            if let Ok(notes) = value.getattr("__notes__") {
                let notes: Vec<String> = notes.extract().unwrap();
                assert!(notes
                    .iter()
                    .any(|note| note.contains("injected close failure")));
            } else {
                let context = value.getattr("__context__").unwrap();
                assert!(context.to_string().contains("injected close failure"));
            }
            assert_eq!(value.to_string(), "body failure");
            assert!(encoder.finalize().is_err());
        });
    }

    #[test]
    fn hostile_body_exception_keeps_identity_and_close_context() {
        Python::initialize();
        Python::attach(|py| {
            let module = pyo3::types::PyModule::from_code(
                py,
                pyo3::ffi::c_str!(
                    "class HostileError(Exception):\n    def add_note(self, note):\n        raise RuntimeError('note rejected')\n    def __setattr__(self, name, value):\n        raise RuntimeError('assignment rejected')\nbody = HostileError('body failure')"
                ),
                pyo3::ffi::c_str!("encoder_test.py"),
                pyo3::ffi::c_str!("encoder_test"),
            ).unwrap();
            let body = module.getattr("body").unwrap();
            let mut encoder = FlacEncoder::new(PathBuf::from("unused.flac"), 48_000, 1, 0).unwrap();
            let close = PyRuntimeError::new_err("injected close failure");
            let expected = close.clone_ref(py);
            encoder.fail(close);
            assert!(!encoder
                .__exit__(Some(body.get_type().into_any()), Some(body.clone()), None)
                .unwrap());
            assert!(body.getattr("__context__").unwrap().is(expected.value(py)));
            assert_eq!(body.to_string(), "body failure");
        });
    }

    #[test]
    fn split_file_path_inserts_zero_padded_index() {
        assert_eq!(
            split_file_path(Path::new("rec.flac"), 1),
            PathBuf::from("rec-001.flac")
        );
        assert_eq!(
            split_file_path(Path::new("rec.flac"), 2),
            PathBuf::from("rec-002.flac")
        );
        // Digits grow naturally from index 1000 onward.
        assert_eq!(
            split_file_path(Path::new("rec.flac"), 1000),
            PathBuf::from("rec-1000.flac")
        );
        // Preserve the parent directory.
        assert_eq!(
            split_file_path(Path::new("/tmp/out/rec.flac"), 3),
            PathBuf::from("/tmp/out/rec-003.flac")
        );
        // Append the sequence number when there is no extension.
        assert_eq!(
            split_file_path(Path::new("rec"), 5),
            PathBuf::from("rec-005")
        );
    }

    #[test]
    fn frames_per_file_reflects_split_seconds() {
        // split_seconds × sample_rate is the threshold; 0 means no splitting.
        let enc = FlacEncoder::new(PathBuf::from("x.flac"), 48_000, 2, 0).unwrap();
        assert_eq!(enc.frames_per_file, 0);

        let enc = FlacEncoder::new(PathBuf::from("x.flac"), 48_000, 2, 10).unwrap();
        assert_eq!(enc.frames_per_file, 480_000);

        let enc = FlacEncoder::new(PathBuf::from("x.flac"), 16_000, 1, 3).unwrap();
        assert_eq!(enc.frames_per_file, 48_000);
    }

    #[test]
    fn new_rejects_out_of_range_params() {
        assert!(FlacEncoder::new(PathBuf::from("x.flac"), 48_000, 0, 0).is_err());
        assert!(FlacEncoder::new(PathBuf::from("x.flac"), 48_000, 3, 0).is_err());
        assert!(FlacEncoder::new(PathBuf::from("x.flac"), 0, 2, 0).is_err());
        assert!(FlacEncoder::new(PathBuf::from("x.flac"), MAX_SAMPLE_RATE + 1, 2, 0).is_err());
        // Values in range are accepted.
        assert!(FlacEncoder::new(PathBuf::from("x.flac"), 48_000, 2, 0).is_ok());
    }
}
