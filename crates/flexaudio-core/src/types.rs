//! Shared types: internal canonical-form constants / [`AudioChunk`] / [`ChunkFlags`] /
//! [`SourceKind`] / [`OutputFormat`] / [`StreamConfig`] / [`Event`] / [`Error`].
//!
//! The internal canonical form is interleaved `f32` / 48000 Hz / stereo 2ch / 20ms = 960
//! frames per chunk.
//!
//! The output format is specified with [`OutputFormat`] (default `{48000, 2}`). The
//! Normalizer's second stage re-converts from the internal canonical form to that rate/channel
//! count. Output chunks are 20ms in time, so [`AudioChunk::frames`] varies with the rate
//! (48k=960 / 16k=320 / 8k=160). With the default `{48000, 2}` the second stage is a
//! passthrough and the internal canonical form is output as-is.

use bitflags::bitflags;

/// Sample rate (Hz) of the internal canonical form. Every stream is first normalized to this
/// rate.
pub const SAMPLE_RATE: u32 = 48_000;

/// Channel count of the internal canonical form. Always stereo (2ch interleaved).
pub const CHANNELS: u16 = 2;

bitflags! {
    /// State flags attached to one [`AudioChunk`].
    ///
    /// Represented as a `u32` so it crosses FFI with a stable bit width.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
    pub struct ChunkFlags: u32 {
        /// There was a stream discontinuity (drop / gap) right before this chunk.
        const DISCONTINUITY = 0b0000_0001;
        /// The first chunk after automatic recovery from e.g. device loss.
        const RECOVERED = 0b0000_0010;
        /// Generated silence chunk (silence synthesized e.g. to fill a gap).
        const SILENCE = 0b0000_0100;
    }
}

/// Normalized 20ms audio chunk.
///
/// `data` is interleaved `f32` (in output channel order) with length
/// `frames * output.channels`. Chunks are 20ms in time, so `frames` varies with the output
/// rate (48k=960 / 16k=320 / 8k=160). With the default output `{48000, 2}`,
/// `frames == 960` (1920 samples).
#[derive(Debug, Clone, PartialEq)]
pub struct AudioChunk {
    /// Interleaved `f32` samples. Length = `frames * output.channels`.
    pub data: Vec<f32>,
    /// Number of frames in the chunk (1 frame = one sample for every output channel).
    pub frames: usize,
    /// Normalized monotonic presentation timestamp (ns) of the first sample.
    pub pts_ns: i64,
    /// Sequence number assigned monotonically increasing by the stream layer.
    pub seq: u64,
    /// State flags of this chunk.
    pub flags: ChunkFlags,
    /// Number of chunks dropped (immediately) before this chunk arrived.
    pub dropped_before: u32,
    /// Maximum absolute value over all samples of this chunk's final `data` (in the output
    /// format). Linear amplitude (normally `0.0..=1.0`).
    pub peak: f32,
    /// Root mean square (linear) of this chunk's final `data` (in the output format).
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
    /// Number of frames in the chunk (1 frame = one sample for every output channel).
    pub frames: usize,
    /// Presentation timestamp (ns) of the first sample, with 0 at recording start.
    /// It is on the same recording clock as the primary [`AudioChunk`], but its value is
    /// independent of the primary.
    pub pts_ns: i64,
    /// Monotonic sequence number specific to the secondary tap (a counter separate from the
    /// primary tap's).
    pub seq: u64,
    /// State flags of this chunk.
    pub flags: ChunkFlags,
    /// Number of secondary chunks dropped (immediately) before this chunk arrived.
    pub dropped_before: u32,
    /// Maximum absolute value over all samples in `samples` (pre-quantization f32; linear
    /// amplitude).
    pub peak: f32,
    /// Root mean square (linear) of `samples` (pre-quantization f32).
    pub rms: f32,
}

/// Information `devices()` returns for each device.
///
/// A shape common to all OS backends. Microphone inputs ([`SourceKind::Mic`]) and system audio
/// outputs ([`SourceKind::SystemLoopback`]) are returned together in one list.
///
/// `id` uses a key that is as stable as can be obtained, so that the index does not change on
/// reconnect. cpal (microphone, all OSes) has no persistent ID, so the device name is used as
/// the id; PipeWire (Linux) uses `node.name` as the id (and `node.description` as the display
/// name `name`).
///
/// Re-enumerating on the same machine with the same configuration returns the same `id`.
/// Uniqueness across machines or OSes is not guaranteed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    /// Stable ID. The key that can be passed to [`StreamConfig::device_id`] (cpal=device name /
    /// PipeWire=`node.name`).
    pub id: String,
    /// Human-readable display name (PipeWire prefers `node.description`, falling back to
    /// `node.name`).
    pub name: String,
    /// The source kind used when capturing this device.
    pub source_kind: SourceKind,
    /// The device's native (default) sample rate (Hz). A reasonable default when unknown.
    pub sample_rate: u32,
    /// The device's native (default) channel count. A reasonable default when unknown.
    pub channels: u16,
    /// `true` for a loopback (a monitor of a system output), `false` for a recording device
    /// (microphone).
    pub is_loopback: bool,
    /// `true` if this is the OS default device (default input / default output sink).
    pub is_default: bool,
}

