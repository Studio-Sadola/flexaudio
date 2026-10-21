//! Legacy entry points inherit strict defaults without changing C sentinel decoding.
use super::*;
use std::{mem, ptr};

fn v2(base: &FlexConfig) -> vad_integration::FlexStreamConfigV2 {
    vad_integration::FlexStreamConfigV2 {
        size: mem::size_of::<vad_integration::FlexStreamConfigV2>()
            .try_into()
            .unwrap(),
        version: 2,
        config: base,
        vad: ptr::null(),
    }
}

fn v3(base: &FlexConfig) -> v3_types::FlexStreamConfigV3 {
    v3_types::FlexStreamConfigV3 {
        size: mem::size_of::<v3_types::FlexStreamConfigV3>()
            .try_into()
            .unwrap(),
        version: 3,
        config: base,
        vad: ptr::null(),
        exclude_pids: ptr::null(),
        exclude_pids_len: 0,
        secondary_output: ptr::null(),
        windows_classic_loopback_keepalive: 0,
    }
}

#[test]
fn legacy_opens_reject_ignored_nondefaults_with_named_reports() {
    unsafe {
        let mut base: FlexConfig = mem::zeroed();
        base.exclude_self = 1;
        for version in 1..=3 {
            let mut out = ptr::dangling_mut();
            let status = match version {
                1 => {
                    out = flexaudio_open(&base);
                    error::root_code(&error::last_audio_error().unwrap())
                }
                2 => vad_integration::flexaudio_open_v2(&v2(&base), &mut out),
                3 => v3::flexaudio_open_v3(&v3(&base), &mut out),
                _ => unreachable!(),
            };
            assert_eq!(status, error::code::FLEX_IGNORED_SETTINGS, "v{version}");
            assert!(out.is_null());
            let error = error::last_audio_error().unwrap();
            let report = error.ignored_settings().unwrap();
            assert!(matches!(
                report
                    .get(flexaudio::SettingKey::ExcludeSelf)
                    .unwrap()
                    .outcome,
                flexaudio::SettingOutcome::Ignored(_)
            ));
            assert_eq!(
                report
                    .get(flexaudio::SettingKey::SilenceProbe)
                    .unwrap()
                    .outcome,
                flexaudio::SettingOutcome::NotRequested
            );
        }
        assert!(flexaudio_open_with_exclude_pids(&base, ptr::null(), 0).is_null());
        assert_eq!(
            error::last_audio_error().unwrap().kind(),
            flexaudio::ErrorKind::IgnoredSettings
        );
    }
}

fn mock_open(
    config: flexaudio::StreamConfig,
    options: flexaudio::OpenOptions,
) -> flexaudio::Result<flexaudio::Stream> {
    assert!(options.strict);
    assert!(config.silence_probe);
    let report = flexaudio::resolve_settings(
        &flexaudio::SettingsRequest::new(config.clone()).open_options(&options),
    )?;
    assert!(!report.has_ignored());
    flexaudio::Stream::open(
        config,
        Box::new(flexaudio::MockBackend::new(48_000, 2, 0.0)),
    )
}

#[test]
fn legacy_zeroed_microphone_configs_open_with_strict_defaults() {
    unsafe {
        let base: FlexConfig = mem::zeroed();
        let stream = open_with_exclude_pids_options(&base, ptr::null(), 0, mock_open);
        assert!(!stream.is_null());
        flexaudio_free(stream);
        let mut stream = ptr::null_mut();
        assert_eq!(
            vad_integration::open_v2_with(&v2(&base), &mut stream, mock_open),
            0
        );
        assert!(!stream.is_null());
        flexaudio_free(stream);
        assert_eq!(v3::open_with(&v3(&base), &mut stream, mock_open), 0);
        assert!(!stream.is_null());
        flexaudio_free(stream);
    }
}
