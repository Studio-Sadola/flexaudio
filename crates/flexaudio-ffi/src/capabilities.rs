//! Read-only capability accessors; no getter transfers ownership of borrowed errors.
use crate::{
    boundary::{invalid, output},
    capability_records::*,
    capability_storage::{self as projection, FlexCapabilitiesV1},
    convert::{source_kind_from_c, source_kind_to_c, valid_pointer},
    error::{self, code},
    guard_i32,
};
use flexaudio::{
    DeviceSelectionBehavior, SideEffectCapability, SideEffectCondition, WatchScope, WatcherMode,
};

unsafe fn record<T>(
    value: *const FlexCapabilitiesV1,
    out: *mut T,
    get: impl FnOnce(&FlexCapabilitiesV1) -> Result<Option<T>, i32>,
) -> i32 {
    guard_i32(|| {
        error::clear_last_error();
        if !valid_pointer(value) || !valid_pointer(out) {
            return invalid("invalid capability pointer");
        }
        match get(&*value) {
            Ok(Some(record)) => {
                out.write(record);
                1
            }
            Ok(None) => 0,
            Err(status) => status,
        }
    })
}

/// Probe capture support without opening capture, enumerating microphones or playing audio.
/// Returns FLEX_OK and an owned snapshot, including structured unknown query failures.
/// The bounded platform probe can block; GUI hosts should use a worker thread.
/// # Safety
/// out must be writable and aligned. Release the owner with capabilities_free_v1.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_capabilities_v1(out: *mut *mut FlexCapabilitiesV1) -> i32 {
    guard_i32(|| {
        error::clear_last_error();
        if let Err(status) = output(out) {
            return status;
        }
        out.write(Box::into_raw(Box::new(FlexCapabilitiesV1::new(
            flexaudio::capabilities(),
        ))));
        code::FLEX_OK
    })
}

/// Free an owned snapshot and invalidate all borrowed views. NULL is safe.
/// # Safety
/// value must be a live library owner or NULL; release exactly once.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_capabilities_free_v1(value: *mut FlexCapabilitiesV1) {
    guard_i32(|| {
        if value.is_null() {
            return code::FLEX_OK;
        }
        if !valid_pointer(value) {
            return invalid("invalid capabilities handle");
        }
        drop(Box::from_raw(value));
        code::FLEX_OK
    });
}

/// Copy the running OS version: 1 present, 0 unknown, negative invalid argument.
/// All record accessors leave out untouched when absent. Unknown codes never imply support.
/// # Safety
/// value must be a live snapshot; out must be writable and aligned.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_capabilities_os_version_v1(
    value: *const FlexCapabilitiesV1,
    out: *mut FlexOsVersionV1,
) -> i32 {
    record(value, out, |v| {
        Ok(v.snapshot.os_version.as_ref().map(projection::version))
    })
}

/// Copy one source's backend and complete availability. kind uses FLEX_SOURCE_KIND_*.
/// Returns 1 or a negative typed error. Borrowed unknown error lives until snapshot free.
/// # Safety
/// value must be a live snapshot; out must be writable and aligned.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_capabilities_source_v1(
    value: *const FlexCapabilitiesV1,
    kind: i32,
    out: *mut FlexSourceCapabilityV1,
) -> i32 {
    record(value, out, |v| {
        let kind = source_kind_from_c(kind).map_err(|()| code::FLEX_INVALID_ARG)?;
        let (source, index) = match kind {
            flexaudio::SourceKind::Mic => (&v.snapshot.sources.mic, 0),
            flexaudio::SourceKind::SystemLoopback => (&v.snapshot.sources.system, 1),
            flexaudio::SourceKind::ProcessLoopback => (&v.snapshot.sources.process, 2),
            flexaudio::SourceKind::Mix => (&v.snapshot.sources.mix, 3),
        };
        Ok(Some(FlexSourceCapabilityV1 {
            backend: projection::backend(&source.backend),
            availability: v.availability(&source.availability, index),
        }))
    })
}

/// Copy system exclusion availability, backend, PID semantics, limit and device selection.
/// Returns 1 or a negative typed error; all error pointers are borrowed until snapshot free.
/// # Safety
/// value must be a live snapshot; out must be writable and aligned.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_capabilities_system_exclusions_v1(
    value: *const FlexCapabilitiesV1,
    out: *mut FlexSystemExclusionCapabilitiesV1,
) -> i32 {
    record(value, out, |v| {
        let c = &v.snapshot.system_exclusions;
        Ok(Some(FlexSystemExclusionCapabilitiesV1 {
            backend: projection::backend(&c.backend),
            availability: v.availability(&c.availability, 4),
            scope: projection::scope(&c.scope),
            has_max_distinct_pids: u32::from(c.max_distinct_pids.is_some()),
            max_distinct_pids: c.max_distinct_pids.unwrap_or(0),
            device_selection: match c.device_selection {
                DeviceSelectionBehavior::Honored => FLEX_DEVICE_SELECTION_HONORED,
                DeviceSelectionBehavior::Ignored => FLEX_DEVICE_SELECTION_IGNORED,
                DeviceSelectionBehavior::NotApplicable => FLEX_DEVICE_SELECTION_NOT_APPLICABLE,
                _ => FLEX_CAP_OTHER,
            },
        }))
    })
}

