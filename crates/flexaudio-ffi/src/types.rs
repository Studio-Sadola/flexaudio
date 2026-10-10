//! `#[repr(C)]` types and opaque handles exposed through the C ABI.
//!
//! cbindgen copies these directly to structs / enums in `flexaudio.h`. Keep the layout consistent
//! with the C side; do not change field types or order.

use std::os::raw::c_char;

use flexaudio_denoise::Denoiser;
use flexaudio_vad::Vad;

/// Audio source kind to record (corresponds to [`flexaudio::SourceKind`]).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlexSourceKind {
    /// Microphone input.
    Mic = 0,
    /// Loopback of all system output.
    System = 1,
    /// Output loopback for a specific process.
    Process = 2,
    /// Record microphone and system audio mixed into one stream.
    Mix = 3,
}

/// Whether to include or exclude the target PID for process sources (corresponds to [`flexaudio::ProcessMode`]).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlexProcessMode {
    /// Capture only the target PID (and its process tree).
    Include = 0,
    /// Capture all system audio except the target PID.
    Exclude = 1,
}

/// VAD (voice activity detection) configuration. Passed to `FlexConfig::vad` and `flexaudio_vad_new`.
///
/// For each value, the sentinel 0 selects the default from [`flexaudio_vad::VadConfig`].
/// `threshold` 0 → 0.5, `neg_threshold` 0 → Silero formula `max(threshold-0.15, 0.01)`,
/// `min_speech_ms` 0 → 250, `min_silence_ms` 0 → 100, `speech_pad_ms` 0 → 30,
/// `sample_rate` 0 → 16000. `max_speech_ms` 0 means unlimited (the default).
///
/// [`flexaudio_vad_new`]: crate::flexaudio_vad_new
#[repr(C)]
pub struct FlexVadConfig {
    /// Probability threshold (>=) for speech start. 0 selects 0.5.
    pub threshold: f32,
    /// Lower (silence-side) threshold (<) for silence start.
    /// 0 selects the Silero formula automatically.
    pub neg_threshold: f32,
    /// Minimum accepted speech duration (ms). Shorter segments are discarded. 0 selects 250.
    pub min_speech_ms: u32,
    /// Silence duration (ms) required to finalize speech end. 0 selects 100.
    pub min_silence_ms: u32,
    /// Padding (ms) added before and after segment boundaries. 0 selects 30.
    pub speech_pad_ms: u32,
    /// Maximum segment duration (ms). 0 is unlimited (default). Longer segments are forcibly split.
    pub max_speech_ms: u32,
    /// Sample rate (8000 or 16000). 0 selects 16000.
    pub sample_rate: u32,
}

/// One event finalized by VAD. Stored in the output array of `flexaudio_vad_process` and in `FlexChunk::vad_events`
///.
///
/// `at_sample` is measured at the internal VAD rate (`sample_rate` = 8000/16000), not at the input
/// sample rate (same as [`flexaudio_vad::VadEvent`]).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlexVadEvent {
    /// Kind. 0 = speech start (SpeechStart), 1 = speech end (SpeechEnd).
    pub kind: i32,
    /// Event sample position at the internal VAD rate (start inclusive / end exclusive).
    pub at_sample: i64,
}

