//! Read-only capability snapshots projected at the Node boundary.

use flexaudio::core;
use napi::bindgen_prelude::{AsyncTask, Either, Null};
use napi::{Env, Task};
use napi_derive::napi;

use crate::boundary::{audio_error, JsAudioError};

#[napi(object)]
pub struct Capabilities {
    pub os_version: Either<OsVersion, Null>,
    pub sources: SourceCapabilities,
    pub system_exclusions: SystemExclusionCapabilities,
    pub process: ProcessCapabilities,
    pub device_watcher: DeviceWatcherCapabilities,
    pub side_effects: SideEffectCapabilities,
}

#[napi(object)]
pub struct OsVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: Either<u32, Null>,
    pub build: Either<u32, Null>,
}

#[napi(object)]
pub struct SourceCapabilities {
    pub mic: SourceCapability,
    pub system: SourceCapability,
    pub process: SourceCapability,
    pub mix: SourceCapability,
}

#[napi(object)]
pub struct SourceCapability {
    pub backend: String,
    pub availability: CapabilityAvailability,
}

#[napi(object)]
pub struct CapabilityAvailability {
    pub status: String,
    pub reason: Option<CapabilityUnavailableReason>,
    pub error: Option<JsAudioError>,
}

#[napi(object)]
pub struct CapabilityUnavailableReason {
    pub kind: String,
    pub required: Option<OsVersion>,
    pub actual: Option<OsVersion>,
    pub code: Option<String>,
}

#[napi(object)]
pub struct SystemExclusionCapabilities {
    pub availability: CapabilityAvailability,
    pub backend: String,
    pub scope: String,
    pub max_distinct_pids: Either<u32, Null>,
    pub device_selection: String,
}

#[napi(object)]
pub struct ProcessCapabilities {
    pub include_scope: String,
    pub exclude_scope: String,
}

#[napi(object)]
pub struct DeviceWatcherCapabilities {
    pub availability: CapabilityAvailability,
    pub mode: String,
    pub scopes: Vec<String>,
}

#[napi(object)]
pub struct SideEffectCapabilities {
    pub silence_probe: SideEffectCapability,
    pub classic_loopback_keepalive: SideEffectCapability,
}

#[napi(object)]
pub struct SideEffectCapability {
    pub availability: CapabilityAvailability,
    pub enabled_by_default: bool,
    pub sources: Vec<String>,
    pub condition: String,
}

fn nullable<T>(value: Option<T>) -> Either<T, Null> {
    value.map(Either::A).unwrap_or(Either::B(Null))
}

fn version(value: core::OsVersion) -> OsVersion {
    OsVersion {
        major: value.major,
        minor: value.minor,
        patch: nullable(value.patch),
        build: nullable(value.build),
    }
}

fn backend(value: core::BackendKind) -> String {
    match value {
        core::BackendKind::Cpal => "cpal",
        core::BackendKind::PipeWire => "pipeWire",
        core::BackendKind::WasapiLoopback => "wasapiLoopback",
        core::BackendKind::WasapiProcessLoopback => "wasapiProcessLoopback",
        core::BackendKind::CoreAudioProcessTap => "coreAudioProcessTap",
        core::BackendKind::Composite => "composite",
        core::BackendKind::Unsupported => "unsupported",
        _ => "other",
    }
    .into()
}

fn unavailable_reason(value: core::UnavailableReason) -> CapabilityUnavailableReason {
    let mut result = CapabilityUnavailableReason {
        kind: String::new(),
        required: None,
        actual: None,
        code: None,
    };
    result.kind = match value {
        core::UnavailableReason::UnsupportedOs => "unsupportedOs",
        core::UnavailableReason::OsVersionTooOld { required, actual } => {
            result.required = Some(version(required));
            result.actual = Some(version(actual));
            "osVersionTooOld"
        }
        core::UnavailableReason::PipeWireUnavailable => "pipeWireUnavailable",
        core::UnavailableReason::NotApplicable => "notApplicable",
        other => {
            result.code = Some(format!("{other:?}"));
            "other"
        }
    }
    .into();
    result
}

fn availability(value: core::Availability) -> CapabilityAvailability {
    let mut result = CapabilityAvailability {
        status: String::new(),
        reason: None,
        error: None,
    };
    result.status = match value {
        core::Availability::Supported => "supported",
        core::Availability::Unavailable { reason } => {
            result.reason = Some(unavailable_reason(reason));
            "unavailable"
        }
        core::Availability::Unknown { error } => {
            result.error = Some(audio_error(&error));
            "unknown"
        }
        _ => {
            result.error = Some(audio_error(&core::Error::Unsupported));
            "unknown"
        }
    }
    .into();
    result
}

fn source(value: core::SourceCapability) -> SourceCapability {
    SourceCapability {
        backend: backend(value.backend),
        availability: availability(value.availability),
    }
}

fn pid_scope(value: core::PidScope) -> String {
    match value {
        core::PidScope::ExactPid => "exactPid",
        core::PidScope::ProcessTree => "processTree",
        core::PidScope::NotApplicable => "notApplicable",
        _ => "other",
    }
    .into()
}

// SourceKind is currently exhaustive; retain a fallback if it gains variants.
#[allow(unreachable_patterns)]
fn source_kind(value: core::SourceKind) -> String {
    match value {
        core::SourceKind::Mic => "mic",
        core::SourceKind::SystemLoopback => "system",
        core::SourceKind::ProcessLoopback => "process",
        core::SourceKind::Mix => "mix",
        _ => "other",
    }
    .into()
}

