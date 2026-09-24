//! `#[repr(C)]` types and opaque handles passed across the C ABI.
//!
//! cbindgen maps these directly to structs / enums in `flexaudio.h`. The layout must match the
//! C side, so do not change the type or order of fields arbitrarily.

use std::os::raw::c_char;

use flexaudio_denoise::Denoiser;
use flexaudio_vad::Vad;

/// Kind of audio source to record (corresponds to [`flexaudio::SourceKind`]).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlexSourceKind {
    /// Microphone input.
    Mic = 0,
    /// Loopback of the whole system output.
    System = 1,
    /// Output loopback of a specific process.
    Process = 2,
    /// Records the microphone and system audio mixed into one stream.
    Mix = 3,
}

/// Whether a process source includes or excludes the target PID (corresponds to
/// [`flexaudio::ProcessMode`]).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlexProcessMode {
    /// Records only the target PID (and its process tree).
    Include = 0,
    /// Records all system audio except the target PID.
    Exclude = 1,
}

/// VAD (speech segment detection) settings. Passed to `FlexConfig::vad` and
/// `flexaudio_vad_new`.
///
/// Each value uses the sentinel 0 for the default (mapped to the defaults of
/// [`flexaudio_vad::VadConfig`]). `threshold` 0 → 0.5, `neg_threshold` 0 → the silero formula
/// `max(threshold-0.15, 0.01)`, `min_speech_ms` 0 → 250, `min_silence_ms` 0 → 100,
/// `speech_pad_ms` 0 → 30, `sample_rate` 0 → 16000. For `max_speech_ms`, 0 itself means
/// "unlimited" (the default).
///
/// [`flexaudio_vad_new`]: crate::flexaudio_vad_new
#[repr(C)]
pub struct FlexVadConfig {
    /// Probability threshold for treating speech as started (>=). 0 means 0.5.
    pub threshold: f32,
    /// Negative threshold for treating silence as started (<). 0 means determined
    /// automatically by the silero formula.
    pub neg_threshold: f32,
    /// Minimum length (ms) of speech to accept. Shorter segments are discarded. 0 means 250.
    pub min_speech_ms: u32,
    /// Length of silence (ms) needed to finalize the end of speech. 0 means 100.
    pub min_silence_ms: u32,
    /// Padding (ms) that widens segment boundaries on both sides. 0 means 30.
    pub speech_pad_ms: u32,
    /// Maximum length (ms) of one segment. 0 is unlimited (the default). Longer segments are
    /// forcibly split.
    pub max_speech_ms: u32,
    /// Sample rate (8000 or 16000). 0 means 16000.
    pub sample_rate: u32,
}

/// One event finalized by the VAD. Goes into the output array of `flexaudio_vad_process` and
/// into `FlexChunk::vad_events`.
///
/// `at_sample` counts samples at the VAD internal rate (`sample_rate` = 8000/16000), not input
/// samples (same as [`flexaudio_vad::VadEvent`]).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlexVadEvent {
    /// Kind. 0 = speech start (SpeechStart), 1 = speech end (SpeechEnd).
    pub kind: i32,
    /// Sample position of the event (at the VAD internal rate; start is inclusive / end is
    /// exclusive).
    pub at_sample: i64,
}

