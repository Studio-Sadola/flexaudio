use super::*;

#[test]
fn permission_errors_name_cause_settings_and_recovery() {
    for permission in [Permission::Microphone, Permission::SystemAudio] {
        let error = Error::PermissionDenied {
            permission,
            detail: "authorization is restricted".into(),
        };
        let message = error.to_string();
        assert!(message.contains(&permission.to_string()));
        assert!(!message.contains("authorization is restricted"));
        assert!(message.contains(permission.guidance()));
        assert!(message.contains("Restart the app"));
        assert!(message.contains("retry"));
    }
    assert_eq!(Permission::Microphone.as_str(), "microphone");
    assert_eq!(Permission::SystemAudio.as_str(), "systemAudio");
}

#[test]
fn macos_permission_remedies_never_reference_windows() {
    for (permission, setting) in [
        (Permission::Microphone, "Privacy & Security > Microphone"),
        (Permission::SystemAudio, "Screen & System Audio Recording"),
    ] {
        let remedy = permission_guidance(permission, PermissionPlatform::MacOs);
        assert!(remedy.contains(setting));
        assert!(remedy.contains("Restart the app"));
        assert!(!remedy.contains("Windows"));
    }
}

#[test]
fn windows_permission_remedies_never_reference_macos() {
    let microphone = permission_guidance(Permission::Microphone, PermissionPlatform::Windows);
    assert!(microphone.contains("Windows Settings > Privacy & security > Microphone"));
    assert!(microphone.contains("Let desktop apps access your microphone"));
    let system = permission_guidance(Permission::SystemAudio, PermissionPlatform::Windows);
    assert!(system.contains("target process's access restrictions"));
    assert!(system.contains("security policy"));
    for remedy in [microphone, system] {
        assert!(remedy.contains("Restart the app"));
        assert!(!remedy.contains("macOS"));
        assert!(!remedy.contains("System Settings"));
    }
}

#[test]
fn default_stream_config_matches_contract() {
    let c = StreamConfig::default();
    assert_eq!(c.chunk_ms, 20);
    assert_eq!(c.ring_capacity_chunks, 50);
    assert_eq!(c.mode, ProcessMode::Include);
    assert!(!c.exclude_self);
    assert!(c.exclude_pids.is_empty(), "no pids excluded by default");
    assert_eq!(c.kind, SourceKind::Mic);
    assert_eq!(c.device_id, None);
    assert_eq!(c.target_pid, None);
    assert_eq!(c.gain, 1.0);
    // Defaults for Mix-only fields (no device selected; pre-mix gain 1.0).
    assert_eq!(c.mix_mic_device_id, None);
    assert_eq!(c.mix_system_device_id, None);
    assert_eq!(c.mix_mic_gain, 1.0);
    assert_eq!(c.mix_system_gain, 1.0);
    // Default output matches the internal canonical form (stage 2 pass-through).
    assert_eq!(c.output.sample_rate, SAMPLE_RATE);
    assert_eq!(c.output.channels, CHANNELS);
    assert_eq!(c.output, OutputFormat::default());
    // No secondary tap by default.
    assert_eq!(c.secondary_output, None);
}

#[test]
fn output_format_chunk_frames_are_time_based() {
    assert_eq!(
        OutputFormat {
            sample_rate: 48_000,
            channels: 2
        }
        .chunk_frames(),
        960
    );
    assert_eq!(
        OutputFormat {
            sample_rate: 16_000,
            channels: 1
        }
        .chunk_frames(),
        320
    );
    assert_eq!(
        OutputFormat {
            sample_rate: 8_000,
            channels: 2
        }
        .chunk_frames(),
        160
    );
}

#[test]
fn output_format_validation_rejects_bad_configs() {
    // ch=0 / ch=3 are unsupported.
    assert!(OutputFormat {
        sample_rate: 48_000,
        channels: 0
    }
    .validate()
    .is_err());
    assert!(OutputFormat {
        sample_rate: 48_000,
        channels: 3
    }
    .validate()
    .is_err());
    // Extreme rates are unsupported.
    assert!(OutputFormat {
        sample_rate: 100,
        channels: 1
    }
    .validate()
    .is_err());
    assert!(OutputFormat {
        sample_rate: 1_000_000,
        channels: 2
    }
    .validate()
    .is_err());
    // Valid configuration is OK.
    assert!(OutputFormat {
        sample_rate: 16_000,
        channels: 1
    }
    .validate()
    .is_ok());
    assert!(OutputFormat::default().validate().is_ok());
}

#[test]
fn device_info_builds_and_clones() {
    let mic = DeviceInfo {
        id: "alsa_input.pci-0000_00_1f.3".into(),
        name: "Built-in Microphone".into(),
        source_kind: SourceKind::Mic,
        sample_rate: 48_000,
        channels: 2,
        is_loopback: false,
        is_default: true,
    };
    // Clone / PartialEq work (used to compare and duplicate enumeration results).
    assert_eq!(mic, mic.clone());
    assert!(!mic.is_loopback);
    assert!(mic.is_default);
    assert_eq!(mic.source_kind, SourceKind::Mic);

    let sys = DeviceInfo {
        source_kind: SourceKind::SystemLoopback,
        is_loopback: true,
        is_default: false,
        ..mic.clone()
    };
    assert!(sys.is_loopback);
    assert_ne!(mic, sys);
}

#[test]
fn process_mode_default_is_include() {
    // Default is Include (capture only the target PID). Exclude must be explicitly selected.
    assert_eq!(ProcessMode::default(), ProcessMode::Include);
    assert_ne!(ProcessMode::Include, ProcessMode::Exclude);
}

