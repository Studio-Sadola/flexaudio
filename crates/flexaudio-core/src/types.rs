//! Shared types: internal canonical-form constants / [`AudioChunk`] / [`ChunkFlags`] / [`SourceKind`] /
//! [`OutputFormat`] / [`StreamConfig`] / [`Event`] / [`Error`].
//!
//! The internal canonical form is interleaved `f32` / 48000 Hz / stereo, 2 channels / 20 ms = 960
//! frames per chunk.
//!
//! Specify the output format with [`OutputFormat`] (default `{48000, 2}`).
//! Normalizer stage 2 converts from the internal canonical form to that rate and channel count. Since output
//! chunks are time-based at 20 ms, [`AudioChunk::frames`] varies with the rate
//! (48k=960 / 16k=320 / 8k=160). With the default `{48000, 2}`, stage 2 is
//! a pass-through and emits the internal canonical form unchanged.

use bitflags::bitflags;

/// Sample rate (Hz) of the internal canonical form. All streams are normalized to this rate first.
pub const SAMPLE_RATE: u32 = 48_000;

/// Channel count of the internal canonical form. Always stereo (2-channel interleaved).
pub const CHANNELS: u16 = 2;

bitflags! {
    /// State flags associated with an [`AudioChunk`].
    ///
    /// Use `u32` as the representation to keep the bit width stable across FFI.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
    pub struct ChunkFlags: u32 {
        /// A stream discontinuity (drop / gap) occurred immediately before this chunk.
        const DISCONTINUITY = 0b0000_0001;
        /// First chunk after automatic recovery, such as after device loss.
        const RECOVERED = 0b0000_0010;
        /// Generated-silence chunk (silence synthesized to fill a gap, etc.).
        const SILENCE = 0b0000_0100;
    }
}

/// Normalized 20 ms audio chunk.
///
/// `data` is interleaved `f32` in output-channel order, with length
/// `frames * output.channels`. Chunks are time-based at 20 ms, so `frames` varies
/// with the output rate (48k=960 / 16k=320 / 8k=160). With default output `{48000, 2}`
///, `frames == 960` (1920 samples).
#[derive(Debug, Clone, PartialEq)]
pub struct AudioChunk {
    /// Interleaved `f32` samples. Length = `frames * output.channels`.
    pub data: Vec<f32>,
    /// Number of frames in the chunk (one sample per output channel per frame).
    pub frames: usize,
    /// Normalized monotonic presentation timestamp (ns) of the first sample.
    pub pts_ns: i64,
    /// Monotonically increasing sequence number assigned by the stream layer.
    pub seq: u64,
    /// State flags for this chunk.
    pub flags: ChunkFlags,
    /// Number of chunks dropped immediately before this chunk arrived.
    pub dropped_before: u32,
    /// Maximum absolute sample value in the final `data` (output format) for this chunk.
    /// Linear amplitude (usually `0.0..=1.0`).
    pub peak: f32,
    /// Linear root-mean-square value in the final `data` (output format) for this chunk.
    pub rms: f32,
}

/// One 20ms chunk of a secondary output tap.
///
/// A secondary tap is an additional rendering of the same capture in its own
/// format (see [`StreamConfig::secondary_output`]). It always carries `f32`
/// samples; encoding to another sample type (e.g. signed 16-bit) is the
/// binding layer's responsibility and happens downstream. The secondary tap
/// carries its own `pts_ns`/`seq` on the same recording clock as the primary
/// [`AudioChunk`], but the values are independent of the primary tap: consumers
/// pair a primary chunk with a secondary chunk by `pts_ns` (time), never by
/// `seq` (each tap has its own counter). Because the secondary tap runs through
/// its own resampler, its chunks lag the primary by roughly one to three chunks
/// (about 20-60ms).
#[derive(Debug, Clone, PartialEq)]
pub struct SecondaryChunk {
    /// Interleaved `f32` samples. Length = `frames * secondary_output.channels`.
    pub samples: Vec<f32>,
    /// Number of frames in the chunk (one sample per output channel per frame).
    pub frames: usize,
    /// Presentation timestamp (ns) of the first sample, relative to recording start at 0.
    /// Uses the same recording clock as the primary [`AudioChunk`], but has independent values.
    pub pts_ns: i64,
    /// Monotonic sequence number for the secondary tap (separate counter from the primary tap).
    pub seq: u64,
    /// State flags for this chunk.
    pub flags: ChunkFlags,
    /// Number of secondary chunks dropped immediately before this chunk arrived.
    pub dropped_before: u32,
    /// Maximum absolute sample value (linear amplitude) in `samples` (f32 before quantization).
    pub peak: f32,
    /// Linear root-mean-square value in `samples` (f32 before quantization).
    pub rms: f32,
}

