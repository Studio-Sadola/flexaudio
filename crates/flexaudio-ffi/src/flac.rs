//! Standalone handle and C ABI for the streaming FLAC writer addon.
//!
//! [`FlexFlac`] is an opaque handle that streams recording chunks (interleaved f32) to files while
//! compressing them losslessly. It drives [`flexaudio_encode::FlacWriter`] internally. When
//! `split_seconds` is set, it rotates to numbered files (`name-001.flac`,
//! `name-002.flac`, …) each time the written frame count reaches the threshold, matching the CLI's `--split-seconds` behavior.
//!
//! It follows the crate-wide conventions: guards catch panics, NULL is checked, and failures set last_error.

use std::ffi::CStr;
use std::os::raw::c_char;
use std::path::{Path, PathBuf};
use std::slice;

use flexaudio_encode::{EncodeError, FlacWriter};

use crate::error::{clear_last_error, code, set_last_error};
use crate::{guard_i32, guard_ptr};

/// Maximum sample rate supported by the FLAC writer (Hz). Match the limit imposed by
/// [`flexaudio_encode::FlacWriter`] (96 kHz for flacenc validation; reject it early in create).
const MAX_SAMPLE_RATE: u32 = 96_000;

/// Create the path for the `index`th split recording file (1-based; pure function).
///
/// For `rec.flac`, insert a zero-padded three-digit sequence before the extension, as in
/// `rec-001.flac, rec-002.flac, …`. The number grows naturally beyond 999. If there is no extension,
/// append the number to the path. Preserve the parent directory (same rule as CLI `split_file_path`).
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

/// Terminal state is retained even when rotation consumed the current writer.
enum EncoderState {
    Open,
    Finalized,
    Failed(StoredFailure),
}

enum StoredFailure {
    Io {
        kind: std::io::ErrorKind,
        message: String,
    },
    Unsupported(String),
    Encoder(String),
}

impl StoredFailure {
    fn capture(error: &EncodeError) -> Self {
        match error {
            EncodeError::Io(error) => Self::Io {
                message: error.to_string(),
                kind: error.kind(),
            },
            EncodeError::Unsupported(message) => Self::Unsupported(message.clone()),
            EncodeError::Encoder(message) => Self::Encoder(message.clone()),
            _ => Self::Encoder(error.to_string()),
        }
    }

    fn error(&self) -> EncodeError {
        match self {
            Self::Io { kind, message } => {
                EncodeError::Io(std::io::Error::new(*kind, message.clone()))
            }
            Self::Unsupported(message) => EncodeError::Unsupported(message.clone()),
            Self::Encoder(message) => EncodeError::Encoder(message.clone()),
        }
    }
}

fn validate_destination(base: &Path) -> Result<(), EncodeError> {
    if base.is_dir() || base.file_stem().is_none_or(|stem| stem.is_empty()) {
        return Err(EncodeError::Unsupported(
            "destination must have a nonempty file stem and must not be a directory".into(),
        ));
    }
    Ok(())
}

/// FLAC writer with file rotation. `split_seconds = 0` writes a single file.
///
/// Files are opened lazily on the first chunk, so finishing exactly at a boundary does not leave
/// an empty trailing file (same as CLI `RotatingWavWriter`). Boundaries are checked per chunk:
/// rotate when the threshold is reached or exceeded, so a file may run up to one chunk longer than requested.
struct RotatingFlac {
    /// Base path (used to form numbered paths when splitting, otherwise used as-is).
    base: PathBuf,
    sample_rate: u32,
    channels: u16,
    /// Frame threshold per file (`split_seconds × rate`). 0 = no splitting.
    frames_per_file: u64,
    /// Current writer (created lazily; None after rotation or before writing).
    writer: Option<FlacWriter>,
    /// Frames written to the current file (reset to 0 on rotation).
    frames_in_current: u64,
    /// Sequence number for the next split file to open (1-based).
    file_index: u64,
    /// Retain the first failure and reject writes after finalize.
    state: EncoderState,
}

