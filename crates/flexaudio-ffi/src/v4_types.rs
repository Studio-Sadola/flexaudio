//! Additive C layouts for decoded settings and capture envelopes.
use crate::{
    types::FlexDeviceInfo,
    v3_types::{FlexStreamConfigV3, FlexSwitchConfigV3},
};
use std::os::raw::c_char;

pub const FLEX_STREAM_VERSION_4: u32 = 4;
pub const FLEX_SETTING_NOT_REQUESTED: u32 = 0;
pub const FLEX_SETTING_APPLIED: u32 = 1;
pub const FLEX_SETTING_IGNORED: u32 = 2;
pub const FLEX_SETTING_ADJUSTED: u32 = 3;

/// Borrowed controls. Strict: 0 strict on open/retain on switch, 1 strict, 2 permissive.
/// NULL ring capacity uses 50 on open and retains capacity on switch.
#[repr(C)]
pub struct FlexOptionsV4 {
    pub size: u32,
    pub version: u32,
    pub strict: u32,
    pub ring_capacity_chunks: *const u64,
}
/// Borrowed envelope; size/version must exactly match before other fields are read.
#[repr(C)]
pub struct FlexStreamConfigV4 {
    pub size: u32,
    pub version: u32,
    pub base: *const FlexStreamConfigV3,
    pub options: *const FlexOptionsV4,
}
/// Borrowed switch envelope. NULL options retains controls and capacity.
#[repr(C)]
pub struct FlexSwitchConfigV4 {
    pub size: u32,
    pub version: u32,
    pub base: *const FlexSwitchConfigV3,
    pub options: *const FlexOptionsV4,
}
/// cbindgen:rename-all=ScreamingSnakeCase
#[repr(C)]
#[allow(non_camel_case_types)]
#[derive(Clone, Copy)]
pub enum FlexValueKindV4 {
    FLEX_VALUE_ABSENT = 0,
    FLEX_VALUE_BOOL = 1,
    FLEX_VALUE_UNSIGNED = 2,
    FLEX_VALUE_FLOAT = 3,
    FLEX_VALUE_TEXT = 4,
    FLEX_VALUE_PIDS = 5,
    FLEX_VALUE_FORMAT = 6,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FlexPidsV4 {
    pub data: *const u32,
    pub len: usize,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FlexFormatV4 {
    pub sample_rate: u32,
    pub channels: u16,
}
/// Read only the union member selected by kind.
#[repr(C)]
#[derive(Clone, Copy)]
pub union FlexValuePayloadV4 {
    pub boolean: bool,
    pub unsigned_value: u64,
    pub float_value: f32,
    pub text: *const c_char,
    pub pids: FlexPidsV4,
    pub format: FlexFormatV4,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FlexValueV4 {
    pub kind: FlexValueKindV4,
    pub value: FlexValuePayloadV4,
}
/// Borrowed views valid until their settings owner is freed. reason is NULL for
/// not-requested/applied, otherwise a shared reason tag. Never free item pointers.
#[repr(C)]
pub struct FlexSettingResolutionV4 {
    pub setting: *const c_char,
    pub status: u32,
    pub reason: *const c_char,
    pub requested: FlexValueV4,
    pub effective: FlexValueV4,
}
/// Owned device strings; namespace strings have static lifetime.
#[repr(C)]
pub struct FlexNamespacedDeviceV4 {
    pub device: FlexDeviceInfo,
    pub id_namespace: *const c_char,
}
