//! Standalone handle and C ABI for the noise suppression (RNNoise) addon.
//!
//! [`FlexDenoiser`] is an opaque handle wrapping a [`flexaudio_denoise::Denoiser`] and can be
//! used independently of a stream (it processes interleaved f32 at hand in place). It is a
//! separate path from the denoise built into a stream (`FlexConfig::denoise`).
//!
//! # Assumes 48kHz
//! RNNoise assumes interleaved f32 at 48kHz, normalized to ±1.0 (internally it is cut into
//! fixed frames of 480 samples/ch for processing). Other sample rates are not rejected, but the
//! suppression will not work as intended. The caller must pass 48kHz. The output is the input
//! delayed by 480 samples/ch, and that much at the start is silence (streaming latency).

use std::slice;

use flexaudio_denoise::Denoiser;

use crate::error::{clear_last_error, code, set_last_error};
use crate::{guard_i32, guard_ptr};

/// Opaque noise suppression handle. It contains a [`flexaudio_denoise::Denoiser`].
/// Create it with `flexaudio_denoise_new` and free it with `flexaudio_denoise_free`.
pub struct FlexDenoiser {
    pub(crate) inner: Denoiser,
}

/// Builds a denoiser for the given channel count (1 = mono / 2 = stereo interleaved).
///
/// If `channels` is not in 1..=2, returns NULL and sets last_error. Free the returned handle
/// with `flexaudio_denoise_free`. See the module docs for the 48kHz assumption.
#[no_mangle]
pub extern "C" fn flexaudio_denoise_new(channels: u16) -> *mut FlexDenoiser {
    guard_ptr(|| {
        clear_last_error();
        match Denoiser::new(channels) {
            Ok(inner) => Box::into_raw(Box::new(FlexDenoiser { inner })),
            Err(e) => {
                set_last_error(e.to_string());
                std::ptr::null_mut()
            }
        }
    })
}

/// Applies noise suppression **in place** to interleaved f32 (48kHz, normalized to ±1.0).
///
/// `len` must be a multiple of the channel count (InvalidArg otherwise). `len=0` is a no-op.
/// The output is the input delayed by 480 samples/ch, and that much at the start of the
/// stream is silence. Returns 0 = success / negative = error.
///
/// # Safety
/// `d` must be a valid handle and `samples` must be a valid mutable array of `len` elements
/// (may be NULL if `len=0`).
#[no_mangle]
pub unsafe extern "C" fn flexaudio_denoise_process(
    d: *mut FlexDenoiser,
    samples: *mut f32,
    len: usize,
) -> i32 {
    guard_i32(|| {
        clear_last_error();
        let Some(dn) = d.as_mut() else {
            set_last_error("flexaudio_denoise_process: denoiser pointer is null");
            return code::FLEX_INVALID_ARG;
        };
        if len == 0 {
            // Empty is a no-op (safe even if NULL).
            return code::FLEX_OK;
        }
        if samples.is_null() {
            set_last_error("flexaudio_denoise_process: samples pointer is null");
            return code::FLEX_INVALID_ARG;
        }
        let buf = slice::from_raw_parts_mut(samples, len);
        match dn.inner.process(buf) {
            Ok(()) => code::FLEX_OK,
            Err(e) => {
                // Cases such as a length that is not a multiple of the channel count are
                // treated as argument problems.
                set_last_error(e.to_string());
                code::FLEX_INVALID_ARG
            }
        }
    })
}

/// Resets the RNN state, carry-over buffer, and delay line (back to the state right after
/// creation).
///
/// # Safety
/// `d` must be a valid handle (NULL is InvalidArg).
#[no_mangle]
pub unsafe extern "C" fn flexaudio_denoise_reset(d: *mut FlexDenoiser) -> i32 {
    guard_i32(|| {
        clear_last_error();
        let Some(dn) = d.as_mut() else {
            set_last_error("flexaudio_denoise_reset: denoiser pointer is null");
            return code::FLEX_INVALID_ARG;
        };
        dn.inner.reset();
        code::FLEX_OK
    })
}

/// Frees a denoiser handle. NULL-safe.
///
/// # Safety
/// `d` must be a handle returned by `flexaudio_denoise_new` (or NULL).
/// `d` must not be used after it is freed.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_denoise_free(d: *mut FlexDenoiser) {
    guard_i32(|| {
        if !d.is_null() {
            drop(Box::from_raw(d));
        }
        code::FLEX_OK
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test that one full new → process (silence) → reset → free cycle does not panic.
    #[test]
    fn denoise_new_process_free_smoke() {
        let d = unsafe { &mut *flexaudio_denoise_new(1) };
        let dptr = d as *mut FlexDenoiser;
        let mut buf = vec![0.0f32; 960]; // equivalent to 48k/20ms/mono
        let rc = unsafe { flexaudio_denoise_process(dptr, buf.as_mut_ptr(), buf.len()) };
        assert_eq!(rc, code::FLEX_OK);
        // The first 480 samples are latency silence (0.0).
        assert!(buf[..480].iter().all(|&x| x == 0.0));
        assert_eq!(unsafe { flexaudio_denoise_reset(dptr) }, code::FLEX_OK);
        unsafe { flexaudio_denoise_free(dptr) };
    }

    /// An invalid channel count gives NULL; a NULL handle / length mismatch gives InvalidArg.
    #[test]
    fn denoise_invalid_inputs() {
        // channels=3 is unsupported → NULL.
        assert!(flexaudio_denoise_new(3).is_null());

        // Length is not a multiple of 2ch → InvalidArg.
        let d = flexaudio_denoise_new(2);
        let mut buf = vec![0.0f32; 3];
        let rc = unsafe { flexaudio_denoise_process(d, buf.as_mut_ptr(), buf.len()) };
        assert_eq!(rc, code::FLEX_INVALID_ARG);
        unsafe { flexaudio_denoise_free(d) };

        // NULL handle.
        assert_eq!(
            unsafe { flexaudio_denoise_process(std::ptr::null_mut(), std::ptr::null_mut(), 0) },
            code::FLEX_INVALID_ARG
        );
        assert_eq!(
            unsafe { flexaudio_denoise_reset(std::ptr::null_mut()) },
            code::FLEX_INVALID_ARG
        );
        unsafe { flexaudio_denoise_free(std::ptr::null_mut()) };
    }
}