impl RotatingFlac {
    fn new(base: PathBuf, sample_rate: u32, channels: u16, split_seconds: u32) -> RotatingFlac {
        RotatingFlac {
            base,
            sample_rate,
            channels,
            frames_per_file: u64::from(split_seconds) * u64::from(sample_rate),
            writer: None,
            frames_in_current: 0,
            file_index: 1,
            state: EncoderState::Open,
        }
    }

    /// Path for the next write (base path when not splitting, numbered path when splitting).
    fn current_path(&self) -> PathBuf {
        if self.frames_per_file == 0 {
            self.base.clone()
        } else {
            split_file_path(&self.base, self.file_index)
        }
    }

    /// Write interleaved f32. Length must be a multiple of the channel count. On reaching the
    /// boundary, finalize the current file and advance the sequence number.
    fn write(&mut self, samples: &[f32]) -> Result<(), EncodeError> {
        match &self.state {
            EncoderState::Failed(error) => return Err(error.error()),
            EncoderState::Finalized => {
                return Err(EncodeError::Unsupported(
                    "writer is already finalized".into(),
                ))
            }
            EncoderState::Open => {}
        }
        let ch = self.channels as usize;
        if samples.is_empty() {
            // Empty input is a no-op (do not open or create an empty file).
            return Ok(());
        }
        if !samples.len().is_multiple_of(ch) {
            return Err(EncodeError::Unsupported(format!(
                "chunk length {} is not a multiple of channels {ch}",
                samples.len()
            )));
        }

        let result = self.write_validated(samples);
        if let Err(error) = &result {
            self.state = EncoderState::Failed(StoredFailure::capture(error));
        }
        result
    }

    fn write_validated(&mut self, samples: &[f32]) -> Result<(), EncodeError> {
        let ch = usize::from(self.channels);
        // Lazy creation: open the current file for the first time with this chunk.
        if self.writer.is_none() {
            let path = self.current_path();
            self.writer = Some(FlacWriter::create(&path, self.sample_rate, self.channels)?);
        }
        // The writer was initialized above.
        self.writer
            .as_mut()
            .expect("writer was initialized immediately above")
            .write_chunk(samples)?;

        self.frames_in_current += (samples.len() / ch) as u64;

        // Once the threshold is reached, close the current file and use the next file for the next chunk.
        if self.frames_per_file > 0 && self.frames_in_current >= self.frames_per_file {
            if let Some(w) = self.writer.take() {
                w.finalize()?;
            }
            self.file_index += 1;
            self.frames_in_current = 0;
        }
        Ok(())
    }

    /// Write any remaining data, finalize and close the current file. Further writes are rejected.
    fn finalize(&mut self) -> Result<(), EncodeError> {
        match &self.state {
            EncoderState::Failed(error) => return Err(error.error()),
            EncoderState::Finalized => return Ok(()),
            EncoderState::Open => {}
        }
        let result = match self.writer.take() {
            Some(w) => w.finalize(),
            None => Ok(()),
        };
        self.state = match &result {
            Ok(()) => EncoderState::Finalized,
            Err(error) => EncoderState::Failed(StoredFailure::capture(error)),
        };
        result
    }
}

/// Opaque handle for FLAC output. Create it with `flexaudio_flac_create`, append chunks with
/// `flexaudio_flac_write`, finalize with `flexaudio_flac_finalize`, and release with `flexaudio_flac_free`.
pub struct FlexFlac {
    inner: RotatingFlac,
}

/// Map EncodeError to an error code (argument errors become InvalidArg; all others become Failure).
fn flac_err(e: EncodeError) -> i32 {
    let is_arg = matches!(e, EncodeError::Unsupported(_));
    set_last_error(e.to_string());
    if is_arg {
        code::FLEX_INVALID_ARG
    } else {
        code::FLEX_FAILURE
    }
}

