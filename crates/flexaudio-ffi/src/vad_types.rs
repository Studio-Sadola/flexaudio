//! Fixed-width generic VAD contracts for the C ABI.

/// Pinned segmentation parameters. NULL uses defaults; every supplied zero is literal.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FlexVadOptions {
    pub threshold: f32,
    pub min_speech_ms: i32,
    pub min_silence_ms: i32,
    /// Maximum segmentation budget in integer milliseconds. Zero or values above
    /// 100000000 use the effectively unbounded INT_MAX/2-sample sentinel (1073741823 samples).
    /// Other nonzero values must be at least 32 + 2 * speech_pad_ms.
    /// Zero split budgets are valid; negative budgets are rejected.
    /// The final segment merger can join neighboring splits.
    pub max_speech_ms: u32,
    pub speech_pad_ms: i32,
}

/// Preview policy. provisional must be 0 (disabled) or 1 (enabled).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FlexVadRuntimeOptions {
    pub provisional: u8,
}

/// Half-open final interval on the 10 ms grid; its end may exceed physical EOF.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlexSpeechSegment {
    pub start_ms: u64,
    pub end_ms: u64,
}

pub const FLEX_VAD_SEGMENT: u32 = 1;
pub const FLEX_VAD_SPEECH_START: u32 = 2;
pub const FLEX_VAD_SPEECH_END: u32 = 3;
pub const FLEX_VAD_CUT: u32 = 4;
pub const FLEX_VAD_EPOCH_END: u32 = 5;
pub const FLEX_VAD_HYSTERESIS: u32 = 1;
pub const FLEX_VAD_FINISH: u32 = 2;
pub const FLEX_VAD_RESET: u32 = 3;
pub const FLEX_VAD_ERROR: u32 = 4;
pub const FLEX_VAD_LIMIT: u32 = 5;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct FlexVadSpeechStart {
    pub at_ms: u64,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FlexVadSpeechEnd {
    pub at_ms: u64,
    pub reason: u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FlexVadCut {
    pub start_ms: u64,
    pub end_ms: u64,
    pub reason: u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FlexVadEpochEnd {
    pub reason: u32,
}

/// Read only the payload selected by the event's type tag.
#[repr(C)]
#[derive(Clone, Copy)]
pub union FlexVadPayload {
    pub segment: FlexSpeechSegment,
    pub speech_start: FlexVadSpeechStart,
    pub speech_end: FlexVadSpeechEnd,
    pub cut: FlexVadCut,
    pub epoch_end: FlexVadEpochEnd,
}

/// Standalone ordered event. All times are epoch-relative integer milliseconds.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FlexVadEvent {
    pub r#type: u32,
    pub epoch: u32,
    pub seq: u64,
    pub data: FlexVadPayload,
}
