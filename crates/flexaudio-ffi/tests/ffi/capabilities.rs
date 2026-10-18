//! Complete capability projection without native probing or capture access.
use super::*;
use crate::{capability_storage::FlexCapabilitiesV1, v2::flexaudio_error_kind_v2};
use flexaudio::*;
use std::{mem::MaybeUninit, ptr};

fn fixture() -> FlexCapabilitiesV1 {
    let required = OsVersion::new(14, 2, None, None);
    let actual = OsVersion::new(13, 1, Some(0), Some(42));
    let unknown = || Availability::Unknown {
        error: Error::Backend("capability query failed".into())
            .with_context(ErrorContext::new(Operation::Enumerate)),
    };
    let side = |condition| {
        SideEffectCapability::new(
            unknown(),
            true,
            vec![SourceKind::SystemLoopback, SourceKind::Mix],
            condition,
        )
    };
    FlexCapabilitiesV1::new(Capabilities::new(
        Some(actual.clone()),
        SourceCapabilities::new(
            SourceCapability::new(BackendKind::Cpal, Availability::Supported),
            SourceCapability::new(
                BackendKind::CoreAudioProcessTap,
                Availability::Unavailable {
                    reason: UnavailableReason::OsVersionTooOld { required, actual },
                },
            ),
            SourceCapability::new(BackendKind::PipeWire, unknown()),
            SourceCapability::new(
                BackendKind::Composite,
                Availability::Unavailable {
                    reason: UnavailableReason::UnsupportedOs,
                },
            ),
        ),
        SystemExclusionCapabilities::new(
            Availability::Unavailable {
                reason: UnavailableReason::PipeWireUnavailable,
            },
            BackendKind::WasapiProcessLoopback,
            PidScope::ProcessTree,
            Some(1),
            DeviceSelectionBehavior::Ignored,
        ),
        ProcessCapabilities::new(PidScope::ExactPid, PidScope::ProcessTree),
        DeviceWatcherCapabilities::new(
            Availability::Supported,
            WatcherMode::Native,
            vec![WatchScope::SystemOutputs, WatchScope::DefaultSystemOutput],
        ),
        SideEffectCapabilities::new(
            side(SideEffectCondition::SilentActiveSystemAudio),
            side(SideEffectCondition::ClassicLoopbackWithoutExclusions),
        ),
    ))
}