/// Configuration for opening a stream. Passed to `flexaudio_open` / `flexaudio_switch_source`.
///
/// Sentinel values mean "unspecified" for strings and optional values (`device_id` NULL selects the default device,
/// `process_id` 0 means none, and `output_rate`/`output_channels`/`chunk_ms` 0 select defaults).
#[repr(C)]
pub struct FlexConfig {
    /// Source kind integer (FlexSourceKind code); unknown values are rejected.
    pub kind: i32,
    /// ID of the selected device (UTF-8, NUL-terminated). NULL selects the default device.
    pub device_id: *const c_char,
    /// Target PID for a process source. 0 means none (may cause an error when starting a process source).
    pub process_id: u32,
    /// Include/exclude integer (FlexProcessMode code), validated even for other sources.
    pub mode: i32,
    /// Whether to exclude this process's playback from system audio (system source only;
    /// for mix, applies to the system side).
    /// Boolean integer: only 0 and 1 are valid.
    pub exclude_self: u8,
    /// Output sample rate (Hz). 0 selects 48000.
    pub output_rate: u32,
    /// Output channel count. 0 selects 2.
    pub output_channels: u16,
    /// Chunk duration (ms). 0 selects 20; all other non-20 values are rejected.
    pub chunk_ms: u32,
    /// Input gain at start (linear multiplier). 0 selects 1.0 (default). For runtime mute, use
    /// `flexaudio_set_gain(s, 0.0)`.
    pub gain: f32,
    /// Input device ID (UTF-8, NUL-terminated) for the mic side of mix (mix only).
    /// NULL selects the default input.
    pub mix_mic_device_id: *const c_char,
    /// Output endpoint ID (UTF-8, NUL-terminated) for the system side of mix (mix only).
    /// NULL selects the default output.
    pub mix_system_device_id: *const c_char,
    /// Pre-mix linear gain for the mic side of mix (mix only). 0 selects 1.0 (default).
    /// Global `gain` is applied after mixing.
    pub mix_mic_gain: f32,
    /// Pre-mix linear gain for the system side of mix (mix only). 0 selects 1.0 (default).
    pub mix_system_gain: f32,
    /// Whether to apply noise suppression (RNNoise) to the stream. Enabled when `true`. The output rate must be
    /// 48000 or `flexaudio_open` fails (NULL + last_error). denoise
    /// processes data in place just before `poll_chunk` returns (before VAD).
    /// Boolean integer: only 0 and 1 are valid.
    pub denoise: u8,
    /// Whether to apply VAD (voice activity detection) to the stream. When `true`, each polled chunk is processed
    /// by VAD according to `vad` and populates `FlexChunk::vad_events`.
    /// Boolean integer: only 0 and 1 are valid.
    pub has_vad: u8,
    /// VAD configuration (used only when `has_vad` is `true`; ignored when `false`).
    pub vad: FlexVadConfig,
}

/// One captured audio chunk, populated by `flexaudio_poll_chunk`.
///
/// `data` is flexaudio-owned interleaved f32 with length `len` (= `frames * channels`).
/// Always release it with `flexaudio_chunk_free` when finished (do not use C `free`).
#[repr(C)]
pub struct FlexChunk {
    /// Pointer to interleaved f32 samples. Release with `flexaudio_chunk_free`.
    pub data: *mut f32,
    /// Number of elements in `data` (= `frames * channels`).
    pub len: usize,
    /// Number of frames in the chunk.
    pub frames: u32,
    /// Monotonic presentation timestamp (ns) of the first sample.
    pub pts_ns: i64,
    /// Monotonically increasing sequence number assigned by the stream layer.
    pub seq: u64,
    /// Chunk state flags (ChunkFlags bits).
    pub flags: u32,
    /// Number of chunks dropped before this chunk arrived.
    pub dropped_before: u32,
    /// Maximum absolute delivered sample value after denoise (linear amplitude).
    pub peak: f32,
    /// Root-mean-square of delivered samples after denoise (linear).
    pub rms: f32,
    /// Events finalized by VAD for this chunk. NULL when VAD is disabled or there are no events
    /// (`vad_events_len = 0`). When non-NULL, `flexaudio_chunk_free` releases it
    /// together with `data`.
    /// On DISCONTINUITY, flushed pre-gap events precede any post-gap events. The core's fixed
    /// 20 ms chunks are shorter than a fresh 32 ms VAD frame, so this chunk contains only pre-gap
    /// events; all events on subsequent chunks use the new sample clock, restarted at zero.
    /// SpeechStart and SpeechEnd are delivered together when a segment is finalized.
    pub vad_events: *mut FlexVadEvent,
    /// Number of `vad_events`. 0 when VAD is disabled or there are no events.
    pub vad_events_len: usize,
}

/// Stream event kind (corresponds to [`flexaudio::Event`]).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlexEventKind {
    /// Chunks were dropped because the chunk ring was full (count in `FlexEvent::count`).
    ChunkDropped = 0,
    /// Data stopped arriving and the stream stalled.
    Stalled = 1,
    /// Data resumed after a stall.
    Recovered = 2,
    /// A required permission was denied; terminal, including a confirmed macOS self-probe failure.
    /// Retrieve the cause and remedy with flexaudio_last_error; capture stops.
    PermissionDenied = 3,
    /// The capture device was lost.
    DeviceLost = 4,
    /// Other backend error (retrieve the message with `flexaudio_last_error`).
    Error = 5,
    /// Event not matching a known kind (reserved for future variants).
    Unknown = 6,
    /// Exact-zero system capture with an inconclusive permission diagnosis; advisory only.
    /// Missing permission and genuine digital silence remain possible.
    /// Retrieve the explanation with flexaudio_last_error; capture continues.
    SilenceWhileSourceActive = 7,
    /// Recording consent remains undecided; advisory only, capture continues.
    /// Retrieve guidance with flexaudio_last_error; capture may stay silent until granted.
    PermissionPending = 8,
}

