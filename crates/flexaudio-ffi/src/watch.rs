//! C ABI for device hot-plug monitoring.
//!
//! [`FlexWatcher`] is an opaque handle around [`flexaudio::DeviceWatcher`]. It delivers device
//! connection, disconnection, and default-change events through the pull API
//! ([`flexaudio_watcher_poll`]). This is separate from capture-stream events
//! (`flexaudio_poll_event`) and covers device-level events. It fills a parity gap in the C ABI (the
//! napi API already had this functionality).
//!
//! Follows the crate's conventions: catch panics with a guard, check for NULL, and report failures
//! through last_error. Strings (`id`/`name`) in `FlexDeviceEvent` filled by poll are owned by
//! flexaudio and must be released with [`flexaudio_device_event_free`] (do not use C `free`).

use std::ffi::CString;
use std::os::raw::c_char;

use flexaudio::{DeviceEvent, DeviceWatcher};

use crate::convert::{source_kind_to_c, string_to_c};
use crate::error::{clear_last_error, code, set_last_error};
use crate::types::FlexSourceKind;
use crate::{guard_i32, guard_ptr};

/// Device connection event kind (corresponds to [`flexaudio::DeviceEvent`]).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlexDeviceEventKind {
    /// A device was added (`device`/`name`, etc. are populated).
    Added = 0,
    /// A device was removed (`id` only).
    Removed = 1,
    /// The OS default device changed (`id` and `source_kind`).
    DefaultChanged = 2,
    /// An event that does not match a known kind (for future variants).
    Unknown = 3,
}

/// One retrieved device event, populated by `flexaudio_watcher_poll`.
///
/// Valid fields depend on `kind`:
/// - `Added`: all of `id`/`name` and `source_kind`/`sample_rate`/`channels`/`is_loopback`/`is_default`
///   are populated (full details for the added device).
/// - `Removed`: only `id` (`name` is NULL and numeric fields are 0).
/// - `DefaultChanged`: only `id` and `source_kind` (the side whose default changed).
///
/// `id`/`name` are UTF-8 NUL-terminated strings owned by flexaudio; release them with
/// [`flexaudio_device_event_free`] (do not use C `free`).
#[repr(C)]
pub struct FlexDeviceEvent {
    /// Event kind.
    pub kind: i32,
    /// Stable ID (valid for `Added`/`Removed`/`DefaultChanged`; release with
    /// `flexaudio_device_event_free`). NULL for `Unknown`.
    pub id: *mut c_char,
    /// Display name (`Added` only; release with `flexaudio_device_event_free`). NULL for other kinds.
    pub name: *mut c_char,
    /// For `Added`, the device source kind. For `DefaultChanged`, the side whose default changed
    /// (`Mic` = default source / `System` = default sink). Unused for other kinds (`Mic`).
    pub source_kind: i32,
    /// Native sample rate (`Added` only; 0 otherwise).
    pub sample_rate: u32,
    /// Native channel count (`Added` only; 0 otherwise).
    pub channels: u16,
    /// Whether this is loopback (`Added` only).
    pub is_loopback: bool,
    /// Whether this is the OS default device (`Added` only).
    pub is_default: bool,
}

/// Convert [`DeviceEvent`] to `FlexDeviceEvent` (transfer ownership of `id`/`name` to C).
fn device_event_to_c(ev: DeviceEvent) -> FlexDeviceEvent {
    match ev {
        DeviceEvent::Added(info) => FlexDeviceEvent {
            kind: FlexDeviceEventKind::Added as i32,
            id: string_to_c(info.id),
            name: string_to_c(info.name),
            source_kind: source_kind_to_c(info.source_kind),
            sample_rate: info.sample_rate,
            channels: info.channels,
            is_loopback: info.is_loopback,
            is_default: info.is_default,
        },
        DeviceEvent::Removed { id } => FlexDeviceEvent {
            kind: FlexDeviceEventKind::Removed as i32,
            id: string_to_c(id),
            name: std::ptr::null_mut(),
            source_kind: FlexSourceKind::Mic as i32,
            sample_rate: 0,
            channels: 0,
            is_loopback: false,
            is_default: false,
        },
        DeviceEvent::DefaultChanged { kind, id } => FlexDeviceEvent {
            kind: FlexDeviceEventKind::DefaultChanged as i32,
            id: string_to_c(id),
            name: std::ptr::null_mut(),
            source_kind: source_kind_to_c(kind.into()),
            sample_rate: 0,
            channels: 0,
            is_loopback: false,
            is_default: false,
        },
        // DeviceEvent is #[non_exhaustive]. Preserve unknown kinds as Unknown; do not discard them.
        DeviceEvent::DefaultCleared { .. } | DeviceEvent::RescanRequired { .. } => {
            // New deltas have no representable v1 payload; use the v2 polling API.
            FlexDeviceEvent {
                kind: FlexDeviceEventKind::Unknown as i32,
                id: std::ptr::null_mut(),
                name: std::ptr::null_mut(),
                source_kind: FlexSourceKind::Mic as i32,
                sample_rate: 0,
                channels: 0,
                is_loopback: false,
                is_default: false,
            }
        }
        _ => {
            set_last_error("unknown device event".to_string());
            FlexDeviceEvent {
                kind: FlexDeviceEventKind::Unknown as i32,
                id: std::ptr::null_mut(),
                name: std::ptr::null_mut(),
                source_kind: FlexSourceKind::Mic as i32,
                sample_rate: 0,
                channels: 0,
                is_loopback: false,
                is_default: false,
            }
        }
    }
}

