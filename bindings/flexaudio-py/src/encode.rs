//! FLAC streaming writer class [`FlacEncoder`].
//!
//! Python interface for the [`flexaudio_encode`] add-on, which compresses recording chunks
//! into a FLAC file as they arrive. With `split_seconds>0`, it rotates through numbered files
//! (`name-001.flac`, `name-002.flac`, ...); rotation is based on frame count, and chunks are
//! never split or dropped.

use std::path::{Path, PathBuf};

use pyo3::exceptions::PyRuntimeError;
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
    // Reject write_chunk calls after finalize.
    finalized: bool,
}

impl FlacEncoder {
    /// Path of the next file to open. Uses the base path without splitting, or a 1-based sequence when splitting.
    fn next_path(&self) -> PathBuf {
        if self.frames_per_file > 0 {
            split_file_path(&self.base, self.files_opened + 1)
        } else {
            self.base.clone()
        }
    }

    /// Open the output file if it is not already open (lazy creation).
    fn ensure_writer(&mut self) -> PyResult<()> {
        if self.writer.is_none() {
            let path = self.next_path();
            let writer = FlacWriter::create(&path, self.sample_rate, self.channels)
                .map_err(encode_err_to_py)?;
            self.writer = Some(writer);
            self.files_opened += 1;
        }
        Ok(())
    }

    /// Finalize and close the current file, if open.
    fn finalize_current(&mut self) -> PyResult<()> {
        if let Some(writer) = self.writer.take() {
            writer.finalize().map_err(encode_err_to_py)?;
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
        Ok(FlacEncoder {
            base: path,
            sample_rate,
            channels,
            // split_seconds × sample_rate. Saturate on overflow, which is not expected in practice.
            frames_per_file: split_seconds.saturating_mul(u64::from(sample_rate)),
            writer: None,
            frames_in_current: 0,
            files_opened: 0,
            finalized: false,
        })
    }

    /// Append interleaved f32 samples. The length must be a multiple of `channels` (otherwise
    /// `ValueError` is raised). `samples` can be a list, array.array, or NumPy array. Empty input is a no-op.
    ///
    /// If the frame count reaches `split_seconds × sample_rate` after writing, the current file
    /// is finalized and rotation to the next numbered file happens immediately.
    fn write_chunk(&mut self, samples: Vec<f32>) -> PyResult<()> {
        if self.finalized {
            return Err(PyRuntimeError::new_err(
                "FlacEncoder is already finalized; cannot write more chunks",
            ));
        }
        if samples.is_empty() {
            return Ok(());
        }
        self.ensure_writer()?;
        {
            let writer = self.writer.as_mut().expect("writer was just opened");
            // FlacWriter returns Unsupported if the length is not a multiple of the channel count.
            writer.write_chunk(&samples).map_err(encode_err_to_py)?;
        }
        // write_chunk succeeded, so the length is a multiple of channels. Accumulate the frame count.
        let frames = (samples.len() / self.channels as usize) as u64;
        self.frames_in_current += frames;

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
        self.finalize_current()?;
        self.finalized = true;
        Ok(())
    }

    /// Context manager support; use as `with flexaudio.FlacEncoder(...) as enc:`.
    fn __enter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    /// Finalize when leaving a `with` block. Propagate finalize errors if no exception occurred
    /// in the block; while another exception is active, use best effort to avoid masking it.
    fn __exit__(
        &mut self,
        exc_type: Option<Bound<'_, PyAny>>,
        _exc_value: Option<Bound<'_, PyAny>>,
        _traceback: Option<Bound<'_, PyAny>>,
    ) -> PyResult<bool> {
        let result = self.finalize_current();
        self.finalized = true;
        // Propagate finalize errors after normal block completion. If an exception is active,
        // preserve it and swallow finalize errors (the underlying Drop will not run; writer was taken).
        if exc_type.is_none() {
            result?;
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
