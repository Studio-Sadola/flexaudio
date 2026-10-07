//! flexaudio-ffi — C ABI public layer (cbindgen → flexaudio.h). Pull-based API.
//!
//! The third way for a C app to use flexaudio in-process (the first is the CLI pipe, the
//! second is the N-API addon). Unlike napi, this uses no bridge thread or callbacks; the caller
//! periodically invokes `flexaudio_poll_chunk` / `flexaudio_poll_event` to retrieve chunks and
//! events.
//!
//! The design (config construction, chunk / event / device conversion, and error handling)
//! follows `flexaudio-napi`. The only difference is polling instead of callbacks.
//!
//! Guarantees:
//! - No function unwinds a panic across the FFI boundary (`catch_unwind` catches it and
//!   returns an error code, NULL, or false).
//! - Pointer arguments are checked for NULL.
//! - On failure, a negative `i32` is returned and a message is stored in thread-local
//!   last_error, available through `flexaudio_last_error`.
//! - Memory passed to C (chunk `data`, device strings and arrays) must be freed by Rust through
//!   the corresponding free function; do not use C's `free`.
//!
//! Regenerate the `include/flexaudio.h` header with cbindgen (using `cbindgen.toml`).

mod convert;
mod denoise;
mod error;
mod flac;
mod integration;
mod types;
mod vad;
mod watch;

// This crate builds C libraries rather than an rlib, so compile the FFI regression tests
// as a unit-test module while keeping their source under tests/.
#[cfg(test)]
#[path = "../tests/ffi/exclude_pids.rs"]
mod exclude_pids_tests;

use std::os::raw::c_char;
use std::panic::{catch_unwind, AssertUnwindSafe};

use error::{clear_last_error, code, last_error_ptr, set_last_error};
use types::{FlexChunk, FlexConfig, FlexDeviceInfo, FlexEvent, FlexProcessInfo, FlexStream};

// ---------------------------------------------------------------------------
// Panic guards
//
// Unwinding a Rust panic across the FFI boundary is undefined behavior. Wrap each function
// body in catch_unwind and return a value to the caller when a panic is caught. The failure
// value depends on the return type (PANIC code for i32, NULL for pointers, false for bool).
// ---------------------------------------------------------------------------

/// Wrap an i32-returning function in a panic guard. On panic, set last_error and return PANIC.
pub(crate) fn guard_i32(f: impl FnOnce() -> i32) -> i32 {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(v) => v,
        Err(_) => {
            set_last_error("panic caught at FFI boundary");
            code::FLEX_PANIC
        }
    }
}

/// Wrap a pointer-returning function in a panic guard. On panic, set last_error and return NULL.
pub(crate) fn guard_ptr<T>(f: impl FnOnce() -> *mut T) -> *mut T {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(v) => v,
        Err(_) => {
            set_last_error("panic caught at FFI boundary");
            std::ptr::null_mut()
        }
    }
}

/// Wrap a bool-returning function in a panic guard. On panic, return false.
fn guard_bool(f: impl FnOnce() -> bool) -> bool {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(false)
}

/// Small helper that records a `flexaudio::Error` in last_error and returns FAILURE.
fn fail(err: flexaudio::Error) -> i32 {
    set_last_error(err.to_string());
    code::FLEX_FAILURE
}

// ---------------------------------------------------------------------------
// Stream lifecycle
// ---------------------------------------------------------------------------

/// Open a stream from the configuration (without starting it). On failure, return NULL and
/// set last_error.
///
/// If `config.denoise` / `config.has_vad` is enabled, build the corresponding add-ons (noise
/// suppression / VAD) here and attach them to the stream (`poll_chunk` applies denoise → VAD).
/// When denoise is enabled, the output rate must be 48000 (otherwise return NULL and set
/// last_error; RNNoise requires 48 kHz). Free the returned handle with `flexaudio_free`.
/// This is equivalent to `flexaudio_open_with_exclude_pids(config, NULL, 0)`.
///
/// # Safety
/// `config` must point to a valid `FlexConfig` (NULL is treated as a failure).
#[no_mangle]
pub unsafe extern "C" fn flexaudio_open(config: *const FlexConfig) -> *mut FlexStream {
    open_with_exclude_pids(config, std::ptr::null(), 0)
}