#[test]
fn every_snapshot_field_and_borrowed_error_is_projected() {
    unsafe {
        let owner = Box::into_raw(Box::new(fixture()));
        let mut version = MaybeUninit::uninit();
        assert_eq!(
            flexaudio_capabilities_os_version_v1(owner, version.as_mut_ptr()),
            1
        );
        let version = version.assume_init();
        assert_eq!(
            (
                version.major,
                version.minor,
                version.has_patch,
                version.patch,
                version.has_build,
                version.build
            ),
            (13, 1, 1, 0, 1, 42)
        );
        let mut source = MaybeUninit::uninit();
        for (kind, backend, status) in [
            (0, FLEX_BACKEND_CPAL, FLEX_CAP_SUPPORTED),
            (1, FLEX_BACKEND_CORE_AUDIO_PROCESS_TAP, FLEX_CAP_UNAVAILABLE),
            (2, FLEX_BACKEND_PIPEWIRE, FLEX_CAP_UNKNOWN),
            (3, FLEX_BACKEND_COMPOSITE, FLEX_CAP_UNAVAILABLE),
        ] {
            assert_eq!(
                flexaudio_capabilities_source_v1(owner, kind, source.as_mut_ptr()),
                1
            );
            let source = source.assume_init();
            assert_eq!(
                (source.backend, source.availability.status),
                (backend, status)
            );
            match kind {
                0 => assert!(source.availability.unknown.is_null()),
                1 => {
                    let a = source.availability;
                    assert_eq!(
                        (a.reason, a.has_os_versions),
                        (FLEX_CAP_OS_VERSION_TOO_OLD, 1)
                    );
                    assert_eq!(
                        (
                            a.required.major,
                            a.required.minor,
                            a.required.has_patch,
                            a.required.has_build
                        ),
                        (14, 2, 0, 0)
                    );
                    assert_eq!((a.actual.major, a.actual.minor), (13, 1));
                }
                2 => {
                    let borrowed = source.availability.unknown;
                    assert!(!borrowed.is_null());
                    // Unrelated failures replace TLS, never the borrowed snapshot error tree.
                    flexaudio_capabilities_source_v1(owner, 99, ptr::null_mut());
                    let mut code = 0;
                    assert_eq!(flexaudio_error_kind_v2(borrowed, &mut code), 1);
                    assert_eq!(code, error::code::FLEX_FAILURE);
                    let mut count = 0;
                    assert_eq!(
                        crate::v2::flexaudio_error_context_count_v2(borrowed, &mut count),
                        1
                    );
                    assert_eq!(count, 1);
                }
                _ => assert_eq!(source.availability.reason, FLEX_CAP_UNSUPPORTED_OS),
            }
        }
        let mut exclusions = MaybeUninit::uninit();
        assert_eq!(
            flexaudio_capabilities_system_exclusions_v1(owner, exclusions.as_mut_ptr()),
            1
        );
        let e = exclusions.assume_init();
        assert_eq!(
            (
                e.backend,
                e.availability.status,
                e.availability.reason,
                e.scope,
                e.has_max_distinct_pids,
                e.max_distinct_pids,
                e.device_selection
            ),
            (
                FLEX_BACKEND_WASAPI_PROCESS_LOOPBACK,
                FLEX_CAP_UNAVAILABLE,
                FLEX_CAP_PIPEWIRE_UNAVAILABLE,
                FLEX_PID_PROCESS_TREE,
                1,
                1,
                FLEX_DEVICE_SELECTION_IGNORED
            )
        );
        let mut process = MaybeUninit::uninit();
        assert_eq!(
            flexaudio_capabilities_process_v1(owner, process.as_mut_ptr()),
            1
        );
        let p = process.assume_init();
        assert_eq!(
            (p.include_scope, p.exclude_scope),
            (FLEX_PID_EXACT, FLEX_PID_PROCESS_TREE)
        );
        let mut watcher = MaybeUninit::uninit();
        assert_eq!(
            flexaudio_capabilities_watcher_v1(owner, watcher.as_mut_ptr()),
            1
        );
        let w = watcher.assume_init();
        assert_eq!(
            (w.mode, w.scopes_len, w.availability.status),
            (FLEX_WATCHER_NATIVE, 2, FLEX_CAP_SUPPORTED)
        );
        let mut scope = 0;
        for (index, expected) in [
            (0, FLEX_WATCH_SYSTEM_OUTPUTS),
            (1, FLEX_WATCH_DEFAULT_SYSTEM_OUTPUT),
        ] {
            assert_eq!(
                flexaudio_capabilities_watcher_scope_v1(owner, index, &mut scope),
                1
            );
            assert_eq!(scope, expected);
        }
        for (kind, condition) in [
            (
                FLEX_SIDE_EFFECT_SILENCE_PROBE,
                FLEX_CONDITION_SILENT_ACTIVE_SYSTEM_AUDIO,
            ),
            (
                FLEX_SIDE_EFFECT_CLASSIC_LOOPBACK_KEEPALIVE,
                FLEX_CONDITION_CLASSIC_LOOPBACK_WITHOUT_EXCLUSIONS,
            ),
        ] {
            let mut effect = MaybeUninit::uninit();
            assert_eq!(
                flexaudio_capabilities_side_effect_v1(owner, kind, effect.as_mut_ptr()),
                1
            );
            let effect = effect.assume_init();
            assert_eq!(
                (
                    effect.condition,
                    effect.enabled_by_default,
                    effect.sources_len,
                    effect.availability.status
                ),
                (condition, 1, 2, FLEX_CAP_UNKNOWN)
            );
            let mut code = 0;
            assert_eq!(
                flexaudio_error_kind_v2(effect.availability.unknown, &mut code),
                1
            );
            for (index, expected) in [(0, 1), (1, 3)] {
                let mut source = -1;
                assert_eq!(
                    flexaudio_capabilities_side_effect_source_v1(owner, kind, index, &mut source),
                    1
                );
                assert_eq!(source, expected);
            }
        }
        flexaudio_capabilities_free_v1(owner);
        flexaudio_capabilities_free_v1(ptr::null_mut());
    }
}

