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
fn strict_scheduled_switch_rejects_before_backend_access() {
    let cli = Cli::try_parse_from([
        "flexaudio-cli",
        "--strict",
        "--sources",
        "mic:1,mic:1",
        "--gain",
        "2",
    ])
    .unwrap();
    let config = config_for_kind(&cli, SourceKind::Mic);
    let mut stream = Stream::open(
        config,
        Box::new(flexaudio::MockBackend::new(48000, 2, 440.0)),
    )
    .unwrap();
    stream.start().unwrap();
    let start = Instant::now();
    let segments = parse_sources(cli.sources.as_ref().unwrap()).unwrap();
    let mut scheduler = SwitchScheduler::new(&cli, &segments, start);
    let error = scheduler
        .tick(&mut stream, start + Duration::from_secs(1))
        .unwrap_err();
    assert_eq!(error.kind(), flexaudio::ErrorKind::IgnoredSettings);
    assert!(error.ignored_settings().unwrap().has_ignored());
}
