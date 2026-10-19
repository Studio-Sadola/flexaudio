//! Exhaustive static applicability checks without platform/device dependencies.

use crate::settings_resolver::*;
use crate::ProcessMode;
use crate::*;

fn resolve_settings(
    request: &SettingsRequest,
    platform: SettingsPlatform,
    self_pid: u32,
    previous: Option<&SettingsReport>,
) -> Result<SettingsReport> {
    let mut request = request.clone();
    request.previous = previous.cloned();
    resolve_for_platform(&request, platform, self_pid)
}

fn requested(r: &mut SettingsRequest, key: SettingKey) {
    match key {
        SettingKey::DeviceId => r.config.device_id = Some("selector".into()),
        SettingKey::TargetPid => r.config.target_pid = Some(42),
        SettingKey::Mode => r.config.mode = ProcessMode::Exclude,
        SettingKey::ExcludeSelf => r.config.exclude_self = true,
        SettingKey::ExcludePids => r.config.exclude_pids = vec![42],
        SettingKey::MixMicDeviceId => r.config.mix_mic_device_id = Some("input".into()),
        SettingKey::MixSystemDeviceId => r.config.mix_system_device_id = Some("output".into()),
        SettingKey::MixMicGain => r.config.mix_mic_gain = 2.0,
        SettingKey::MixSystemGain => r.config.mix_system_gain = 2.0,
        SettingKey::SilenceProbe => r.config.silence_probe = false,
        SettingKey::WindowsClassicLoopbackKeepalive => r.keepalive = Some(false),
        SettingKey::Gain => r.config.gain = 2.0,
        SettingKey::RingCapacityChunks => r.config.ring_capacity_chunks = 7,
        SettingKey::Denoise => r.attachments.denoise = Some(true),
        SettingKey::Vad => r.attachments.vad = Some(true),
        _ => panic!("matrix key must have a non-default test value"),
    }
}

#[test]
fn every_ignored_matrix_cell_and_its_default_value() {
    use IgnoreReason::*;
    use SettingKey::*;
    use SettingsPlatform::*;
    use SourceKind::*;
    let platforms = [Linux, Windows, MacOs, Other];
    let kinds = [Mic, SystemLoopback, ProcessLoopback, Mix];
    let previous = resolve_settings(
        &SettingsRequest::new(StreamConfig::default()),
        Linux,
        100,
        None,
    )
    .unwrap();
    let mut rows = Vec::new();
    for platform in platforms {
        for kind in kinds {
            for switch in [false, true] {
                for exclusions in [false, true] {
                    let base = (platform, kind, switch, exclusions);
                    if matches!(kind, ProcessLoopback | Mix) {
                        rows.push((base, DeviceId, WrongSourceKind));
                    }
                    if kind != ProcessLoopback {
                        for key in [TargetPid, Mode] {
                            rows.push((base, key, WrongSourceKind));
                        }
                    }
                    if matches!(kind, Mic | ProcessLoopback) {
                        for key in [ExcludeSelf, ExcludePids, WindowsClassicLoopbackKeepalive] {
                            rows.push((base, key, WrongSourceKind));
                        }
                    }
                    if kind != Mix {
                        for key in [MixMicDeviceId, MixSystemDeviceId, MixMicGain, MixSystemGain] {
                            rows.push((base, key, WrongSourceKind));
                        }
                    }
                    if kind == Mic {
                        rows.push((base, SilenceProbe, WrongSourceKind));
                    } else if platform != MacOs {
                        rows.push((base, SilenceProbe, UnsupportedPlatform));
                    }
                    if matches!(kind, SystemLoopback | Mix) {
                        if platform != Windows {
                            rows.push((base, WindowsClassicLoopbackKeepalive, UnsupportedPlatform));
                        } else if exclusions {
                            rows.push((base, WindowsClassicLoopbackKeepalive, ProcessLoopbackPath));
                        }
                        if exclusions && matches!(platform, Linux | Windows) {
                            rows.push((
                                base,
                                if kind == Mix {
                                    MixSystemDeviceId
                                } else {
                                    DeviceId
                                },
                                ExclusionDeviceUnscoped,
                            ));
                        }
                    }
                    if switch {
                        for key in [Gain, RingCapacityChunks, Denoise, Vad] {
                            rows.push((base, key, FixedAtOpen));
                        }
                    }
                }
            }
        }
    }
    assert!(rows.len() > 500);
    for ((platform, kind, switch, exclusions), key, reason) in rows {
        let mut r = SettingsRequest::new(StreamConfig {
            kind,
            ..Default::default()
        });
        r.attachments.fixed_at_open = vec![Denoise, Vad];
        if exclusions {
            r.config.exclude_pids = vec![99];
        }
        // The setting under test must start at its documented default even when
        // another field activates exclusions for this row.
        if key == ExcludePids {
            r.config.exclude_pids.clear();
        }
        let prior = switch.then_some(&previous);
        let default = resolve_settings(&r, platform, 100, prior).unwrap();
        assert_eq!(
            default.get(key).unwrap().outcome,
            SettingOutcome::NotRequested,
            "{platform:?} {kind:?} {key:?}"
        );
        requested(&mut r, key);
        let report = resolve_settings(&r, platform, 100, prior).unwrap();
        let entry = report.get(key).unwrap();
        assert_eq!(
            entry.outcome,
            SettingOutcome::Ignored(reason),
            "{platform:?} {kind:?} {key:?}"
        );
        if reason != FixedAtOpen {
            assert_eq!(entry.effective, SettingValue::Absent);
        }
    }
}