fn side_effect(value: core::SideEffectCapability) -> SideEffectCapability {
    SideEffectCapability {
        availability: availability(value.availability),
        enabled_by_default: value.enabled_by_default,
        sources: value.sources.into_iter().map(source_kind).collect(),
        condition: match value.condition {
            core::SideEffectCondition::SilentActiveSystemAudio => "silentActiveSystemAudio",
            core::SideEffectCondition::ClassicLoopbackWithoutExclusions => {
                "classicLoopbackWithoutExclusions"
            }
            _ => "other",
        }
        .into(),
    }
}

pub(crate) fn snapshot(value: core::Capabilities) -> Capabilities {
    Capabilities {
        os_version: nullable(value.os_version.map(version)),
        sources: SourceCapabilities {
            mic: source(value.sources.mic),
            system: source(value.sources.system),
            process: source(value.sources.process),
            mix: source(value.sources.mix),
        },
        system_exclusions: SystemExclusionCapabilities {
            availability: availability(value.system_exclusions.availability),
            backend: backend(value.system_exclusions.backend),
            scope: pid_scope(value.system_exclusions.scope),
            max_distinct_pids: nullable(value.system_exclusions.max_distinct_pids),
            device_selection: match value.system_exclusions.device_selection {
                core::DeviceSelectionBehavior::Honored => "honored",
                core::DeviceSelectionBehavior::Ignored => "ignored",
                core::DeviceSelectionBehavior::NotApplicable => "notApplicable",
                _ => "other",
            }
            .into(),
        },
        process: ProcessCapabilities {
            include_scope: pid_scope(value.process.include_scope),
            exclude_scope: pid_scope(value.process.exclude_scope),
        },
        device_watcher: DeviceWatcherCapabilities {
            availability: availability(value.device_watcher.availability),
            mode: match value.device_watcher.mode {
                core::WatcherMode::Native => "native",
                core::WatcherMode::Noop => "noop",
                _ => "other",
            }
            .into(),
            scopes: value
                .device_watcher
                .scopes
                .into_iter()
                .map(|scope| {
                    match scope {
                        core::WatchScope::SystemOutputs => "systemOutputs",
                        core::WatchScope::DefaultSystemOutput => "defaultSystemOutput",
                        _ => "other",
                    }
                    .into()
                })
                .collect(),
        },
        side_effects: SideEffectCapabilities {
            silence_probe: side_effect(value.side_effects.silence_probe),
            classic_loopback_keepalive: side_effect(value.side_effects.classic_loopback_keepalive),
        },
    }
}

/// Run the bounded prerequisite probe on the libuv thread pool.
pub struct CapabilitiesTask;

impl Task for CapabilitiesTask {
    type Output = core::Capabilities;
    type JsValue = Capabilities;

    fn compute(&mut self) -> napi::Result<Self::Output> {
        Ok(flexaudio::capabilities())
    }

    fn resolve(&mut self, _env: Env, output: Self::Output) -> napi::Result<Self::JsValue> {
        Ok(snapshot(output))
    }
}

/// Query support and side effects without opening streams or requesting permission.
/// Supported does not imply device availability or recording permission. Probe failures
/// remain in the snapshot as unknown availability with a structured AudioError.
#[napi(ts_return_type = "Promise<Capabilities>")]
pub fn capabilities() -> AsyncTask<CapabilitiesTask> {
    AsyncTask::new(CapabilitiesTask)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_backend_tags_are_stable() {
        let values = [
            (core::BackendKind::Cpal, "cpal"),
            (core::BackendKind::PipeWire, "pipeWire"),
            (core::BackendKind::WasapiLoopback, "wasapiLoopback"),
            (
                core::BackendKind::WasapiProcessLoopback,
                "wasapiProcessLoopback",
            ),
            (
                core::BackendKind::CoreAudioProcessTap,
                "coreAudioProcessTap",
            ),
            (core::BackendKind::Composite, "composite"),
            (core::BackendKind::Unsupported, "unsupported"),
        ];
        for (value, expected) in values {
            assert_eq!(backend(value), expected);
        }
    }

    #[test]
    fn availability_preserves_typed_errors_and_version_requirements() {
        let error = core::Error::DeviceLost
            .with_context(core::ErrorContext::new(core::Operation::Enumerate));
        let unknown = availability(core::Availability::Unknown { error });
        assert_eq!(unknown.status, "unknown");
        assert!(unknown.reason.is_none());
        let error = unknown.error.unwrap();
        assert_eq!(error.kind, "deviceLost");
        assert_eq!(error.contexts[0].operation, "enumerate");

        let unavailable = availability(core::Availability::Unavailable {
            reason: core::UnavailableReason::OsVersionTooOld {
                required: core::OsVersion::new(14, 4, None, None),
                actual: core::OsVersion::new(14, 3, Some(1), Some(42)),
            },
        });
        assert_eq!(unavailable.status, "unavailable");
        assert!(unavailable.error.is_none());
        let reason = unavailable.reason.unwrap();
        assert_eq!(reason.kind, "osVersionTooOld");
        let required = reason.required.unwrap();
        assert_eq!((required.major, required.minor), (14, 4));
        assert!(matches!(required.patch, Either::B(Null)));
        assert!(matches!(required.build, Either::B(Null)));
        let actual = reason.actual.unwrap();
        assert!(matches!(actual.patch, Either::A(1)));
        assert!(matches!(actual.build, Either::A(42)));
    }
}
