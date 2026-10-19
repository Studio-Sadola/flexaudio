//! Capture envelopes using shared boundary validation and canonical processing.
use crate::{
    boundary::{invalid, output},
    error::{self, code},
    guard_i32,
    secondary::SecondaryState,
    types::FlexStream,
    v3_types::*,
    v3_validation as validation,
    vad_integration::{reject_tap, validate_vad},
};

/// Open an unstarted stream with optional VAD, PID exclusions and secondary output.
/// Denoise runs once on canonical capture, before both output formats and VAD.
/// Valid out initializes to NULL, including on errors. All options are copied.
/// # Safety
/// Config has at least its size/version words, and complete readable storage when
/// they match. Borrowed fields must be readable for the call; out must be writable.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_open_v3(
    config: *const FlexStreamConfigV3,
    out: *mut *mut FlexStream,
) -> i32 {
    open_with(config, out, flexaudio::open_with_options)
}

unsafe fn open_with(
    config: *const FlexStreamConfigV3,
    out: *mut *mut FlexStream,
    open: impl FnOnce(
        flexaudio::StreamConfig,
        flexaudio::OpenOptions,
    ) -> flexaudio::Result<flexaudio::Stream>,
) -> i32 {
    guard_i32(|| {
        error::clear_last_error();
        if let Err(status) = output(out) {
            return status;
        }
        let prepared = (|| {
            let envelope = validation::envelope(config)?;
            let mut config = validation::config(
                envelope.config,
                envelope.exclude_pids,
                envelope.exclude_pids_len,
            )?;
            let options = validation::options(
                envelope.windows_classic_loopback_keepalive,
                &flexaudio::OpenOptions::default(),
            )?;
            let secondary = validation::secondary(envelope.secondary_output)?;
            config.secondary_output = secondary.map(|(format, _)| format);
            let vad = validate_vad(envelope.vad, secondary.is_some())?;
            let encoding = match secondary {
                Some((_, SecondaryEncoding::S16)) => "s16",
                _ => "f32",
            };
            let options = options.settings_attachments(
                crate::settings_request::attachments(
                    &*envelope.config,
                    envelope.windows_classic_loopback_keepalive,
                )
                .vad(vad.is_some())
                .secondary_encoding(encoding),
            );
            Ok::<_, i32>((
                config,
                options,
                secondary,
                vad,
                (*envelope.config).denoise != 0,
            ))
        })();
        let (config, options, secondary, vad, denoise) = match prepared {
            Ok(value) => value,
            Err(status) => return status,
        };
        finish_open(
            PreparedOpen {
                config,
                options,
                secondary,
                vad,
                denoise,
            },
            out,
            open,
        )
    })
}

pub(crate) struct PreparedOpen {
    pub config: flexaudio::StreamConfig,
    pub options: flexaudio::OpenOptions,
    pub secondary: Option<(flexaudio::OutputFormat, SecondaryEncoding)>,
    pub vad: Option<crate::vad_integration::ValidatedVad>,
    pub denoise: bool,
}

pub(crate) unsafe fn finish_open(
    prepared: PreparedOpen,
    out: *mut *mut FlexStream,
    open: impl FnOnce(
        flexaudio::StreamConfig,
        flexaudio::OpenOptions,
    ) -> flexaudio::Result<flexaudio::Stream>,
) -> i32 {
    let PreparedOpen {
        config,
        options,
        secondary,
        vad,
        denoise,
    } = prepared;
    let vad_secondary = vad.as_ref().is_some_and(|(_, _, secondary)| *secondary);
    // Every caller-controlled discriminator and count is checked before model loading.
    let vad_tap = match vad
        .map(|(params, runtime, _)| flexaudio_vad::VadTap::new(params, runtime))
        .transpose()
    {
        Ok(value) => value,
        Err(error) => return reject_tap(error),
    };
    let mut inner = match open(config, options) {
        Ok(value) => value,
        Err(error) => return error::set_audio_error(error),
    };
    inner.set_denoise(denoise);
    if vad_tap.is_some() {
        if let Err(error) = inner.enable_capture_tap() {
            return error::set_audio_error(error);
        }
    }
    out.write(Box::into_raw(Box::new(FlexStream {
        events: Default::default(),
        secondary: secondary
            .map(|(format, encoding)| SecondaryState::new(format, encoding, vad_secondary)),
        inner,
        shutdown: None,
        shutdown_event_index: 0,
        vad_tap,
        vad_events: Vec::new(),
        vad_origin: (0, 0),
        vad_error: None,
        vad_cleanup: Vec::new(),
        ready_chunks: Default::default(),
    })));
    code::FLEX_OK
}

/// Switch source and optionally replace keepalive. Zero retains the current option.
/// Output formats, attached VAD, gain and denoise retain their open-time settings.
/// Failure retains the old options. No model is created by this operation.
/// # Safety
/// Stream must be live and exclusively borrowed. Config's header and, on a match,
/// complete envelope and borrowed fields must be readable during this call.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_switch_source_v3(
    stream: *mut FlexStream,
    config: *const FlexSwitchConfigV3,
) -> i32 {
    guard_i32(|| {
        error::clear_last_error();
        if !crate::convert::valid_pointer(stream) {
            return invalid("invalid stream handle");
        }
        let prepared = (|| {
            let envelope = validation::envelope(config)?;
            let mut config = validation::config(
                envelope.config,
                envelope.exclude_pids,
                envelope.exclude_pids_len,
            )?;
            let stream = &mut *stream;
            config.secondary_output = stream.inner.config().secondary_output;
            let options = validation::options(
                envelope.windows_classic_loopback_keepalive,
                stream.inner.open_options(),
            )?;
            let options = options.settings_attachments(
                crate::settings_request::attachments(
                    &*envelope.config,
                    envelope.windows_classic_loopback_keepalive,
                )
                .omitted_on_switch([
                    flexaudio::SettingKey::Vad,
                    flexaudio::SettingKey::SecondaryOutput,
                    flexaudio::SettingKey::SecondaryEncoding,
                ]),
            );
            Ok::<_, i32>((config, options))
        })();
        let (config, options) = match prepared {
            Ok(value) => value,
            Err(status) => return status,
        };
        match (*stream).inner.switch_source_with_options(config, options) {
            Ok(()) => code::FLEX_OK,
            Err(error) => error::set_audio_error(error),
        }
    })
}

#[cfg(test)]
#[path = "../tests/ffi/v3.rs"]
mod tests;
