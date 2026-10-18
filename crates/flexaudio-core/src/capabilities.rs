//! Capture support and optional side effects, independent of capture permissions.
//!
//! A supported capability describes an implemented backend, not permission to use a
//! particular device or process. Snapshots do not open capture or render streams.

use crate::{Error, SourceKind};

/// Infallible capability snapshot for the current platform.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    /// Running OS version, when it can be read.
    pub os_version: Option<OsVersion>,
    /// Capture without system exclusions.
    pub sources: SourceCapabilities,
    /// System capture with `exclude_self` or `exclude_pids`.
    pub system_exclusions: SystemExclusionCapabilities,
    /// PID matching semantics for process capture.
    pub process: ProcessCapabilities,
    /// Device notification support.
    pub device_watcher: DeviceWatcherCapabilities,
    /// Optional capture-related audio rendering.
    pub side_effects: SideEffectCapabilities,
}

impl Capabilities {
    /// Construct a platform snapshot from independently reported capabilities.
    pub fn new(
        os_version: Option<OsVersion>,
        sources: SourceCapabilities,
        system_exclusions: SystemExclusionCapabilities,
        process: ProcessCapabilities,
        device_watcher: DeviceWatcherCapabilities,
        side_effects: SideEffectCapabilities,
    ) -> Self {
        Self {
            os_version,
            sources,
            system_exclusions,
            process,
            device_watcher,
            side_effects,
        }
    }
}

/// Numeric OS version; absent components were not reported by the OS.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OsVersion {
    /// Major version.
    pub major: u32,
    /// Minor version.
    pub minor: u32,
    /// Patch version, if known.
    pub patch: Option<u32>,
    /// Build number, if known.
    pub build: Option<u32>,
}

impl OsVersion {
    /// Construct a version without inventing unreported components.
    pub fn new(major: u32, minor: u32, patch: Option<u32>, build: Option<u32>) -> Self {
        Self {
            major,
            minor,
            patch,
            build,
        }
    }
}

/// Capture sources without system PID exclusions.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceCapabilities {
    /// Microphone capture.
    pub mic: SourceCapability,
    /// System output capture without exclusions.
    pub system: SourceCapability,
    /// Individual process capture.
    pub process: SourceCapability,
    /// Microphone and system mix without exclusions.
    pub mix: SourceCapability,
}

impl SourceCapabilities {
    /// Construct the four source descriptions.
    pub fn new(
        mic: SourceCapability,
        system: SourceCapability,
        process: SourceCapability,
        mix: SourceCapability,
    ) -> Self {
        Self {
            mic,
            system,
            process,
            mix,
        }
    }
}

/// Backend and availability of one capture source.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceCapability {
    /// Implementing backend.
    pub backend: BackendKind,
    /// Current support or query failure.
    pub availability: Availability,
}

impl SourceCapability {
    /// Construct a source description.
    pub fn new(backend: BackendKind, availability: Availability) -> Self {
        Self {
            backend,
            availability,
        }
    }
}

/// Implementation used for a capability.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendKind {
    /// CPAL microphone capture.
    Cpal,
    /// PipeWire capture.
    PipeWire,
    /// WASAPI render-endpoint loopback.
    WasapiLoopback,
    /// WASAPI process-tree loopback.
    WasapiProcessLoopback,
    /// Core Audio process taps.
    CoreAudioProcessTap,
    /// Microphone and system capture composed by the facade.
    Composite,
    /// No implementation on this platform.
    Unsupported,
}

/// Capability availability, distinct from device or recording permission.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    /// Implemented and any platform prerequisite checked successfully.
    Supported,
    /// A known prerequisite is absent.
    Unavailable {
        /// Confirmed reason for lack of support.
        reason: UnavailableReason,
    },
    /// The prerequisite could not be determined.
    Unknown {
        /// Typed query failure; diagnostic details remain opt-in.
        error: Error,
    },
}

/// Confirmed reason why a capability cannot be used.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnavailableReason {
    /// No implementation for this OS.
    UnsupportedOs,
    /// The OS predates the backend's minimum version.
    OsVersionTooOld {
        /// Minimum required version.
        required: OsVersion,
        /// Running version.
        actual: OsVersion,
    },
    /// A PipeWire server is confirmed unavailable.
    PipeWireUnavailable,
    /// This feature does not apply on the platform.
    NotApplicable,
}

