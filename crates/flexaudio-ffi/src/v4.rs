//! Version-four envelopes and pure settings preflight before native/model work.
use crate::{
    boundary::{invalid, output},
    convert::valid_pointer,
    error, guard_i32,
    settings_request::attachments,
    types::FlexStream,
    v3::{finish_open, PreparedOpen},
    v3_types::*,
    v3_validation as v3,
    v4_types::*,
    vad_integration::validate_vad,
};
use flexaudio::{OpenOptions, SettingKey as K, SettingsRequest, StreamConfig};

unsafe fn envelope<'a, T>(value: *const T) -> Result<&'a T, i32> {
    if !valid_pointer(value) {
        return Err(invalid("invalid v4 envelope pointer"));
    }
    // Only the two header words are readable until the exact layout is established.
    let words = value.cast::<u32>();
    if usize::try_from(words.read()).ok() != Some(std::mem::size_of::<T>())
        || words.add(1).read() != FLEX_STREAM_VERSION_4
    {
        return Err(invalid("v4 envelope size/version mismatch"));
    }
    Ok(&*value)
}

unsafe fn controls(
    value: *const FlexOptionsV4,
    config: &mut StreamConfig,
    options: &mut OpenOptions,
) -> Result<(), i32> {
    if value.is_null() {
        return Ok(());
    }
    let value = envelope(value)?;
    options.strict = match value.strict {
        0 => options.strict,
        1 => true,
        2 => false,
        _ => return Err(invalid("strict must be 0, 1, or 2")),
    };
    if !value.ring_capacity_chunks.is_null() {
        if !valid_pointer(value.ring_capacity_chunks) {
            return Err(invalid("invalid ring capacity pointer"));
        }
        config.ring_capacity_chunks = usize::try_from(value.ring_capacity_chunks.read())
            .ok()
            .filter(|v| *v > 0)
            .ok_or_else(|| invalid("ring capacity must be positive and fit size_t"))?;
    }
    Ok(())
}

fn validate(config: &StreamConfig) -> Result<(), i32> {
    if !config.gain.is_finite() || config.gain < 0.0 {
        return Err(invalid("gain must be finite and nonnegative"));
    }
    if config.kind == flexaudio::SourceKind::ProcessLoopback && config.target_pid.is_none() {
        return Err(invalid("process capture requires process_id"));
    }
    if config.kind == flexaudio::SourceKind::Mix
        && [config.mix_mic_gain, config.mix_system_gain]
            .iter()
            .any(|v| !v.is_finite() || *v < 0.0)
    {
        return Err(invalid("mix gains must be finite and nonnegative"));
    }
    Ok(())
}

fn preflight(config: &StreamConfig, options: &OpenOptions) -> Result<(), i32> {
    validate(config)?;
    let request = SettingsRequest::new(config.clone()).open_options(options);
    let report = flexaudio::resolve_settings(&request).map_err(error::set_audio_error)?;
    if options.strict && report.has_ignored() {
        return Err(error::set_audio_error(flexaudio::Error::IgnoredSettings {
            report,
        }));
    }
    Ok(())
}

/// Open using exact-size v4/v3 envelopes. Strict preflight precedes native/model work.
/// out is NULL on failure. Options and all borrowed configuration are copied.
/// # Safety
/// Envelopes have readable header words and full storage only when those match.
/// Their fields are readable for this call, and out is writable.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_open_v4(
    config: *const FlexStreamConfigV4,
    out: *mut *mut FlexStream,
) -> i32 {
    open_with(config, out, flexaudio::open_with_options)
}
unsafe fn open_with(
    config: *const FlexStreamConfigV4,
    out: *mut *mut FlexStream,
    open: impl FnOnce(StreamConfig, OpenOptions) -> flexaudio::Result<flexaudio::Stream>,
) -> i32 {
    guard_i32(|| {
        error::clear_last_error();
        if let Err(status) = output(out) {
            return status;
        }
        let prepared = (|| {
            let envelope = envelope(config)?;
            let base = v3::envelope(envelope.base)?;
            let mut config = v3::config(base.config, base.exclude_pids, base.exclude_pids_len)?;
            let mut options = v3::options(
                base.windows_classic_loopback_keepalive,
                &OpenOptions::default(),
            )?;
            controls(envelope.options, &mut config, &mut options)?;
            let secondary = v3::secondary(base.secondary_output)?;
            config.secondary_output = secondary.map(|(format, _)| format);
            let vad = validate_vad(base.vad, secondary.is_some())?;
            let denoise = (*base.config).denoise != 0;
            let encoding = match secondary {
                Some((_, SecondaryEncoding::S16)) => "s16",
                _ => "f32",
            };
            options = options.settings_attachments(
                attachments(&*base.config, base.windows_classic_loopback_keepalive)
                    .vad(vad.is_some())
                    .secondary_encoding(encoding),
            );
            preflight(&config, &options)?;
            Ok::<_, i32>(PreparedOpen {
                config,
                options,
                secondary,
                vad,
                denoise,
            })
        })();
        match prepared {
            Ok(prepared) => finish_open(prepared, out, open),
            Err(status) => status,
        }
    })
}

/// Switch source with retained strictness unless explicitly replaced. Gain, capacity,
/// denoise, VAD and output formats retain their open-time configuration.
/// Failure preserves the previous source, options and settings report.
/// # Safety
/// stream is live/exclusively borrowed; envelopes and fields obey open_v4's requirements.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_switch_source_v4(
    stream: *mut FlexStream,
    config: *const FlexSwitchConfigV4,
) -> i32 {
    guard_i32(|| {
        error::clear_last_error();
        if !valid_pointer(stream) {
            return invalid("invalid stream handle");
        }
        let prepared = (|| {
            let envelope = envelope(config)?;
            let base = v3::envelope(envelope.base)?;
            let mut config = v3::config(base.config, base.exclude_pids, base.exclude_pids_len)?;
            let stream = &*stream;
            config.secondary_output = stream.inner.config().secondary_output;
            if config.output != stream.inner.config().output {
                return Err(invalid("output format cannot change during switch_source"));
            }
            let mut options = v3::options(
                base.windows_classic_loopback_keepalive,
                stream.inner.open_options(),
            )?;
            controls(envelope.options, &mut config, &mut options)?;
            options = options.settings_attachments(
                attachments(&*base.config, base.windows_classic_loopback_keepalive)
                    .omitted_on_switch([K::SecondaryOutput, K::SecondaryEncoding, K::Vad]),
            );
            // The facade preflights switches before backend construction, using the
            // actual retained gain/denoise values, then commits options atomically.
            Ok::<_, i32>((config, options))
        })();
        let (config, options) = match prepared {
            Ok(value) => value,
            Err(status) => return status,
        };
        match (*stream).inner.switch_source_with_options(config, options) {
            Ok(()) => 0,
            Err(error) => error::set_audio_error(error),
        }
    })
}

#[cfg(test)]
#[path = "../tests/ffi/v4.rs"]
mod tests;
