//! C ABI for device hotplug watching.
//!
//! [`FlexWatcher`] is an opaque handle wrapping a [`flexaudio::DeviceWatcher`] and delivers
//! device connects, disconnects, and default changes in a pull-based way
//! ([`flexaudio_watcher_poll`]). It is a separate channel from per-capture-stream events
//! (`flexaudio_poll_event`) and handles per-device occurrences. It fills a parity gap the C ABI
//! had until now (the napi side already has it).
//!
//! Conventions are the same as the rest of the crate (guards absorb panics, NULL checks,
//! failures go to last_error). The strings (`id`/`name`) of the `FlexDeviceEvent` filled by
//! poll are owned by flexaudio and are freed with [`flexaudio_device_event_free`] (do not use
//! C's free).

use std::ffi::CString;
use std::os::raw::c_char;

use flexaudio::{DeviceEvent, DeviceWatcher};

use crate::convert::{source_kind_to_c, string_to_c};
use crate::error::{clear_last_error, code, set_last_error};
use crate::types::FlexSourceKind;
use crate::{guard_i32, guard_ptr};

/// Kind of device hotplug event (corresponds to [`flexaudio::DeviceEvent`]).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlexDeviceEventKind {
    /// A device was added (`device`/`name` etc. are filled).
    Added = 0,
    /// A device was removed (`id` only).
    Removed = 1,
    /// The OS default device changed (`id` and `source_kind`).
    DefaultChanged = 2,
    /// An event that matches none of the known ones (in preparation for future variants).
    Unknown = 3,
}

/// One retrieved device event. Filled by `flexaudio_watcher_poll`.
///
/// Which fields are valid depends on `kind`:
/// - `Added`: `id`/`name` and `source_kind`/`sample_rate`/`channels`/`is_loopback`/`is_default`
///   are all filled (complete information about the added device).
/// - `Removed`: `id` only (`name` is NULL, numbers are 0).
/// - `DefaultChanged`: only `id` and `source_kind` (the side whose default switched).
///
/// `id`/`name` are UTF-8 NUL-terminated strings owned by flexaudio and are freed with
/// [`flexaudio_device_event_free`] (do not use C's free).
#[repr(C)]
pub struct FlexDeviceEvent {
    /// Event kind.
    pub kind: FlexDeviceEventKind,
    /// Stable ID (valid for `Added`/`Removed`/`DefaultChanged`; freed by
    /// `flexaudio_device_event_free`). NULL for `Unknown`.
    pub id: *mut c_char,
    /// Display name (`Added` only; freed by `flexaudio_device_event_free`). NULL otherwise.
    pub name: *mut c_char,
    /// For `Added`, the source kind of the device; for `DefaultChanged`, the side whose
    /// default switched (`Mic` = default source / `System` = default sink). Unused otherwise
    /// (`Mic`).
    pub source_kind: FlexSourceKind,
    /// Native sample rate (`Added` only; 0 otherwise).
    pub sample_rate: u32,
    /// Native channel count (`Added` only; 0 otherwise).
    pub channels: u16,
    /// Loopback (`Added` only).
    pub is_loopback: bool,
    /// OS default device (`Added` only).
    pub is_default: bool,
}

/// Maps a [`DeviceEvent`] to a `FlexDeviceEvent` (ownership of `id`/`name` passes to C).
fn device_event_to_c(ev: DeviceEvent) -> FlexDeviceEvent {
    match ev {
        DeviceEvent::Added(info) => FlexDeviceEvent {
            kind: FlexDeviceEventKind::Added,
            id: string_to_c(info.id),
            name: string_to_c(info.name),
            source_kind: source_kind_to_c(info.source_kind),
            sample_rate: info.sample_rate,
            channels: info.channels,
            is_loopback: info.is_loopback,
            is_default: info.is_default,
        },
        DeviceEvent::Removed { id } => FlexDeviceEvent {
            kind: FlexDeviceEventKind::Removed,
            id: string_to_c(id),
            name: std::ptr::null_mut(),
            source_kind: FlexSourceKind::Mic,
            sample_rate: 0,
            channels: 0,
            is_loopback: false,
            is_default: false,
        },
        DeviceEvent::DefaultChanged { kind, id } => FlexDeviceEvent {
            kind: FlexDeviceEventKind::DefaultChanged,
            id: string_to_c(id),
            name: std::ptr::null_mut(),
            source_kind: source_kind_to_c(kind),
            sample_rate: 0,
            channels: 0,
            is_loopback: false,
            is_default: false,
        },
        // DeviceEvent is #[non_exhaustive]. An unknown kind becomes Unknown rather than being
        // swallowed.
        other => {
            set_last_error(format!("unknown device event: {other:?}"));
            FlexDeviceEvent {
                kind: FlexDeviceEventKind::Unknown,
                id: std::ptr::null_mut(),
                name: std::ptr::null_mut(),
                source_kind: FlexSourceKind::Mic,
                sample_rate: 0,
                channels: 0,
                is_loopback: false,
                is_default: false,
            }
        }
    }
}

