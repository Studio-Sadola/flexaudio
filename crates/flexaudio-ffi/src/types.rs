//! `#[repr(C)]` types and opaque handles exposed through the C ABI.
//!
//! cbindgen copies these directly to structs / enums in `flexaudio.h`. Keep the layout consistent
//! with the C side; do not change field types or order.

use std::os::raw::c_char;

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
    /// Whether to apply noise suppression (RNNoise) to the stream. Enabled when `true`.
    /// Runs once in the library on canonical 48 kHz stereo capture before downmix and gain,
    /// feeding output conversion and attached VAD before polling. v1/v2 require a 48000 Hz
    /// primary output rate; v3 permits other primary output rates.
    /// Boolean integer: only 0 and 1 are valid.
    pub denoise: u8,
    /// Silence diagnosis probe: 0 = default (enabled), 1 = enabled, 2 = disabled.
    pub silence_probe: u8,
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
    /// Deprecated naming since 0.8; use flexaudio_chunk_arrival_ns_v4. This frozen
    /// field remains permanently in the C layout (other bindings retain it through 0.9).
    /// Arrival-time label assigned at facade ingest and clamped non-decreasing. Primary/secondary
    /// ranges may overlap, repeat labels or leave gaps by callback jitter; this is not a media clock.
    /// flexaudio_chunk_frame_index is authoritative (gaps are loss). Exact VAD mapping uses
    /// epoch_start.pts_ns + t_ms*1000000 or epoch_start.capture_sample + t_ms*48.
    pub pts_ns: i64,
    /// Monotonically increasing sequence number assigned by the stream layer.
    pub seq: u64,
    /// Chunk state flags (ChunkFlags bits).
    pub flags: u32,
    /// Number of chunks dropped before this chunk arrived.
    pub dropped_before: u32,
    /// Maximum absolute delivered sample value after denoise and gain (linear amplitude).
    pub peak: f32,
    /// Root-mean-square of delivered samples after denoise and gain (linear).
    pub rms: f32,
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
    /// Custom-backend/compatibility notice: recording consent remains undecided.
    /// Built-in sources on every OS emit neither pending nor granted notices.
    /// Advisory only; capture continues.
    /// Retrieve guidance with flexaudio_last_error; capture may stay silent until granted.
    PermissionPending = 8,
}

/// One captured event, populated by `flexaudio_poll_event`.
///
/// For Error, PermissionDenied, SilenceWhileSourceActive, and PermissionPending,
/// the message is stored in flexaudio_last_error. PermissionDenied retains kind 3;
/// PermissionPending has kind 8 and does not stop capture. This arm remains for
/// custom Rust backends and compatibility; built-in sources on every OS emit
/// neither pending nor granted notices.
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
/// `flexaudio_devices_free` (do not use C `free`). Unknown names are empty strings;
/// Windows/macOS use selectable native IDs when names are unreadable. IDs may change
/// between queries: a native fallback may become the recovered name. A name-based ID
/// cannot be opened while its name is unreadable; the native ID stays openable in both
/// name states. macOS omits unreadable output direction from discovery; the typed
/// direction cause is returned for the requested device. Explicit-output scans skip
/// vanished or unrelated unreadable devices. If the requested device's direction query
/// fails and neither its name nor UID is readable, open returns DeviceNotFound because
/// it cannot be matched. Unreadable default queries leave is_default=false.
#[repr(C)]
pub struct FlexDeviceInfo {
    /// Selectable ID (released by `flexaudio_devices_free`).
    pub id: *mut c_char,
    /// Human-readable display name (released by `flexaudio_devices_free`).
    pub name: *mut c_char,
    /// Source kind used to capture this device.
    pub source_kind: i32,
    /// Native rate (Hz), or 0 if unavailable; use flexaudio_device_format_available_v2.
    pub sample_rate: u32,
    /// Native channel count, or 0 if unavailable (both rate/channels are 0).
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
/// The facade owns denoise. VAD consumes canonical capture before
/// output conversion. `flexaudio_switch_source` replaces only the source;
/// addons retain their configuration from open (same as gain).
pub struct FlexStream {
    pub(crate) events: std::sync::Mutex<crate::lifecycle_v4::EventQueues>,
    pub(crate) secondary: Option<crate::secondary::SecondaryState>,
    pub(crate) inner: flexaudio::Stream,
    pub(crate) shutdown: Option<flexaudio::ShutdownReport>,
    pub(crate) shutdown_event_index: usize,
    pub(crate) vad_tap: Option<flexaudio_vad::VadTap>,
    pub(crate) vad_events: Vec<flexaudio_vad::AttachedVadEvent>,
    pub(crate) vad_origin: (u64, i64),
    pub(crate) vad_error: Option<flexaudio_vad::VadTapError>,
    pub(crate) vad_cleanup: Vec<flexaudio::Error>,
    pub(crate) ready_chunks: std::collections::VecDeque<crate::vad_integration::FlexChunkV2>,
}

impl Drop for FlexStream {
    fn drop(&mut self) {
        for mut chunk in self.ready_chunks.drain(..) {
            unsafe {
                crate::vad_integration::flexaudio_chunk_free_v2(&mut chunk);
            }
        }
    }
}

#[cfg(test)]
#[path = "../tests/ffi/layout.rs"]
mod layout_tests;