/// How a PID identifies captured or excluded audio.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PidScope {
    /// Only the specified PID.
    ExactPid,
    /// The root PID and its process tree.
    ProcessTree,
    /// No applicable PID operation.
    NotApplicable,
}

/// Treatment of an explicit system output device during exclusion capture.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceSelectionBehavior {
    /// Exclusions and device selection are applied together.
    Honored,
    /// Exclusion capture ignores the selected output device.
    Ignored,
    /// No applicable exclusion backend.
    NotApplicable,
}

/// Support for system capture with `exclude_self` or `exclude_pids`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemExclusionCapabilities {
    /// Availability of the exclusion backend.
    pub availability: Availability,
    /// Implementing backend.
    pub backend: BackendKind,
    /// Meaning of each excluded PID.
    pub scope: PidScope,
    /// Distinct PID limit, including implicit self; `None` means no count limit.
    pub max_distinct_pids: Option<u32>,
    /// Whether output-device selection is honored with exclusions.
    pub device_selection: DeviceSelectionBehavior,
}

impl SystemExclusionCapabilities {
    /// Construct a system exclusion description.
    pub fn new(
        availability: Availability,
        backend: BackendKind,
        scope: PidScope,
        max_distinct_pids: Option<u32>,
        device_selection: DeviceSelectionBehavior,
    ) -> Self {
        Self {
            availability,
            backend,
            scope,
            max_distinct_pids,
            device_selection,
        }
    }
}

/// PID semantics for process-source include and exclude modes.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessCapabilities {
    /// Include-mode PID semantics.
    pub include_scope: PidScope,
    /// Exclude-mode PID semantics.
    pub exclude_scope: PidScope,
}

impl ProcessCapabilities {
    /// Construct process-mode semantics.
    pub fn new(include_scope: PidScope, exclude_scope: PidScope) -> Self {
        Self {
            include_scope,
            exclude_scope,
        }
    }
}

/// Device watcher implementation.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatcherMode {
    /// Native OS notifications.
    Native,
    /// Intentional watcher that emits no notifications.
    Noop,
}

/// Device changes a watcher can report.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchScope {
    /// System output additions and removals.
    SystemOutputs,
    /// Default system output changes.
    DefaultSystemOutput,
}

/// Device watcher support, independent of capture sources.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceWatcherCapabilities {
    /// Availability of notifications.
    pub availability: Availability,
    /// Implementing watcher mode.
    pub mode: WatcherMode,
    /// Reported change domains; empty for a no-op watcher.
    pub scopes: Vec<WatchScope>,
}

impl DeviceWatcherCapabilities {
    /// Construct a watcher description.
    pub fn new(availability: Availability, mode: WatcherMode, scopes: Vec<WatchScope>) -> Self {
        Self {
            availability,
            mode,
            scopes,
        }
    }
}

/// Condition under which an optional audio-rendering side effect occurs.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SideEffectCondition {
    /// Sustained captured silence while system audio is active.
    SilentActiveSystemAudio,
    /// Classic system loopback, including the Mix lane, without PID exclusions.
    ClassicLoopbackWithoutExclusions,
}

/// One optional capture-related side effect.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SideEffectCapability {
    /// Platform availability of this side effect.
    pub availability: Availability,
    /// Whether default capture options enable it.
    pub enabled_by_default: bool,
    /// Capture sources that may trigger it.
    pub sources: Vec<SourceKind>,
    /// Required capture condition.
    pub condition: SideEffectCondition,
}

impl SideEffectCapability {
    /// Construct a side-effect description.
    pub fn new(
        availability: Availability,
        enabled_by_default: bool,
        sources: Vec<SourceKind>,
        condition: SideEffectCondition,
    ) -> Self {
        Self {
            availability,
            enabled_by_default,
            sources,
            condition,
        }
    }
}

/// Optional audio rendering performed by capture backends.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SideEffectCapabilities {
    /// Silence diagnosis through a short active audio probe.
    pub silence_probe: SideEffectCapability,
    /// Silent rendering that keeps classic WASAPI loopback delivering idle packets.
    pub classic_loopback_keepalive: SideEffectCapability,
}

impl SideEffectCapabilities {
    /// Construct the optional side-effect descriptions.
    pub fn new(
        silence_probe: SideEffectCapability,
        classic_loopback_keepalive: SideEffectCapability,
    ) -> Self {
        Self {
            silence_probe,
            classic_loopback_keepalive,
        }
    }
}