/// Config for opening a stream. Passed to `flexaudio_open` / `flexaudio_switch_source`.
///
/// Strings and optional values use sentinels for "unspecified" (`device_id` NULL means the
/// default device, `process_id` 0 means none, `output_rate`/`output_channels`/`chunk_ms` 0
/// means the default).
#[repr(C)]
pub struct FlexConfig {
    /// Source kind.
    pub kind: FlexSourceKind,
    /// ID of the device to select (UTF-8, NUL-terminated). NULL means the default device.
    pub device_id: *const c_char,
    /// Target PID of a process source. 0 means none (a process source may fail at start).
    pub process_id: u32,
    /// Whether to include or exclude the target PID (process source only).
    pub mode: FlexProcessMode,
    /// Whether to exclude the host's own playback from system audio (system source only;
    /// for mix it applies to the system side).
    pub exclude_self: bool,
    /// Output sample rate (Hz). 0 means 48000.
    pub output_rate: u32,
    /// Number of output channels. 0 means 2.
    pub output_channels: u16,
    /// Chunk length (milliseconds). 0 means 20.
    pub chunk_ms: u32,
    /// Input gain (linear multiplier) at start. 0 means 1.0 (the default). To mute at runtime,
    /// use `flexaudio_set_gain(s, 0.0)`.
    pub gain: f32,
    /// ID of the input device to select for the mic side of mix (UTF-8, NUL-terminated; mix
    /// only). NULL means the default input.
    pub mix_mic_device_id: *const c_char,
    /// ID of the output endpoint to select for the system side of mix (UTF-8, NUL-terminated;
    /// mix only). NULL means the default output.
    pub mix_system_device_id: *const c_char,
    /// Pre-mix multiplier for the mic side of mix (linear; mix only). 0 means 1.0 (the
    /// default). The global `gain` is applied after mixing.
    pub mix_mic_gain: f32,
    /// Pre-mix multiplier for the system side of mix (linear; mix only). 0 means 1.0 (the
    /// default).
    pub mix_system_gain: f32,
    /// Whether to insert noise suppression (RNNoise) into the stream. `true` enables it. When
    /// enabled, `flexaudio_open` fails (NULL + last_error) unless the output rate is 48000.
    /// denoise processes data in place right before `poll_chunk` returns (before VAD).
    pub denoise: bool,
    /// Whether to insert VAD (speech segment detection) into the stream. `true` follows the
    /// `vad` settings, passes each polled chunk through the VAD, and fills
    /// `FlexChunk::vad_events`.
    pub has_vad: bool,
    /// VAD settings (used only when `has_vad` is `true`; ignored when `false`).
    pub vad: FlexVadConfig,
}

/// Audio data of one retrieved chunk. Filled by `flexaudio_poll_chunk`.
///
/// `data` is interleaved f32 owned by flexaudio, of length `len` (= `frames * channels`).
/// Always free it with `flexaudio_chunk_free` when done (do not use C's free).
#[repr(C)]
pub struct FlexChunk {
    /// Pointer to interleaved f32 samples. Free with `flexaudio_chunk_free`.
    pub data: *mut f32,
    /// Element count of `data` (= `frames * channels`).
    pub len: usize,
    /// Number of frames in the chunk.
    pub frames: u32,
    /// Monotonic presentation timestamp (ns) of the first sample.
    pub pts_ns: i64,
    /// Monotonically increasing sequence number assigned by the stream layer.
    pub seq: u64,
    /// Chunk status flags (ChunkFlags bits).
    pub flags: u32,
    /// Number of chunks dropped before this chunk arrived.
    pub dropped_before: u32,
    /// Maximum absolute value over all samples (linear amplitude).
    pub peak: f32,
    /// Root mean square over all samples (linear).
    pub rms: f32,
    /// Array of events the VAD finalized in this chunk. NULL (`vad_events_len = 0`) when VAD
    /// is disabled or there are no events. When non-NULL, `flexaudio_chunk_free` frees it
    /// together with `data`.
    pub vad_events: *mut FlexVadEvent,
    /// Element count of `vad_events`. 0 when VAD is disabled or there are no events.
    pub vad_events_len: usize,
}

/// Kind of stream event (corresponds to [`flexaudio::Event`]).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlexEventKind {
    /// Chunks were dropped because the chunk ring was full (the count is `FlexEvent::count`).
    ChunkDropped = 0,
    /// Data stopped arriving and the stream stalled.
    Stalled = 1,
    /// Data resumed arriving after a stall.
    Recovered = 2,
    /// A required permission was denied.
    PermissionDenied = 3,
    /// The capture device was lost.
    DeviceLost = 4,
    /// Any other backend error (get the message with `flexaudio_last_error`).
    Error = 5,
    /// An event that matches none of the known ones (in preparation for future variants).
    Unknown = 6,
}