/// Information `processes()` returns for each process (candidate targets for per-process
/// capture).
///
/// A shape common to all OS backends. What is enumerated is "processes that have an audio
/// output session (stream) that can likely be recorded right now through that per-process
/// capture path ([`SourceKind::ProcessLoopback`])". Stopped and Idle ones are also listed.
/// Whether it is sounding right now is given by [`is_output_active`](Self::is_output_active):
/// - Linux (PipeWire): Clients that own a `Stream/Output/Audio` node (the PID is the Client's
///   `pipewire.sec.pid`, i.e. the value the daemon assigns from the socket credentials).
/// - Windows (WASAPI): processes that have an audio session on an active render endpoint
///   (Windows build 20348 or later (Windows 11 / Windows Server 2022). On older builds the
///   enumeration itself returns [`Error::UnsupportedOsVersion`]).
/// - macOS (Core Audio, 14.4+): process objects known to Core Audio (including input-only
///   processes).
///
/// Passing [`pid`](Self::pid) to [`StreamConfig::target_pid`] records that process.
/// `name` / `executable` / `bundle_id` are for display and include values the app reports about
/// itself, so do not use them for authorization or identity checks (the key is `pid`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessInfo {
    /// OS process ID. The key passed to [`StreamConfig::target_pid`]. Always non-zero.
    pub pid: u32,
    /// Human-readable display name (always non-empty). Decided in the order: the name the OS
    /// reports (PipeWire's `application.name`, etc.) → executable name → bundle ID →
    /// `"pid <N>"`.
    pub name: String,
    /// Base name of the executable (e.g. `firefox` / `chrome.exe`). `Some` only when it could
    /// be obtained. Linux tries `/proc/<pid>/exe` and falls back to `/proc/<pid>/comm` if that
    /// cannot be read.
    pub executable: Option<String>,
    /// macOS bundle ID (e.g. `com.apple.Music`). `Some` only when obtained on macOS.
    pub bundle_id: Option<String>,
    /// Whether it is outputting audio right now. `Some` only when the OS exposes that state
    /// (Linux=the node is Running / Windows=the session is Active /
    /// macOS=`kAudioProcessPropertyIsRunningOutput`). `None` (unknown) when it cannot be
    /// obtained.
    pub is_output_active: Option<bool>,
}

/// Hotplug event describing device hotplug and default changes.
///
/// A separate channel from the per-capture-stream [`Event`]; `DeviceWatcher` (facade layer)
/// delivers these as per-device occurrences. Retrieve them with `poll_event`. Hotplug is
/// infrequent but must not be missed, so the delivery queue is unbounded.
///
/// `#[non_exhaustive]` so variants can be added in the future (an external match needs
/// `_ =>`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DeviceEvent {
    /// A device was added (connected, or a new node appeared).
    Added(DeviceInfo),
    /// A device was removed (disconnected, or the node disappeared).
    /// PipeWire's `global_remove` only passes a numeric id, so only the stable ID
    /// (`node.name`) is returned.
    Removed {
        /// Stable ID of the removed device (= [`DeviceInfo::id`] = PipeWire's `node.name`).
        id: String,
    },
    /// The OS default device changed (default sink / source switched).
    DefaultChanged {
        /// Source kind whose default switched (`Mic` = default source / `SystemLoopback` =
        /// default sink).
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
    /// Loopback of the whole system output (the default speaker mix).
    SystemLoopback,
    /// Output loopback of a specific process.
    ProcessLoopback,
    /// Records the microphone and system audio combined into one stream (mic + system mix).
    Mix,
}

/// How [`SourceKind::ProcessLoopback`] treats the target PID (process source only).
///
/// - [`Include`](ProcessMode::Include) (default): records only the target `target_pid` (its
///   process tree).
/// - [`Exclude`](ProcessMode::Exclude): records all system audio except the target
///   `target_pid` (its process tree) (`target_pid` is required).
///
/// The process source looks only at this `mode` and ignores [`StreamConfig::exclude_self`];
/// the system source looks only at `exclude_self` and ignores `mode` (both are irrelevant for
/// mic).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProcessMode {
    /// Records only the target `target_pid` (its process tree) (default).
    #[default]
    Include,
    /// Records all system audio except the target `target_pid` (its process tree).
    /// `target_pid` is required (without it the facade returns [`Error::InvalidArg`]).
    Exclude,
}

