//! Shared helpers for the macOS backends: PID → AudioObjectID translation, thin wrappers over
//! `AudioObjectGetPropertyData`, reading `(rate, channels)` and the float check from the ASBD,
//! `OSStatus` → [`Error`] conversion, and the monotonic clock.
//!
//! [`MacSystemBackend`](crate::MacSystemBackend) and
//! [`MacProcessBackend`](crate::MacProcessBackend) both run the [`tap`](crate::tap) chain
//! (process tap → aggregate device → IOProc). The only difference between them is how the
//! [`CATapDescription`] is built (INCLUDE = mixdown / EXCLUDE = global), so everything from
//! tap creation through aggregate, IOProc, and teardown is shared.

use std::ffi::c_void;
use std::ptr::NonNull;

use flexaudio_core::clock::monotonic_now_ns;
use flexaudio_core::types::Error;

use objc2_core_audio::{
    kAudioHardwareBadPropertySizeError, kAudioHardwarePropertyTranslatePIDToProcessObject,
    kAudioObjectPropertyElementMain, kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject,
    kAudioTapPropertyFormat, AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize,
    AudioObjectID, AudioObjectPropertyAddress,
};
use objc2_core_audio_types::{kAudioFormatFlagIsFloat, AudioStreamBasicDescription};
use objc2_core_foundation::{CFRetained, CFString};

/// CoreAudio's `OSStatus` success value `noErr`.
pub(crate) const NO_ERR: i32 = 0;

/// Fallback `(48000, 2)` used when getting the format fails.
pub(crate) const FALLBACK_FORMAT: (u32, u16) = (48_000, 2);

/// Monotonic clock (ns). The downstream `ClockNormalizer` takes the origin on first use, so
/// a monotonic approximation of the arrival time is sufficient here.
pub(crate) fn now_ns() -> i64 {
    monotonic_now_ns()
}

