//! Fixed-width output records for the opaque v2 API.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FlexAudioLossV2 {
    /// 0 capture, 1 Mix FIFO, 2 output.
    pub path: u32,
    /// 0 none, 1 microphone, 2 system audio.
    pub lane: u32,
    /// 0 primary, 1 secondary; used only for output path.
    pub tap: u32,
    /// 0 raw overflow, 1 FIFO overflow, 2 corrupt, 3 malformed, 4 callback rejected, 5 output overflow.
    pub reason: u32,
    /// 0 unknown, 1 known positive scalar interleaved sample count.
    pub count_known: u32,
    pub samples: u64,
    pub sample_rate: u32,
    pub channels: u16,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FlexNativeFormatV2 {
    pub sample_rate: u32,
    pub channels: u16,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FlexNativeFormatChangeV2 {
    pub advertised: FlexNativeFormatV2,
    pub actual: FlexNativeFormatV2,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FlexErrorContextV2 {
    /// 0 enumerate, 1 start, 2 normalize, 3 flush, 4 reopen, 5 rollback, 6 stop, 7 join, 8 link.
    pub operation: u32,
    /// 0 none, 1 microphone, 2 system audio.
    pub lane: u32,
    /// 0 none, 1 HRESULT, 2 OSStatus.
    pub native_code_kind: u32,
    pub native_code: i64,
    /// Borrowed from the error owner; never free separately.
    pub native_call: *const std::os::raw::c_char,
}

/// Late microphone consent; permission getter returns 1.
pub const FLEX_EVENT_KIND_PERMISSION_GRANTED: i32 = 9;
/// Validated interval sample loss.
pub const FLEX_EVENT_KIND_AUDIO_LOSS: i32 = 10;
/// Cleanup failure; does not replace the capture primary.
pub const FLEX_EVENT_KIND_SHUTDOWN_ERROR: i32 = 11;
/// Fatal typed capture failure.
pub const FLEX_EVENT_KIND_TERMINAL_ERROR: i32 = 12;
/// Advisory typed failure; capture may continue.
pub const FLEX_EVENT_KIND_RECOVERABLE_ERROR: i32 = 13;
/// Coalesced upstream clipping, without exact chunk attribution.
pub const FLEX_EVENT_KIND_CLIPPED: i32 = 14;
/// The default endpoint no longer exists; no fabricated device ID.
pub const FLEX_DEVICE_EVENT_KIND_DEFAULT_CLEARED: i32 = 4;
/// Incremental inventory is invalid; obtain a complete inventory again.
pub const FLEX_DEVICE_EVENT_KIND_RESCAN_REQUIRED: i32 = 5;