/// Open a stream with additional PIDs excluded from system capture (or the system side of
/// mix), combined with `config.exclude_self`. Mic and process sources ignore valid lists.
/// The stream is not started; free the returned handle with `flexaudio_free`.
///
/// Every PID must be in 1..=4294967295, even for sources that ignore the list. At most 4096
/// entries are accepted. Order and duplicates are preserved. The list is copied before
/// returning; the caller may release or change the array after this call.
/// On Windows, all listed PIDs must equal the process tree root (this process when
/// `exclude_self` is true, otherwise the first listed PID). macOS resolves PIDs once at
/// start; Linux matches exact PIDs without their child processes.
///
/// Returns NULL and sets `flexaudio_last_error` on failure, including a NULL pointer with
/// nonzero length, a misaligned PID pointer, an excessive length, or a zero PID (the message
/// names its index). A zero length never dereferences `exclude_pids` and permits NULL.
/// Add-on settings and output defaults follow `flexaudio_open`.
///
/// # Safety
/// `config` must point to a valid `FlexConfig` with valid NUL-terminated string fields or NULL.
/// For a nonempty list of at most 4096 entries, `exclude_pids` must point to that many
/// initialized uint32_t values in one allocation, readable and unchanged during this call.
/// NULL and misaligned pointers are rejected before dereferencing.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_open_with_exclude_pids(
    config: *const FlexConfig,
    exclude_pids: *const u32,
    exclude_pids_len: usize,
) -> *mut FlexStream {
    open_with_exclude_pids(config, exclude_pids, exclude_pids_len)
}

/// Shared implementation for both open entry points, including validation and panic handling.
unsafe fn open_with_exclude_pids(
    config: *const FlexConfig,
    exclude_pids: *const u32,
    exclude_pids_len: usize,
) -> *mut FlexStream {
    guard_ptr(|| {
        clear_last_error();
        if !config.is_null() && !config.is_aligned() {
            set_last_error("flexaudio_open: config pointer is not aligned");
            return std::ptr::null_mut();
        }
        let Some(config) = config.as_ref() else {
            set_last_error("flexaudio_open: config pointer is null");
            return std::ptr::null_mut();
        };
        let exclude_pids = match convert::copy_exclude_pids(exclude_pids, exclude_pids_len) {
            Ok(pids) => pids,
            Err(e) => {
                set_last_error(e.to_string());
                return std::ptr::null_mut();
            }
        };
        let stream_config = match convert::build_config(config, exclude_pids) {
            Ok(c) => c,
            // build_config has already set last_error.
            Err(()) => return std::ptr::null_mut(),
        };
        // Build add-ons (denoise / VAD) first. Reject a 48 kHz constraint violation or model
        // loading failure here (build_addons has set last_error). Return before touching devices.
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
                #[cfg(test)]
                error::record_open_failure(e.clone());
                set_last_error(e.to_string());
                std::ptr::null_mut()
            }
        }
    })
}

/// Stop the stream, then free it. NULL-safe.
///
/// # Safety
/// `s` must be a handle returned by `flexaudio_open` (or NULL). Do not use `s` after freeing it.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_free(s: *mut FlexStream) {
    // Use the i32 guard to catch panics; discard its return value.
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

/// Start capture.
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

/// Stop capture.
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

/// Pause delivery while leaving the device running.
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

/// Resume delivery after a pause.
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
        match stream.inner.resume() {
            Ok(()) => code::FLEX_OK,
            Err(error) => fail(error),
        }
    })
}

/// Return true if paused. Return false for NULL or on panic.
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

/// Change the input gain (linear multiplier): 1.0 leaves it unchanged, 2.0 is about +6 dB,
/// and 0.0 is silent. Can be called during recording; it takes effect on the next chunk (20 ms
/// granularity). Samples are clamped to ±1.0 after multiplication. Must be finite and at least
/// 0, or FLEX_INVALID_ARG is returned.
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

/// Return the current input gain (linear multiplier). Return 1.0 for NULL or on panic.
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

/// Write the current backend's native format `(sample_rate, channels)` to `sr` / `ch`.
///
/// These values come from the backend at open and are updated by `flexaudio_switch_source`.
/// They are for display / diagnostics; the output format is set in `config`. Return 0 on
/// success and a negative value on error.
///
/// # Safety
/// `s` must be a valid handle, and `sr` / `ch` must be valid output pointers (NULL is InvalidArg).
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

/// Return the total number of chunks dropped by the chunk ring using DROP_OLDEST. Return 0 for
/// NULL or on panic.
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

