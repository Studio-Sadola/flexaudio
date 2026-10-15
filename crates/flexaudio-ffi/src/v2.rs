//! Opaque v2 polling and conditional borrowed/scalar accessors.
//! Handles must be live library owners, and outputs must be writable for their declared type.
//! NULL and misalignment are rejected before dereference; no getter transfers ownership.
use crate::v2_records::*;
use crate::v2_storage::*;
use crate::{
    error::{self, code},
    guard_const_ptr, guard_i32, guard_ptr,
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
    guard_const_ptr(|| {
        error::clear_last_error();
        if !valid(value) {
            invalid();
            return ptr::null();
        }
        let v = &*value;
        v.error
            .as_ref()
            .map(|e| e as *const FlexErrorInfoV2)
            .unwrap_or(ptr::null())
    })
}
/// Borrow a view until the owning tree is freed. NULL means absent/wrong arm.
/// Invalid pointer/index sets last_error. Do not free the borrowed view separately.
/// # Safety
/// value must be a live readable owner.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_event_message_v2(value: *const FlexEventV2) -> *const c_char {
    guard_const_ptr(|| {
        error::clear_last_error();
        if !valid(value) {
            invalid();
            return ptr::null();
        }
        let v = &*value;
        v.message
            .as_ref()
            .map(|s| s.as_ptr())
            .unwrap_or(ptr::null())
    })
}
/// Borrow a view until the owning tree is freed. NULL means absent/wrong arm.
/// Invalid pointer/index sets last_error. Do not free the borrowed view separately.
/// # Safety
/// value must be a live readable owner.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_device_event_device_v2(
    value: *const FlexDeviceEventV2,
) -> *const FlexDeviceInfo {
    guard_const_ptr(|| {
        error::clear_last_error();
        if !valid(value) {
            invalid();
            return ptr::null();
        }
        let v = &*value;
        v.device
            .as_ref()
            .map(|e| e as *const FlexDeviceInfo)
            .unwrap_or(ptr::null())
    })
}
/// Borrow a view until the owning tree is freed. NULL means absent/wrong arm.
/// Invalid pointer/index sets last_error. Do not free the borrowed view separately.
/// # Safety
/// value must be a live readable owner.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_device_event_id_v2(
    value: *const FlexDeviceEventV2,
) -> *const c_char {
    guard_const_ptr(|| {
        error::clear_last_error();
        if !valid(value) {
            invalid();
            return ptr::null();
        }
        let v = &*value;
        v.id.as_ref().map(|s| s.as_ptr()).unwrap_or(ptr::null())
    })
}
/// Borrow a view until the owning tree is freed. NULL means absent/wrong arm.
/// Invalid pointer/index sets last_error. Do not free the borrowed view separately.
/// # Safety
/// value must be a live readable owner.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_error_message_v2(
    value: *const FlexErrorInfoV2,
) -> *const c_char {
    guard_const_ptr(|| {
        error::clear_last_error();
        if !valid(value) {
            invalid();
            return ptr::null();
        }
        let v = &*value;
        v.message.as_ptr()
    })
}
/// Borrow the stable VAD cause code until the error owner is freed.
/// NULL means this error has no VAD cause; invalid pointers set last_error.
/// # Safety
/// value must be a live readable error owner. Do not free the borrowed string.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_error_vad_code_v2(
    value: *const FlexErrorInfoV2,
) -> *const c_char {
    guard_const_ptr(|| {
        error::clear_last_error();
        if !valid(value) {
            invalid();
            return ptr::null();
        }
        (*value)
            .vad_code
            .as_ref()
            .map_or(ptr::null(), |code| code.as_ptr())
    })
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
    guard_const_ptr(|| {
        error::clear_last_error();
        if !valid(value) {
            invalid();
            return ptr::null();
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
    })
}
/// Borrow a view until the owning tree is freed. NULL means absent/wrong arm.
/// Invalid pointer/index sets last_error. Do not free the borrowed view separately.
/// # Safety
/// value must be a live readable owner.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_shutdown_primary_v2(
    value: *const FlexShutdownReportV2,
) -> *const FlexErrorInfoV2 {
    guard_const_ptr(|| {
        error::clear_last_error();
        if !valid(value) {
            invalid();
            return ptr::null();
        }
        let v = &*value;
        v.primary
            .as_ref()
            .map(|e| e as *const FlexErrorInfoV2)
            .unwrap_or(ptr::null())
    })
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
    guard_const_ptr(|| {
        error::clear_last_error();
        if !valid(value) {
            invalid();
            return ptr::null();
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
    })
}

/// Suppressed microphone native frames; 0 for other event arms, output untouched.
/// # Safety
/// value must be a live event; out must be writable and aligned.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_event_startup_frames_v2(
    value: *const FlexEventV2,
    out: *mut u64,
) -> i32 {
    scalar(value, out, |event| Ok(event.startup_frames))
}
/// Startup-drop reason; currently FLEX_STARTUP_DROP_OUT_OF_RANGE (includes non-finite samples).
/// # Safety
/// value must be a live event; out must be writable and aligned.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_event_startup_reason_v2(
    value: *const FlexEventV2,
    out: *mut i32,
) -> i32 {
    scalar(value, out, |event| Ok(event.startup_reason))
}

