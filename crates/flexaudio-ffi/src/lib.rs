//! flexaudio-ffi — C ABI layer (cbindgen → flexaudio.h). Pull-based API.
//!
//! The third way for a C application to use flexaudio in-process (the first is the CLI pipe,
//! the second is the N-API addon). Unlike napi, it does not use a bridge thread + callbacks;
//! it is pull-based: the caller periodically calls `flexaudio_poll_chunk` /
//! `flexaudio_poll_event` to take out chunks and events.
//!
//! The design (config construction, chunk/event/device conversion, error handling) follows
//! `flexaudio-napi` as the model. The only difference is polling instead of callbacks.
//!
//! Guarantees:
//! - No function lets a panic unwind across the FFI boundary (each is wrapped in
//!   `catch_unwind`; on panic it returns an error code / NULL / false).
//! - Pointer arguments are checked for NULL.
//! - On failure, the `i32` is negative and a message is stored in the thread-local last_error,
//!   retrievable with `flexaudio_last_error`.
//! - Allocations handed to C (a chunk's `data`, device strings and arrays) are always freed
//!   by the Rust side through the matching free function (C's free must not be used).
//!
//! The header `include/flexaudio.h` is regenerated with cbindgen (using `cbindgen.toml`).

mod convert;
mod denoise;
mod error;
mod flac;
mod integration;
mod types;
mod vad;
mod watch;

use std::os::raw::c_char;
use std::panic::{catch_unwind, AssertUnwindSafe};

use error::{clear_last_error, code, last_error_ptr, set_last_error};
use types::{FlexChunk, FlexConfig, FlexDeviceInfo, FlexEvent, FlexProcessInfo, FlexStream};

// ---------------------------------------------------------------------------
// Panic guards
//
// Letting a Rust panic unwind across the FFI boundary is undefined behavior. Each function
// body is wrapped in catch_unwind, and a caught panic is returned to the caller as a value.
// The "value that means failure" differs by type (i32 is the PANIC code / pointer is NULL /
// bool is false).
// ---------------------------------------------------------------------------

/// Wraps a function returning `i32` in a panic guard. On panic, sets last_error and returns PANIC.
pub(crate) fn guard_i32(f: impl FnOnce() -> i32) -> i32 {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(v) => v,
        Err(_) => {
            set_last_error("panic caught at FFI boundary");
            code::FLEX_PANIC
        }
    }
}

/// Wraps a function returning a pointer in a panic guard. On panic, sets last_error and returns
/// NULL.
pub(crate) fn guard_ptr<T>(f: impl FnOnce() -> *mut T) -> *mut T {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(v) => v,
        Err(_) => {
            set_last_error("panic caught at FFI boundary");
            std::ptr::null_mut()
        }
    }
}

/// Wraps a function returning `bool` in a panic guard. On panic, returns false.
fn guard_bool(f: impl FnOnce() -> bool) -> bool {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(false)
}

/// Small helper that records a `flexaudio::Error` in last_error and returns the FAILURE code.
fn fail(err: flexaudio::Error) -> i32 {
    set_last_error(err.to_string());
    code::FLEX_FAILURE
}

// ---------------------------------------------------------------------------
// Stream lifecycle
// ---------------------------------------------------------------------------

/// Opens a stream from a config (does not start it yet). On failure returns NULL and sets
/// last_error.
///
/// If `config.denoise` / `config.has_vad` are enabled, the corresponding addons (noise
/// suppression / VAD) are built here and live alongside the stream (`poll_chunk` passes chunks
/// through denoise → VAD in that order). With denoise enabled, this fails unless the output
/// rate is 48000 (NULL + last_error; RNNoise is fixed at 48kHz). Free the returned handle
/// with `flexaudio_free`.
///
/// # Safety
/// `config` must point to a valid `FlexConfig` (NULL is treated as a failure).
#[no_mangle]
pub unsafe extern "C" fn flexaudio_open(config: *const FlexConfig) -> *mut FlexStream {
    guard_ptr(|| {
        clear_last_error();
        let Some(config) = config.as_ref() else {
            set_last_error("flexaudio_open: config pointer is null");
            return std::ptr::null_mut();
        };
        let stream_config = match convert::build_config(config) {
            Ok(c) => c,
            // build_config has already set last_error.
            Err(()) => return std::ptr::null_mut(),
        };
        // Build the addons (denoise / VAD) first. A 48k constraint violation or a model load
        // failure is rejected here (build_addons has already set last_error). Return before
        // touching the device.
        let (denoiser, vad) = match integration::build_addons(config) {
            Ok(pair) => pair,
            Err(()) => return std::ptr::null_mut(),
        };
        match flexaudio::open(stream_config) {
            Ok(inner) => Box::into_raw(Box::new(FlexStream {
                inner,
                denoiser,
                vad,
            })),
            Err(e) => {
                set_last_error(e.to_string());
                std::ptr::null_mut()
            }
        }
    })
}