/// Retrieve one chunk and fill `out`.
///
/// Return 1 when a chunk is retrieved and `out` is filled, 0 when none is available, or a
/// negative value on error. A terminal permission denial returns FLEX_FAILURE (-2)
/// with actionable guidance in flexaudio_last_error, including after stop.
/// `out.data` is owned by flexaudio; free it with
/// `flexaudio_chunk_free` when done.
///
/// If add-ons are enabled, the chunk passes through denoise → VAD before it is returned. When
/// VAD is enabled, confirmed events are stored in `out.vad_events` (with count
/// `out.vad_events_len`) and freed along with `data` by `flexaudio_chunk_free` (NULL/0 when
/// disabled or when there are no events).
///
/// # Safety
/// `s` must be a valid handle, and `out` must point to a valid `FlexChunk` destination.
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
        if let Some(error) = stream.inner.terminal_error() {
            return fail(error);
        }
        // Write the result after add-ons (denoise → VAD); pass through unchanged if disabled.
        match stream.poll_processed() {
            Ok(Some(chunk)) => {
                out.write(chunk);
                1
            }
            Ok(None) => match stream.inner.terminal_error() {
                Some(error) => fail(error),
                None => 0,
            },
            Err(error) => {
                set_last_error(error.to_string());
                code::FLEX_FAILURE
            }
        }
    })
}

/// Free the `data` filled by `flexaudio_poll_chunk` and set `data=NULL` / `len=0`.
/// Safe for NULL and repeated calls.
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

/// Retrieve one event and fill `out`.
///
/// Return 1 when an event is retrieved, 0 when none is available, or a negative value on error.
/// Error, PermissionDenied (kind 3), and SilenceWhileSourceActive (kind 7)
/// store their explanation in last_error. The advisory does not stop capture.
///
/// # Safety
/// `s` must be a valid handle, and `out` must point to a valid `FlexEvent` destination.
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
            // event_to_c stores Error/Unknown messages in last_error.
            Some(ev) => {
                out.write(convert::event_to_c(ev));
                1
            }
            None => 0,
        }
    })
}

/// Hot-swap the input source without stopping capture. `config.gain` is ignored because gain is
/// stream state; change it with `flexaudio_set_gain`. `config.denoise` / `config.has_vad` /
/// `config.vad` are also ignored because the add-ons configured at open remain in use. Since
/// `switch_source` cannot change the output format, the 48 kHz constraint and VAD settings do
/// not change.
/// This is equivalent to `flexaudio_switch_source_with_exclude_pids(s, config, NULL, 0)`.
///
/// # Safety
/// `s` must be a valid handle, and `config` must point to a valid `FlexConfig`.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_switch_source(
    s: *mut FlexStream,
    config: *const FlexConfig,
) -> i32 {
    switch_source_with_exclude_pids(s, config, std::ptr::null(), 0)
}

/// Hot-swap the source with additional PIDs excluded from system capture (or the system
/// side of mix), combined with `config.exclude_self`. Mic and process sources ignore valid
/// lists. Gain, add-ons, and output format follow `flexaudio_switch_source`.
///
/// Every PID must be in 1..=4294967295 and at most 4096 entries are accepted. Order and
/// duplicates are preserved. The list is copied before returning; the caller may release or
/// change the array after this call. Platform exclusion rules follow
/// `flexaudio_open_with_exclude_pids`.
///
/// Returns FLEX_OK on success, FLEX_INVALID_ARG for invalid arguments, or another negative
/// error code on failure, and sets `flexaudio_last_error`. A NULL pointer with nonzero length,
/// a misaligned PID pointer, an excessive length, or a zero PID is invalid; zero-PID messages
/// name the offending index. A zero length never dereferences `exclude_pids` and permits NULL.
/// Invalid lists are rejected before replacing the source or accessing devices.
///
/// # Safety
/// `s` must be a valid stream handle and `config` must point to a valid `FlexConfig` with valid
/// NUL-terminated string fields or NULL. For a nonempty list of at most 4096 entries,
/// `exclude_pids` must point to that many initialized uint32_t values in one allocation,
/// readable and unchanged during this call. NULL and misaligned pointers are rejected before
/// dereferencing.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_switch_source_with_exclude_pids(
    s: *mut FlexStream,
    config: *const FlexConfig,
    exclude_pids: *const u32,
    exclude_pids_len: usize,
) -> i32 {
    switch_source_with_exclude_pids(s, config, exclude_pids, exclude_pids_len)
}

