//! Enumerate output devices and resolve names to UIDs.
//!
//! [`list_output_devices`] enumerates output (playback) devices and returns a list of [`DeviceInfo`].
//! When [`MacSystemBackend`](crate::MacSystemBackend) creates a tap for a specific device, it uses
//! [`resolve_output_device_uid`] to resolve a public name-based ID or native-UID fallback to a CoreAudio device UID.
//!
//! Named devices keep name-based IDs. Unreadable names use a selectable UID and an empty name,
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

use flexaudio_core::types::{DeviceInfo, Error, Result, SourceKind};

use crate::common::{map_os_status_at, query_cfstring_property, read_system_object_list};
use flexaudio_core::{ErrorContext, Operation};

fn invalid_property() -> Error {
    Error::Backend(
        "required output device property is missing or invalid; retry device discovery".into(),
    )
    .with_context(ErrorContext::new(Operation::Enumerate))
}

fn check_status(call: &'static str, status: i32) -> Result<()> {
    if status == 0 {
        Ok(())
    } else {
        Err(map_os_status_at(Operation::Enumerate, call, status))
    }
}

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
/// On failure, return the normalized [`Error`](flexaudio_core::types::Error) from [`map_os_status_at`].
/// The reader, [`read_system_object_list`], is shared with process enumeration.
///
/// Accept a reader so tests can distinguish retrieval failure from a valid empty list without calling
/// CoreAudio. Production callers always pass [`read_system_object_list`].
type SystemObjectListReader = fn(u32) -> std::result::Result<Vec<AudioObjectID>, i32>;

fn all_device_ids(reader: SystemObjectListReader) -> Result<Vec<AudioObjectID>> {
    reader(kAudioHardwarePropertyDevices).map_err(|status| {
        map_os_status_at(
            flexaudio_core::Operation::Enumerate,
            "AudioObjectGetPropertyData(Devices)",
            status,
        )
    })
}

/// Read the device's `kAudioDevicePropertyNominalSampleRate` (output scope, Float64).
fn device_sample_rate(device: AudioObjectID) -> Result<u32> {
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
    check_status("AudioObjectGetPropertyData(NominalSampleRate)", status)?;
    if size as usize != core::mem::size_of::<f64>()
        || !rate.is_finite()
        || rate < 1.0
        || rate > u32::MAX as f64
        || rate.fract() != 0.0
    {
        return Err(invalid_property());
    }
    Ok(rate as u32)
}

