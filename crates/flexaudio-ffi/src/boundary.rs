//! Common checked destinations for additive owned-handle APIs.
use crate::{convert::valid_pointer, error};

pub(crate) fn invalid(message: &str) -> i32 {
    error::set_audio_error(flexaudio::Error::InvalidArg(message.into()))
}

pub(crate) unsafe fn output<T>(out: *mut *mut T) -> Result<(), i32> {
    if !valid_pointer(out) {
        return Err(invalid("invalid output destination"));
    }
    out.write(std::ptr::null_mut());
    Ok(())
}
