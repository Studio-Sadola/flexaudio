//! Argument and settings contracts that require no audio hardware.
use super::*;
use serde_json::json;

#[test]
fn new_flags_parse_with_unchanged_defaults() {
    let defaults = Cli::try_parse_from(["flexaudio-cli"]).unwrap();
    assert!(!defaults.strict && !defaults.settings_json && !defaults.lifecycle_events);
    assert!(!defaults.list_devices_json && !defaults.denoise);
    assert_eq!(defaults.ring_capacity_chunks, 50);
    let cli = Cli::try_parse_from([
        "flexaudio-cli",
        "--strict",
        "--settings-json",
        "--lifecycle-events",
        "--list-devices-json",
        "--denoise",
        "--ring-capacity-chunks",
        "7",
    ])
    .unwrap();
    assert!(cli.strict && cli.settings_json && cli.lifecycle_events && cli.denoise);
    assert!(cli.list_devices_json);
    assert_eq!(
        config_for_kind(&cli, SourceKind::Mic).ring_capacity_chunks,
        7
    );
    assert!(cli.open_options().strict);
}

#[test]
fn capacity_requires_positive_native_integer() {
    for value in [
        "0",
        "-1",
        "1.5",
        "true",
        "NaN",
        "+2",
        "",
        "18446744073709551616",
    ] {
        assert!(
            Cli::try_parse_from(["flexaudio-cli", &format!("--ring-capacity-chunks={value}")])
                .is_err(),
            "{value}"
        );
    }
    for value in ["1".to_string(), "50".to_string(), usize::MAX.to_string()] {
        assert!(Cli::try_parse_from(["flexaudio-cli", "--ring-capacity-chunks", &value]).is_ok());
    }
    assert!(Cli::try_parse_from(["flexaudio-cli", "--ring-capacity-chunks"]).is_err());
    assert!(Cli::try_parse_from(["flexaudio-cli", "--strict=false"]).is_err());
    assert!(
        Cli::try_parse_from(["flexaudio-cli", "--list-devices", "--list-devices-json"]).is_err()
    );
}

#[test]
fn strict_candidate_report_has_complete_shared_json_shape() {
    let cli = Cli::try_parse_from([
        "flexaudio-cli",
        "--strict",
        "--mode",
        "exclude",
        "--denoise",
    ])
    .unwrap();
    let error =
        flexaudio::open_with_options(config_for_kind(&cli, SourceKind::Mic), cli.open_options())
            .err()
            .unwrap();
    let report = error.ignored_settings().unwrap();
    let projected = settings_json::project(report, cli.denoise);
    assert_eq!(projected["revision"], 0);
    assert_eq!(projected["entries"].as_array().unwrap().len(), 20);
    let entries = projected["entries"].as_array().unwrap();
    let mode = entries
        .iter()
        .find(|entry| entry["setting"] == "mode")
        .unwrap();
    assert_eq!(
        mode,
        &json!({"setting":"mode","requested":"exclude","effective":null,
        "outcome":{"status":"ignored","reason":"wrongSourceKind"}})
    );
    let denoise = entries
        .iter()
        .find(|entry| entry["setting"] == "denoise")
        .unwrap();
    assert_eq!(
        denoise,
        &json!({"setting":"denoise","requested":true,"effective":true,"outcome":{"status":"applied"}})
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&projected.to_string()).unwrap(),
        projected
    );
}

#[test]
fn strict_schedules_retain_open_gain_and_capacity() {
    for extra in [
        vec!["--gain", "2"],
        vec!["--ring-capacity-chunks", "100"],
        vec!["--gain", "2", "--ring-capacity-chunks", "100"],
    ] {
        let mut args = vec!["flexaudio-cli", "--strict", "--sources", "mic:2,system:2"];
        args.extend(extra);
        let cli = Cli::try_parse_from(args).unwrap();
        let reports = schedule_reports(&cli);
        for (key, expected) in [
            (
                flexaudio::SettingKey::Gain,
                flexaudio::SettingValue::Float(cli.gain),
            ),
            (
                flexaudio::SettingKey::RingCapacityChunks,
                flexaudio::SettingValue::Unsigned(cli.ring_capacity_chunks as u64),
            ),
        ] {
            let entry = reports[1]
                .entries
                .iter()
                .find(|entry| entry.setting == key)
                .unwrap();
            assert_eq!(entry.outcome, flexaudio::SettingOutcome::NotRequested);
            assert_eq!(entry.effective, expected);
        }
    }
}

/// Exercise the same open/switch settings resolution without native audio devices.
fn schedule_reports(cli: &Cli) -> Vec<flexaudio::SettingsReport> {
    assert!(cli.strict);
    let segments = parse_sources(cli.sources.as_ref().unwrap()).unwrap();
    let scheduler = SwitchScheduler::new(cli, &segments, Instant::now());
    let mut reports = Vec::new();
    for config in std::iter::once(config_for_kind(cli, segments[0].kind)).chain(scheduler.configs) {
        let mut request = flexaudio::SettingsRequest::new(config).open_options(&cli.open_options());
        if let Some(previous) = reports.last() {
            request = request.previous(previous);
        }
        let report = flexaudio::resolve_settings(&request).unwrap();
        assert!(
            !report.has_ignored(),
            "strict schedule must accept: {report:?}"
        );
        reports.push(report);
    }
    reports
}

#[test]
fn strict_schedule_routes_mode_and_pid_only_to_process_segments() {
    let cli = Cli::try_parse_from([
        "flexaudio-cli",
        "--strict",
        "--mode",
        "exclude",
        "--sources",
        "mic:2,process:2,system:2",
        "--process-id",
        "123",
    ])
    .unwrap();
    schedule_reports(&cli);
    let scheduler = SwitchScheduler::new(
        &cli,
        &parse_sources(cli.sources.as_ref().unwrap()).unwrap(),
        Instant::now(),
    );
    let mic = config_for_kind(&cli, SourceKind::Mic);
    assert_eq!(mic.mode, ProcessMode::Include);
    assert_eq!(mic.target_pid, None);
    assert_eq!(scheduler.configs[0].mode, ProcessMode::Exclude);
    assert_eq!(scheduler.configs[0].target_pid, Some(123));
    assert_eq!(scheduler.configs[1].mode, ProcessMode::Include);
    assert_eq!(scheduler.configs[1].target_pid, None);
}

#[test]
fn strict_schedule_routes_device_and_exclusions_only_to_applicable_segments() {
    let cli = Cli::try_parse_from([
        "flexaudio-cli",
        "--strict",
        "--sources",
        "mic:2,process:2,mic:2",
        "--process-id",
        "123",
        "--device-id",
        "selected-device",
    ])
    .unwrap();
    schedule_reports(&cli);
    let scheduler = SwitchScheduler::new(
        &cli,
        &parse_sources(cli.sources.as_ref().unwrap()).unwrap(),
        Instant::now(),
    );
    assert_eq!(
        config_for_kind(&cli, SourceKind::Mic).device_id.as_deref(),
        Some("selected-device")
    );
    assert_eq!(scheduler.configs[0].device_id, None);
    assert_eq!(
        scheduler.configs[1].device_id.as_deref(),
        Some("selected-device")
    );

    let cli = Cli::try_parse_from([
        "flexaudio-cli",
        "--strict",
        "--sources",
        "mic:2,system:2,process:2",
        "--process-id",
        "123",
        "--exclude-self",
        "--exclude-pid",
        "456",
    ])
    .unwrap();
    schedule_reports(&cli);
}
