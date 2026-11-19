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
    kAudioDevicePropertyStreamConfiguration, kAudioDevicePropertyStreamFormat,
    kAudioDevicePropertyStreams, kAudioHardwarePropertyDefaultOutputDevice,
    kAudioHardwarePropertyDevices, kAudioObjectPropertyElementMain, kAudioObjectPropertyName,
    kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyScopeOutput, kAudioObjectSystemObject,
    AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize, AudioObjectID,
    AudioObjectPropertyAddress,
};
use objc2_core_audio_types::{AudioBufferList, AudioStreamBasicDescription};

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

/// Resolve the selectable output device a request refers to.
///
/// `None` resolves the default output and fails with `DeviceNotFound` when there is none;
/// `Some(id)` accepts the same name-based IDs and UID fallbacks as [`list_output_devices`]
/// and is resolved with the identity matching used by the tap. Shared by
/// [`selected_output_format`] and [`resolve_output_device_uid`] so both agree on which
/// device is meant.
fn resolve_output_device_id(requested_id: Option<&str>) -> Result<AudioObjectID> {
    match requested_id {
        Some(requested_id) => {
            for id in all_device_ids(read_system_object_list)? {
                if crate::inventory::output_device_matches(is_output_device(id), || {
                    flexaudio_core::inventory::device_identity_matches(
                        requested_id,
                        device_name(id),
                        || device_uid(id),
                    )
                })? {
                    return Ok(id);
                }
            }
            Err(Error::DeviceNotFound)
        }
        None => {
            let id = default_output_device()?;
            if id == 0 {
                return Err(Error::DeviceNotFound);
            }
            Ok(id)
        }
    }
}

/// Real output stream format `(rate, channels)` of the selected/default output device.
///
/// A process tap follows its destination device's output stream ASBD, which is not
/// necessarily the device's nominal sample rate: Bluetooth A2DP outputs, for example, can
/// report a nominal rate that differs from the active stream format. Seeding
/// `MacSystemBackend::native_format` from this value keeps the tap's startup verification
/// consistent instead of assuming 48 kHz stereo.
///
/// This is the single format source for output devices: both `native_format` seeding and
/// `require_output_format` (open-time validation) read the same stream 0 ASBD the tap sees.
pub(crate) fn selected_output_format(requested_id: Option<&str>) -> Result<(u32, u16)> {
    output_stream_format(resolve_output_device_id(requested_id)?)
}

/// Read stream 0's output ASBD on the device's output scope.
///
/// That is the native format a process tap on this device receives, so callers use it for
/// both the expected `native_format` and open-time format validation.
fn output_stream_format(device: AudioObjectID) -> Result<(u32, u16)> {
    let addr = address(
        kAudioDevicePropertyStreamFormat,
        kAudioObjectPropertyScopeOutput,
    );
    // ASBD has no Default, so zero-initialize it and let the OS fill it in.
    // SAFETY: AudioStreamBasicDescription is a `#[repr(C)]` POD with only numeric fields.
    let mut asbd: AudioStreamBasicDescription = unsafe { core::mem::zeroed() };
    let mut size = core::mem::size_of::<AudioStreamBasicDescription>() as u32;
    // SAFETY: addr, size, and asbd are valid local values.
    let status = unsafe {
        AudioObjectGetPropertyData(
            device,
            NonNull::from(&addr),
            0,
            core::ptr::null(),
            NonNull::from(&mut size),
            NonNull::new_unchecked(
                (&mut asbd as *mut AudioStreamBasicDescription).cast::<c_void>(),
            ),
        )
    };
    check_status("AudioObjectGetPropertyData(StreamFormat)", status)?;
    if size as usize != core::mem::size_of::<AudioStreamBasicDescription>() {
        return Err(invalid_property());
    }
    let rate = asbd.mSampleRate;
    if !rate.is_finite() || rate <= 0.0 || asbd.mChannelsPerFrame == 0 {
        return Err(invalid_property());
    }
    let rate = u32::try_from(rate as u64).map_err(|_| invalid_property())?;
    let channels = u16::try_from(asbd.mChannelsPerFrame).map_err(|_| invalid_property())?;
    Ok((rate, channels))
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
    list_output_devices_with_namespaces()
        .map(|devices| devices.into_iter().map(|entry| entry.device).collect())
}

/// Enumerate Core Audio output selectors with their provider provenance.
/// IDs and discovery failures follow [`list_output_devices`]. Readable names
/// and UID fallbacks share one namespace; IDs need not be persistent.
pub fn list_output_devices_with_namespaces() -> Result<Vec<flexaudio_core::NamespacedDevice>> {
    let default_id = default_output_device().unwrap_or(0);
    flexaudio_core::inventory::collect(all_device_ids(read_system_object_list), |id| {
        let Some((identity, name)) = output_device_identity(id) else {
            return Ok(None);
        };
        Ok(Some(namespaced_output_device(
            identity,
            name,
            device_sample_rate(id)
                .and_then(|rate| device_channels(id).map(|channels| (rate, channels))),
            id == default_id && default_id != 0,
        )))
    })
}

#[cfg(test)]
mod namespace_tests {
    use super::*;