/// Format of output chunks (sample rate and channel count).
///
/// The Normalizer's second stage re-converts from the internal canonical form 48k/stereo to
/// this format. The default is `{sample_rate: 48000, channels: 2}`, identical to the internal
/// canonical form, in which case the second stage is a passthrough.
///
/// `sample_rate` is the down/upsampling target (via rubato, including anti-aliasing).
/// `channels` is 1 (mono) or 2 (stereo); stereo→mono averages L/R, and mono→stereo
/// duplicates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputFormat {
    /// Output sample rate (Hz).
    pub sample_rate: u32,
    /// Output channel count (1 = mono / 2 = stereo).
    pub channels: u16,
}

impl OutputFormat {
    /// Lower/upper bounds of the supported output rate (rejects 0 and extreme values).
    const MIN_RATE: u32 = 4_000;
    const MAX_RATE: u32 = 384_000;

    /// Validates the configuration. `channels` must be 1 or 2, and `sample_rate` must be in
    /// `MIN_RATE..=MAX_RATE`. Otherwise returns [`Error::UnsupportedFormat`].
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

    /// Number of frames in a 20ms chunk at the output rate (48k=960 / 16k=320 / 8k=160).
    pub fn chunk_frames(&self) -> usize {
        (self.sample_rate as usize * 20) / 1000
    }
}

impl Default for OutputFormat {
    fn default() -> Self {
        // Identical to the internal canonical form, making the second stage a passthrough.
        Self {
            sample_rate: SAMPLE_RATE,
            channels: CHANNELS,
        }
    }
}

/// Configuration for opening one stream.
///
/// [`Default`] returns `chunk_ms = 20`, `ring_capacity_chunks = 50`, `mode = Include`,
/// `exclude_self = false`, `kind = Mic`, `output = {48000, 2}`, `gain = 1.0`,
/// `mix_mic_device_id = None`, `mix_system_device_id = None`, `mix_mic_gain = 1.0`,
/// `mix_system_gain = 1.0`.
///
/// How the process source treats the target PID is decided only by [`mode`](Self::mode), and
/// the system source's own-host exclusion only by [`exclude_self`](Self::exclude_self). The
/// process source ignores `exclude_self`, and the system source ignores `mode` (both are
/// irrelevant for mic). The four `mix_*` fields are for [`SourceKind::Mix`] only and are
/// ignored by every other source.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamConfig {
    /// The device to select. Applies to both mic (input device) and system (output endpoint).
    /// `None` means the default (mic=default input / system=default output); `Some(id)` means
    /// the device matching the stable ID returned by `devices()`. If nothing matches, `start`
    /// returns [`Error::DeviceNotFound`]. Ignored for [`SourceKind::ProcessLoopback`] (the
    /// target is decided by `target_pid`). Also ignored for [`SourceKind::Mix`] (each side is
    /// selected with `mix_mic_device_id` / `mix_system_device_id` instead).
    pub device_id: Option<String>,
    /// Source kind.
    pub kind: SourceKind,
    /// Chunk length (milliseconds). Fixed at 20.
    pub chunk_ms: u32,
    /// Capacity of the chunk ring (in chunks). DROP_OLDEST when full.
    pub ring_capacity_chunks: usize,
    /// Target PID for [`SourceKind::ProcessLoopback`].
    pub target_pid: Option<u32>,
    /// Whether to include or exclude the target PID (process source only).
    /// [`ProcessMode::Include`] is the default. Ignored for anything other than
    /// [`SourceKind::ProcessLoopback`]. `Exclude` requires `target_pid` (without it the facade
    /// returns [`Error::InvalidArg`]).
    pub mode: ProcessMode,
    /// Whether to exclude the own host's (own process's) playback from system audio (system
    /// source only; prevents feedback loops). `true` excludes the self PID
    /// (`std::process::id()`). For [`SourceKind::Mix`] it applies to the system-side child
    /// capture. Ignored by every other source.
    pub exclude_self: bool,
    /// Format of output chunks. Default `{48000, 2}` (passthrough).
    pub output: OutputFormat,
    /// Format of the secondary output tap (omitted = no secondary tap).
    ///
    /// Specifying `Some(fmt)` enables a secondary tap that re-converts the same capture to a
    /// format different from the primary [`output`](Self::output), retrievable as
    /// [`SecondaryChunk`] from `poll_secondary`. The internal canonical form (48k/stereo) is
    /// produced only once, and the primary and secondary are each re-converted by their own
    /// independent second stage, so the two are not "samples of the identical span"; match them
    /// by time via `pts_ns`. The secondary tap is always `f32` (it has no sample encoding). With
    /// `None`, no secondary tap is created and `poll_secondary` always returns `None`.
    ///
    /// It cannot be changed by `Stream::switch_source` (fixed at open time).
    pub secondary_output: Option<OutputFormat>,
    /// Input gain at start (linear multiplier). 1.0=unchanged, 2.0=about +6dB, 0.0=silence.
    /// Default 1.0. Must be finite and at least 0.0 (otherwise open returns
    /// [`Error::InvalidArg`]). Change it at runtime with `Stream::set_gain`.
    pub gain: f32,
    /// Input device selected for the mic side of [`SourceKind::Mix`]. `None` means the default
    /// input. The id is a mic stable ID returned by `devices()`. Ignored for anything other than
    /// `Mix`.
    pub mix_mic_device_id: Option<String>,
    /// Output endpoint selected for the system side of [`SourceKind::Mix`]. `None` means the
    /// default output. The id is a system stable ID returned by `devices()`. Ignored for
    /// anything other than `Mix`.
    pub mix_system_device_id: Option<String>,
    /// Pre-mix multiplier (linear) for the mic side of [`SourceKind::Mix`]. Default 1.0.
    /// Ignored for anything other than `Mix`. The existing [`gain`](Self::gain) is global and is
    /// applied after mixing
    /// (final value ≈ clamp(clamp(mic×mix_mic_gain + sys×mix_system_gain) × gain)).
    pub mix_mic_gain: f32,
    /// Pre-mix multiplier (linear) for the system side of [`SourceKind::Mix`]. Default 1.0.
    /// Ignored for anything other than `Mix`. See [`gain`](Self::gain) for the global
    /// post-mix multiplier.
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