#[test]
fn defaults_applied_values_and_normalization() {
    let defaults = SettingsRequest::new(StreamConfig::default());
    let report = resolve_settings(&defaults, SettingsPlatform::Linux, 100, None).unwrap();
    assert_eq!(report.entries.len(), 20);
    assert!(report
        .entries
        .iter()
        .all(|entry| entry.outcome == SettingOutcome::NotRequested));
    let mut r = SettingsRequest::new(StreamConfig {
        kind: SourceKind::Mix,
        exclude_self: true,
        exclude_pids: vec![42, 42],
        mix_system_device_id: Some("output".into()),
        ..Default::default()
    });
    let report = resolve_settings(&r, SettingsPlatform::MacOs, 100, None).unwrap();
    let pids = report.get(SettingKey::ExcludePids).unwrap();
    assert_eq!(pids.requested, SettingValue::Pids(vec![42, 42]));
    assert_eq!(pids.effective, SettingValue::Pids(vec![42, 100]));
    assert_eq!(
        pids.outcome,
        SettingOutcome::Adjusted(AdjustmentReason::ExclusionSetNormalized)
    );
    assert_eq!(
        report.get(SettingKey::MixSystemDeviceId).unwrap().outcome,
        SettingOutcome::Applied
    );
    r.config.exclude_pids.clear();
    let report = resolve_settings(&r, SettingsPlatform::MacOs, 100, None).unwrap();
    assert_eq!(
        report.get(SettingKey::ExcludePids).unwrap().outcome,
        SettingOutcome::NotRequested
    );
    assert_eq!(
        report.get(SettingKey::ExcludePids).unwrap().effective,
        SettingValue::Pids(vec![100])
    );
    r.config.gain = 2.0;
    r.attachments.legacy_sentinels = vec![SettingKey::Gain];
    let report = resolve_settings(&r, SettingsPlatform::MacOs, 100, None).unwrap();
    assert_eq!(
        report.get(SettingKey::Gain).unwrap().outcome,
        SettingOutcome::Adjusted(AdjustmentReason::LegacySentinel)
    );
    r.config.gain = 1.0;
    let report = resolve_settings(&r, SettingsPlatform::MacOs, 100, None).unwrap();
    assert_eq!(
        report.get(SettingKey::Gain).unwrap().outcome,
        SettingOutcome::NotRequested
    );
}

#[test]
fn switch_defaults_compare_with_defaults_and_retain_fixed_values() {
    let mut r = SettingsRequest::new(StreamConfig {
        gain: 2.0,
        ring_capacity_chunks: 7,
        ..Default::default()
    });
    let initial = resolve_settings(&r, SettingsPlatform::Linux, 100, None).unwrap();
    let identical = resolve_settings(&r, SettingsPlatform::Linux, 100, Some(&initial)).unwrap();
    assert_eq!(identical.revision, 1);
    assert_eq!(
        identical.get(SettingKey::Gain).unwrap().outcome,
        SettingOutcome::Ignored(IgnoreReason::FixedAtOpen)
    );
    r.config.gain = 1.0;
    r.config.ring_capacity_chunks = 50;
    r.attachments.denoise = Some(true);
    let report = resolve_settings(&r, SettingsPlatform::Linux, 100, Some(&initial)).unwrap();
    for key in [SettingKey::Gain, SettingKey::RingCapacityChunks] {
        assert_eq!(
            report.get(key).unwrap().outcome,
            SettingOutcome::NotRequested
        );
        assert_eq!(
            report.get(key).unwrap().effective,
            initial.get(key).unwrap().effective
        );
    }
    assert_eq!(
        report.get(SettingKey::Denoise).unwrap().outcome,
        SettingOutcome::Applied
    );
    let mut exhausted = initial.clone();
    exhausted.revision = u64::MAX;
    assert_eq!(
        resolve_settings(&r, SettingsPlatform::Linux, 100, Some(&exhausted))
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidState
    );
}

#[test]
fn ignored_error_preserves_primary_context_and_existing_eq_contract() {
    let r = SettingsRequest::new(StreamConfig {
        mix_mic_gain: f32::NAN,
        ..Default::default()
    });
    let report = resolve_settings(&r, SettingsPlatform::Linux, 100, None).unwrap();
    let error = Error::IgnoredSettings { report };
    assert_eq!(error, error.clone());
    let wrapped = Error::Multiple(ErrorGroup::new(
        error.clone(),
        Error::DeviceLost,
        Vec::new(),
    ))
    .with_context(ErrorContext::new(Operation::Start))
    .with_diagnostic_detail("detail".into());
    assert_eq!(wrapped.kind(), ErrorKind::IgnoredSettings);
    assert!(wrapped.ignored_settings().is_some());
    assert_eq!(wrapped, wrapped.clone());
    assert!(!wrapped.to_string().contains("NaN"));
    assert!(Error::DeviceLost.ignored_settings().is_none());
}

