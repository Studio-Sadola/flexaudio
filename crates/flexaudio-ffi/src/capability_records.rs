//! Stable numeric codes and copyable capability views. Unknown codes never imply support.
use crate::v2_storage::FlexErrorInfoV2;

pub const FLEX_CAP_OTHER: u32 = 4294967295;
pub const FLEX_CAP_SUPPORTED: u32 = 1;
pub const FLEX_CAP_UNAVAILABLE: u32 = 2;
pub const FLEX_CAP_UNKNOWN: u32 = 3;
pub const FLEX_CAP_REASON_NONE: u32 = 0;
pub const FLEX_CAP_UNSUPPORTED_OS: u32 = 1;
pub const FLEX_CAP_OS_VERSION_TOO_OLD: u32 = 2;
pub const FLEX_CAP_PIPEWIRE_UNAVAILABLE: u32 = 3;
pub const FLEX_CAP_NOT_APPLICABLE: u32 = 4;
pub const FLEX_BACKEND_CPAL: u32 = 1;
pub const FLEX_BACKEND_PIPEWIRE: u32 = 2;
pub const FLEX_BACKEND_WASAPI_LOOPBACK: u32 = 3;
pub const FLEX_BACKEND_WASAPI_PROCESS_LOOPBACK: u32 = 4;
pub const FLEX_BACKEND_CORE_AUDIO_PROCESS_TAP: u32 = 5;
pub const FLEX_BACKEND_COMPOSITE: u32 = 6;
pub const FLEX_BACKEND_UNSUPPORTED: u32 = 7;
pub const FLEX_PID_EXACT: u32 = 1;
pub const FLEX_PID_PROCESS_TREE: u32 = 2;
pub const FLEX_PID_NOT_APPLICABLE: u32 = 3;
pub const FLEX_DEVICE_SELECTION_HONORED: u32 = 1;
pub const FLEX_DEVICE_SELECTION_IGNORED: u32 = 2;
pub const FLEX_DEVICE_SELECTION_NOT_APPLICABLE: u32 = 3;
pub const FLEX_WATCHER_NATIVE: u32 = 1;
pub const FLEX_WATCHER_NOOP: u32 = 2;
pub const FLEX_WATCH_SYSTEM_OUTPUTS: u32 = 1;
pub const FLEX_WATCH_DEFAULT_SYSTEM_OUTPUT: u32 = 2;
pub const FLEX_SIDE_EFFECT_SILENCE_PROBE: u32 = 1;
pub const FLEX_SIDE_EFFECT_CLASSIC_LOOPBACK_KEEPALIVE: u32 = 2;
pub const FLEX_CONDITION_SILENT_ACTIVE_SYSTEM_AUDIO: u32 = 1;
pub const FLEX_CONDITION_CLASSIC_LOOPBACK_WITHOUT_EXCLUSIONS: u32 = 2;

/// Optional version components are present only when their has_* word equals 1.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FlexOsVersionV1 {
    pub major: u32,
    pub minor: u32,
    pub has_patch: u32,
    pub patch: u32,
    pub has_build: u32,
    pub build: u32,
}

/// Availability with optional OS prerequisite and borrowed structured query error.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FlexAvailabilityV1 {
    pub status: u32,
    pub reason: u32,
    /// 1 only for OS_VERSION_TOO_OLD, otherwise required/actual are absent.
    pub has_os_versions: u32,
    pub required: FlexOsVersionV1,
    pub actual: FlexOsVersionV1,
    /// Borrowed until capabilities_free_v1. Never free this error separately.
    pub unknown: *const FlexErrorInfoV2,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct FlexSourceCapabilityV1 {
    pub backend: u32,
    pub availability: FlexAvailabilityV1,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct FlexSystemExclusionCapabilitiesV1 {
    pub backend: u32,
    pub availability: FlexAvailabilityV1,
    pub scope: u32,
    /// 0 means no distinct PID count limit; 1 means max_distinct_pids is present.
    pub has_max_distinct_pids: u32,
    pub max_distinct_pids: u32,
    pub device_selection: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct FlexProcessCapabilitiesV1 {
    pub include_scope: u32,
    pub exclude_scope: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct FlexDeviceWatcherCapabilitiesV1 {
    pub availability: FlexAvailabilityV1,
    pub mode: u32,
    /// Read individual scopes with capabilities_watcher_scope_v1.
    pub scopes_len: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct FlexSideEffectCapabilityV1 {
    pub availability: FlexAvailabilityV1,
    /// Only 1 means enabled; unknown future conditions fail closed.
    pub enabled_by_default: u32,
    /// Read individual kinds with capabilities_side_effect_source_v1.
    pub sources_len: usize,
    pub condition: u32,
}
