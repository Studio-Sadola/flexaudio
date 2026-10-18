//! Additive versioned capture configuration and immutable secondary metadata.
use crate::{types::FlexConfig, vad_integration::FlexStreamVadOptions};

pub const FLEX_STREAM_VERSION_3: u32 = 3;
pub const FLEX_SECONDARY_F32: u32 = 1;
pub const FLEX_SECONDARY_S16: u32 = 2;
pub const FLEX_KEEPALIVE_DEFAULT: u32 = 0;
pub const FLEX_KEEPALIVE_ON: u32 = 1;
pub const FLEX_KEEPALIVE_OFF: u32 = 2;

#[derive(Clone, Copy)]
pub(crate) enum SecondaryEncoding {
    F32,
    S16,
}

impl SecondaryEncoding {
    pub(crate) fn code(self) -> u32 {
        match self {
            Self::F32 => FLEX_SECONDARY_F32,
            Self::S16 => FLEX_SECONDARY_S16,
        }
    }
}

/// Borrowed open envelope. Initialize size to sizeof(FlexStreamConfigV3) and version to 3.
#[repr(C)]
pub struct FlexStreamConfigV3 {
    pub size: u32,
    pub version: u32,
    pub config: *const FlexConfig,
    /// NULL disables attached VAD.
    pub vad: *const FlexStreamVadOptions,
    pub exclude_pids: *const u32,
    pub exclude_pids_len: usize,
    /// NULL disables secondary output.
    pub secondary_output: *const FlexSecondaryOutputV3,
    /// 0 default (enabled), 1 enabled, 2 disabled.
    pub windows_classic_loopback_keepalive: u32,
}

/// Borrowed source switch envelope. Output formats, gain and denoise stay fixed at open.
#[repr(C)]
pub struct FlexSwitchConfigV3 {
    pub size: u32,
    pub version: u32,
    pub config: *const FlexConfig,
    pub exclude_pids: *const u32,
    pub exclude_pids_len: usize,
    /// 0 retain current setting, 1 enabled, 2 disabled.
    pub windows_classic_loopback_keepalive: u32,
}

/// Secondary format, fixed for the stream lifetime. Rate/channels are literal, not sentinels.
#[repr(C)]
pub struct FlexSecondaryOutputV3 {
    pub size: u32,
    pub version: u32,
    pub rate: u32,
    pub channels: u16,
    /// FLEX_SECONDARY_F32 or FLEX_SECONDARY_S16. Unknown values are rejected.
    pub encoding: u32,
}

/// Secondary metadata; meters describe float samples before integer quantization.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FlexSecondaryMetadataV3 {
    pub size: u32,
    pub version: u32,
    pub rate: u32,
    pub channels: u16,
    pub encoding: u32,
    pub frames: u32,
    pub frame_index: u64,
    pub pts_ns: i64,
    pub seq: u64,
    pub flags: u32,
    pub dropped_before: u32,
    pub peak: f32,
    pub rms: f32,
}
