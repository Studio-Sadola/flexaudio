//! Enumerate output devices and resolve names to UIDs.
//!
//! [`list_output_devices`] enumerates output (playback) devices and returns a list of [`DeviceInfo`].
//! When [`MacSystemBackend`](crate::MacSystemBackend) creates a tap for a specific device, it uses
//! [`uid_for_device_name`] to resolve the public ID (the device name) to a CoreAudio device UID.
//!
//! Match the `DeviceInfo` shape used by all OS backends: `id` and `name` are both the device name,
//! `sample_rate` / `channels` describe the device format, `is_loopback` is always `true` (output
//! monitor), and `is_default` is `true` when this is the default output device.
//!
//! # Why use the name as the ID
//! Output devices have stable UIDs, but the public ID uses the device name to match how other OSes
//! display `DeviceInfo.id`. Resolve the name to a UID internally just before creating the tap. If
//! multiple devices share a name, use the first match.

use std::ffi::c_void;
use std::ptr::NonNull;

use objc2_core_audio::{
    kAudioDevicePropertyDeviceUID, kAudioDevicePropertyNominalSampleRate,
    kAudioDevicePropertyStreamConfiguration, kAudioDevicePropertyStreams,
    kAudioHardwarePropertyDefaultOutputDevice, kAudioHardwarePropertyDevices,
    kAudioObjectPropertyElementMain, kAudioObjectPropertyName, kAudioObjectPropertyScopeGlobal,
    kAudioObjectPropertyScopeOutput, kAudioObjectSystemObject, AudioObjectGetPropertyData,
    AudioObjectGetPropertyDataSize, AudioObjectID, AudioObjectPropertyAddress,
};
use objc2_core_audio_types::AudioBufferList;

use flexaudio_core::types::{DeviceInfo, Result, SourceKind};

use crate::common::{
    map_os_status, read_cfstring_property, read_system_object_list, FALLBACK_FORMAT,
};

/// Create a property address for the given scope and element.
fn address(selector: u32, scope: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMain,
    }
}

/// Read `kAudioHardwarePropertyDevices` from the system object and return all `AudioObjectID`s.
///
/// On failure, return the normalized [`Error`](flexaudio_core::types::Error) from [`map_os_status`].
/// The reader, [`read_system_object_list`], is shared with process enumeration.
///
/// Accept a reader so tests can distinguish retrieval failure from a valid empty list without calling
/// CoreAudio. Production callers always pass [`read_system_object_list`].
type SystemObjectListReader = fn(u32) -> std::result::Result<Vec<AudioObjectID>, i32>;

fn all_device_ids(reader: SystemObjectListReader) -> Result<Vec<AudioObjectID>> {
    reader(kAudioHardwarePropertyDevices)
        .map_err(|status| map_os_status("AudioObjectGetPropertyData(Devices)", status))
}

/// Read the device's `kAudioDevicePropertyNominalSampleRate` (output scope, Float64).
fn device_sample_rate(device: AudioObjectID) -> Option<u32> {
    let addr = address(
        kAudioDevicePropertyNominalSampleRate,
        kAudioObjectPropertyScopeOutput,
    );
    let mut rate: f64 = 0.0;
    let mut size = core::mem::size_of::<f64>() as u32;
    // SAFETY: addr, size, and rate are valid local values.
    let status = unsafe {
        AudioObjectGetPropertyData(
            device,
            NonNull::from(&addr),
            0,
            core::ptr::null(),
            NonNull::from(&mut size),
            NonNull::new_unchecked((&mut rate as *mut f64).cast::<c_void>()),
        )
    };
    if status != 0 || rate <= 0.0 {
        return None;
    }
    Some(rate as u32)
}

