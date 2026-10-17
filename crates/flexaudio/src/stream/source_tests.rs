//! Device-free source configuration and recovery regressions.
use super::*;
use crate::mock::MockBackend;
use flexaudio_core::types::{ProcessMode, SourceKind};

fn mock_backend() -> Box<dyn CaptureBackend> {
    Box::new(MockBackend::new(48_000, 2, 440.0))
}

#[test]
fn switch_source_snapshot_matches_mix_backend_and_survives_reopen() {
    let initial = StreamConfig {
        mix_mic_device_id: Some("old-mic".into()),
        mix_system_device_id: Some("old-output".into()),
        mix_mic_gain: 0.25,
        mix_system_gain: 0.5,
        gain: 0.75,
        ring_capacity_chunks: 7,
        secondary_output: Some(OutputFormat {
            sample_rate: 16_000,
            channels: 1,
        }),
        ..Default::default()
    };
    let replacement = StreamConfig {
        kind: SourceKind::Mix,
        mix_mic_device_id: Some("new-mic".into()),
        mix_system_device_id: Some("new-output".into()),
        mix_mic_gain: 1.5,
        mix_system_gain: 2.0,
        gain: 3.0,
        ring_capacity_chunks: 19,
        mode: ProcessMode::Exclude,
        exclude_self: true,
        exclude_pids: vec![42],
        silence_probe: false,
        ..initial.clone()
    };
    let mut stream = Stream::open(initial.clone(), mock_backend()).unwrap();
    stream.start().unwrap();
    stream
        .switch_source_using(replacement.clone(), None, |config| {
            assert_eq!(config, &replacement);
            Ok(mock_backend())
        })
        .unwrap();
    let expected = StreamConfig {
        gain: initial.gain,
        ring_capacity_chunks: initial.ring_capacity_chunks,
        ..replacement
    };
    assert_eq!(stream.config(), &expected);
    assert_eq!(stream.gain(), initial.gain);

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
    let initial = StreamConfig {
        mix_mic_device_id: Some("old-mic".into()),
        mix_system_device_id: Some("old-output".into()),
        mix_mic_gain: 0.25,
        mix_system_gain: 0.5,
        ..Default::default()
    };
    let mut stream = Stream::open(initial.clone(), mock_backend()).unwrap();
    stream.start().unwrap();
    let replacement = StreamConfig {
        kind: SourceKind::Mix,
        mix_mic_device_id: Some("new-mic".into()),
        mix_system_device_id: Some("new-output".into()),
        mix_mic_gain: 1.5,
        mix_system_gain: 2.0,
        ..initial.clone()
    };
    let failure = Error::DeviceNotFound;
    assert_eq!(
        stream.switch_source_using(replacement, None, |_| Err(failure.clone())),
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
    let changed_output = StreamConfig {
        output: OutputFormat {
            sample_rate: 16_000,
            channels: 1,
        },
        ..initial.clone()
    };
    let changed_secondary = StreamConfig {
        secondary_output: Some(OutputFormat {
            sample_rate: 16_000,
            channels: 1,
        }),
        ..initial.clone()
    };
    for config in [changed_output, changed_secondary] {
        assert!(matches!(
            stream.switch_source_using(config, None, |_| panic!(
                "invalid switch reached backend construction"
            )),
            Err(Error::InvalidArg(_))
        ));
        assert_eq!(stream.config(), &initial);
    }
    stream.stop_checked().unwrap();
}