/// Opaque device watcher handle. It contains a [`flexaudio::DeviceWatcher`].
/// Create it with `flexaudio_watch_devices` and free it with `flexaudio_watcher_free`.
pub struct FlexWatcher {
    inner: DeviceWatcher,
}

/// Starts watching device hotplug and default changes and returns a watcher handle.
///
/// On Linux, the PipeWire registry is watched persistently. Without PipeWire or on an
/// unsupported OS, it degrades to a no-op and returns a valid handle (hotplug events simply
/// never arrive; poll always returns 0). NULL + last_error only on failure. Free the returned
/// handle with `flexaudio_watcher_free`.
#[no_mangle]
pub extern "C" fn flexaudio_watch_devices() -> *mut FlexWatcher {
    guard_ptr(|| {
        clear_last_error();
        match flexaudio::watch_devices() {
            Ok(inner) => Box::into_raw(Box::new(FlexWatcher { inner })),
            Err(e) => {
                set_last_error(e.to_string());
                std::ptr::null_mut()
            }
        }
    })
}

/// Takes out one device event and fills `out` (non-blocking).
///
/// Returns 1 = got one and filled `out` / 0 = none right now / negative = error. Free the
/// filled `out` with `flexaudio_device_event_free` when done.
///
/// # Safety
/// `w` must be a valid handle and `out` must be a valid `FlexDeviceEvent` write target.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_watcher_poll(
    w: *mut FlexWatcher,
    out: *mut FlexDeviceEvent,
) -> i32 {
    guard_i32(|| {
        clear_last_error();
        let Some(watcher) = w.as_mut() else {
            set_last_error("flexaudio_watcher_poll: watcher pointer is null");
            return code::FLEX_INVALID_ARG;
        };
        if out.is_null() {
            set_last_error("flexaudio_watcher_poll: out pointer is null");
            return code::FLEX_INVALID_ARG;
        }
        match watcher.inner.poll_event() {
            Some(ev) => {
                out.write(device_event_to_c(ev));
                1
            }
            None => 0,
        }
    })
}

/// Frees the `id`/`name` filled by `flexaudio_watcher_poll` and sets them to NULL. Safe for
/// both NULL and double free.
///
/// # Safety
/// `ev` must point to a `FlexDeviceEvent` filled by `flexaudio_watcher_poll` (or be NULL).
#[no_mangle]
pub unsafe extern "C" fn flexaudio_device_event_free(ev: *mut FlexDeviceEvent) {
    guard_i32(|| {
        if let Some(ev) = ev.as_mut() {
            if !ev.id.is_null() {
                drop(CString::from_raw(ev.id));
                ev.id = std::ptr::null_mut();
            }
            if !ev.name.is_null() {
                drop(CString::from_raw(ev.name));
                ev.name = std::ptr::null_mut();
            }
        }
        code::FLEX_OK
    });
}