/// Stops the stream and then frees it. NULL-safe.
///
/// # Safety
/// `s` must be a handle returned by `flexaudio_open` (or NULL).
/// `s` must not be used after it is freed.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_free(s: *mut FlexStream) {
    // Ride on the i32 guard, discarding the return value, to absorb panics.
    guard_i32(|| {
        if s.is_null() {
            return code::FLEX_OK;
        }
        let mut stream = Box::from_raw(s);
        stream.inner.stop();
        drop(stream);
        code::FLEX_OK
    });
}

/// Starts capture.
///
/// # Safety
/// `s` must be a valid handle (NULL is InvalidArg).
#[no_mangle]
pub unsafe extern "C" fn flexaudio_start(s: *mut FlexStream) -> i32 {
    guard_i32(|| {
        clear_last_error();
        let Some(stream) = s.as_mut() else {
            set_last_error("flexaudio_start: stream pointer is null");
            return code::FLEX_INVALID_ARG;
        };
        match stream.inner.start() {
            Ok(()) => code::FLEX_OK,
            Err(e) => fail(e),
        }
    })
}

/// Stops capture.
///
/// # Safety
/// `s` must be a valid handle (NULL is InvalidArg).
#[no_mangle]
pub unsafe extern "C" fn flexaudio_stop(s: *mut FlexStream) -> i32 {
    guard_i32(|| {
        clear_last_error();
        let Some(stream) = s.as_mut() else {
            set_last_error("flexaudio_stop: stream pointer is null");
            return code::FLEX_INVALID_ARG;
        };
        stream.inner.stop();
        code::FLEX_OK
    })
}

/// Pauses delivery (the device keeps running).
///
/// # Safety
/// `s` must be a valid handle (NULL is InvalidArg).
#[no_mangle]
pub unsafe extern "C" fn flexaudio_pause(s: *mut FlexStream) -> i32 {
    guard_i32(|| {
        clear_last_error();
        let Some(stream) = s.as_mut() else {
            set_last_error("flexaudio_pause: stream pointer is null");
            return code::FLEX_INVALID_ARG;
        };
        stream.inner.pause();
        code::FLEX_OK
    })
}

/// Clears the pause and resumes delivery.
///
/// # Safety
/// `s` must be a valid handle (NULL is InvalidArg).
#[no_mangle]
pub unsafe extern "C" fn flexaudio_resume(s: *mut FlexStream) -> i32 {
    guard_i32(|| {
        clear_last_error();
        let Some(stream) = s.as_mut() else {
            set_last_error("flexaudio_resume: stream pointer is null");
            return code::FLEX_INVALID_ARG;
        };
        stream.inner.resume();
        code::FLEX_OK
    })
}

/// Returns true while paused. Returns false on NULL or panic.
///
/// # Safety
/// `s` must be a valid handle (or NULL).
#[no_mangle]
pub unsafe extern "C" fn flexaudio_is_paused(s: *const FlexStream) -> bool {
    guard_bool(|| match s.as_ref() {
        Some(stream) => stream.inner.is_paused(),
        None => false,
    })
}