#[test]
fn absent_versions_unlimited_pids_and_not_applicable_stay_distinct() {
    unsafe {
        let mut snapshot = fixture().snapshot;
        snapshot.os_version = None;
        snapshot.system_exclusions.max_distinct_pids = None;
        snapshot.system_exclusions.availability = Availability::Unavailable {
            reason: UnavailableReason::NotApplicable,
        };
        snapshot.system_exclusions.device_selection = DeviceSelectionBehavior::NotApplicable;
        snapshot.system_exclusions.scope = PidScope::NotApplicable;
        snapshot.system_exclusions.backend = BackendKind::Unsupported;
        snapshot.device_watcher.mode = WatcherMode::Noop;
        snapshot.device_watcher.scopes.clear();
        let owner = FlexCapabilitiesV1::new(snapshot);
        let mut version = FlexOsVersionV1 {
            major: 999,
            ..Default::default()
        };
        assert_eq!(
            flexaudio_capabilities_os_version_v1(&owner, &mut version),
            0
        );
        assert_eq!(version.major, 999);
        let mut e = MaybeUninit::uninit();
        assert_eq!(
            flexaudio_capabilities_system_exclusions_v1(&owner, e.as_mut_ptr()),
            1
        );
        let e = e.assume_init();
        assert_eq!(
            (
                e.has_max_distinct_pids,
                e.availability.reason,
                e.scope,
                e.backend,
                e.device_selection
            ),
            (
                0,
                FLEX_CAP_NOT_APPLICABLE,
                FLEX_PID_NOT_APPLICABLE,
                FLEX_BACKEND_UNSUPPORTED,
                FLEX_DEVICE_SELECTION_NOT_APPLICABLE
            )
        );
        let mut w = MaybeUninit::uninit();
        assert_eq!(flexaudio_capabilities_watcher_v1(&owner, w.as_mut_ptr()), 1);
        let w = w.assume_init();
        assert_eq!((w.mode, w.scopes_len), (FLEX_WATCHER_NOOP, 0));
    }
}

#[test]
fn null_misaligned_unknown_codes_and_indices_fail_closed() {
    unsafe {
        let owner = fixture();
        let mut source = MaybeUninit::uninit();
        assert_eq!(
            flexaudio_capabilities_v1(ptr::null_mut()),
            code::FLEX_INVALID_ARG
        );
        assert_eq!(
            flexaudio_capabilities_v1(ptr::dangling_mut::<u8>().cast()),
            code::FLEX_INVALID_ARG
        );
        assert_eq!(
            flexaudio_capabilities_source_v1(ptr::null(), 0, source.as_mut_ptr()),
            code::FLEX_INVALID_ARG
        );
        assert_eq!(
            flexaudio_capabilities_source_v1(ptr::dangling::<u8>().cast(), 0, source.as_mut_ptr()),
            code::FLEX_INVALID_ARG
        );
        assert_eq!(
            flexaudio_capabilities_source_v1(&owner, 0, ptr::dangling_mut::<u8>().cast()),
            code::FLEX_INVALID_ARG
        );
        for kind in [-1, 4, i32::MAX] {
            assert_eq!(
                flexaudio_capabilities_source_v1(&owner, kind, source.as_mut_ptr()),
                code::FLEX_INVALID_ARG
            );
        }
        let mut word = 123;
        assert_eq!(
            flexaudio_capabilities_watcher_scope_v1(&owner, usize::MAX, &mut word),
            code::FLEX_INVALID_ARG
        );
        assert_eq!(word, 123);
        let mut effect = MaybeUninit::uninit();
        for kind in [0, 3, FLEX_CAP_OTHER] {
            assert_eq!(
                flexaudio_capabilities_side_effect_v1(&owner, kind, effect.as_mut_ptr()),
                code::FLEX_INVALID_ARG
            );
        }
        let mut kind = 123;
        assert_eq!(
            flexaudio_capabilities_side_effect_source_v1(
                &owner,
                FLEX_SIDE_EFFECT_SILENCE_PROBE,
                2,
                &mut kind
            ),
            code::FLEX_INVALID_ARG
        );
        assert_eq!(kind, 123);
        assert_eq!(
            projection::backend(&BackendKind::WasapiLoopback),
            FLEX_BACKEND_WASAPI_LOOPBACK
        );
        assert_eq!(
            projection::backend(&BackendKind::WasapiProcessLoopback),
            FLEX_BACKEND_WASAPI_PROCESS_LOOPBACK
        );
        let mut e = owner.snapshot.system_exclusions.clone();
        e.device_selection = DeviceSelectionBehavior::Honored;
        let mut snapshot = owner.snapshot.clone();
        snapshot.system_exclusions = e;
        let owner = FlexCapabilitiesV1::new(snapshot);
        let mut e = MaybeUninit::uninit();
        flexaudio_capabilities_system_exclusions_v1(&owner, e.as_mut_ptr());
        assert_eq!(
            e.assume_init().device_selection,
            FLEX_DEVICE_SELECTION_HONORED
        );
    }
}
