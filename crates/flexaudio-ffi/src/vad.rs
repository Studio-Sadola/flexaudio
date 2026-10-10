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
use crate::convert::{valid_array, valid_pointer};
use crate::error::{clear_last_error, code, set_audio_error};
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
        if !config.is_null() && !valid_pointer(config) {
            set_audio_error(flexaudio::Error::InvalidArg(
                "invalid VAD config pointer".into(),
            ));
            return std::ptr::null_mut();
        }
        // NULL means “all defaults.” Otherwise copy the values into VadConfig, including sentinels.
        let vad_config = match config.as_ref() {
            Some(c) => vad_config_from_c(c),
            None => Default::default(),
        };
        match Vad::new(vad_config) {
            Ok(inner) => Box::into_raw(Box::new(FlexVad { inner })),
            Err(e) => {
                vad_failure(e);
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
        if !valid_pointer(out) || !valid_pointer(out_len) {
            return set_audio_error(flexaudio::Error::InvalidArg(
                "invalid VAD output destination".into(),
            ));
        }
        out.write(std::ptr::null_mut());
        out_len.write(0);
        if !valid_pointer(v) || !valid_array(samples, len) {
            return set_audio_error(flexaudio::Error::InvalidArg(
                "invalid VAD handle or sample array".into(),
            ));
        }
        let vad = &mut *v;
        let input = if len == 0 {
            &[]
        } else {
            slice::from_raw_parts(samples, len)
        };
        let events = match vad.inner.process_pcm(input, in_rate, in_ch) {
            Ok(events) => events,
            Err(error) => return vad_failure(error),
        };
        let (ptr, ev_len) = vad_events_to_c(events);
        out.write(ptr);
        out_len.write(ev_len);
        code::FLEX_OK
    })
}

/// Finalize pending speech at EOF and return a library-owned VAD event array.
///
/// Valid destinations initialize to NULL/0, also on error. A repeated successful
/// flush returns NULL/0 unless new input arrived. Free with `flexaudio_vad_events_free`.
///
/// # Safety
/// `v` must be a live handle; `out`/`out_len` must be aligned writable destinations.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_vad_flush(
    v: *mut FlexVad,
    out: *mut *mut FlexVadEvent,
    out_len: *mut usize,
) -> i32 {
    guard_i32(|| {
        clear_last_error();
        if !valid_pointer(out) || !valid_pointer(out_len) {
            return set_audio_error(flexaudio::Error::InvalidArg(
                "invalid VAD output destination".into(),
            ));
        }
        out.write(std::ptr::null_mut());
        out_len.write(0);
        if !valid_pointer(v) {
            return set_audio_error(flexaudio::Error::InvalidArg("invalid VAD handle".into()));
        }
        let events = match (*v).inner.flush() {
            Ok(events) => events,
            Err(error) => return vad_failure(error),
        };
        let (events, len) = vad_events_to_c(events);
        out.write(events);
        out_len.write(len);
        code::FLEX_OK
    })
}

/// Map addon failures by their typed variant; raw inference diagnostics stay private.
pub(crate) fn vad_failure(error: flexaudio_vad::VadError) -> i32 {
    use flexaudio_vad::VadError;
    let error = match error {
        VadError::InvalidFormat(message) | VadError::InvalidConfig(message) => {
            flexaudio::Error::InvalidArg(message)
        }
        VadError::ModelLoad(_) => flexaudio::Error::Backend("VAD model load failed".into()),
        VadError::Inference(_) => flexaudio::Error::Backend("VAD inference failed".into()),
        VadError::Resample(_) => flexaudio::Error::Backend("VAD resampling failed".into()),
        VadError::Reset(_) => flexaudio::Error::Backend("VAD reset failed".into()),
    };
    set_audio_error(error)
}

/// Free an event array allocated by `flexaudio_vad_process` or `flexaudio_vad_flush`. NULL / 0 is safe.
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
        if !valid_pointer(v) {
            return set_audio_error(flexaudio::Error::InvalidArg("invalid VAD handle".into()));
        }
        if let Err(error) = (*v).inner.reset() {
            return vad_failure(error);
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
            if !valid_pointer(v) {
                return set_audio_error(flexaudio::Error::InvalidArg("invalid VAD handle".into()));
            }
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

#[cfg(test)]
mod boundary_tests {
    use super::*;
    use std::ptr;

    #[test]
    fn vad_flush_and_process_validate_output_handle_and_slice() {
        unsafe {
            let mut out = ptr::dangling_mut();
            let mut len = 99;
            assert_eq!(
                flexaudio_vad_flush(ptr::null_mut(), &mut out, &mut len),
                code::FLEX_INVALID_ARG
            );
            assert!(out.is_null());
            assert_eq!(len, 0);
            assert_eq!(
                flexaudio_vad_flush(ptr::null_mut(), ptr::dangling_mut::<u8>().cast(), &mut len),
                code::FLEX_INVALID_ARG
            );
            assert_eq!(
                flexaudio_vad_flush(ptr::dangling_mut::<u8>().cast(), &mut out, &mut len),
                code::FLEX_INVALID_ARG
            );
            let handle = flexaudio_vad_new(ptr::null());
            assert!(!handle.is_null());
            for (samples, sample_count) in [
                (ptr::dangling::<u8>().cast::<f32>(), 1),
                (ptr::dangling::<f32>(), usize::MAX),
                (ptr::null(), 1),
            ] {
                out = ptr::dangling_mut();
                len = 99;
                assert_eq!(
                    flexaudio_vad_process(
                        handle,
                        samples,
                        sample_count,
                        16000,
                        1,
                        &mut out,
                        &mut len
                    ),
                    code::FLEX_INVALID_ARG
                );
                assert!(out.is_null());
                assert_eq!(len, 0);
            }
            assert_eq!(
                flexaudio_vad_flush(handle, &mut out, &mut len),
                code::FLEX_OK
            );
            assert!(out.is_null());
            assert_eq!(len, 0);
            flexaudio_vad_events_free(ptr::dangling_mut::<u8>().cast(), 1);
            assert_eq!(
                crate::error::last_audio_error().unwrap().kind(),
                flexaudio::ErrorKind::InvalidArg
            );
            flexaudio_vad_events_free(ptr::null_mut(), 1);
            flexaudio_vad_events_free(ptr::dangling_mut(), usize::MAX);
            flexaudio_vad_events_free(ptr::null_mut(), 0);
            flexaudio_vad_free(handle);
        }
    }
}
