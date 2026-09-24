//! flexaudio-cli — reference capture CLI.
//!
//! Captures N seconds from the default microphone (or another source), collects chunks in the
//! output format (default 48000 Hz / stereo 2ch / interleaved f32) and writes them to a 16-bit
//! PCM WAV. It also prints a summary of peak / RMS (dBFS) / chunk count / drop count.
//!
//! The output format is set with `--output-rate <Hz>` (default 48000) and
//! `--output-channels <1|2>` (default 2). The audio is re-converted from the internal canonical
//! format (48k/stereo) by the second-stage resampler (rubato, anti-aliasing included), and the
//! rate/ch of the WAV header and of stdout follow it. Chunks are fixed at 20ms of time, so the
//! number of frames per chunk depends on the rate (48k=960 / 16k=320).
//!
//! ```text
//! flexaudio-cli --source mic --seconds 5 --out mic.wav
//! flexaudio-cli --source system --output-rate 16000 --output-channels 1 --out 16k.wav --seconds 3
//! ```
//!
//! `--device-id <ID>` selects the device (the ID is the ID column of `--list-devices`). For mic
//! it selects the input device; for system it selects the output endpoint. If omitted, the
//! default is used (mic=default input / system=default output). process determines its target
//! with `--process-id`, so device_id is ignored.
//!
//! The process and system concepts are split into separate flags (they are not mixed):
//! - `--mode include|exclude` (process only, default include): include=record only the target
//!   PID / exclude=record all system audio except the target PID (`--process-id` required).
//! - `--exclude-self` (system only): removes the host process's own playback from the system
//!   audio (feedback prevention).
//!   The process source ignores `--exclude-self`, and the system source ignores `--mode`.
//!
//! ```text
//! flexaudio-cli --list-devices
//! flexaudio-cli --list-processes
//! flexaudio-cli --source mic --device-id "Stereo Mix (Realtek(R) Audio)" --out cap.wav
//! ```
//!
//! With `--out -`, instead of a WAV, headerless raw PCM is streamed to stdout (binary) as soon
//! as each chunk arrives. The receiver (e.g. a host app that runs `spawn('flexaudio-cli', ...)`
//! and reads stdout) can receive the audio in real time. `--encoding f32|s16` selects the
//! sample format. In this mode stdout is dedicated to PCM bytes, and logs such as the summary go
//! to stderr. `--seconds 0` streams forever (stopped by a broken pipe / Ctrl-C). The rate/ch of
//! the raw PCM also follow the output format (match the receiver's `-r/-c`).
//!
//! ```text
//! flexaudio-cli --source system --out - --encoding s16 --seconds 0 | aplay -f S16_LE -r 48000 -c 2
//! flexaudio-cli --source system --out - --encoding s16 --output-rate 16000 --output-channels 1 --seconds 0 | aplay -f S16_LE -r 16000 -c 1
//! ```
//!
//! `--split-seconds <N>` (default 0 = no splitting) splits the WAV recording into numbered files
//! of N seconds each. With `--out rec.wav` they are `rec-001.wav, rec-002.wav, ...` (a 3-digit
//! zero-padded number before the extension; from the 1000th file on the number simply gets more
//! digits). The boundary has chunk granularity (20ms): "move to the next file once the frames
//! written reach `N × output sample rate`", so each file can be up to 1 chunk (±20ms) longer
//! than specified. Chunks are never split and nothing is dropped (the next file starts with the
//! next chunk). Since it is based on frame counts, it works orthogonally with `--sources`
//! (hot-swap) and mix. It cannot be combined with stdout streaming (`--out -`) (error at
//! startup).
//!
//! ```text
//! flexaudio-cli --source mic --seconds 30 --split-seconds 10 --out rec.wav
//! ```
//!
//! In environments without an input device (servers, CI, etc.) real capture is not possible;
//! the CLI prints a clear message and exits with a non-zero status (it does not panic).

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

/// Kind of source to capture (for the CLI argument).
#[derive(Debug, Clone, Copy, ValueEnum)]
enum SourceArg {
    /// Default microphone input.
    Mic,
    /// System output loopback (Linux / Windows / macOS).
    System,
    /// Process output loopback (Linux / Windows / macOS, `--process-id <PID>` required).
    Process,
    /// Mix of microphone + system audio (Linux / Windows / macOS).
    /// Devices are set with `--mic-device-id` / `--system-device-id`, and per-side gains with
    /// `--mic-gain` / `--system-gain`.
    Mix,
}

/// How the target PID of `--source process` is handled (process only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ModeArg {
    /// Record only the target PID (and its tree) (default).
    Include,
    /// Record all system audio except the target PID (and its tree) (`--process-id` required).
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
    /// interleaved f32 little-endian (the internal canonical format as is).
    F32,
    /// interleaved i16 little-endian. Compatible with external tools such as `aplay -f S16_LE`.
    S16,
}

/// flexaudio capture CLI.
#[derive(Debug, Parser)]
#[command(name = "flexaudio-cli", about = "flexaudio capture CLI")]
struct Cli {
    /// Do not record; list the available audio devices and exit
    /// (unified enumeration of `devices()`. Works independently of `--source` etc.).
    #[arg(long)]
    list_devices: bool,

    /// Do not record; list the processes that have an audio output session (stream) and can
    /// therefore be targets of per-process capture (`--source process --process-id <PID>`), then
    /// exit (`processes()`. Stopped/Idle ones are listed too. Works independently of `--source`
    /// etc.).
    #[arg(long)]
    list_processes: bool,

    /// Do not record; watch device hotplug (attach/detach) and keep printing it to stderr
    /// (`watch_devices()`. Stop with Ctrl-C. Works independently of `--source` etc.).
    #[arg(long)]
    watch_devices: bool,

    /// Source to capture (mic / system / process).
    #[arg(long, value_enum, default_value_t = SourceArg::Mic)]
    source: SourceArg,

    /// Schedule for seamlessly hot-swapping the source during recording.
    /// A comma-separated list of `<src>:<secs>` (e.g. `mic:2,system:2,process:2`).
    /// Each segment is recorded for the given seconds, then `switch_source` switches to the next
    /// source. The destination (file/pipe) stays a single one, producing a single continuous
    /// chunk stream. When given, it overrides `--source` / `--seconds` (the first segment is the
    /// initial source, and the sum of the secs is the total recording time). If it contains
    /// `process`, `--process-id` is required. At each switch boundary `[switch] -> <kind>` is
    /// printed to stderr. If a switch fails, a warning is printed and recording continues (with
    /// the old source). For WAV output each secs must be 1 or more.
    /// `--mode` / `--exclude-self` apply uniformly to all segments; only `--mode` takes effect
    /// for process segments and only `--exclude-self` for system segments.
    #[arg(long)]
    sources: Option<String>,

    /// Target process PID for `--source process` (Linux / Windows / macOS, required for
    /// process). Records a copy by fan-out linking to the target PID's app output nodes.
    /// Non-invasive: the user's speakers keep playing. It is normal for the target to start
    /// playing only later.
    #[arg(long)]
    process_id: Option<u32>,

    /// ID of the device to select (copy it from the ID column of `--list-devices`). For mic it
    /// selects the input device; for system it selects the output endpoint. If omitted, the
    /// default is used (mic=default input / system=default output). process determines its
    /// target with `--process-id`, so this value is ignored. If no device matches, it exits with
    /// DeviceNotFound instead of crashing.
    #[arg(long)]
    device_id: Option<String>,

    /// How the target PID of `--source process` is handled (process only, default include).
    /// `include`=record only the target PID / `exclude`=record all system audio except the
    /// target PID (`--process-id` required, supported on Linux / Windows / macOS). Ignored for
    /// mic / system. To exclude a target, use `--mode exclude` (it is not meant for excluding
    /// the host process itself).
    #[arg(long, value_enum, default_value_t = ModeArg::Include)]
    mode: ModeArg,

