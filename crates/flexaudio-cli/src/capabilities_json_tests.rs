use super::*;

macro_rules! assert_tags {
    ($function:ident, [$($variant:expr => $expected:literal),+ $(,)?]) => {
        $(assert_eq!($function(&$variant), $expected);)+
    };
}

#[test]
fn enum_tags_match_contract() {
    assert_tags!(backend, [
        BackendKind::Cpal => "cpal",
        BackendKind::PipeWire => "pipeWire",
        BackendKind::WasapiLoopback => "wasapiLoopback",
        BackendKind::WasapiProcessLoopback => "wasapiProcessLoopback",
        BackendKind::CoreAudioProcessTap => "coreAudioProcessTap",
        BackendKind::Composite => "composite",
        BackendKind::Unsupported => "unsupported",
    ]);
    assert_tags!(pid_scope, [PidScope::ExactPid => "exactPid", PidScope::ProcessTree => "processTree", PidScope::NotApplicable => "notApplicable"]);
    assert_tags!(device_selection, [DeviceSelectionBehavior::Honored => "honored", DeviceSelectionBehavior::Ignored => "ignored", DeviceSelectionBehavior::NotApplicable => "notApplicable"]);
    assert_tags!(watcher_mode, [WatcherMode::Native => "native", WatcherMode::Noop => "noop"]);
    assert_tags!(watch_scope, [WatchScope::SystemOutputs => "systemOutputs", WatchScope::DefaultSystemOutput => "defaultSystemOutput"]);
    assert_tags!(side_effect_condition, [SideEffectCondition::SilentActiveSystemAudio => "silentActiveSystemAudio", SideEffectCondition::ClassicLoopbackWithoutExclusions => "classicLoopbackWithoutExclusions"]);
    assert_tags!(source_kind, [SourceKind::Mic => "mic", SourceKind::SystemLoopback => "system", SourceKind::ProcessLoopback => "process", SourceKind::Mix => "mix"]);
    assert_tags!(error_kind, [
        ErrorKind::InvalidArg => "invalidArg",
        ErrorKind::InvalidState => "invalidState",
        ErrorKind::DeviceNotFound => "deviceNotFound",
        ErrorKind::PermissionDenied => "permissionDenied",
        ErrorKind::UnsupportedOsVersion => "unsupportedOsVersion",
        ErrorKind::DeviceLost => "deviceLost",
        ErrorKind::Backend => "backend",
        ErrorKind::UnsupportedFormat => "unsupportedFormat",
        ErrorKind::NativeFormatChanged => "nativeFormatChanged",
        ErrorKind::Unsupported => "unsupported",
        ErrorKind::AmbiguousDeviceName => "ambiguousDeviceName",
    ]);
}

#[test]
fn availability_and_reason_shapes_match_contract() {
    assert_eq!(
        availability(&Availability::Supported),
        json!({"status": "supported"})
    );
    for (reason, tag) in [
        (UnavailableReason::UnsupportedOs, "unsupportedOs"),
        (
            UnavailableReason::PipeWireUnavailable,
            "pipeWireUnavailable",
        ),
        (UnavailableReason::NotApplicable, "notApplicable"),
    ] {
        assert_eq!(
            availability(&Availability::Unavailable { reason }),
            json!({"status": "unavailable", "reason": {"kind": tag}})
        );
    }
    assert_eq!(
        unavailable_reason(&UnavailableReason::OsVersionTooOld {
            required: OsVersion::new(14, 2, None, None),
            actual: OsVersion::new(13, 0, Some(1), Some(2)),
        }),
        json!({"kind": "osVersionTooOld", "required": {"major": 14, "minor": 2, "patch": null, "build": null}, "actual": {"major": 13, "minor": 0, "patch": 1, "build": 2}})
    );
}

#[test]
fn unknown_error_has_kind_and_safe_message() {
    let error = flexaudio::Error::Backend("capability probe failed".into())
        .with_diagnostic_detail("private diagnostic".into())
        .with_context(flexaudio::core::ErrorContext::new(
            flexaudio::core::Operation::Enumerate,
        ));
    assert_eq!(
        availability(&Availability::Unknown { error }),
        json!({
            "status": "unknown", "error": {"kind": "backend", "message": "backend error: capability probe failed during enumerate"},
        })
    );
}

#[test]
fn snapshot_projects_all_fields_in_camel_case() {
    use flexaudio::{
        DeviceWatcherCapabilities, ProcessCapabilities, SideEffectCapabilities, SourceCapabilities,
        SystemExclusionCapabilities,
    };
    let src = || SourceCapability::new(BackendKind::Cpal, Availability::Supported);
    let effect = |condition| {
        SideEffectCapability::new(
            Availability::Supported,
            true,
            vec![SourceKind::SystemLoopback, SourceKind::Mix],
            condition,
        )
    };
    let snapshot = Capabilities::new(
        None,
        SourceCapabilities::new(src(), src(), src(), src()),
        SystemExclusionCapabilities::new(
            Availability::Supported,
            BackendKind::PipeWire,
            PidScope::ExactPid,
            Some(4),
            DeviceSelectionBehavior::Honored,
        ),
        ProcessCapabilities::new(PidScope::ExactPid, PidScope::ProcessTree),
        DeviceWatcherCapabilities::new(
            Availability::Supported,
            WatcherMode::Native,
            vec![WatchScope::SystemOutputs, WatchScope::DefaultSystemOutput],
        ),
        SideEffectCapabilities::new(
            effect(SideEffectCondition::SilentActiveSystemAudio),
            effect(SideEffectCondition::ClassicLoopbackWithoutExclusions),
        ),
    );
    let source = json!({"backend": "cpal", "availability": {"status": "supported"}});
    let effect = |condition| json!({"availability": {"status": "supported"}, "enabledByDefault": true, "sources": ["system", "mix"], "condition": condition});
    assert_eq!(
        project(&snapshot),
        json!({
            "osVersion": null,
            "sources": {"mic": source, "system": source, "process": source, "mix": source},
            "systemExclusions": {"availability": {"status": "supported"}, "backend": "pipeWire", "scope": "exactPid", "maxDistinctPids": 4, "deviceSelection": "honored"},
            "process": {"includeScope": "exactPid", "excludeScope": "processTree"},
            "deviceWatcher": {"availability": {"status": "supported"}, "mode": "native", "scopes": ["systemOutputs", "defaultSystemOutput"]},
            "sideEffects": {"silenceProbe": effect("silentActiveSystemAudio"), "classicLoopbackKeepalive": effect("classicLoopbackWithoutExclusions")},
        })
    );
}
