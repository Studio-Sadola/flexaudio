//! Error codes and the most recent thread-local error message.
//!
//! Functions return only the error kind (`i32`); human-readable messages are stored
//! per caller thread. On failure, C callers can get the message with
//! [`flexaudio_last_error`].
//!
//! [`flexaudio_last_error`]: crate::flexaudio_last_error

use std::cell::RefCell;
use std::ffi::CString;
use std::os::raw::c_char;
use std::ptr;

/// FFI function return codes. Zero means success; negative values indicate errors.
///
/// Only `poll_*` uses positive 1 for “available” and 0 for “none”; errors remain negative.
/// The C header uses names such as `FLEX_OK` to avoid collisions in the C namespace.
pub mod code {
    /// Success.
    pub const FLEX_OK: i32 = 0;
    /// Invalid argument (NULL pointer, invalid UTF-8, unknown enum value, etc.).
    pub const FLEX_INVALID_ARG: i32 = -1;
    /// A flexaudio operation failed (the message is stored in last_error).
    pub const FLEX_FAILURE: i32 = -2;
    /// A panic was caught at the FFI boundary (the message is stored in last_error).
    pub const FLEX_PANIC: i32 = -3;
    /// The handle state does not allow the operation (such as writing to finalized FLAC).
    pub const FLEX_INVALID_STATE: i32 = -4;
}

thread_local! {
    // Most recent error message, valid until the next FFI call on this thread updates
    // last_error. The pointer returned by `flexaudio_last_error` refers to this value.
    static LAST_ERROR: RefCell<Option<CString>> = const { RefCell::new(None) };
}

/// Records the most recent error message for the current thread.
///
/// CString rejects embedded NUL bytes, so replace such messages with a fixed string.
/// Always set last_error, even if the original message is lost.
pub fn set_last_error(msg: impl Into<String>) {
    let cstring = CString::new(msg.into())
        .unwrap_or_else(|_| CString::new("error message contained a NUL byte").unwrap());
    LAST_ERROR.with(|slot| *slot.borrow_mut() = Some(cstring));
}

/// Clears the most recent error so successful operations do not leave stale messages.
pub fn clear_last_error() {
    LAST_ERROR.with(|slot| *slot.borrow_mut() = None);
}

/// Returns a pointer to the most recent error message for the current thread.
///
/// The pointer refers to thread-local storage and remains valid until the next call on
/// the same thread updates last_error. Returns NULL if there is no error. Do not free
/// it from C.
pub fn last_error_ptr() -> *const c_char {
    LAST_ERROR.with(|slot| match &*slot.borrow() {
        Some(cstring) => cstring.as_ptr(),
        None => ptr::null(),
    })
}