    #[test]
    fn output_enumeration_tags_duplicate_names_and_native_fallbacks() {
        let entries = flexaudio_core::inventory::collect(Ok([true, true, false]), |readable| {
            let name = if readable {
                Ok("duplicate output".into())
            } else {
                Err(Error::DeviceNotFound)
            };
            let (id, name) =
                flexaudio_core::inventory::device_identity(name, || Ok("native selector".into()))
                    .unwrap();
            Ok(Some(namespaced_output_device(
                id,
                name,
                Ok((48_000, 2)),
                false,
            )))
        })
        .unwrap();
        assert_eq!(entries.len(), 3);
        for entry in &entries {
            assert_eq!(
                entry.id_namespace,
                flexaudio_core::DeviceIdNamespace::CoreAudioOutputSelector
            );
            assert_eq!(entry.device.source_kind, SourceKind::SystemLoopback);
            assert!(entry.device.is_loopback);
            assert!(entry.device.format_available);
        }
        assert_eq!(entries[0].device.id, "duplicate output");
        assert_eq!(entries[1].device.id, entries[0].device.id);
        assert_eq!(entries[2].device.id, "native selector");
        assert!(entries[2].device.name.is_empty());
    }
}

fn namespaced_output_device(
    identity: String,
    name: String,
    format: Result<(u32, u16)>,
    is_default: bool,
) -> flexaudio_core::NamespacedDevice {
    let (sample_rate, channels, format_available) =
        flexaudio_core::inventory::device_format(format);
    flexaudio_core::NamespacedDevice::new(
        DeviceInfo {
            id: identity,
            name,
            source_kind: SourceKind::SystemLoopback,
            sample_rate,
            channels,
            format_available,
            is_loopback: true,
            is_default,
        },
        flexaudio_core::DeviceIdNamespace::CoreAudioOutputSelector,
    )
}

/// One identity/classification path for both discovery and selected-device open.
fn output_device_identity(id: AudioObjectID) -> Option<(String, String)> {
    if !crate::inventory::output_device_matches(is_output_device(id), || true).ok()? {
        return None;
    }
    flexaudio_core::inventory::device_identity(device_name(id), || device_uid(id))
}

/// Validate the device's output stream ASBD before creating any tap.
///
/// Deliberately reads the stream format (what the tap receives) rather than
/// `kAudioDevicePropertyNominalSampleRate`, so open-time validation and startup verification
/// agree on non-48 kHz outputs. See [`selected_output_format`].
fn require_output_format(id: AudioObjectID) -> Result<(u32, u16)> {
    flexaudio_core::inventory::require_device_format(output_stream_format(id))
}

/// Resolve a name-based inventory ID or its unknown-name UID fallback.
/// None retains default tap routing after validating the resolved default device.
///
/// Format validation uses [`require_output_format`], i.e. the same output stream ASBD that
/// [`selected_output_format`] seeds into `native_format`.
pub(crate) fn resolve_output_device_uid(requested_id: Option<&str>) -> Result<Option<String>> {
    let device = resolve_output_device_id(requested_id)?;
    require_output_format(device)?;
    if requested_id.is_none() {
        return Ok(None);
    }
    device_uid(device).map(Some)
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

    /// Open-time validation and native-format seeding read the same stream ASBD.
    #[test]
    fn require_output_format_agrees_with_selected_output_format() {
        let Ok(id) = default_output_device() else {
            return;
        };
        if id == 0 {
            return;
        }
        let Ok(required) = require_output_format(id) else {
            return;
        };
        let Ok(selected) = selected_output_format(None) else {
            return;
        };
        assert_eq!(required, selected);
    }

    /// The selected/default output resolves to a usable stream format, or the environment has no
    /// default output (headless CI); neither case may panic or fabricate a format.
    #[test]
    fn selected_output_format_resolves_default_output() {
        match default_output_device() {
            Ok(id) if id != 0 => {
                let (rate, channels) =
                    selected_output_format(None).expect("default output stream format");
                assert!(rate > 0, "rate must be positive");
                assert!(channels > 0, "channels must be positive");
            }
            _ => {}
        }
    }

    /// A listed output device resolves through the same identity matching the tap uses.
    #[test]
    fn selected_output_format_resolves_listed_device() {
        let devices = list_output_devices().expect("list_output_devices should not error");
        let Some(device) = devices
            .iter()
            .find(|device| device.format_available && device.channels > 0)
        else {
            return;
        };
        match selected_output_format(Some(&device.id)) {
            Ok((rate, channels)) => {
                assert!(rate > 0, "rate must be positive");
                assert!(channels > 0, "channels must be positive");
            }
            // The device may disappear between enumeration and resolution.
            Err(Error::DeviceNotFound) => {}
            Err(error) => panic!("unexpected error for listed device: {error:?}"),
        }
    }

    /// A nonexistent device is reported as DeviceNotFound instead of silently using the default.
    #[test]
    fn selected_output_format_rejects_unknown_device() {
        assert!(matches!(
            selected_output_format(Some("flexaudio-no-such-output-device-xyzzy")),
            Err(Error::DeviceNotFound)
        ));
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
