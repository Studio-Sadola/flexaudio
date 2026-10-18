//! Windows capture capabilities without activating audio devices or clients.

use flexaudio_core::{
    Availability, BackendKind, Capabilities, DeviceSelectionBehavior, DeviceWatcherCapabilities,
    OsVersion, PidScope, ProcessCapabilities, Result, SideEffectCapabilities, SideEffectCapability,
    SideEffectCondition, SourceCapabilities, SourceCapability, SourceKind,
    SystemExclusionCapabilities, UnavailableReason, WatcherMode,
};

use crate::exclusion::MAX_DISTINCT_EXCLUSION_PIDS;
use crate::version::{process_loopback_supported, MIN_PROCESS_LOOPBACK_BUILD};

/// Snapshot capture support without enumerating devices or opening audio clients.
///
/// This reports OS support; endpoint availability and access are checked when opening capture.
#[cfg(target_os = "windows")]
pub fn capabilities() -> Capabilities {
    from_version(crate::version::current_os_version())
}

fn from_version(version: Result<OsVersion>) -> Capabilities {
    let process_availability = match &version {
        Ok(actual) => match actual.build {
            Some(build) if process_loopback_supported(build) => Availability::Supported,
            Some(_) => Availability::Unavailable {
                reason: UnavailableReason::OsVersionTooOld {
                    required: OsVersion::new(10, 0, None, Some(MIN_PROCESS_LOOPBACK_BUILD)),
                    actual: actual.clone(),
                },
            },
            None => Availability::Unknown {
                error: flexaudio_core::Error::Backend(
                    "Windows version query returned no build number".into(),
                ),
            },
        },
        Err(error) => Availability::Unknown {
            error: error.clone(),
        },
    };
    Capabilities::new(
        version.ok(),
        SourceCapabilities::new(
            SourceCapability::new(BackendKind::Cpal, Availability::Supported),
            SourceCapability::new(BackendKind::WasapiLoopback, Availability::Supported),
            SourceCapability::new(
                BackendKind::WasapiProcessLoopback,
                process_availability.clone(),
            ),
            SourceCapability::new(BackendKind::Composite, Availability::Supported),
        ),
        SystemExclusionCapabilities::new(
            process_availability,
            BackendKind::WasapiProcessLoopback,
            PidScope::ProcessTree,
            Some(u32::try_from(MAX_DISTINCT_EXCLUSION_PIDS).expect("exclusion limit fits u32")),
            DeviceSelectionBehavior::Ignored,
        ),
        ProcessCapabilities::new(PidScope::ProcessTree, PidScope::ProcessTree),
        DeviceWatcherCapabilities::new(not_applicable(), WatcherMode::Noop, Vec::new()),
        SideEffectCapabilities::new(
            SideEffectCapability::new(
                not_applicable(),
                false,
                Vec::new(),
                SideEffectCondition::SilentActiveSystemAudio,
            ),
            SideEffectCapability::new(
                Availability::Supported,
                true,
                vec![SourceKind::SystemLoopback, SourceKind::Mix],
                SideEffectCondition::ClassicLoopbackWithoutExclusions,
            ),
        ),
    )
}