/// Changes the input gain (linear multiplier). 1.0 leaves it unchanged, 2.0 is about +6dB, 0.0
/// is silence. Can be called at any time during recording and takes effect from the next chunk
/// (20ms granularity). Samples after multiplication are clamped to ±1.0. Returns
/// FLEX_INVALID_ARG unless the value is finite and at least 0.
///
/// # Safety
/// `s` must be a valid handle (NULL is InvalidArg).
#[no_mangle]
pub unsafe extern "C" fn flexaudio_set_gain(s: *mut FlexStream, gain: f32) -> i32 {
    guard_i32(|| {
        clear_last_error();
        let Some(stream) = s.as_mut() else {
            set_last_error("flexaudio_set_gain: stream pointer is null");
            return code::FLEX_INVALID_ARG;
        };
        match stream.inner.set_gain(gain) {
            Ok(()) => code::FLEX_OK,
            Err(e) => {
                set_last_error(e.to_string());
                code::FLEX_INVALID_ARG
            }
        }
    })
}

/// Returns the current input gain (linear multiplier). Returns 1.0 on NULL or panic.
///
/// # Safety
/// `s` must be a valid handle (or NULL).
#[no_mangle]
pub unsafe extern "C" fn flexaudio_gain(s: *const FlexStream) -> f32 {
    catch_unwind(AssertUnwindSafe(|| match s.as_ref() {
        Some(stream) => stream.inner.gain(),
        None => 1.0,
    }))
    .unwrap_or(1.0)
}

/// Writes the current backend's native format `(sample_rate, channels)` to `sr`/`ch`.
///
/// The value is obtained from the backend at open time and is updated by
/// `flexaudio_switch_source`. For display and diagnostics (the output format is the value
/// specified in `config`). Returns 0 = success / negative = error.
///
/// # Safety
/// `s` must be a valid handle and `sr`/`ch` must be valid write targets (NULL is InvalidArg).
#[no_mangle]
pub unsafe extern "C" fn flexaudio_native_format(
    s: *const FlexStream,
    sr: *mut u32,
    ch: *mut u16,
) -> i32 {
    guard_i32(|| {
        clear_last_error();
        let Some(stream) = s.as_ref() else {
            set_last_error("flexaudio_native_format: stream pointer is null");
            return code::FLEX_INVALID_ARG;
        };
        if sr.is_null() || ch.is_null() {
            set_last_error("flexaudio_native_format: output pointer is null");
            return code::FLEX_INVALID_ARG;
        }
        let (rate, channels) = stream.inner.native_format();
        sr.write(rate);
        ch.write(channels);
        code::FLEX_OK
    })
}

/// Returns the cumulative number of chunks the chunk ring has discarded so far via
/// DROP_OLDEST. Returns 0 on NULL or panic.
///
/// # Safety
/// `s` must be a valid handle (or NULL).
#[no_mangle]
pub unsafe extern "C" fn flexaudio_dropped_chunks(s: *const FlexStream) -> u64 {
    catch_unwind(AssertUnwindSafe(|| match s.as_ref() {
        Some(stream) => stream.inner.dropped_chunks(),
        None => 0,
    }))
    .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Polling (the core of the pull-based API)
// ---------------------------------------------------------------------------

/// Takes out one chunk and fills `out`.
///
/// Returns 1 = got one and filled `out` / 0 = none right now / negative = error. `out.data`
/// is owned by flexaudio; free it with `flexaudio_chunk_free` when done.
///
/// If addons are enabled, the chunk is passed through denoise → VAD in that order before it
/// is returned. With VAD enabled, the finalized events go into `out.vad_events` (element count
/// `out.vad_events_len`), which `flexaudio_chunk_free` also frees together with `data`
/// (NULL/0 when disabled or when there are no events).
///
/// # Safety
/// `s` must be a valid handle and `out` must be a valid `FlexChunk` write target.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_poll_chunk(s: *mut FlexStream, out: *mut FlexChunk) -> i32 {
    guard_i32(|| {
        clear_last_error();
        let Some(stream) = s.as_mut() else {
            set_last_error("flexaudio_poll_chunk: stream pointer is null");
            return code::FLEX_INVALID_ARG;
        };
        if out.is_null() {
            set_last_error("flexaudio_poll_chunk: out pointer is null");
            return code::FLEX_INVALID_ARG;
        }
        // Write the result after passing through the addons (denoise → VAD). Pass-through if
        // they are disabled.
        match stream.poll_processed() {
            Some(chunk) => {
                out.write(chunk);
                1
            }
            None => 0,
        }
    })
}

