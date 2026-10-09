//! Shared helpers for the macOS backend: PID → AudioObjectID conversion, a thin wrapper around
//! `AudioObjectGetPropertyData`, reading `(rate, channels)` and float status from ASBD, converting
//! `OSStatus` to [`Error`], and a monotonic clock.
//!
//! Both [`MacSystemBackend`](crate::MacSystemBackend) and
//! [`MacProcessBackend`](crate::MacProcessBackend) run the [`tap`](crate::tap) chain (process tap
//! → aggregate device → IOProc). They differ only in how they create [`CATapDescription`]
//! (INCLUDE = mixdown / EXCLUDE = global), so tap creation, aggregation, IOProc, and teardown are
//! shared.

use std::ffi::c_void;
use std::ptr::NonNull;

use flexaudio_core::clock::monotonic_now_ns;
use flexaudio_core::types::{Error, Permission};

use objc2_core_audio::{
    kAudioHardwareBadPropertySizeError, kAudioHardwarePropertyTranslatePIDToProcessObject,
    kAudioObjectPropertyElementMain, kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject,
    kAudioTapPropertyFormat, AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize,
    AudioObjectID, AudioObjectPropertyAddress,
};
use objc2_core_audio_types::{kAudioFormatFlagIsFloat, AudioStreamBasicDescription};
use objc2_core_foundation::{CFRetained, CFString};

/// CoreAudio's `OSStatus` success value, `noErr`.
pub(crate) const NO_ERR: i32 = 0;

/// Fallback `(48000, 2)` when format retrieval fails.
pub(crate) const FALLBACK_FORMAT: (u32, u16) = (48_000, 2);

/// Monotonic clock (ns). The downstream `ClockNormalizer` establishes the initial origin, so a
/// monotonic approximation of the arrival time is sufficient here.
pub(crate) fn now_ns() -> i64 {
    monotonic_now_ns()
}

/// Convert `OSStatus` to [`Error`] with context.
///
/// Map permission-denial statuses (`kAudioHardwareIllegalOperationError`, returned when TCC
/// rejects tap creation) to [`Error::PermissionDenied`], and missing-device statuses
/// (`kAudioHardwareBadDeviceError`) to [`Error::DeviceNotFound`] to align error kinds across OSes.
/// The first capture's OS prompt is the authoritative permission check (no private TCC SPI), so
/// this only best-effort maps statuses that look like denials. Other statuses become
/// [`Error::Backend`].
pub(crate) fn map_os_status(ctx: &str, status: i32) -> Error {
    // Representative CoreAudio OSStatus values (four-character codes).
    // 'who?' = kAudioHardwareUnknownPropertyError, '!obj' = kAudioHardwareBadObjectError,
    // 'nope' = kAudioHardwareIllegalOperationError, 'stop' = kAudioHardwareNotRunningError,
    // '!dev' = kAudioHardwareBadDeviceError.
    const ILLEGAL_OPERATION: i32 = 0x6e6f7065; // 'nope' — common when TCC denies access
    const NOT_RUNNING: i32 = 0x73746f70; // 'stop'
    const BAD_OBJECT: i32 = 0x216f626a; // '!obj'
    const BAD_DEVICE: i32 = 0x21646576; // '!dev' — requested device is missing or invalid

    match status {
        // 'nope' (illegal operation) can also mean tap/aggregate creation was denied due to
        // missing permission, so map it to PermissionDenied (typical when the OS prompt is denied).
        ILLEGAL_OPERATION => Error::PermissionDenied {
            permission: Permission::SystemAudio,
            detail: format!("{ctx}: Core Audio rejected the operation (OSStatus 'nope'); system/process recording access may have been denied"),
        },
        // '!dev' (invalid device) indicates the requested device or endpoint is missing, so map it
        // to DeviceNotFound.
        BAD_DEVICE => Error::DeviceNotFound,
        NOT_RUNNING => Error::Backend(format!("{ctx}: CoreAudio not running (OSStatus 'stop')")),
        BAD_OBJECT => Error::Backend(format!("{ctx}: bad audio object (OSStatus '!obj')")),
        other => {
            // Make the four-character code readable (four printable ASCII characters, or decimal).
            let be = (other as u32).to_be_bytes();
            if be.iter().all(|&b| (0x20..=0x7e).contains(&b)) {
                Error::Backend(format!(
                    "{ctx}: OSStatus '{}' ({other})",
                    String::from_utf8_lossy(&be)
                ))
            } else {
                Error::Backend(format!("{ctx}: OSStatus {other}"))
            }
        }
    }
}

/// Create a property address with the given scope and main element.
fn property_address(selector: u32, scope: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMain,
    }
}

/// Create a property address for global scope and the main element.
fn global_address(selector: u32) -> AudioObjectPropertyAddress {
    property_address(selector, kAudioObjectPropertyScopeGlobal)
}

