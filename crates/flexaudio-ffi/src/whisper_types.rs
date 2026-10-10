//! Additive fixed-width whisper-compatible C contracts. Legacy layouts are unchanged.

/// Pinned segmentation parameters. NULL uses defaults; every supplied zero is literal.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FlexWhisperVadParams {
    pub threshold: f32,
    pub min_speech_duration_ms: i32,
    pub min_silence_duration_ms: i32,
    pub max_speech_duration_s: f32,
    pub speech_pad_ms: i32,
}

/// Preview policy. provisional must be 0 (disabled) or 1 (enabled).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FlexWhisperVadOptions {
    pub provisional: u8,
}

/// Half-open final interval on the 10 ms grid; its end may exceed physical EOF.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlexWhisperSpeechSegment {
    pub start_ms: u64,
    pub end_ms: u64,
}

pub const FLEX_WHISPER_SEGMENT: u32 = 1;
pub const FLEX_WHISPER_SPEECH_START: u32 = 2;
pub const FLEX_WHISPER_SPEECH_END: u32 = 3;
pub const FLEX_WHISPER_CUT: u32 = 4;
pub const FLEX_WHISPER_EPOCH_END: u32 = 5;
pub const FLEX_WHISPER_HYSTERESIS: u32 = 1;
pub const FLEX_WHISPER_FINISH: u32 = 2;
pub const FLEX_WHISPER_RESET: u32 = 3;
pub const FLEX_WHISPER_ERROR: u32 = 4;
pub const FLEX_WHISPER_LIMIT: u32 = 5;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct FlexWhisperSpeechStart {
    pub at_ms: u64,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FlexWhisperSpeechEnd {
    pub at_ms: u64,
    pub reason: u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FlexWhisperCut {
    pub start_ms: u64,
    pub end_ms: u64,
    pub reason: u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FlexWhisperEpochEnd {
    pub reason: u32,
}

/// Read only the payload selected by the event's type tag.
#[repr(C)]
#[derive(Clone, Copy)]
pub union FlexWhisperVadPayload {
    pub segment: FlexWhisperSpeechSegment,
    pub speech_start: FlexWhisperSpeechStart,
    pub speech_end: FlexWhisperSpeechEnd,
    pub cut: FlexWhisperCut,
    pub epoch_end: FlexWhisperEpochEnd,
}

/// Standalone ordered event. All times are epoch-relative integer milliseconds.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FlexWhisperVadEvent {
    pub r#type: u32,
    pub epoch: u32,
    pub seq: u64,
    pub data: FlexWhisperVadPayload,
}
