//! Additive presence-aware gains over unchanged v4 configuration envelopes.
use crate::v4_types::{FlexStreamConfigV4, FlexSwitchConfigV4};

pub const FLEX_STREAM_VERSION_5: u32 = 5;

/// Borrowed gains. Each NULL pointer selects 1.0; each supplied float is literal,
/// including 0.0 (mute). Values must be finite and nonnegative, with no upper bound.
/// These replace all three gain fields in the nested FlexConfig, which must be zero.
/// Nonzero legacy gain fields return InvalidArg; use FlexGainsV5 instead.
#[repr(C)]
pub struct FlexGainsV5 {
    pub size: u32,
    pub version: u32,
    pub gain: *const f32,
    pub mix_mic_gain: *const f32,
    pub mix_system_gain: *const f32,
}

/// Borrowed v5 envelope; exact size/version required. base uses unchanged v4/v3
/// layouts and controls. NULL gains omits all gains. Nested FlexConfig gain,
/// mix_mic_gain and mix_system_gain must be zero; use FlexGainsV5 for gain values.
#[repr(C)]
pub struct FlexStreamConfigV5 {
    pub size: u32,
    pub version: u32,
    pub base: *const FlexStreamConfigV4,
    pub gains: *const FlexGainsV5,
}

/// Borrowed v5 switch envelope. Omitted gain retains the current output gain;
/// explicit non-default gain is fixedAtOpen (strict rejects, permissive ignores).
/// Mix gains select 1.0 when omitted and literal values when supplied on switch.
/// Nested FlexConfig gain fields must be zero, as in FlexStreamConfigV5.
#[repr(C)]
pub struct FlexSwitchConfigV5 {
    pub size: u32,
    pub version: u32,
    pub base: *const FlexSwitchConfigV4,
    pub gains: *const FlexGainsV5,
}