#[test]
fn all_shared_setting_and_reason_tags() {
    let report = resolve_settings(
        &SettingsRequest::new(StreamConfig::default()),
        SettingsPlatform::Linux,
        100,
        None,
    )
    .unwrap();
    assert_eq!(
        report
            .entries
            .iter()
            .map(|entry| entry.setting.as_str())
            .collect::<Vec<_>>(),
        [
            "kind",
            "deviceId",
            "targetPid",
            "mode",
            "chunkMs",
            "ringCapacityChunks",
            "excludeSelf",
            "excludePids",
            "output",
            "secondaryOutput",
            "secondaryEncoding",
            "gain",
            "mixMicDeviceId",
            "mixSystemDeviceId",
            "mixMicGain",
            "mixSystemGain",
            "silenceProbe",
            "windowsClassicLoopbackKeepalive",
            "denoise",
            "vad"
        ]
    );
    assert_eq!(
        [
            IgnoreReason::WrongSourceKind,
            IgnoreReason::UnsupportedPlatform,
            IgnoreReason::ExclusionDeviceUnscoped,
            IgnoreReason::ProcessLoopbackPath,
            IgnoreReason::FixedAtOpen
        ]
        .map(IgnoreReason::as_str),
        [
            "wrongSourceKind",
            "unsupportedPlatform",
            "exclusionDeviceUnscoped",
            "processLoopbackPath",
            "fixedAtOpen"
        ]
    );
    assert_eq!(
        [
            AdjustmentReason::ExclusionSetNormalized,
            AdjustmentReason::LegacySentinel
        ]
        .map(AdjustmentReason::as_str),
        ["exclusionSetNormalized", "legacySentinel"]
    );
}

#[test]
fn applicable_platform_rows_apply_without_discovery() {
    for kind in [
        SourceKind::SystemLoopback,
        SourceKind::ProcessLoopback,
        SourceKind::Mix,
    ] {
        let mut request = SettingsRequest::new(StreamConfig {
            kind,
            silence_probe: false,
            ..Default::default()
        });
        let report = resolve_settings(&request, SettingsPlatform::MacOs, 100, None).unwrap();
        assert_eq!(
            report.get(SettingKey::SilenceProbe).unwrap().outcome,
            SettingOutcome::Applied
        );
        if kind != SourceKind::ProcessLoopback {
            request.keepalive = Some(false);
            let report = resolve_settings(&request, SettingsPlatform::Windows, 100, None).unwrap();
            assert_eq!(
                report
                    .get(SettingKey::WindowsClassicLoopbackKeepalive)
                    .unwrap()
                    .outcome,
                SettingOutcome::Applied
            );
        }
    }
    for platform in [
        SettingsPlatform::Linux,
        SettingsPlatform::Windows,
        SettingsPlatform::MacOs,
    ] {
        for kind in [SourceKind::Mic, SourceKind::SystemLoopback] {
            let request = SettingsRequest::new(StreamConfig {
                kind,
                device_id: Some("selector".into()),
                ..Default::default()
            });
            let report = resolve_settings(&request, platform, 100, None).unwrap();
            assert_eq!(
                report.get(SettingKey::DeviceId).unwrap().outcome,
                SettingOutcome::Applied
            );
        }
    }
}

#[test]
fn omitted_switch_options_use_default_requests_and_inherit_effective_values() {
    for platform in [
        SettingsPlatform::Linux,
        SettingsPlatform::Windows,
        SettingsPlatform::MacOs,
    ] {
        let mut request = SettingsRequest::new(StreamConfig {
            kind: SourceKind::SystemLoopback,
            ..Default::default()
        });
        request.keepalive = Some(false);
        request.attachments.denoise = Some(true);
        let initial = resolve_settings(&request, platform, 100, None).unwrap();
        request.keepalive = None;
        request.attachments.denoise = None;
        let report = resolve_settings(&request, platform, 100, Some(&initial)).unwrap();
        let keepalive = report
            .get(SettingKey::WindowsClassicLoopbackKeepalive)
            .unwrap();
        assert_eq!(keepalive.requested, SettingValue::Bool(true));
        assert_eq!(keepalive.outcome, SettingOutcome::NotRequested);
        assert_eq!(
            keepalive.effective,
            if platform == SettingsPlatform::Windows {
                SettingValue::Bool(false)
            } else {
                SettingValue::Absent
            }
        );
        let denoise = report.get(SettingKey::Denoise).unwrap();
        assert_eq!(denoise.requested, SettingValue::Bool(false));
        assert_eq!(denoise.outcome, SettingOutcome::NotRequested);
        assert_eq!(denoise.effective, SettingValue::Bool(true));
        assert!(!report.has_ignored());
    }
}