/// Shared implementation for both source-switch entry points.
unsafe fn switch_source_with_exclude_pids(
    s: *mut FlexStream,
    config: *const FlexConfig,
    exclude_pids: *const u32,
    exclude_pids_len: usize,
) -> i32 {
    guard_i32(|| {
        clear_last_error();
        if !s.is_null() && !s.is_aligned() {
            set_last_error("flexaudio_switch_source: stream pointer is not aligned");
            return code::FLEX_INVALID_ARG;
        }
        let Some(stream) = s.as_mut() else {
            set_last_error("flexaudio_switch_source: stream pointer is null");
            return code::FLEX_INVALID_ARG;
        };
        if let Some(error) = stream.inner.terminal_error() {
            return fail(error);
        }
        if !config.is_null() && !config.is_aligned() {
            set_last_error("flexaudio_switch_source: config pointer is not aligned");
            return code::FLEX_INVALID_ARG;
        }
        let Some(config) = config.as_ref() else {
            set_last_error("flexaudio_switch_source: config pointer is null");
            return code::FLEX_INVALID_ARG;
        };
        let exclude_pids = match convert::copy_exclude_pids(exclude_pids, exclude_pids_len) {
            Ok(pids) => pids,
            Err(e) => {
                set_last_error(e.to_string());
                return code::FLEX_INVALID_ARG;
            }
        };
        let stream_config = match convert::build_config(config, exclude_pids) {
            Ok(c) => c,
            Err(()) => return code::FLEX_INVALID_ARG,
        };
        match stream.inner.switch_source(stream_config) {
            Ok(()) => code::FLEX_OK,
            Err(e @ flexaudio::Error::InvalidArg(_)) => {
                set_last_error(e.to_string());
                code::FLEX_INVALID_ARG
            }
            Err(e) => fail(e),
        }
    })
}

/// Return FLEX_OK when no terminal failure is stored, or FLEX_FAILURE (-2) and
/// set flexaudio_last_error to the terminal reason. Does not consume events and
/// remains available after flexaudio_stop.
///
/// # Safety
/// s must be a valid handle (NULL is InvalidArg).
#[no_mangle]
pub unsafe extern "C" fn flexaudio_terminal_error(s: *const FlexStream) -> i32 {
    guard_i32(|| {
        clear_last_error();
        // SAFETY: The caller supplies a valid handle or NULL as documented.
        let Some(stream) = (unsafe { s.as_ref() }) else {
            set_last_error("flexaudio_terminal_error: stream pointer is null");
            return code::FLEX_INVALID_ARG;
        };
        match stream.inner.terminal_error() {
            Some(error) => fail(error),
            None => code::FLEX_OK,
        }
    })
}

// ---------------------------------------------------------------------------
// Device enumeration
// ---------------------------------------------------------------------------

/// List available devices, allocate an array, and set `out_array` / `out_count`.
///
/// Return 0 on success. Free the allocated array with `flexaudio_devices_free`. In a headless
/// environment, an empty result (`out_array=NULL` / `out_count=0`) is still successful.
///
/// # Safety
/// `out_array` / `out_count` must be valid output pointers (NULL is InvalidArg).
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

/// Pass a converted array to C (shared by `flexaudio_devices` / `flexaudio_processes`).
///
/// If empty, set `out_array=NULL` / `out_count=0` without allocating. Converting to `Box<[T]>`
/// makes the allocation size exactly match the element count (capacity == len), which matches
/// the free side's `Vec::from_raw_parts(ptr, count, count)`.
///
/// # Safety
/// `out_array` / `out_count` must be valid non-NULL output pointers (checked by the caller).
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

/// Free the array allocated by `flexaudio_devices` and each `id` / `name`. NULL-safe.
///
/// # Safety
/// `arr` / `count` must be values returned by `flexaudio_devices` (or NULL/0).
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

/// List processes with audio output sessions (streams) that can be captured individually,
/// allocate an array, and set `out_array` / `out_count`. The calling process is excluded.
/// Processes whose audio sessions are stopped or idle are also included. Check `output_activity`
/// to see whether a process is currently producing audio.
///
/// Return 0 on success. If there are no candidates, an empty result (`out_array=NULL` /
/// `out_count=0`) is still successful: per-process capture is available, but there are no
/// matching processes now. Free the allocated array **once only** with
/// `flexaudio_processes_free`. Return `FLEX_FAILURE` (with the reason in `flexaudio_last_error`)
/// if per-process capture is unavailable in this environment (PipeWire is unreachable on
/// Linux; macOS is earlier than 14.4; Windows is older than build 20348 (Windows 11 / Windows
/// Server 2022 or later is required); the OS is unsupported; or access is denied), if the OS does
/// not respond within 3 seconds, or if the previous query is still running.
///
/// # Safety
/// `out_array` / `out_count` must be valid output pointers (NULL is InvalidArg).
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

