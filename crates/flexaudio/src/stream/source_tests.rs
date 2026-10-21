//! Device-free source configuration and recovery regressions.
use super::*;
use crate::mock::MockBackend;
use flexaudio_core::types::{ProcessMode, SourceKind};
use std::cell::Cell;

fn mock_backend() -> Box<dyn CaptureBackend> {
    Box::new(MockBackend::new(48_000, 2, 440.0))
}

fn snapshot_with_inapplicable_mix_settings() -> StreamConfig {
    let mut config = StreamConfig::default();
    config.mix_mic_device_id = Some("old-mic".into());
    config.mix_system_device_id = Some("old-output".into());
    config.mix_mic_gain = 0.25;
    config.mix_system_gain = 0.5;
    config
}

#[test]
fn strict_switch_resolves_custom_history_without_rejecting_it_as_a_new_request() {
    let initial = snapshot_with_inapplicable_mix_settings();
    let mut stream = Stream::open_with_backend_options(
        initial,
        mock_backend(),
        OpenOptions::new().windows_classic_loopback_keepalive(false),
    )
    .unwrap();
    assert!(stream.open_options().strict);
    stream.start().unwrap();
    let replacement = StreamConfig::default();
    let builds = Cell::new(0);
    stream
        .switch_source_using(replacement.clone(), None, None, |config, options| {
            builds.set(builds.get() + 1);
            assert_eq!(config, &replacement);
            assert!(options.strict);
            assert!(!options.windows_classic_loopback_keepalive);
            Ok(mock_backend())
        })
        .unwrap();
    assert_eq!(builds.get(), 1);
    assert_eq!(stream.config(), &replacement);
    assert!(stream.open_options().strict);
    assert!(!stream.open_options().windows_classic_loopback_keepalive);
    assert!(stream.effective_settings().is_none());
    {
        let mut backend = stream.shared.backend.lock().unwrap();
        stop_backend_reconciling(&stream.shared, &mut backend, false);
    }
    Stream::open_backend_once(&stream.shared, GenerationChange::Recovery).unwrap();
    assert_eq!(stream.config(), &replacement);
    assert!(stream.open_options().strict);
    assert!(!stream.open_options().windows_classic_loopback_keepalive);

    // Only historical state is exempt: the same fields in a new request still fail.
    let error = stream
        .switch_source_using(
            snapshot_with_inapplicable_mix_settings(),
            None,
            None,
            |_, _| panic!("ignored new requests must fail before construction"),
        )
        .unwrap_err();
    assert_eq!(error.kind(), crate::ErrorKind::IgnoredSettings);
    assert_eq!(stream.config(), &replacement);
    assert!(stream.open_options().strict);
    stream.stop_checked().unwrap();
}

#[test]
fn historical_custom_settings_do_not_mask_switch_construction_failure() {
    let initial = snapshot_with_inapplicable_mix_settings();
    let mut stream = Stream::open(initial.clone(), mock_backend()).unwrap();
    stream.start().unwrap();
    let options = stream.open_options().clone();
    assert!(options.strict);
    let builds = Cell::new(0);
    let error = stream
        .switch_source_using(StreamConfig::default(), None, None, |_, _| {
            builds.set(builds.get() + 1);
            Err(Error::DeviceNotFound)
        })
        .unwrap_err();
    assert_eq!(error, Error::DeviceNotFound);
    assert_eq!(builds.get(), 1);
    assert_eq!(stream.config(), &initial);
    assert_eq!(stream.open_options(), &options);
    assert!(stream.effective_settings().is_none());
    // Recovery also keeps the old installed backend and its historical snapshot.
    {
        let mut backend = stream.shared.backend.lock().unwrap();
        stop_backend_reconciling(&stream.shared, &mut backend, false);
    }
    Stream::open_backend_once(&stream.shared, GenerationChange::Recovery).unwrap();
    assert_eq!(stream.config(), &initial);
    assert_eq!(stream.open_options(), &options);
    stream.stop_checked().unwrap();
}