/// Opaque device watcher handle containing [`flexaudio::DeviceWatcher`]. Create with
/// `flexaudio_watch_devices` and release with `flexaudio_watcher_free`.
pub struct FlexWatcher {
    pub(crate) inner: DeviceWatcher,
}

/// Start monitoring device connection and default changes, then return a watcher handle.
///
/// On Linux, continuously monitor the PipeWire registry. Startup failure returns NULL
/// with `flexaudio_last_error` and typed `flexaudio_last_error_info_v2`, rather than
/// a no-op handle. Unsupported operating systems return a valid no-op handle. Release it with
/// `flexaudio_watcher_free`.
#[no_mangle]
pub extern "C" fn flexaudio_watch_devices() -> *mut FlexWatcher {
    guard_ptr(|| {
        clear_last_error();
        match flexaudio::watch_devices() {
            Ok(inner) => Box::into_raw(Box::new(FlexWatcher { inner })),
            Err(e) => {
                crate::error::set_audio_error(e.clone());
                #[cfg(test)]
                crate::error::record_open_failure(e);
                std::ptr::null_mut()
            }
        }
    })
}

/// Retrieve one device event into `out` (non-blocking).
///
/// Return 1 = retrieved and filled `out` / 0 = none currently available / negative = error. Release
/// a populated `out` with `flexaudio_device_event_free` when finished.
///
/// # Safety
/// `w` must be a valid handle and `out` must point to a valid `FlexDeviceEvent` destination.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_watcher_poll(
    w: *mut FlexWatcher,
    out: *mut FlexDeviceEvent,
) -> i32 {
    guard_i32(|| {
        clear_last_error();
        if !crate::convert::valid_pointer(w) || !crate::convert::valid_pointer(out) {
            return crate::error::set_audio_error(flexaudio::Error::InvalidArg(
                "invalid watcher handle or event destination".into(),
            ));
        }
        let watcher = &mut *w;
        match watcher.inner.poll_event() {
            Some(ev) => {
                out.write(device_event_to_c(ev));
                1
            }
            None => 0,
        }
    })
}