#[cfg(test)]
mod startup_tests {
    #[test]
    fn startup_mapping_exhaustively_matches_the_typed_reason() {
        // The enum has one variant today, so a semantic assertion cannot detect
        // a hardcoded reason. Keep the boundary exhaustive when the contract grows.
        let source = include_str!("v2_storage.rs");
        let arm = source
            .split_once("Event::StartupSamplesDropped")
            .expect("startup event mapping")
            .1
            .split_once("Event::Clipped")
            .expect("end of startup event mapping")
            .0;
        assert!(
            arm.contains("{ frames, reason }"),
            "startup mapping must retain its typed reason"
        );
        let arms = arm
            .split_once("match reason {")
            .expect("startup reason must be mapped exhaustively")
            .1
            .split_once("});")
            .expect("end of reason mapping")
            .0;
        let mut count = 0;
        for line in arms.lines().filter(|line| line.contains("=>")) {
            let pattern = line.split_once("=>").expect("match arm").0.trim();
            assert!(
                pattern.starts_with("flexaudio::StartupDropReason::"),
                "startup reasons must use typed variant arms, without a wildcard"
            );
            assert!(
                pattern.split('|').all(|variant| variant.trim() != "_"),
                "startup reason alternatives must not contain a wildcard"
            );
            count += 1;
        }
        assert!(count > 0, "typed startup mapping must contain a reason arm");
    }

    use super::*;
    #[test]
    fn startup_accessors_preserve_unsigned_frames_and_reject_wrong_arms() {
        let event = FlexEventV2::new(flexaudio::Event::StartupSamplesDropped {
            frames: u64::MAX,
            reason: flexaudio::StartupDropReason::OutOfRange,
        });
        let control = FlexEventV2::new(flexaudio::Event::Clipped);
        let mut frames = 7;
        let mut reason = -1;
        unsafe {
            assert_eq!(flexaudio_event_startup_frames_v2(&event, &mut frames), 1);
            assert_eq!(frames, u64::MAX);
            assert_eq!(flexaudio_event_startup_reason_v2(&event, &mut reason), 1);
            assert_eq!(reason, FLEX_STARTUP_DROP_OUT_OF_RANGE);
            assert_eq!(flexaudio_event_startup_frames_v2(&control, &mut frames), 0);
            assert_eq!(frames, u64::MAX);
            assert_eq!(
                flexaudio_event_startup_reason_v2(&event, ptr::null_mut()),
                code::FLEX_INVALID_ARG
            );
        }
    }
}