/// Count channels in the device's output scope using `kAudioDevicePropertyStreamConfiguration`.
///
/// Sum `AudioBuffer.mNumberChannels` across the `AudioBufferList`. Return `None` on failure.
fn device_channels(device: AudioObjectID) -> Option<u16> {
    let addr = address(
        kAudioDevicePropertyStreamConfiguration,
        kAudioObjectPropertyScopeOutput,
    );
    let mut size: u32 = 0;
    // SAFETY: addr and size are valid local values.
    let status = unsafe {
        AudioObjectGetPropertyDataSize(
            device,
            NonNull::from(&addr),
            0,
            core::ptr::null(),
            NonNull::from(&mut size),
        )
    };
    if status != 0 || size == 0 {
        return None;
    }
    // AudioBufferList is variable-length (`mBuffers` is a trailing flexible array). Allocate a buffer
    // of the reported size, read into it, and interpret the start as an AudioBufferList. It contains
    // pointer fields and requires 8-byte alignment. A Vec<u8> (alignment 1) could make reading the OS-
    // written list an unaligned reference. Allocate a Vec<AudioBufferList> instead to guarantee
    // alignment, with enough elements to cover the full reported size.
    let elem = core::mem::size_of::<AudioBufferList>();
    let count = (size as usize).div_ceil(elem).max(1);
    // SAFETY: AudioBufferList is a #[repr(C)] POD containing only numbers and pointers, so zero-init is valid.
    let mut storage: Vec<AudioBufferList> = vec![unsafe { core::mem::zeroed() }; count];
    // SAFETY: `storage` is at least `size` bytes and is 8-byte aligned.
    let status = unsafe {
        AudioObjectGetPropertyData(
            device,
            NonNull::from(&addr),
            0,
            core::ptr::null(),
            NonNull::from(&mut size),
            NonNull::new_unchecked(storage.as_mut_ptr().cast::<c_void>()),
        )
    };
    if status != 0 {
        return None;
    }
    // SAFETY: `storage` starts with the OS-written, properly aligned AudioBufferList, followed by
    // `mNumberBuffers` AudioBuffer entries.
    let list = storage.as_ptr();
    let num_buffers = unsafe { (*list).mNumberBuffers } as usize;
    if num_buffers == 0 {
        return None;
    }
    // SAFETY: `mBuffers` points to the first of `num_buffers` AudioBuffer entries, within the reported size.
    let buffers = unsafe { std::slice::from_raw_parts((*list).mBuffers.as_ptr(), num_buffers) };
    let total: u32 = buffers.iter().map(|b| b.mNumberChannels).sum();
    if total == 0 {
        return None;
    }
    Some(total.min(u16::MAX as u32) as u16)
}

/// Check whether the device is an output (playback) device. At least one stream in the output scope means yes.
fn is_output_device(device: AudioObjectID) -> bool {
    let addr = address(kAudioDevicePropertyStreams, kAudioObjectPropertyScopeOutput);
    let mut size: u32 = 0;
    // SAFETY: addr and size are valid local values.
    let status = unsafe {
        AudioObjectGetPropertyDataSize(
            device,
            NonNull::from(&addr),
            0,
            core::ptr::null(),
            NonNull::from(&mut size),
        )
    };
    status == 0 && size > 0
}

/// Return the default output device's `AudioObjectID`, or `0` if it cannot be retrieved.
fn default_output_device() -> AudioObjectID {
    let addr = address(
        kAudioHardwarePropertyDefaultOutputDevice,
        kAudioObjectPropertyScopeGlobal,
    );
    let mut device: AudioObjectID = 0;
    let mut size = core::mem::size_of::<AudioObjectID>() as u32;
    // SAFETY: addr, size, and device are valid local values.
    let status = unsafe {
        AudioObjectGetPropertyData(
            kAudioObjectSystemObject as AudioObjectID,
            NonNull::from(&addr),
            0,
            core::ptr::null(),
            NonNull::from(&mut size),
            NonNull::new_unchecked((&mut device as *mut AudioObjectID).cast::<c_void>()),
        )
    };
    if status != 0 {
        return 0;
    }
    device
}

/// Read the device name (`kAudioObjectPropertyName`). Return `None` if unavailable.
fn device_name(device: AudioObjectID) -> Option<String> {
    read_cfstring_property(
        device,
        kAudioObjectPropertyName,
        kAudioObjectPropertyScopeGlobal,
    )
}

/// Read the device UID (`kAudioDevicePropertyDeviceUID`). Return `None` if unavailable.
fn device_uid(device: AudioObjectID) -> Option<String> {
    read_cfstring_property(
        device,
        kAudioDevicePropertyDeviceUID,
        kAudioObjectPropertyScopeGlobal,
    )
}