/// Information returned for each device by `devices()`.
///
/// Common shape for all OS backends. Combines microphone input ([`SourceKind::Mic`]) and system audio output
/// ([`SourceKind::SystemLoopback`]) in one list.
///
/// Use the most stable key available for `id`; a device's list index can change after reconnect.
/// cpal (microphones on all OSes) has no persistent ID, so use the device name as id. PipeWire
/// (Linux) uses `node.name` as id (`node.description` is used for the display name `name`).
///
/// Enumerating again on the same machine with the same configuration returns the same `id`. Uniqueness across
/// different machines or operating systems is not guaranteed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    /// Stable ID. Key accepted by [`StreamConfig::device_id`] (cpal = device name /
    /// PipeWire = `node.name`).
    pub id: String,
    /// Human-readable display name (PipeWire prefers `node.description`, falling back to `node.name`).
    pub name: String,
    /// Source kind used to capture this device.
    pub source_kind: SourceKind,
    /// Native (default) device sample rate (Hz), or a reasonable default if unknown.
    pub sample_rate: u32,
    /// Native (default) device channel count, or a reasonable default if unknown.
    pub channels: u16,
    /// `true` for loopback (system-output monitor), `false` for a recording device (microphone).
    pub is_loopback: bool,
    /// `true` if this is the OS default device (default input / output sink).
    pub is_default: bool,
}

/// Information returned for each process by `processes()` (candidate for per-process capture).
///
/// Common shape for all OS backends. Lists processes with an audio-output session that can likely be captured
/// through the per-process capture path ([`SourceKind::ProcessLoopback`])
/// (audio output session/stream). Stopped and idle sessions are included. Check whether audio is currently playing with
/// [`is_output_active`](Self::is_output_active):
/// - Linux (PipeWire): Client with a `Stream/Output/Audio` node (PID is the Client's
///   `pipewire.sec.pid`, set by the daemon from socket credentials).
/// - Windows (WASAPI): Process with an audio session on an active render endpoint
///   (Windows build 20348 or later (Windows 11 / Windows Server 2022). Below this, enumeration
///   itself returns [`Error::UnsupportedOsVersion`]).
/// - macOS (Core Audio, 14.4+): Process objects known to Core Audio
///   (including input-only processes).
///
/// Pass [`pid`](Self::pid) to [`StreamConfig::target_pid`] to capture that process.
/// `name` / `executable` / `bundle_id` are for display and may contain values claimed by the app, so
/// do not use them for authorization or identity checks (`pid` is the key).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessInfo {
    /// OS process ID. Key passed to [`StreamConfig::target_pid`]. Always nonzero.
    pub pid: u32,
    /// Human-readable display name (never empty). Chosen in this order: OS-reported name (such as PipeWire `application.name`) →
    /// executable name → bundle ID → `"pid <N>"`.
    pub name: String,
    /// Executable basename (for example, `firefox` or `chrome.exe`). `Some` only when the OS provides it.
    /// Linux tries `/proc/<pid>/exe` and falls back to `/proc/<pid>/comm` if it cannot be read.
    pub executable: Option<String>,
    /// macOS bundle ID (for example, `com.apple.Music`). `Some` only when macOS provides it.
    pub bundle_id: Option<String>,
    /// Whether audio is currently playing. `Some` only when the OS exposes this state
    /// (Linux = node is Running / Windows = session is Active /
    /// macOS = `kAudioProcessPropertyIsRunningOutput`). `None` if unavailable (unknown).
    pub is_output_active: Option<bool>,
}

