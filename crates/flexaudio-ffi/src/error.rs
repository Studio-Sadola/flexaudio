//! Error codes and the thread-local most recent error message.
//!
//! A function's return value (`i32`) carries only the kind of error; the human-readable message
//! is kept per calling thread. When the C side sees a failure, it gets the string with
//! [`flexaudio_last_error`].
//!
//! [`flexaudio_last_error`]: crate::flexaudio_last_error

use std::cell::RefCell;
use std::ffi::CString;
use std::os::raw::c_char;
use std::ptr;

/// Return codes of the FFI functions. 0 is success, negative is an error.
///
/// Only `poll_*` uses a positive 1 for "got one" and 0 for "none" (errors stay negative).
/// The names become `FLEX_OK` etc. in the C header so they do not collide with names in the
/// C namespace.
pub mod code {
    /// Success.
    pub const FLEX_OK: i32 = 0;
    /// Invalid argument (NULL pointer, invalid UTF-8, unknown enum value, etc.).
    pub const FLEX_INVALID_ARG: i32 = -1;
    /// A flexaudio operation failed (the message is stored in last_error).
    pub const FLEX_FAILURE: i32 = -2;
    /// A panic was caught at the FFI boundary (the message is stored in last_error).
    pub const FLEX_PANIC: i32 = -3;
    /// The handle's state does not fit the operation (such as a write to a finalized FLAC).
    pub const FLEX_INVALID_STATE: i32 = -4;
}

thread_local! {
    // The most recent error message. Valid until the next FFI call on the same thread that
    // updates last_error. The pointer returned by `flexaudio_last_error` points into this.
    static LAST_ERROR: RefCell<Option<CString>> = const { RefCell::new(None) };
}

/// Records the most recent error message for the current thread.
///
/// CString rejects a NUL inside the message, so in that case it is replaced with a fixed
/// message (even if the message is lost, last_error itself is always set).
pub fn set_last_error(msg: impl Into<String>) {
    let cstring = CString::new(msg.into())
        .unwrap_or_else(|_| CString::new("error message contained a NUL byte").unwrap());
    LAST_ERROR.with(|slot| *slot.borrow_mut() = Some(cstring));
}

/// Clears the most recent error (called around successful operations so no stale message
/// remains).
pub fn clear_last_error() {
    LAST_ERROR.with(|slot| *slot.borrow_mut() = None);
}

/// Returns a pointer to the most recent error message for the current thread.
///
/// The returned pointer points into the thread-local contents and is valid until the next call
/// on the same thread that updates last_error. NULL if there is no error. It must not be freed
/// on the C side.
pub fn last_error_ptr() -> *const c_char {
    LAST_ERROR.with(|slot| match &*slot.borrow() {
        Some(cstring) => cstring.as_ptr(),
        None => ptr::null(),
    })
}
