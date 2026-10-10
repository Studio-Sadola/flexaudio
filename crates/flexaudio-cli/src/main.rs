//! flexaudio-cli — reference capture CLI.
//!
//! Captures N seconds from the default microphone or another source, collects chunks in the
//! output format (default: 48000 Hz / stereo 2ch / interleaved f32), and writes a 16-bit PCM WAV.
//! Also reports peak / RMS (dBFS) / chunk count / dropped chunk count.
//!
//! Set the output format with `--output-rate <Hz>` (default 48000) and
//! `--output-channels <1|2>` (default 2). Audio is converted from the internal canonical form
//! (48k/stereo) by a second resampler stage (rubato, with anti-aliasing); the WAV header and stdout
//! rate/ch follow these settings. Chunks are fixed at 20 ms, so their frame count depends on the
//! rate (48k=960 / 16k=320).
//!
//! ```text
//! flexaudio-cli --source mic --seconds 5 --out mic.wav
//! flexaudio-cli --source system --output-rate 16000 --output-channels 1 --out 16k.wav --seconds 3
//! ```
//!
//! Select a device with `--device-id <ID>` (copy the ID from `--list-devices`). For mic, this
//! selects an input device; for system, an output endpoint. Omit it to use the default (mic=default
//! input / system=default output). For process, the target is selected by `--process-id`, so
//! `device_id` is ignored.
//!
//! Process and system use separate flags:
//! - `--mode include|exclude` (process only; default include): include captures only the target
//!   PID; exclude captures all system audio except the target PID (`--process-id` required).
//! - `--exclude-self` / repeatable `--exclude-pid <PID>`: remove playback from system capture
//!   or the system side of mix. Require a system/mix source, including in `--sources` schedules.
//!   The system source ignores `--mode`.
//!
//! ```text
//! flexaudio-cli --list-devices
//! flexaudio-cli --list-processes
//! flexaudio-cli --source mic --device-id "Stereo Mixer (Realtek(R) Audio)" --out cap.wav
//! ```
//!
//! With `--out -`, stream headerless raw PCM to stdout (binary) as each chunk arrives instead of
//! writing a WAV. A receiver (for example, a host app that calls `spawn('flexaudio-cli', ...)` and
//! reads stdout) can receive audio in real time. Select the sample format with `--encoding f32|s16`.
//! In this mode stdout contains only PCM bytes; summaries and other logs go to stderr. Set
//! `--seconds 0` for an infinite stream (stops on a broken pipe or Ctrl-C). Raw PCM rate/ch also
//! follow the output format; make the receiver's `-r/-c` settings match.
//!
//! ```text
//! flexaudio-cli --source system --out - --encoding s16 --seconds 0 | aplay -f S16_LE -r 48000 -c 2
//! flexaudio-cli --source system --out - --encoding s16 --output-rate 16000 --output-channels 1 --seconds 0 | aplay -f S16_LE -r 16000 -c 1
//! ```
//!
//! Use `--split-seconds <N>` (default 0 = no splitting) to record WAV files in numbered N-second
//! segments. With `--out rec.wav`, files are named `rec-001.wav, rec-002.wav, ...` (three-digit
//! zero-padded index before the extension; the width grows naturally from file 1000 onward). Boundaries
//! use 20 ms chunk granularity: a new file starts when written frames reach `N × output sample
//! rate`, so each file can be up to one chunk (±20 ms) longer than requested. Chunks are never
//! split or dropped; the next file starts with the next chunk. Since splitting is frame-based, it
//! works independently of `--sources` (hot swap) and mix. It cannot be combined with stdout
//! streaming (`--out -`); this is rejected at startup.
//!
//! ```text
//! flexaudio-cli --source mic --seconds 30 --split-seconds 10 --out rec.wav
//! ```
//!
//! In environments without an input device (such as a server or CI), real capture is unavailable.
//! The CLI prints a clear message and exits with a nonzero status instead of panicking.

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use clap::{Parser, ValueEnum};

use flexaudio::core::{AudioChunk, Error, OutputFormat, SourceKind, StreamConfig};
use flexaudio::{ProcessMode, Stream};

/// Capture source kinds (for CLI arguments).
#[derive(Debug, Clone, Copy, ValueEnum)]
enum SourceArg {
    /// Default microphone input.
    Mic,
    /// System output loopback (Linux / Windows / macOS).
    System,
    /// Process output loopback (Linux / Windows / macOS; requires `--process-id <PID>`).
    Process,
    /// Microphone + system audio mix (Linux / Windows / macOS).
    /// Set devices with `--mic-device-id` / `--system-device-id`, and per-source gain with
    /// `--mic-gain` / `--system-gain`.
    Mix,
}

/// How to handle the target PID for `--source process` (process only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ModeArg {
    /// Capture only the target PID and its process tree (default).
    Include,
    /// Capture all system audio except the target PID and its process tree (`--process-id` required).
    Exclude,
}

impl From<ModeArg> for ProcessMode {
    fn from(m: ModeArg) -> Self {
        match m {
            ModeArg::Include => ProcessMode::Include,
            ModeArg::Exclude => ProcessMode::Exclude,
        }
    }
}

/// Sample format for stdout streaming (`--out -` only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum EncodingArg {
    /// Interleaved f32 little-endian (the internal canonical form).
    F32,
    /// Interleaved i16 little-endian, compatible with external tools such as `aplay -f S16_LE`.
    S16,
}

/// flexaudio capture CLI.
#[derive(Debug, Parser)]
#[command(name = "flexaudio-cli", about = "flexaudio capture CLI")]
struct Cli {
    /// List available audio devices and exit without recording
    /// (unified `devices()` enumeration; independent of `--source` and related options).
    #[arg(long)]
    list_devices: bool,

    /// List processes with audio output sessions (streams) that can be captured with
    /// `--source process --process-id <PID>`, then exit without recording
    /// (`processes()`; independent of `--source`). Processes whose audio sessions are stopped or
    /// idle are also included.
    #[arg(long)]
    list_processes: bool,

    /// Monitor device hot-plug events and print them to stderr without recording
    /// (`watch_devices()`; stop with Ctrl-C; independent of `--source`).
    #[arg(long)]
    watch_devices: bool,

    /// Capture source (mic / system / process).
    #[arg(long, value_enum, default_value_t = SourceArg::Mic)]
    source: SourceArg,

    /// Schedule seamless source hot swaps during recording.
    /// List comma-separated `<src>:<secs>` entries (for example, `mic:2,system:2,process:2`).
    /// Each segment is captured for the specified duration before `switch_source` selects the next.
    /// The output (file or pipe) remains a single continuous chunk stream.
    /// Overrides `--source` / `--seconds` (the first segment is the initial source; total duration
    /// is the sum of all `secs`). If `process` is included, `--process-id` is required.
    /// Prints `[switch] -> <kind>` to stderr at each boundary. If a switch fails, a warning is
    /// printed and recording continues with the previous source. For WAV output, each `secs` must
    /// be at least 1. `--mode` / `--exclude-self` / `--exclude-pid` are passed to every segment;
    /// `--mode` applies only to process segments, and exclusions only to system segments.
    #[arg(long)]
    sources: Option<String>,

    /// Target process PID for `--source process` (Linux / Windows / macOS; required for process).
    /// Captures a copy via a fan-out link to the target PID's application output node. This is
    /// non-invasive; the user's speakers keep playing. It is normal for the target to start
    /// producing audio later.
    #[arg(long)]
    process_id: Option<u32>,

    /// Device ID to select (copy it from the ID column of `--list-devices`). For mic, selects an
    /// input device; for system, an output endpoint. Omit it to use the default (mic=default input /
    /// system=default output). For process, the target is selected by `--process-id`, so this value
    /// is ignored. If no device matches, exits with DeviceNotFound instead of crashing.
    #[arg(long)]
    device_id: Option<String>,

    /// How to handle the target PID for `--source process` (process only; default include).
    /// `include` captures only the target PID; `exclude` captures all system audio except the target
    /// PID (`--process-id` required; Linux / Windows / macOS). Ignored for mic / system.
    /// Use `--mode exclude` to exclude the target; it does not exclude this process.
    #[arg(long, value_enum, default_value_t = ModeArg::Include)]
    mode: ModeArg,

    /// Remove this process's playback from system capture or the system side of mix
    /// (prevents feedback loops; Linux / Windows / macOS). Requires a system/mix source.
    /// Use `--mode exclude` to exclude a target PID.
    #[arg(long, default_value_t = false)]
    exclude_self: bool,

    /// Exclude this PID's playback from system capture (or the system side of mix). Repeat for
    /// multiple PIDs; combines with --exclude-self. On Windows all excluded PIDs must belong to
    /// one process tree root (pass the root PID once).
    #[arg(long = "exclude-pid", value_name = "PID", value_parser = clap::value_parser!(u32).range(1..))]
    exclude_pids: Vec<u32>,

    /// Capture duration in seconds. `0` streams indefinitely (intended for `--out -`; stops on
    /// Ctrl-C or a broken pipe).
    #[arg(long, default_value_t = 5)]
    seconds: u64,

    /// Output destination. A file path writes a WAV; `-` streams raw PCM to stdout.
    #[arg(long, default_value = "capture.wav")]
    out: PathBuf,

    /// Seconds per WAV segment. Default 0 = no splitting (one file, as before).
    /// For values of 1 or more, finalize the current file (write its WAV header) and switch to the
    /// next numbered file whenever written frames reach `split-seconds × output sample rate`
    /// (`--out rec.wav` produces `rec-001.wav, rec-002.wav, ...`). Boundaries use 20 ms chunk
    /// granularity, so each file can be up to one chunk longer than requested. Chunks are never
    /// split or dropped; the next file starts with the next chunk. Cannot be combined with stdout
    /// streaming (`--out -`).
    #[arg(long, default_value_t = 0)]
    split_seconds: u64,

    /// Sample format for stdout streaming (`--out -` only; ignored for WAV output).
    #[arg(long, value_enum, default_value_t = EncodingArg::F32)]
    encoding: EncodingArg,

    /// Output sample rate (Hz). Default 48000. For example, `--output-rate 16000` downsamples to
    /// 16 kHz. The WAV header and stdout sample rate follow this setting.
    #[arg(long, default_value_t = 48_000)]
    output_rate: u32,

    /// Number of output channels (1 = mono / 2 = stereo). Default 2. Stereo-to-mono averages L/R.
    #[arg(long, default_value_t = 2)]
    output_channels: u16,

    /// Input gain (linear multiplier). Default 1.0; 1.0 leaves audio unchanged, 2.0 is about +6 dB,
    /// and 0.0 is silence. Samples are clamped to ±1.0 after multiplication. Negative and NaN
    /// values are errors.
    #[arg(long, default_value_t = 1.0)]
    gain: f32,

    /// Input device ID for the mic side of `--source mix` (mix only; copy from the ID column of
    /// `--list-devices`). Omit it to use the default input. Ignored for mic / system / process.
    #[arg(long)]
    mic_device_id: Option<String>,

    /// Output endpoint ID for the system side of `--source mix` (mix only). Omit it to use the
    /// default output. Ignored for mic / system / process.
    #[arg(long)]
    system_device_id: Option<String>,

    /// Pre-mix linear multiplier for the mic side of `--source mix` (mix only). Default 1.0.
    /// `--gain` is applied after mixing. Negative and NaN values are errors.
    #[arg(long, default_value_t = 1.0)]
    mic_gain: f32,

    /// Pre-mix linear multiplier for the system side of `--source mix` (mix only). Default 1.0.
    /// Negative and NaN values are errors.
    #[arg(long, default_value_t = 1.0)]
    system_gain: f32,
}