/// Asynchronous events delivered to the consumer while a stream is running.
///
/// `#[non_exhaustive]` so variants can be added in the future (an external match needs
/// `_ =>`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Event {
    /// `count` chunks were dropped because the chunk ring was full.
    ChunkDropped {
        /// Cumulative (or incremental) count dropped since the most recent notification.
        count: u64,
    },
    /// Data arrival stopped and the stream was judged to have stalled.
    StreamStalled,
    /// Data arrival resumed after a stall.
    StreamRecovered,
    /// A required permission was denied.
    PermissionDenied,
    /// The capture device was lost (e.g. disconnected).
    DeviceLost,
    /// Any other backend error (with a description).
    Error(String),
}

/// Errors that flexaudio-core operations can produce.
///
/// `#[non_exhaustive]` so variants can be added in the future (an external match needs
/// `_ =>`).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Invalid argument.
    #[error("invalid argument: {0}")]
    InvalidArg(String),
    /// Operation not possible in the current state.
    #[error("invalid state: {0}")]
    InvalidState(String),
    /// The specified device was not found.
    #[error("device not found")]
    DeviceNotFound,
    /// Permission was denied.
    #[error("permission denied")]
    PermissionDenied,
    /// The running OS version does not meet the requirement of the feature.
    #[error("unsupported OS version")]
    UnsupportedOsVersion,
    /// The device was lost while running.
    #[error("device lost")]
    DeviceLost,
    /// Backend-specific error (with a description).
    #[error("backend error: {0}")]
    Backend(String),
    /// The requested output format (rate/channels) is not supported.
    #[error("unsupported output format: {0}")]
    UnsupportedFormat(String),
    /// Operation not supported in this environment.
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
        assert_eq!(c.kind, SourceKind::Mic);
        assert_eq!(c.device_id, None);
        assert_eq!(c.target_pid, None);
        assert_eq!(c.gain, 1.0);
        // Defaults of the Mix-only fields (no device specified, pre-mix multiplier 1.0).
        assert_eq!(c.mix_mic_device_id, None);
        assert_eq!(c.mix_system_device_id, None);
        assert_eq!(c.mix_mic_gain, 1.0);
        assert_eq!(c.mix_system_gain, 1.0);
        // The default output is identical to the internal canonical form (second-stage
        // passthrough).
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
        // ch=0 / ch=3 are not supported.
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
        // Extreme rates are not supported.
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
        // A reasonable configuration is OK.
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
        // Clone / PartialEq work (used for comparing and copying enumeration results).
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
        // The default is Include (records only the target PID). Exclude must be specified
        // explicitly.
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
