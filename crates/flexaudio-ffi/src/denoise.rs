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

use crate::convert::{valid_array, valid_pointer};
use crate::error::{clear_last_error, code, set_audio_error};
use crate::{guard_i32, guard_ptr};

/// Opaque noise suppression handle containing [`flexaudio_denoise::Denoiser`].
/// Create it with `flexaudio_denoise_new` and release it with `flexaudio_denoise_free`.
pub struct FlexDenoiser {
    pub(crate) inner: Denoiser,
    pending: bool,
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
            Ok(inner) => Box::into_raw(Box::new(FlexDenoiser {
                inner,
                pending: false,
            })),
            Err(e) => {
                set_audio_error(flexaudio::Error::InvalidArg(e.to_string()));
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
        if !valid_pointer(d) || !valid_array(samples, len) {
            return set_audio_error(flexaudio::Error::InvalidArg(
                "invalid denoiser handle or sample array".into(),
            ));
        }
        let dn = &mut *d;
        if len == 0 {
            // Empty input is a no-op and is safe even when NULL.
            return code::FLEX_OK;
        }
        let buf = slice::from_raw_parts_mut(samples, len);
        match dn.inner.process(buf) {
            Ok(()) => {
                dn.pending = true;
                code::FLEX_OK
            }
            Err(e) => {
                // A length that is not divisible by the channel count is an argument error.
                set_audio_error(flexaudio::Error::InvalidArg(e.to_string()));
                code::FLEX_INVALID_ARG
            }
        }
    })
}

/// Drain actual interleaved delayed samples into a library-owned array.
///
/// Valid output destinations initialize to NULL/0, including on errors. Repeated
/// flush without new nonempty input returns NULL/0. Release samples with
/// `flexaudio_denoise_samples_free`, never C free.
///
/// # Safety
/// `d` must be a live handle; `out`/`out_len` must be aligned writable destinations.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_denoise_flush(
    d: *mut FlexDenoiser,
    out: *mut *mut f32,
    out_len: *mut usize,
) -> i32 {
    guard_i32(|| {
        clear_last_error();
        if !valid_pointer(out) || !valid_pointer(out_len) {
            return set_audio_error(flexaudio::Error::InvalidArg(
                "invalid denoise output destination".into(),
            ));
        }
        out.write(std::ptr::null_mut());
        out_len.write(0);
        if !valid_pointer(d) {
            return set_audio_error(flexaudio::Error::InvalidArg(
                "invalid denoiser handle".into(),
            ));
        }
        let dn = &mut *d;
        if !dn.pending {
            return code::FLEX_OK;
        }
        let samples = dn.inner.flush();
        dn.pending = false;
        if !samples.is_empty() {
            let (pointer, len) = crate::chunk_storage::store(samples, 0);
            out_len.write(len);
            out.write(pointer);
        }
        code::FLEX_OK
    })
}

/// Release the sample array returned by `flexaudio_denoise_flush`. NULL/0 is safe.
///
/// # Safety
/// `samples`/`len` must be the exact live allocation returned by flush, or NULL/0.
/// Release each allocation once.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_denoise_samples_free(samples: *mut f32, len: usize) {
    guard_i32(|| {
        if samples.is_null() && len == 0 {
            return code::FLEX_OK;
        }
        if !valid_array(samples, len) || samples.is_null() || len == 0 {
            return set_audio_error(flexaudio::Error::InvalidArg(
                "invalid denoise sample array".into(),
            ));
        }
        if crate::chunk_storage::frame_index(samples, len).is_none() {
            return set_audio_error(flexaudio::Error::InvalidArg(
                "denoise samples are not a live library allocation with this length".into(),
            ));
        }
        crate::chunk_storage::release(samples, len);
        code::FLEX_OK
    });
}

/// Resets the RNN state, carry buffer, and delay line to their initial state.
///
/// # Safety
/// `d` must be a valid handle (NULL is InvalidArg).
#[no_mangle]
pub unsafe extern "C" fn flexaudio_denoise_reset(d: *mut FlexDenoiser) -> i32 {
    guard_i32(|| {
        clear_last_error();
        if !valid_pointer(d) {
            return set_audio_error(flexaudio::Error::InvalidArg(
                "invalid denoiser handle".into(),
            ));
        }
        let dn = &mut *d;
        dn.inner.reset();
        dn.pending = false;
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
            if !valid_pointer(d) {
                return set_audio_error(flexaudio::Error::InvalidArg(
                    "invalid denoiser handle".into(),
                ));
            }
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

#[cfg(test)]
mod boundary_tests {
    use super::*;
    use std::ptr;

    #[test]
    fn denoise_flush_validates_before_consuming_tail_and_initializes_errors() {
        unsafe {
            let handle = flexaudio_denoise_new(1);
            assert!(!handle.is_null());
            let mut out = ptr::dangling_mut();
            let mut len = 99;
            assert_eq!(
                flexaudio_denoise_flush(handle, &mut out, &mut len),
                code::FLEX_OK
            );
            assert!(out.is_null());
            assert_eq!(len, 0);
            let mut samples = [0.25; 961];
            assert_eq!(
                flexaudio_denoise_process(handle, samples.as_mut_ptr(), samples.len()),
                code::FLEX_OK
            );
            let unaligned_out = ptr::dangling_mut::<u8>().cast::<*mut f32>();
            assert_eq!(
                flexaudio_denoise_flush(handle, unaligned_out, &mut len),
                code::FLEX_INVALID_ARG
            );
            assert_eq!(
                flexaudio_denoise_flush(handle, ptr::null_mut(), &mut len),
                code::FLEX_INVALID_ARG
            );
            let unaligned_samples = ptr::dangling_mut::<u8>().cast::<f32>();
            assert_eq!(
                flexaudio_denoise_process(handle, unaligned_samples, 1),
                code::FLEX_INVALID_ARG
            );
            assert_eq!(
                flexaudio_denoise_process(handle, samples.as_mut_ptr(), usize::MAX),
                code::FLEX_INVALID_ARG
            );
            assert_eq!(
                flexaudio_denoise_flush(handle, &mut out, &mut len),
                code::FLEX_OK
            );
            assert!(!out.is_null());
            assert_eq!(len, 480);
            flexaudio_denoise_samples_free(out, len);
            out = ptr::dangling_mut();
            len = 99;
            assert_eq!(
                flexaudio_denoise_flush(ptr::null_mut(), &mut out, &mut len),
                code::FLEX_INVALID_ARG
            );
            assert!(out.is_null());
            assert_eq!(len, 0);
            assert_eq!(
                flexaudio_denoise_flush(ptr::dangling_mut::<u8>().cast(), &mut out, &mut len),
                code::FLEX_INVALID_ARG
            );
            flexaudio_denoise_samples_free(unaligned_samples, 1);
            assert_eq!(
                crate::error::last_audio_error().unwrap().kind(),
                flexaudio::ErrorKind::InvalidArg
            );
            flexaudio_denoise_samples_free(samples.as_mut_ptr(), usize::MAX);
            flexaudio_denoise_samples_free(ptr::null_mut(), 1);
            flexaudio_denoise_samples_free(ptr::null_mut(), 0);
            flexaudio_denoise_free(handle);
        }
    }
}