impl Cli {
    /// Whether `--out -` (a single hyphen) was specified. If true, stream raw PCM to stdout.
    fn is_stdout_stream(&self) -> bool {
        self.out == Path::new("-")
    }

    /// Build [`OutputFormat`] from CLI arguments.
    fn output_format(&self) -> OutputFormat {
        OutputFormat {
            sample_rate: self.output_rate,
            channels: self.output_channels,
        }
    }
}

/// One `--sources` segment: the next source and its duration in seconds.
#[derive(Debug, Clone, Copy)]
struct Segment {
    kind: SourceKind,
    secs: u32,
}

/// Parse `--sources "mic:2,system:2,process:2"` into a `Vec<Segment>`.
///
/// Each entry is `<src>:<secs>`. `<src>` is `mic|system|process`; `<secs>` is a positive integer
/// (seconds). Empty or invalid entries and sources unsupported on this OS return a human-readable
/// `String` error. On platforms other than Linux / Windows / macOS, system/process entries are
/// rejected here as well.
fn parse_sources(spec: &str) -> std::result::Result<Vec<Segment>, String> {
    let mut segments = Vec::new();
    for (idx, raw) in spec.split(',').enumerate() {
        let item = raw.trim();
        if item.is_empty() {
            return Err(format!(
                "--sources entry {} is empty (format: <src>:<secs>, e.g. mic:2)",
                idx + 1
            ));
        }
        let (src, secs_str) = item.split_once(':').ok_or_else(|| {
            format!("--sources entry {item:?} must use the <src>:<secs> format (e.g. mic:2)")
        })?;
        let kind = match src.trim() {
            "mic" => SourceKind::Mic,
            "system" => {
                #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
                {
                    SourceKind::SystemLoopback
                }
                #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
                {
                    return Err(
                        "--sources system (system output loopback) is currently supported only on Linux / Windows / macOS."
                            .into(),
                    );
                }
            }
            "process" => {
                #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
                {
                    SourceKind::ProcessLoopback
                }
                #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
                {
                    return Err(
                        "--sources process (process output loopback) is currently supported only on Linux / Windows / macOS."
                            .into(),
                    );
                }
            }
            other => {
                return Err(format!(
                    "unknown --sources source {other:?}; expected mic, system, or process"
                ))
            }
        };
        let secs: u32 = secs_str.trim().parse().map_err(|_| {
            format!("--sources duration {secs_str:?} must be a positive integer (e.g. mic:2)")
        })?;
        if secs == 0 {
            return Err(format!(
                "--sources duration must be at least 1 second (entry {item:?})"
            ));
        }
        segments.push(Segment { kind, secs });
    }
    if segments.is_empty() {
        return Err("--sources is empty (e.g. mic:2,system:2)".into());
    }
    Ok(segments)
}

/// Build [`StreamConfig`] from the given [`SourceKind`] and shared CLI settings
/// (output / pid / exclusions). Used for initial capture and every `--sources` segment.
fn config_for_kind(cli: &Cli, kind: SourceKind) -> StreamConfig {
    StreamConfig {
        kind,
        output: cli.output_format(),
        target_pid: cli.process_id,
        // `mode` applies only to process segments (the facade ignores it for mic/system).
        mode: cli.mode.into(),
        // Exclusions apply to system capture and the system side of mix (ignored for mic/process).
        exclude_self: cli.exclude_self,
        exclude_pids: cli.exclude_pids.clone(),
        // `device_id` applies to mic (input) and system (output endpoint); the facade ignores it
        // for process. Set it on every segment so the relevant segment can use it.
        device_id: cli.device_id.clone(),
        // Gain does not change on a source switch (core ignores it), but keep it aligned with the
        // initial config.
        gain: cli.gain,
        // Mix-only settings (the facade ignores them for other segment types).
        mix_mic_device_id: cli.mic_device_id.clone(),
        mix_system_device_id: cli.system_device_id.clone(),
        mix_mic_gain: cli.mic_gain,
        mix_system_gain: cli.system_gain,
        ..Default::default()
    }
}

/// Reject exclusions when the effective source plan has no system capture.
fn validate_exclusion_sources(cli: &Cli, segments: Option<&[Segment]>) -> Result<(), Error> {
    let has_system = match segments {
        Some(segments) => segments
            .iter()
            .any(|segment| matches!(segment.kind, SourceKind::SystemLoopback | SourceKind::Mix)),
        None => matches!(cli.source, SourceArg::System | SourceArg::Mix),
    };
    if (cli.exclude_self || !cli.exclude_pids.is_empty()) && !has_system {
        return Err(Error::InvalidArg(
            "--exclude-pid and --exclude-self require --source system or mix, or a system segment in --sources."
                .into(),
        ));
    }
    Ok(())
}

/// Hot-swap scheduler for `--sources`.
///
/// The first segment is the initial source (already opened and started). At each later segment
/// boundary (cumulative seconds), `stream.switch_source()` selects the next source. The caller
/// keeps one output (file/pipe), so switches are transparent in the continuous chunk stream
/// (`Stream` guarantees continuous seq/PTS).
///
/// Call [`tick`](Self::tick) on each collection loop iteration to execute any switches whose
/// boundaries have passed. A failed switch prints a warning with `eprintln!` and recording
/// continues on the previous source. Prints `[switch] -> <kind>` to stderr at each boundary.
struct SwitchScheduler {
    /// Index of the next segment to switch to (1-based; the initial source is not a switch target).
    next: usize,
    /// Absolute time for each boundary (`deadlines[i]` is when to switch to segment `i+1`).
    deadlines: Vec<Instant>,
    /// Config for the next source (`configs[i]` is used at `deadlines[i]`).
    configs: Vec<StreamConfig>,
    /// Display label (`labels[i]` is the kind label for `configs[i]`).
    labels: Vec<&'static str>,
}

impl SwitchScheduler {
    /// Build a scheduler from the segment plan, using `start` as the reference time.
    /// The first segment is the initial source, so it is not a switch target.
    fn new(cli: &Cli, segments: &[Segment], start: Instant) -> Self {
        let mut deadlines = Vec::new();
        let mut configs = Vec::new();
        let mut labels = Vec::new();
        let mut cumulative = 0u64;
        for (i, seg) in segments.iter().enumerate() {
            cumulative += seg.secs as u64;
            // The last segment's end is the total recording duration, not a switch boundary.
            // Segment i ends at the switch to segment i+1, if that segment exists.
            if i + 1 < segments.len() {
                deadlines.push(start + Duration::from_secs(cumulative));
                let next_seg = segments[i + 1];
                configs.push(config_for_kind(cli, next_seg.kind));
                labels.push(source_kind_label(next_seg.kind));
            }
        }
        Self {
            next: 0,
            deadlines,
            configs,
            labels,
        }
    }

    /// Total recording duration (sum of all segment durations), used as the collection-loop deadline.
    fn total_duration(segments: &[Segment]) -> Duration {
        let total: u64 = segments.iter().map(|s| s.secs as u64).sum();
        Duration::from_secs(total)
    }

    /// Execute every switch whose boundary has passed by `now`. Warn and continue on failure.
    fn tick(&mut self, stream: &mut Stream, now: Instant) -> Result<(), Error> {
        while self.next < self.deadlines.len() && now >= self.deadlines[self.next] {
            let label = self.labels[self.next];
            let config = self.configs[self.next].clone();
            match stream.switch_source(config) {
                Ok(()) => {
                    eprintln!("[switch] -> {label}");
                }
                Err(e @ Error::PermissionDenied { .. }) => return Err(e),
                Err(e) => {
                    eprintln!(
                        "[switch] Warning: failed to switch to {label} (recording continues): {e}"
                    );
                }
            }
            self.next += 1;
        }
        Ok(())
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    // Always print errors from main to stderr (do not contaminate stdout PCM streaming).
    match run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("Error: {msg}");
            ExitCode::FAILURE
        }
    }
}