    /// Remove the host process's own playback from the system audio (system only, prevents
    /// feedback loops, supported on Linux / Windows / macOS). Only takes effect with
    /// `--source system`; ignored for mic / process. To exclude a target PID, use
    /// `--mode exclude`.
    #[arg(long, default_value_t = false)]
    exclude_self: bool,

    /// Capture duration in seconds. `0` streams forever (intended for `--out -`; stopped by
    /// Ctrl-C / a broken pipe).
    #[arg(long, default_value_t = 5)]
    seconds: u64,

    /// Destination. A file path writes a WAV; `-` streams raw PCM to stdout.
    #[arg(long, default_value = "capture.wav")]
    out: PathBuf,

    /// Seconds per file for split WAV recording. Default 0 = no splitting (a single file, as
    /// before). When 1 or more, each time the frames written reach
    /// `split-seconds × output sample rate`, the current file is finalized (WAV header
    /// finalized) and recording switches to the next numbered file
    /// (`--out rec.wav` gives `rec-001.wav, rec-002.wav, ...`). The boundary has chunk
    /// granularity (20ms), "move to the next file once reached or exceeded", so each file can be
    /// up to 1 chunk longer than specified. Chunks are never split and nothing is dropped (the
    /// next file starts with the next chunk). Cannot be combined with stdout streaming
    /// (`--out -`).
    #[arg(long, default_value_t = 0)]
    split_seconds: u64,

    /// Sample format for stdout streaming (`--out -` only; ignored for WAV output).
    #[arg(long, value_enum, default_value_t = EncodingArg::F32)]
    encoding: EncodingArg,

    /// Output sample rate (Hz). Default 48000. E.g. `--output-rate 16000` downsamples to
    /// 16kHz. The sample rate of the WAV header and of stdout follows it.
    #[arg(long, default_value_t = 48_000)]
    output_rate: u32,

    /// Number of output channels (1 = mono / 2 = stereo). Default 2. stereo→mono is the L/R
    /// average.
    #[arg(long, default_value_t = 2)]
    output_channels: u16,

    /// Input gain (linear multiplier). Default 1.0. 1.0 leaves the audio unchanged, 2.0 is about
    /// +6dB, 0.0 is silence. Samples are clamped to ±1.0 after multiplication. Negative values
    /// and NaN are errors.
    #[arg(long, default_value_t = 1.0)]
    gain: f32,

    /// ID of the input device selected for the mic side of `--source mix` (mix only; copy it
    /// from the ID column of `--list-devices`). If omitted, the default input is used. Ignored
    /// for mic / system / process.
    #[arg(long)]
    mic_device_id: Option<String>,

    /// ID of the output endpoint selected for the system side of `--source mix` (mix only).
    /// If omitted, the default output is used. Ignored for mic / system / process.
    #[arg(long)]
    system_device_id: Option<String>,

    /// Pre-mix multiplier for the mic side of `--source mix` (linear, mix only). Default 1.0.
    /// `--gain` is applied after mixing. Negative values and NaN are errors.
    #[arg(long, default_value_t = 1.0)]
    mic_gain: f32,

    /// Pre-mix multiplier for the system side of `--source mix` (linear, mix only). Default
    /// 1.0. Negative values and NaN are errors.
    #[arg(long, default_value_t = 1.0)]
    system_gain: f32,
}

impl Cli {
    /// Whether this is `--out -` (a single hyphen). If true, stream raw PCM to stdout.
    fn is_stdout_stream(&self) -> bool {
        self.out == Path::new("-")
    }

    /// Builds the [`OutputFormat`] from the CLI arguments.
    fn output_format(&self) -> OutputFormat {
        OutputFormat {
            sample_rate: self.output_rate,
            channels: self.output_channels,
        }
    }
}

/// One segment of `--sources`: the source to switch to and its duration in seconds.
#[derive(Debug, Clone, Copy)]
struct Segment {
    kind: SourceKind,
    secs: u32,
}

/// Parses `--sources "mic:2,system:2,process:2"` into a `Vec<Segment>`.
///
/// Each element is `<src>:<secs>`. `<src>` is `mic|system|process` and `<secs>` is a positive
/// integer (seconds). Empty or invalid elements and sources unsupported on the OS are errors (a
/// human-readable `String`). A system/process entry on a non-Linux OS is also rejected here.
fn parse_sources(spec: &str) -> std::result::Result<Vec<Segment>, String> {
    let mut segments = Vec::new();
    for (idx, raw) in spec.split(',').enumerate() {
        let item = raw.trim();
        if item.is_empty() {
            return Err(format!(
                "element {} of --sources is empty (format: <src>:<secs>, e.g. mic:2)",
                idx + 1
            ));
        }
        let (src, secs_str) = item.split_once(':').ok_or_else(|| {
            format!("element {item:?} of --sources must be in the form <src>:<secs> (e.g. mic:2)")
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
                        "system (system output loopback) in --sources is currently supported only on Linux / Windows / macOS."
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
                        "process (process output loopback) in --sources is currently supported only on Linux / Windows / macOS."
                            .into(),
                    );
                }
            }
            other => {
                return Err(format!(
                    "unknown source {other:?} in --sources (must be one of mic|system|process)"
                ))
            }
        };
        let secs: u32 = secs_str.trim().parse().map_err(|_| {
            format!("the seconds {secs_str:?} in --sources must be a positive integer (e.g. mic:2)")
        })?;
        if secs == 0 {
            return Err(format!(
                "the seconds in --sources must be 1 or more (element {item:?})"
            ));
        }
        segments.push(Segment { kind, secs });
    }
    if segments.is_empty() {
        return Err("--sources is empty (e.g.: mic:2,system:2)".into());
    }
    Ok(segments)
}

/// Builds a [`StreamConfig`] from the given [`SourceKind`] and the CLI's shared settings
/// (output / pid / exclude_self). Used to generate the config of each `--sources` segment.
fn config_for_kind(cli: &Cli, kind: SourceKind) -> StreamConfig {
    StreamConfig {
        kind,
        output: cli.output_format(),
        target_pid: cli.process_id,
        // mode only takes effect for process segments (the facade ignores it for mic/system).
        mode: cli.mode.into(),
        // exclude_self only takes effect for system segments (ignored for mic/process).
        exclude_self: cli.exclude_self,
        // device_id takes effect for mic (input) and system (output endpoint) (the facade
        // ignores it for process). Putting it uniformly on all segments lets the relevant
        // segments pick it up.
        device_id: cli.device_id.clone(),
        // gain does not change on a switch (core ignores it), but keep it in line with the
        // initial config.
        gain: cli.gain,
        // mix only (the facade ignores these for segments other than mix).
        mix_mic_device_id: cli.mic_device_id.clone(),
        mix_system_device_id: cli.system_device_id.clone(),
        mix_mic_gain: cli.mic_gain,
        mix_system_gain: cli.system_gain,
        ..Default::default()
    }
}

/// Hot-swap scheduler for `--sources`.
///
/// The first segment is the initial source (already opened/started). When a subsequent segment
/// boundary (cumulative seconds) is reached, `stream.switch_source()` swaps to the next source.
/// The caller keeps the destination (file/pipe) as a single one, so switches are reflected
/// transparently in a single continuous chunk stream (the Stream layer guarantees seq/PTS
/// continuity).
///
/// Calling [`tick`](Self::tick) on every iteration of the collection loop performs all switches
/// whose boundary time has passed. A failed switch is warned about with `eprintln!` and
/// recording continues (with the old source). At each boundary `[switch] -> <kind>` is printed
/// to stderr.
struct SwitchScheduler {
    /// Index of the next segment to switch to (1-based. The first is the initial source and is
    /// not a switch target).
    next: usize,
    /// Absolute time of each boundary (`deadlines[i]` = the time to switch to segment `i+1`).
    deadlines: Vec<Instant>,
    /// Config to switch to (`configs[i]` = the config switched to at `deadlines[i]`).
    configs: Vec<StreamConfig>,
    /// Display labels (`labels[i]` = the kind label of `configs[i]`).
    labels: Vec<&'static str>,
}

