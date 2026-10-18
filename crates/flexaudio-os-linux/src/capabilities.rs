//! Linux capability mapping; capture is never opened by this query.

use flexaudio_core::{
    Availability, BackendKind, Capabilities, DeviceSelectionBehavior, DeviceWatcherCapabilities,
    PidScope, ProcessCapabilities, SideEffectCapabilities, SideEffectCapability,
    SideEffectCondition, SourceCapabilities, SourceCapability, SystemExclusionCapabilities,
    UnavailableReason, WatchScope, WatcherMode,
};

/// Return support after a bounded PipeWire connection and server sync.
///
/// Does not enumerate devices, query microphone permissions, or open audio streams.
/// Confirmed missing PipeWire sockets report unavailable. Socket inspection or
/// connection failures report unknown, including stale sockets and access errors.
/// A query already in progress returns unknown immediately rather than starting
/// another worker. Native connection calls cannot be interrupted; at most one
/// worker remains alive after a timeout, until that native call returns.
pub fn capabilities() -> Capabilities {
    from_probe(crate::capability_probe::probe())
}

fn from_probe(pipewire: Availability) -> Capabilities {
    let not_applicable = || Availability::Unavailable {
        reason: UnavailableReason::NotApplicable,
    };
    Capabilities::new(
        None,
        SourceCapabilities::new(
            SourceCapability::new(BackendKind::Cpal, Availability::Supported),
            SourceCapability::new(BackendKind::PipeWire, pipewire.clone()),
            SourceCapability::new(BackendKind::PipeWire, pipewire.clone()),
            SourceCapability::new(BackendKind::Composite, pipewire.clone()),
        ),
        SystemExclusionCapabilities::new(
            pipewire.clone(),
            BackendKind::PipeWire,
            PidScope::ExactPid,
            None,
            DeviceSelectionBehavior::Ignored,
        ),
        ProcessCapabilities::new(PidScope::ExactPid, PidScope::ExactPid),
        DeviceWatcherCapabilities::new(
            pipewire,
            WatcherMode::Native,
            vec![WatchScope::SystemOutputs, WatchScope::DefaultSystemOutput],
        ),
        SideEffectCapabilities::new(
            SideEffectCapability::new(
                not_applicable(),
                false,
                vec![],
                SideEffectCondition::SilentActiveSystemAudio,
            ),
            SideEffectCapability::new(
                not_applicable(),
                false,
                vec![],
                SideEffectCondition::ClassicLoopbackWithoutExclusions,
            ),
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use flexaudio_core::Error;

    #[test]
    fn pipewire_prerequisite_is_shared_without_affecting_microphones() {
        for availability in [
            Availability::Supported,
            Availability::Unavailable {
                reason: UnavailableReason::PipeWireUnavailable,
            },
            Availability::Unknown {
                error: Error::Backend("capability probe timed out".into()),
            },
        ] {
            let caps = from_probe(availability.clone());
            assert_eq!(caps.sources.mic.backend, BackendKind::Cpal);
            assert_eq!(caps.sources.mic.availability, Availability::Supported);
            assert_eq!(caps.sources.system.backend, BackendKind::PipeWire);
            assert_eq!(caps.sources.process.backend, BackendKind::PipeWire);
            assert_eq!(caps.sources.mix.backend, BackendKind::Composite);
            assert_eq!(caps.sources.system.availability, availability);
            assert_eq!(caps.sources.process.availability, availability);
            assert_eq!(caps.sources.mix.availability, availability);
            assert_eq!(caps.system_exclusions.availability, availability);
            assert_eq!(caps.device_watcher.availability, availability);
        }
    }

    #[test]
    fn linux_pid_device_watcher_and_side_effect_semantics() {
        let caps = from_probe(Availability::Supported);
        assert!(caps.os_version.is_none());
        assert_eq!(caps.system_exclusions.backend, BackendKind::PipeWire);
        assert_eq!(caps.system_exclusions.scope, PidScope::ExactPid);
        assert_eq!(caps.system_exclusions.max_distinct_pids, None);
        assert_eq!(
            caps.system_exclusions.device_selection,
            DeviceSelectionBehavior::Ignored
        );
        assert_eq!(caps.process.include_scope, PidScope::ExactPid);
        assert_eq!(caps.process.exclude_scope, PidScope::ExactPid);
        assert_eq!(caps.device_watcher.mode, WatcherMode::Native);
        assert_eq!(
            caps.device_watcher.scopes,
            [WatchScope::SystemOutputs, WatchScope::DefaultSystemOutput]
        );
        for effect in [
            caps.side_effects.silence_probe,
            caps.side_effects.classic_loopback_keepalive,
        ] {
            assert_eq!(
                effect.availability,
                Availability::Unavailable {
                    reason: UnavailableReason::NotApplicable
                }
            );
            assert!(!effect.enabled_by_default);
            assert!(effect.sources.is_empty());
        }
    }
}