/// Hotplug event for device attach/detach or default-device changes.
///
/// Separate from capture-stream [`Event`], `DeviceWatcher` (facade layer) delivers these events
/// per device through `poll_event`. Attach/detach events are infrequent, but must not be lost,
/// so the delivery queue is unbounded.
///
/// Mark `#[non_exhaustive]` to allow future variants (external matches must include `_ =>`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DeviceEvent {
    /// A device was added (connected or a new node appeared).
    Added(DeviceInfo),
    /// A device was removed (disconnected or node disappeared).
    /// PipeWire `global_remove` provides only a numeric id, so return only the stable ID (`node.name`).
    Removed {
        /// Stable ID of the removed device (= [`DeviceInfo::id`] = PipeWire `node.name`).
        id: String,
    },
    /// The OS default device changed (default sink / source switched).
    DefaultChanged {
        /// Source kind whose default changed (`Mic` = default source / `SystemLoopback` = default sink).
        kind: SourceKind,
        /// Stable ID of the new default device (= `node.name`).
        id: String,
    },
}

/// Kind of audio source to capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SourceKind {
    /// Microphone input (recording device).
    Mic,
    /// Loopback of all system output (default speaker mix).
    SystemLoopback,
    /// Output loopback for a specific process.
    ProcessLoopback,
    /// Record microphone and system audio mixed into one stream (mic + system mix).
    Mix,
}

/// How to handle the target PID for [`SourceKind::ProcessLoopback`] (process sources only).
///
/// - [`Include`](ProcessMode::Include) (default): Capture only the target `target_pid` (its process
///   tree).
/// - [`Exclude`](ProcessMode::Exclude): Capture all system audio except the target `target_pid` (and its process tree)
///   (`target_pid` is required).
///
/// Process sources use this `mode` and ignore [`StreamConfig::exclude_self`] and
/// [`StreamConfig::exclude_pids`]. System sources use `exclude_self` and
/// `exclude_pids` and ignore `mode`; microphone sources ignore all three.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProcessMode {
    /// Capture only the target `target_pid` (and its process tree; default).
    #[default]
    Include,
    /// Capture all system audio except the target `target_pid` (and its process tree).
    /// `target_pid` is required (otherwise the facade returns [`Error::InvalidArg`]).
    Exclude,
}

/// Output chunk format (sample rate and channel count).
///
/// Normalizer stage 2 converts from the internal canonical form (48 kHz/stereo) to this format.
/// The default is the same as the internal canonical form, `{sample_rate: 48000, channels: 2}`; in that case, stage 2
/// is a pass-through.
///
/// `sample_rate` is the target rate for down/up-sampling (with rubato antialiasing).
/// `channels` is 1 (mono) or 2 (stereo). stereo→mono averages L/R;
/// mono→stereo duplicates the channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputFormat {
    /// Output sample rate (Hz).
    pub sample_rate: u32,
    /// Output channel count (1 = mono / 2 = stereo).
    pub channels: u16,
}

impl OutputFormat {
    /// Supported output-rate range (rejects 0 and extreme values).
    const MIN_RATE: u32 = 4_000;
    const MAX_RATE: u32 = 384_000;

    /// Validate the configuration. `channels` must be 1 or 2; `sample_rate` must be
    /// within `MIN_RATE..=MAX_RATE`, otherwise return [`Error::UnsupportedFormat`].
    pub fn validate(&self) -> Result<()> {
        if self.channels != 1 && self.channels != 2 {
            return Err(Error::UnsupportedFormat(format!(
                "output channels must be 1 or 2, got {}",
                self.channels
            )));
        }
        if self.sample_rate < Self::MIN_RATE || self.sample_rate > Self::MAX_RATE {
            return Err(Error::UnsupportedFormat(format!(
                "output sample_rate {} Hz out of supported range {}..={}",
                self.sample_rate,
                Self::MIN_RATE,
                Self::MAX_RATE
            )));
        }
        Ok(())
    }

    /// Frames per 20 ms chunk at the output rate (48k=960 / 16k=320 / 8k=160).
    pub fn chunk_frames(&self) -> usize {
        (self.sample_rate as usize * 20) / 1000
    }
}

impl Default for OutputFormat {
    fn default() -> Self {
        // Match the internal canonical form so stage 2 is a pass-through.
        Self {
            sample_rate: SAMPLE_RATE,
            channels: CHANNELS,
        }
    }
}