impl SwitchScheduler {
    /// Builds the scheduler from the segment plan, relative to `start`.
    /// The first segment is the initial source, so it is not included as a switch target.
    fn new(cli: &Cli, segments: &[Segment], start: Instant) -> Self {
        let mut deadlines = Vec::new();
        let mut configs = Vec::new();
        let mut labels = Vec::new();
        let mut cumulative = 0u64;
        for (i, seg) in segments.iter().enumerate() {
            cumulative += seg.secs as u64;
            // The end of the last segment is the "total recording time", not a switch boundary.
            // End of segment i = time to switch to segment i+1 (only if i+1 exists).
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

    /// Total recording time (sum of all segment seconds). Used for the collection loop's
    /// deadline.
    fn total_duration(segments: &[Segment]) -> Duration {
        let total: u64 = segments.iter().map(|s| s.secs as u64).sum();
        Duration::from_secs(total)
    }

    /// Performs all switches for boundaries reached by `now`. Failures are warned about and
    /// execution continues.
    fn tick(&mut self, stream: &mut Stream, now: Instant) {
        while self.next < self.deadlines.len() && now >= self.deadlines[self.next] {
            let label = self.labels[self.next];
            let config = self.configs[self.next].clone();
            match stream.switch_source(config) {
                Ok(()) => {
                    eprintln!("[switch] -> {label}");
                }
                Err(e) => {
                    eprintln!(
                        "[switch] warning: failed to switch to {label} (recording continues): {e}"
                    );
                }
            }
            self.next += 1;
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    // Errors from main always go to stderr (so stdout is not polluted during stdout streaming).
    match run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("error: {msg}");
            ExitCode::FAILURE
        }
    }
}

/// The actual processing. Failures are returned as a human-readable message (`String`).
fn run(cli: &Cli) -> std::result::Result<(), String> {
    // Device list mode (enumerate without recording and exit). Handled first, independently of
    // `--source` etc.
    if cli.list_devices {
        return list_devices();
    }

    // Process list mode (enumerate without recording and exit). Also independent of `--source`
    // etc.
    if cli.list_processes {
        return list_processes();
    }

    // Device hotplug watch mode (keep watching without recording). Also independent of
    // `--source` etc.
    if cli.watch_devices {
        return watch_devices_loop();
    }

    let stdout_stream = cli.is_stdout_stream();

    // --split-seconds is for WAV file output only. stdout streaming (--out -) has no "file"
    // boundaries and cannot be split, so reject it here before opening the stream.
    if stdout_stream && cli.split_seconds > 0 {
        return Err(
            "--split-seconds (split recording) is for WAV file output only. It cannot be \
             combined with --out - (raw PCM streaming to stdout). Specify a file path for --out."
                .into(),
        );
    }

    // Switch the log destination. During stdout streaming all logs go to stderr (stdout is for
    // PCM only); for file output a summary on stdout is fine. The println!/eprintln! below go
    // through this.
    macro_rules! log {
        ($($arg:tt)*) => {
            if stdout_stream {
                eprintln!($($arg)*);
            } else {
                println!($($arg)*);
            }
        };
    }

    // Resolve --sources (hot-swap schedule). When given, it overrides --source / --seconds and
    // the first segment becomes the initial source. If it contains process, --process-id is
    // required (build_backend via switch_source would fail on the missing PID, so reject it
    // here first).
    let segments: Option<Vec<Segment>> = match &cli.sources {
        None => None,
        Some(spec) => {
            let segs = parse_sources(spec)?;
            let needs_pid = segs.iter().any(|s| s.kind == SourceKind::ProcessLoopback);
            if needs_pid && cli.process_id.is_none() {
                return Err(
                    "--process-id <PID> is required when --sources contains process.".into(),
                );
            }
            Some(segs)
        }
    };

    // Resolve the SourceKind and display label from the source kind. Building and selecting the
    // backend is done by the facade `flexaudio::open` (it picks a Box<dyn CaptureBackend>
    // internally and returns a Stream). The CLI side only decides the SourceKind and display
    // label and does the human-oriented pre-checks (PID required for process, rejecting
    // system/process on non-Linux). When --sources is given, the kind of the first segment is
    // used as the initial source.
    let (kind, source_label): (SourceKind, &str) = if let Some(segs) = &segments {
        let first = segs[0].kind;
        let label = match first {
            SourceKind::Mic => "mic (default input device)",
            SourceKind::SystemLoopback => "system (loopback of the default output)",
            SourceKind::ProcessLoopback => "process (output of the given PID)",
            SourceKind::Mix => "mix (microphone + system audio mixed)",
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
                        "system (loopback of the default output)",
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
                // process requires a PID. Without one, stop with a clear error (the facade also
                // returns InvalidArg, but reject it here first with human-oriented wording).
                // This check is OS-independent.
                if cli.process_id.is_none() {
                    return Err("--source process requires --process-id <PID>. \
                     (Specify the PID of the target process, e.g. the PID obtained by \
                     running speaker-test)"
                        .into());
                }
                #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
                {
                    (
                        SourceKind::ProcessLoopback,
                        "process (output of the given PID)",
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
                // The system-side loopback is required, so the supported OSes are the same as
                // for system.
                #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
                {
                    (SourceKind::Mix, "mix (microphone + system audio mixed)")
                }
                #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
                {
                    return Err(
                    "--source mix (mix of microphone + system audio) is currently supported only on Linux / Windows / macOS."
                        .into(),
                );
                }
            }
        }
    };

    // --- Resolve and validate the output format ---
    let output = cli.output_format();
    output.validate().map_err(|e| {
        format!(
            "output format {}Hz/{}ch is not supported: {e}",
            output.sample_rate, output.channels
        )
    })?;
    let out_rate = output.sample_rate;
    let out_ch = output.channels;

    // Open the stream. open picks a Box<dyn CaptureBackend> internally according to
    // config.kind and returns it. Not started yet (two-step scheme). native_format is taken from
    // the opened Stream.
    let config = StreamConfig {
        kind,
        output,
        target_pid: cli.process_id,
        // mode is process only. include by default.
        mode: cli.mode.into(),
        // exclude_self is the system-only exclusion of the own host process.
        exclude_self: cli.exclude_self,
        // device_id selects mic (input) and system (output endpoint) (the facade ignores it
        // for process).
        device_id: cli.device_id.clone(),
        // Input gain at start (linear multiplier). open rejects invalid values with InvalidArg.
        gain: cli.gain,
        // mix-only device selection and per-side gains (the facade ignores them for anything
        // other than mix; open rejects invalid per-side gains with InvalidArg).
        mix_mic_device_id: cli.mic_device_id.clone(),
        mix_system_device_id: cli.system_device_id.clone(),
        mix_mic_gain: cli.mic_gain,
        mix_system_gain: cli.system_gain,
        ..Default::default()
    };
    let mut stream = flexaudio::open(config).map_err(describe_error)?;

    // --- Show the native format ---
    let (native_rate, native_ch) = stream.native_format();
    log!("Source             : {source_label}");
    // When device_id is given, show the selected device explicitly (valid for mic and system;
    // ignored for process / mix).
    if let Some(id) = &cli.device_id {
        match kind {
            SourceKind::ProcessLoopback => {
                log!("Device ID          : {id} (note: ignored for process)");
            }
            SourceKind::Mix => {
                log!(
                    "Device ID          : {id} (note: ignored for mix; \
                     use --mic-device-id / --system-device-id)"
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
            "Schedule           : {} ({total} s total, one continuous stream)",
            plan.join(" -> ")
        );
    } else if cli.seconds == 0 {
        log!("Capture duration   : infinite (stopped by Ctrl-C / a broken pipe)");
    } else {
        log!("Capture duration   : {} s", cli.seconds);
    }
    if stdout_stream {
        log!("Output             : stdout (raw PCM streaming)");
    } else if cli.split_seconds > 0 {
        // With split recording no file is created at the base path itself, so show the actual
        // numbered names.
        log!(
            "Output path        : {}, {}, ... (split every {} s)",
            split_file_path(&cli.out, 1).display(),
            split_file_path(&cli.out, 2).display(),
            cli.split_seconds
        );
    } else {
        log!("Output path        : {}", cli.out.display());
    }
    log!("");

    // --- Start capture (two-step scheme: start the already opened Stream) ---
    stream.start().map_err(describe_error)?;

    log!("Capturing ...");

    if stdout_stream {
        run_stdout_stream(cli, &mut stream, segments.as_deref())
    } else {
        run_wav(cli, &mut stream, output, segments.as_deref())
    }
}

/// `--list-devices`: gets the devices with `devices()` and prints them as a table.
///
/// Columns: SOURCE (mic/system/process) / LOOPBACK / DEFAULT / RATE / CH / NAME / ID.
/// id is the device name (cpal) or node.name (PipeWire). In environments without devices it
/// says so (not an error).
fn list_devices() -> std::result::Result<(), String> {
    let devices = flexaudio::devices().map_err(|e| format!("failed to enumerate devices: {e}"))?;

    if devices.is_empty() {
        println!("No available audio devices were found.");
        println!(
            "(Run this in an environment with audio devices. \
             Enumerating system on Linux requires a PipeWire session.)"
        );
        return Ok(());
    }

    println!("Available audio devices: {}", devices.len());
    println!();
    // Header. Aligned with fixed widths (the variable-length id/name come last).
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
        "(A * in DEFAULT marks the OS default device. The ID is a stable key that can be used \
         to select the device with `--device-id <ID>`. For mic it is the input device, for \
         system the output endpoint. Ignored for process.)"
    );
    Ok(())
}

/// `--list-processes`: gets the recordable processes with `processes()` and prints them as a
/// table.
///
/// Columns: ACTIVE (`*` if outputting, `?` if unknown) / PID / NAME / EXECUTABLE / BUNDLE.
/// The PID can be passed to `--source process --process-id <PID>`. In environments with no
/// candidates it says so, and in environments where per-process capture itself is unavailable
/// it is an error.
fn list_processes() -> std::result::Result<(), String> {
    let processes = flexaudio::processes().map_err(|e| {
        format!(
            "failed to enumerate processes (per-process capture is not available in this \
             environment): {e}"
        )
    })?;

    if processes.is_empty() {
        println!("There are currently no processes with an audio output session (stream).");
        println!("(Per-process capture is available here. The list includes stopped/Idle ones,");
        println!(
            "  so empty means there is no output session at all, not \"nothing is playing\".)"
        );
        return Ok(());
    }

    println!("Recordable processes: {}", processes.len());
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
        "(A * in ACTIVE means outputting; ? means the OS does not expose the state. The PID \
         can be passed to `--source process --process-id <PID>`.)"
    );
    Ok(())
}

/// `--watch-devices`: watches device hotplug (attach/detach) with `watch_devices()` and keeps
/// printing the events to stderr (stop with Ctrl-C).
///
/// stdout is kept free for future machine-readable output, so all logs and events go to
/// stderr. At startup the existing devices are counted with `devices()` and the count is shown.
///
/// Display format (all on stderr):
/// - `[+] ADDED   <source> <name> (<id>)` — device added
/// - `[-] REMOVED <id>` — device removed (id = node.name only)
/// - `[*] DEFAULT <source> -> <id>` — default device changed
fn watch_devices_loop() -> std::result::Result<(), String> {
    use flexaudio::core::DeviceEvent;

    // At startup, count the existing devices and report (stderr). An enumeration failure is not
    // fatal.
    let existing = flexaudio::devices().map(|d| d.len()).unwrap_or(0);
    eprintln!("Started watching device hotplug ({existing} existing). Press Ctrl-C to stop.");
    eprintln!();

    // Clear the running flag on Ctrl-C (SIGINT).
    let running = Arc::new(AtomicBool::new(true));
    {
        let r = running.clone();
        ctrlc::set_handler(move || {
            r.store(false, Ordering::SeqCst);
        })
        .map_err(|e| format!("failed to register the Ctrl-C handler: {e}"))?;
    }

    // Start watching. Even without PipeWire etc. it degrades to Ok (hotplug events just never
    // come).
    let mut watcher =
        flexaudio::watch_devices().map_err(|e| format!("failed to start watching devices: {e}"))?;

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
                    eprintln!("[*] DEFAULT {:<7} -> {}", source_kind_label(kind), id,);
                }
                // DeviceEvent is #[non_exhaustive]. In preparation for future variants, unknown
                // kinds are also printed with their debug representation (not swallowed).
                other => {
                    eprintln!("[?] UNKNOWN  {other:?}");
                }
            }
        }
        // Hotplug is infrequent. Sleep moderately to avoid spinning (100ms responsiveness is
        // enough).
        thread::sleep(Duration::from_millis(100));
    }

    watcher.stop();
    eprintln!();
    eprintln!("Stopped watching device hotplug (Ctrl-C).");
    Ok(())
}