#[test]
fn chunk_flags_are_distinct_bits() {
    let all = ChunkFlags::DISCONTINUITY | ChunkFlags::RECOVERED | ChunkFlags::SILENCE;
    assert_eq!(all.bits(), 0b111);
    assert!(all.contains(ChunkFlags::SILENCE));
    assert_eq!(ChunkFlags::default(), ChunkFlags::empty());
}

#[test]
fn default_changed_accepts_default_device_kinds() {
    for kind in [
        DefaultDeviceKind::Microphone,
        DefaultDeviceKind::SystemAudio,
    ] {
        let event = DeviceEvent::DefaultChanged {
            kind,
            id: "endpoint".into(),
        };
        assert!(
            matches!(event, DeviceEvent::DefaultChanged { kind: observed, .. } if observed == kind)
        );
        assert!(
            matches!(DeviceEvent::DefaultCleared { kind }, DeviceEvent::DefaultCleared { kind: observed } if observed == kind)
        );
    }
}

#[test]
fn display_redacts_nested_errors() {
    let context = ErrorContext::new(Operation::Stop)
        .with_lane(MixLane::SystemAudio)
        .with_native_status(NativeStatus::HResult {
            call: "private_native_label",
            bits: 0x8000_0001,
        });
    let error = Error::Multiple(ErrorGroup::new(
        Error::Backend("capture worker failed".into()).with_context(context),
        Error::PermissionDenied {
            permission: Permission::Microphone,
            detail: "token-like-secret".into(),
        },
        vec![
            Error::InvalidArg("channels must be positive".into())
                .with_context(ErrorContext::new(Operation::Rollback)),
            Error::AmbiguousDeviceName,
        ],
    ));
    let message = error.to_string();
    for private in ["device-name", "token-like-secret", "private_native_label"] {
        assert!(!message.contains(private));
    }
    assert!(message.contains("backend error: capture worker failed"));
    assert!(message.contains("invalid argument: channels must be positive"));
    assert!(message.contains("HRESULT 0x80000001"));
    assert!(message.contains("during stop"));
    assert!(message.contains("during rollback"));
    assert_eq!(error.kind(), ErrorKind::Backend);
    assert_eq!(
        error.root(),
        &Error::Backend("capture worker failed".into())
    );
}

#[test]
fn wrapped_permission_kind_and_shutdown_primary_are_preserved() {
    let primary = Error::PermissionDenied {
        permission: Permission::Microphone,
        detail: "restricted".into(),
    }
    .with_context(ErrorContext::new(Operation::Start));
    let cleanup =
        Error::Backend("owner failure".into()).with_context(ErrorContext::new(Operation::Stop));
    let report = ShutdownReport::new(Some(primary.clone()), vec![cleanup.clone()]);
    let error = report.result().unwrap_err();
    assert_eq!(error.kind(), ErrorKind::PermissionDenied);
    assert_eq!(error.permission(), Some(Permission::Microphone));
    assert_eq!(report.primary(), Some(&primary));
    assert_eq!(report.cleanup(), &[cleanup]);
    let Error::Multiple(group) = error else {
        panic!("both causes must be retained")
    };
    assert_eq!(group.secondary().count(), 1);
    assert!(ShutdownReport::new(None, Vec::new()).result().is_ok());
}

#[test]
fn loss_paths_formats_and_unknown_counts_are_validated() {
    use std::num::NonZeroU64;
    let count = NonZeroU64::new(200);
    let loss = AudioLoss::raw_overflow(None, count, 48_000, 2).unwrap();
    assert_eq!(loss.path(), AudioPath::Capture { lane: None });
    assert_eq!(loss.reason(), LossReason::RawOverflow);
    assert_eq!(loss.samples(), count);
    assert_eq!(
        loss.with_capture_lane(MixLane::Microphone).unwrap().path(),
        AudioPath::Capture {
            lane: Some(MixLane::Microphone)
        }
    );
    let loss = AudioLoss::mix_fifo_overflow(MixLane::SystemAudio, None);
    assert_eq!(loss.reason(), LossReason::MixFifoOverflow);
    assert_eq!(
        (loss.sample_rate(), loss.channels(), loss.samples()),
        (48_000, 2, None)
    );
    let loss = AudioLoss::output_overflow(OutputTap::Secondary, count, 16_000, 1).unwrap();
    assert_eq!(
        loss.path(),
        AudioPath::Output {
            tap: OutputTap::Secondary
        }
    );
    assert_eq!(loss.reason(), LossReason::OutputOverflow);
    assert!(AudioLoss::raw_overflow(None, None, 0, 2).is_err());
    assert!(AudioLoss::output_overflow(OutputTap::Primary, None, 48_000, 0).is_err());
}

#[test]
fn validation_display_preserves_library_explanations() {
    for (error, expected) in [
        (
            Error::InvalidArg("channels must be positive".into()),
            "invalid argument: channels must be positive",
        ),
        (
            Error::InvalidState("capture is stopping".into()),
            "invalid state: capture is stopping",
        ),
        (
            Error::UnsupportedFormat("native channels must be at most two".into()),
            "unsupported output format: native channels must be at most two",
        ),
        (
            Error::Backend("capture worker failed".into()),
            "backend error: capture worker failed",
        ),
    ] {
        assert_eq!(error.to_string(), expected);
    }
}

#[test]
fn private_error_fields_are_not_in_display() {
    assert_eq!(
        Error::AmbiguousDeviceName.to_string(),
        "device selection is ambiguous; select a unique device"
    );
    let error = Error::PermissionDenied {
        permission: Permission::Microphone,
        detail: "private_environment_detail".into(),
    };
    assert!(!error.to_string().contains("private_environment_detail"));
    assert!(error.to_string().contains("recording permission denied"));
}
