//! Provenance-aware discovery and arrival-time aliases over frozen layouts.
use crate::{
    boundary::{invalid, output},
    convert, error, guard_i32,
    secondary::FlexSecondaryChunkV3,
    types::FlexChunk,
    v4_types::FlexNamespacedDeviceV4,
};
use std::ffi::CString;

pub(crate) fn device(entry: flexaudio::NamespacedDevice) -> FlexNamespacedDeviceV4 {
    let namespace = match entry.id_namespace.as_str() {
        "cpalDeviceName" => c"cpalDeviceName",
        "pipeWireNodeName" => c"pipeWireNodeName",
        "wasapiOutputSelector" => c"wasapiOutputSelector",
        "coreAudioOutputSelector" => c"coreAudioOutputSelector",
        _ => c"unknown",
    };
    FlexNamespacedDeviceV4 {
        device: convert::device_info_to_c(entry.device),
        id_namespace: namespace.as_ptr(),
    }
}
/// Enumerate unchanged selectable devices with provider-assigned namespace tags.
/// The array owns device strings; namespace strings have static lifetime.
/// Both outputs initialize to empty on failure when their pointers are valid.
/// # Safety
/// out and len are writable; release the array using namespaced_devices_free_v4.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_devices_with_namespaces_v4(
    out: *mut *mut FlexNamespacedDeviceV4,
    len: *mut usize,
) -> i32 {
    guard_i32(|| {
        error::clear_last_error();
        if let Err(status) = output(out) {
            return status;
        }
        if !convert::valid_pointer(len) {
            return invalid("invalid device count destination");
        }
        len.write(0);
        let entries = match flexaudio::devices_with_namespaces() {
            Ok(entries) => entries,
            Err(error) => return error::set_audio_error(error),
        };
        crate::write_c_array(entries.into_iter().map(device).collect(), out, len);
        0
    })
}
/// Free the namespaced array and its device strings. NULL-safe.
/// # Safety
/// entries and len are exactly an unfreed result of devices_with_namespaces_v4, or NULL.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_namespaced_devices_free_v4(
    entries: *mut FlexNamespacedDeviceV4,
    len: usize,
) {
    guard_i32(|| {
        if entries.is_null() {
            return 0;
        }
        if !convert::valid_array(entries, len) {
            return invalid("invalid namespaced device array");
        }
        let entries = Box::from_raw(std::ptr::slice_from_raw_parts_mut(entries, len));
        for entry in entries.iter() {
            drop(CString::from_raw(entry.device.id));
            drop(CString::from_raw(entry.device.name));
        }
        0
    });
}
/// Return the exact arrival-time label. Also accepts &chunk_v2.chunk.
/// # Safety
/// chunk is a readable FlexChunk; out is writable.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_chunk_arrival_ns_v4(
    chunk: *const FlexChunk,
    out: *mut i64,
) -> i32 {
    guard_i32(|| {
        error::clear_last_error();
        if !convert::valid_pointer(chunk) || !convert::valid_pointer(out) {
            return invalid("invalid arrival-time pointer");
        }
        out.write((*chunk).pts_ns);
        0
    })
}
/// Return the secondary chunk's exact arrival-time label.
/// # Safety
/// chunk is a live secondary owner; out is writable.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_secondary_arrival_ns_v4(
    chunk: *const FlexSecondaryChunkV3,
    out: *mut i64,
) -> i32 {
    guard_i32(|| {
        error::clear_last_error();
        if !convert::valid_pointer(chunk) || !convert::valid_pointer(out) {
            return invalid("invalid arrival-time pointer");
        }
        out.write((*chunk).metadata.pts_ns);
        0
    })
}