/// Open FLAC output at `path`. `split_seconds = 0` creates one file; values of 1 or more rotate to
/// numbered files such as `name-001.flac` every `split_seconds` seconds.
///
/// On failure (NULL, invalid UTF-8 path, unsupported `sr` or `ch`), return NULL and set last_error.
/// `ch` must be 1..=2 and `sr` must be 1..=96000 Hz. Release the returned handle with
/// `flexaudio_flac_free` (free without `flexaudio_flac_finalize` still attempts a best-effort close).
///
/// # Safety
/// `path` must point to a valid NUL-terminated UTF-8 C string (NULL is treated as failure).
#[no_mangle]
pub unsafe extern "C" fn flexaudio_flac_create(
    path: *const c_char,
    sr: u32,
    ch: u16,
    split_seconds: u32,
) -> *mut FlexFlac {
    guard_ptr(|| {
        clear_last_error();
        if path.is_null() {
            set_last_error("flexaudio_flac_create: path pointer is null");
            return std::ptr::null_mut();
        }
        let path = match CStr::from_ptr(path).to_str() {
            Ok(s) => PathBuf::from(s),
            Err(_) => {
                set_last_error("flexaudio_flac_create: path is not valid UTF-8");
                return std::ptr::null_mut();
            }
        };
        // Reject invalid values early, matching FlacWriter::create; no file is created.
        if !(1..=2).contains(&ch) {
            set_last_error(format!(
                "flexaudio_flac_create: channels must be 1 or 2, got {ch}"
            ));
            return std::ptr::null_mut();
        }
        if !(1..=MAX_SAMPLE_RATE).contains(&sr) {
            set_last_error(format!(
                "flexaudio_flac_create: sample rate must be 1..={MAX_SAMPLE_RATE} Hz, got {sr}"
            ));
            return std::ptr::null_mut();
        }
        if let Err(error) = validate_destination(&path) {
            set_last_error(error.to_string());
            return std::ptr::null_mut();
        }
        let inner = RotatingFlac::new(path, sr, ch, split_seconds);
        Box::into_raw(Box::new(FlexFlac { inner }))
    })
}

/// Append interleaved f32 (length = frame count × channel count).
///
/// `len` must be a multiple of the channel count (otherwise InvalidArg). `len=0` is a no-op.
/// Writing to a finalized handle returns [`FLEX_INVALID_STATE`](code::FLEX_INVALID_STATE).
/// Returns 0 on success and a negative value on error.
///
/// # Safety
/// `f` must be a valid handle and `samples` a valid array of `len` elements (NULL is allowed when `len=0`).
#[no_mangle]
pub unsafe extern "C" fn flexaudio_flac_write(
    f: *mut FlexFlac,
    samples: *const f32,
    len: usize,
) -> i32 {
    guard_i32(|| {
        clear_last_error();
        let Some(flac) = f.as_mut() else {
            set_last_error("flexaudio_flac_write: flac pointer is null");
            return code::FLEX_INVALID_ARG;
        };
        if matches!(flac.inner.state, EncoderState::Finalized) {
            set_last_error("flexaudio_flac_write: writer is already finalized");
            return code::FLEX_INVALID_STATE;
        }
        if let EncoderState::Failed(error) = &flac.inner.state {
            return flac_err(error.error());
        }
        let input: &[f32] = if len == 0 {
            &[]
        } else if samples.is_null() {
            set_last_error("flexaudio_flac_write: samples pointer is null");
            return code::FLEX_INVALID_ARG;
        } else {
            slice::from_raw_parts(samples, len)
        };
        match flac.inner.write(input) {
            Ok(()) => code::FLEX_OK,
            Err(e) => flac_err(e),
        }
    })
}