fn not_applicable() -> Availability {
    Availability::Unavailable {
        reason: UnavailableReason::NotApplicable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(build: u32) -> OsVersion {
        OsVersion::new(10, 0, None, Some(build))
    }

    #[test]
    fn reported_exclusion_limit_matches_capture_validation() {
        use crate::exclusion::validate_exclusion;

        let snapshot = from_version(Ok(version(MIN_PROCESS_LOOPBACK_BUILD)));
        let limit = snapshot.system_exclusions.max_distinct_pids.unwrap();
        assert_eq!(limit, 1, "WASAPI accepts one process-tree root");
        for exclude_self in [false, true] {
            let pids: Vec<_> = (1..=limit).collect();
            assert_eq!(validate_exclusion(exclude_self, 1, &pids).unwrap(), Some(1));
            let mut duplicates = pids.clone();
            duplicates.extend_from_slice(&pids);
            assert_eq!(
                validate_exclusion(exclude_self, 1, &duplicates).unwrap(),
                Some(1)
            );
            let over_limit: Vec<_> = (1..=limit + 1).collect();
            assert!(matches!(
                validate_exclusion(exclude_self, 1, &over_limit),
                Err(flexaudio_core::Error::InvalidArg(_))
            ));
        }
        // The implicit self PID counts even when it is omitted from the explicit list.
        assert!(matches!(
            validate_exclusion(true, 1, &[2]),
            Err(flexaudio_core::Error::InvalidArg(_))
        ));
        assert!(matches!(
            validate_exclusion(false, 1, &[0]),
            Err(flexaudio_core::Error::InvalidArg(_))
        ));
    }

    #[test]
    fn process_loopback_boundary_does_not_gate_classic_capture() {
        for (build, supported) in [(20_347, false), (20_348, true)] {
            let actual = version(build);
            let snapshot = from_version(Ok(actual.clone()));
            assert_eq!(snapshot.os_version, Some(actual.clone()));
            assert_eq!(snapshot.sources.mic.backend, BackendKind::Cpal);
            assert_eq!(snapshot.sources.system.backend, BackendKind::WasapiLoopback);
            assert_eq!(snapshot.sources.mix.backend, BackendKind::Composite);
            assert_eq!(
                snapshot.sources.system.availability,
                Availability::Supported
            );
            assert_eq!(snapshot.sources.mix.availability, Availability::Supported);
            assert_eq!(
                snapshot.sources.process.backend,
                BackendKind::WasapiProcessLoopback
            );
            let expected = if supported {
                Availability::Supported
            } else {
                Availability::Unavailable {
                    reason: UnavailableReason::OsVersionTooOld {
                        required: version(MIN_PROCESS_LOOPBACK_BUILD),
                        actual,
                    },
                }
            };
            assert_eq!(snapshot.sources.process.availability, expected);
            assert_eq!(snapshot.system_exclusions.availability, expected);
            assert_eq!(
                snapshot.system_exclusions.backend,
                BackendKind::WasapiProcessLoopback
            );
            assert_eq!(snapshot.system_exclusions.scope, PidScope::ProcessTree);
            assert_eq!(snapshot.system_exclusions.max_distinct_pids, Some(1));
            assert_eq!(
                snapshot.system_exclusions.device_selection,
                DeviceSelectionBehavior::Ignored
            );
            assert_eq!(snapshot.process.include_scope, PidScope::ProcessTree);
            assert_eq!(snapshot.process.exclude_scope, PidScope::ProcessTree);
        }
    }

    #[test]
    fn failed_version_query_preserves_unknown_cause_only_for_process_loopback() {
        let error = flexaudio_core::Error::Backend("version query failed".into());
        let snapshot = from_version(Err(error.clone()));
        assert_eq!(snapshot.os_version, None);
        assert_eq!(
            snapshot.sources.process.availability,
            Availability::Unknown { error }
        );
        assert_eq!(
            snapshot.system_exclusions.availability,
            snapshot.sources.process.availability
        );
        assert_eq!(
            snapshot.sources.system.availability,
            Availability::Supported
        );
        assert_eq!(snapshot.sources.mix.availability, Availability::Supported);
    }

    #[test]
    fn missing_build_is_unknown_rather_than_supported() {
        let snapshot = from_version(Ok(OsVersion::new(10, 0, None, None)));
        assert!(matches!(
            snapshot.sources.process.availability,
            Availability::Unknown { .. }
        ));
    }

    #[test]
    fn watcher_and_side_effects_match_windows_paths() {
        let snapshot = from_version(Ok(version(MIN_PROCESS_LOOPBACK_BUILD)));
        assert_eq!(snapshot.device_watcher.mode, WatcherMode::Noop);
        assert_eq!(snapshot.device_watcher.availability, not_applicable());
        assert!(snapshot.device_watcher.scopes.is_empty());
        assert_eq!(
            snapshot.side_effects.silence_probe.availability,
            not_applicable()
        );
        assert!(!snapshot.side_effects.silence_probe.enabled_by_default);
        assert!(snapshot.side_effects.silence_probe.sources.is_empty());
        let keepalive = snapshot.side_effects.classic_loopback_keepalive;
        assert_eq!(keepalive.availability, Availability::Supported);
        assert!(keepalive.enabled_by_default);
        assert_eq!(
            keepalive.sources,
            [SourceKind::SystemLoopback, SourceKind::Mix]
        );
        assert_eq!(
            keepalive.condition,
            SideEffectCondition::ClassicLoopbackWithoutExclusions
        );
    }
}
