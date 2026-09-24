//! Output device enumeration and name → UID resolution.
//!
//! [`list_output_devices`] enumerates the output (playback) devices and returns a list of
//! [`DeviceInfo`]. When [`MacSystemBackend`](crate::MacSystemBackend) creates a tap targeting
//! a specific device, it uses [`uid_for_device_name`] to look up the CoreAudio device UID
//! from the public ID (= device name).
//!
//! Matches the `DeviceInfo` shape of all OS backends: `id` and `name` are both the device
//! name, `sample_rate`/`channels` are the device format, `is_loopback` is always `true` (a
//! monitor of the output), and `is_default` is `true` when it matches the default output
//! device.
//!
//! # Why the name is used as the ID
//! Output devices have a UID as a stable key, but to match how other OSes present
//! `DeviceInfo.id`, the device name is used as the public ID and resolved to the UID
//! internally just before the tap is created. If several devices share a name, the first
//! match is used.

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

/// Builds a property address for the given scope/element.
fn address(selector: u32, scope: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMain,
    }
}

/// Reads the system object's `kAudioHardwarePropertyDevices` and returns all
/// `AudioObjectID`s.
///
/// On failure, returns an [`Error`](flexaudio_core::types::Error) unified through
/// [`map_os_status`]. The actual read is [`read_system_object_list`], shared with process
/// enumeration.
///
/// The reader is a parameter so that tests can verify, without calling CoreAudio, that "read
/// failed" and "a valid empty list" are different values. Production callers always pass
/// [`read_system_object_list`].
type SystemObjectListReader = fn(u32) -> std::result::Result<Vec<AudioObjectID>, i32>;

fn all_device_ids(reader: SystemObjectListReader) -> Result<Vec<AudioObjectID>> {
    reader(kAudioHardwarePropertyDevices)
        .map_err(|status| map_os_status("AudioObjectGetPropertyData(Devices)", status))
}