/// Converts a [`SourceKind`] into a short label for CLI display.
fn source_kind_label(kind: SourceKind) -> &'static str {
    match kind {
        SourceKind::Mic => "mic",
        SourceKind::SystemLoopback => "system",
        SourceKind::ProcessLoopback => "process",
        SourceKind::Mix => "mix",
    }
}

/// Truncates a string to `max` characters (in chars) for display (the excess becomes `…`).
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

/// WAV output path (the original behavior). Collects for N seconds (>0), writes a 16-bit WAV and
/// prints a summary to stdout.
/// `output` is the output format (used for the WAV header's rate/ch and for computing the
/// seconds recorded).
fn run_wav(
    cli: &Cli,
    stream: &mut Stream,
    output: OutputFormat,
    segments: Option<&[Segment]>,
) -> std::result::Result<(), String> {
    // Total recording time: the sum of the segments when --sources is given, otherwise
    // --seconds. On the WAV path --seconds 0 would keep accumulating forever, so it is rejected
    // (each secs of --sources is already forced to be 1 or more by parse_sources, so it is
    // never 0).
    if segments.is_none() && cli.seconds == 0 {
        stream.stop();
        return Err(
            "--seconds 0 (infinite) is only for raw PCM streaming (--out -). \
             For WAV output specify 1 or more."
                .into(),
        );
    }

    let start = Instant::now();
    let total = match segments {
        Some(segs) => SwitchScheduler::total_duration(segs),
        None => Duration::from_secs(cli.seconds),
    };
    // Hot-swap scheduler (only when --sources is given; the destination stays a single one).
    let mut scheduler = segments.map(|segs| SwitchScheduler::new(cli, segs, start));

    // WAV writer. Chunks are written as they arrive instead of being accumulated (so a file can
    // be finalized immediately at a split boundary; it also avoids piling up memory in long
    // recordings). With --split-seconds 0 it writes a single file to --out as before; with 1 or
    // more it rotates through numbered files. Peak / RMS are accumulated over the whole
    // recording.
    let mut writer = RotatingWavWriter::new(&cli.out, output, cli.split_seconds);
    let mut chunk_count: u64 = 0;

    // Loop poll_chunk for the total recording time and write out every chunk.
    let deadline = start + total;
    while Instant::now() < deadline {
        // If a segment boundary has been reached, switch the source (independent of rotating
        // the file being written).
        if let Some(sch) = scheduler.as_mut() {
            sch.tick(stream, Instant::now());
        }

        let mut got_any = false;
        while let Some(chunk) = stream.poll_chunk() {
            got_any = true;
            chunk_count += 1;
            if let Err(e) = write_wav_chunk(&mut writer, &chunk) {
                stream.stop();
                return Err(e);
            }
        }
        // Drain poll_event for display (print any events).
        while let Some(ev) = stream.poll_event() {
            println!("  event: {ev:?}");
        }
        if !got_any {
            // One chunk ≈ 20ms. Sleep moderately to avoid spinning.
            thread::sleep(Duration::from_millis(10));
        }
    }

    let dropped = stream.dropped_chunks();
    stream.stop();

    // Write out the chunks still left in the ring after stop too (zero drops).
    while let Some(chunk) = stream.poll_chunk() {
        chunk_count += 1;
        write_wav_chunk(&mut writer, &chunk)?;
    }

    // Even if no chunk arrived, the recording itself ran, so this is not a failure. Write an
    // empty WAV and only print a warning (this happens when the source is silent or excluded,
    // the selected endpoint is inactive, etc.).
    if chunk_count == 0 {
        eprintln!(
            "warning: could not capture any audio (the source may be silent or excluded, or \
             the selected endpoint may be inactive). Writing an empty WAV."
        );
    }

    // Done writing. Finalize the WAV header of the open file and get the peak / RMS of the
    // whole recording. WAV is currently fixed to s16 (if an f32 WAV is needed, the encoding
    // could also be applied to WAV output).
    let summary = writer
        .finish()
        .map_err(|e| format!("failed to write WAV: {e}"))?;
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
    println!("=== Result ===");
    println!("Chunks received    : {chunk_count}");
    println!("Total frames       : {total_frames}");
    println!("Seconds captured   : {captured_secs:.3} s");
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
    // When split recording produced multiple files, show the first to last numbered names
    // (finish guarantees at least 1 file, so files is non-empty). For a single file, print the
    // path as before.
    if summary.files.len() == 1 {
        println!("WAV written        : {}", summary.files[0].display());
    } else {
        println!(
            "WAV written        : {} to {} ({} files, split every {} s)",
            summary.files[0].display(),
            summary.files[summary.files.len() - 1].display(),
            summary.files.len(),
            cli.split_seconds
        );
    }

    // If chunks arrived but their content is nearly silent (peak/RMS nearly 0), warn in the same
    // spirit as for an empty WAV. This makes it clear that "it was silent" also when silent
    // frames flow due to OS differences (resulting in a silent WAV). Silence is judged on the
    // statistics of the whole recording (all files combined).
    if chunk_count > 0 && stats.peak < SILENCE_PEAK && stats.rms < SILENCE_RMS {
        eprintln!(
            "warning: could not capture any audio (the source may be silent or excluded, or \
             the selected endpoint may be inactive). The recording is nearly silent."
        );
    }

    Ok(())
}

