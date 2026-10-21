//! Version-four envelopes and pure settings preflight before native/model work.
use crate::{
    boundary::{invalid, output},
    convert::valid_pointer,
    error, guard_i32,
    settings_request::attachments,
    types::{FlexConfig, FlexStream},
    v3::{finish_open, PreparedOpen},
    v3_types::*,
    v3_validation as v3,
    v4_types::*,
    vad_integration::validate_vad,
};
use flexaudio::{OpenOptions, SettingKey as K, SettingsRequest, StreamConfig};

/// Literal gains replace the three legacy sentinel fields only for v5.
pub(crate) struct LiteralGains {
    pub gain: f32,
    pub mix_mic_gain: f32,
    pub mix_system_gain: f32,
}

fn apply_gains(config: &mut StreamConfig, gains: &Option<LiteralGains>) {
    if let Some(gains) = gains {
        config.gain = gains.gain;
        config.mix_mic_gain = gains.mix_mic_gain;
        config.mix_system_gain = gains.mix_system_gain;
    }
}

unsafe fn decode_config(
    base: *const FlexConfig,
    pids: *const u32,
    len: usize,
    gains: &Option<LiteralGains>,
) -> Result<StreamConfig, i32> {
    if gains.is_some() {
        if !valid_pointer(base) {
            return Err(invalid("invalid base config pointer"));
        }
        // The caller guarantees readable storage after the structural pointer check.
        let base = &*base;
        for (field, value) in [
            ("gain", base.gain),
            ("mix_mic_gain", base.mix_mic_gain),
            ("mix_system_gain", base.mix_system_gain),
        ] {
            if value != 0.0 {
                return Err(invalid(&format!(
                    "FlexConfig.{field} must be zero in v5; use FlexGainsV5.{field}"
                )));
            }
        }
    }
    let mut config = v3::config(base, pids, len)?;
    apply_gains(&mut config, gains);
    Ok(config)
}

fn settings_attachments(
    base: &crate::types::FlexConfig,
    keepalive: u32,
    gains: &Option<LiteralGains>,
) -> flexaudio::SettingsAttachments {
    if gains.is_some() {
        crate::settings_request::literal_gain_attachments(base, keepalive)
    } else {
        attachments(base, keepalive)
    }
}

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
pub(crate) unsafe fn open_with(
    config: *const FlexStreamConfigV4,
    out: *mut *mut FlexStream,
    open: impl FnOnce(StreamConfig, OpenOptions) -> flexaudio::Result<flexaudio::Stream>,
) -> i32 {
    open_shared(|| Ok((config, None)), out, open)
}

pub(crate) unsafe fn open_shared(
    decode: impl FnOnce() -> Result<(*const FlexStreamConfigV4, Option<LiteralGains>), i32>,
    out: *mut *mut FlexStream,
    open: impl FnOnce(StreamConfig, OpenOptions) -> flexaudio::Result<flexaudio::Stream>,
) -> i32 {
    guard_i32(|| {
        error::clear_last_error();
        if let Err(status) = output(out) {
            return status;
        }
        let prepared = (|| {
            let (config, gains) = decode()?;
            let envelope = envelope(config)?;
            let base = v3::envelope(envelope.base)?;
            let mut config = decode_config(
                base.config,
                base.exclude_pids,
                base.exclude_pids_len,
                &gains,
            )?;
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
                settings_attachments(
                    &*base.config,
                    base.windows_classic_loopback_keepalive,
                    &gains,
                )
                .vad(vad.is_some())
                .secondary_encoding(encoding),
            );
            // v5 delegates both argument validation and applicability to core.
            // Legacy entry points retain their existing preflight and ordering.
            let core_first = gains.is_some();
            if !core_first {
                preflight(&config, &options)?;
            }
            Ok::<_, i32>((
                PreparedOpen {
                    config,
                    options,
                    secondary,
                    vad,
                    denoise,
                },
                core_first,
            ))
        })();
        match prepared {
            Ok((prepared, true)) => {
                // Core's open_config/source_config and strict preflight must run
                // before model loading. The already opened stream is then attached
                // through the same finishing path as every other C entry point.
                match open(prepared.config.clone(), prepared.options.clone()) {
                    Ok(inner) => finish_open(prepared, out, |_, _| Ok(inner)),
                    Err(error) => error::set_audio_error(error),
                }
            }
            Ok((prepared, false)) => finish_open(prepared, out, open),
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
    switch_shared(stream, || Ok((config, None)))
}

pub(crate) unsafe fn switch_shared(
    stream: *mut FlexStream,
    decode: impl FnOnce() -> Result<(*const FlexSwitchConfigV4, Option<LiteralGains>), i32>,
) -> i32 {
    switch_shared_with(
        stream,
        decode,
        flexaudio::Stream::switch_source_with_options,
    )
}

pub(crate) unsafe fn switch_shared_with(
    stream: *mut FlexStream,
    decode: impl FnOnce() -> Result<(*const FlexSwitchConfigV4, Option<LiteralGains>), i32>,
    switch: impl FnOnce(&mut flexaudio::Stream, StreamConfig, OpenOptions) -> flexaudio::Result<()>,
) -> i32 {
    guard_i32(|| {
        error::clear_last_error();
        if !valid_pointer(stream) {
            return invalid("invalid stream handle");
        }
        let prepared = (|| {
            let (config, gains) = decode()?;
            let envelope = envelope(config)?;
            let base = v3::envelope(envelope.base)?;
            let mut config = decode_config(
                base.config,
                base.exclude_pids,
                base.exclude_pids_len,
                &gains,
            )?;
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
                settings_attachments(
                    &*base.config,
                    base.windows_classic_loopback_keepalive,
                    &gains,
                )
                .omitted_on_switch([
                    K::SecondaryOutput,
                    K::SecondaryEncoding,
                    K::Vad,
                ]),
            );
            // The facade preflights switches before backend construction, using the
            // actual retained gain/denoise values, then commits options atomically.
            Ok::<_, i32>((config, options))
        })();
        let (config, options) = match prepared {
            Ok(value) => value,
            Err(status) => return status,
        };
        match switch(&mut (*stream).inner, config, options) {
            Ok(()) => 0,
            Err(error) => error::set_audio_error(error),
        }
    })
}

#[cfg(test)]
#[path = "../tests/ffi/v4.rs"]
mod tests;