/// Converts an `OSStatus` into an [`Error`] with a context string.
///
/// Maps permission-denied codes (`kAudioHardwareIllegalOperationError`, returned when TCC
/// rejects tap creation) to [`Error::PermissionDenied`] and device-missing codes
/// (`kAudioHardwareBadDeviceError`) to [`Error::DeviceNotFound`], keeping the error kinds
/// consistent with the other OSes. The policy is to leave definitive permission decisions to
/// the OS prompt at the first capture (no private TCC SPI), so this only maps "looks like a
/// denial" codes on a best-effort basis. Everything else is [`Error::Backend`].
pub(crate) fn map_os_status(ctx: &str, status: i32) -> Error {
    // Common CoreAudio OSStatus values (4cc).
    // 'who?' = kAudioHardwareUnknownPropertyError, '!obj' = kAudioHardwareBadObjectError,
    // 'nope' = kAudioHardwareIllegalOperationError, 'stop' = kAudioHardwareNotRunningError,
    // '!dev' = kAudioHardwareBadDeviceError.
    const ILLEGAL_OPERATION: i32 = 0x6e6f7065; // 'nope' — typical when TCC denies access
    const NOT_RUNNING: i32 = 0x73746f70; // 'stop'
    const BAD_OBJECT: i32 = 0x216f626a; // '!obj'
    const BAD_DEVICE: i32 = 0x21646576; // '!dev' — given device missing/invalid

    match status {
        // 'nope' (illegal operation) is also returned when tap/aggregate creation is refused
        // for lack of permission, so map it to PermissionDenied (typical when the OS prompt
        // has not been approved).
        ILLEGAL_OPERATION => Error::PermissionDenied,
        // '!dev' (bad device) corresponds to a missing device/endpoint, so map it to
        // DeviceNotFound.
        BAD_DEVICE => Error::DeviceNotFound,
        NOT_RUNNING => Error::Backend(format!("{ctx}: CoreAudio not running (OSStatus 'stop')")),
        BAD_OBJECT => Error::Backend(format!("{ctx}: bad audio object (OSStatus '!obj')")),
        other => {
            // Make the 4cc readable (4 characters if printable ASCII, otherwise decimal).
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

/// Builds a property address for the given scope and the main element.
fn property_address(selector: u32, scope: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMain,
    }
}

/// Builds a property address for the global scope / main element.
fn global_address(selector: u32) -> AudioObjectPropertyAddress {
    property_address(selector, kAudioObjectPropertyScopeGlobal)
}

/// Reads a CFString property (device name / UID / a process's bundle ID) into a `String`.
/// `None` if it cannot be obtained.
///
/// These properties return a `CFStringRef` with a +1 retain (CF's Copy rule).
/// `CFRetained::from_raw` takes ownership, and drop releases it.
pub(crate) fn read_cfstring_property(
    object: AudioObjectID,
    selector: u32,
    scope: u32,
) -> Option<String> {
    let addr = property_address(selector, scope);
    let mut cf_ref: *const CFString = core::ptr::null();
    let mut size = core::mem::size_of::<*const CFString>() as u32;
    // SAFETY: addr/size are valid locals. out is a pointer-sized region for one CFStringRef.
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
    // SAFETY: cf_ref is a valid CFString returned by the OS with a +1 retain. from_raw takes
    // ownership, and drop releases it when this function returns.
    let cf = unsafe { CFRetained::from_raw(NonNull::new_unchecked(cf_ref as *mut CFString)) };
    Some(cf.to_string())
}

/// Translates a PID into an `AudioObjectID` (process object).
///
/// Queries the system object for `kAudioHardwarePropertyTranslatePIDToProcessObject` with
/// `pid` (i32) as the qualifier. `Ok(0)` means the process is silent/absent and has no
/// corresponding audio object (the caller interprets it as [`Error::DeviceNotFound`] or
/// similar).
pub(crate) fn translate_pid_to_object(pid: i32) -> Result<AudioObjectID, Error> {
    let address = global_address(kAudioHardwarePropertyTranslatePIDToProcessObject);
    let mut out_object: AudioObjectID = 0;
    let mut size = core::mem::size_of::<AudioObjectID>() as u32;

    // SAFETY: address/size/out are valid locals. The qualifier is a valid pointer to pid
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

/// Reads an "array of `AudioObjectID`" property of the system object (the device list of
/// `kAudioHardwarePropertyDevices`, the process object list of
/// `kAudioHardwarePropertyProcessObjectList`, etc.).
///
/// If the list size changes between the size query and the data read, the result can be
/// `kAudioHardwareBadPropertySizeError`, so transient failures are retried a few times. The
/// size passed to `GetPropertyData` is element count × element size (exactly the byte size
/// of the allocated buffer).
///
/// On failure, returns the raw `OSStatus`. Whether to treat it as empty or turn it into a
/// typed error with [`map_os_status`] is up to the caller (device enumeration treats it as
/// empty; process enumeration returns a typed error).
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
    // SAFETY: address/size are valid locals. No qualifier needed (null/0).
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
    // SAFETY: ids is allocated for count elements. data_size is element count × element
    // size.
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
    // Truncate to the number of elements actually written (the list can shrink between the
    // two calls).
    ids.truncate(data_size as usize / elem);
    Ok(ids)
}

/// Reads the tap's `kAudioTapPropertyFormat` (ASBD).
///
/// `None` if it cannot be obtained (the caller uses a fallback). Both rate/channels and
/// `mFormatFlags` (the float check) are read from here.
fn read_tap_asbd(tap_id: AudioObjectID) -> Option<AudioStreamBasicDescription> {
    let address = global_address(kAudioTapPropertyFormat);
    // ASBD has no Default, so zero-initialize it and let the OS fill it in.
    // SAFETY: AudioStreamBasicDescription is a `#[repr(C)]` POD of numeric fields only, so
    // zero-initialization yields a valid value.
    let mut asbd: AudioStreamBasicDescription = unsafe { core::mem::zeroed() };
    let mut size = core::mem::size_of::<AudioStreamBasicDescription>() as u32;

    // SAFETY: address/size/asbd are valid locals. No qualifier needed (null/0).
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

/// Reads `(sample_rate, channels)` from the tap's ASBD. `None` if it cannot be obtained.
pub(crate) fn tap_native_format(tap_id: AudioObjectID) -> Option<(u32, u16)> {
    let asbd = read_tap_asbd(tap_id)?;
    let rate = asbd.mSampleRate as u32;
    let channels = asbd.mChannelsPerFrame as u16;
    if rate == 0 || channels == 0 {
        return None;
    }
    Some((rate, channels))
}

/// Checks whether the tap's ASBD has float samples (`kAudioFormatFlagIsFloat`).
///
/// The IOProc reads samples directly as f32 via `mData as *const f32`, so a non-float tap
/// (int PCM etc.) could cause UB. This is called at build time, and the tap is rejected only
/// when it is confirmed to be non-float. When the ASBD cannot be obtained (`None`), the check
/// is inconclusive, so the caller continues with the fallback behavior (assume float). Taps
/// on real hardware are always float, so there is no need to reject when it cannot be read.
///
/// - `Some(true)`  : the float bit is set.
/// - `Some(false)` : the float bit is not set (non-float, so it should be rejected).
/// - `None`        : the ASBD could not be obtained; inconclusive.
pub(crate) fn tap_format_is_float(tap_id: AudioObjectID) -> Option<bool> {
    let asbd = read_tap_asbd(tap_id)?;
    Some((asbd.mFormatFlags & kAudioFormatFlagIsFloat) != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `map_os_status` converts the common codes as expected.
    #[test]
    fn map_os_status_maps_known_codes() {
        assert!(matches!(
            map_os_status("x", 0x6e6f7065),
            Error::PermissionDenied
        ));
        // '!dev' (bad device) is DeviceNotFound.
        assert!(matches!(
            map_os_status("x", 0x21646576),
            Error::DeviceNotFound
        ));
        assert!(matches!(map_os_status("x", 0x73746f70), Error::Backend(_)));
        // Readable 4cc (printable ASCII).
        let e = map_os_status("ctx", i32::from_be_bytes(*b"abcd"));
        assert!(format!("{e}").contains("abcd"));
    }

    /// The fallback format is `(48000, 2)`, per the contract.
    #[test]
    fn fallback_format_is_48k_stereo() {
        assert_eq!(FALLBACK_FORMAT, (48_000, 2));
    }
}
