//! Strict preflight must run before even constructing a native backend.

use crate::*;
use std::cell::Cell;

#[test]
fn strict_preflight_precedes_all_backend_construction_and_device_access() {
    let calls = Cell::new(0);
    let mut config = StreamConfig::default();
    config.device_id = Some("unused selector".into());
    config.target_pid = Some(42);
    config.exclude_self = true;
    config.mix_mic_gain = 2.0;
    config.silence_probe = false;
    let error = crate::open_using(config.clone(), OpenOptions::new().strict(true), |_, _| {
        // Any permission/format/device query or model load is below this boundary.
        calls.set(calls.get() + 1);
        Err(Error::DeviceNotFound)
    })
    .err()
    .expect("strict request must fail");
    assert_eq!(calls.get(), 0);
    assert_eq!(error.kind(), ErrorKind::IgnoredSettings);
    let report = error.ignored_settings().unwrap();
    assert_eq!(report.revision, 0);
    assert_eq!(report.entries.len(), 20);
    assert_eq!(
        report
            .entries
            .iter()
            .filter(|entry| matches!(entry.outcome, SettingOutcome::Ignored(_)))
            .count(),
        4
    );
    let error = crate::open_using(config, OpenOptions::default(), |_, _| {
        calls.set(calls.get() + 1);
        Err(Error::DeviceNotFound)
    })
    .err()
    .expect("control must reach builder");
    assert_eq!(calls.get(), 1);
    assert_eq!(error.kind(), ErrorKind::DeviceNotFound);
}

#[test]
fn explicit_defaults_pass_strict_and_facade_streams_have_reports() {
    let stream = crate::open_using(
        StreamConfig::default(),
        OpenOptions::new().strict(true),
        |_, _| Ok(Box::new(MockBackend::new(48_000, 2, 0.0))),
    )
    .unwrap();
    let report = stream.effective_settings().unwrap();
    assert_eq!(report.revision, 0);
    assert!(report
        .entries
        .iter()
        .all(|entry| entry.outcome == SettingOutcome::NotRequested));
    let custom = Stream::open(
        StreamConfig::default(),
        Box::new(MockBackend::new(48_000, 2, 0.0)),
    )
    .unwrap();
    assert!(custom.effective_settings().is_none());
    assert!(!OpenOptions::new().strict);
}

#[test]
fn invalid_input_kinds_survive_strict_preflight() {
    let configs = [
        {
            let mut config_value = StreamConfig::default();
            config_value.gain = f32::NAN;
            config_value.target_pid = Some(42);
            config_value
        },
        {
            let mut config_value = StreamConfig::default();
            config_value.ring_capacity_chunks = 0;
            config_value.target_pid = Some(42);
            config_value
        },
        {
            let mut config_value = StreamConfig::new(SourceKind::ProcessLoopback);
            config_value.device_id = Some("ignored".into());
            config_value
        },
        {
            let mut config_value = StreamConfig::new(SourceKind::Mix);
            config_value.mix_mic_gain = -1.0;
            config_value.device_id = Some("ignored".into());
            config_value
        },
    ];
    for config in configs {
        let error = crate::open_using(config, OpenOptions::new().strict(true), |_, _| {
            panic!("invalid request cannot acquire a backend")
        })
        .err()
        .expect("invalid input");
        assert_eq!(error.kind(), ErrorKind::InvalidArg);
    }
}

#[test]
fn strict_allows_existing_exclusion_normalization() {
    let self_pid = std::process::id();
    let mut config = StreamConfig::new(SourceKind::SystemLoopback);
    config.exclude_self = true;
    config.exclude_pids = vec![self_pid, self_pid];
    let stream = crate::open_using(config, OpenOptions::new().strict(true), |_, _| {
        Ok(Box::new(MockBackend::new(48_000, 2, 0.0)))
    })
    .unwrap();
    let pids = stream
        .effective_settings()
        .unwrap()
        .get(SettingKey::ExcludePids)
        .unwrap();
    assert_eq!(
        pids.outcome,
        SettingOutcome::Adjusted(AdjustmentReason::ExclusionSetNormalized)
    );
    assert_eq!(pids.effective, SettingValue::Pids(vec![self_pid]));
}