/// Configuration for opening one stream.
///
/// [`Default`] returns `chunk_ms = 20`, `ring_capacity_chunks = 50`, `mode = Include`,
/// `exclude_self = false`, `exclude_pids = []`, `kind = Mic`, `output = {48000, 2}`, `gain = 1.0`,
/// `mix_mic_device_id = None`, `mix_system_device_id = None`, `mix_mic_gain = 1.0`,
/// and `mix_system_gain = 1.0`.
///
/// Process-source PID handling is controlled by [`mode`](Self::mode). System-source
/// exclusion is controlled by [`exclude_pids`](Self::exclude_pids) and
/// [`exclude_self`](Self::exclude_self), with effective set
/// `exclude_pids ∪ {this process if exclude_self}`. The system side of [`SourceKind::Mix`]
/// uses the same exclusion set; microphone and process sources ignore both exclusion fields.
/// The four `mix_*` fields apply only to [`SourceKind::Mix`] and are ignored for other sources.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamConfig {
    /// Device to select. Applies to both mic (input device) and system (output endpoint).
    /// `None` selects the default (mic = default input / system = default output); `Some(id)` selects the device returned by `devices()`
    /// with the matching stable ID. If no device matches, `start` returns
    /// [`Error::DeviceNotFound`]. Ignored for [`SourceKind::ProcessLoopback`]
    /// (the target is selected by `target_pid`). Also ignored for [`SourceKind::Mix`]
    /// (select each side with `mix_mic_device_id` / `mix_system_device_id` instead).
    /// Exclusion on a system capture is a known limitation: while exclusion is active,
    /// the system capture does not honor `device_id`.
    pub device_id: Option<String>,
    /// Source kind.
    pub kind: SourceKind,
    /// Chunk duration (ms). Fixed at 20.
    pub chunk_ms: u32,
    /// Chunk-ring capacity (number of chunks). Drops the oldest when full.
    pub ring_capacity_chunks: usize,
    /// Target PID for [`SourceKind::ProcessLoopback`].
    pub target_pid: Option<u32>,
    /// Whether to include or exclude the target PID (process sources only). [`ProcessMode::Include`]
    /// is the default. Ignored except for [`SourceKind::ProcessLoopback`]. `Exclude` requires
    /// `target_pid` (otherwise the facade returns [`Error::InvalidArg`]).
    pub mode: ProcessMode,
    /// Exclude this process's playback from system audio to prevent feedback
    /// (system source only). When `true`, `std::process::id()` is added to the
    /// exclusion set. This applies to the system-side child capture of
    /// [`SourceKind::Mix`] and is ignored by microphone and process sources.
    pub exclude_self: bool,
    /// Additional process IDs whose playback is excluded from a system capture.
    /// Applies only to a system source and the system side of [`SourceKind::Mix`];
    /// microphone and process sources ignore it. The effective exclusion set is
    /// `exclude_pids ∪ {this process if exclude_self}`. Every PID must be a positive
    /// integer; zero is rejected.
    ///
    /// On Linux, every listed PID is excluded. Pulse-proxied streams (including
    /// `pipewire-pulse` clients) are matched by `application.process.id`; while
    /// exclusion is active, such a stream is never captured until its application
    /// PID is known.
    ///
    /// On macOS, each listed PID with a Core Audio process object at capture start
    /// is excluded. This is a start-time snapshot: a PID with no audio object at
    /// start is not excluded. If lookup of a requested PID fails, start fails
    /// unless that process is confirmed to have exited. PIDs must be in
    /// `1..=i32::MAX`.
    ///
    /// On Windows, WASAPI excludes one process tree per capture. The root is this
    /// process when `exclude_self` is true, otherwise the first listed PID. Any
    /// other listed PID causes start to fail with [`Error::InvalidArg`]; excluding
    /// unrelated trees is unsupported. A common ancestor may be listed when
    /// excluding its entire tree is acceptable.
    ///
    /// While exclusion is active, the system capture does not honor
    /// [`device_id`](Self::device_id) (known limitation).
    pub exclude_pids: Vec<u32>,
    /// Output chunk format. Default `{48000, 2}` (pass-through).
    pub output: OutputFormat,
    /// Secondary output-tap format (omitted = no secondary tap).
    ///
    /// With `Some(fmt)`, enable a secondary tap that converts the same capture to a format
    /// different from primary [`output`](Self::output) and returns it as [`SecondaryChunk`]
    /// through `poll_secondary`. Generate the internal canonical form (48 kHz/stereo) once, then
    /// convert primary and secondary independently in stage 2. They are not samples from exactly the same
    /// interval, so align them by `pts_ns`. The secondary tap is always `f32` (no sample encoding
    ///). With `None`, no secondary tap is created and `poll_secondary` always returns `None`.
    ///
    /// Cannot be changed with `Stream::switch_source` (fixed when opened).
    pub secondary_output: Option<OutputFormat>,
    /// Input gain at start (linear multiplier). 1.0 = unchanged, 2.0 ≈ +6 dB, 0.0 = silence. Default 1.0.
    /// Must be finite and >= 0.0 (otherwise open returns [`Error::InvalidArg`]).
    /// Change it at runtime with `Stream::set_gain`.
    pub gain: f32,
    /// Input device for the mic side of [`SourceKind::Mix`]. `None` selects the default input.
    /// id is the stable mic ID returned by `devices()`. Ignored except for `Mix`.
    pub mix_mic_device_id: Option<String>,
    /// Output endpoint for the system side of [`SourceKind::Mix`]. `None` selects the default output.
    /// id is the stable system ID returned by `devices()`. Ignored except for `Mix`.
    pub mix_system_device_id: Option<String>,
    /// Pre-mix linear gain for the mic side of [`SourceKind::Mix`]. Default 1.0. Ignored except for
    /// `Mix`. Existing [`gain`](Self::gain) is global and applied after mixing
    /// (final value ≈ clamp(clamp(mic×mix_mic_gain + sys×mix_system_gain) × gain)).
    pub mix_mic_gain: f32,
    /// Pre-mix linear gain for the system side of [`SourceKind::Mix`]. Default 1.0. Ignored except for
    /// `Mix`. See [`gain`](Self::gain) for the global post-mix multiplier.
    pub mix_system_gain: f32,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            device_id: None,
            kind: SourceKind::Mic,
            chunk_ms: 20,
            ring_capacity_chunks: 50,
            target_pid: None,
            mode: ProcessMode::Include,
            exclude_self: false,
            exclude_pids: Vec::new(),
            output: OutputFormat::default(),
            secondary_output: None,
            gain: 1.0,
            mix_mic_device_id: None,
            mix_system_device_id: None,
            mix_mic_gain: 1.0,
            mix_system_gain: 1.0,
        }
    }
}