/// Main operation. Returns failures as human-readable `String` messages.
fn run(cli: &Cli) -> std::result::Result<(), String> {
    // Enforce the resource guard before any device access, including enumeration modes.
    if cli.exclude_pids.len() > 4096 {
        return Err(describe_error(Error::InvalidArg(
            "exclude_pids: too many entries (max 4096)".into(),
        )));
    }

    // Device listing mode (enumerate without recording, then exit). Handle this before and
    // independently of `--source` and related options.
    if cli.list_devices {
        return list_devices();
    }

    // Process listing mode (enumerate without recording, then exit), also independent of `--source`.
    if cli.list_processes {
        return list_processes();
    }

    // Device hot-plug monitoring mode (monitor without recording), also independent of `--source`.
    if cli.watch_devices {
        return watch_devices_loop();
    }

    let stdout_stream = cli.is_stdout_stream();
    if !stdout_stream {
        validate_output_path(&cli.out).map_err(|error| error.to_string())?;
    }

    // `--split-seconds` is only for WAV files. Stdout streaming (`--out -`) has no file boundaries,
    // so reject this combination before opening the stream.
    if stdout_stream && cli.split_seconds > 0 {
        return Err(
            "--split-seconds (split recording) is only for WAV file output. It cannot be combined with \
             raw PCM streaming to stdout (--out -). Specify a file path for --out."
                .into(),
        );
    }

    // Select the log destination. During stdout streaming, send all logs to stderr (stdout is PCM
    // only); for file output, summaries can go to stdout. Route subsequent println!/eprintln! calls
    // through this macro.
    macro_rules! log {
        ($($arg:tt)*) => {
            if stdout_stream {
                eprintln!($($arg)*);
            } else {
                println!($($arg)*);
            }
        };
    }

    // Resolve the `--sources` hot-swap schedule. When set, it overrides `--source` / `--seconds`
    // and uses the first segment as the initial source. Require `--process-id` if any segment is
    // process (otherwise `build_backend` via `switch_source` fails because the PID is missing).
    let segments: Option<Vec<Segment>> = match &cli.sources {
        None => None,
        Some(spec) => {
            let segs = parse_sources(spec)?;
            let needs_pid = segs.iter().any(|s| s.kind == SourceKind::ProcessLoopback);
            if needs_pid && cli.process_id.is_none() {
                return Err(
                    "--process-id <PID> is required when --sources includes process.".into(),
                );
            }
            Some(segs)
        }
    };

    validate_exclusion_sources(cli, segments.as_deref()).map_err(describe_error)?;

    // Resolve SourceKind and its display label. The `flexaudio::open` facade builds and selects
    // the backend (internally choosing a `Box<dyn CaptureBackend>` and returning a Stream). The CLI
    // only selects SourceKind and its label, and performs human-readable preflight checks (PID
    // required for process; reject system/process on unsupported OSes). With `--sources`, use the
    // first segment's kind as the initial source.
    let (kind, source_label): (SourceKind, &str) = if let Some(segs) = &segments {
        let first = segs[0].kind;
        let label = match first {
            SourceKind::Mic => "mic (default input device)",
            SourceKind::SystemLoopback => "system (default output loopback)",
            SourceKind::ProcessLoopback => "process (output from specified PID)",
            SourceKind::Mix => "mix (microphone + system audio)",
        };
        (first, label)
    } else {
        match cli.source {
            SourceArg::Mic => (SourceKind::Mic, "mic (default input device)"),
            SourceArg::System => {
                #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
                {
                    (
                        SourceKind::SystemLoopback,
                        "system (default output loopback)",
                    )
                }
                #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
                {
                    return Err(
                    "--source system (system output loopback) is currently supported only on Linux / Windows / macOS."
                        .into(),
                );
                }
            }
            SourceArg::Process => {
                // Process requires a PID. Stop with a clear error if it is missing (the facade also
                // returns InvalidArg, but reject it here first with a human-readable message). This
                // check is OS-independent.
                if cli.process_id.is_none() {
                    return Err("--process-id <PID> is required for --source process. \
                     Specify the target process PID (for example, the PID of a running \
                     speaker-test process)."
                        .into());
                }
                #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
                {
                    (
                        SourceKind::ProcessLoopback,
                        "process (output from specified PID)",
                    )
                }
                #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
                {
                    return Err(
                    "--source process (process output loopback) is currently supported only on Linux / Windows / macOS."
                        .into(),
                );
                }
            }
            SourceArg::Mix => {
                // Mix requires system-side loopback, so it has the same OS support as system.
                #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
                {
                    (SourceKind::Mix, "mix (microphone + system audio)")
                }
                #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
                {
                    return Err(
                    "--source mix (microphone + system audio) is currently supported only on Linux / Windows / macOS."
                        .into(),
                );
                }
            }
        }
    };

    // --- Resolve and validate output format ---
    let output = cli.output_format();
    output.validate().map_err(|e| {
        format!(
            "Unsupported output format {}Hz/{}ch: {e}",
            output.sample_rate, output.channels
        )
    })?;
    let out_rate = output.sample_rate;
    let out_ch = output.channels;

    // `--split-seconds × output rate` is the per-file frame threshold. Reject a product that
    // overflows u64 here, before any device access, so the writer never has to fall back on a
    // saturated threshold (`parse_sources` has already bounded the `--sources` alternative).
    if cli.split_seconds > 0
        && cli
            .split_seconds
            .checked_mul(u64::from(cli.output_rate))
            .is_none()
    {
        return Err(describe_error(Error::InvalidArg(format!(
            "--split-seconds {} is too large for --output-rate {} (the per-file frame count \
             overflows)",
            cli.split_seconds, cli.output_rate
        ))));
    }

    // Validate that the recording deadline (`start + total duration`) is representable before
    // opening the stream, so an unsatisfiable duration (for example `--seconds` near u64::MAX)
    // fails with a typed argument error before any device access or capture start. `Instant::now()
    // .checked_add` is enough to prove representability. The later `checked_add` calls in run_wav /
    // run_stdout_stream remain as defensive guards (they must not panic).
    let total_duration = match &segments {
        Some(segs) => SwitchScheduler::total_duration(segs),
        None => Duration::from_secs(cli.seconds),
    };
    if Instant::now().checked_add(total_duration).is_none() {
        return Err(describe_error(Error::InvalidArg(
            "the recording duration is too large: its deadline overflows the clock".into(),
        )));
    }

    // Open the stream. `open` selects a `Box<dyn CaptureBackend>` internally based on config.kind.
    // Do not start it yet (two-stage flow). Read native_format from the opened Stream.
    let config = config_for_kind(cli, kind);
    let mut stream = flexaudio::open(config).map_err(describe_error)?;

    // --- Show native format ---
    let (native_rate, native_ch) = stream.native_format();
    log!("Source             : {source_label}");
    // Show the selected device when `device_id` is set (valid for mic/system; ignored for process/mix).
    if let Some(id) = &cli.device_id {
        match kind {
            SourceKind::ProcessLoopback => {
                log!("Device ID          : {id} (Note: ignored for process)");
            }
            SourceKind::Mix => {
                log!(
                    "Device ID          : {id} (Note: ignored for mix. \
                     Use --mic-device-id / --system-device-id instead)"
                );
            }
            _ => log!("Device ID          : {id}"),
        }
    }
    log!("Native format      : {native_rate} Hz / {native_ch} ch");
    if stdout_stream {
        let enc = match cli.encoding {
            EncodingArg::F32 => "f32 LE",
            EncodingArg::S16 => "s16 LE",
        };
        log!("Output format      : {out_rate} Hz / {out_ch} ch / {enc} raw PCM (stdout)");
    } else {
        log!("Output format      : {out_rate} Hz / {out_ch} ch / 16-bit PCM WAV");
    }
    if let Some(segs) = &segments {
        let plan: Vec<String> = segs
            .iter()
            .map(|s| format!("{}:{}s", source_kind_label(s.kind), s.secs))
            .collect();
        let total: u32 = segs.iter().map(|s| s.secs).sum();
        log!(
            "Schedule           : {} (total {total} seconds; one continuous stream)",
            plan.join(" -> ")
        );
    } else if cli.seconds == 0 {
        log!("Capture duration   : unlimited (stops on Ctrl-C / broken pipe)");
    } else {
        log!("Capture duration   : {} seconds", cli.seconds);
    }
    if stdout_stream {
        log!("Output             : stdout (raw PCM streaming)");
    } else if cli.split_seconds > 0 {
        // Split recording does not create a file at the base path, so show the actual numbered names.
        log!(
            "Output files       : {}, {}, ... (split every {} seconds)",
            split_file_path(&cli.out, 1).display(),
            split_file_path(&cli.out, 2).display(),
            cli.split_seconds
        );
    } else {
        log!("Output path        : {}", cli.out.display());
    }
    log!("");

    // --- Start capture (second stage: start the already-open Stream) ---
    stream.start().map_err(describe_error)?;

    log!("Capturing ...");

    if stdout_stream {
        run_stdout_stream(cli, &mut stream, segments.as_deref())
    } else {
        run_wav(cli, &mut stream, output, segments.as_deref())
    }
}

/// `--list-devices`: Get devices with `devices()` and display them in a table.
///
/// Columns: SOURCE (mic/system/process) / LOOPBACK / DEFAULT / RATE / CH / NAME / ID.
/// `id` is the device name (cpal) or `node.name` (PipeWire). If no devices are available, report
/// that without returning an error.
fn list_devices() -> std::result::Result<(), String> {
    let devices = flexaudio::devices().map_err(|e| format!("Failed to enumerate devices: {e}"))?;

    if devices.is_empty() {
        println!("No audio devices are available.");
        println!(
            "Run this in an environment with an audio device. \
             A PipeWire session is required to enumerate system devices on Linux."
        );
        return Ok(());
    }

    println!("Available audio devices: {}", devices.len());
    println!();
    // Fixed-width header (variable-length id/name are at the end).
    println!(
        "{:<7} {:<8} {:<7} {:>6} {:>3}  {:<28} ID",
        "SOURCE", "LOOPBACK", "DEFAULT", "RATE", "CH", "NAME"
    );
    println!("{}", "-".repeat(88));
    for d in &devices {
        println!(
            "{:<7} {:<8} {:<7} {:>6} {:>3}  {:<28} {}",
            source_kind_label(d.source_kind),
            if d.is_loopback { "yes" } else { "no" },
            if d.is_default { "*" } else { "" },
            d.sample_rate,
            d.channels,
            truncate(&d.name, 28),
            d.id,
        );
    }
    println!();
    println!(
        "DEFAULT * marks the OS default device. ID is a stable key for selecting a device with \
         `--device-id <ID>`. Mic uses an input device, system an output endpoint; ignored for process."
    );
    Ok(())
}

/// `--list-processes`: Get capturable processes with `processes()` and display them in a table.
///
/// Columns: ACTIVE (`*` if output is active, `?` if unknown) / PID / NAME / EXECUTABLE / BUNDLE.
/// Pass the PID to `--source process --process-id <PID>`. If there are no candidates, report that;
/// return an error if process capture itself is unavailable.
fn list_processes() -> std::result::Result<(), String> {
    let processes = flexaudio::processes().map_err(|e| {
        format!("Failed to enumerate processes (process capture is unavailable in this environment): {e}")
    })?;

    if processes.is_empty() {
        println!("No processes currently have an audio output session (stream).");
        println!("Process capture is available in this environment. Processes whose audio sessions are stopped or idle are also included.");
        println!("So an empty list means there are no output sessions, not merely that nothing is playing.");
        return Ok(());
    }

    println!("Capturable processes: {}", processes.len());
    println!();
    println!(
        "{:<6} {:>7}  {:<28} {:<24} BUNDLE",
        "ACTIVE", "PID", "NAME", "EXECUTABLE"
    );
    println!("{}", "-".repeat(88));
    for p in &processes {
        let active = match p.is_output_active {
            Some(true) => "*",
            Some(false) => "",
            None => "?",
        };
        println!(
            "{:<6} {:>7}  {:<28} {:<24} {}",
            active,
            p.pid,
            truncate(&p.name, 28),
            truncate(p.executable.as_deref().unwrap_or("-"), 24),
            p.bundle_id.as_deref().unwrap_or("-"),
        );
    }
    println!();
    println!(
        "ACTIVE * means output is active; ? means the OS does not expose the state. Pass the PID to \
         `--source process --process-id <PID>`."
    );
    Ok(())
}

/// `--watch-devices`: Monitor device hot-plug events with `watch_devices()` and print them to
/// stderr until stopped with Ctrl-C.
///
/// Keep stdout available for future machine-readable output, so send all logs and events to stderr.
/// Count existing devices with `devices()` and print the count at startup.
///
/// Output format (all to stderr):
/// - `[+] ADDED   <source> <name> (<id>)` — device added
/// - `[-] REMOVED <id>` — device removed (`id` is `node.name` only)
/// - `[*] DEFAULT <source> -> <id>` — default device changed
fn watch_devices_loop() -> std::result::Result<(), String> {
    use flexaudio::core::DeviceEvent;

    // Report the number of existing devices at startup (stderr). Enumeration failure is non-fatal.
    let existing = flexaudio::devices().map(|d| d.len()).unwrap_or(0);
    eprintln!(
        "Started device hot-plug monitoring ({existing} existing devices). Press Ctrl-C to stop."
    );
    eprintln!();

    // Clear the running flag on Ctrl-C (SIGINT).
    let running = Arc::new(AtomicBool::new(true));
    {
        let r = running.clone();
        ctrlc::set_handler(move || {
            r.store(false, Ordering::SeqCst);
        })
        .map_err(|e| format!("Failed to register Ctrl-C handler: {e}"))?;
    }

    // Start monitoring. Degrade to Ok if PipeWire is unavailable (there will simply be no events).
    let mut watcher = flexaudio::watch_devices()
        .map_err(|e| format!("Failed to start device monitoring: {e}"))?;

    while running.load(Ordering::SeqCst) {
        while let Some(ev) = watcher.poll_event() {
            match ev {
                DeviceEvent::Added(info) => {
                    eprintln!(
                        "[+] ADDED   {:<7} {} ({})",
                        source_kind_label(info.source_kind),
                        info.name,
                        info.id,
                    );
                }
                DeviceEvent::Removed { id } => {
                    eprintln!("[-] REMOVED {id}");
                }
                DeviceEvent::DefaultChanged { kind, id } => {
                    eprintln!(
                        "[*] DEFAULT {:<7} -> {}",
                        source_kind_label(kind.into()),
                        id,
                    );
                }
                DeviceEvent::DefaultCleared { .. } | DeviceEvent::RescanRequired { .. } => {
                    eprintln!("[?] UNKNOWN device event: pending 0.5 CLI support");
                }
                // Future variants remain observable without exposing raw diagnostics.
                _ => {
                    eprintln!("[?] UNKNOWN device event");
                }
            }
        }
        // Hot-plug events are infrequent. Sleep to avoid spinning; 100 ms is responsive enough.
        thread::sleep(Duration::from_millis(100));
    }

    watcher.stop();
    eprintln!();
    eprintln!("Stopped device hot-plug monitoring (Ctrl-C).");
    Ok(())
}

/// Convert [`SourceKind`] to a short label for CLI output.
fn source_kind_label(kind: SourceKind) -> &'static str {
    match kind {
        SourceKind::Mic => "mic",
        SourceKind::SystemLoopback => "system",
        SourceKind::ProcessLoopback => "process",
        SourceKind::Mix => "mix",
    }
}

