//! Capture capabilities derived only from the operating system version.

use flexaudio_core::{
    Availability, BackendKind, Capabilities, DeviceSelectionBehavior, DeviceWatcherCapabilities,
    OsVersion, PidScope, ProcessCapabilities, Result, SideEffectCapabilities, SideEffectCapability,
    SideEffectCondition, SourceCapabilities, SourceCapability, SourceKind,
    SystemExclusionCapabilities, UnavailableReason, WatcherMode,
};

use crate::version::{meets_min_version, MIN_MAJOR, MIN_MINOR};

/// Snapshot capture support without accessing devices or requesting permissions.
#[cfg(target_os = "macos")]
pub fn capabilities() -> Capabilities {
    for_version(crate::version::current_os_version())
}

fn for_version(version: Result<OsVersion>) -> Capabilities {
    let (os_version, tap_availability) = match version {
        Ok(actual) => {
            let availability = if meets_min_version(actual.major, actual.minor) {
                Availability::Supported
            } else {
                Availability::Unavailable {
                    reason: UnavailableReason::OsVersionTooOld {
                        required: OsVersion::new(MIN_MAJOR, MIN_MINOR, None, None),
                        actual: actual.clone(),
                    },
                }
            };
            (Some(actual), availability)
        }
        Err(error) => (None, Availability::Unknown { error }),
    };
    let tap = SourceCapability::new(BackendKind::CoreAudioProcessTap, tap_availability.clone());
    let not_applicable = Availability::Unavailable {
        reason: UnavailableReason::NotApplicable,
    };

    Capabilities::new(
        os_version,
        SourceCapabilities::new(
            SourceCapability::new(BackendKind::Cpal, Availability::Supported),
            tap.clone(),
            tap.clone(),
            tap,
        ),
        SystemExclusionCapabilities::new(
            tap_availability.clone(),
            BackendKind::CoreAudioProcessTap,
            PidScope::ExactPid,
            None,
            DeviceSelectionBehavior::Honored,
        ),
        ProcessCapabilities::new(PidScope::ExactPid, PidScope::ExactPid),
        DeviceWatcherCapabilities::new(not_applicable.clone(), WatcherMode::Noop, Vec::new()),
        SideEffectCapabilities::new(
            SideEffectCapability::new(
                tap_availability,
                true,
                vec![
                    SourceKind::SystemLoopback,
                    SourceKind::ProcessLoopback,
                    SourceKind::Mix,
                ],
                SideEffectCondition::SilentActiveSystemAudio,
            ),
            SideEffectCapability::new(
                not_applicable,
                false,
                Vec::new(),
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
    fn version_14_3_disables_all_tap_features_without_disabling_microphones() {
        let actual = OsVersion::new(14, 3, Some(1), None);
        let snapshot = for_version(Ok(actual.clone()));
        let unavailable = Availability::Unavailable {
            reason: UnavailableReason::OsVersionTooOld {
                required: OsVersion::new(MIN_MAJOR, MIN_MINOR, None, None),
                actual: actual.clone(),
            },
        };
        assert_eq!(snapshot.os_version, Some(actual));
        assert_eq!(snapshot.sources.mic.availability, Availability::Supported);
        for source in [
            snapshot.sources.system,
            snapshot.sources.process,
            snapshot.sources.mix,
        ] {
            assert_eq!(source.backend, BackendKind::CoreAudioProcessTap);
            assert_eq!(source.availability, unavailable);
        }
        assert_eq!(snapshot.system_exclusions.availability, unavailable);
        assert_eq!(
            snapshot.side_effects.silence_probe.availability,
            unavailable
        );
    }

    #[test]
    fn version_14_4_enables_exact_pid_taps_and_reports_platform_policies() {
        let snapshot = for_version(Ok(OsVersion::new(14, 4, Some(0), None)));
        assert_eq!(snapshot.sources.mic.backend, BackendKind::Cpal);
        for source in [
            snapshot.sources.system,
            snapshot.sources.process,
            snapshot.sources.mix,
        ] {
            assert_eq!(source.backend, BackendKind::CoreAudioProcessTap);
            assert_eq!(source.availability, Availability::Supported);
        }
        assert_eq!(
            snapshot.system_exclusions.availability,
            Availability::Supported
        );
        assert_eq!(
            snapshot.system_exclusions.backend,
            BackendKind::CoreAudioProcessTap
        );
        assert_eq!(snapshot.system_exclusions.scope, PidScope::ExactPid);
        assert_eq!(snapshot.system_exclusions.max_distinct_pids, None);
        assert_eq!(
            snapshot.system_exclusions.device_selection,
            DeviceSelectionBehavior::Honored
        );
        assert_eq!(snapshot.process.include_scope, PidScope::ExactPid);
        assert_eq!(snapshot.process.exclude_scope, PidScope::ExactPid);
        assert_eq!(snapshot.device_watcher.mode, WatcherMode::Noop);
        assert_eq!(
            snapshot.device_watcher.availability,
            Availability::Unavailable {
                reason: UnavailableReason::NotApplicable
            }
        );
        assert!(snapshot.device_watcher.scopes.is_empty());
        let probe = snapshot.side_effects.silence_probe;
        assert_eq!(probe.availability, Availability::Supported);
        assert!(probe.enabled_by_default);
        assert_eq!(
            probe.sources,
            vec![
                SourceKind::SystemLoopback,
                SourceKind::ProcessLoopback,
                SourceKind::Mix
            ]
        );
        assert_eq!(
            probe.condition,
            SideEffectCondition::SilentActiveSystemAudio
        );
        let keepalive = snapshot.side_effects.classic_loopback_keepalive;
        assert_eq!(
            keepalive.availability,
            Availability::Unavailable {
                reason: UnavailableReason::NotApplicable
            }
        );
        assert!(!keepalive.enabled_by_default);
        assert!(keepalive.sources.is_empty());
        assert_eq!(
            keepalive.condition,
            SideEffectCondition::ClassicLoopbackWithoutExclusions
        );
    }

    #[test]
    fn version_lookup_failure_remains_unknown_for_tap_features() {
        let error = Error::Backend("version lookup failed".into());
        let snapshot = for_version(Err(error.clone()));
        let unknown = Availability::Unknown { error };
        assert_eq!(snapshot.os_version, None);
        assert_eq!(snapshot.sources.mic.availability, Availability::Supported);
        assert_eq!(snapshot.sources.system.availability, unknown);
        assert_eq!(snapshot.sources.process.availability, unknown);
        assert_eq!(snapshot.sources.mix.availability, unknown);
        assert_eq!(snapshot.system_exclusions.availability, unknown);
        assert_eq!(snapshot.side_effects.silence_probe.availability, unknown);
    }
}