/// Asynchronous event delivered to the consumer while the stream runs.
///
/// Mark `#[non_exhaustive]` to allow future variants (external matches must include `_ =>`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Event {
    /// `count` chunks were dropped because the chunk ring was full.
    ChunkDropped {
        /// Total (or incremental) number dropped since the previous notification.
        count: u64,
    },
    /// Data stopped arriving; the stream was deemed stalled.
    StreamStalled,
    /// Data resumed after the stall.
    StreamRecovered,
    /// A required permission was denied.
    PermissionDenied,
    /// The capture device was lost (for example, disconnected).
    DeviceLost,
    /// Other backend error (with description).
    Error(String),
}

/// Errors that can occur during flexaudio-core operations.
///
/// Mark `#[non_exhaustive]` to allow future variants (external matches must include `_ =>`).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Invalid argument.
    #[error("invalid argument: {0}")]
    InvalidArg(String),
    /// Operation is not allowed in the current state.
    #[error("invalid state: {0}")]
    InvalidState(String),
    /// Specified device was not found.
    #[error("device not found")]
    DeviceNotFound,
    /// Permission was denied.
    #[error("permission denied")]
    PermissionDenied,
    /// The running OS version does not meet this feature's requirements.
    #[error("unsupported OS version")]
    UnsupportedOsVersion,
    /// The device was lost while running.
    #[error("device lost")]
    DeviceLost,
    /// Backend-specific error (with description).
    #[error("backend error: {0}")]
    Backend(String),
    /// Requested output format (rate / channels) is unsupported.
    #[error("unsupported output format: {0}")]
    UnsupportedFormat(String),
    /// Operation is unsupported in this environment.
    #[error("unsupported")]
    Unsupported,
}