/// Truncate a display string to `max` characters (by `char`), using `…` for overflow.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let keep = max.saturating_sub(1);
        let mut t: String = s.chars().take(keep).collect();
        t.push('…');
        t
    }
}

/// Report advisory events and return confirmed denial to the caller.
fn report_capture_event(event: flexaudio::Event) -> Result<(), Error> {
    match event {
        flexaudio::Event::TerminalError { error } => Err(error),
        flexaudio::Event::PermissionDenied { permission, detail } => {
            Err(Error::PermissionDenied { permission, detail })
        }
        flexaudio::Event::PermissionPending { detail, .. }
        | flexaudio::Event::SilenceWhileSourceActive { detail } => {
            eprintln!("Warning: {detail}");
            Ok(())
        }
        flexaudio::Event::RecoverableError { .. }
        | flexaudio::Event::ShutdownError { .. }
        | flexaudio::Event::AudioLoss { .. }
        | flexaudio::Event::Clipped
        | flexaudio::Event::PermissionGranted => {
            eprintln!("Unknown event: pending 0.5 CLI support");
            Ok(())
        }
        _ => {
            eprintln!("Unknown event");
            Ok(())
        }
    }
}

/// Both output paths share terminal-failure handling, including final shutdown.
fn drain_capture_events(stream: &mut Stream) -> std::result::Result<(), String> {
    let result = (|| {
        while let Some(event) = stream.poll_event() {
            report_capture_event(event)?;
        }
        match stream.terminal_error() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    })();
    result.map_err(|error| {
        stream.stop();
        describe_error(error)
    })
}

/// WAV output path (legacy behavior). Collect N seconds (>0), write a 16-bit WAV, and print a
/// summary to stdout. `output` is the output format (used for the WAV header rate/ch and captured
/// duration calculation).
fn run_wav(
    cli: &Cli,
    stream: &mut Stream,
    output: OutputFormat,
    segments: Option<&[Segment]>,
) -> std::result::Result<(), String> {
    // Total duration: sum of segments when `--sources` is set, otherwise `--seconds`.
    // Reject `--seconds 0` for WAV output to avoid buffering forever (`parse_sources` already
    // requires every `--sources` duration to be at least 1).
    if segments.is_none() && cli.seconds == 0 {
        stream.stop();
        return Err(
            "--seconds 0 (unlimited) is only for raw PCM streaming (--out -). \
             Specify at least 1 second for WAV output."
                .into(),
        );
    }

    let start = Instant::now();
    let total = match segments {
        Some(segs) => SwitchScheduler::total_duration(segs),
        None => Duration::from_secs(cli.seconds),
    };
    // Hot-swap scheduler (only when `--sources` is set; keep one output destination).
    let mut scheduler = segments.map(|segs| SwitchScheduler::new(cli, segs, start));

    // WAV writer. Write chunks as they arrive without buffering (so each split boundary can be
    // finalized immediately and long recordings do not accumulate in memory). `--split-seconds 0`
    // writes one file to `--out` as before; values >= 1 rotate through numbered files. Peak / RMS
    // are calculated over the entire recording.
    let mut writer = RotatingWavWriter::new(&cli.out, output, cli.split_seconds);
    let mut chunk_count: u64 = 0;

    // Poll chunks for the full recording duration and write them all. A duration that no clock can
    // reach (for example `--seconds` near u64::MAX) is a typed argument error, not a panic.
    let Some(deadline) = start.checked_add(total) else {
        stream.stop();
        return Err(describe_error(Error::InvalidArg(
            "the recording duration is too large: its deadline overflows the clock".into(),
        )));
    };
    while Instant::now() < deadline {
        // Switch sources at segment boundaries, independently of output-file rotation.
        if let Some(sch) = scheduler.as_mut() {
            if let Err(error) = sch.tick(stream, Instant::now()) {
                stream.stop();
                return Err(describe_error(error));
            }
        }

        drain_capture_events(stream)?;
        let mut got_any = false;
        while let Some(chunk) = stream.poll_chunk() {
            got_any = true;
            chunk_count += 1;
            if let Err(e) = write_wav_chunk(&mut writer, &chunk) {
                stream.stop();
                return Err(e);
            }
        }
        drain_capture_events(stream)?;
        if !got_any {
            // One chunk is about 20 ms. Sleep briefly to avoid spinning.
            thread::sleep(Duration::from_millis(10));
        }
    }

    let dropped = stream.dropped_chunks();
    stream.stop();
    drain_capture_events(stream)?;

    // Write any chunks remaining in the ring after stop (no dropped data).
    while let Some(chunk) = stream.poll_chunk() {
        chunk_count += 1;
        write_wav_chunk(&mut writer, &chunk)?;
    }

    // Recording ran even if no chunks arrived, so do not fail. Write an empty WAV and warn (this
    // can happen if the source is silent/excluded or the selected endpoint is inactive).
    if chunk_count == 0 {
        eprintln!(
            "Warning: No audio was captured (the source may be silent/excluded or the selected endpoint \
             may be inactive). Writing an empty WAV."
        );
    }

    // Finish writing, finalize the open WAV header, and get peak / RMS for the full recording.
    // WAV output is currently fixed to s16 (apply `encoding` to WAV output if f32 WAV is needed).
    let summary = writer
        .finish()
        .map_err(|e| format!("Failed to write WAV: {e}"))?;
    let total_frames = summary.total_frames;
    let stats = summary.stats;

    let captured_secs = total_frames as f64 / output.sample_rate as f64;
    let rms_dbfs = if stats.rms > 0.0 {
        20.0 * stats.rms.log10()
    } else {
        f64::NEG_INFINITY
    };
    let peak_dbfs = if stats.peak > 0.0 {
        20.0 * (stats.peak as f64).log10()
    } else {
        f64::NEG_INFINITY
    };

    println!();
    println!("=== Results ===");
    println!("Captured chunks    : {chunk_count}");
    println!("Total frames       : {total_frames}");
    println!("Captured duration  : {captured_secs:.3} seconds");
    println!("Dropped chunks     : {dropped}");
    println!(
        "Peak               : {:.4} ({})",
        stats.peak,
        fmt_dbfs(peak_dbfs)
    );
    println!(
        "RMS                : {:.6} ({})",
        stats.rms,
        fmt_dbfs(rms_dbfs)
    );
    // When split recording creates multiple files, show the first and last numbered paths (`finish`
    // guarantees at least one file, so `files` is nonempty). For one file, show its path as before.
    if summary.files.len() == 1 {
        println!("WAV output         : {}", summary.files[0].display());
    } else {
        println!(
            "WAV output         : {} to {} ({} files, split every {} seconds)",
            summary.files[0].display(),
            summary.files[summary.files.len() - 1].display(),
            summary.files.len(),
            cli.split_seconds
        );
    }

    // If chunks arrived but are nearly silent (peak/RMS near zero), warn as for an empty WAV.
    // This also identifies OSes that deliver silent frames (producing a silent WAV). Determine
    // silence from statistics over the entire recording (all files combined).
    if chunk_count > 0 && stats.peak < SILENCE_PEAK && stats.rms < SILENCE_RMS {
        eprintln!(
            "Warning: No meaningful audio was captured (the source may be silent/excluded or the \
             selected endpoint may be inactive). The recording is nearly silent."
        );
    }

    Ok(())
}

/// Linear peak / RMS thresholds for classifying audio as nearly silent. Below these values, warn
/// that the recording is silent. This is a loose threshold around -60 dBFS, not an exact value.
const SILENCE_PEAK: f32 = 1.0e-3;
const SILENCE_RMS: f64 = 1.0e-4;

/// Raw PCM streaming path to stdout.
///
/// Write each chunk to stdout as soon as it arrives and flush immediately to avoid buffering
/// (low latency). With `--seconds 0`, stream indefinitely (normal stop on Ctrl-C or `BrokenPipe`);
/// with `--seconds N>0`, stop after N seconds. In either case, drain remaining ring chunks after stop.
fn run_stdout_stream(
    cli: &Cli,
    stream: &mut Stream,
    segments: Option<&[Segment]>,
) -> std::result::Result<(), String> {
    // With `--sources`, record for the finite sum of segment durations (`--seconds` is overridden).
    let infinite = segments.is_none() && cli.seconds == 0;

    // Ctrl-C (SIGINT) flag. Stop when pressed during an infinite stream. `ctrlc` errors on duplicate
    // handler registration, so register only for infinite streams.
    let running = Arc::new(AtomicBool::new(true));
    if infinite {
        let r = running.clone();
        ctrlc::set_handler(move || {
            r.store(false, Ordering::SeqCst);
        })
        .map_err(|e| format!("Failed to register Ctrl-C handler: {e}"))?;
    }

    // Lock stdout and wrap it in a BufWriter. Flush each chunk so data does not accumulate.
    let stdout = io::stdout();
    let mut out = BufWriter::new(stdout.lock());

    let start = Instant::now();
    // Finite recording deadline: sum of segment durations with `--sources`, otherwise `--seconds`.
    // A duration that no clock can reach (for example `--seconds` near u64::MAX) is a typed argument
    // error, not a panic.
    let deadline = if infinite {
        None
    } else {
        let dur = match segments {
            Some(segs) => SwitchScheduler::total_duration(segs),
            None => Duration::from_secs(cli.seconds),
        };
        match start.checked_add(dur) {
            Some(deadline) => Some(deadline),
            None => {
                stream.stop();
                return Err(describe_error(Error::InvalidArg(
                    "the recording duration is too large: its deadline overflows the clock".into(),
                )));
            }
        }
    };
    // Hot-swap scheduler (only with `--sources`; stdout remains one pipe).
    let mut scheduler = segments.map(|segs| SwitchScheduler::new(cli, segs, start));

    let mut wrote_any = false;
    let mut broken_pipe = false;

    // Main loop: poll and stream chunks to stdout.
    'outer: loop {
        // Check stop conditions (Ctrl-C for an infinite stream, deadline for a finite one).
        if infinite {
            if !running.load(Ordering::SeqCst) {
                break;
            }
        } else if let Some(dl) = deadline {
            if Instant::now() >= dl {
                break;
            }
        }

        // Switch sources at segment boundaries (keep the same output pipe).
        if let Some(sch) = scheduler.as_mut() {
            if let Err(error) = sch.tick(stream, Instant::now()) {
                stream.stop();
                return Err(describe_error(error));
            }
        }

        drain_capture_events(stream)?;
        let mut got_any = false;
        while let Some(chunk) = stream.poll_chunk() {
            got_any = true;
            match write_chunk(&mut out, &chunk, cli.encoding) {
                Ok(()) => wrote_any = true,
                Err(e) if e.kind() == io::ErrorKind::BrokenPipe => {
                    // The receiver closed the pipe (for example, `| head`). Stop normally, not as an error.
                    broken_pipe = true;
                    break 'outer;
                }
                Err(e) => return Err(format!("Failed to write to stdout: {e}")),
            }
        }

        drain_capture_events(stream)?;

        if !got_any {
            // One chunk is about 20 ms. Sleep briefly to avoid spinning.
            thread::sleep(Duration::from_millis(10));
        }
    }

    let dropped = stream.dropped_chunks();
    stream.stop();
    drain_capture_events(stream)?;

    // Drain chunks remaining in the ring after stop (skip if the pipe is broken).
    if !broken_pipe {
        while let Some(chunk) = stream.poll_chunk() {
            match write_chunk(&mut out, &chunk, cli.encoding) {
                Ok(()) => wrote_any = true,
                Err(e) if e.kind() == io::ErrorKind::BrokenPipe => {
                    broken_pipe = true;
                    break;
                }
                Err(e) => return Err(format!("Failed to write to stdout: {e}")),
            }
        }
    }

    // Final flush. A broken pipe is also normal here.
    if !broken_pipe {
        if let Err(e) = out.flush() {
            if e.kind() != io::ErrorKind::BrokenPipe {
                return Err(format!("Failed to flush stdout: {e}"));
            }
            broken_pipe = true;
        }
    }

    // --- Summary (stderr) ---
    eprintln!();
    eprintln!("=== Results (stderr) ===");
    if broken_pipe {
        eprintln!("Stop reason        : receiver closed the pipe (normal exit)");
    } else if infinite {
        eprintln!("Stop reason        : Ctrl-C (normal exit)");
    } else {
        eprintln!("Stop reason        : {} seconds elapsed", cli.seconds);
    }
    eprintln!("Dropped chunks     : {dropped}");

    // A broken pipe or Ctrl-C is a receiver-driven stop, so zero samples is not an error.
    // Warn only when a finite-duration stream completes normally without producing a sample.
    if !wrote_any && !broken_pipe && !infinite {
        return Err(
            "No chunks were captured. \
             The device opened, but no samples arrived (check mute, permissions, and similar settings)."
                .into(),
        );
    }

    Ok(())
}

