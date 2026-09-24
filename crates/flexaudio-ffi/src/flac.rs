//! Standalone handle and C ABI for the streaming FLAC writer addon.
//!
//! [`FlexFlac`] is an opaque handle that streams recorded chunks (interleaved f32) to a file
//! while losslessly compressing them, driving a [`flexaudio_encode::FlacWriter`] internally.
//! When `split_seconds` is given, it rotates to numbered files (`name-001.flac`,
//! `name-002.flac`, …) each time the number of written frames reaches the threshold (same
//! convention as the CLI's `--split-seconds`).
//!
//! Conventions are the same as the rest of the crate (guards absorb panics, NULL checks,
//! failures go to last_error).

use std::ffi::CStr;
use std::os::raw::c_char;
use std::path::{Path, PathBuf};
use std::slice;

use flexaudio_encode::{EncodeError, FlacWriter};

use crate::error::{clear_last_error, code, set_last_error};
use crate::{guard_i32, guard_ptr};

/// Upper limit (Hz) of the sample rate the FLAC writer supports. Aligned with
/// [`flexaudio_encode::FlacWriter`], which limits it to 96kHz to match flacenc's validation
/// (rejected up front in create).
const MAX_SAMPLE_RATE: u32 = 96_000;

/// Builds the file path of the `index`-th (1-based) file of a split recording (pure function).
///
/// For `rec.flac`, a 3-digit zero-padded sequence number is inserted before the extension,
/// as in `rec-001.flac, rec-002.flac, …`. From 1000 on, the digits simply grow. A path without
/// an extension gets the number appended at the end. The parent directory is preserved (same
/// rules as the CLI's `split_file_path`).
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

/// FLAC writer with rotation. `split_seconds = 0` means a single file.
///
/// A file is not opened until the first chunk arrives (lazy creation), so no empty trailing
/// file is left even when recording ends exactly on a boundary (same as the CLI's
/// `RotatingWavWriter`). The boundary is "move to the next file once at or above" at chunk
/// granularity, so each file can be up to one chunk longer than the specified seconds.
struct RotatingFlac {
    /// Base path (the source of the numbered names when splitting; used as is when not
    /// splitting).
    base: PathBuf,
    sample_rate: u32,
    channels: u16,
    /// Frame count threshold per file (split_seconds × rate). 0 = no splitting.
    frames_per_file: u64,
    /// Writer currently being written to (created lazily; None right after a rotation or
    /// before any write).
    writer: Option<FlacWriter>,
    /// Frames written to the current file (reset to 0 on rotation).
    frames_in_current: u64,
    /// Sequence number (1-based) of the next split file to open.
    file_index: u64,
    /// Once finalized, subsequent writes are rejected.
    finalized: bool,
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
            finalized: false,
        }
    }

    /// File path to write to now (base when not splitting, the numbered path when splitting).
    fn current_path(&self) -> PathBuf {
        if self.frames_per_file == 0 {
            self.base.clone()
        } else {
            split_file_path(&self.base, self.file_index)
        }
    }

    /// Writes interleaved f32. The length must be a multiple of the channel count. When the
    /// boundary is reached, finalizes the current file and advances the sequence number.
    fn write(&mut self, samples: &[f32]) -> Result<(), EncodeError> {
        let ch = self.channels as usize;
        if samples.is_empty() {
            // Empty is a no-op (no file is opened = no empty file is created).
            return Ok(());
        }
        if !samples.len().is_multiple_of(ch) {
            return Err(EncodeError::Unsupported(format!(
                "chunk length {} is not a multiple of channels {ch}",
                samples.len()
            )));
        }

        // Lazy creation: the current file is opened for the first time on this chunk.
        if self.writer.is_none() {
            let path = self.current_path();
            self.writer = Some(FlacWriter::create(&path, self.sample_rate, self.channels)?);
        }
        // The writer has always been prepared just above.
        self.writer
            .as_mut()
            .expect("writer was created just above")
            .write_chunk(samples)?;

        self.frames_in_current += (samples.len() / ch) as u64;

        // When the threshold is reached, close the current file; the next chunk goes to the
        // next file.
        if self.frames_per_file > 0 && self.frames_in_current >= self.frames_per_file {
            if let Some(w) = self.writer.take() {
                w.finalize()?;
            }
            self.file_index += 1;
            self.frames_in_current = 0;
        }
        Ok(())
    }

    /// Writes out the remainder, then finalizes and closes the current file. No writes are
    /// allowed afterwards.
    fn finalize(&mut self) -> Result<(), EncodeError> {
        let result = match self.writer.take() {
            Some(w) => w.finalize(),
            None => Ok(()),
        };
        self.finalized = true;
        result
    }
}

/// Opaque handle for FLAC writing. Create it with `flexaudio_flac_create`, append chunks with
/// `flexaudio_flac_write`, finalize with `flexaudio_flac_finalize`, and free it with
/// `flexaudio_flac_free`.
pub struct FlexFlac {
    inner: RotatingFlac,
}

/// Maps an EncodeError to an error code (argument-related ones are InvalidArg, others are
/// Failure).
fn flac_err(e: EncodeError) -> i32 {
    let is_arg = matches!(e, EncodeError::Unsupported(_));
    set_last_error(e.to_string());
    if is_arg {
        code::FLEX_INVALID_ARG
    } else {
        code::FLEX_FAILURE
    }
}