/// Reads the device's `kAudioDevicePropertyNominalSampleRate` (output scope, Float64).
fn device_sample_rate(device: AudioObjectID) -> Option<u32> {
    let addr = address(
        kAudioDevicePropertyNominalSampleRate,
        kAudioObjectPropertyScopeOutput,
    );
    let mut rate: f64 = 0.0;
    let mut size = core::mem::size_of::<f64>() as u32;
    // SAFETY: addr/size/rate are valid locals.
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

/// Counts the channels of the device's output scope from
/// `kAudioDevicePropertyStreamConfiguration`.
///
/// Sums `AudioBuffer.mNumberChannels` over the `AudioBufferList`. `None` if it cannot be
/// obtained.
fn device_channels(device: AudioObjectID) -> Option<u16> {
    let addr = address(
        kAudioDevicePropertyStreamConfiguration,
        kAudioObjectPropertyScopeOutput,
    );
    let mut size: u32 = 0;
    // SAFETY: addr/size are valid locals.
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
    // AudioBufferList is variable-length (mBuffers is a trailing flexible array). Allocate a
    // buffer of the reported size, read into it, and interpret the head as an
    // AudioBufferList. AudioBufferList has *mut fields inside, so it needs 8-byte alignment.
    // Placing it in a Vec<u8> (align 1) could cause a misaligned dereference when reading the
    // AudioBufferList the OS wrote, so allocate a Vec of AudioBufferList itself to guarantee
    // alignment (taking enough elements at that alignment to cover the whole reported
    // size).
    let elem = core::mem::size_of::<AudioBufferList>();
    let count = (size as usize).div_ceil(elem).max(1);
    // SAFETY: AudioBufferList is a #[repr(C)] POD of numbers/pointers only, so
    // zero-initialization is valid.
    let mut storage: Vec<AudioBufferList> = vec![unsafe { core::mem::zeroed() }; count];
    // SAFETY: storage has at least size bytes allocated with 8-byte alignment.
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
    // SAFETY: The head of storage is the AudioBufferList the OS wrote (properly aligned),
    // followed by mNumberBuffers AudioBuffers.
    let list = storage.as_ptr();
    let num_buffers = unsafe { (*list).mNumberBuffers } as usize;
    if num_buffers == 0 {
        return None;
    }
    // SAFETY: mBuffers is the start of num_buffers AudioBuffers, within the reported size.
    let buffers = unsafe { std::slice::from_raw_parts((*list).mBuffers.as_ptr(), num_buffers) };
    let total: u32 = buffers.iter().map(|b| b.mNumberChannels).sum();
    if total == 0 {
        return None;
    }
    Some(total.min(u16::MAX as u32) as u16)
}

/// Whether the device is an output (playback) device. It is considered an output if its
/// output scope has at least one stream.
fn is_output_device(device: AudioObjectID) -> bool {
    let addr = address(kAudioDevicePropertyStreams, kAudioObjectPropertyScopeOutput);
    let mut size: u32 = 0;
    // SAFETY: addr/size are valid locals.
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

/// `AudioObjectID` of the default output device. `0` if it cannot be obtained.
fn default_output_device() -> AudioObjectID {
    let addr = address(
        kAudioHardwarePropertyDefaultOutputDevice,
        kAudioObjectPropertyScopeGlobal,
    );
    let mut device: AudioObjectID = 0;
    let mut size = core::mem::size_of::<AudioObjectID>() as u32;
    // SAFETY: addr/size/device are valid locals.
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

/// Reads the device's name (`kAudioObjectPropertyName`). `None` if it cannot be obtained.
fn device_name(device: AudioObjectID) -> Option<String> {
    read_cfstring_property(
        device,
        kAudioObjectPropertyName,
        kAudioObjectPropertyScopeGlobal,
    )
}

/// Reads the device's UID (`kAudioDevicePropertyDeviceUID`). `None` if it cannot be
/// obtained.
fn device_uid(device: AudioObjectID) -> Option<String> {
    read_cfstring_property(
        device,
        kAudioDevicePropertyDeviceUID,
        kAudioObjectPropertyScopeGlobal,
    )
}

/// Enumerates the output (playback) devices.
///
/// Each [`DeviceInfo`]:
/// - `id` / `name`: the device name (`kAudioObjectPropertyName`).
/// - `source_kind`: [`SourceKind::SystemLoopback`].
/// - `sample_rate` / `channels`: the device's output format ([`FALLBACK_FORMAT`] if
///   unavailable).
/// - `is_loopback`: always `true` (output monitor).
/// - `is_default`: `true` if it matches the default output device.
///
/// Enumeration only, so TCC is not required. Devices whose name cannot be read are
/// skipped.
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

/// Looks up the CoreAudio device UID from an output device name.
///
/// Converts a selection given as an `id` (= device name) from [`list_output_devices`] into
/// the UID the tap requires. If several devices share the name, the first match is used.
/// Returns `Ok(None)` if no device matches (the caller returns
/// [`Error::DeviceNotFound`](flexaudio_core::types::Error)). If the list cannot be obtained,
/// returns an `Err` mapped through [`map_os_status`].
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

    /// Enumeration does not panic and returns `Ok` (zero or more output devices even on
    /// headless/CI). Each DeviceInfo is loopback=true / SystemLoopback per the contract and
    /// has a sensible rate/channels.
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

    /// A CoreAudio read failure and a valid result of zero devices are different values.
    ///
    /// This test injects a reader into `all_device_ids` itself, so if failures were turned
    /// back into an empty vec, `failure` would be `Ok(Vec::new())` instead of `Err` and the
    /// test would always fail.
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
            Err(Error::PermissionDenied)
        ));
    }

    /// Resolving the UID of a nonexistent name yields `None`.
    #[test]
    fn uid_for_unknown_device_is_none() {
        assert!(matches!(
            uid_for_device_name("flexaudio-no-such-output-device-xyzzy"),
            Ok(None)
        ));
    }
}