/// Write one chunk of interleaved f32 to `out` in the specified encoding (little-endian).
///
/// Combine a chunk into one byte buffer to avoid small per-sample writes, then write it with one
/// `write_all` call. Flush immediately after writing (low latency, no buffering).
fn write_chunk<W: Write>(out: &mut W, chunk: &AudioChunk, encoding: EncodingArg) -> io::Result<()> {
    match encoding {
        EncodingArg::F32 => {
            // f32 LE: use the format as-is. 4 bytes per sample.
            let mut buf = Vec::with_capacity(chunk.data.len() * 4);
            for &x in &chunk.data {
                buf.extend_from_slice(&x.to_le_bytes());
            }
            out.write_all(&buf)?;
        }
        EncodingArg::S16 => {
            // s16 LE: quantize with the shared canonical implementation
            // [`flexaudio::core::quantize_i16`] (scale 32768, round, clamp, NaN→0). 2 bytes per sample.
            let mut buf = Vec::with_capacity(chunk.data.len() * 2);
            for &x in &chunk.data {
                let s = flexaudio::core::quantize_i16(x);
                buf.extend_from_slice(&s.to_le_bytes());
            }
            out.write_all(&buf)?;
        }
    }
    // Write immediately when data arrives; do not buffer in BufWriter.
    out.flush()
}

/// Signal statistics computed while writing the WAV.
struct Stats {
    /// Maximum absolute value across all samples (linear, roughly 0.0..=1.0).
    peak: f32,
    /// Root mean square across all samples (linear).
    rms: f64,
}

/// Reject invalid destinations before capture or file creation.
fn validate_output_path(path: &Path) -> std::io::Result<()> {
    if path.is_dir() || path.file_stem().is_none_or(|stem| stem.is_empty()) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "output destination must have a nonempty file stem and must not be a directory",
        ));
    }
    Ok(())
}

/// Create the path for the `index`th split recording file (1-based; pure function).
///
/// For `rec.wav`, insert a three-digit zero-padded index before the extension, as in
/// `rec-001.wav, rec-002.wav, ...`. The width grows naturally from file 1000 (`rec-1000.wav`).
/// For a path without an extension (`rec`), append the index (`rec-001`). Preserve the parent directory.
fn split_file_path(base: &Path, index: u64) -> PathBuf {
    let stem = base
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = match base.extension() {
        Some(ext) => format!("{stem}-{index:03}.{}", ext.to_string_lossy()),
        None => format!("{stem}-{index:03}"),
    };
    base.with_file_name(name)
}

/// Recording summary (written file list, total frames, and whole-recording statistics).
struct WavSummary {
    /// Paths of written files, in order (at least one).
    files: Vec<PathBuf>,
    /// Total frames across all files.
    total_frames: u64,
    /// Peak / RMS for the entire recording (all files combined).
    stats: Stats,
}

/// Small writer that handles WAV output splitting (rotation).
///
/// With `--split-seconds 0` (no splitting), write one file to `--out` as before. For values >= 1,
/// write to numbered paths from [`split_file_path`]. Whenever written frames reach
/// `split_seconds × output sample rate`, finalize (write the WAV header) and close the current file,
/// then write the next chunk to the next file. Boundaries use 20 ms chunk granularity, so a file
/// can be up to one chunk longer than requested (chunks are never split or dropped).
///
/// Open each file only when its first chunk arrives (lazy creation), so recording that ends exactly
/// at a boundary leaves no empty trailing file. Calculate peak / RMS across the entire recording
/// (all files combined) to preserve the same statistics and silence warning as single-file output.
/// Quantization uses the shared canonical [`flexaudio::core::quantize_i16`] (scale 32768, round,
/// clamp); output is fixed to 16-bit PCM.
struct RotatingWavWriter {
    /// Base path for `--out` (used to derive numbered paths when splitting; otherwise used as-is).
    base: PathBuf,
    /// WAV header format (follows output format; fixed to 16-bit PCM).
    spec: hound::WavSpec,
    /// Frame threshold per file (`split_seconds × rate`). 0 = no splitting.
    frames_per_file: u64,
    /// Current writer (lazy-created; None before writing and just after rotation).
    writer: Option<hound::WavWriter<BufWriter<File>>>,
    /// Frames written to the current file (reset to 0 on rotation).
    frames_in_current: u64,
    /// Paths of files started so far, in write order.
    files: Vec<PathBuf>,
    /// Peak for the full recording (maximum absolute linear value).
    peak: f32,
    /// Sum of squares for the full recording (for RMS calculation).
    sum_sq: f64,
    /// Sample count for the full recording (for RMS calculation).
    samples: u64,
    /// Total frames across all files.
    total_frames: u64,
}