/// Copy include/exclude process PID scopes. Returns 1 or a negative typed error.
/// # Safety
/// value must be a live snapshot; out must be writable and aligned.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_capabilities_process_v1(
    value: *const FlexCapabilitiesV1,
    out: *mut FlexProcessCapabilitiesV1,
) -> i32 {
    record(value, out, |v| {
        Ok(Some(FlexProcessCapabilitiesV1 {
            include_scope: projection::scope(&v.snapshot.process.include_scope),
            exclude_scope: projection::scope(&v.snapshot.process.exclude_scope),
        }))
    })
}

/// Copy watcher availability, mode and scope count. Returns 1 or a negative typed error.
/// # Safety
/// value must be a live snapshot; out must be writable and aligned.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_capabilities_watcher_v1(
    value: *const FlexCapabilitiesV1,
    out: *mut FlexDeviceWatcherCapabilitiesV1,
) -> i32 {
    record(value, out, |v| {
        let c = &v.snapshot.device_watcher;
        Ok(Some(FlexDeviceWatcherCapabilitiesV1 {
            availability: v.availability(&c.availability, 5),
            mode: match c.mode {
                WatcherMode::Native => FLEX_WATCHER_NATIVE,
                WatcherMode::Noop => FLEX_WATCHER_NOOP,
                _ => FLEX_CAP_OTHER,
            },
            scopes_len: c.scopes.len(),
        }))
    })
}

/// Read an indexed watcher scope. Returns 1 or a negative typed error (including out of range).
/// # Safety
/// value must be a live snapshot; out must be writable and aligned.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_capabilities_watcher_scope_v1(
    value: *const FlexCapabilitiesV1,
    index: usize,
    out: *mut u32,
) -> i32 {
    record(value, out, |v| {
        let scope = v
            .snapshot
            .device_watcher
            .scopes
            .get(index)
            .ok_or_else(|| invalid("watcher scope index out of range"))?;
        Ok(Some(match scope {
            WatchScope::SystemOutputs => FLEX_WATCH_SYSTEM_OUTPUTS,
            WatchScope::DefaultSystemOutput => FLEX_WATCH_DEFAULT_SYSTEM_OUTPUT,
            _ => FLEX_CAP_OTHER,
        }))
    })
}

fn effect(snapshot: &FlexCapabilitiesV1, kind: u32) -> Result<(&SideEffectCapability, usize), i32> {
    match kind {
        FLEX_SIDE_EFFECT_SILENCE_PROBE => Ok((&snapshot.snapshot.side_effects.silence_probe, 6)),
        FLEX_SIDE_EFFECT_CLASSIC_LOOPBACK_KEEPALIVE => Ok((
            &snapshot.snapshot.side_effects.classic_loopback_keepalive,
            7,
        )),
        _ => Err(invalid("unknown side effect kind")),
    }
}

/// Copy side effect availability, default state, source count and triggering condition.
/// kind uses FLEX_SIDE_EFFECT_*. Returns 1 or a negative typed error.
/// # Safety
/// value must be a live snapshot; out must be writable and aligned.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_capabilities_side_effect_v1(
    value: *const FlexCapabilitiesV1,
    kind: u32,
    out: *mut FlexSideEffectCapabilityV1,
) -> i32 {
    record(value, out, |v| {
        let (c, index) = effect(v, kind)?;
        let condition = match c.condition {
            SideEffectCondition::SilentActiveSystemAudio => {
                FLEX_CONDITION_SILENT_ACTIVE_SYSTEM_AUDIO
            }
            SideEffectCondition::ClassicLoopbackWithoutExclusions => {
                FLEX_CONDITION_CLASSIC_LOOPBACK_WITHOUT_EXCLUSIONS
            }
            _ => FLEX_CAP_OTHER,
        };
        Ok(Some(FlexSideEffectCapabilityV1 {
            availability: v.availability(&c.availability, index),
            enabled_by_default: u32::from(c.enabled_by_default && condition != FLEX_CAP_OTHER),
            sources_len: c.sources.len(),
            condition,
        }))
    })
}

/// Read a side effect source as FLEX_SOURCE_KIND_*. Invalid kind/index returns InvalidArg.
/// # Safety
/// value must be a live snapshot; out must be writable and aligned.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_capabilities_side_effect_source_v1(
    value: *const FlexCapabilitiesV1,
    kind: u32,
    index: usize,
    out: *mut i32,
) -> i32 {
    record(value, out, |v| {
        let (c, _) = effect(v, kind)?;
        let source = c
            .sources
            .get(index)
            .ok_or_else(|| invalid("side effect source index out of range"))?;
        Ok(Some(source_kind_to_c(*source)))
    })
}

#[cfg(test)]
#[path = "../tests/ffi/capabilities.rs"]
mod tests;
