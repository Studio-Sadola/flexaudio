//! Independent handle and C ABI for the VAD (speech segment detection) addon.
//!
//! [`FlexVad`] wraps [`flexaudio_vad::Vad`] in an opaque handle and can be used independently of a
//! stream (feed it any f32 samples already available in any format). It is separate from the VAD
//! integrated into a stream (`FlexConfig::has_vad`).
//!
//! It follows the crate-wide conventions (guard against panics, check NULL, report failures via last_error).
//! The output event array is allocated with `into_boxed_slice` and freed with [`flexaudio_vad_events_free`].

use std::slice;

use flexaudio_vad::Vad;

use crate::convert::{vad_config_from_c, vad_events_to_c};
use crate::error::{clear_last_error, code, set_last_error};
use crate::types::{FlexVadConfig, FlexVadEvent};
use crate::{guard_i32, guard_ptr};

/// Opaque VAD handle containing [`flexaudio_vad::Vad`] (which holds one ONNX session).
/// Create it with `flexaudio_vad_new` and free it with `flexaudio_vad_free`.
pub struct FlexVad {
    pub(crate) inner: Vad,
}

/// Create a VAD from a config. A NULL `config` uses the defaults (Silero-compatible).
///
/// Returns NULL and sets last_error on failure (model load failure, invalid sample_rate, etc.).
/// Free the returned handle with `flexaudio_vad_free`.
///
/// # Safety
/// `config` must be NULL or point to a valid `FlexVadConfig`.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_vad_new(config: *const FlexVadConfig) -> *mut FlexVad {
    guard_ptr(|| {
        clear_last_error();
        // NULL means “all defaults.” Otherwise copy the values into VadConfig, including sentinels.
        let vad_config = match config.as_ref() {
            Some(c) => vad_config_from_c(c),
            None => Default::default(),
        };
        match Vad::new(vad_config) {
            Ok(inner) => Box::into_raw(Box::new(FlexVad { inner })),
            Err(e) => {
                set_last_error(e.to_string());
                std::ptr::null_mut()
            }
        }
    })
}

/// Process samples in any format (`in_rate` / `in_ch`, interleaved f32) through VAD,
/// allocate the confirmed event array, and set `out` / `out_len`.
///
/// Internally, convert to mono and resample to the VAD rate before processing ([`flexaudio_vad::Vad::process_pcm`]).
/// If there are no events, set `out=NULL` / `out_len=0`. Free the allocated array with
/// `flexaudio_vad_events_free`. Returns 0 on success or a negative value on error.
///
/// # Safety
/// `v` must be a valid handle; `samples` must be a valid array of `len` elements (NULL is allowed when `len=0`);
/// `out` / `out_len` must point to valid writable locations.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_vad_process(
    v: *mut FlexVad,
    samples: *const f32,
    len: usize,
    in_rate: u32,
    in_ch: u16,
    out: *mut *mut FlexVadEvent,
    out_len: *mut usize,
) -> i32 {
    guard_i32(|| {
        clear_last_error();
        let Some(vad) = v.as_mut() else {
            set_last_error("flexaudio_vad_process: vad pointer is null");
            return code::FLEX_INVALID_ARG;
        };
        if out.is_null() || out_len.is_null() {
            set_last_error("flexaudio_vad_process: output pointer is null");
            return code::FLEX_INVALID_ARG;
        }
        // Treat len=0 as an empty slice (safe even if samples is NULL).
        let input: &[f32] = if len == 0 {
            &[]
        } else if samples.is_null() {
            set_last_error("flexaudio_vad_process: samples pointer is null");
            return code::FLEX_INVALID_ARG;
        } else {
            slice::from_raw_parts(samples, len)
        };

        out.write(std::ptr::null_mut());
        out_len.write(0);
        let events = match vad.inner.process_pcm(input, in_rate, in_ch) {
            Ok(events) => events,
            Err(error) => {
                set_last_error(error.to_string());
                return match error {
                    flexaudio_vad::VadError::InvalidFormat(_)
                    | flexaudio_vad::VadError::InvalidConfig(_) => code::FLEX_INVALID_ARG,
                    _ => code::FLEX_FAILURE,
                };
            }
        };
        let (ptr, ev_len) = vad_events_to_c(events);
        out.write(ptr);
        out_len.write(ev_len);
        code::FLEX_OK
    })
}

/// Free an event array allocated by `flexaudio_vad_process`. NULL / 0 is safe.
///
/// # Safety
/// `events` / `len` must come from `flexaudio_vad_process` (or be NULL / 0).
#[no_mangle]
pub unsafe extern "C" fn flexaudio_vad_events_free(events: *mut FlexVadEvent, len: usize) {
    guard_i32(|| {
        crate::convert::free_vad_events(events, len);
        code::FLEX_OK
    });
}