/// Frees the `data` filled by `flexaudio_poll_chunk` and sets `data=NULL` / `len=0`.
/// Safe for both NULL and double free.
///
/// # Safety
/// `chunk` must point to a `FlexChunk` filled by `flexaudio_poll_chunk` (or be NULL).
#[no_mangle]
pub unsafe extern "C" fn flexaudio_chunk_free(chunk: *mut FlexChunk) {
    guard_i32(|| {
        if let Some(chunk) = chunk.as_mut() {
            convert::free_chunk_data(chunk);
        }
        code::FLEX_OK
    });
}

/// Takes out one event and fills `out`.
///
/// Returns 1 = got one / 0 = none right now / negative = error. For an `Error` event,
/// sets `out.kind = Error` and puts the message in last_error.
///
/// # Safety
/// `s` must be a valid handle and `out` must be a valid `FlexEvent` write target.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_poll_event(s: *mut FlexStream, out: *mut FlexEvent) -> i32 {
    guard_i32(|| {
        clear_last_error();
        let Some(stream) = s.as_mut() else {
            set_last_error("flexaudio_poll_event: stream pointer is null");
            return code::FLEX_INVALID_ARG;
        };
        if out.is_null() {
            set_last_error("flexaudio_poll_event: out pointer is null");
            return code::FLEX_INVALID_ARG;
        }
        match stream.inner.poll_event() {
            // event_to_c puts the Error/Unknown message in last_error.
            Some(ev) => {
                out.write(convert::event_to_c(ev));
                1
            }
            None => 0,
        }
    })
}

/// Hot-swaps the input source without stopping the recording. `config.gain` is ignored
/// (gain is stream state; change it with `flexaudio_set_gain`). Likewise `config.denoise` /
/// `config.has_vad` / `config.vad` are ignored (the addons keep what was fixed at open time;
/// the output format cannot be changed by switch_source, so the 48k constraint and the VAD
/// settings stay unchanged).
///
/// # Safety
/// `s` must be a valid handle and `config` must point to a valid `FlexConfig`.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_switch_source(
    s: *mut FlexStream,
    config: *const FlexConfig,
) -> i32 {
    guard_i32(|| {
        clear_last_error();
        let Some(stream) = s.as_mut() else {
            set_last_error("flexaudio_switch_source: stream pointer is null");
            return code::FLEX_INVALID_ARG;
        };
        let Some(config) = config.as_ref() else {
            set_last_error("flexaudio_switch_source: config pointer is null");
            return code::FLEX_INVALID_ARG;
        };
        let stream_config = match convert::build_config(config) {
            Ok(c) => c,
            Err(()) => return code::FLEX_INVALID_ARG,
        };
        match stream.inner.switch_source(stream_config) {
            Ok(()) => code::FLEX_OK,
            Err(e) => fail(e),
        }
    })
}

// ---------------------------------------------------------------------------
// Device enumeration
// ---------------------------------------------------------------------------

/// Enumerates the available devices, allocates an array, and sets `out_array` / `out_count`.
///
/// Returns 0 on success. Free the allocated array with `flexaudio_devices_free`. In a headless
/// environment, 0 devices (`out_array=NULL` / `out_count=0`) is also treated as success.
///
/// # Safety
/// `out_array` / `out_count` must be valid write targets (NULL is InvalidArg).
#[no_mangle]
pub unsafe extern "C" fn flexaudio_devices(
    out_array: *mut *mut FlexDeviceInfo,
    out_count: *mut usize,
) -> i32 {
    guard_i32(|| {
        clear_last_error();
        if out_array.is_null() || out_count.is_null() {
            set_last_error("flexaudio_devices: output pointer is null");
            return code::FLEX_INVALID_ARG;
        }
        let list = match flexaudio::devices() {
            Ok(list) => list,
            Err(e) => return fail(e),
        };
        write_c_array(
            list.into_iter().map(convert::device_info_to_c).collect(),
            out_array,
            out_count,
        );
        code::FLEX_OK
    })
}