/// Borrow the typed watcher failure; return 1 when present, 0 for other event arms.
/// # Safety
/// value must be live; out must be writable and aligned. The view lives until value is freed.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_device_event_error_v2(
    value: *const FlexDeviceEventV2,
    out: *mut *const FlexErrorInfoV2,
) -> i32 {
    scalar(value, out, |v| {
        Ok(v.error.as_ref().map(|error| error as *const _))
    })
}
/// Borrow the optional typed addon cause code.
/// # Safety
/// value must be live; out must be writable and aligned. Never free the string separately.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_error_addon_kind_v2(
    value: *const FlexErrorInfoV2,
    out: *mut *const c_char,
) -> i32 {
    scalar(value, out, |v| {
        Ok(v.addon_kind.as_ref().map(|text| text.as_ptr()))
    })
}
/// Borrow the optional opt-in addon cause message.
/// # Safety
/// value must be live; out must be writable and aligned. Never free the string separately.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_error_addon_message_v2(
    value: *const FlexErrorInfoV2,
    out: *mut *const c_char,
) -> i32 {
    scalar(value, out, |v| {
        Ok(v.addon_message.as_ref().map(|text| text.as_ptr()))
    })
}
/// Count opt-in native diagnostic details, excluded from the safe display message.
/// # Safety
/// value must be live; out must be writable and aligned.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_error_diagnostic_count_v2(
    value: *const FlexErrorInfoV2,
    out: *mut u32,
) -> i32 {
    scalar(value, out, |v| {
        u32::try_from(v.diagnostic_details.len())
            .map(Some)
            .map_err(|_| ())
    })
}
/// Borrow a native diagnostic detail. Treat its contents as private host data.
/// # Safety
/// value must be live; out must be writable and aligned. Never free the string separately.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_error_diagnostic_detail_v2(
    value: *const FlexErrorInfoV2,
    index: u32,
    out: *mut *const c_char,
) -> i32 {
    scalar(value, out, |v| {
        v.diagnostic_details
            .get(index as usize)
            .map(|text| Some(text.as_ptr()))
            .ok_or(())
    })
}

#[cfg(test)]
mod retained_cause_tests {
    use super::*;
    use flexaudio::core::{AddonErrorKind, DeviceEvent, Error, ErrorContext, Operation};
    #[test]
    fn terminal_watcher_exposes_borrowed_addon_and_opt_in_native_details() {
        let error = Error::Addon {
            kind: AddonErrorKind::VadReset,
            message: "synthetic reset cause".into(),
        }
        .with_context(ErrorContext::new(Operation::Flush))
        .with_diagnostic_detail("synthetic native detail".into());
        let event = FlexDeviceEventV2::new(DeviceEvent::TerminalError { error });
        let mut kind = -1;
        let mut view = ptr::null();
        // SAFETY: owners and aligned outputs remain live throughout these borrowed accesses.
        unsafe {
            assert_eq!(flexaudio_device_event_kind_v2(&event, &mut kind), 1);
            assert_eq!(kind, 6);
            assert_eq!(flexaudio_device_event_error_v2(&event, &mut view), 1);
            let mut text = ptr::null();
            assert_eq!(flexaudio_error_addon_kind_v2(view, &mut text), 1);
            assert_eq!(std::ffi::CStr::from_ptr(text).to_str().unwrap(), "vadReset");
            assert_eq!(flexaudio_error_addon_message_v2(view, &mut text), 1);
            assert_eq!(
                std::ffi::CStr::from_ptr(text).to_str().unwrap(),
                "synthetic reset cause"
            );
            let mut count = 0;
            assert_eq!(flexaudio_error_diagnostic_count_v2(view, &mut count), 1);
            assert_eq!(count, 1);
            assert_eq!(flexaudio_error_diagnostic_detail_v2(view, 0, &mut text), 1);
            assert_eq!(
                std::ffi::CStr::from_ptr(text).to_str().unwrap(),
                "synthetic native detail"
            );
            assert!(!std::ffi::CStr::from_ptr(flexaudio_error_message_v2(view))
                .to_str()
                .unwrap()
                .contains("synthetic"));
        }
    }
}
