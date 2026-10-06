//! Independent handle and C ABI for the RNNoise noise suppression add-on.
//!
//! [`FlexDenoiser`] is an opaque handle around [`flexaudio_denoise::Denoiser`] that can
//! be used independently of a stream to process interleaved f32 samples in place. This
//! is separate from stream-integrated denoising (`FlexConfig::denoise`).
//!
//! # 48 kHz requirement
//! RNNoise expects 48 kHz interleaved f32 samples normalized to ±1.0. It processes
//! fixed frames of 480 samples per channel. Other sample rates are accepted but will
//! not produce the intended suppression, so callers must pass 48 kHz. Output is
//! delayed by 480 samples per channel; the initial delay is silence.

use std::slice;

use flexaudio_denoise::Denoiser;

use crate::error::{clear_last_error, code, set_last_error};
use crate::{guard_i32, guard_ptr};

/// Opaque noise suppression handle containing [`flexaudio_denoise::Denoiser`].
/// Create it with `flexaudio_denoise_new` and release it with `flexaudio_denoise_free`.
pub struct FlexDenoiser {
    pub(crate) inner: Denoiser,
}

/// Creates a denoiser for the given channel count (1 = mono, 2 = interleaved stereo).
///
/// Returns NULL and sets last_error if `channels` is outside 1..=2. Release the returned
/// handle with `flexaudio_denoise_free`. See the module docs for the 48 kHz requirement.
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

/// Suppresses noise in interleaved f32 samples (48 kHz, normalized to ±1.0) **in place**.
///
/// `len` must be a multiple of the channel count (otherwise InvalidArg). `len=0` is a
/// no-op. Output is delayed by 480 samples per channel, so the beginning is silent.
/// Returns 0 on success and a negative value on error.
///
/// # Safety
/// `d` must be a valid handle. `samples` must point to a valid mutable array of `len`
/// elements; NULL is allowed when `len=0`.
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
            // Empty input is a no-op and is safe even when NULL.
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
                // A length that is not divisible by the channel count is an argument error.
                set_last_error(e.to_string());
                code::FLEX_INVALID_ARG
            }
        }
    })
}

/// Resets the RNN state, carry buffer, and delay line to their initial state.
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

/// Releases a denoiser handle. NULL is safe.
///
/// # Safety
/// `d` must be a handle returned by `flexaudio_denoise_new`, or NULL. Do not use `d`
/// after releasing it.
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

    /// Smoke test: new → process (silence) → reset → free completes without panicking.
    #[test]
    fn denoise_new_process_free_smoke() {
        let d = unsafe { &mut *flexaudio_denoise_new(1) };
        let dptr = d as *mut FlexDenoiser;
        let mut buf = vec![0.0f32; 960]; // Equivalent to 48 kHz / 20 ms / mono.
        let rc = unsafe { flexaudio_denoise_process(dptr, buf.as_mut_ptr(), buf.len()) };
        assert_eq!(rc, code::FLEX_OK);
        // The first 480 samples are silence from the processing delay (0.0).
        assert!(buf[..480].iter().all(|&x| x == 0.0));
        assert_eq!(unsafe { flexaudio_denoise_reset(dptr) }, code::FLEX_OK);
        unsafe { flexaudio_denoise_free(dptr) };
    }

    /// Invalid channel counts return NULL; NULL handles and mismatched lengths return InvalidArg.
    #[test]
    fn denoise_invalid_inputs() {
        // Three channels are unsupported, so return NULL.
        assert!(flexaudio_denoise_new(3).is_null());

        // A length that is not divisible by two channels returns InvalidArg.
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