/// Free the array allocated by `flexaudio_processes` and each string. NULL-safe.
/// Call **once only**; calling twice with the same pointer causes a double-free and undefined
/// behavior.
///
/// # Safety
/// `arr` / `count` must be values returned by `flexaudio_processes` (or NULL/0). Call this
/// function only once for the same `arr`.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_processes_free(arr: *mut FlexProcessInfo, count: usize) {
    guard_i32(|| {
        convert::free_process_array(arr, count);
        code::FLEX_OK
    });
}

// ---------------------------------------------------------------------------
// Retrieve errors
// ---------------------------------------------------------------------------

/// Return the most recent error message for the current thread.
///
/// Valid until the next FFI call on the same thread updates last_error. Returns NULL if there
/// is no error. The returned pointer is owned by flexaudio; do not free it from C.
#[no_mangle]
pub extern "C" fn flexaudio_last_error() -> *const c_char {
    // last_error_ptr does not panic, but guard it just in case and return NULL on panic.
    match catch_unwind(last_error_ptr) {
        Ok(p) => p,
        Err(_) => std::ptr::null(),
    }
}

#[cfg(test)]
mod permission_tests {
    use super::*;
    use flexaudio as fa;
    use std::ffi::CStr;

    struct DeniedBackend(Option<fa::Event>);

    impl fa::CaptureBackend for DeniedBackend {
        fn native_format(&self) -> (u32, u16) {
            (48_000, 2)
        }
        fn start(&mut self, _sink: fa::core::backend::RawSink) -> fa::Result<()> {
            Ok(())
        }
        fn stop(&mut self) {}
        fn poll_event(&mut self) -> Option<fa::Event> {
            self.0.take()
        }
    }

    fn denied_stream() -> fa::Stream {
        let mut stream = fa::Stream::open(
            fa::StreamConfig::default(),
            Box::new(DeniedBackend(Some(fa::Event::PermissionDenied {
                permission: fa::Permission::Microphone,
                detail: "denied by user".into(),
            }))),
        )
        .expect("open fake backend");
        stream.start().expect("start fake backend");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while stream.terminal_error().is_none() {
            assert!(
                std::time::Instant::now() < deadline,
                "terminal event was not processed"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        stream
    }

    #[test]
    fn terminal_poll_failure_keeps_code_reason_and_event_after_stop() {
        let mut stream = FlexStream {
            inner: denied_stream(),
            denoiser: None,
            vad: None,
        };
        let mut chunk = std::mem::MaybeUninit::<FlexChunk>::uninit();
        // SAFETY: stream is live and all output pointers reference writable local destinations.
        unsafe {
            assert_eq!(flexaudio_poll_chunk(&mut stream, chunk.as_mut_ptr()), -2);
            let message = CStr::from_ptr(flexaudio_last_error()).to_str().unwrap();
            assert!(message.contains("denied by user"));
            assert!(message.contains(&fa::Permission::Microphone.to_string()));
            assert!(message.contains(fa::Permission::Microphone.guidance()));
            let mut event = std::mem::MaybeUninit::<FlexEvent>::uninit();
            assert_eq!(flexaudio_poll_event(&mut stream, event.as_mut_ptr()), 1);
            assert_eq!(event.assume_init().kind as i32, 3);
            assert_eq!(flexaudio_terminal_error(&stream), -2);
            assert_eq!(flexaudio_stop(&mut stream), 0);
            assert_eq!(flexaudio_poll_chunk(&mut stream, chunk.as_mut_ptr()), -2);
            assert_eq!(flexaudio_resume(&mut stream), -2);
            assert_eq!(flexaudio_terminal_error(&stream), -2);
        }
    }

    #[test]
    fn terminal_accessor_handles_empty_and_null_streams() {
        let inner = fa::Stream::open(
            fa::StreamConfig::default(),
            Box::new(fa::MockBackend::new(48_000, 2, 0.0)),
        )
        .expect("open mock");
        let stream = FlexStream {
            inner,
            denoiser: None,
            vad: None,
        };
        // SAFETY: a valid local stream handle and explicitly permitted NULL are supplied.
        unsafe {
            assert_eq!(flexaudio_terminal_error(&stream), 0);
            assert!(flexaudio_last_error().is_null());
            assert_eq!(flexaudio_terminal_error(std::ptr::null()), -1);
        }
    }
}