#[test]
fn switch_source_snapshot_matches_mix_backend_and_survives_reopen() {
    let mut initial = StreamConfig::default();
    initial.mix_mic_device_id = Some("old-mic".into());
    initial.mix_system_device_id = Some("old-output".into());
    initial.mix_mic_gain = 0.25;
    initial.mix_system_gain = 0.5;
    initial.gain = 0.75;
    initial.ring_capacity_chunks = 7;
    initial.secondary_output = Some(OutputFormat {
        sample_rate: 16_000,
        channels: 1,
    });
    let mut replacement = initial.clone();
    replacement.kind = SourceKind::Mix;
    replacement.mix_mic_device_id = Some("new-mic".into());
    replacement.mix_system_device_id = Some("new-output".into());
    replacement.mix_mic_gain = 1.5;
    replacement.mix_system_gain = 2.0;
    replacement.gain = 3.0;
    replacement.ring_capacity_chunks = 19;
    replacement.mode = ProcessMode::Exclude;
    replacement.exclude_self = true;
    replacement.exclude_pids = vec![42];
    replacement.silence_probe = false;
    // This snapshot test deliberately requests fixed-at-open gain/capacity and
    // other ignored values; keep its permissive behavior explicit.
    let mut stream = Stream::open_with_backend_options(
        initial.clone(),
        mock_backend(),
        OpenOptions::new().strict(false),
    )
    .unwrap();
    stream.start().unwrap();
    stream
        .switch_source_using(replacement.clone(), None, None, |config, _| {
            assert_eq!(config, &replacement);
            Ok(mock_backend())
        })
        .unwrap();
    let mut expected = replacement;
    expected.gain = initial.gain;
    expected.ring_capacity_chunks = initial.ring_capacity_chunks;
    assert_eq!(stream.config(), &expected);
    assert_eq!(stream.gain(), initial.gain);
    assert!(!stream.open_options().strict);

    // Recovery restarts the installed backend; it must not reconstruct an old source.
    {
        let mut backend = stream.shared.backend.lock().unwrap();
        stop_backend_reconciling(&stream.shared, &mut backend, false);
    }
    Stream::open_backend_once(&stream.shared, GenerationChange::Recovery).unwrap();
    assert_eq!(stream.config(), &expected);
    stream.stop_checked().unwrap();
}

#[test]
fn failed_source_switch_keeps_mix_snapshot() {
    let mut initial = StreamConfig::default();
    initial.mix_mic_device_id = Some("old-mic".into());
    initial.mix_system_device_id = Some("old-output".into());
    initial.mix_mic_gain = 0.25;
    initial.mix_system_gain = 0.5;
    let mut stream = Stream::open(initial.clone(), mock_backend()).unwrap();
    stream.start().unwrap();
    let mut replacement = initial.clone();
    replacement.kind = SourceKind::Mix;
    replacement.mix_mic_device_id = Some("new-mic".into());
    replacement.mix_system_device_id = Some("new-output".into());
    replacement.mix_mic_gain = 1.5;
    replacement.mix_system_gain = 2.0;
    let failure = Error::DeviceNotFound;
    assert_eq!(
        stream.switch_source_using(replacement, None, None, |_, _| Err(failure.clone())),
        Err(failure)
    );
    assert_eq!(stream.config(), &initial);
    stream.stop_checked().unwrap();
}

#[test]
fn source_switch_rejects_output_and_secondary_changes_before_backend_construction() {
    let initial = StreamConfig::default();
    let mut stream = Stream::open(initial.clone(), mock_backend()).unwrap();
    stream.start().unwrap();
    let mut changed_output = initial.clone();
    changed_output.output = OutputFormat {
        sample_rate: 16_000,
        channels: 1,
    };
    let mut changed_secondary = initial.clone();
    changed_secondary.secondary_output = Some(OutputFormat {
        sample_rate: 16_000,
        channels: 1,
    });
    for config in [changed_output, changed_secondary] {
        assert!(matches!(
            stream.switch_source_using(config, None, None, |_, _| panic!(
                "invalid switch reached backend construction"
            )),
            Err(Error::InvalidArg(_))
        ));
        assert_eq!(stream.config(), &initial);
    }
    stream.stop_checked().unwrap();
}