/// Read a CFString property (device name / UID / process bundle ID) as a `String`. Returns `None`
/// if it cannot be read.
///
/// These properties return a `CFStringRef` with +1 retain (the CF Copy rule). Take ownership with
/// `CFRetained::from_raw` and release it on drop.
pub(crate) fn read_cfstring_property(
    object: AudioObjectID,
    selector: u32,
    scope: u32,
) -> Option<String> {
    let addr = property_address(selector, scope);
    let mut cf_ref: *const CFString = core::ptr::null();
    let mut size = core::mem::size_of::<*const CFString>() as u32;
    // SAFETY: `addr`/`size` are valid locals. `out` points to storage for one CFStringRef.
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            NonNull::from(&addr),
            0,
            core::ptr::null(),
            NonNull::from(&mut size),
            NonNull::new_unchecked((&mut cf_ref as *mut *const CFString).cast::<c_void>()),
        )
    };
    if status != NO_ERR || cf_ref.is_null() {
        return None;
    }
    // SAFETY: `cf_ref` is a valid CFString returned by the OS with +1 retain. `from_raw` takes
    // ownership, and drop releases it when this function exits.
    let cf = unsafe { CFRetained::from_raw(NonNull::new_unchecked(cf_ref as *mut CFString)) };
    Some(cf.to_string())
}

/// Convert a PID to an `AudioObjectID` (process object).
///
/// Query the system object with `kAudioHardwarePropertyTranslatePIDToProcessObject` and pass
/// `pid` (i32) as the qualifier. `Ok(0)` means the process is silent or absent and has no
/// corresponding audio object (the caller interprets this as [`Error::DeviceNotFound`] or similar).
pub(crate) fn translate_pid_to_object(pid: i32) -> Result<AudioObjectID, Error> {
    let address = global_address(kAudioHardwarePropertyTranslatePIDToProcessObject);
    let mut out_object: AudioObjectID = 0;
    let mut size = core::mem::size_of::<AudioObjectID>() as u32;

    // SAFETY: `address`/`size`/`out` are valid locals. The qualifier is a valid pointer to `pid`
    // (i32).
    let status = unsafe {
        AudioObjectGetPropertyData(
            kAudioObjectSystemObject as AudioObjectID,
            NonNull::from(&address),
            core::mem::size_of::<i32>() as u32,
            (&pid as *const i32).cast::<c_void>(),
            NonNull::from(&mut size),
            NonNull::new_unchecked((&mut out_object as *mut AudioObjectID).cast::<c_void>()),
        )
    };
    if status != NO_ERR {
        return Err(map_os_status(
            "AudioObjectGetPropertyData(TranslatePIDToProcessObject)",
            status,
        ));
    }
    Ok(out_object)
}

/// Read an array-valued `AudioObjectID` property from the system object (for example, the device
/// list from `kAudioHardwarePropertyDevices` or process objects from
/// `kAudioHardwarePropertyProcessObjectList`).
///
/// If the list changes size between the size query and data read, it may return
/// `kAudioHardwareBadPropertySizeError`, so retry transient failures a few times. Pass the exact
/// allocated buffer size to `GetPropertyData`: element count × element size in bytes.
///
/// On failure, return the raw `OSStatus`. The caller decides whether to treat it as empty or
/// convert it to a typed error with [`map_os_status`] (device enumeration treats it as empty;
/// process enumeration returns a typed error).
pub(crate) fn read_system_object_list(selector: u32) -> Result<Vec<AudioObjectID>, i32> {
    const MAX_ATTEMPTS: u32 = 4;
    let mut last_status = NO_ERR;
    for _ in 0..MAX_ATTEMPTS {
        match read_system_object_list_once(selector) {
            Ok(ids) => return Ok(ids),
            Err(status) if status == kAudioHardwareBadPropertySizeError => {
                last_status = status;
            }
            Err(status) => return Err(status),
        }
    }
    Err(last_status)
}

fn read_system_object_list_once(selector: u32) -> Result<Vec<AudioObjectID>, i32> {
    let address = global_address(selector);
    let mut size: u32 = 0;
    // SAFETY: `address`/`size` are valid locals. No qualifier is needed (null/0).
    let status = unsafe {
        AudioObjectGetPropertyDataSize(
            kAudioObjectSystemObject as AudioObjectID,
            NonNull::from(&address),
            0,
            core::ptr::null(),
            NonNull::from(&mut size),
        )
    };
    if status != NO_ERR {
        return Err(status);
    }
    let elem = core::mem::size_of::<AudioObjectID>();
    let count = size as usize / elem;
    if count == 0 {
        return Ok(Vec::new());
    }
    let mut ids: Vec<AudioObjectID> = vec![0; count];
    let mut data_size = (count * elem) as u32;
    // SAFETY: `ids` has space for `count` elements. `data_size` is element count × element size.
    let status = unsafe {
        AudioObjectGetPropertyData(
            kAudioObjectSystemObject as AudioObjectID,
            NonNull::from(&address),
            0,
            core::ptr::null(),
            NonNull::from(&mut data_size),
            NonNull::new_unchecked(ids.as_mut_ptr().cast::<c_void>()),
        )
    };
    if status != NO_ERR {
        return Err(status);
    }
    // Truncate to the number of elements actually written (the list may shrink between calls).
    ids.truncate(data_size as usize / elem);
    Ok(ids)
}

