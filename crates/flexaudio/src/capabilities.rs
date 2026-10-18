//! Assemble the current platform's read-only capture capability snapshot.

use flexaudio_core::Capabilities;

/// Return an infallible snapshot of backend support and optional side effects.
///
/// This query never enumerates microphones, requests recording permission, opens
/// audio streams, or plays audio. Supported describes platform prerequisites,
/// not device availability or recording permission. Query failures are retained
/// as [`Availability::Unknown`](crate::Availability::Unknown).
///
/// On Linux, one bounded PipeWire connection and server sync checks the shared
/// system/process prerequisite. A timed-out native connection may finish later;
/// further queries report unknown while that single query remains in progress.
pub fn capabilities() -> Capabilities {
    platform_capabilities()
}

#[cfg(target_os = "linux")]
fn platform_capabilities() -> Capabilities {
    flexaudio_os_linux::capabilities()
}

#[cfg(target_os = "windows")]
fn platform_capabilities() -> Capabilities {
    flexaudio_os_windows::capabilities()
}

#[cfg(target_os = "macos")]
fn platform_capabilities() -> Capabilities {
    flexaudio_os_macos::capabilities()
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
fn platform_capabilities() -> Capabilities {
    use flexaudio_core::{
        Availability, BackendKind, DeviceSelectionBehavior, DeviceWatcherCapabilities, PidScope,
        ProcessCapabilities, SideEffectCapabilities, SideEffectCapability, SideEffectCondition,
        SourceCapabilities, SourceCapability, SystemExclusionCapabilities, UnavailableReason,
        WatcherMode,
    };
    let unsupported = || Availability::Unavailable {
        reason: UnavailableReason::UnsupportedOs,
    };
    let source = || SourceCapability::new(BackendKind::Unsupported, unsupported());
    let side_effect =
        |condition| SideEffectCapability::new(unsupported(), false, vec![], condition);
    Capabilities::new(
        None,
        SourceCapabilities::new(
            SourceCapability::new(BackendKind::Cpal, Availability::Supported),
            source(),
            source(),
            source(),
        ),
        SystemExclusionCapabilities::new(
            unsupported(),
            BackendKind::Unsupported,
            PidScope::NotApplicable,
            None,
            DeviceSelectionBehavior::NotApplicable,
        ),
        ProcessCapabilities::new(PidScope::NotApplicable, PidScope::NotApplicable),
        DeviceWatcherCapabilities::new(unsupported(), WatcherMode::Noop, vec![]),
        SideEffectCapabilities::new(
            side_effect(SideEffectCondition::SilentActiveSystemAudio),
            side_effect(SideEffectCondition::ClassicLoopbackWithoutExclusions),
        ),
    )
}