/// One retrieved event. Filled by `flexaudio_poll_event`.
///
/// For `Error`, the message goes into `flexaudio_last_error`.
#[repr(C)]
pub struct FlexEvent {
    /// Event kind.
    pub kind: FlexEventKind,
    /// Drop count for `ChunkDropped`. 0 otherwise.
    pub count: i64,
}

/// Information about one enumerated device (corresponds to [`flexaudio::DeviceInfo`]).
///
/// `id` / `name` are UTF-8 NUL-terminated strings owned by flexaudio. Free them together with
/// the array via `flexaudio_devices_free` (do not use C's free).
#[repr(C)]
pub struct FlexDeviceInfo {
    /// Stable ID (freed by `flexaudio_devices_free`).
    pub id: *mut c_char,
    /// Human-readable display name (freed by `flexaudio_devices_free`).
    pub name: *mut c_char,
    /// Source kind to use when capturing this device.
    pub source_kind: FlexSourceKind,
    /// Native (default) sample rate (Hz).
    pub sample_rate: u32,
    /// Native (default) channel count.
    pub channels: u16,
    /// True if this is a loopback (a monitor of the system output).
    pub is_loopback: bool,
    /// True if this is the OS default device.
    pub is_default: bool,
}

/// Whether the process is outputting audio right now (corresponds to
/// [`flexaudio::ProcessInfo::is_output_active`]).
///
/// `Unknown` (Rust's `None`) when the OS does not expose that state.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlexOutputActivity {
    /// The OS does not expose the state / it could not be read.
    Unknown = 0,
    /// Not outputting (Linux = node is not Running / Windows = session is Inactive /
    /// macOS = IsRunningOutput is 0).
    Inactive = 1,
    /// Outputting.
    Active = 2,
}

/// Information about one enumerated process (corresponds to [`flexaudio::ProcessInfo`]).
///
/// Passing `pid` to `FlexConfig::process_id` records that process. The strings are UTF-8
/// NUL-terminated and owned by flexaudio; free them together with the array via
/// `flexaudio_processes_free` (do not use C's free). `executable` / `bundle_id` are NULL when
/// they could not be obtained.
#[repr(C)]
pub struct FlexProcessInfo {
    /// OS process ID (non-zero).
    pub pid: u32,
    /// Display name (always non-empty; freed by `flexaudio_processes_free`).
    pub name: *mut c_char,
    /// Base name of the executable. NULL if it could not be obtained.
    pub executable: *mut c_char,
    /// macOS bundle ID. NULL if it could not be obtained (always NULL outside macOS).
    pub bundle_id: *mut c_char,
    /// Whether it is outputting (`Unknown` if not known).
    pub output_activity: FlexOutputActivity,
}

/// Opaque handle to a recording stream. It contains a [`flexaudio::Stream`] and, when enabled,
/// the addons (denoise / VAD) living alongside it; the C side holds only the pointer. Create it
/// with `flexaudio_open` and free it with `flexaudio_free`.
///
/// The addons are enclosed here as stream state (a thin wrapper). `poll_chunk` passes chunks
/// through denoise → VAD in that order before returning them. `flexaudio_switch_source`
/// replaces only the source and keeps the addons as configured at open time (same treatment
/// as gain).
pub struct FlexStream {
    pub(crate) inner: flexaudio::Stream,
    /// Noise suppressor when enabled (assumes 48k; built at open time). `None` if disabled.
    pub(crate) denoiser: Option<Denoiser>,
    /// VAD when enabled (built at open time). `None` if disabled.
    pub(crate) vad: Option<Vad>,
}
