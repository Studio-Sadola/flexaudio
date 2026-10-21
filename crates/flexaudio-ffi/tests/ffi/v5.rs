//! Offline v5 gain presence, PCM, validation and shared settings contracts.
use super::*;
use crate::{
    error::{self, code},
    types::FlexConfig,
    v3_types::*,
    v4_types::*,
};
use flexaudio::{SettingKey as K, SettingOutcome, SettingValue};
use std::{
    mem, ptr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

type Sink = Arc<Mutex<Option<flexaudio::core::backend::RawSink>>>;
struct Input(Sink);
impl flexaudio::CaptureBackend for Input {
    fn native_format(&self) -> (u32, u16) {
        (48_000, 2)
    }
    fn start(&mut self, sink: flexaudio::core::backend::RawSink) -> flexaudio::Result<()> {
        *self.0.lock().unwrap() = Some(sink);
        Ok(())
    }
    fn stop(&mut self) {
        self.0.lock().unwrap().take();
    }
}

fn base3(base: &FlexConfig) -> FlexStreamConfigV3 {
    FlexStreamConfigV3 {
        size: mem::size_of::<FlexStreamConfigV3>().try_into().unwrap(),
        version: 3,
        config: base,
        vad: ptr::null(),
        exclude_pids: ptr::null(),
        exclude_pids_len: 0,
        secondary_output: ptr::null(),
        windows_classic_loopback_keepalive: 0,
    }
}
fn controls(strict: u32) -> FlexOptionsV4 {
    FlexOptionsV4 {
        size: mem::size_of::<FlexOptionsV4>().try_into().unwrap(),
        version: 4,
        strict,
        ring_capacity_chunks: ptr::null(),
    }
}
fn base4(base: &FlexStreamConfigV3, options: &FlexOptionsV4) -> FlexStreamConfigV4 {
    FlexStreamConfigV4 {
        size: mem::size_of::<FlexStreamConfigV4>().try_into().unwrap(),
        version: 4,
        base,
        options,
    }
}
fn gain_options(gain: *const f32) -> FlexGainsV5 {
    FlexGainsV5 {
        size: mem::size_of::<FlexGainsV5>().try_into().unwrap(),
        version: 5,
        gain,
        mix_mic_gain: ptr::null(),
        mix_system_gain: ptr::null(),
    }
}
fn request(base: &FlexStreamConfigV4, gains: *const FlexGainsV5) -> FlexStreamConfigV5 {
    FlexStreamConfigV5 {
        size: mem::size_of::<FlexStreamConfigV5>().try_into().unwrap(),
        version: 5,
        base,
        gains,
    }
}

unsafe fn pcm(config: &FlexStreamConfigV5, expected_gain: f32) -> Vec<f32> {
    let sink = Sink::default();
    let mut stream = ptr::null_mut();
    assert_eq!(
        open_with(config, &mut stream, |config, options| {
            assert_eq!(config.gain, expected_gain);
            let report = flexaudio::resolve_settings(
                &flexaudio::SettingsRequest::new(config.clone()).open_options(&options),
            )?;
            let gain = report.get(K::Gain).unwrap();
            assert_eq!(gain.requested, SettingValue::Float(expected_gain));
            assert_eq!(gain.effective, SettingValue::Float(expected_gain));
            assert_eq!(
                gain.outcome,
                if expected_gain == 1.0 {
                    SettingOutcome::NotRequested
                } else {
                    SettingOutcome::Applied
                }
            );
            flexaudio::test_support::open(config, options, Box::new(Input(sink.clone())))
        }),
        code::FLEX_OK
    );
    assert_eq!(crate::flexaudio_start(stream), 0);
    // Nonzero, in-range input prevents input silence or startup suppression from
    // making an incorrectly decoded zero gain appear to mute successfully.
    let input = vec![0.25; 1920];
    assert_eq!(
        sink.lock().unwrap().as_mut().unwrap().push(&input, 0),
        input.len()
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut chunk = mem::zeroed();
    loop {
        let status = crate::flexaudio_poll_chunk(stream, &mut chunk);
        assert!(status >= 0);
        if status == 1 {
            break;
        }
        assert!(Instant::now() < deadline, "mock PCM timed out");
        std::thread::sleep(Duration::from_millis(1));
    }
    let samples = std::slice::from_raw_parts(chunk.data, chunk.len).to_vec();
    assert_eq!(samples.len(), input.len());
    assert!(samples.iter().all(|sample| *sample == 0.25 * expected_gain));
    crate::flexaudio_chunk_free(&mut chunk);
    crate::flexaudio_free(stream);
    samples
}

#[test]
fn literal_zero_mutes_omission_is_unity_half_scales_and_v4_keeps_its_sentinel() {
    // SAFETY: all nested envelopes and borrowed floats live throughout each call;
    // handles and returned chunks are exclusively owned and freed once.
    unsafe {
        let mut base: FlexConfig = mem::zeroed();
        let b3 = base3(&base);
        let options = controls(0);
        let b4 = base4(&b3, &options);
        let muted = pcm(&request(&b4, &gain_options(&0.0)), 0.0);
        let unity = pcm(&request(&b4, &gain_options(ptr::null())), 1.0);
        assert_eq!(pcm(&request(&b4, ptr::null()), 1.0), unity);
        let half = pcm(&request(&b4, &gain_options(&0.5)), 0.5);
        assert!(muted.iter().all(|v| *v == 0.0));
        assert!(unity.iter().any(|v| *v != 0.0));
        assert!(half
            .iter()
            .zip(&unity)
            .all(|(half, unity)| *half == unity * 0.5));

        base.gain = 0.0;
        let b3 = base3(&base);
        let b4 = base4(&b3, &options);
        let mut stream = ptr::null_mut();
        let sink = Sink::default();
        assert_eq!(
            v4::open_with(&b4, &mut stream, |config, _| {
                assert_eq!(config.gain, 1.0);
                flexaudio::Stream::open(config, Box::new(Input(sink.clone())))
            }),
            0
        );
        assert_eq!(crate::flexaudio_start(stream), 0);
        assert_eq!(
            sink.lock()
                .unwrap()
                .as_mut()
                .unwrap()
                .push(&[0.25; 1920], 0),
            1920
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut chunk = mem::zeroed();
        loop {
            let status = crate::flexaudio_poll_chunk(stream, &mut chunk);
            assert!(status >= 0);
            if status == 1 {
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(std::slice::from_raw_parts(chunk.data, chunk.len), unity);
        crate::flexaudio_chunk_free(&mut chunk);
        crate::flexaudio_free(stream);

        #[cfg(target_os = "linux")]
        {
            // Process backend construction is offline until start, so exercise the
            // production open/report API without opening any device or socket.
            base.kind = crate::FLEX_SOURCE_KIND_PROCESS;
            base.process_id = 17;
            let b3 = base3(&base);
            let b4 = base4(&b3, &options);
            let gains = gain_options(&0.0);
            let config = request(&b4, &gains);
            let mut stream = ptr::null_mut();
            assert_eq!(flexaudio_open_v5(&config, &mut stream), 0);
            let mut snapshot = ptr::null_mut();
            assert_eq!(
                crate::settings_v4::flexaudio_settings_v4(stream, &mut snapshot),
                0
            );
            let mut revision = 99;
            let mut count = 0;
            assert_eq!(
                crate::settings_v4::flexaudio_settings_count_v4(
                    snapshot,
                    &mut revision,
                    &mut count
                ),
                0
            );
            let mut found = false;
            for index in 0..count {
                let mut row = mem::MaybeUninit::uninit();
                assert_eq!(
                    crate::settings_v4::flexaudio_settings_item_v4(
                        snapshot,
                        index,
                        row.as_mut_ptr()
                    ),
                    0
                );
                let row = row.assume_init();
                if std::ffi::CStr::from_ptr(row.setting) == c"gain" {
                    found = true;
                    assert_eq!(row.status, FLEX_SETTING_APPLIED);
                    assert!(matches!(
                        row.requested.kind,
                        FlexValueKindV4::FLEX_VALUE_FLOAT
                    ));
                    assert!(matches!(
                        row.effective.kind,
                        FlexValueKindV4::FLEX_VALUE_FLOAT
                    ));
                    assert_eq!(row.requested.value.float_value, 0.0);
                    assert_eq!(row.effective.value.float_value, 0.0);
                }
            }
            assert!(found);
            crate::settings_v4::flexaudio_settings_free_v4(snapshot);
            crate::flexaudio_free(stream);
        }
    }
}

#[test]
fn strict_controls_report_literal_mix_zeros_and_invalid_gains_match_rust() {
    // SAFETY: locals provide aligned, readable envelopes/floats and writable outputs.
    unsafe {
        let base: FlexConfig = mem::zeroed();
        let b3 = base3(&base);
        let mut options = controls(0);
        let b4 = base4(&b3, &options);
        pcm(&request(&b4, &gain_options(&0.0)), 0.0);
        // On a microphone the mix gains are ignored non-default requests. Both
        // zero-valued lanes must be reported, rather than decoded as sentinels.
        let mut gains = gain_options(&0.0);
        gains.mix_mic_gain = &0.0;
        gains.mix_system_gain = &0.0;
        for strict in [0, 1, 2] {
            options.strict = strict;
            let b4 = base4(&b3, &options);
            let config = request(&b4, &gains);
            let mut stream = ptr::dangling_mut();
            let status = open_with(&config, &mut stream, |config, options| {
                assert_eq!(options.strict, strict != 2);
                assert_eq!(
                    (config.gain, config.mix_mic_gain, config.mix_system_gain),
                    (0.0, 0.0, 0.0)
                );
                flexaudio::test_support::open(config, options, Box::new(Input(Sink::default())))
            });
            if strict == 2 {
                assert_eq!(status, 0);
                crate::flexaudio_free(stream);
            } else {
                assert_eq!(status, code::FLEX_IGNORED_SETTINGS);
                assert!(stream.is_null());
                let error = error::last_audio_error().unwrap();
                let report = error.ignored_settings().unwrap();
                assert_eq!(
                    report.get(K::Gain).unwrap().effective,
                    SettingValue::Float(0.0)
                );
                for key in [K::MixMicGain, K::MixSystemGain] {
                    let row = report.get(key).unwrap();
                    assert_eq!(row.requested, SettingValue::Float(0.0));
                    assert!(matches!(row.outcome, SettingOutcome::Ignored(_)));
                }
            }
        }
        options.strict = 0;
        let b4 = base4(&b3, &options);
        for invalid in [f32::NAN, -0.5, -1.0, f32::INFINITY] {
            let mut config = flexaudio::StreamConfig::new(flexaudio::SourceKind::Mic);
            config.gain = invalid;
            let rust = flexaudio::Stream::open(config, Box::new(Input(Sink::default())))
                .err()
                .unwrap();
            let mut stream = ptr::dangling_mut();
            let reached_core = std::cell::Cell::new(false);
            assert_eq!(
                open_with(
                    &request(&b4, &gain_options(&invalid)),
                    &mut stream,
                    |config, options| {
                        reached_core.set(true);
                        flexaudio::test_support::open(
                            config,
                            options,
                            Box::new(Input(Sink::default())),
                        )
                    }
                ),
                code::FLEX_INVALID_ARG
            );
            assert!(stream.is_null());
            assert!(
                reached_core.get(),
                "open gain validation must belong to core"
            );
            assert_eq!(error::last_audio_error().unwrap().kind(), rust.kind());
        }
        // Mix accepts literal zero lanes under strict mode; omission is unity.
        let mut base: FlexConfig = mem::zeroed();
        base.kind = crate::FLEX_SOURCE_KIND_MIX;
        let b3 = base3(&base);
        let b4 = base4(&b3, &options);
        for lane in [0, 1] {
            for value in [
                None,
                Some(0.0),
                Some(0.5),
                Some(f32::NAN),
                Some(-1.0),
                Some(f32::INFINITY),
            ] {
                let mut gains = gain_options(&0.0);
                let pointer = value.as_ref().map_or(ptr::null(), ptr::from_ref);
                if lane == 0 {
                    gains.mix_mic_gain = pointer;
                } else {
                    gains.mix_system_gain = pointer;
                }
                let mut stream = ptr::null_mut();
                let status = open_with(&request(&b4, &gains), &mut stream, |config, options| {
                    let inner = flexaudio::test_support::open(
                        config.clone(),
                        options.clone(),
                        Box::new(Input(Sink::default())),
                    )?;
                    let expected = value.unwrap_or(1.0);
                    assert_eq!(
                        if lane == 0 {
                            config.mix_mic_gain
                        } else {
                            config.mix_system_gain
                        },
                        expected
                    );
                    let report = flexaudio::resolve_settings(
                        &flexaudio::SettingsRequest::new(config.clone()).open_options(&options),
                    )?;
                    let row = report
                        .get(if lane == 0 {
                            K::MixMicGain
                        } else {
                            K::MixSystemGain
                        })
                        .unwrap();
                    assert_eq!(row.effective, SettingValue::Float(expected));
                    assert_eq!(
                        row.outcome,
                        if expected == 1.0 {
                            SettingOutcome::NotRequested
                        } else {
                            SettingOutcome::Applied
                        }
                    );
                    Ok(inner)
                });
                if value.is_some_and(|v| !v.is_finite() || v < 0.0) {
                    assert_eq!(status, code::FLEX_INVALID_ARG);
                    assert!(stream.is_null());
                    let mut config = flexaudio::StreamConfig::new(flexaudio::SourceKind::Mix);
                    if lane == 0 {
                        config.mix_mic_gain = value.unwrap();
                    } else {
                        config.mix_system_gain = value.unwrap();
                    }
                    let rust =
                        flexaudio::open_with_options(config, flexaudio::OpenOptions::default())
                            .err()
                            .unwrap();
                    assert_eq!(error::last_audio_error().unwrap().kind(), rust.kind());
                } else {
                    assert_eq!(status, 0);
                    crate::flexaudio_free(stream);
                }
            }
        }
    }
}

fn assert_same_row(actual: &flexaudio::SettingResolution, expected: &flexaudio::SettingResolution) {
    assert_eq!(actual.setting, expected.setting);
    assert_eq!(actual.outcome, expected.outcome);
    for (actual, expected) in [
        (&actual.requested, &expected.requested),
        (&actual.effective, &expected.effective),
    ] {
        match (actual, expected) {
            // NaN is not equal to itself. Comparing bits also detects accidental
            // clamping or replacement of a literal at the binding boundary.
            (SettingValue::Float(actual), SettingValue::Float(expected)) => {
                assert_eq!(actual.to_bits(), expected.to_bits());
            }
            _ => assert_eq!(actual, expected),
        }
    }
}

#[test]
fn switch_nonfinite_and_negative_output_gain_follow_core_fixed_at_open() {
    // SAFETY: all borrowed envelopes and floats are live and aligned; both started
    // streams are exclusively owned. Only backend construction is replaced in the
    // permissive case; production core preflight and switch commit still execute.
    unsafe {
        for value in [f32::NAN, -1.0] {
            for strict in [0, 1, 2] {
                let base: FlexConfig = mem::zeroed();
                let b3 = base3(&base);
                let opening_options = controls(0);
                let b4 = base4(&b3, &opening_options);
                let mut stream = ptr::null_mut();
                assert_eq!(
                    open_with(
                        &request(&b4, &gain_options(&0.5)),
                        &mut stream,
                        |config, options| flexaudio::test_support::open(
                            config,
                            options,
                            Box::new(Input(Sink::default()))
                        )
                    ),
                    0
                );
                let mut rust_config = flexaudio::StreamConfig::new(flexaudio::SourceKind::Mic);
                rust_config.gain = 0.5;
                let mut rust = flexaudio::test_support::open(
                    rust_config,
                    flexaudio::OpenOptions::default(),
                    Box::new(Input(Sink::default())),
                )
                .unwrap();
                assert_eq!(crate::flexaudio_start(stream), 0);
                rust.start().unwrap();
                let switch3 = FlexSwitchConfigV3 {
                    size: mem::size_of::<FlexSwitchConfigV3>().try_into().unwrap(),
                    version: 3,
                    config: &base,
                    exclude_pids: ptr::null(),
                    exclude_pids_len: 0,
                    windows_classic_loopback_keepalive: 0,
                };
                let options = controls(strict);
                let switch4 = FlexSwitchConfigV4 {
                    size: mem::size_of::<FlexSwitchConfigV4>().try_into().unwrap(),
                    version: 4,
                    base: &switch3,
                    options: &options,
                };
                let gains = gain_options(&value);
                let config = FlexSwitchConfigV5 {
                    size: mem::size_of::<FlexSwitchConfigV5>().try_into().unwrap(),
                    version: 5,
                    base: &switch4,
                    gains: &gains,
                };
                let status = if strict == 2 {
                    switch_with(stream, &config, |stream, config, options| {
                        assert_eq!(config.gain.to_bits(), value.to_bits());
                        assert!(!options.strict);
                        flexaudio::test_support::switch(
                            stream,
                            config,
                            options,
                            Box::new(Input(Sink::default())),
                        )
                    })
                } else {
                    // Rejection precedes native construction, so use the exported
                    // C entry point and public Rust switch directly in strict mode.
                    flexaudio_switch_source_v5(stream, &config)
                };
                let c_error = error::last_audio_error();
                let mut replacement = flexaudio::StreamConfig::new(flexaudio::SourceKind::Mic);
                replacement.gain = value;
                let rust_options = flexaudio::OpenOptions::default().strict(strict != 2);
                let result = if strict == 2 {
                    flexaudio::test_support::switch(
                        &mut rust,
                        replacement,
                        rust_options,
                        Box::new(Input(Sink::default())),
                    )
                } else {
                    rust.switch_source_with_options(replacement, rust_options)
                };
                let (actual, expected) = if strict == 2 {
                    assert_eq!(status, code::FLEX_OK);
                    result.unwrap();
                    assert!(c_error.is_none());
                    assert_eq!((*stream).inner.effective_settings().unwrap().revision, 1);
                    (
                        (*stream)
                            .inner
                            .effective_settings()
                            .unwrap()
                            .get(K::Gain)
                            .unwrap(),
                        rust.effective_settings().unwrap().get(K::Gain).unwrap(),
                    )
                } else {
                    assert_eq!(status, code::FLEX_IGNORED_SETTINGS);
                    let c_error = c_error.as_ref().unwrap();
                    let rust_error = result.err().unwrap();
                    assert_eq!(c_error.kind(), rust_error.kind());
                    let actual = c_error.ignored_settings().unwrap().get(K::Gain).unwrap();
                    let expected = rust_error.ignored_settings().unwrap().get(K::Gain).unwrap();
                    assert_same_row(actual, expected);
                    assert_eq!(actual.effective, SettingValue::Float(0.5));
                    assert!(
                        matches!(actual.outcome, SettingOutcome::Ignored(reason) if reason.as_str() == "fixedAtOpen")
                    );
                    assert_eq!((*stream).inner.effective_settings().unwrap().revision, 0);
                    assert_eq!((*stream).inner.gain(), 0.5);
                    assert_eq!(rust.gain(), 0.5);
                    crate::flexaudio_free(stream);
                    continue;
                };
                assert_same_row(actual, expected);
                assert_eq!(actual.effective, SettingValue::Float(0.5));
                assert!(
                    matches!(actual.outcome, SettingOutcome::Ignored(reason) if reason.as_str() == "fixedAtOpen")
                );
                assert_eq!((*stream).inner.config().gain, 0.5);
                assert_eq!(rust.config().gain, 0.5);
                assert_eq!((*stream).inner.gain(), 0.5);
                assert_eq!(rust.gain(), 0.5);
                crate::flexaudio_free(stream);
            }
        }
    }
}

#[test]
fn microphone_invalid_mix_gains_are_reported_like_rust_in_both_strict_modes() {
    // SAFETY: live aligned envelopes and float storage are borrowed for each call;
    // owned handles are freed once. Injected backends use full core open validation.
    unsafe {
        for key in [K::MixMicGain, K::MixSystemGain] {
            for value in [f32::NAN, -1.0] {
                for strict in [0, 1, 2] {
                    let base: FlexConfig = mem::zeroed();
                    let b3 = base3(&base);
                    let options = controls(strict);
                    let b4 = base4(&b3, &options);
                    let mut gains = gain_options(&0.0);
                    let mut rust_config = flexaudio::StreamConfig::new(flexaudio::SourceKind::Mic);
                    rust_config.gain = 0.0;
                    if key == K::MixMicGain {
                        gains.mix_mic_gain = &value;
                        rust_config.mix_mic_gain = value;
                    } else {
                        gains.mix_system_gain = &value;
                        rust_config.mix_system_gain = value;
                    }
                    let rust = flexaudio::test_support::open(
                        rust_config,
                        flexaudio::OpenOptions::default().strict(strict != 2),
                        Box::new(Input(Sink::default())),
                    );
                    let mut stream = ptr::dangling_mut();
                    let status =
                        open_with(&request(&b4, &gains), &mut stream, |config, options| {
                            flexaudio::test_support::open(
                                config,
                                options,
                                Box::new(Input(Sink::default())),
                            )
                        });
                    let actual = if let Ok(rust) = &rust {
                        assert_eq!(status, code::FLEX_OK);
                        assert!(!stream.is_null());
                        assert_eq!((*stream).inner.config().gain, 0.0);
                        let actual = (*stream)
                            .inner
                            .effective_settings()
                            .unwrap()
                            .get(key)
                            .unwrap()
                            .clone();
                        assert_same_row(
                            &actual,
                            rust.effective_settings().unwrap().get(key).unwrap(),
                        );
                        crate::flexaudio_free(stream);
                        actual
                    } else {
                        let rust_error = rust.as_ref().err().unwrap();
                        assert_eq!(status, error::root_code(rust_error));
                        assert_eq!(status, code::FLEX_IGNORED_SETTINGS);
                        assert!(stream.is_null());
                        let c_error = error::last_audio_error().unwrap();
                        let actual = c_error
                            .ignored_settings()
                            .unwrap()
                            .get(key)
                            .unwrap()
                            .clone();
                        assert_same_row(
                            &actual,
                            rust_error.ignored_settings().unwrap().get(key).unwrap(),
                        );
                        actual
                    };
                    assert!(
                        matches!(actual.outcome, SettingOutcome::Ignored(reason) if reason.as_str() == "wrongSourceKind")
                    );
                    assert_eq!(actual.effective, SettingValue::Absent);
                    assert!(
                        matches!(actual.requested, SettingValue::Float(v) if v.to_bits() == value.to_bits())
                    );
                }
            }
        }
    }
}

#[test]
fn switch_omission_retains_gain_and_literal_zero_is_fixed_at_open_like_rust() {
    // SAFETY: the stream is exclusively owned; all borrowed pointers remain live.
    unsafe {
        let base: FlexConfig = mem::zeroed();
        let b3 = base3(&base);
        let options = controls(0);
        let b4 = base4(&b3, &options);
        pcm(&request(&b4, &gain_options(&0.0)), 0.0);
        let mut stream = ptr::null_mut();
        assert_eq!(
            open_with(
                &request(&b4, &gain_options(&0.5)),
                &mut stream,
                |config, _| { flexaudio::Stream::open(config, Box::new(Input(Sink::default()))) }
            ),
            0
        );
        assert_eq!(crate::flexaudio_start(stream), 0);
        // Keep this entirely offline: an unrelated ignored request forces core
        // preflight to return its full candidate before native backend construction.
        let mut replacement: FlexConfig = mem::zeroed();
        replacement.exclude_self = 1;
        let b3 = FlexSwitchConfigV3 {
            size: mem::size_of::<FlexSwitchConfigV3>().try_into().unwrap(),
            version: 3,
            config: &replacement,
            exclude_pids: ptr::null(),
            exclude_pids_len: 0,
            windows_classic_loopback_keepalive: 0,
        };
        let mut options = controls(0);
        for strict in [0, 1] {
            options.strict = strict;
            let b4 = FlexSwitchConfigV4 {
                size: mem::size_of::<FlexSwitchConfigV4>().try_into().unwrap(),
                version: 4,
                base: &b3,
                options: &options,
            };
            for value in [None, Some(0.0), Some(0.25), Some(1.0)] {
                let gains = gain_options(value.as_ref().map_or(ptr::null(), ptr::from_ref));
                let config = FlexSwitchConfigV5 {
                    size: mem::size_of::<FlexSwitchConfigV5>().try_into().unwrap(),
                    version: 5,
                    base: &b4,
                    gains: &gains,
                };
                assert_eq!(
                    flexaudio_switch_source_v5(stream, &config),
                    code::FLEX_IGNORED_SETTINGS
                );
                let c_error = error::last_audio_error().unwrap();
                let c_report = c_error.ignored_settings().unwrap();
                let mut rust_config = flexaudio::StreamConfig::new(flexaudio::SourceKind::Mic);
                rust_config.exclude_self = true;
                rust_config.gain = value.unwrap_or(1.0);
                let rust_error = (*stream)
                    .inner
                    .switch_source_with_options(rust_config, flexaudio::OpenOptions::default())
                    .unwrap_err();
                assert_eq!(
                    c_report.get(K::Gain),
                    rust_error.ignored_settings().unwrap().get(K::Gain)
                );
                let row = c_report.get(K::Gain).unwrap();
                assert_eq!(row.requested, SettingValue::Float(value.unwrap_or(1.0)));
                assert_eq!(row.effective, SettingValue::Float(0.5));
                assert_eq!(
                    row.outcome == SettingOutcome::NotRequested,
                    value.is_none() || value == Some(1.0)
                );
                if let SettingOutcome::Ignored(reason) = row.outcome {
                    assert_eq!(reason.as_str(), "fixedAtOpen");
                }
                assert_eq!((*stream).inner.config().gain, 0.5);
                assert!((*stream).inner.effective_settings().is_none());
            }
        }
        crate::flexaudio_free(stream);
    }
}

#[test]
fn v5_envelopes_and_gain_pointers_fail_closed_before_native_access() {
    #[repr(C, align(8))]
    struct Header(u32, u32);
    // SAFETY: truncated envelopes intentionally expose only their initialized header
    // words; mismatched layouts must be rejected before following nested pointers.
    unsafe {
        let base: FlexConfig = mem::zeroed();
        let b3 = base3(&base);
        let options = controls(0);
        let b4 = base4(&b3, &options);
        pcm(&request(&b4, &gain_options(&0.0)), 0.0);
        let mut gains = gain_options(&0.0);
        gains.mix_system_gain = ptr::dangling::<u8>().cast();
        let mut config = request(&b4, &gains);
        let header = Header(8, 5);
        for pointer in [ptr::null(), ptr::from_ref(&header).cast(), &config] {
            let mut stream = ptr::dangling_mut();
            assert_eq!(
                open_with(pointer, &mut stream, |_, _| panic!(
                    "invalid pointer reached backend"
                )),
                code::FLEX_INVALID_ARG
            );
            assert!(stream.is_null());
        }
        gains.mix_system_gain = ptr::null();
        gains.version = 4;
        config.gains = &gains;
        let mut stream = ptr::dangling_mut();
        assert_eq!(
            flexaudio_open_v5(&config, &mut stream),
            code::FLEX_INVALID_ARG
        );
        assert!(stream.is_null());
        config.version = 4;
        assert_eq!(
            flexaudio_open_v5(&config, &mut stream),
            code::FLEX_INVALID_ARG
        );
        assert!(stream.is_null());
        assert_eq!(
            flexaudio_open_v5(ptr::null(), ptr::null_mut()),
            code::FLEX_INVALID_ARG
        );
        assert_eq!(
            flexaudio_switch_source_v5(ptr::null_mut(), ptr::null()),
            code::FLEX_INVALID_ARG
        );
        options_invalid_strict(&b3);
    }
}

unsafe fn options_invalid_strict(base: &FlexStreamConfigV3) {
    let options = controls(3);
    let b4 = base4(base, &options);
    let gains = gain_options(&0.0);
    let config = request(&b4, &gains);
    let mut stream = ptr::dangling_mut();
    assert_eq!(
        flexaudio_open_v5(&config, &mut stream),
        code::FLEX_INVALID_ARG
    );
    assert!(stream.is_null());
}

#[test]
fn legacy_gain_fields_are_rejected_on_open_and_switch_before_the_opener() {
    // SAFETY: all envelopes and floats have live aligned storage. The mock stream
    // is exclusively owned and unstarted, so reaching core switch would return
    // InvalidState rather than the field-specific InvalidArg required here.
    unsafe {
        let zeroed: FlexConfig = mem::zeroed();
        let b3 = base3(&zeroed);
        let options = controls(0);
        let b4 = base4(&b3, &options);
        let literal = gain_options(&0.0);
        pcm(&request(&b4, &literal), 0.0);
        let sink = Sink::default();
        let mut stream = ptr::null_mut();
        assert_eq!(
            open_with(&request(&b4, &literal), &mut stream, |config, _| {
                flexaudio::Stream::open(config, Box::new(Input(sink.clone())))
            }),
            code::FLEX_OK
        );
        for field in ["gain", "mix_mic_gain", "mix_system_gain"] {
            for value in [0.5, 1.0, -0.5, f32::NAN, f32::INFINITY] {
                let mut base: FlexConfig = mem::zeroed();
                match field {
                    "gain" => base.gain = value,
                    "mix_mic_gain" => base.mix_mic_gain = value,
                    "mix_system_gain" => base.mix_system_gain = value,
                    _ => unreachable!(),
                }
                let b3 = base3(&base);
                let switch3 = FlexSwitchConfigV3 {
                    size: mem::size_of::<FlexSwitchConfigV3>().try_into().unwrap(),
                    version: 3,
                    config: &base,
                    exclude_pids: ptr::null(),
                    exclude_pids_len: 0,
                    windows_classic_loopback_keepalive: 0,
                };
                // The rejection is unconditional, including permissive controls
                // and supplied literal gains that would otherwise mask the legacy value.
                for strict in [0, 1, 2] {
                    let options = controls(strict);
                    let b4 = base4(&b3, &options);
                    let switch4 = FlexSwitchConfigV4 {
                        size: mem::size_of::<FlexSwitchConfigV4>().try_into().unwrap(),
                        version: 4,
                        base: &switch3,
                        options: &options,
                    };
                    for gains in [ptr::null(), &literal] {
                        let config = request(&b4, gains);
                        let mut out = ptr::dangling_mut();
                        assert_eq!(
                            open_with(&config, &mut out, |_, _| panic!(
                                "nonzero legacy gain reached the opener"
                            )),
                            code::FLEX_INVALID_ARG,
                            "open {field}={value}, strict={strict}"
                        );
                        assert!(out.is_null());
                        let message = std::ffi::CStr::from_ptr(crate::flexaudio_last_error())
                            .to_str()
                            .unwrap();
                        assert!(
                            message.contains(&format!(
                                "FlexConfig.{field} must be zero in v5; use FlexGainsV5.{field}"
                            )),
                            "{message}"
                        );
                        let switch = FlexSwitchConfigV5 {
                            size: mem::size_of::<FlexSwitchConfigV5>().try_into().unwrap(),
                            version: 5,
                            base: &switch4,
                            gains,
                        };
                        assert_eq!(
                            flexaudio_switch_source_v5(stream, &switch),
                            code::FLEX_INVALID_ARG,
                            "switch {field}={value}, strict={strict}"
                        );
                        let message = std::ffi::CStr::from_ptr(crate::flexaudio_last_error())
                            .to_str()
                            .unwrap();
                        assert!(
                            message.contains(&format!(
                                "FlexConfig.{field} must be zero in v5; use FlexGainsV5.{field}"
                            )),
                            "{message}"
                        );
                        assert!(sink.lock().unwrap().is_none());
                        assert_eq!((*stream).inner.config().gain, 0.0);
                    }
                }
            }
        }
        crate::flexaudio_free(stream);
        // All three zeroed legacy fields permit explicit, independently chosen
        // v5 gains, including muted output and a muted system lane.
        let mut base: FlexConfig = mem::zeroed();
        base.kind = crate::FLEX_SOURCE_KIND_MIX;
        let b3 = base3(&base);
        let b4 = base4(&b3, &options);
        let mut literal = gain_options(&0.0);
        literal.mix_mic_gain = &0.5;
        literal.mix_system_gain = &0.0;
        let mut stream = ptr::null_mut();
        assert_eq!(
            open_with(&request(&b4, &literal), &mut stream, |config, _| {
                assert_eq!(config.gain, 0.0);
                assert_eq!(config.mix_mic_gain, 0.5);
                assert_eq!(config.mix_system_gain, 0.0);
                flexaudio::Stream::open(config, Box::new(Input(Sink::default())))
            }),
            code::FLEX_OK
        );
        crate::flexaudio_free(stream);
    }
}
