//! Immutable settings owners and borrowed item projections.
use crate::{
    boundary::{invalid, output},
    convert::valid_pointer,
    error, guard_i32,
    types::FlexStream,
    v2_storage::{cstring, FlexErrorInfoV2},
    v4_types::*,
};
use flexaudio::{SettingOutcome, SettingValue, SettingsReport};
use std::{ffi::CString, os::raw::c_char, ptr};

struct Value {
    value: SettingValue,
    text: Option<CString>,
}
impl Value {
    fn new(value: SettingValue) -> Self {
        let text = match &value {
            SettingValue::Text(text) => Some(cstring(text.clone())),
            _ => None,
        };
        Self { value, text }
    }
    fn view(&self) -> Result<FlexValueV4, i32> {
        use FlexValueKindV4::*;
        let (kind, value) = match &self.value {
            SettingValue::Absent => (FLEX_VALUE_ABSENT, FlexValuePayloadV4 { unsigned_value: 0 }),
            SettingValue::Bool(v) => (FLEX_VALUE_BOOL, FlexValuePayloadV4 { boolean: *v }),
            SettingValue::Unsigned(v) => (
                FLEX_VALUE_UNSIGNED,
                FlexValuePayloadV4 { unsigned_value: *v },
            ),
            SettingValue::Float(v) => (FLEX_VALUE_FLOAT, FlexValuePayloadV4 { float_value: *v }),
            SettingValue::Text(_) => (
                FLEX_VALUE_TEXT,
                FlexValuePayloadV4 {
                    text: self.text.as_ref().map_or(ptr::null(), |v| v.as_ptr()),
                },
            ),
            SettingValue::Pids(v) => (
                FLEX_VALUE_PIDS,
                FlexValuePayloadV4 {
                    pids: FlexPidsV4 {
                        data: if v.is_empty() {
                            ptr::null()
                        } else {
                            v.as_ptr()
                        },
                        len: v.len(),
                    },
                },
            ),
            SettingValue::Format(v) => (
                FLEX_VALUE_FORMAT,
                FlexValuePayloadV4 {
                    format: FlexFormatV4 {
                        sample_rate: v.sample_rate,
                        channels: v.channels,
                    },
                },
            ),
            _ => return Err(invalid("unrecognized settings value")),
        };
        Ok(FlexValueV4 { kind, value })
    }
}
struct Entry {
    setting: CString,
    status: u32,
    reason: Option<CString>,
    requested: Value,
    effective: Value,
}
/// Immutable snapshot owner. Item strings and arrays borrow from this object.
/// Standalone snapshots must be freed exactly once; error-owned views must not be freed.
pub struct FlexSettingsV4 {
    revision: u64,
    entries: Vec<Entry>,
}
impl FlexSettingsV4 {
    pub(crate) fn new(report: &SettingsReport) -> Self {
        Self {
            revision: report.revision,
            entries: report
                .entries
                .iter()
                .map(|entry| {
                    let (status, reason) = match entry.outcome {
                        SettingOutcome::NotRequested => (FLEX_SETTING_NOT_REQUESTED, None),
                        SettingOutcome::Applied => (FLEX_SETTING_APPLIED, None),
                        SettingOutcome::Ignored(reason) => {
                            (FLEX_SETTING_IGNORED, Some(cstring(reason.as_str())))
                        }
                        SettingOutcome::Adjusted(reason) => {
                            (FLEX_SETTING_ADJUSTED, Some(cstring(reason.as_str())))
                        }
                        _ => (u32::MAX, None),
                    };
                    Entry {
                        setting: cstring(entry.setting.as_str()),
                        status,
                        reason,
                        requested: Value::new(entry.requested.clone()),
                        effective: Value::new(entry.effective.clone()),
                    }
                })
                .collect(),
        }
    }
}

/// Copy the last committed settings into an independent owner. out is NULL on failure.
/// # Safety
/// stream is a live handle; out is writable. Free the returned snapshot with settings_free_v4.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_settings_v4(
    stream: *const FlexStream,
    out: *mut *mut FlexSettingsV4,
) -> i32 {
    guard_i32(|| {
        error::clear_last_error();
        if let Err(status) = output(out) {
            return status;
        }
        if !valid_pointer(stream) {
            return invalid("invalid stream handle");
        }
        let Some(report) = (*stream).inner.effective_settings() else {
            return error::set_audio_error(flexaudio::Error::InvalidState(
                "stream has no settings report".into(),
            ));
        };
        out.write(Box::into_raw(Box::new(FlexSettingsV4::new(report))));
        0
    })
}
/// Return revision and item count, with zero on success.
/// # Safety
/// settings is a live owner/view, and both outputs are writable.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_settings_count_v4(
    settings: *const FlexSettingsV4,
    revision: *mut u64,
    count: *mut usize,
) -> i32 {
    guard_i32(|| {
        error::clear_last_error();
        if !valid_pointer(settings) || !valid_pointer(revision) || !valid_pointer(count) {
            return invalid("invalid settings count pointer");
        }
        revision.write((*settings).revision);
        count.write((*settings).entries.len());
        0
    })
}
/// Borrow an item from a snapshot; all pointers remain owned by the snapshot.
/// # Safety
/// settings is live and out is writable. index must be less than the reported count.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_settings_item_v4(
    settings: *const FlexSettingsV4,
    index: usize,
    out: *mut FlexSettingResolutionV4,
) -> i32 {
    guard_i32(|| {
        error::clear_last_error();
        if !valid_pointer(settings) || !valid_pointer(out) {
            return invalid("invalid settings item pointer");
        }
        let settings = &*settings;
        let Some(entry) = settings.entries.get(index) else {
            return invalid("settings index out of bounds");
        };
        let (requested, effective) = match (entry.requested.view(), entry.effective.view()) {
            (Ok(a), Ok(b)) => (a, b),
            (Err(status), _) | (_, Err(status)) => return status,
        };
        out.write(FlexSettingResolutionV4 {
            setting: entry.setting.as_ptr(),
            status: entry.status,
            reason: entry
                .reason
                .as_ref()
                .map_or(ptr::null::<c_char>(), |v| v.as_ptr()),
            requested,
            effective,
        });
        0
    })
}
/// Free a standalone settings snapshot; NULL-safe. Never free an error-owned view.
/// # Safety
/// settings is NULL or an unfreed standalone snapshot from settings_v4.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_settings_free_v4(settings: *mut FlexSettingsV4) {
    guard_i32(|| {
        if settings.is_null() {
            return 0;
        }
        if !valid_pointer(settings) {
            return invalid("invalid settings owner");
        }
        drop(Box::from_raw(settings));
        0
    });
}
/// Borrow the candidate report from an error: 1 present, 0 absent, negative on error.
/// The view and its items live until the error owner is freed. out is NULL on absence/error.
/// # Safety
/// error is a live error owner and out is writable. Do not free the borrowed view.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_error_settings_v4(
    value: *const FlexErrorInfoV2,
    out: *mut *const FlexSettingsV4,
) -> i32 {
    guard_i32(|| {
        error::clear_last_error();
        if !valid_pointer(out) {
            return invalid("invalid settings view output");
        }
        out.write(ptr::null());
        if !valid_pointer(value) {
            return invalid("invalid error owner");
        }
        match (*value).settings.as_ref() {
            Some(settings) => {
                out.write(settings);
                1
            }
            None => 0,
        }
    })
}