/// Read the tap's `kAudioTapPropertyFormat` (ASBD).
///
/// Returns `None` if unavailable (the caller uses a fallback). Read both rate/channels and
/// `mFormatFlags` (for float detection) here.
fn read_tap_asbd(tap_id: AudioObjectID) -> Option<AudioStreamBasicDescription> {
    let address = global_address(kAudioTapPropertyFormat);
    // ASBD has no Default, so zero-initialize it and let the OS fill it in.
    // SAFETY: AudioStreamBasicDescription is a `#[repr(C)]` POD with only numeric fields, so zero
    // initialization is valid.
    let mut asbd: AudioStreamBasicDescription = unsafe { core::mem::zeroed() };
    let mut size = core::mem::size_of::<AudioStreamBasicDescription>() as u32;

    // SAFETY: `address`/`size`/`asbd` are valid locals. No qualifier is needed (null/0).
    let status = unsafe {
        AudioObjectGetPropertyData(
            tap_id,
            NonNull::from(&address),
            0,
            core::ptr::null(),
            NonNull::from(&mut size),
            NonNull::new_unchecked(
                (&mut asbd as *mut AudioStreamBasicDescription).cast::<c_void>(),
            ),
        )
    };
    if status != NO_ERR {
        return None;
    }
    Some(asbd)
}

/// Read `(sample_rate, channels)` from the tap's ASBD. Returns `None` if unavailable.
pub(crate) fn tap_native_format(tap_id: AudioObjectID) -> Option<(u32, u16)> {
    let asbd = read_tap_asbd(tap_id)?;
    let rate = asbd.mSampleRate as u32;
    let channels = asbd.mChannelsPerFrame as u16;
    if rate == 0 || channels == 0 {
        return None;
    }
    Some((rate, channels))
}

/// Check whether the tap's ASBD indicates float samples (`kAudioFormatFlagIsFloat`).
///
/// The IOProc reads samples directly as f32 via `mData as *const f32`, so a non-float tap (such
/// as int PCM) could cause UB. Call this during build and reject only when non-float is confirmed.
/// If the ASBD cannot be read (`None`), the format is unknown, so the caller continues with its
/// fallback behavior (assume float). Real hardware taps are always float, so an unavailable ASBD
/// does not need to cause rejection.
///
/// - `Some(true)`: float bit is set.
/// - `Some(false)`: float bit is absent (reject as non-float).
/// - `None`: ASBD could not be read, so the format is unknown.
pub(crate) fn tap_format_is_float(tap_id: AudioObjectID) -> Option<bool> {
    let asbd = read_tap_asbd(tap_id)?;
    Some((asbd.mFormatFlags & kAudioFormatFlagIsFloat) != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `map_os_status` converts representative codes as expected.
    #[test]
    fn map_os_status_maps_known_codes() {
        assert!(matches!(
            map_os_status("x", 0x6e6f7065),
            Error::PermissionDenied {
                permission: Permission::SystemAudio,
                ..
            }
        ));
        // '!dev' (invalid device) maps to DeviceNotFound.
        assert!(matches!(
            map_os_status("x", 0x21646576),
            Error::DeviceNotFound
        ));
        assert!(matches!(map_os_status("x", 0x73746f70), Error::Backend(_)));
        // Four-character-code formatting (printable ASCII).
        let e = map_os_status("ctx", i32::from_be_bytes(*b"abcd"));
        assert!(format!("{e}").contains("abcd"));
    }

    /// The fallback format is `(48000, 2)` as specified by the contract.
    #[test]
    fn fallback_format_is_48k_stereo() {
        assert_eq!(FALLBACK_FORMAT, (48_000, 2));
    }
}

#[cfg(test)]
mod repro_tests {
    use super::*;

    #[test]
    #[ignore = "repro: C F17 / D M7 native context"]
    fn repro_p7mac_bad_device_retains_operation_and_status() {
        let error = map_os_status("AudioDeviceStart", 0x21646576);
        let message = error.to_string();
        assert!(message.contains("AudioDeviceStart"));
        assert!(message.contains("!dev") || message.contains("560227702"));
    }
}