impl RotatingWavWriter {
    /// Create a writer from the base path, output format, and split duration (0 = no splitting).
    /// Does not open a file yet; that happens when the first chunk arrives.
    fn new(out: &Path, output: OutputFormat, split_seconds: u64) -> Self {
        let spec = hound::WavSpec {
            channels: output.channels,
            sample_rate: output.sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        Self {
            base: out.to_path_buf(),
            spec,
            // Saturating, never panicking: `run` rejects a `--split-seconds` whose product with the
            // output rate overflows u64 before this is constructed, and a saturated threshold only
            // means "no file ever reaches the boundary".
            frames_per_file: split_seconds.saturating_mul(u64::from(output.sample_rate)),
            writer: None,
            frames_in_current: 0,
            files: Vec::new(),
            peak: 0.0,
            sum_sq: 0.0,
            samples: 0,
            total_frames: 0,
        }
    }

    /// Whether this is split recording (`--split-seconds` is at least 1).
    fn is_split(&self) -> bool {
        self.frames_per_file > 0
    }

    /// Path of the next file to open. Use the base path without splitting, or a 1-based index when splitting.
    fn next_path(&self) -> PathBuf {
        if self.is_split() {
            split_file_path(&self.base, self.files.len() as u64 + 1)
        } else {
            self.base.clone()
        }
    }

    /// Write one chunk in full to the current file (never split it). If the frame count reaches the
    /// threshold, finalize immediately and rotate to the next file, where the next chunk will begin.
    /// Return the finalized file path for rotation progress display, or None if no rotation occurred.
    fn write_chunk(&mut self, chunk: &AudioChunk) -> hound::Result<Option<PathBuf>> {
        validate_output_path(&self.base)?;
        // Open the file when a chunk arrives (lazy creation).
        if self.writer.is_none() {
            let path = self.next_path();
            self.writer = Some(hound::WavWriter::create(&path, self.spec)?);
            self.files.push(path);
        }
        let writer = self.writer.as_mut().expect("writer was just opened");
        for &x in &chunk.data {
            // Accumulate statistics across the entire recording (all files combined).
            let a = x.abs();
            if a > self.peak {
                self.peak = a;
            }
            self.sum_sq += (x as f64) * (x as f64);
            self.samples += 1;

            let s = flexaudio::core::quantize_i16(x);
            writer.write_sample(s)?;
        }
        self.frames_in_current += chunk.frames as u64;
        self.total_frames += chunk.frames as u64;

        // Split boundary: finalize as soon as the threshold is reached (up to one chunk of excess is expected).
        if self.is_split() && self.frames_in_current >= self.frames_per_file {
            let writer = self.writer.take().expect("writer was just used above");
            writer.finalize()?;
            self.frames_in_current = 0;
            return Ok(self.files.last().cloned());
        }
        Ok(None)
    }

    /// Finish recording and finalize the open WAV header. If no chunks arrived, write one empty WAV
    /// as before (numbered file 1 when splitting), so the returned `files` contains at least one
    /// entry. Also return whole-recording statistics and total frames.
    fn finish(mut self) -> hound::Result<WavSummary> {
        validate_output_path(&self.base)?;
        if let Some(writer) = self.writer.take() {
            writer.finalize()?;
        } else if self.files.is_empty() {
            // No chunks arrived. Leave an empty WAV as evidence that recording ran (legacy behavior).
            let path = self.next_path();
            hound::WavWriter::create(&path, self.spec)?.finalize()?;
            self.files.push(path);
        }
        let rms = if self.samples > 0 {
            (self.sum_sq / self.samples as f64).sqrt()
        } else {
            0.0
        };
        Ok(WavSummary {
            files: self.files,
            total_frames: self.total_frames,
            stats: Stats {
                peak: self.peak,
                rms,
            },
        })
    }
}

/// Write one chunk for `run_wav`. If rotation occurs, print progress to stderr in the same style as
/// `[switch]`, and convert write errors to human-readable messages.
fn write_wav_chunk(
    writer: &mut RotatingWavWriter,
    chunk: &AudioChunk,
) -> std::result::Result<(), String> {
    match writer.write_chunk(chunk) {
        Ok(Some(done)) => {
            eprintln!(
                "[split] finalized {} (next file starts with the next chunk)",
                done.display()
            );
            Ok(())
        }
        Ok(None) => Ok(()),
        Err(e) => Err(format!("Failed to write WAV: {e}")),
    }
}

/// Format dBFS for readability (`-inf dBFS` for silence).
fn fmt_dbfs(db: f64) -> String {
    if db.is_finite() {
        format!("{db:.1} dBFS")
    } else {
        "-inf dBFS (silence)".into()
    }
}

/// Convert a [`Error`] from `flexaudio` to a human-readable message.
///
/// Replace a missing-device (`DeviceNotFound`) error with guidance to run on real hardware.
fn describe_error(err: Error) -> String {
    match err {
        Error::DeviceNotFound => {
            "The specified device/endpoint was not found. Check the ID with `--list-devices`."
                .into()
        }
        error @ Error::PermissionDenied { .. } => error.to_string(),
        Error::DeviceLost => {
            "The input device was lost during capture (for example, disconnected).".into()
        }
        other => format!("Failed to initialize stream: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build `Cli` from argument strings via clap. At minimum, put `flexaudio-cli` first.
    fn cli_from(args: &[&str]) -> Cli {
        let mut full = vec!["flexaudio-cli"];
        full.extend_from_slice(args);
        Cli::parse_from(full)
    }

    #[test]
    fn exclude_pid_parses_range_and_preserves_repeated_values() {
        let cli = cli_from(&[
            "--source",
            "system",
            "--exclude-pid",
            "1",
            "--exclude-pid",
            "4294967295",
            "--exclude-pid",
            "1",
            "--exclude-self",
        ]);
        assert_eq!(cli.exclude_pids, vec![1, u32::MAX, 1]);
        assert!(cli.exclude_self);
        assert!(cli_from(&[]).exclude_pids.is_empty());
    }

    #[test]
    fn exclude_pid_rejects_invalid_values() {
        for value in ["0", "-1", "4294967296", "abc", "1.5", "true"] {
            let argument = format!("--exclude-pid={value}");
            assert!(
                Cli::try_parse_from(["flexaudio-cli", argument.as_str()]).is_err(),
                "must reject {value}"
            );
        }
    }

    #[test]
    fn run_rejects_exclusions_without_system_capture() {
        for exclusion in [vec!["--exclude-self"], vec!["--exclude-pid", "123"]] {
            for source in ["mic", "process"] {
                let mut args = vec!["--source", source, "--process-id", "42"];
                args.extend_from_slice(&exclusion);
                let err = run(&cli_from(&args)).expect_err("reject before device access");
                assert!(err.contains("require --source system or mix"), "err: {err}");
            }
            // The schedule overrides --source, so its effective sources decide validity.
            let mut args = vec![
                "--source",
                "system",
                "--sources",
                "mic:1,process:1",
                "--process-id",
                "42",
            ];
            args.extend_from_slice(&exclusion);
            let err = run(&cli_from(&args)).expect_err("reject before device access");
            assert!(err.contains("system segment in --sources"), "err: {err}");
        }
    }

    #[test]
    fn exclusions_allow_system_mix_and_later_system_segments() {
        for source in ["system", "mix"] {
            let cli = cli_from(&["--source", source, "--exclude-self", "--exclude-pid", "123"]);
            validate_exclusion_sources(&cli, None).expect("system capture supports exclusions");
        }
        let cli = cli_from(&[
            "--sources",
            "mic:1,system:1",
            "--exclude-self",
            "--exclude-pid",
            "123",
        ]);
        let segments =
            parse_sources(cli.sources.as_deref().expect("schedule")).expect("valid schedule");
        validate_exclusion_sources(&cli, Some(&segments)).expect("later system segment suffices");
        validate_exclusion_sources(&cli_from(&[]), None).expect("no exclusions is valid");
    }

    #[test]
    fn exclusion_pid_limit_is_checked_before_device_access() {
        let mut cli = cli_from(&["--source", "system"]);
        cli.exclude_pids = vec![1; 4097];
        let err = run(&cli).expect_err("reject before device access");
        assert!(
            err.contains("exclude_pids: too many entries (max 4096)"),
            "err: {err}"
        );
    }

    #[test]
    fn exclusions_reach_shared_builder_and_scheduled_configs() {
        let cli = cli_from(&[
            "--exclude-pid",
            "123",
            "--exclude-pid",
            "123",
            "--exclude-self",
        ]);
        for kind in [
            SourceKind::Mic,
            SourceKind::SystemLoopback,
            SourceKind::ProcessLoopback,
            SourceKind::Mix,
        ] {
            let config = config_for_kind(&cli, kind);
            assert_eq!(config.exclude_pids, vec![123, 123]);
            assert!(config.exclude_self);
        }
        let segments = parse_sources("mic:1,system:1,process:1").expect("valid schedule");
        let scheduler = SwitchScheduler::new(&cli, &segments, Instant::now());
        assert_eq!(scheduler.configs.len(), 2);
        for config in &scheduler.configs {
            assert_eq!(config.exclude_pids, vec![123, 123]);
            assert!(config.exclude_self);
        }
    }

    // --- parse_sources ---

    /// Correctly parse `mic:2,system:2,process:2` into three segments (kind + duration).
    #[test]
    fn parse_sources_three_segments() {
        let segs = parse_sources("mic:2,system:2,process:2").expect("valid spec");
        assert_eq!(segs.len(), 3);
        assert_eq!(segs[0].kind, SourceKind::Mic);
        assert_eq!(segs[0].secs, 2);
        assert_eq!(segs[1].kind, SourceKind::SystemLoopback);
        assert_eq!(segs[2].kind, SourceKind::ProcessLoopback);
    }

    /// Allow whitespace and different durations; whitespace is trimmed.
    #[test]
    fn parse_sources_trims_and_varies_secs() {
        let segs = parse_sources(" mic:1 , system:5 ").expect("valid spec");
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].kind, SourceKind::Mic);
        assert_eq!(segs[0].secs, 1);
        assert_eq!(segs[1].kind, SourceKind::SystemLoopback);
        assert_eq!(segs[1].secs, 5);
    }

    /// A single segment is valid.
    #[test]
    fn parse_sources_single_segment() {
        let segs = parse_sources("mic:3").expect("valid");
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].kind, SourceKind::Mic);
        assert_eq!(segs[0].secs, 3);
    }

    /// An empty string is an error (empty spec).
    #[test]
    fn parse_sources_rejects_empty_string() {
        assert!(parse_sources("").is_err());
    }

    /// An empty segment (consecutive commas) is an error.
    #[test]
    fn parse_sources_rejects_empty_segment() {
        assert!(parse_sources("mic:2,,system:2").is_err());
    }

    /// A value that does not use `<src>:<secs>` format (missing colon) is an error.
    #[test]
    fn parse_sources_rejects_missing_colon() {
        assert!(parse_sources("mic2").is_err());
    }

    /// An unknown source name is an error.
    #[test]
    fn parse_sources_rejects_unknown_source() {
        assert!(parse_sources("foo:2").is_err());
    }

    /// A non-numeric duration is an error.
    #[test]
    fn parse_sources_rejects_non_numeric_secs() {
        assert!(parse_sources("mic:abc").is_err());
    }

    /// A duration of 0 is an error (must be at least 1).
    #[test]
    fn parse_sources_rejects_zero_secs() {
        assert!(parse_sources("mic:0").is_err());
    }

    // --- config_for_kind ---

    /// `config_for_kind` correctly copies shared CLI settings (output / pid / mode / exclude_self /
    /// device_id) into StreamConfig and overrides kind with the argument.
    #[test]
    fn config_for_kind_reflects_cli_settings() {
        let cli = cli_from(&[
            "--source",
            "process",
            "--process-id",
            "4321",
            "--mode",
            "exclude",
            "--output-rate",
            "16000",
            "--output-channels",
            "1",
            "--device-id",
            "my-mic",
            "--gain",
            "2.5",
        ]);
        let cfg = config_for_kind(&cli, SourceKind::ProcessLoopback);
        assert_eq!(cfg.kind, SourceKind::ProcessLoopback);
        assert_eq!(cfg.target_pid, Some(4321));
        assert_eq!(cfg.mode, ProcessMode::Exclude);
        assert_eq!(cfg.output.sample_rate, 16_000);
        assert_eq!(cfg.output.channels, 1);
        assert_eq!(cfg.device_id.as_deref(), Some("my-mic"));
        assert_eq!(cfg.gain, 2.5);
        // Kind is overridden by the argument (independent of the CLI's --source).
        let cfg_mic = config_for_kind(&cli, SourceKind::Mic);
        assert_eq!(cfg_mic.kind, SourceKind::Mic);
        // Other shared settings remain unchanged.
        assert_eq!(cfg_mic.output.sample_rate, 16_000);
    }

    /// `--exclude-self` is reflected in `StreamConfig.exclude_self`.
    #[test]
    fn config_for_kind_reflects_exclude_self() {
        let cli = cli_from(&["--source", "system", "--exclude-self"]);
        let cfg = config_for_kind(&cli, SourceKind::SystemLoopback);
        assert!(cfg.exclude_self);
        // Default (not specified) is false.
        let cli2 = cli_from(&["--source", "system"]);
        assert!(!config_for_kind(&cli2, SourceKind::SystemLoopback).exclude_self);
    }

    /// Default CLI (minimum arguments) produces output {48000,2} / mode Include / pid None.
    #[test]
    fn config_for_kind_defaults() {
        let cli = cli_from(&[]);
        let cfg = config_for_kind(&cli, SourceKind::Mic);
        assert_eq!(cfg.output.sample_rate, 48_000);
        assert_eq!(cfg.output.channels, 2);
        assert_eq!(cfg.mode, ProcessMode::Include);
        assert_eq!(cfg.target_pid, None);
        assert!(!cfg.exclude_self);
        assert_eq!(cfg.device_id, None);
        assert_eq!(cfg.gain, 1.0);
    }

    /// Basic behavior of `Cli::output_format` / `is_stdout_stream`.
    #[test]
    fn cli_output_format_and_stdout_detection() {
        let cli = cli_from(&["--output-rate", "8000", "--output-channels", "1"]);
        let of = cli.output_format();
        assert_eq!(of.sample_rate, 8_000);
        assert_eq!(of.channels, 1);
        assert!(!cli.is_stdout_stream());

        let cli_stream = cli_from(&["--out", "-"]);
        assert!(cli_stream.is_stdout_stream());
    }

    // --- describe_error ---

    /// Undecided consent is a warning and must not fail the capture loop.
    #[test]
    fn permission_pending_warns_and_continues_capture() {
        report_capture_event(flexaudio::Event::PermissionPending {
            permission: flexaudio::Permission::Microphone,
            detail: "Microphone permission is pending; run from Terminal to answer the prompt"
                .into(),
        })
        .expect("pending consent must not terminate capture");
    }

    #[test]
    fn runtime_permission_denial_fails_but_silence_advisory_continues() {
        for permission in [
            flexaudio::Permission::Microphone,
            flexaudio::Permission::SystemAudio,
        ] {
            let error = report_capture_event(flexaudio::Event::PermissionDenied {
                permission,
                detail: "denied by user".into(),
            })
            .expect_err("confirmed denial must fail the capture loop");
            let message = describe_error(error);
            assert!(message.contains(&permission.to_string()));
            assert!(message.contains("recording permission denied"));
            assert!(!message.contains("denied by user"));
            assert!(message.contains("Restart"));
        }
        report_capture_event(flexaudio::Event::SilenceWhileSourceActive {
            detail:
                "Recording permission may be missing; genuine digital silence can also cause this"
                    .into(),
        })
        .expect("advisory must continue capture");
    }

    /// Main Error variants are mapped to human-readable messages (one branch per variant).
    #[test]
    fn describe_error_maps_known_variants() {
        assert!(describe_error(Error::DeviceNotFound).contains("not found"));
        assert!(describe_error(Error::PermissionDenied {
            permission: flexaudio::Permission::Microphone,
            detail: "denied by user".into()
        })
        .contains("permission"));
        assert!(describe_error(Error::DeviceLost).contains("lost"));
        // Other variants include the generic message and Display output.
        let msg = describe_error(Error::Unsupported);
        assert!(msg.contains("Failed to initialize stream"));
    }

    /// The DeviceNotFound message is source-neutral (does not assume mic). It also gives useful
    /// guidance when an invalid device ID is supplied for system or process.
    #[test]
    fn describe_error_device_not_found_is_source_neutral() {
        let msg = describe_error(Error::DeviceNotFound);
        assert!(!msg.contains("microphone"));
        assert!(!msg.contains("input device"));
        // The message prompts the user to check the ID.
        assert!(msg.contains("--list-devices"));
    }

    // --- source_kind_label / truncate ---

    /// Labels are short English identifiers.
    #[test]
    fn source_kind_label_is_short() {
        assert_eq!(source_kind_label(SourceKind::Mic), "mic");
        assert_eq!(source_kind_label(SourceKind::SystemLoopback), "system");
        assert_eq!(source_kind_label(SourceKind::ProcessLoopback), "process");
        assert_eq!(source_kind_label(SourceKind::Mix), "mix");
    }

    /// Mix flags (--mic-device-id / --system-device-id / --mic-gain / --system-gain) are reflected
    /// in StreamConfig's mix_* fields. Defaults are None / 1.0.
    #[test]
    fn config_for_kind_reflects_mix_settings() {
        let cli = cli_from(&[
            "--source",
            "mix",
            "--mic-device-id",
            "mic-a",
            "--system-device-id",
            "sink-b",
            "--mic-gain",
            "0.5",
            "--system-gain",
            "2.0",
        ]);
        let cfg = config_for_kind(&cli, SourceKind::Mix);
        assert_eq!(cfg.kind, SourceKind::Mix);
        assert_eq!(cfg.mix_mic_device_id.as_deref(), Some("mic-a"));
        assert_eq!(cfg.mix_system_device_id.as_deref(), Some("sink-b"));
        assert_eq!(cfg.mix_mic_gain, 0.5);
        assert_eq!(cfg.mix_system_gain, 2.0);

        // Defaults when unspecified: no device and per-source gain 1.0.
        let cli2 = cli_from(&["--source", "mix"]);
        let cfg2 = config_for_kind(&cli2, SourceKind::Mix);
        assert_eq!(cfg2.mix_mic_device_id, None);
        assert_eq!(cfg2.mix_system_device_id, None);
        assert_eq!(cfg2.mix_mic_gain, 1.0);
        assert_eq!(cfg2.mix_system_gain, 1.0);
    }

    /// `truncate` leaves strings up to max characters unchanged and limits longer strings to max
    /// characters with an ellipsis.
    #[test]
    fn truncate_respects_char_boundary() {
        assert_eq!(truncate("abc", 5), "abc");
        // Exactly max characters are unchanged.
        assert_eq!(truncate("abcde", 5), "abcde");
        // Longer strings are limited to max characters with an ellipsis (keep = max-1).
        let t = truncate("abcdefgh", 5);
        assert_eq!(t.chars().count(), 5);
        assert!(t.ends_with('…'));
        assert!(t.starts_with("abcd"));
        // Truncate safely by char boundary for multibyte text too (without panicking).
        let jp = truncate("éøåæœ", 3);
        assert_eq!(jp.chars().count(), 3);
        assert!(jp.ends_with('…'));
    }

    #[test]
    fn output_path_rejects_directories_and_empty_stems() {
        for path in ["", ".", "..", "/"] {
            assert!(validate_output_path(Path::new(path)).is_err(), "{path:?}");
        }
        // An existing directory on every OS (a literal "/tmp" is a plain file name on Windows).
        let dir = std::env::temp_dir();
        assert!(validate_output_path(&dir).is_err(), "{dir:?}");
        assert!(validate_output_path(Path::new("recording.wav")).is_ok());
    }

    // --- split_file_path ---

    /// Insert a three-digit zero-padded index before the extension; the width grows naturally past 999.
    #[test]
    fn split_file_path_inserts_padded_index() {
        assert_eq!(
            split_file_path(Path::new("rec.wav"), 1),
            PathBuf::from("rec-001.wav")
        );
        assert_eq!(
            split_file_path(Path::new("rec.wav"), 42),
            PathBuf::from("rec-042.wav")
        );
        assert_eq!(
            split_file_path(Path::new("rec.wav"), 999),
            PathBuf::from("rec-999.wav")
        );
        // From file 1000 onward, the index grows beyond the zero-padding width.
        assert_eq!(
            split_file_path(Path::new("rec.wav"), 1000),
            PathBuf::from("rec-1000.wav")
        );
    }

    /// Preserve the parent directory and append the index for paths without an extension.
    #[test]
    fn split_file_path_keeps_parent_and_handles_no_extension() {
        assert_eq!(
            split_file_path(Path::new("/tmp/dir/rec.wav"), 3),
            PathBuf::from("/tmp/dir/rec-003.wav")
        );
        assert_eq!(
            split_file_path(Path::new("rec"), 1),
            PathBuf::from("rec-001")
        );
        // For names with multiple dots, insert the index before the final extension.
        assert_eq!(
            split_file_path(Path::new("a.b.wav"), 2),
            PathBuf::from("a.b-002.wav")
        );
    }

    // --- RotatingWavWriter ---

    /// Create a test chunk with identical interleaved samples.
    fn chunk_of(frames: usize, channels: usize, value: f32) -> AudioChunk {
        AudioChunk {
            frame_index: 0,
            data: vec![value; frames * channels],
            frames,
            pts_ns: 0,
            seq: 0,
            flags: flexaudio::core::ChunkFlags::empty(),
            dropped_before: 0,
            peak: value.abs(),
            rms: value.abs(),
        }
    }

    /// Create and return an empty temporary directory for tests (isolated by test name and safe for
    /// parallel execution).
    fn test_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("flexaudio_cli_{}_{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create test dir");
        dir
    }

    /// Read a WAV and return its frame count (= sample count / channel count).
    fn wav_frames(path: &Path) -> u64 {
        let reader = hound::WavReader::open(path).expect("open wav");
        (reader.len() / reader.spec().channels as u32) as u64
    }

    /// With splitting disabled (split 0), write one file to the base path as before and produce the
    /// correct header (rate/ch/16-bit) and peak/RMS. Read the WAV back with hound to verify.
    #[test]
    fn rotating_writer_without_split_matches_legacy_single_file() {
        let output = OutputFormat {
            sample_rate: 16_000,
            channels: 1,
        };
        let dir = test_dir("nosplit");
        let path = dir.join("capture.wav");

        // Alternating amplitudes 0.5 / -0.5 (peak=0.5, rms=0.5).
        let data: Vec<f32> = (0..320)
            .map(|i| if i % 2 == 0 { 0.5 } else { -0.5 })
            .collect();
        let chunk = AudioChunk {
            frame_index: 0,
            data,
            frames: 320,
            pts_ns: 0,
            seq: 0,
            flags: flexaudio::core::ChunkFlags::empty(),
            dropped_before: 0,
            peak: 0.5,
            rms: 0.5,
        };

        let mut writer = RotatingWavWriter::new(&path, output, 0);
        assert!(writer.write_chunk(&chunk).expect("write").is_none());
        let summary = writer.finish().expect("finish");

        // Without splitting, use the base path without an index (fully compatible).
        assert_eq!(summary.files, vec![path.clone()]);
        assert_eq!(summary.total_frames, 320);

        // Peak/RMS are known: every sample has |0.5|, so peak=0.5 and rms=0.5.
        assert!(
            (summary.stats.peak - 0.5).abs() < 1e-6,
            "peak: {}",
            summary.stats.peak
        );
        assert!(
            (summary.stats.rms - 0.5).abs() < 1e-6,
            "rms: {}",
            summary.stats.rms
        );

        // Read the header back and verify rate/ch/bits.
        let reader = hound::WavReader::open(&path).expect("open wav");
        let spec = reader.spec();
        assert_eq!(spec.sample_rate, 16_000);
        assert_eq!(spec.channels, 1);
        assert_eq!(spec.bits_per_sample, 16);
        assert_eq!(spec.sample_format, hound::SampleFormat::Int);
        assert_eq!(reader.len(), 320);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Deterministic frame-based split boundaries: writing 150 chunks of 20 ms (320 frames) at
    /// 16 kHz mono with a 1-second split threshold (16000 frames) creates exactly three files of 50
    /// chunks each, with matching total frame counts (no dropped data).
    #[test]
    fn rotating_writer_splits_exactly_on_multiple() {
        let output = OutputFormat {
            sample_rate: 16_000,
            channels: 1,
        };
        let dir = test_dir("split_exact");
        let base = dir.join("rec.wav");
        let mut writer = RotatingWavWriter::new(&base, output, 1);

        let mut rotations = Vec::new();
        for i in 0..150 {
            if let Some(done) = writer.write_chunk(&chunk_of(320, 1, 0.25)).expect("write") {
                rotations.push((i, done));
            }
        }
        // 16000 / 320 = 50, so files finalize on chunks 50, 100, and 150 (zero-based: 49, 99, 149).
        assert_eq!(
            rotations.iter().map(|(i, _)| *i).collect::<Vec<_>>(),
            vec![49, 99, 149]
        );

        let summary = writer.finish().expect("finish");
        // Recording ended exactly at the boundary, so no empty fourth file is created.
        assert_eq!(summary.files.len(), 3);
        assert_eq!(
            summary.files,
            vec![
                dir.join("rec-001.wav"),
                dir.join("rec-002.wav"),
                dir.join("rec-003.wav"),
            ]
        );
        assert_eq!(summary.total_frames, 150 * 320);

        // Read back: each file is exactly one second; total equals input (no dropped data).
        let mut read_total = 0u64;
        for f in &summary.files {
            let frames = wav_frames(f);
            assert_eq!(frames, 16_000);
            read_total += frames;
        }
        assert_eq!(read_total, summary.total_frames);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Boundaries use chunk granularity ("advance when at or above"). If the threshold is not a
    /// multiple of the chunk size, a file includes the chunk that crosses the threshold (up to one
    /// chunk of excess). Chunks are not split; the next file starts with the next chunk.
    #[test]
    fn rotating_writer_rounds_boundary_up_to_chunk() {
        // Threshold 1000 frames (1000 Hz × 1 second); chunk size 320 frames.
        // The fourth chunk reaches 1280 >= 1000 and finalizes (up to one chunk of excess is expected).
        let output = OutputFormat {
            sample_rate: 1_000,
            channels: 1,
        };
        let dir = test_dir("split_roundup");
        let base = dir.join("rec.wav");
        let mut writer = RotatingWavWriter::new(&base, output, 1);

        // Seven chunks: the first four finalize file 1; finish finalizes the remaining three
        // (960 < 1000) in file 2.
        let mut rotated_at = Vec::new();
        for i in 0..7 {
            if writer
                .write_chunk(&chunk_of(320, 1, 0.25))
                .expect("write")
                .is_some()
            {
                rotated_at.push(i);
            }
        }
        assert_eq!(rotated_at, vec![3]);

        let summary = writer.finish().expect("finish");
        assert_eq!(summary.files.len(), 2);
        // File 1 has four chunks (1280 frames, rounded up from the 1000-frame threshold); file 2 has the rest.
        assert_eq!(wav_frames(&summary.files[0]), 1280);
        assert_eq!(wav_frames(&summary.files[1]), 960);
        assert_eq!(summary.total_frames, 7 * 320);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Count boundaries in frames, not samples, for stereo too.
    #[test]
    fn rotating_writer_counts_frames_not_samples_for_stereo() {
        // Threshold is 640 frames; each stereo chunk has 320 frames (640 samples).
        // Counting samples would rotate incorrectly after the first chunk.
        let output = OutputFormat {
            sample_rate: 640,
            channels: 2,
        };
        let dir = test_dir("split_stereo");
        let base = dir.join("rec.wav");
        let mut writer = RotatingWavWriter::new(&base, output, 1);

        assert!(writer
            .write_chunk(&chunk_of(320, 2, 0.25))
            .expect("write")
            .is_none());
        assert!(writer
            .write_chunk(&chunk_of(320, 2, 0.25))
            .expect("write")
            .is_some());

        let summary = writer.finish().expect("finish");
        assert_eq!(summary.files.len(), 1);
        assert_eq!(wav_frames(&summary.files[0]), 640);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `finish` leaves one empty WAV even if no chunks arrive (preserves legacy behavior).
    /// Without splitting it uses the base path; with splitting it uses index 1.
    #[test]
    fn rotating_writer_finish_writes_empty_wav_when_no_chunks() {
        let output = OutputFormat {
            sample_rate: 16_000,
            channels: 2,
        };
        let dir = test_dir("split_empty");

        // No splitting -> capture.wav (as before).
        let base = dir.join("capture.wav");
        let summary = RotatingWavWriter::new(&base, output, 0)
            .finish()
            .expect("finish");
        assert_eq!(summary.files, vec![base.clone()]);
        assert_eq!(summary.total_frames, 0);
        assert_eq!(wav_frames(&base), 0);

        // With splitting -> rec-001.wav (empty first file).
        let base2 = dir.join("rec.wav");
        let summary2 = RotatingWavWriter::new(&base2, output, 5)
            .finish()
            .expect("finish");
        assert_eq!(summary2.files, vec![dir.join("rec-001.wav")]);
        assert_eq!(wav_frames(&dir.join("rec-001.wav")), 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Integration test that drives Stream with MockBackend: recording three seconds with a
    /// one-second split produces three files, and their total frames match the written frame count
    /// (no dropped data). Stop deterministically by counting finalized files, not wall-clock time.
    #[test]
    fn rotating_writer_splits_three_seconds_from_mock_backend() {
        use flexaudio::MockBackend;

        let output = OutputFormat {
            sample_rate: 16_000,
            channels: 1,
        };
        let config = StreamConfig {
            kind: SourceKind::Mic,
            output,
            ..Default::default()
        };
        // 48 kHz mono sine-wave source (no real hardware required). Stream converts it to 16 kHz mono.
        let backend = Box::new(MockBackend::new(48_000, 1, 440.0));
        let mut stream = Stream::open(config, backend).expect("open stream");
        stream.start().expect("start stream");

        let dir = test_dir("split_mock");
        let base = dir.join("rec.wav");
        let mut writer = RotatingWavWriter::new(&base, output, 1);

        // Feed chunks until file 3 is finalized (equivalent to writing three seconds). Write every
        // received chunk, so `fed_frames` is the expected value. Set only a maximum chunk count as
        // a safety guard (do not assert on ratios or elapsed time).
        let mut fed_frames: u64 = 0;
        let mut max_chunk_frames: u64 = 0;
        let mut finalized = 0usize;
        let mut polled_chunks = 0u64;
        while finalized < 3 {
            match stream.poll_chunk() {
                Some(chunk) => {
                    polled_chunks += 1;
                    fed_frames += chunk.frames as u64;
                    max_chunk_frames = max_chunk_frames.max(chunk.frames as u64);
                    if writer.write_chunk(&chunk).expect("write").is_some() {
                        finalized += 1;
                    }
                }
                None => thread::sleep(Duration::from_millis(5)),
            }
            assert!(
                polled_chunks < 2_000,
                "Chunk limit reached before finalizing 3 files (Stream is not producing data)"
            );
        }
        stream.stop();

        let summary = writer.finish().expect("finish");
        // Stopped exactly when file 3 finalized, so no empty fourth file is created.
        assert_eq!(summary.files.len(), 3);
        assert_eq!(
            summary.files,
            vec![
                dir.join("rec-001.wav"),
                dir.join("rec-002.wav"),
                dir.join("rec-003.wav"),
            ]
        );
        assert_eq!(summary.total_frames, fed_frames);

        // Read back: total equals written data (no dropped data). Each file is at least one second
        // and less than one chunk over (boundaries round up to chunk granularity).
        let mut read_total = 0u64;
        for f in &summary.files {
            let frames = wav_frames(f);
            assert!(
                frames >= 16_000,
                "each file has at least the split duration: {frames}"
            );
            assert!(
                frames < 16_000 + max_chunk_frames,
                "excess is less than one chunk: {frames}"
            );
            read_total += frames;
        }
        assert_eq!(
            read_total, fed_frames,
            "read-back total = written frame count"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- run: reject --out - combined with --split-seconds ---

    /// Combining stdout streaming (--out -) with --split-seconds is rejected before opening the
    /// stream with a clear error (can be checked without a device).
    #[test]
    fn run_rejects_split_seconds_with_stdout_stream() {
        let cli = cli_from(&["--out", "-", "--split-seconds", "5"]);
        let err = run(&cli).expect_err("must be rejected");
        assert!(err.contains("--split-seconds"), "err: {err}");
        assert!(err.contains("cannot be combined"), "err: {err}");
    }

    /// A recording deadline that no clock can reach (--seconds near u64::MAX) is rejected by `run`
    /// with the overflow InvalidArg *before* the stream is opened/started: the returned message is
    /// the preflight overflow error, not a device/open error, and no device is required here.
    #[test]
    fn run_rejects_duration_overflow_before_opening_stream() {
        let cli = cli_from(&["--seconds", "18446744073709551615"]);
        let err = run(&cli).expect_err("overflow must be rejected before open");
        assert!(err.contains("deadline overflows the clock"), "err: {err}");
    }

    /// s16 quantization in `write_chunk`: f32 -> i16. Uses the shared canonical `quantize_i16`
    /// (scale 32768, round, clamp, NaN->0). Negative full scale `-1.0` becomes `-32768`; values
    /// outside the range saturate.
    #[test]
    fn write_chunk_s16_quantizes_and_clamps() {
        let chunk = AudioChunk {
            frame_index: 0,
            // 0.0 / 1.0 / -1.0 / out-of-range 2.0 (-> clamp 32767) / -2.0 (-> clamp -32768).
            data: vec![0.0, 1.0, -1.0, 2.0, -2.0],
            frames: 5,
            pts_ns: 0,
            seq: 0,
            flags: flexaudio::core::ChunkFlags::empty(),
            dropped_before: 0,
            peak: 1.0,
            rms: 0.5,
        };
        let mut buf: Vec<u8> = Vec::new();
        write_chunk(&mut buf, &chunk, EncodingArg::S16).expect("write");
        // s16 LE: 2 bytes per sample × 5 = 10 bytes.
        assert_eq!(buf.len(), 10);
        let s = |i: usize| i16::from_le_bytes([buf[i * 2], buf[i * 2 + 1]]);
        assert_eq!(s(0), 0); // 0.0
        assert_eq!(s(1), 32767); // 1.0 → clamp 32767
        assert_eq!(s(2), -32768); // -1.0 -> negative full scale -32768
        assert_eq!(s(3), 32767); // 2.0 → clamp 32767
        assert_eq!(s(4), -32768); // -2.0 → clamp -32768
    }

    /// f32 path in `write_chunk`: byte length is sample count × 4 and little-endian decoding matches.
    #[test]
    fn write_chunk_f32_roundtrips() {
        let chunk = AudioChunk {
            frame_index: 0,
            data: vec![0.25, -0.5, 0.75],
            frames: 3,
            pts_ns: 0,
            seq: 0,
            flags: flexaudio::core::ChunkFlags::empty(),
            dropped_before: 0,
            peak: 0.75,
            rms: 0.5,
        };
        let mut buf: Vec<u8> = Vec::new();
        write_chunk(&mut buf, &chunk, EncodingArg::F32).expect("write");
        assert_eq!(buf.len(), 12); // 3 samples × 4 bytes.
        let f = |i: usize| {
            f32::from_le_bytes([buf[i * 4], buf[i * 4 + 1], buf[i * 4 + 2], buf[i * 4 + 3]])
        };
        assert_eq!(f(0), 0.25);
        assert_eq!(f(1), -0.5);
        assert_eq!(f(2), 0.75);
    }

    /// `fmt_dbfs`: finite values use dBFS notation; infinity is displayed as silence.
    #[test]
    fn fmt_dbfs_finite_and_infinite() {
        assert!(fmt_dbfs(-6.0).contains("dBFS"));
        assert!(fmt_dbfs(f64::NEG_INFINITY).contains("silence"));
    }
}

#[cfg(test)]
mod reproduction_tests {
    use super::*;
    fn cli_from(args: &[&str]) -> Cli {
        let mut full = vec!["flexaudio-cli"];
        full.extend_from_slice(args);
        Cli::try_parse_from(full).unwrap()
    }
    fn test_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("flexaudio-repro-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
    // Offline audit reproductions. These exercise the existing CLI paths without devices.
    struct ReproFinalEventBackend {
        inner: flexaudio::MockBackend,
        stopped: bool,
        final_event: Option<flexaudio::Event>,
    }

    impl flexaudio::core::CaptureBackend for ReproFinalEventBackend {
        fn native_format(&self) -> (u32, u16) {
            (48_000, 1)
        }
        fn start(&mut self, sink: flexaudio::core::RawSink) -> flexaudio::core::Result<()> {
            self.inner.start(sink)
        }
        fn stop(&mut self) {
            self.inner.stop();
            self.stopped = true;
        }
        fn poll_event(&mut self) -> Option<flexaudio::Event> {
            if self.stopped {
                self.final_event.take()
            } else {
                None
            }
        }
    }

    fn repro_wav_capture(final_event: Option<flexaudio::Event>) -> std::result::Result<(), String> {
        let name = match &final_event {
            Some(flexaudio::Event::Error(_)) => "repro_final_legacy",
            Some(_) => "repro_final_typed",
            None => "repro_final_control",
        };
        let dir = test_dir(name);
        let mut cli = cli_from(&["--seconds", "1"]);
        cli.out = dir.join("rec.wav");
        let config = StreamConfig::default();
        let output = config.output;
        let backend = ReproFinalEventBackend {
            inner: flexaudio::MockBackend::new(48_000, 1, 440.0),
            stopped: false,
            final_event,
        };
        let mut stream = Stream::open(config, Box::new(backend)).expect("mock open");
        stream.start().expect("mock start");
        let result = run_wav(&cli, &mut stream, output, None);
        stream.stop();
        std::fs::remove_dir_all(dir).expect("remove test output");
        result
    }

    #[test]
    #[ignore = "repro: C F48"]
    fn repro_p12_f48_legacy_fatal_final_event() {
        let result = repro_wav_capture(Some(flexaudio::Event::Error(
            "normalizer push failed: injected fatal DSP failure".into(),
        )));
        assert!(
            result.is_err(),
            "fatal final event was reported but recording returned success"
        );
    }

    #[test]
    fn repro_p12_f48_control_and_typed_terminal() {
        assert!(repro_wav_capture(None).is_ok());
        let result = repro_wav_capture(Some(flexaudio::Event::TerminalError {
            error: Error::Backend("injected terminal failure".into()),
        }));
        assert!(result
            .expect_err("typed final error must fail")
            .contains("injected terminal failure"));
    }

    #[test]
    fn repro_p12_f50_split_overflow() {
        let cli = cli_from(&["--split-seconds", "18446744073709551615"]);
        let result = std::panic::catch_unwind(|| {
            RotatingWavWriter::new(
                Path::new("unused.wav"),
                cli.output_format(),
                cli.split_seconds,
            )
        });
        assert!(result.is_ok(), "accepted split duration must not panic");
    }

    #[test]
    fn repro_p12_f50_deadline_overflow() {
        let cli = cli_from(&["--seconds", "18446744073709551615"]);
        let config = StreamConfig::default();
        let output = config.output;
        let mut stream = Stream::open(
            config,
            Box::new(flexaudio::MockBackend::new(48_000, 1, 0.0)),
        )
        .expect("mock open");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_wav(&cli, &mut stream, output, None)
        }));
        stream.stop();
        assert!(result.is_ok(), "accepted recording duration must not panic");
    }

    #[test]
    fn repro_p12_f50_control() {
        let cli = cli_from(&["--split-seconds", "1"]);
        let writer = RotatingWavWriter::new(
            Path::new("unused.wav"),
            cli.output_format(),
            cli.split_seconds,
        );
        assert_eq!(writer.frames_per_file, u64::from(cli.output_rate));
        assert!(Instant::now()
            .checked_add(Duration::from_secs(cli.seconds))
            .is_some());
    }

    #[test]
    fn repro_p12_split_path_and_permission_controls() {
        assert!(validate_output_path(Path::new(".")).is_err());
        assert!(validate_output_path(Path::new("recording.wav")).is_ok());
        let message = describe_error(Error::PermissionDenied {
            permission: flexaudio::Permission::SystemAudio,
            detail: "target process access restricted".into(),
        });
        assert!(message.contains("system audio"));
        assert!(message.contains("target process access restricted"));
        assert!(!message.contains("Microphone permission denied"));
    }
}
