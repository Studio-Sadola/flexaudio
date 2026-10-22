//! JSON boundary for the read-only capability snapshot.
use flexaudio::core::{ErrorKind, SourceKind};
use flexaudio::{
    Availability, BackendKind, Capabilities, DeviceSelectionBehavior, OsVersion, PidScope,
    SideEffectCapability, SideEffectCondition, SourceCapability, UnavailableReason, WatchScope,
    WatcherMode,
};
use serde_json::{json, Value};

pub(super) fn project(snapshot: &Capabilities) -> Value {
    json!({
        "osVersion": snapshot.os_version.as_ref().map(version),
        "sources": {
            "mic": source(&snapshot.sources.mic),
            "system": source(&snapshot.sources.system),
            "process": source(&snapshot.sources.process),
            "mix": source(&snapshot.sources.mix),
        },
        "systemExclusions": {
            "availability": availability(&snapshot.system_exclusions.availability),
            "backend": backend(&snapshot.system_exclusions.backend),
            "scope": pid_scope(&snapshot.system_exclusions.scope),
            "maxDistinctPids": snapshot.system_exclusions.max_distinct_pids,
            "deviceSelection": device_selection(&snapshot.system_exclusions.device_selection),
        },
        "process": {
            "includeScope": pid_scope(&snapshot.process.include_scope),
            "excludeScope": pid_scope(&snapshot.process.exclude_scope),
        },
        "deviceWatcher": {
            "availability": availability(&snapshot.device_watcher.availability),
            "mode": watcher_mode(&snapshot.device_watcher.mode),
            "scopes": snapshot.device_watcher.scopes.iter().map(watch_scope).collect::<Vec<_>>(),
        },
        "sideEffects": {
            "silenceProbe": side_effect(&snapshot.side_effects.silence_probe),
            "classicLoopbackKeepalive": side_effect(&snapshot.side_effects.classic_loopback_keepalive),
        },
    })
}

fn version(value: &OsVersion) -> Value {
    json!({"major": value.major, "minor": value.minor, "patch": value.patch, "build": value.build})
}

fn source(value: &SourceCapability) -> Value {
    json!({"backend": backend(&value.backend), "availability": availability(&value.availability)})
}

fn availability(value: &Availability) -> Value {
    match value {
        Availability::Supported => json!({"status": "supported"}),
        Availability::Unavailable { reason } => {
            json!({"status": "unavailable", "reason": unavailable_reason(reason)})
        }
        Availability::Unknown { error } => json!({
            "status": "unknown",
            "error": {"kind": error_kind(&error.kind()), "message": error.to_string()},
        }),
        _ => json!({"status": "unknown", "error": {
            "kind": "unsupported", "message": "Unrecognized capability availability",
        }}),
    }
}

fn unavailable_reason(value: &UnavailableReason) -> Value {
    match value {
        UnavailableReason::UnsupportedOs => json!({"kind": "unsupportedOs"}),
        UnavailableReason::OsVersionTooOld { required, actual } => json!({
            "kind": "osVersionTooOld", "required": version(required), "actual": version(actual),
        }),
        UnavailableReason::PipeWireUnavailable => json!({"kind": "pipeWireUnavailable"}),
        UnavailableReason::NotApplicable => json!({"kind": "notApplicable"}),
        _ => json!({"kind": "other", "code": format!("{value:?}")}),
    }
}

fn side_effect(value: &SideEffectCapability) -> Value {
    json!({
        "availability": availability(&value.availability),
        "enabledByDefault": value.enabled_by_default,
        "sources": value.sources.iter().map(source_kind).collect::<Vec<_>>(),
        "condition": side_effect_condition(&value.condition),
    })
}

// Keep wire tags explicit: Debug names are not the public JSON contract.
macro_rules! tags {
    ($name:ident, $ty:ty, {$($variant:path => $tag:literal),+ $(,)?}) => {
        fn $name(value: &$ty) -> &'static str {
            match value {
                $($variant => $tag,)+
                #[allow(unreachable_patterns)]
                _ => "other",
            }
        }
    };
}

tags!(backend, BackendKind, {
    BackendKind::Cpal => "cpal",
    BackendKind::PipeWire => "pipeWire",
    BackendKind::WasapiLoopback => "wasapiLoopback",
    BackendKind::WasapiProcessLoopback => "wasapiProcessLoopback",
    BackendKind::CoreAudioProcessTap => "coreAudioProcessTap",
    BackendKind::Composite => "composite",
    BackendKind::Unsupported => "unsupported",
});
tags!(pid_scope, PidScope, {
    PidScope::ExactPid => "exactPid",
    PidScope::ProcessTree => "processTree",
    PidScope::NotApplicable => "notApplicable",
});
tags!(device_selection, DeviceSelectionBehavior, {
    DeviceSelectionBehavior::Honored => "honored",
    DeviceSelectionBehavior::Ignored => "ignored",
    DeviceSelectionBehavior::NotApplicable => "notApplicable",
});
tags!(watcher_mode, WatcherMode, {
    WatcherMode::Native => "native",
    WatcherMode::Noop => "noop",
});
tags!(watch_scope, WatchScope, {
    WatchScope::SystemOutputs => "systemOutputs",
    WatchScope::DefaultSystemOutput => "defaultSystemOutput",
});
tags!(side_effect_condition, SideEffectCondition, {
    SideEffectCondition::SilentActiveSystemAudio => "silentActiveSystemAudio",
    SideEffectCondition::ClassicLoopbackWithoutExclusions => "classicLoopbackWithoutExclusions",
});
tags!(source_kind, SourceKind, {
    SourceKind::Mic => "mic",
    SourceKind::SystemLoopback => "system",
    SourceKind::ProcessLoopback => "process",
    SourceKind::Mix => "mix",
});
fn error_kind(value: &ErrorKind) -> &'static str {
    match value {
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
        ErrorKind::IgnoredSettings => "ignoredSettings",
        // ErrorKind is non-exhaustive outside core. Its runtime inventory test
        // makes a new kind fail until this explicit wire mapping is updated.
        _ => panic!("unmapped error kind: {value:?}"),
    }
}

#[cfg(test)]
#[path = "capabilities_json_tests.rs"]
mod tests;