/// Peak / RMS (linear) thresholds for judging "nearly silent". Below these it is treated as
/// silence and a warning is printed. A loose threshold aimed at around -60 dBFS (not an exact
/// value).
const SILENCE_PEAK: f32 = 1.0e-3;
const SILENCE_RMS: f64 = 1.0e-4;

/// stdout raw PCM streaming path.
///
/// Chunks are written to stdout as soon as they arrive and flushed each time, without
/// accumulating (low latency). With `--seconds 0` it runs forever (stopped normally by Ctrl-C /
/// a broken pipe `BrokenPipe`); with `--seconds N>0` it stops after N seconds. In both cases the
/// chunks left in the ring are flushed out after stop.
fn run_stdout_stream(
    cli: &Cli,
    stream: &mut Stream,
    segments: Option<&[Segment]>,
) -> std::result::Result<(), String> {
    // When --sources is given, it is a finite recording of the sum of the segments (--seconds is
    // overridden).
    let infinite = segments.is_none() && cli.seconds == 0;

    // Ctrl-C (SIGINT) flag. Stops when pressed in infinite mode. ctrlc returns Err on duplicate
    // registration, so it is registered only in infinite mode.
    let running = Arc::new(AtomicBool::new(true));
    if infinite {
        let r = running.clone();
        ctrlc::set_handler(move || {
            r.store(false, Ordering::SeqCst);
        })
        .map_err(|e| format!("failed to register the Ctrl-C handler: {e}"))?;
    }

    // Lock stdout and wrap it in a BufWriter. It is flushed per chunk, so nothing accumulates.
    let stdout = io::stdout();
    let mut out = BufWriter::new(stdout.lock());

    let start = Instant::now();
    // Deadline of a finite recording: the sum of the segments when --sources is given,
    // otherwise --seconds.
    let deadline = if infinite {
        None
    } else {
        let dur = match segments {
            Some(segs) => SwitchScheduler::total_duration(segs),
            None => Duration::from_secs(cli.seconds),
        };
        Some(start + dur)
    };
    // Hot-swap scheduler (only when --sources is given; stdout stays a single pipe).
    let mut scheduler = segments.map(|segs| SwitchScheduler::new(cli, segs, start));

    let mut wrote_any = false;
    let mut broken_pipe = false;

    // Main loop: poll and stream chunks to stdout.
    'outer: loop {
        // Check the stop condition (Ctrl-C in infinite mode, the deadline in finite mode).
        if infinite {
            if !running.load(Ordering::SeqCst) {
                break;
            }
        } else if let Some(dl) = deadline {
            if Instant::now() >= dl {
                break;
            }
        }

        // If a segment boundary has been reached, switch the source (the destination stays the
        // same single pipe).
        if let Some(sch) = scheduler.as_mut() {
            sch.tick(stream, Instant::now());
        }

        let mut got_any = false;
        while let Some(chunk) = stream.poll_chunk() {
            got_any = true;
            match write_chunk(&mut out, &chunk, cli.encoding) {
                Ok(()) => wrote_any = true,
                Err(e) if e.kind() == io::ErrorKind::BrokenPipe => {
                    // The receiver closed (| head etc.). Not an error; go to a normal stop.
                    broken_pipe = true;
                    break 'outer;
                }
                Err(e) => return Err(format!("failed to write to stdout: {e}")),
            }
        }

        // Events go to stderr (stdout is for PCM only).
        while let Some(ev) = stream.poll_event() {
            eprintln!("  event: {ev:?}");
        }

        if !got_any {
            // One chunk ≈ 20ms. Sleep moderately to avoid spinning.
            thread::sleep(Duration::from_millis(10));
        }
    }

    let dropped = stream.dropped_chunks();
    stream.stop();

    // Flush out the chunks still left in the ring after stop too (skipped after a broken pipe).
    if !broken_pipe {
        while let Some(chunk) = stream.poll_chunk() {
            match write_chunk(&mut out, &chunk, cli.encoding) {
                Ok(()) => wrote_any = true,
                Err(e) if e.kind() == io::ErrorKind::BrokenPipe => {
                    broken_pipe = true;
                    break;
                }
                Err(e) => return Err(format!("failed to write to stdout: {e}")),
            }
        }
    }

    // Final flush. A broken pipe is treated as normal here too.
    if !broken_pipe {
        if let Err(e) = out.flush() {
            if e.kind() != io::ErrorKind::BrokenPipe {
                return Err(format!("failed to flush stdout: {e}"));
            }
            broken_pipe = true;
        }
    }

    // --- Summary (stderr) ---
    eprintln!();
    eprintln!("=== Result (stderr) ===");
    if broken_pipe {
        eprintln!("Stop reason        : the receiver closed the pipe (normal exit)");
    } else if infinite {
        eprintln!("Stop reason        : Ctrl-C (normal exit)");
    } else {
        eprintln!("Stop reason        : {} s elapsed", cli.seconds);
    }
    eprintln!("Dropped chunks     : {dropped}");

    // A broken pipe or Ctrl-C is "a stop for the receiver's reasons", so zero samples is not an
    // error. Warn only when a finite duration ended normally and not a single sample came out.
    if !wrote_any && !broken_pipe && !infinite {
        return Err("could not get a single chunk. \
             The device opened, but no samples are flowing (check mute/permissions etc.)."
            .into());
    }

    Ok(())
}