/// Result type used throughout flexaudio-core.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_stream_config_matches_contract() {
        let c = StreamConfig::default();
        assert_eq!(c.chunk_ms, 20);
        assert_eq!(c.ring_capacity_chunks, 50);
        assert_eq!(c.mode, ProcessMode::Include);
        assert!(!c.exclude_self);
        assert!(c.exclude_pids.is_empty(), "no pids excluded by default");
        assert_eq!(c.kind, SourceKind::Mic);
        assert_eq!(c.device_id, None);
        assert_eq!(c.target_pid, None);
        assert_eq!(c.gain, 1.0);
        // Defaults for Mix-only fields (no device selected; pre-mix gain 1.0).
        assert_eq!(c.mix_mic_device_id, None);
        assert_eq!(c.mix_system_device_id, None);
        assert_eq!(c.mix_mic_gain, 1.0);
        assert_eq!(c.mix_system_gain, 1.0);
        // Default output matches the internal canonical form (stage 2 pass-through).
        assert_eq!(c.output.sample_rate, SAMPLE_RATE);
        assert_eq!(c.output.channels, CHANNELS);
        assert_eq!(c.output, OutputFormat::default());
        // No secondary tap by default.
        assert_eq!(c.secondary_output, None);
    }

    #[test]
    fn output_format_chunk_frames_are_time_based() {
        assert_eq!(
            OutputFormat {
                sample_rate: 48_000,
                channels: 2
            }
            .chunk_frames(),
            960
        );
        assert_eq!(
            OutputFormat {
                sample_rate: 16_000,
                channels: 1
            }
            .chunk_frames(),
            320
        );
        assert_eq!(
            OutputFormat {
                sample_rate: 8_000,
                channels: 2
            }
            .chunk_frames(),
            160
        );
    }

    #[test]
    fn output_format_validation_rejects_bad_configs() {
        // ch=0 / ch=3 are unsupported.
        assert!(OutputFormat {
            sample_rate: 48_000,
            channels: 0
        }
        .validate()
        .is_err());
        assert!(OutputFormat {
            sample_rate: 48_000,
            channels: 3
        }
        .validate()
        .is_err());
        // Extreme rates are unsupported.
        assert!(OutputFormat {
            sample_rate: 100,
            channels: 1
        }
        .validate()
        .is_err());
        assert!(OutputFormat {
            sample_rate: 1_000_000,
            channels: 2
        }
        .validate()
        .is_err());
        // Valid configuration is OK.
        assert!(OutputFormat {
            sample_rate: 16_000,
            channels: 1
        }
        .validate()
        .is_ok());
        assert!(OutputFormat::default().validate().is_ok());
    }

    #[test]
    fn device_info_builds_and_clones() {
        let mic = DeviceInfo {
            id: "alsa_input.pci-0000_00_1f.3".into(),
            name: "Built-in Microphone".into(),
            source_kind: SourceKind::Mic,
            sample_rate: 48_000,
            channels: 2,
            is_loopback: false,
            is_default: true,
        };
        // Clone / PartialEq work (used to compare and duplicate enumeration results).
        assert_eq!(mic, mic.clone());
        assert!(!mic.is_loopback);
        assert!(mic.is_default);
        assert_eq!(mic.source_kind, SourceKind::Mic);

        let sys = DeviceInfo {
            source_kind: SourceKind::SystemLoopback,
            is_loopback: true,
            is_default: false,
            ..mic.clone()
        };
        assert!(sys.is_loopback);
        assert_ne!(mic, sys);
    }

    #[test]
    fn process_mode_default_is_include() {
        // Default is Include (capture only the target PID). Exclude must be explicitly selected.
        assert_eq!(ProcessMode::default(), ProcessMode::Include);
        assert_ne!(ProcessMode::Include, ProcessMode::Exclude);
    }

    #[test]
    fn chunk_flags_are_distinct_bits() {
        let all = ChunkFlags::DISCONTINUITY | ChunkFlags::RECOVERED | ChunkFlags::SILENCE;
        assert_eq!(all.bits(), 0b111);
        assert!(all.contains(ChunkFlags::SILENCE));
        assert_eq!(ChunkFlags::default(), ChunkFlags::empty());
    }
}