/// Release `id`/`name` populated by `flexaudio_watcher_poll` and set them to NULL. Safe for NULL and
/// repeated calls.
///
/// # Safety
/// `ev` must point to a `FlexDeviceEvent` populated by `flexaudio_watcher_poll`, or be NULL.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_device_event_free(ev: *mut FlexDeviceEvent) {
    guard_i32(|| {
        if !ev.is_null() && !crate::convert::valid_pointer(ev) {
            return crate::error::set_audio_error(flexaudio::Error::InvalidArg(
                "invalid device event pointer".into(),
            ));
        }
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

/// Stop and release the watcher. NULL-safe.
///
/// # Safety
/// `w` must be a handle returned by `flexaudio_watch_devices`, or NULL. Do not use `w` after release.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_watcher_free(w: *mut FlexWatcher) {
    guard_i32(|| {
        if !w.is_null() {
            if !crate::convert::valid_pointer(w) {
                return crate::error::set_audio_error(flexaudio::Error::InvalidArg(
                    "invalid watcher handle".into(),
                ));
            }
            // Dropping DeviceWatcher calls stop().
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

    /// One watch → poll → free cycle, or a typed startup failure without a panic.
    #[test]
    fn watch_poll_free_smoke() {
        if std::env::var("FLEXAUDIO_RUN_NATIVE_TESTS").as_deref() != Ok("1") {
            return;
        }
        let w = flexaudio_watch_devices();
        if w.is_null() {
            let error = crate::error::take_open_failure()
                .expect("NULL must retain a typed watcher startup failure, not a panic");
            let message = crate::error::last_error_ptr();
            assert!(!message.is_null());
            assert_eq!(
                unsafe { CStr::from_ptr(message) }.to_str().unwrap(),
                error.to_string(),
            );
            unsafe { flexaudio_watcher_free(w) };
            return;
        }
        let mut ev = std::mem::MaybeUninit::<FlexDeviceEvent>::uninit();
        // A valid watcher returns either no event or one owned event.
        let rc = unsafe { flexaudio_watcher_poll(w, ev.as_mut_ptr()) };
        unsafe { flexaudio_watcher_free(w) };
        assert!(rc >= 0, "poll returned an error: {rc}");
        if rc == 1 {
            // Release the event if one was retrieved.
            unsafe { flexaudio_device_event_free(ev.as_mut_ptr()) };
        }
    }

    /// NULL handle and NULL output destination are InvalidArg. Freeing NULL is safe.
    #[test]
    fn watcher_null_args() {
        let mut ev = std::mem::MaybeUninit::<FlexDeviceEvent>::uninit();
        assert_eq!(
            unsafe { flexaudio_watcher_poll(std::ptr::null_mut(), ev.as_mut_ptr()) },
            code::FLEX_INVALID_ARG
        );
        assert_eq!(
            unsafe { flexaudio_watcher_poll(std::ptr::null_mut(), std::ptr::null_mut()) },
            code::FLEX_INVALID_ARG
        );
        unsafe { flexaudio_watcher_free(std::ptr::null_mut()) };
        unsafe { flexaudio_device_event_free(std::ptr::null_mut()) };
    }

    #[test]
    fn new_device_deltas_use_unknown_v1_without_fabricated_ids() {
        for event in [
            DeviceEvent::DefaultCleared {
                kind: flexaudio::DefaultDeviceKind::SystemAudio,
            },
            DeviceEvent::RescanRequired {
                dropped_events: u64::MAX,
            },
        ] {
            let mut projected = device_event_to_c(event);
            assert_eq!(projected.kind, FlexDeviceEventKind::Unknown as i32);
            assert!(projected.id.is_null());
            assert!(projected.name.is_null());
            unsafe {
                flexaudio_device_event_free(&mut projected);
            }
        }
    }

    /// C conversion and string release are consistent for each DeviceEvent variant.
    #[test]
    fn device_event_conversion_and_free() {
        // Added: all fields are populated.
        let added = device_event_to_c(DeviceEvent::Added(DeviceInfo {
            id: "node-1".to_string(),
            name: "Mic A".to_string(),
            source_kind: SourceKind::Mic,
            sample_rate: 48_000,
            channels: 2,
            is_loopback: false,
            is_default: true,
        }));
        assert_eq!(added.kind, FlexDeviceEventKind::Added as i32);
        assert_eq!(added.source_kind, FlexSourceKind::Mic as i32);
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

        // Removed: id only; name is NULL.
        let mut removed = device_event_to_c(DeviceEvent::Removed {
            id: "node-2".to_string(),
        });
        assert_eq!(removed.kind, FlexDeviceEventKind::Removed as i32);
        assert!(removed.name.is_null());
        assert_eq!(
            unsafe { CStr::from_ptr(removed.id) }.to_str().unwrap(),
            "node-2"
        );
        unsafe { flexaudio_device_event_free(&mut removed) };

        // DefaultChanged: id + source_kind.
        let mut def = device_event_to_c(DeviceEvent::DefaultChanged {
            kind: flexaudio::DefaultDeviceKind::SystemAudio,
            id: "sink-3".to_string(),
        });
        assert_eq!(def.kind, FlexDeviceEventKind::DefaultChanged as i32);
        assert_eq!(def.source_kind, FlexSourceKind::System as i32);
        assert_eq!(
            unsafe { CStr::from_ptr(def.id) }.to_str().unwrap(),
            "sink-3"
        );
        unsafe { flexaudio_device_event_free(&mut def) };
        // Repeated release is safe.
        unsafe { flexaudio_device_event_free(&mut def) };
    }
}
