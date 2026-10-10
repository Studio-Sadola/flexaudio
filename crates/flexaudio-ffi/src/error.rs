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
    /// Device lookup completed without a match.
    pub const FLEX_DEVICE_NOT_FOUND: i32 = -5;
    /// Active device lost.
    pub const FLEX_DEVICE_LOST: i32 = -6;
    /// Recording permission denied.
    pub const FLEX_PERMISSION_DENIED: i32 = -7;
    /// Unsupported OS version.
    pub const FLEX_UNSUPPORTED_OS_VERSION: i32 = -8;
    /// Unsupported operation.
    pub const FLEX_UNSUPPORTED: i32 = -9;
    /// Unsupported PCM format.
    pub const FLEX_UNSUPPORTED_FORMAT: i32 = -10;
    /// Negotiated native format changed.
    pub const FLEX_NATIVE_FORMAT_CHANGED: i32 = -11;
    /// Device selection is ambiguous.
    pub const FLEX_AMBIGUOUS_DEVICE_NAME: i32 = -12;
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
    static LAST_AUDIO_ERROR: RefCell<Option<flexaudio::Error>> = const { RefCell::new(None) };
    static LAST_ERROR: RefCell<Option<CString>> = const { RefCell::new(None) };
}

// Native-open regression tests must distinguish a typed host denial from every
// other NULL result without interpreting a localized or display-only message.
#[cfg(test)]
thread_local! {
    static LAST_OPEN_FAILURE: RefCell<Option<flexaudio::Error>> = const { RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn record_open_failure(error: flexaudio::Error) {
    LAST_OPEN_FAILURE.with(|slot| *slot.borrow_mut() = Some(error));
}

#[cfg(test)]
pub(crate) fn take_open_failure() -> Option<flexaudio::Error> {
    LAST_OPEN_FAILURE.with(|slot| slot.borrow_mut().take())
}

/// Records the most recent error message for the current thread.
///
/// CString rejects embedded NUL bytes, so replace such messages with a fixed string.
/// Always set last_error, even if the original message is lost.
pub fn set_last_error(msg: impl Into<String>) {
    LAST_AUDIO_ERROR.with(|slot| {
        *slot.borrow_mut() = Some(flexaudio::Error::InvalidArg("invalid FFI argument".into()))
    });
    let cstring = CString::new(msg.into())
        .unwrap_or_else(|_| CString::new("error message contained a NUL byte").unwrap());
    LAST_ERROR.with(|slot| *slot.borrow_mut() = Some(cstring));
}

/// Clears the most recent error so successful operations do not leave stale messages.
pub fn clear_last_error() {
    LAST_AUDIO_ERROR.with(|slot| *slot.borrow_mut() = None);
    LAST_ERROR.with(|slot| *slot.borrow_mut() = None);
    #[cfg(test)]
    LAST_OPEN_FAILURE.with(|slot| *slot.borrow_mut() = None);
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

/// Root result code; wrappers always preserve the primary classification.
pub(crate) fn root_code(error: &flexaudio::Error) -> i32 {
    use flexaudio::ErrorKind;
    match error.kind() {
        ErrorKind::InvalidArg => code::FLEX_INVALID_ARG,
        ErrorKind::InvalidState => code::FLEX_INVALID_STATE,
        ErrorKind::DeviceNotFound => code::FLEX_DEVICE_NOT_FOUND,
        ErrorKind::DeviceLost => code::FLEX_DEVICE_LOST,
        ErrorKind::PermissionDenied => code::FLEX_PERMISSION_DENIED,
        ErrorKind::UnsupportedOsVersion => code::FLEX_UNSUPPORTED_OS_VERSION,
        ErrorKind::Unsupported => code::FLEX_UNSUPPORTED,
        ErrorKind::UnsupportedFormat => code::FLEX_UNSUPPORTED_FORMAT,
        ErrorKind::NativeFormatChanged => code::FLEX_NATIVE_FORMAT_CHANGED,
        ErrorKind::AmbiguousDeviceName => code::FLEX_AMBIGUOUS_DEVICE_NAME,
        ErrorKind::Backend => code::FLEX_FAILURE,
        _ => code::FLEX_FAILURE,
    }
}
/// Store a safe message and the original typed tree together.
pub(crate) fn set_audio_error(error: flexaudio::Error) -> i32 {
    let code = root_code(&error);
    set_last_error(error.to_string());
    LAST_AUDIO_ERROR.with(|slot| *slot.borrow_mut() = Some(error));
    code
}
pub(crate) fn last_audio_error() -> Option<flexaudio::Error> {
    LAST_AUDIO_ERROR.with(|slot| slot.borrow().clone())
}