/// Hands a converted array to C (shared by `flexaudio_devices` / `flexaudio_processes`).
///
/// If empty, `out_array=NULL` / `out_count=0` (nothing is allocated). Collecting into a
/// `Box<[T]>` makes the allocation size exactly the element count (capacity == len), which
/// matches `Vec::from_raw_parts(ptr, count, count)` on the free side.
///
/// # Safety
/// `out_array` / `out_count` must be non-NULL valid write targets (already checked by the
/// caller).
unsafe fn write_c_array<T>(items: Box<[T]>, out_array: *mut *mut T, out_count: *mut usize) {
    if items.is_empty() {
        out_array.write(std::ptr::null_mut());
        out_count.write(0);
        return;
    }
    let count = items.len();
    out_array.write(Box::into_raw(items) as *mut T);
    out_count.write(count);
}

/// Frees the array allocated by `flexaudio_devices` and each `id`/`name`. NULL-safe.
///
/// # Safety
/// `arr`/`count` must be what `flexaudio_devices` returned (or NULL/0).
#[no_mangle]
pub unsafe extern "C" fn flexaudio_devices_free(arr: *mut FlexDeviceInfo, count: usize) {
    guard_i32(|| {
        convert::free_device_array(arr, count);
        code::FLEX_OK
    });
}

// ---------------------------------------------------------------------------
// Process enumeration
// ---------------------------------------------------------------------------

/// Enumerates the processes that have an audio output session (stream) and can therefore be
/// targeted by per-process capture, allocates an array, and sets `out_array` / `out_count`.
/// The calling process itself is not included. Stopped and Idle ones are listed too. Whether
/// one is playing right now is shown by `output_activity`.
///
/// Returns 0 on success. If there are no candidates, it succeeds with 0 entries
/// (`out_array=NULL` / `out_count=0`) (per-process capture is usable, but no such process
/// exists right now). Free the allocated array **exactly once** with
/// `flexaudio_processes_free`. Returns `FLEX_FAILURE` (reason in `flexaudio_last_error`) when
/// per-process capture is not usable in this environment (PipeWire unreachable on Linux,
/// macOS older than 14.4, not Windows build 20348 or later (Windows 11 / Windows Server 2022),
/// unsupported OS, permission denied), when the OS did not respond within 3 seconds, or when
/// a previous query has not finished yet.
///
/// # Safety
/// `out_array` / `out_count` must be valid write targets (NULL is InvalidArg).
#[no_mangle]
pub unsafe extern "C" fn flexaudio_processes(
    out_array: *mut *mut FlexProcessInfo,
    out_count: *mut usize,
) -> i32 {
    guard_i32(|| {
        clear_last_error();
        if out_array.is_null() || out_count.is_null() {
            set_last_error("flexaudio_processes: output pointer is null");
            return code::FLEX_INVALID_ARG;
        }
        let list = match flexaudio::processes() {
            Ok(list) => list,
            Err(e) => return fail(e),
        };
        write_c_array(
            list.into_iter().map(convert::process_info_to_c).collect(),
            out_array,
            out_count,
        );
        code::FLEX_OK
    })
}

/// Frees the array allocated by `flexaudio_processes` and each string. NULL-safe.
/// Call it **exactly once** (calling it twice on the same pointer is a double free =
/// undefined behavior).
///
/// # Safety
/// `arr`/`count` must be what `flexaudio_processes` returned (or NULL/0), and this function
/// is called exactly once for the same `arr`.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_processes_free(arr: *mut FlexProcessInfo, count: usize) {
    guard_i32(|| {
        convert::free_process_array(arr, count);
        code::FLEX_OK
    });
}

// ---------------------------------------------------------------------------
// Error retrieval
// ---------------------------------------------------------------------------

/// Returns the most recent error message for the current thread.
///
/// Valid until the next FFI call on the same thread that updates last_error. NULL if there is
/// no error. The returned pointer is owned by flexaudio and must not be freed on the C side.
#[no_mangle]
pub extern "C" fn flexaudio_last_error() -> *const c_char {
    // last_error_ptr itself does not panic, but guard it anyway and return NULL.
    match catch_unwind(last_error_ptr) {
        Ok(p) => p,
        Err(_) => std::ptr::null(),
    }
}
