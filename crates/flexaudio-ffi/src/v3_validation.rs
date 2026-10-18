//! Structural envelope checks before references, allocation or platform access.
use crate::{boundary::invalid, convert, error, types::FlexConfig, v3_types::*};
use flexaudio::{OpenOptions, OutputFormat, StreamConfig};
use std::mem;

/// The caller provides at least the two initialized header words; full storage is
/// required only when size/version match. No full reference is formed on mismatch.
pub(crate) unsafe fn envelope<'a, T>(value: *const T) -> Result<&'a T, i32> {
    if !convert::valid_pointer(value) {
        return Err(invalid("invalid v3 envelope pointer"));
    }
    let words = value.cast::<u32>();
    let size = words.read();
    let version = words.add(1).read();
    if usize::try_from(size).ok() != Some(mem::size_of::<T>()) || version != FLEX_STREAM_VERSION_3 {
        return Err(invalid("v3 envelope size/version mismatch"));
    }
    Ok(&*value)
}

pub(crate) unsafe fn config(
    base: *const FlexConfig,
    pids: *const u32,
    len: usize,
) -> Result<StreamConfig, i32> {
    if !convert::valid_pointer(base) {
        return Err(invalid("invalid base config pointer"));
    }
    let pids = convert::copy_exclude_pids(pids, len).map_err(error::set_audio_error)?;
    let config = convert::build_config(&*base, pids).map_err(|()| error::code::FLEX_INVALID_ARG)?;
    config.output.validate().map_err(error::set_audio_error)?;
    Ok(config)
}

pub(crate) fn options(value: u32, current: &OpenOptions) -> Result<OpenOptions, i32> {
    match value {
        FLEX_KEEPALIVE_DEFAULT => Ok(current.clone()),
        FLEX_KEEPALIVE_ON => Ok(current.clone().windows_classic_loopback_keepalive(true)),
        FLEX_KEEPALIVE_OFF => Ok(current.clone().windows_classic_loopback_keepalive(false)),
        _ => Err(invalid("keepalive must be 0, 1, or 2")),
    }
}

pub(crate) unsafe fn secondary(
    value: *const FlexSecondaryOutputV3,
) -> Result<Option<(OutputFormat, SecondaryEncoding)>, i32> {
    if value.is_null() {
        return Ok(None);
    }
    let value = envelope(value)?;
    let format = OutputFormat {
        sample_rate: value.rate,
        channels: value.channels,
    };
    format.validate().map_err(error::set_audio_error)?;
    match value.encoding {
        FLEX_SECONDARY_F32 => Ok(Some((format, SecondaryEncoding::F32))),
        FLEX_SECONDARY_S16 => Ok(Some((format, SecondaryEncoding::S16))),
        _ => Err(invalid("unknown secondary encoding")),
    }
}