/// Writes one chunk of interleaved f32 to `out` in the given encoding (little-endian).
///
/// To avoid small per-sample writes, the chunk's bytes are assembled first and emitted with a
/// single `write_all`. It flushes right after writing (low latency, no accumulation).
fn write_chunk<W: Write>(out: &mut W, chunk: &AudioChunk, encoding: EncodingArg) -> io::Result<()> {
    match encoding {
        EncodingArg::F32 => {
            // f32 LE: the contract as is. 4 bytes per sample.
            let mut buf = Vec::with_capacity(chunk.data.len() * 4);
            for &x in &chunk.data {
                buf.extend_from_slice(&x.to_le_bytes());
            }
            out.write_all(&buf)?;
        }
        EncodingArg::S16 => {
            // s16 LE: quantization is the canonical one shared by all layers,
            // [`flexaudio::core::quantize_i16`] (scale 32768, round, clamp, NaN→0). 2 bytes per
            // sample.
            let mut buf = Vec::with_capacity(chunk.data.len() * 2);
            for &x in &chunk.data {
                let s = flexaudio::core::quantize_i16(x);
                buf.extend_from_slice(&s.to_le_bytes());
            }
            out.write_all(&buf)?;
        }
    }
    // Emit as soon as it arrives. Do not accumulate in the BufWriter.
    out.flush()
}

/// Signal statistics computed while writing the WAV.
struct Stats {
    /// Maximum absolute value over all samples (linear, roughly 0.0..=1.0).
    peak: f32,
    /// Root mean square over all samples (linear).
    rms: f64,
}

/// Builds the file path of the `index`-th (1-based) file of a split recording (pure function).
///
/// For `rec.wav` it inserts a 3-digit zero-padded number before the extension, as in
/// `rec-001.wav, rec-002.wav, ...`. From the 1000th file on the number simply gets more digits
/// (`rec-1000.wav`). A path without an extension (`rec`) gets the number appended at the end
/// (`rec-001`). The parent directory is preserved.
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

/// Summary at the end of recording (list of written files, total frame count, statistics of the
/// whole recording).
struct WavSummary {
    /// Paths of the written files (in write order, at least 1).
    files: Vec<PathBuf>,
    /// Total frame count over all files.
    total_frames: u64,
    /// Peak / RMS of the whole recording (all files combined).
    stats: Stats,
}

/// Small writer responsible for splitting (rotating) WAV output.
///
/// With `--split-seconds 0` (no splitting) it writes a single file to the `--out` path as
/// before. With 1 or more it writes to the numbered paths of [`split_file_path`], and each time
/// the frames written reach `split_seconds × output sample rate` it finalizes (WAV header
/// finalized) and closes the current file, then writes to the next file from the next chunk
/// on. The boundary has chunk granularity (20ms), "move to the next file once reached or
/// exceeded", so each file can be up to 1 chunk longer than specified (chunks are never split
/// and nothing is dropped).
///
/// A file is not opened until the next chunk arrives (lazy creation), so even if the recording
/// ends exactly on a boundary no empty trailing file is left behind. Peak / RMS are aggregated
/// over the whole recording (all files combined) (to keep the statistics and the silence
/// warning meaning the same as with the original single file).
/// Quantization is fixed to 16-bit PCM using the canonical one shared by all layers,
/// [`flexaudio::core::quantize_i16`] (scale 32768, round, clamp).
struct RotatingWavWriter {
    /// Base path of `--out` (the basis of the numbered names when splitting; used as is when
    /// not splitting).
    base: PathBuf,
    /// WAV header spec (follows the output format, fixed to 16-bit PCM).
    spec: hound::WavSpec,
    /// Frame-count threshold per file (split_seconds × rate). 0 = no splitting.
    frames_per_file: u64,
    /// Writer currently being written to (lazily created. None right after a rotation or
    /// before anything is written).
    writer: Option<hound::WavWriter<BufWriter<File>>>,
    /// Frames written to the current file (reset to 0 on rotation).
    frames_in_current: u64,
    /// Paths of the files started so far (in write order).
    files: Vec<PathBuf>,
    /// Peak of the whole recording (maximum linear absolute value).
    peak: f32,
    /// Sum of squares over the whole recording (for computing RMS).
    sum_sq: f64,
    /// Sample count over the whole recording (for computing RMS).
    samples: u64,
    /// Total frame count over all files.
    total_frames: u64,
}