/// Enumerate output (playback) devices.
///
/// Each [`DeviceInfo`]:
/// - `id` / `name`: device name (`kAudioObjectPropertyName`).
/// - `source_kind`: [`SourceKind::SystemLoopback`].
/// - `sample_rate` / `channels`: device output format, or [`FALLBACK_FORMAT`] if unavailable.
/// - `is_loopback`: always `true` (output monitor).
/// - `is_default`: `true` if this is the default output device.
///
/// TCC permission is not needed for enumeration. Skip devices whose names cannot be read.
pub fn list_output_devices() -> Result<Vec<DeviceInfo>> {
    let default_id = default_output_device();
    let mut out: Vec<DeviceInfo> = Vec::new();
    for id in all_device_ids(read_system_object_list)? {
        if !is_output_device(id) {
            continue;
        }
        let Some(name) = device_name(id) else {
            continue;
        };
        let (fallback_rate, fallback_ch) = FALLBACK_FORMAT;
        out.push(DeviceInfo {
            id: name.clone(),
            name,
            source_kind: SourceKind::SystemLoopback,
            sample_rate: device_sample_rate(id).unwrap_or(fallback_rate),
            channels: device_channels(id).unwrap_or(fallback_ch),
            is_loopback: true,
            is_default: id == default_id && default_id != 0,
        });
    }
    Ok(out)
}

/// Resolve a CoreAudio device UID from an output device name.
///
/// Convert the `id` (device name) from [`list_output_devices`] to the UID required by a tap.
/// Use the first match if names are duplicated. Return `Ok(None)` if no device matches (the caller
/// returns [`Error::DeviceNotFound`](flexaudio_core::types::Error)). If the device list cannot be read,
/// return the corresponding error from [`map_os_status`].
pub(crate) fn uid_for_device_name(name: &str) -> Result<Option<String>> {
    for id in all_device_ids(read_system_object_list)? {
        if !is_output_device(id) {
            continue;
        }
        if device_name(id).as_deref() == Some(name) {
            return Ok(device_uid(id));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    use flexaudio_core::types::Error;

    /// Enumeration returns `Ok` without panicking (there may be zero or more output devices in headless/CI).
    /// Each DeviceInfo follows the contract: loopback=true / SystemLoopback and valid rate/channels.
    #[test]
    fn list_output_devices_is_well_formed() {
        let devices = list_output_devices().expect("list_output_devices should not error");
        for d in &devices {
            assert_eq!(d.source_kind, SourceKind::SystemLoopback);
            assert!(d.is_loopback);
            assert!(!d.id.is_empty());
            assert_eq!(d.id, d.name);
            assert!(d.sample_rate > 0);
            assert!(d.channels > 0);
        }
    }

    /// A CoreAudio retrieval failure differs from a valid result containing zero devices.
    ///
    /// This test injects a reader into `all_device_ids`, so converting failure to an empty vec makes
    /// `failure` become `Ok(Vec::new())` instead of `Err`, and the test fails.
    #[test]
    fn all_device_ids_distinguishes_failure_from_empty_list() {
        fn empty_reader(_: u32) -> std::result::Result<Vec<AudioObjectID>, i32> {
            Ok(Vec::new())
        }

        fn failing_reader(_: u32) -> std::result::Result<Vec<AudioObjectID>, i32> {
            Err(-1)
        }

        fn permission_denied_reader(_: u32) -> std::result::Result<Vec<AudioObjectID>, i32> {
            Err(0x6e6f7065)
        }

        let empty = all_device_ids(empty_reader);
        assert!(matches!(empty, Ok(ids) if ids.is_empty()));

        match all_device_ids(failing_reader) {
            Err(Error::Backend(message)) => {
                assert_eq!(message, "AudioObjectGetPropertyData(Devices): OSStatus -1")
            }
            other => panic!("expected OSStatus-bearing error, got {other:?}"),
        }

        assert!(matches!(
            all_device_ids(permission_denied_reader),
            Err(Error::PermissionDenied { .. })
        ));
    }

    /// Resolving a nonexistent name to a UID returns `None`.
    #[test]
    fn uid_for_unknown_device_is_none() {
        assert!(matches!(
            uid_for_device_name("flexaudio-no-such-output-device-xyzzy"),
            Ok(None)
        ));
    }
}