/// Opens FLAC writing to `path`. `split_seconds = 0` means a single file; 1 or more rotates to
/// numbered `name-001.flac` files every `split_seconds` seconds.
///
/// On failure (NULL / invalid UTF-8 path / unsupported `sr` or `ch`) returns NULL and sets
/// last_error. `ch` is 1..=2 and `sr` is 1..=96000 Hz. Free the returned handle with
/// `flexaudio_flac_free` (even if it is freed without calling `flexaudio_flac_finalize`, it
/// is closed on a best-effort basis).
///
/// # Safety
/// `path` must point to a valid NUL-terminated C string (UTF-8) (NULL is treated as a
/// failure).
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
        // Reject early at create time (same range as FlacWriter::create; no file is created).
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
        let inner = RotatingFlac::new(path, sr, ch, split_seconds);
        Box::into_raw(Box::new(FlexFlac { inner }))
    })
}

/// Appends interleaved f32 (length = frames × channels).
///
/// `len` must be a multiple of the channel count (InvalidArg otherwise). `len=0` is a no-op.
/// A write to a finalized handle returns [`FLEX_INVALID_STATE`](code::FLEX_INVALID_STATE).
/// Returns 0 = success / negative = error.
///
/// # Safety
/// `f` must be a valid handle and `samples` must be a valid array of `len` elements (may be
/// NULL if `len=0`).
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
        if flac.inner.finalized {
            set_last_error("flexaudio_flac_write: writer is already finalized");
            return code::FLEX_INVALID_STATE;
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

/// Writes out the remainder, then finalizes and closes the current file. Subsequent writes
/// return InvalidState.
///
/// A double finalize is safe (a no-op that returns 0). Returns 0 = success / negative = error.
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
        if flac.inner.finalized {
            // Do nothing if already finalized (idempotent).
            return code::FLEX_OK;
        }
        match flac.inner.finalize() {
            Ok(()) => code::FLEX_OK,
            Err(e) => flac_err(e),
        }
    })
}

/// Frees a FLAC handle. NULL-safe.
///
/// Even when freed without finalize, the inner [`FlacWriter`] tries on drop, on a best-effort
/// basis, to write out the remainder and finalize the header (errors are swallowed; to detect
/// them reliably, call `flexaudio_flac_finalize` first).
///
/// # Safety
/// `f` must be a handle returned by `flexaudio_flac_create` (or NULL).
/// `f` must not be used after it is freed.
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

    // split_file_path follows the same numbering rules as the CLI (representative points are
    // pinned to prevent regressions).
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
        // split_seconds × rate = frame count threshold per file. 0 means no splitting.
        let r = RotatingFlac::new(PathBuf::from("x.flac"), 48_000, 2, 5);
        assert_eq!(r.frames_per_file, 5 * 48_000);
        let single = RotatingFlac::new(PathBuf::from("x.flac"), 48_000, 2, 0);
        assert_eq!(single.frames_per_file, 0);
        assert_eq!(single.current_path(), PathBuf::from("x.flac"));
    }

    /// Unique temporary path (process ID + label avoid collisions; always removed after the
    /// test).
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
        // A write after finalize is InvalidState.
        assert_eq!(
            unsafe { flexaudio_flac_write(f, samples.as_ptr(), samples.len()) },
            code::FLEX_INVALID_STATE
        );
        // A double finalize is idempotent.
        assert_eq!(unsafe { flexaudio_flac_finalize(f) }, code::FLEX_OK);
        unsafe { flexaudio_flac_free(f) };

        assert!(path.exists(), "the FLAC file should have been created");
        assert!(
            fs::metadata(&path).unwrap().len() > 0,
            "it should not be empty"
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

        // split_seconds=1 @48k mono → one file per 48000 frames.
        let f = unsafe { flexaudio_flac_create(cpath.as_ptr(), 48_000, 1, 1) };
        assert!(!f.is_null());
        // Write exactly enough to reach the threshold → file-001 is closed and the sequence
        // number advances.
        let block = vec![0.0f32; 48_000];
        assert_eq!(
            unsafe { flexaudio_flac_write(f, block.as_ptr(), block.len()) },
            code::FLEX_OK
        );
        // The next chunk opens file-002.
        let block2 = vec![0.0f32; 4096];
        assert_eq!(
            unsafe { flexaudio_flac_write(f, block2.as_ptr(), block2.len()) },
            code::FLEX_OK
        );
        assert_eq!(unsafe { flexaudio_flac_finalize(f) }, code::FLEX_OK);
        unsafe { flexaudio_flac_free(f) };

        assert!(
            f1.exists(),
            "the first file {f1:?} should have been created"
        );
        assert!(
            f2.exists(),
            "the second file {f2:?} should have been created"
        );
        let _ = fs::remove_file(&f1);
        let _ = fs::remove_file(&f2);
    }

    #[test]
    fn create_rejects_bad_params_and_null() {
        // NULL path.
        assert!(unsafe { flexaudio_flac_create(std::ptr::null(), 48_000, 1, 0) }.is_null());
        // Unsupported channels / sample rate.
        let p = CString::new("/tmp/does_not_matter.flac").unwrap();
        assert!(unsafe { flexaudio_flac_create(p.as_ptr(), 48_000, 3, 0) }.is_null());
        assert!(unsafe { flexaudio_flac_create(p.as_ptr(), 0, 1, 0) }.is_null());
        // Operations on a NULL handle are InvalidArg / free is safe.
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
