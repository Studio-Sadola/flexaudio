//! Standalone handle and C ABI for the VAD (speech segment detection) addon.
//!
//! [`FlexVad`] is an opaque handle wrapping a [`flexaudio_vad::Vad`] and can be used
//! independently of a stream (f32 samples of any format already at hand can be fed in). It is
//! a separate path from the VAD built into a stream (`FlexConfig::has_vad`).
//!
//! Conventions are the same as the rest of the crate (guards absorb panics, NULL checks,
//! failures go to last_error). The output event array is allocated with `into_boxed_slice`
//! and freed with [`flexaudio_vad_events_free`].

use std::slice;

use flexaudio_vad::Vad;

use crate::convert::{vad_config_from_c, vad_events_to_c};
use crate::error::{clear_last_error, code, set_last_error};
use crate::types::{FlexVadConfig, FlexVadEvent};
use crate::{guard_i32, guard_ptr};

/// Opaque VAD handle. It contains a [`flexaudio_vad::Vad`] (which holds one ONNX session).
/// Create it with `flexaudio_vad_new` and free it with `flexaudio_vad_free`.
pub struct FlexVad {
    pub(crate) inner: Vad,
}

/// Builds a VAD from settings. If `config` is NULL, the default settings (silero-compliant)
/// are used.
///
/// On failure (model load failure, invalid sample_rate, etc.) returns NULL and sets
/// last_error. Free the returned handle with `flexaudio_vad_free`.
///
/// # Safety
/// `config` must be NULL or point to a valid `FlexVadConfig`.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_vad_new(config: *const FlexVadConfig) -> *mut FlexVad {
    guard_ptr(|| {
        clear_last_error();
        // NULL means "all defaults". Otherwise map to VadConfig, including sentinels.
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

/// Passes samples of any format (interleaved f32 at `in_rate` / `in_ch`) through the VAD,
/// allocates an array of the finalized events, and sets `out` / `out_len`.
///
/// Internally downmixes to mono and resamples to the VAD rate before processing
/// ([`flexaudio_vad::Vad::process_pcm`]). If there are no events, `out=NULL` / `out_len=0`.
/// Free the allocated array with `flexaudio_vad_events_free`. Returns 0 = success /
/// negative = error.
///
/// # Safety
/// `v` must be a valid handle, `samples` must be a valid array of `len` elements (may be NULL
/// if `len=0`), and `out` / `out_len` must be valid write targets.
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
        // len=0 is treated as an empty slice (safe even if samples is NULL).
        let input: &[f32] = if len == 0 {
            &[]
        } else if samples.is_null() {
            set_last_error("flexaudio_vad_process: samples pointer is null");
            return code::FLEX_INVALID_ARG;
        } else {
            slice::from_raw_parts(samples, len)
        };

        let events = vad.inner.process_pcm(input, in_rate, in_ch);
        let (ptr, ev_len) = vad_events_to_c(events);
        out.write(ptr);
        out_len.write(ev_len);
        code::FLEX_OK
    })
}

/// Frees the event array allocated by `flexaudio_vad_process`. NULL / 0 is safe.
///
/// # Safety
/// `events`/`len` must be what `flexaudio_vad_process` returned (or NULL/0).
#[no_mangle]
pub unsafe extern "C" fn flexaudio_vad_events_free(events: *mut FlexVadEvent, len: usize) {
    guard_i32(|| {
        crate::convert::free_vad_events(events, len);
        code::FLEX_OK
    });
}

/// Resets the VAD's state (internal state / context / remainder buffer / resampler).
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
        vad.inner.reset();
        code::FLEX_OK
    })
}

/// Frees a VAD handle. NULL-safe.
///
/// # Safety
/// `v` must be a handle returned by `flexaudio_vad_new` (or NULL).
/// `v` must not be used after it is freed.
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

    /// One full new → process (silence) → reset → free cycle does not panic, and allocation
    /// and freeing of the event array are consistent (smoke test for into_boxed_slice free
    /// consistency).
    #[test]
    fn vad_new_process_free_smoke() {
        // NULL config = all defaults.
        let v = unsafe { flexaudio_vad_new(std::ptr::null()) };
        assert!(!v.is_null(), "vad_new failed: model load?");

        // Feed 48k/stereo silence (by default no speech events are produced, so out=NULL is
        // expected).
        let samples = vec![0.0f32; 48_000 * 2];
        let mut out: *mut FlexVadEvent = std::ptr::null_mut();
        let mut out_len: usize = 123; // must get overwritten
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
        // Silence, so there are no events (NULL/0).
        assert!(out.is_null());
        assert_eq!(out_len, 0);
        unsafe { flexaudio_vad_events_free(out, out_len) };

        // Safe even with empty input.
        let rc2 = unsafe {
            flexaudio_vad_process(v, std::ptr::null(), 0, 48_000, 2, &mut out, &mut out_len)
        };
        assert_eq!(rc2, code::FLEX_OK);
        assert!(out.is_null());
        assert_eq!(out_len, 0);

        assert_eq!(unsafe { flexaudio_vad_reset(v) }, code::FLEX_OK);
        unsafe { flexaudio_vad_free(v) };
    }

    /// A NULL handle or NULL output target is InvalidArg (does not panic).
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