/// One captured event, populated by `flexaudio_poll_event`.
///
/// For Error, PermissionDenied, SilenceWhileSourceActive, and PermissionPending,
/// the message is stored in flexaudio_last_error. PermissionDenied retains kind 3;
/// PermissionPending has kind 8 and does not stop capture.
#[repr(C)]
pub struct FlexEvent {
    /// Event kind.
    pub kind: i32,
    /// Number dropped for `ChunkDropped`; 0 for other kinds.
    pub count: i64,
}

/// Information for one enumerated device (corresponds to [`flexaudio::DeviceInfo`]).
///
/// `id` / `name` are flexaudio-owned, NUL-terminated UTF-8 strings. Release the entire array with
/// `flexaudio_devices_free` (do not use C `free`).
#[repr(C)]
pub struct FlexDeviceInfo {
    /// Stable ID (released by `flexaudio_devices_free`).
    pub id: *mut c_char,
    /// Human-readable display name (released by `flexaudio_devices_free`).
    pub name: *mut c_char,
    /// Source kind used to capture this device.
    pub source_kind: i32,
    /// Native (default) sample rate (Hz).
    pub sample_rate: u32,
    /// Native (default) channel count.
    pub channels: u16,
    /// True for loopback (system-output monitor).
    pub is_loopback: bool,
    /// True if this is the OS default device.
    pub is_default: bool,
}

/// Whether the process is currently outputting audio (corresponds to [`flexaudio::ProcessInfo::is_output_active`]).
///
/// `Unknown` when the OS does not expose this state (Rust `None`).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlexOutputActivity {
    /// The OS does not expose the state or it could not be read.
    Unknown = 0,
    /// Not outputting (Linux = node is not Running / Windows = session is Inactive /
    /// macOS = IsRunningOutput is 0).
    Inactive = 1,
    /// Outputting audio.
    Active = 2,
}

/// Information for one enumerated process (corresponds to [`flexaudio::ProcessInfo`]).
///
/// Pass `pid` to `FlexConfig::process_id` to capture that process. Strings are flexaudio-owned,
/// NUL-terminated UTF-8 and released with the entire array by `flexaudio_processes_free` (do not use C `free`).
/// `executable` / `bundle_id` are NULL when unavailable.
#[repr(C)]
pub struct FlexProcessInfo {
    /// OS process ID (nonzero).
    pub pid: u32,
    /// Display name (never empty; released by `flexaudio_processes_free`).
    pub name: *mut c_char,
    /// Executable basename, or NULL if unavailable.
    pub executable: *mut c_char,
    /// macOS bundle ID, or NULL if unavailable (always NULL outside macOS).
    pub bundle_id: *mut c_char,
    /// Whether it is outputting audio (`Unknown` if unavailable).
    pub output_activity: i32,
}

/// Opaque handle for a recording stream. It contains [`flexaudio::Stream`] and any enabled
/// addons (denoise / VAD); C code holds only a pointer. Create with `flexaudio_open` and
/// release with `flexaudio_free`.
///
/// Keep addons here as part of the stream state (thin wrapper). Before `poll_chunk`
/// returns, process data through denoise → VAD. `flexaudio_switch_source` replaces only the source;
/// addons retain their configuration from open (same as gain).
pub struct FlexStream {
    pub(crate) inner: flexaudio::Stream,
    pub(crate) shutdown: Option<flexaudio::ShutdownReport>,
    pub(crate) shutdown_event_index: usize,
    pub(crate) last_output: Option<(u64, i64, u64)>,
    pub(crate) whisper: Option<flexaudio_vad::WhisperVadTap>,
    pub(crate) whisper_events: Vec<flexaudio_vad::AttachedWhisperVadEvent>,
    pub(crate) whisper_origin: (u64, i64),
    pub(crate) whisper_error: Option<flexaudio_vad::WhisperVadTapError>,
    pub(crate) whisper_error_reported: bool,
    pub(crate) ready_chunks: std::collections::VecDeque<crate::whisper_integration::FlexChunkV2>,
    /// Noise suppressor when enabled (requires 48 kHz; constructed at open). `None` when disabled.
    pub(crate) denoiser: Option<Denoiser>,
    /// VAD when enabled (constructed at open). `None` when disabled.
    pub(crate) vad: Option<Vad>,
}

impl Drop for FlexStream {
    fn drop(&mut self) {
        for mut chunk in self.ready_chunks.drain(..) {
            unsafe {
                crate::whisper_integration::flexaudio_chunk_free_v2(&mut chunk);
            }
        }
    }
}

#[cfg(test)]
#[path = "../tests/ffi/layout.rs"]
mod layout_tests;
