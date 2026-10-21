//! Presence-aware C gains; all shared conversion and execution belongs to v4.
use crate::{boundary::invalid, convert::valid_pointer, types::FlexStream, v4, v5_types::*};

unsafe fn envelope<'a, T>(value: *const T) -> Result<&'a T, i32> {
    if !valid_pointer(value) {
        return Err(invalid("invalid v5 envelope pointer"));
    }
    // Only the two header words are readable until the exact layout is established.
    let words = value.cast::<u32>();
    if usize::try_from(words.read()).ok() != Some(std::mem::size_of::<T>())
        || words.add(1).read() != FLEX_STREAM_VERSION_5
    {
        return Err(invalid("v5 envelope size/version mismatch"));
    }
    Ok(&*value)
}

unsafe fn gain(value: *const f32) -> Result<f32, i32> {
    if value.is_null() {
        return Ok(1.0);
    }
    if !valid_pointer(value) {
        return Err(invalid("invalid gain pointer"));
    }
    // The caller guarantees a readable float for the duration of this call.
    Ok(value.read())
}

unsafe fn gains(value: *const FlexGainsV5) -> Result<v4::LiteralGains, i32> {
    let value = if value.is_null() {
        None
    } else {
        Some(envelope(value)?)
    };
    Ok(v4::LiteralGains {
        gain: gain(value.map_or(std::ptr::null(), |v| v.gain))?,
        mix_mic_gain: gain(value.map_or(std::ptr::null(), |v| v.mix_mic_gain))?,
        mix_system_gain: gain(value.map_or(std::ptr::null(), |v| v.mix_system_gain))?,
    })
}

/// Open with presence-aware gains and unchanged v4 controls, reports and strict default.
/// NULL gain pointers select unity; explicit zero mutes. Nested FlexConfig gain,
/// mix_mic_gain and mix_system_gain must be zero; otherwise InvalidArg names the
/// field and directs callers to FlexGainsV5 before any native access.
/// Core validates output gain on open and mix gains only for Mix sources.
/// Other sources report non-default mix gains as ignored (strict rejects them).
/// out is NULL on failure. All borrowed configuration is copied.
/// # Safety
/// Envelopes have readable header words and full storage only when those match.
/// Nested fields and supplied floats are readable for this call; out is writable.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_open_v5(
    config: *const FlexStreamConfigV5,
    out: *mut *mut FlexStream,
) -> i32 {
    open_with(config, out, flexaudio::open_with_options)
}

unsafe fn open_with(
    config: *const FlexStreamConfigV5,
    out: *mut *mut FlexStream,
    open: impl FnOnce(
        flexaudio::StreamConfig,
        flexaudio::OpenOptions,
    ) -> flexaudio::Result<flexaudio::Stream>,
) -> i32 {
    v4::open_shared(
        || {
            let config = envelope(config)?;
            Ok((config.base, Some(gains(config.gains)?)))
        },
        out,
        open,
    )
}

/// Switch with v4 strict semantics (0 retain, 1 on, 2 off). Omitted output gain
/// retains the current gain. Explicit non-default output gain, including zero,
/// returns FLEX_IGNORED_SETTINGS under strict mode, or is ignored in permissive mode.
/// Mix gains apply to Mix sources, using literal values or unity defaults. Other
/// sources report non-default mix gains as ignored. Failure preserves the old source/report.
/// Output gain requests, even NaN or negative values, follow core's fixedAtOpen
/// reporting without range validation on switch. Core validates Mix-source gains.
/// Nested FlexConfig gain fields must be zero, as on open_v5; nonzero fields
/// return InvalidArg directing callers to FlexGainsV5 before any native access.
/// # Safety
/// stream is live/exclusively borrowed; envelopes and fields obey open_v5's requirements.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_switch_source_v5(
    stream: *mut FlexStream,
    config: *const FlexSwitchConfigV5,
) -> i32 {
    switch_with(
        stream,
        config,
        flexaudio::Stream::switch_source_with_options,
    )
}

unsafe fn switch_with(
    stream: *mut FlexStream,
    config: *const FlexSwitchConfigV5,
    switch: impl FnOnce(
        &mut flexaudio::Stream,
        flexaudio::StreamConfig,
        flexaudio::OpenOptions,
    ) -> flexaudio::Result<()>,
) -> i32 {
    v4::switch_shared_with(
        stream,
        || {
            let config = envelope(config)?;
            Ok((config.base, Some(gains(config.gains)?)))
        },
        switch,
    )
}

#[cfg(test)]
#[path = "../tests/ffi/v5.rs"]
mod tests;