/// Reset VAD state (internal state / context / remainder buffer / resampler).
///
/// # Safety
/// `v` must be a valid handle (NULL is InvalidArg).
#[no_mangle]
pub unsafe extern "C" fn flexaudio_vad_reset(v: *mut FlexVad) -> i32 {
    guard_i32(|| {
        clear_last_error();
        let Some(vad) = v.as_mut() else {
            set_last_error("flexaudio_vad_reset: vad pointer is null");
            return code::FLEX_INVALID_ARG;
        };
        if let Err(error) = vad.inner.reset() {
            set_last_error(error.to_string());
            return code::FLEX_FAILURE;
        }
        code::FLEX_OK
    })
}

/// Free a VAD handle. NULL is safe.
///
/// # Safety
/// `v` must be a handle returned by `flexaudio_vad_new` (or NULL).
/// Do not use `v` after freeing it.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_vad_free(v: *mut FlexVad) {
    guard_i32(|| {
        if !v.is_null() {
            drop(Box::from_raw(v));
        }
        code::FLEX_OK
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::FlexVadEvent;

    /// Run new → process (silence) → reset → free without panicking, and keep event-array
    /// allocation and deallocation consistent (smoke check for into_boxed_slice ownership).
    #[test]
    fn vad_new_process_free_smoke() {
        // NULL config = all defaults.
        let v = unsafe { flexaudio_vad_new(std::ptr::null()) };
        assert!(!v.is_null(), "vad_new failed: model load?");

        // Process 48k/stereo silence (no speech events are expected by default, so out=NULL).
        let samples = vec![0.0f32; 48_000 * 2];
        let mut out: *mut FlexVadEvent = std::ptr::null_mut();
        let mut out_len: usize = 123; // overwritten below
        let rc = unsafe {
            flexaudio_vad_process(
                v,
                samples.as_ptr(),
                samples.len(),
                48_000,
                2,
                &mut out,
                &mut out_len,
            )
        };
        assert_eq!(rc, code::FLEX_OK);
        // There are no events in silence (NULL / 0).
        assert!(out.is_null());
        assert_eq!(out_len, 0);
        unsafe { flexaudio_vad_events_free(out, out_len) };

        // Empty input is safe too.
        let rc2 = unsafe {
            flexaudio_vad_process(v, std::ptr::null(), 0, 48_000, 2, &mut out, &mut out_len)
        };
        assert_eq!(rc2, code::FLEX_OK);
        assert!(out.is_null());
        assert_eq!(out_len, 0);

        assert_eq!(unsafe { flexaudio_vad_reset(v) }, code::FLEX_OK);
        unsafe { flexaudio_vad_free(v) };
    }

    #[test]
    fn vad_invalid_format_returns_failure_detail() {
        let v = unsafe { flexaudio_vad_new(std::ptr::null()) };
        assert!(!v.is_null());
        let mut out = std::ptr::null_mut();
        let mut len = 123;
        for (rate, channels, reason) in [(16_000, 0, "channels"), (0, 1, "sample rate")] {
            let result = unsafe {
                flexaudio_vad_process(v, std::ptr::null(), 0, rate, channels, &mut out, &mut len)
            };
            assert_eq!(result, code::FLEX_INVALID_ARG);
            assert!(out.is_null());
            assert_eq!(len, 0);
            let message = unsafe { std::ffi::CStr::from_ptr(crate::error::last_error_ptr()) }
                .to_str()
                .unwrap();
            assert!(message.contains(reason));
        }
        unsafe { flexaudio_vad_free(v) };
    }

    /// NULL handle and NULL output pointer return InvalidArg (no panic).
    #[test]
    fn vad_null_args_are_invalid() {
        let mut out: *mut FlexVadEvent = std::ptr::null_mut();
        let mut out_len: usize = 0;
        let rc = unsafe {
            flexaudio_vad_process(
                std::ptr::null_mut(),
                std::ptr::null(),
                0,
                16_000,
                1,
                &mut out,
                &mut out_len,
            )
        };
        assert_eq!(rc, code::FLEX_INVALID_ARG);
        assert_eq!(
            unsafe { flexaudio_vad_reset(std::ptr::null_mut()) },
            code::FLEX_INVALID_ARG
        );
        // Freeing NULL is safe.
        unsafe { flexaudio_vad_free(std::ptr::null_mut()) };
    }
}
