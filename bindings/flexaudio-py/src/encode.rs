//! Incremental FLAC writer class [`FlacEncoder`].
//!
//! Python exposure of the add-on that incrementally compresses recorded chunks into a FLAC
//! file ([`flexaudio_encode`]). With `split_seconds>0` it rotates into numbered files
//! (`name-001.flac`, `name-002.flac`, ...) (boundaries are frame-count based; chunks are never
//! split and nothing is dropped).

use std::path::{Path, PathBuf};

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

use flexaudio_encode::FlacWriter;

use crate::encode_err_to_py;

/// The maximum sample rate (Hz) that flacenc's validation allows for the FLAC format. Same
/// value as in [`FlacWriter::create`].
const MAX_SAMPLE_RATE: u32 = 96_000;

/// Builds the file path of the `index`-th (1-based) file of a split recording (pure function).
///
/// For `rec.flac`, inserts a 3-digit zero-padded sequence number before the extension, as in
/// `rec-001.flac, rec-002.flac, ...`. From the 1000th on, the number of digits grows
/// naturally. A path without an extension (`rec`) gets the number appended (`rec-001`). The
/// parent directory is preserved. Same convention as `split_file_path` in flexaudio-cli.
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

/// Encoder that incrementally writes recorded chunks to FLAC.
///
/// Feed interleaved f32 (the same shape as flexaudio's `AudioChunk.data`) to
/// [`write_chunk`](FlacEncoder::write_chunk), and when done, finalize the header with
/// [`finalize`](FlacEncoder::finalize). It also supports the context manager protocol (`with`)
/// and finalizes when leaving the `with` block.
///
/// With `split_seconds>0`, each time the number of written frames reaches
/// `split_seconds × sample_rate`, the current file is finalized and the next numbered file is
/// switched to (each file can be up to 1 chunk longer than the given seconds; chunks are never
/// split and nothing is dropped).
///
/// The file is not opened until the first chunk arrives (lazy creation). Finalizing without
/// ever writing and without splitting creates no empty file. Even if it is discarded without
/// calling finalize, the underlying `FlacWriter`'s Drop closes it best-effort (call finalize
/// if you need to detect failures reliably).
#[pyclass(module = "flexaudio", name = "FlacEncoder")]
pub struct FlacEncoder {
    // Base path equivalent to `--out` (the source of the numbered names when splitting; used
    // as-is without splitting).
    base: PathBuf,
    sample_rate: u32,
    channels: u16,
    // Frame-count threshold per file (split_seconds × sample_rate). 0 = no splitting.
    frames_per_file: u64,
    // The writer currently being written to (lazily created; None right after a rotation or
    // before any write).
    writer: Option<FlacWriter>,
    // Frames written to the current file (reset to 0 on rotation).
    frames_in_current: u64,
    // Number of files opened so far (used to determine the next sequence number).
    files_opened: u64,
    // Once finalized, subsequent write_chunk calls are rejected.
    finalized: bool,
}

impl FlacEncoder {
    /// Path of the next file to open. Without splitting it is the base path as-is; with
    /// splitting it is the 1-based numbered name.
    fn next_path(&self) -> PathBuf {
        if self.frames_per_file > 0 {
            split_file_path(&self.base, self.files_opened + 1)
        } else {
            self.base.clone()
        }
    }

    /// Opens the destination file if it is not open yet (lazy creation).
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

    /// Finalizes and closes the current file (does nothing if none is open).
    fn finalize_current(&mut self) -> PyResult<()> {
        if let Some(writer) = self.writer.take() {
            writer.finalize().map_err(encode_err_to_py)?;
        }
        Ok(())
    }
}

#[pymethods]
impl FlacEncoder {
    /// Creates an encoder that writes 16-bit FLAC to `path`. `channels` is 1..=2 and
    /// `sample_rate` is 1..=96000 Hz (out of range raises `ValueError`). `split_seconds>0`
    /// enables numbered split rotation.
    ///
    /// The file is not opened at this point (it is opened by the first `write_chunk`).
    #[new]
    #[pyo3(signature = (path, sample_rate, channels, split_seconds = 0))]
    fn new(path: PathBuf, sample_rate: u32, channels: u16, split_seconds: u64) -> PyResult<Self> {
        // Validate the same ranges as the underlying FlacWriter::create here, before creating
        // the file (creation is lazy, so invalid parameters leave no empty file behind).
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
            // split_seconds × sample_rate. Overflow is absorbed by saturation (it does not
            // happen with realistic values).
            frames_per_file: split_seconds.saturating_mul(u64::from(sample_rate)),
            writer: None,
            frames_in_current: 0,
            files_opened: 0,
            finalized: false,
        })
    }

    /// Appends interleaved f32 samples. The length must be a multiple of `channels` (otherwise
    /// `ValueError`). `samples` may be a list, array.array, or numpy array. Empty is a no-op.
    ///
    /// If the frame count reaches `split_seconds × sample_rate` or more after the write, the
    /// current file is finalized on the spot and rotated to the next numbered file.
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
            let writer = self.writer.as_mut().expect("writer was just opened above");
            // If the length is not a multiple of the channel count, FlacWriter returns
            // Unsupported (rejected here).
            writer.write_chunk(&samples).map_err(encode_err_to_py)?;
        }
        // The write_chunk above succeeded = the length is a multiple of channels. Accumulate
        // the frame count.
        let frames = (samples.len() / self.channels as usize) as u64;
        self.frames_in_current += frames;

        if self.frames_per_file > 0 && self.frames_in_current >= self.frames_per_file {
            // Finalize the current file and move to the next number (the next write_chunk
            // opens the new file).
            self.finalize_current()?;
            self.frames_in_current = 0;
        }
        Ok(())
    }

    /// Writes out the partial frames, finalizes the header, and closes the current file. Safe
    /// to call twice (the second and later calls are no-ops).
    fn finalize(&mut self) -> PyResult<()> {
        self.finalize_current()?;
        self.finalized = true;
        Ok(())
    }

    /// Context manager support. Usable as `with flexaudio.FlacEncoder(...) as enc:`.
    fn __enter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    /// Finalizes when leaving the `with` block. If no exception occurred inside the block, a
    /// finalize error is propagated too (while an exception is in flight it is best-effort, so
    /// as not to hide the original exception).
    fn __exit__(
        &mut self,
        exc_type: Option<Bound<'_, PyAny>>,
        _exc_value: Option<Bound<'_, PyAny>>,
        _traceback: Option<Bound<'_, PyAny>>,
    ) -> PyResult<bool> {
        let result = self.finalize_current();
        self.finalized = true;
        // If the block ended normally, propagate the finalize error. While an exception is in
        // flight, the original exception takes priority and the finalize failure is swallowed
        // (the underlying Drop no longer runs = already taken).
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
        // From the 1000th on, the number of digits grows naturally.
        assert_eq!(
            split_file_path(Path::new("rec.flac"), 1000),
            PathBuf::from("rec-1000.flac")
        );
        // The parent directory is preserved.
        assert_eq!(
            split_file_path(Path::new("/tmp/out/rec.flac"), 3),
            PathBuf::from("/tmp/out/rec-003.flac")
        );
        // Without an extension, the number is appended at the end.
        assert_eq!(
            split_file_path(Path::new("rec"), 5),
            PathBuf::from("rec-005")
        );
    }

    #[test]
    fn frames_per_file_reflects_split_seconds() {
        // split_seconds × sample_rate is the threshold. 0 means no splitting.
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
        // In-range values pass.
        assert!(FlacEncoder::new(PathBuf::from("x.flac"), 48_000, 2, 0).is_ok());
    }
}