impl RotatingWavWriter {
    /// Creates the writer from the base path, the output format and the split seconds (0 = no
    /// splitting). No file is opened at this point (it is opened on the first chunk).
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
            frames_per_file: split_seconds * output.sample_rate as u64,
            writer: None,
            frames_in_current: 0,
            files: Vec::new(),
            peak: 0.0,
            sum_sq: 0.0,
            samples: 0,
            total_frames: 0,
        }
    }

    /// Whether this is a split recording (whether `--split-seconds` is 1 or more).
    fn is_split(&self) -> bool {
        self.frames_per_file > 0
    }

    /// Path of the next file to open. Without splitting it is the base path as is; with
    /// splitting it is a 1-based number.
    fn next_path(&self) -> PathBuf {
        if self.is_split() {
            split_file_path(&self.base, self.files.len() as u64 + 1)
        } else {
            self.base.clone()
        }
    }

    /// Writes one chunk. The chunk goes into the current file whole (it is not split), and if
    /// after writing the frame count has reached the threshold, the file is finalized right
    /// away and rotation moves to the next file (the next chunk becomes the head of the new
    /// file). Returns the path of the finalized file (for rotation progress display; None if no
    /// rotation happened).
    fn write_chunk(&mut self, chunk: &AudioChunk) -> hound::Result<Option<PathBuf>> {
        // The file is opened when a chunk arrives (lazy creation).
        if self.writer.is_none() {
            let path = self.next_path();
            self.writer = Some(hound::WavWriter::create(&path, self.spec)?);
            self.files.push(path);
        }
        let writer = self.writer.as_mut().expect("opened just above");
        for &x in &chunk.data {
            // Statistics are aggregated over the whole recording (all files combined).
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

        // Split boundary: finalize immediately once the threshold is "reached or exceeded"
        // (the ±1 chunk error is as specified).
        if self.is_split() && self.frames_in_current >= self.frames_per_file {
            let writer = self.writer.take().expect("just written above");
            writer.finalize()?;
            self.frames_in_current = 0;
            return Ok(self.files.last().cloned());
        }
        Ok(None)
    }

    /// End of recording. Finalizes the WAV header of the open file. If no chunk arrived at all,
    /// it writes one empty WAV as before (number 1 when splitting), so the returned `files` has
    /// at least 1 entry. Also returns the overall statistics and the total frame count.
    fn finish(mut self) -> hound::Result<WavSummary> {
        if let Some(writer) = self.writer.take() {
            writer.finalize()?;
        } else if self.files.is_empty() {
            // Not a single chunk arrived. Leave an empty WAV as evidence that the recording
            // itself ran (the original behavior).
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

/// Writes one chunk for `run_wav`. When a rotation happens it prints progress to stderr in the
/// same style as `[switch]`, and converts write errors into human-readable messages.
fn write_wav_chunk(
    writer: &mut RotatingWavWriter,
    chunk: &AudioChunk,
) -> std::result::Result<(), String> {
    match writer.write_chunk(chunk) {
        Ok(Some(done)) => {
            eprintln!(
                "[split] finalized {} (the next file starts with the next chunk)",
                done.display()
            );
            Ok(())
        }
        Ok(None) => Ok(()),
        Err(e) => Err(format!("failed to write WAV: {e}")),
    }
}

/// Formats dBFS for readability (`-inf dBFS` when silent).
fn fmt_dbfs(db: f64) -> String {
    if db.is_finite() {
        format!("{db:.1} dBFS")
    } else {
        "-inf dBFS (silence)".into()
    }
}

/// Converts a `flexaudio` [`Error`] into a human-readable message.
///
/// A missing device (`DeviceNotFound`) is replaced with guidance that prompts running on real
/// hardware.
fn describe_error(err: Error) -> String {
    match err {
        Error::DeviceNotFound => {
            "The specified device/endpoint was not found. Check the ID shown by \
             `--list-devices`."
                .into()
        }
        Error::PermissionDenied => {
            "No permission to access the microphone. Check the OS microphone permission \
             settings."
                .into()
        }
        Error::DeviceLost => "The input device was lost during capture (e.g. disconnected).".into(),
        other => format!("Failed to initialize the stream: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a `Cli` from a CLI argument list (via clap). `flexaudio-cli` is put first as the
    /// minimum.
    fn cli_from(args: &[&str]) -> Cli {
        let mut full = vec!["flexaudio-cli"];
        full.extend_from_slice(args);
        Cli::parse_from(full)
    }

    // --- parse_sources ---

    /// `mic:2,system:2,process:2` is parsed correctly into 3 segments (kind + secs).
    #[test]
    fn parse_sources_three_segments() {
        let segs = parse_sources("mic:2,system:2,process:2").expect("valid spec");
        assert_eq!(segs.len(), 3);
        assert_eq!(segs[0].kind, SourceKind::Mic);
        assert_eq!(segs[0].secs, 2);
        assert_eq!(segs[1].kind, SourceKind::SystemLoopback);
        assert_eq!(segs[2].kind, SourceKind::ProcessLoopback);
    }

    /// Whitespace and different seconds are accepted, and trimmed.
    #[test]
    fn parse_sources_trims_and_varies_secs() {
        let segs = parse_sources(" mic:1 , system:5 ").expect("valid spec");
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].kind, SourceKind::Mic);
        assert_eq!(segs[0].secs, 1);
        assert_eq!(segs[1].kind, SourceKind::SystemLoopback);
        assert_eq!(segs[1].secs, 5);
    }

    /// A single segment is also valid.
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

    /// Not in the `<src>:<secs>` form (no colon) is an error.
    #[test]
    fn parse_sources_rejects_missing_colon() {
        assert!(parse_sources("mic2").is_err());
    }

    /// An unknown source name is an error.
    #[test]
    fn parse_sources_rejects_unknown_source() {
        assert!(parse_sources("foo:2").is_err());
    }

    /// Non-numeric seconds are an error.
    #[test]
    fn parse_sources_rejects_non_numeric_secs() {
        assert!(parse_sources("mic:abc").is_err());
    }

    /// Zero seconds is an error (1 or more required).
    #[test]
    fn parse_sources_rejects_zero_secs() {
        assert!(parse_sources("mic:0").is_err());
    }

    // --- config_for_kind ---

    /// `config_for_kind` correctly reflects the CLI's shared settings (output / pid / mode /
    /// exclude_self / device_id) in the StreamConfig, and overrides kind with the argument.
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
        // kind is overridden by the argument (it can be given independently of the CLI's
        // --source).
        let cfg_mic = config_for_kind(&cli, SourceKind::Mic);
        assert_eq!(cfg_mic.kind, SourceKind::Mic);
        // The other shared settings are left as they are.
        assert_eq!(cfg_mic.output.sample_rate, 16_000);
    }

    /// `--exclude-self` is reflected in StreamConfig.exclude_self.
    #[test]
    fn config_for_kind_reflects_exclude_self() {
        let cli = cli_from(&["--source", "system", "--exclude-self"]);
        let cfg = config_for_kind(&cli, SourceKind::SystemLoopback);
        assert!(cfg.exclude_self);
        // The default (not given) is false.
        let cli2 = cli_from(&["--source", "system"]);
        assert!(!config_for_kind(&cli2, SourceKind::SystemLoopback).exclude_self);
    }

    /// The default CLI (minimal arguments) gives output {48000,2} / mode Include / pid None.
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

    /// The main Error variants are converted into human-readable wording (branching per kind).
    #[test]
    fn describe_error_maps_known_variants() {
        assert!(describe_error(Error::DeviceNotFound).contains("not found"));
        assert!(describe_error(Error::PermissionDenied).contains("permission"));
        assert!(describe_error(Error::DeviceLost).contains("lost"));
        // Everything else includes the generic wording + Display.
        let msg = describe_error(Error::Unsupported);
        assert!(msg.contains("Failed to initialize the stream"));
    }

    /// The DeviceNotFound wording is source-neutral (it contains no words that assume mic).
    /// It is also appropriate guidance when an invalid device-id is given for system or
    /// process.
    #[test]
    fn describe_error_device_not_found_is_source_neutral() {
        let msg = describe_error(Error::DeviceNotFound);
        assert!(!msg.contains("microphone"));
        assert!(!msg.contains("input device"));
        // It is guidance that prompts checking the ID.
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

    /// The mix flags (--mic-device-id / --system-device-id / --mic-gain / --system-gain)
    /// are reflected in the mix_* fields of StreamConfig. The defaults are None / 1.0.
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

        // If not given, the defaults (device None, per-side gain 1.0).
        let cli2 = cli_from(&["--source", "mix"]);
        let cfg2 = config_for_kind(&cli2, SourceKind::Mix);
        assert_eq!(cfg2.mix_mic_device_id, None);
        assert_eq!(cfg2.mix_system_device_id, None);
        assert_eq!(cfg2.mix_mic_gain, 1.0);
        assert_eq!(cfg2.mix_system_gain, 1.0);
    }

    /// `truncate` returns the string as is if it has at most max chars; otherwise it fits it
    /// into max chars with a trailing ….
    #[test]
    fn truncate_respects_char_boundary() {
        assert_eq!(truncate("abc", 5), "abc");
        // Exactly max is left as is.
        assert_eq!(truncate("abcde", 5), "abcde");
        // Excess becomes max chars with … (keep = max-1).
        let t = truncate("abcdefgh", 5);
        assert_eq!(t.chars().count(), 5);
        assert!(t.ends_with('…'));
        assert!(t.starts_with("abcd"));
        // Multibyte (Japanese) text is also cut safely per char (no panic).
        let jp = truncate("あいうえおかきくけこ", 3);
        assert_eq!(jp.chars().count(), 3);
        assert!(jp.ends_with('…'));
    }

    // --- split_file_path ---

    /// Inserts a 3-digit zero-padded number before the extension; from 1000 on the number
    /// simply gets more digits.
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
        // From the 1000th file on, the number exceeds the zero-padding width and simply gets
        // more digits.
        assert_eq!(
            split_file_path(Path::new("rec.wav"), 1000),
            PathBuf::from("rec-1000.wav")
        );
    }

    /// The parent directory is preserved, and a path without an extension gets the number
    /// appended at the end.
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
        // A name with multiple dots gets the number inserted before the last extension.
        assert_eq!(
            split_file_path(Path::new("a.b.wav"), 2),
            PathBuf::from("a.b-002.wav")
        );
    }

    // --- RotatingWavWriter ---

    /// Builds a test chunk (all samples equal, interleaved).
    fn chunk_of(frames: usize, channels: usize, value: f32) -> AudioChunk {
        AudioChunk {
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

    /// Creates and returns an empty test-only temporary directory (separated by test name, safe
    /// for parallel runs).
    fn test_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("flexaudio_cli_{}_{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create test dir");
        dir
    }

    /// Reads a WAV back and returns its frame count (= sample count / channel count).
    fn wav_frames(path: &Path) -> u64 {
        let reader = hound::WavReader::open(path).expect("open wav");
        (reader.len() / reader.spec().channels as u32) as u64
    }

    /// Without splitting (split 0) it writes a single file to the base path as before and
    /// produces the header (rate/ch/16bit) and peak/rms correctly. Verified by reading the
    /// written WAV back with hound.
    #[test]
    fn rotating_writer_without_split_matches_legacy_single_file() {
        let output = OutputFormat {
            sample_rate: 16_000,
            channels: 1,
        };
        let dir = test_dir("nosplit");
        let path = dir.join("capture.wav");

        // Alternating amplitude 0.5 / -0.5 (peak=0.5, rms=0.5).
        let data: Vec<f32> = (0..320)
            .map(|i| if i % 2 == 0 { 0.5 } else { -0.5 })
            .collect();
        let chunk = AudioChunk {
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

        // Without splitting it is a single file at the base path as is, with no number (fully
        // compatible).
        assert_eq!(summary.files, vec![path.clone()]);
        assert_eq!(summary.total_frames, 320);

        // peak/rms are known (all samples are |0.5|, so peak=0.5, rms=0.5).
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

    /// Computing the split boundary (deterministic, based on frame counts): writing 150 chunks
    /// of 20ms (320 frames) at 16kHz mono / split 1 second (threshold 16000 frames) yields
    /// exactly 3 files of 50 chunks each, and the total frame count matches (zero missing).
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
        // 16000 / 320 = 50, so finalization happens at the 50th, 100th and 150th chunk (49, 99
        // and 149 zero-based).
        assert_eq!(
            rotations.iter().map(|(i, _)| *i).collect::<Vec<_>>(),
            vec![49, 99, 149]
        );

        let summary = writer.finish().expect("finish");
        // Writing ended exactly on a boundary, so no empty 4th file is created.
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

        // Read back: each file is exactly 1 second, and the total = the input (zero drops).
        let mut read_total = 0u64;
        for f in &summary.files {
            let frames = wav_frames(f);
            assert_eq!(frames, 16_000);
            read_total += frames;
        }
        assert_eq!(read_total, summary.total_frames);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The boundary is chunk-granular, "move to the next file once reached or exceeded": when
    /// the threshold is not a multiple of the chunk size, a file includes the chunk that crosses
    /// the threshold (an excess of at most 1 chunk). Chunks are never split, and the next file
    /// starts with the next chunk.
    #[test]
    fn rotating_writer_rounds_boundary_up_to_chunk() {
        // Threshold 1000 frames (1000Hz × 1 second), chunks of 320 frames.
        // At the 4th chunk 1280 >= 1000, so it is finalized (the ±1 chunk error is as
        // specified).
        let output = OutputFormat {
            sample_rate: 1_000,
            channels: 1,
        };
        let dir = test_dir("split_roundup");
        let base = dir.join("rec.wav");
        let mut writer = RotatingWavWriter::new(&base, output, 1);

        // 7 chunks: 4 finalize the 1st file; the remaining 3 (960 < 1000) are finalized into
        // the 2nd file by finish.
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
        // 1st file = 4 chunks (1280 frames, threshold 1000 rounded up); 2nd file = all the
        // rest.
        assert_eq!(wav_frames(&summary.files[0]), 1280);
        assert_eq!(wav_frames(&summary.files[1]), 960);
        assert_eq!(summary.total_frames, 7 * 320);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Even for stereo the boundary is counted in "frames" (not samples).
    #[test]
    fn rotating_writer_counts_frames_not_samples_for_stereo() {
        // Threshold 640 frames, stereo chunks of 320 frames (640 samples).
        // Counting in samples would rotate wrongly at the 1st chunk.
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

    /// Even if no chunk arrives at all, finish leaves one empty WAV (keeping the original
    /// behavior). Without splitting it is the base path; with splitting it is number 1.
    #[test]
    fn rotating_writer_finish_writes_empty_wav_when_no_chunks() {
        let output = OutputFormat {
            sample_rate: 16_000,
            channels: 2,
        };
        let dir = test_dir("split_empty");

        // No splitting → capture.wav (as before).
        let base = dir.join("capture.wav");
        let summary = RotatingWavWriter::new(&base, output, 0)
            .finish()
            .expect("finish");
        assert_eq!(summary.files, vec![base.clone()]);
        assert_eq!(summary.total_frames, 0);
        assert_eq!(wav_frames(&base), 0);

        // Splitting → rec-001.wav (an empty 1st file).
        let base2 = dir.join("rec.wav");
        let summary2 = RotatingWavWriter::new(&base2, output, 5)
            .finish()
            .expect("finish");
        assert_eq!(summary2.files, vec![dir.join("rec-001.wav")]);
        assert_eq!(wav_frames(&dir.join("rec-001.wav")), 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Integration test that really drives a Stream with MockBackend: recording 3 seconds' worth
    /// with split 1 second yields 3 files, and the total frame count read back matches the
    /// frames written (zero missing). The stop condition is deterministic, counted in
    /// "finalized files" rather than by the wall clock.
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
        // 48kHz mono sine-wave source (no real hardware needed). The Stream converts it to the
        // 16k/mono output.
        let backend = Box::new(MockBackend::new(48_000, 1, 440.0));
        let mut stream = Stream::open(config, backend).expect("open stream");
        stream.start().expect("start stream");

        let dir = test_dir("split_mock");
        let base = dir.join("rec.wav");
        let mut writer = RotatingWavWriter::new(&base, output, 1);

        // Feed chunks until the 3rd file is finalized (= 3 seconds' worth has been written).
        // Every chunk that arrives is written, so the frames written (fed_frames) are the
        // expected value. As a safety valve only a maximum chunk count is set (no assertions on
        // ratios or elapsed time).
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
                "reached the chunk limit before 3 files were finalized (the Stream is not flowing)"
            );
        }
        stream.stop();

        let summary = writer.finish().expect("finish");
        // It stopped exactly at the finalization of the 3rd file, so no empty 4th file is
        // created.
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

        // Read back: total = amount written (zero missing); each file is at least 1 second and
        // exceeds it by less than 1 chunk (the boundary is rounded up to chunk granularity).
        let mut read_total = 0u64;
        for f in &summary.files {
            let frames = wav_frames(f);
            assert!(
                frames >= 16_000,
                "each file is at least split seconds long: {frames}"
            );
            assert!(
                frames < 16_000 + max_chunk_frames,
                "the excess is less than 1 chunk: {frames}"
            );
            read_total += frames;
        }
        assert_eq!(read_total, fed_frames, "total read back = frames written");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- run: rejecting --out - combined with --split-seconds ---

    /// Combining stdout streaming (--out -) with --split-seconds is rejected with a clear error
    /// before the stream is opened (verifiable without a device).
    #[test]
    fn run_rejects_split_seconds_with_stdout_stream() {
        let cli = cli_from(&["--out", "-", "--split-seconds", "5"]);
        let err = run(&cli).expect_err("must be rejected");
        assert!(err.contains("--split-seconds"), "err: {err}");
        assert!(err.contains("cannot be combined"), "err: {err}");
    }

    /// s16 quantization in `write_chunk`: f32 → i16. Unified on the canonical `quantize_i16`
    /// shared by all layers (scale 32768, round, clamp, NaN→0). Negative full scale `-1.0` is
    /// `-32768`, and out-of-range values saturate.
    #[test]
    fn write_chunk_s16_quantizes_and_clamps() {
        let chunk = AudioChunk {
            // 0.0 / 1.0 / -1.0 / out of range 2.0(→clamp 32767) / -2.0(→clamp -32768).
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
        assert_eq!(s(2), -32768); // -1.0 → negative full scale -32768
        assert_eq!(s(3), 32767); // 2.0 → clamp 32767
        assert_eq!(s(4), -32768); // -2.0 → clamp -32768
    }

    /// f32 path of `write_chunk`: byte length = sample count × 4, and the LE round trip matches.
    #[test]
    fn write_chunk_f32_roundtrips() {
        let chunk = AudioChunk {
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

    /// `fmt_dbfs`: finite values in dBFS notation, infinity in silence notation.
    #[test]
    fn fmt_dbfs_finite_and_infinite() {
        assert!(fmt_dbfs(-6.0).contains("dBFS"));
        assert!(fmt_dbfs(f64::NEG_INFINITY).contains("silence"));
    }
}