/// Stops and frees the watcher. NULL-safe.
///
/// # Safety
/// `w` must be a handle returned by `flexaudio_watch_devices` (or NULL).
/// `w` must not be used after it is freed.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_watcher_free(w: *mut FlexWatcher) {
    guard_i32(|| {
        if !w.is_null() {
            // DeviceWatcher's Drop calls stop().
            drop(Box::from_raw(w));
        }
        code::FLEX_OK
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use flexaudio::{DeviceInfo, SourceKind};
    use std::ffi::CStr;

    /// One full watch → poll → free cycle (safe via degradation even without PipeWire).
    #[test]
    fn watch_poll_free_smoke() {
        let w = flexaudio_watch_devices();
        assert!(
            !w.is_null(),
            "watch_devices is designed to degrade and always return a handle"
        );
        let mut ev = std::mem::MaybeUninit::<FlexDeviceEvent>::uninit();
        // When degraded, 0 (none right now). Even if one is obtained, it is never negative.
        let rc = unsafe { flexaudio_watcher_poll(w, ev.as_mut_ptr()) };
        assert!(rc >= 0, "poll returned an error: {rc}");
        if rc == 1 {
            // If one was obtained, free the event.
            unsafe { flexaudio_device_event_free(ev.as_mut_ptr()) };
        }
        unsafe { flexaudio_watcher_free(w) };
    }

    /// A NULL handle or NULL output target is InvalidArg. Freeing NULL is safe.
    #[test]
    fn watcher_null_args() {
        let mut ev = std::mem::MaybeUninit::<FlexDeviceEvent>::uninit();
        assert_eq!(
            unsafe { flexaudio_watcher_poll(std::ptr::null_mut(), ev.as_mut_ptr()) },
            code::FLEX_INVALID_ARG
        );
        let w = flexaudio_watch_devices();
        assert_eq!(
            unsafe { flexaudio_watcher_poll(w, std::ptr::null_mut()) },
            code::FLEX_INVALID_ARG
        );
        unsafe { flexaudio_watcher_free(w) };
        unsafe { flexaudio_watcher_free(std::ptr::null_mut()) };
        unsafe { flexaudio_device_event_free(std::ptr::null_mut()) };
    }

    /// The C conversion and string freeing of each DeviceEvent variant are consistent.
    #[test]
    fn device_event_conversion_and_free() {
        // Added: all fields are filled.
        let added = device_event_to_c(DeviceEvent::Added(DeviceInfo {
            id: "node-1".to_string(),
            name: "Mic A".to_string(),
            source_kind: SourceKind::Mic,
            sample_rate: 48_000,
            channels: 2,
            is_loopback: false,
            is_default: true,
        }));
        assert_eq!(added.kind, FlexDeviceEventKind::Added);
        assert_eq!(added.source_kind, FlexSourceKind::Mic);
        assert_eq!(added.sample_rate, 48_000);
        assert!(added.is_default);
        assert_eq!(
            unsafe { CStr::from_ptr(added.id) }.to_str().unwrap(),
            "node-1"
        );
        assert_eq!(
            unsafe { CStr::from_ptr(added.name) }.to_str().unwrap(),
            "Mic A"
        );
        let mut added = added;
        unsafe { flexaudio_device_event_free(&mut added) };
        assert!(added.id.is_null() && added.name.is_null());

        // Removed: id only, name is NULL.
        let mut removed = device_event_to_c(DeviceEvent::Removed {
            id: "node-2".to_string(),
        });
        assert_eq!(removed.kind, FlexDeviceEventKind::Removed);
        assert!(removed.name.is_null());
        assert_eq!(
            unsafe { CStr::from_ptr(removed.id) }.to_str().unwrap(),
            "node-2"
        );
        unsafe { flexaudio_device_event_free(&mut removed) };

        // DefaultChanged: id + source_kind.
        let mut def = device_event_to_c(DeviceEvent::DefaultChanged {
            kind: SourceKind::SystemLoopback,
            id: "sink-3".to_string(),
        });
        assert_eq!(def.kind, FlexDeviceEventKind::DefaultChanged);
        assert_eq!(def.source_kind, FlexSourceKind::System);
        assert_eq!(
            unsafe { CStr::from_ptr(def.id) }.to_str().unwrap(),
            "sink-3"
        );
        unsafe { flexaudio_device_event_free(&mut def) };
        // A double free is safe.
        unsafe { flexaudio_device_event_free(&mut def) };
    }
}
