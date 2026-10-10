//! Opaque v2 polling and conditional borrowed/scalar accessors.
//! Handles must be live library owners, and outputs must be writable for their declared type.
//! NULL and misalignment are rejected before dereference; no getter transfers ownership.
use crate::v2_records::*;
use crate::v2_storage::*;
use crate::{
    error::{self, code},
    guard_i32, guard_ptr,
    types::{FlexDeviceInfo, FlexStream},
    watch::FlexWatcher,
};
use std::{os::raw::c_char, ptr};

pub(crate) fn invalid() -> i32 {
    error::set_audio_error(flexaudio::Error::InvalidArg(
        "invalid v2 pointer or index".into(),
    ))
}
pub(crate) fn valid<T>(p: *const T) -> bool {
    !p.is_null() && p.is_aligned()
}
unsafe fn scalar<O, T>(
    value: *const O,
    out: *mut T,
    get: impl FnOnce(&O) -> Result<Option<T>, ()>,
) -> i32 {
    guard_i32(|| {
        error::clear_last_error();
        if !valid(value) || !valid(out) {
            return invalid();
        }
        match get(&*value) {
            Ok(Some(result)) => {
                out.write(result);
                1
            }
            Ok(None) => 0,
            Err(()) => invalid(),
        }
    })
}
/// Poll one owned event. Valid out initializes to NULL before the queue is touched.
/// # Safety
/// stream must be live and exclusively borrowed; out must be writable.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_poll_event_v2(
    stream: *mut FlexStream,
    out: *mut *mut FlexEventV2,
) -> i32 {
    guard_i32(|| {
        error::clear_last_error();
        if !valid(out) {
            return invalid();
        }
        out.write(ptr::null_mut());
        if !valid(stream) {
            return invalid();
        }
        match (*stream).poll_binding_event() {
            Some(event) => {
                out.write(Box::into_raw(Box::new(FlexEventV2::new(event))));
                1
            }
            None => 0,
        }
    })
}
/// Poll one owned watcher event. Invalid output never consumes a delta.
/// # Safety
/// watcher must be live and exclusively borrowed; out must be writable.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_watcher_poll_v2(
    watcher: *mut FlexWatcher,
    out: *mut *mut FlexDeviceEventV2,
) -> i32 {
    guard_i32(|| {
        error::clear_last_error();
        if !valid(out) {
            return invalid();
        }
        out.write(ptr::null_mut());
        if !valid(watcher) {
            return invalid();
        }
        match (*watcher).inner.poll_event() {
            Some(event) => {
                out.write(Box::into_raw(Box::new(FlexDeviceEventV2::new(event))));
                1
            }
            None => 0,
        }
    })
}
/// Owned clone of the calling thread's typed error, NULL when absent. Free explicitly.
#[no_mangle]
pub extern "C" fn flexaudio_last_error_info_v2() -> *mut FlexErrorInfoV2 {
    guard_ptr(|| {
        error::last_audio_error().map_or(ptr::null_mut(), |e| {
            Box::into_raw(Box::new(FlexErrorInfoV2::new(e)))
        })
    })
}
/// Owned capture primary snapshot, valid independently of the stream and TLS.
/// # Safety
/// stream must be a live readable stream handle.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_terminal_error_info_v2(
    stream: *const FlexStream,
) -> *mut FlexErrorInfoV2 {
    guard_ptr(|| {
        error::clear_last_error();
        if !valid(stream) {
            invalid();
            return ptr::null_mut();
        }
        (*stream)
            .inner
            .terminal_error()
            .map_or(ptr::null_mut(), |e| {
                Box::into_raw(Box::new(FlexErrorInfoV2::new(e)))
            })
    })
}
/// Owned completed report snapshot; NULL before shutdown completes.
/// # Safety
/// stream must be a live readable stream handle.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_shutdown_report_v2(
    stream: *const FlexStream,
) -> *mut FlexShutdownReportV2 {
    guard_ptr(|| {
        error::clear_last_error();
        if !valid(stream) {
            invalid();
            return ptr::null_mut();
        }
        (*stream).shutdown.clone().map_or(ptr::null_mut(), |r| {
            Box::into_raw(Box::new(FlexShutdownReportV2::new(r)))
        })
    })
}
/// Free the owned tree; NULL is safe. Borrowed children must never be freed separately.
/// # Safety
/// value must be a live owned FlexEventV2 returned by this library, or NULL.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_event_free_v2(value: *mut FlexEventV2) {
    guard_i32(|| {
        if value.is_null() {
            return code::FLEX_OK;
        }
        if !valid(value) {
            return invalid();
        }
        drop(Box::from_raw(value));
        code::FLEX_OK
    });
}
/// Free the owned tree; NULL is safe. Borrowed children must never be freed separately.
/// # Safety
/// value must be a live owned FlexDeviceEventV2 returned by this library, or NULL.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_device_event_free_v2(value: *mut FlexDeviceEventV2) {
    guard_i32(|| {
        if value.is_null() {
            return code::FLEX_OK;
        }
        if !valid(value) {
            return invalid();
        }
        drop(Box::from_raw(value));
        code::FLEX_OK
    });
}
/// Free the owned tree; NULL is safe. Borrowed children must never be freed separately.
/// # Safety
/// value must be a live owned FlexErrorInfoV2 returned by this library, or NULL.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_error_info_free_v2(value: *mut FlexErrorInfoV2) {
    guard_i32(|| {
        if value.is_null() {
            return code::FLEX_OK;
        }
        if !valid(value) {
            return invalid();
        }
        drop(Box::from_raw(value));
        code::FLEX_OK
    });
}
/// Free the owned tree; NULL is safe. Borrowed children must never be freed separately.
/// # Safety
/// value must be a live owned FlexShutdownReportV2 returned by this library, or NULL.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_shutdown_report_free_v2(value: *mut FlexShutdownReportV2) {
    guard_i32(|| {
        if value.is_null() {
            return code::FLEX_OK;
        }
        if !valid(value) {
            return invalid();
        }
        drop(Box::from_raw(value));
        code::FLEX_OK
    });
}
/// Return 1 when present, 0 for a different arm, negative InvalidArg for invalid pointers.
/// Output is untouched when absent. Borrowed fields remain valid only while value lives.
/// # Safety
/// value must be a live owner; out must be writable and aligned.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_event_kind_v2(value: *const FlexEventV2, out: *mut i32) -> i32 {
    scalar(value, out, |v| Ok(Some(v.kind)))
}
/// Return 1 when present, 0 for a different arm, negative InvalidArg for invalid pointers.
/// Output is untouched when absent. Borrowed fields remain valid only while value lives.
/// # Safety
/// value must be a live owner; out must be writable and aligned.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_event_count_v2(value: *const FlexEventV2, out: *mut u64) -> i32 {
    scalar(value, out, |v| Ok(v.count))
}
/// Return 1 when present, 0 for a different arm, negative InvalidArg for invalid pointers.
/// Output is untouched when absent. Borrowed fields remain valid only while value lives.
/// # Safety
/// value must be a live owner; out must be writable and aligned.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_event_permission_v2(
    value: *const FlexEventV2,
    out: *mut i32,
) -> i32 {
    scalar(value, out, |v| Ok(v.permission))
}
/// Return 1 when present, 0 for a different arm, negative InvalidArg for invalid pointers.
/// Output is untouched when absent. Borrowed fields remain valid only while value lives.
/// # Safety
/// value must be a live owner; out must be writable and aligned.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_event_loss_v2(
    value: *const FlexEventV2,
    out: *mut FlexAudioLossV2,
) -> i32 {
    scalar(value, out, |v| Ok(v.loss))
}
/// Return 1 when present, 0 for a different arm, negative InvalidArg for invalid pointers.
/// Output is untouched when absent. Borrowed fields remain valid only while value lives.
/// # Safety
/// value must be a live owner; out must be writable and aligned.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_device_event_kind_v2(
    value: *const FlexDeviceEventV2,
    out: *mut i32,
) -> i32 {
    scalar(value, out, |v| Ok(Some(v.kind)))
}
/// Return 1 when present, 0 for a different arm, negative InvalidArg for invalid pointers.
/// Output is untouched when absent. Borrowed fields remain valid only while value lives.
/// # Safety
/// value must be a live owner; out must be writable and aligned.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_device_event_default_kind_v2(
    value: *const FlexDeviceEventV2,
    out: *mut i32,
) -> i32 {
    scalar(value, out, |v| Ok(v.default_kind))
}
/// Return 1 when present, 0 for a different arm, negative InvalidArg for invalid pointers.
/// Output is untouched when absent. Borrowed fields remain valid only while value lives.
/// # Safety
/// value must be a live owner; out must be writable and aligned.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_device_event_dropped_events_v2(
    value: *const FlexDeviceEventV2,
    out: *mut u64,
) -> i32 {
    scalar(value, out, |v| Ok(v.dropped_events))
}
/// Return 1 when present, 0 for a different arm, negative InvalidArg for invalid pointers.
/// Output is untouched when absent. Borrowed fields remain valid only while value lives.
/// # Safety
/// value must be a live owner; out must be writable and aligned.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_error_kind_v2(
    value: *const FlexErrorInfoV2,
    out: *mut i32,
) -> i32 {
    scalar(value, out, |v| Ok(Some(error::root_code(&v.root))))
}
/// Return 1 when present, 0 for a different arm, negative InvalidArg for invalid pointers.
/// Output is untouched when absent. Borrowed fields remain valid only while value lives.
/// # Safety
/// value must be a live owner; out must be writable and aligned.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_error_permission_v2(
    value: *const FlexErrorInfoV2,
    out: *mut i32,
) -> i32 {
    scalar(value, out, |v| Ok(v.root.permission().map(permission)))
}
/// Return 1 when present, 0 for a different arm, negative InvalidArg for invalid pointers.
/// Output is untouched when absent. Borrowed fields remain valid only while value lives.
/// # Safety
/// value must be a live owner; out must be writable and aligned.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_error_context_count_v2(
    value: *const FlexErrorInfoV2,
    out: *mut usize,
) -> i32 {
    scalar(value, out, |v| Ok(Some(v.context_count())))
}
/// Return 1 when present, 0 for a different arm, negative InvalidArg for invalid pointers.
/// Output is untouched when absent. Borrowed fields remain valid only while value lives.
/// # Safety
/// value must be a live owner; out must be writable and aligned.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_error_secondary_count_v2(
    value: *const FlexErrorInfoV2,
    out: *mut usize,
) -> i32 {
    scalar(value, out, |v| Ok(Some(v.secondary.len())))
}
/// Return 1 when present, 0 for a different arm, negative InvalidArg for invalid pointers.
/// Output is untouched when absent. Borrowed fields remain valid only while value lives.
/// # Safety
/// value must be a live owner; out must be writable and aligned.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_shutdown_cleanup_count_v2(
    value: *const FlexShutdownReportV2,
    out: *mut usize,
) -> i32 {
    scalar(value, out, |v| Ok(Some(v.cleanup.len())))
}
/// Return 1 when present, 0 for a different arm, negative InvalidArg for invalid pointers.
/// Output is untouched when absent. Borrowed fields remain valid only while value lives.
/// # Safety
/// value must be a live owner; out must be writable and aligned.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_error_native_format_v2(
    value: *const FlexErrorInfoV2,
    out: *mut FlexNativeFormatChangeV2,
) -> i32 {
    scalar(value, out, |v| {
        Ok(match &v.root {
            flexaudio::Error::NativeFormatChanged { advertised, actual } => {
                Some(FlexNativeFormatChangeV2 {
                    advertised: FlexNativeFormatV2 {
                        sample_rate: advertised.0,
                        channels: advertised.1,
                    },
                    actual: FlexNativeFormatV2 {
                        sample_rate: actual.0,
                        channels: actual.1,
                    },
                })
            }
            _ => None,
        })
    })
}
/// Retrieve an outer-to-inner context record. Invalid index returns InvalidArg.
/// # Safety
/// value must be a live owner; out must be writable and aligned.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_error_context_v2(
    value: *const FlexErrorInfoV2,
    index: usize,
    out: *mut FlexErrorContextV2,
) -> i32 {
    scalar(value, out, |v| v.context(index).map(Some).ok_or(()))
}
/// Borrow a view until the owning tree is freed. NULL means absent/wrong arm.
/// Invalid pointer/index sets last_error. Do not free the borrowed view separately.
/// # Safety
/// value must be a live readable owner.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_event_error_v2(
    value: *const FlexEventV2,
) -> *const FlexErrorInfoV2 {
    guard_ptr(|| {
        error::clear_last_error();
        if !valid(value) {
            invalid();
            return ptr::null_mut();
        }
        let v = &*value;
        v.error
            .as_ref()
            .map(|e| e as *const FlexErrorInfoV2)
            .unwrap_or(ptr::null())
            .cast_mut()
    })
    .cast_const()
}
/// Borrow a view until the owning tree is freed. NULL means absent/wrong arm.
/// Invalid pointer/index sets last_error. Do not free the borrowed view separately.
/// # Safety
/// value must be a live readable owner.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_event_message_v2(value: *const FlexEventV2) -> *const c_char {
    guard_ptr(|| {
        error::clear_last_error();
        if !valid(value) {
            invalid();
            return ptr::null_mut();
        }
        let v = &*value;
        v.message
            .as_ref()
            .map(|s| s.as_ptr())
            .unwrap_or(ptr::null())
            .cast_mut()
    })
    .cast_const()
}
/// Borrow a view until the owning tree is freed. NULL means absent/wrong arm.
/// Invalid pointer/index sets last_error. Do not free the borrowed view separately.
/// # Safety
/// value must be a live readable owner.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_device_event_device_v2(
    value: *const FlexDeviceEventV2,
) -> *const FlexDeviceInfo {
    guard_ptr(|| {
        error::clear_last_error();
        if !valid(value) {
            invalid();
            return ptr::null_mut();
        }
        let v = &*value;
        v.device
            .as_ref()
            .map(|e| e as *const FlexDeviceInfo)
            .unwrap_or(ptr::null())
            .cast_mut()
    })
    .cast_const()
}
/// Borrow a view until the owning tree is freed. NULL means absent/wrong arm.
/// Invalid pointer/index sets last_error. Do not free the borrowed view separately.
/// # Safety
/// value must be a live readable owner.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_device_event_id_v2(
    value: *const FlexDeviceEventV2,
) -> *const c_char {
    guard_ptr(|| {
        error::clear_last_error();
        if !valid(value) {
            invalid();
            return ptr::null_mut();
        }
        let v = &*value;
        v.id.as_ref()
            .map(|s| s.as_ptr())
            .unwrap_or(ptr::null())
            .cast_mut()
    })
    .cast_const()
}
/// Borrow a view until the owning tree is freed. NULL means absent/wrong arm.
/// Invalid pointer/index sets last_error. Do not free the borrowed view separately.
/// # Safety
/// value must be a live readable owner.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_error_message_v2(
    value: *const FlexErrorInfoV2,
) -> *const c_char {
    guard_ptr(|| {
        error::clear_last_error();
        if !valid(value) {
            invalid();
            return ptr::null_mut();
        }
        let v = &*value;
        v.message.as_ptr().cast_mut()
    })
    .cast_const()
}
/// Borrow a view until the owning tree is freed. NULL means absent/wrong arm.
/// Invalid pointer/index sets last_error. Do not free the borrowed view separately.
/// # Safety
/// value must be a live readable owner.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_error_secondary_v2(
    value: *const FlexErrorInfoV2,
    index: usize,
) -> *const FlexErrorInfoV2 {
    guard_ptr(|| {
        error::clear_last_error();
        if !valid(value) {
            invalid();
            return ptr::null_mut();
        }
        let v = &*value;
        if index >= v.secondary.len() {
            invalid();
            return ptr::null();
        }
        v.secondary
            .get(index)
            .map(|e| e as *const FlexErrorInfoV2)
            .unwrap_or(ptr::null())
            .cast_mut()
    })
    .cast_const()
}
/// Borrow a view until the owning tree is freed. NULL means absent/wrong arm.
/// Invalid pointer/index sets last_error. Do not free the borrowed view separately.
/// # Safety
/// value must be a live readable owner.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_shutdown_primary_v2(
    value: *const FlexShutdownReportV2,
) -> *const FlexErrorInfoV2 {
    guard_ptr(|| {
        error::clear_last_error();
        if !valid(value) {
            invalid();
            return ptr::null_mut();
        }
        let v = &*value;
        v.primary
            .as_ref()
            .map(|e| e as *const FlexErrorInfoV2)
            .unwrap_or(ptr::null())
            .cast_mut()
    })
    .cast_const()
}
/// Borrow a view until the owning tree is freed. NULL means absent/wrong arm.
/// Invalid pointer/index sets last_error. Do not free the borrowed view separately.
/// # Safety
/// value must be a live readable owner.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_shutdown_cleanup_v2(
    value: *const FlexShutdownReportV2,
    index: usize,
) -> *const FlexErrorInfoV2 {
    guard_ptr(|| {
        error::clear_last_error();
        if !valid(value) {
            invalid();
            return ptr::null_mut();
        }
        let v = &*value;
        if index >= v.cleanup.len() {
            invalid();
            return ptr::null();
        }
        v.cleanup
            .get(index)
            .map(|e| e as *const FlexErrorInfoV2)
            .unwrap_or(ptr::null())
            .cast_mut()
    })
    .cast_const()
}