/// Count channels in the device's output scope using `kAudioDevicePropertyStreamConfiguration`.
///
/// Sum validated `AudioBuffer.mNumberChannels` entries, rejecting query failures or invalid sizes.
fn device_channels(device: AudioObjectID) -> Result<u16> {
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
    check_status(
        "AudioObjectGetPropertyDataSize(StreamConfiguration)",
        status,
    )?;
    let capacity = size;
    let buffers_offset = core::mem::offset_of!(AudioBufferList, mBuffers);
    if (size as usize) < buffers_offset {
        return Err(invalid_property());
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
    check_status("AudioObjectGetPropertyData(StreamConfiguration)", status)?;
    if size > capacity || (size as usize) < buffers_offset {
        return Err(invalid_property());
    }
    // SAFETY: `storage` starts with the OS-written, properly aligned AudioBufferList, followed by
    // `mNumberBuffers` AudioBuffer entries.
    let list = storage.as_ptr();
    let num_buffers = unsafe { (*list).mNumberBuffers } as usize;
    if num_buffers == 0
        || num_buffers
            > (size as usize - buffers_offset)
                / core::mem::size_of::<objc2_core_audio_types::AudioBuffer>()
    {
        return Err(invalid_property());
    }
    // SAFETY: `mBuffers` points to the first of `num_buffers` AudioBuffer entries, within the reported size.
    let buffers = unsafe { std::slice::from_raw_parts((*list).mBuffers.as_ptr(), num_buffers) };
    let total = buffers
        .iter()
        .try_fold(0u32, |sum, buffer| sum.checked_add(buffer.mNumberChannels))
        .ok_or_else(invalid_property)?;
    let channels = u16::try_from(total).map_err(|_| invalid_property())?;
    if channels == 0 {
        return Err(invalid_property());
    }
    Ok(channels)
}

/// Check whether the device is an output (playback) device. At least one stream in the output scope means yes.
fn is_output_device(device: AudioObjectID) -> Result<bool> {
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
    check_status("AudioObjectGetPropertyDataSize(OutputStreams)", status)?;
    if size as usize % core::mem::size_of::<AudioObjectID>() != 0 {
        return Err(invalid_property());
    }
    Ok(size > 0)
}

/// Query the default output device; a successful zero means there is no default.
fn default_output_device() -> Result<AudioObjectID> {
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
    check_status("AudioObjectGetPropertyData(DefaultOutputDevice)", status)?;
    if size as usize != core::mem::size_of::<AudioObjectID>() {
        return Err(invalid_property());
    }
    Ok(device)
}

/// Query the optional display name (`kAudioObjectPropertyName`).
fn device_name(device: AudioObjectID) -> Result<String> {
    query_cfstring_property(
        device,
        kAudioObjectPropertyName,
        kAudioObjectPropertyScopeGlobal,
    )?
    .ok_or_else(invalid_property)
}

/// Query the required device UID (`kAudioDevicePropertyDeviceUID`).
fn device_uid(device: AudioObjectID) -> Result<String> {
    query_cfstring_property(
        device,
        kAudioDevicePropertyDeviceUID,
        kAudioObjectPropertyScopeGlobal,
    )?
    .ok_or_else(invalid_property)
}

/// Enumerate output (playback) devices.
///
/// Each [`DeviceInfo`]:
/// - `id` / `name`: device name, or native UID / empty name if the display name is unreadable.
/// - `source_kind`: [`SourceKind::SystemLoopback`].
/// - `sample_rate` / `channels`: the successfully queried native output format.
/// - `is_loopback`: always `true` (output monitor).
/// - `is_default`: `true` if this is the default output device.
///
/// TCC permission is not needed. Collection failures are terminal. Omit devices
/// with unreadable direction (cannot classify) or no selectable identity.
pub fn list_output_devices() -> Result<Vec<DeviceInfo>> {
    let default_id = default_output_device().unwrap_or(0);
    flexaudio_core::inventory::collect(all_device_ids(read_system_object_list), |id| {
        let Some((identity, name)) = output_device_identity(id) else {
            return Ok(None);
        };
        let (sample_rate, channels, format_available) = flexaudio_core::inventory::device_format(
            device_sample_rate(id)
                .and_then(|rate| device_channels(id).map(|channels| (rate, channels))),
        );
        Ok(Some(DeviceInfo {
            id: identity,
            name,
            source_kind: SourceKind::SystemLoopback,
            sample_rate,
            channels,
            format_available,
            is_loopback: true,
            is_default: id == default_id && default_id != 0,
        }))
    })
}

/// One identity/classification path for both discovery and selected-device open.
fn output_device_identity(id: AudioObjectID) -> Option<(String, String)> {
    if !crate::inventory::output_device_matches(is_output_device(id), || true).ok()? {
        return None;
    }
    flexaudio_core::inventory::device_identity(device_name(id), || device_uid(id))
}

/// Validate the actual selected/default device's format before creating any tap.
fn require_output_format(id: AudioObjectID) -> Result<(u32, u16)> {
    flexaudio_core::inventory::require_device_format(
        device_sample_rate(id)
            .and_then(|rate| device_channels(id).map(|channels| (rate, channels))),
    )
}

/// Resolve a name-based inventory ID or its unknown-name UID fallback.
/// None retains default tap routing after validating the resolved default device.
pub(crate) fn resolve_output_device_uid(requested_id: Option<&str>) -> Result<Option<String>> {
    let Some(requested_id) = requested_id else {
        let id = default_output_device()?;
        if id == 0 {
            return Err(Error::DeviceNotFound);
        }
        require_output_format(id)?;
        return Ok(None);
    };
    for id in all_device_ids(read_system_object_list)? {
        if crate::inventory::output_device_matches(is_output_device(id), || {
            flexaudio_core::inventory::device_identity_matches(
                requested_id,
                device_name(id),
                || device_uid(id),
            )
        })? {
            require_output_format(id)?;
            return device_uid(id).map(Some);
        }
    }
    Err(Error::DeviceNotFound)
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
            assert!(d.name.is_empty() || d.id == d.name);
            if d.format_available {
                assert!(d.sample_rate > 0);
                assert!(d.channels > 0);
            } else {
                assert_eq!((d.sample_rate, d.channels), (0, 0));
            }
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

        let error = all_device_ids(failing_reader).unwrap_err();
        assert!(matches!(error.root(), Error::Backend(message) if message == "OSStatus -1"));
        let Error::Context { context, .. } = error else {
            panic!("missing native context")
        };
        assert_eq!(context.operation(), flexaudio_core::Operation::Enumerate);
        assert_eq!(
            context.native_status(),
            Some(flexaudio_core::NativeStatus::OsStatus {
                call: "AudioObjectGetPropertyData(Devices)",
                value: -1
            })
        );
        assert_eq!(
            all_device_ids(permission_denied_reader)
                .unwrap_err()
                .permission(),
            Some(flexaudio_core::Permission::SystemAudio)
        );
    }

    /// Resolving a nonexistent inventory ID returns DeviceNotFound.
    #[test]
    fn unknown_output_device_is_not_found() {
        assert!(matches!(
            resolve_output_device_uid(Some("flexaudio-no-such-output-device-xyzzy")),
            Err(Error::DeviceNotFound)
        ));
    }
}