/// Write any remaining data, finalize and close the current file. Further writes return InvalidState.
///
/// Calling finalize more than once after success is safe (no-op returning 0). A failed write or
/// finalize retains its first error for all later writes and finalization attempts.
/// Returns 0 on success and a negative value on error.
///
/// # Safety
/// `f` must be a valid handle (NULL is InvalidArg).
#[no_mangle]
pub unsafe extern "C" fn flexaudio_flac_finalize(f: *mut FlexFlac) -> i32 {
    guard_i32(|| {
        clear_last_error();
        let Some(flac) = f.as_mut() else {
            set_last_error("flexaudio_flac_finalize: flac pointer is null");
            return code::FLEX_INVALID_ARG;
        };
        if matches!(flac.inner.state, EncoderState::Finalized) {
            // Already finalized; do nothing (idempotent).
            return code::FLEX_OK;
        }
        match flac.inner.finalize() {
            Ok(()) => code::FLEX_OK,
            Err(e) => flac_err(e),
        }
    })
}

/// Release a FLAC handle. NULL-safe.
///
/// If freed without finalize, the internal [`FlacWriter`] still makes a best-effort attempt to
/// write remaining data and finalize the header on drop (errors are swallowed; call
/// `flexaudio_flac_finalize` first to detect them reliably).
///
/// # Safety
/// `f` must be a handle returned by `flexaudio_flac_create` (or NULL).
/// Do not use `f` after release.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_flac_free(f: *mut FlexFlac) {
    guard_i32(|| {
        if !f.is_null() {
            drop(Box::from_raw(f));
        }
        code::FLEX_OK
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;
    use std::fs;

    #[test]
    fn invalid_lazy_chunk_preserves_existing_destination() {
        let path = temp_path("invalid_lazy");
        fs::write(&path, b"existing data").unwrap();
        let mut writer = RotatingFlac::new(path.clone(), 48_000, 2, 0);
        assert!(writer.write(&[0.0]).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"existing data");
        writer.finalize().unwrap();
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn rotation_open_failure_is_sticky_even_after_path_is_repaired() {
        let base = temp_path("sticky_rotation");
        let first = split_file_path(&base, 1);
        let second = split_file_path(&base, 2);
        let mut writer = RotatingFlac::new(base, 48_000, 1, 1);
        writer.write(&vec![0.0; 48_000]).unwrap();
        fs::create_dir(&second).unwrap();
        let cause = writer.write(&[0.0]).unwrap_err().to_string();
        fs::remove_dir(&second).unwrap();
        assert_eq!(writer.write(&[0.0]).unwrap_err().to_string(), cause);
        assert_eq!(writer.finalize().unwrap_err().to_string(), cause);
        assert!(!second.exists());
        fs::remove_file(first).unwrap();
    }

    #[test]
    fn destination_rejects_directories_and_missing_stems() {
        for path in [
            Path::new(""),
            Path::new("."),
            Path::new(".."),
            Path::new("/"),
        ] {
            assert!(validate_destination(path).is_err());
        }
        assert!(validate_destination(&std::env::temp_dir()).is_err());
        assert!(validate_destination(Path::new("recording.flac")).is_ok());
    }

    // split_file_path follows the CLI numbering rule (pin representative cases to prevent regressions).
    #[test]
    fn split_file_path_inserts_padded_index() {
        assert_eq!(
            split_file_path(Path::new("rec.flac"), 1),
            PathBuf::from("rec-001.flac")
        );
        assert_eq!(
            split_file_path(Path::new("rec.flac"), 42),
            PathBuf::from("rec-042.flac")
        );
        assert_eq!(
            split_file_path(Path::new("/tmp/dir/rec.flac"), 3),
            PathBuf::from("/tmp/dir/rec-003.flac")
        );
        // No extension.
        assert_eq!(
            split_file_path(Path::new("rec"), 2),
            PathBuf::from("rec-002")
        );
    }

    #[test]
    fn rotating_frames_per_file_reflects_split_seconds() {
        // split_seconds × rate = frame threshold per file. 0 means no splitting.
        let r = RotatingFlac::new(PathBuf::from("x.flac"), 48_000, 2, 5);
        assert_eq!(r.frames_per_file, 5 * 48_000);
        let single = RotatingFlac::new(PathBuf::from("x.flac"), 48_000, 2, 0);
        assert_eq!(single.frames_per_file, 0);
        assert_eq!(single.current_path(), PathBuf::from("x.flac"));
    }

    /// Unique temporary path (process ID + label avoids collisions; always remove it after the test).
    fn temp_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("flexffi_{}_{}.flac", std::process::id(), label))
    }

    #[test]
    fn create_write_finalize_and_write_after_finalize_is_invalid_state() {
        let path = temp_path("single");
        let _ = fs::remove_file(&path);
        let cpath = CString::new(path.to_str().unwrap()).unwrap();

        let f = unsafe { flexaudio_flac_create(cpath.as_ptr(), 48_000, 1, 0) };
        assert!(!f.is_null());

        // Write one block (4096 mono frames).
        let samples = vec![0.0f32; 4096];
        assert_eq!(
            unsafe { flexaudio_flac_write(f, samples.as_ptr(), samples.len()) },
            code::FLEX_OK
        );
        assert_eq!(unsafe { flexaudio_flac_finalize(f) }, code::FLEX_OK);
        // Writes after finalize return InvalidState.
        assert_eq!(
            unsafe { flexaudio_flac_write(f, samples.as_ptr(), samples.len()) },
            code::FLEX_INVALID_STATE
        );
        // A second finalize is idempotent.
        assert_eq!(unsafe { flexaudio_flac_finalize(f) }, code::FLEX_OK);
        unsafe { flexaudio_flac_free(f) };

        assert!(path.exists(), "FLAC file should have been created");
        assert!(
            fs::metadata(&path).unwrap().len() > 0,
            "file should not be empty"
        );
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn split_rotates_to_numbered_files() {
        let base = temp_path("split");
        let f1 = split_file_path(&base, 1);
        let f2 = split_file_path(&base, 2);
        let _ = fs::remove_file(&f1);
        let _ = fs::remove_file(&f2);
        let cpath = CString::new(base.to_str().unwrap()).unwrap();

        // split_seconds=1 at 48 kHz mono → 48000 frames per file.
        let f = unsafe { flexaudio_flac_create(cpath.as_ptr(), 48_000, 1, 1) };
        assert!(!f.is_null());
        // Write exactly to the threshold → file-001 closes and the sequence advances.
        let block = vec![0.0f32; 48_000];
        assert_eq!(
            unsafe { flexaudio_flac_write(f, block.as_ptr(), block.len()) },
            code::FLEX_OK
        );
        // Open file-002 on the next chunk.
        let block2 = vec![0.0f32; 4096];
        assert_eq!(
            unsafe { flexaudio_flac_write(f, block2.as_ptr(), block2.len()) },
            code::FLEX_OK
        );
        assert_eq!(unsafe { flexaudio_flac_finalize(f) }, code::FLEX_OK);
        unsafe { flexaudio_flac_free(f) };

        assert!(f1.exists(), "first file {f1:?} should have been created");
        assert!(f2.exists(), "second file {f2:?} should have been created");
        let _ = fs::remove_file(&f1);
        let _ = fs::remove_file(&f2);
    }

    #[test]
    fn create_rejects_bad_params_and_null() {
        // NULL path.
        assert!(unsafe { flexaudio_flac_create(std::ptr::null(), 48_000, 1, 0) }.is_null());
        // Unsupported channel count / sample rate.
        let p = CString::new("/tmp/does_not_matter.flac").unwrap();
        assert!(unsafe { flexaudio_flac_create(p.as_ptr(), 48_000, 3, 0) }.is_null());
        assert!(unsafe { flexaudio_flac_create(p.as_ptr(), 0, 1, 0) }.is_null());
        // Operations on a NULL handle return InvalidArg; free is safe.
        assert_eq!(
            unsafe { flexaudio_flac_write(std::ptr::null_mut(), std::ptr::null(), 0) },
            code::FLEX_INVALID_ARG
        );
        assert_eq!(
            unsafe { flexaudio_flac_finalize(std::ptr::null_mut()) },
            code::FLEX_INVALID_ARG
        );
        unsafe { flexaudio_flac_free(std::ptr::null_mut()) };
    }
}
