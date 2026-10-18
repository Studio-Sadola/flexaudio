# flexaudio

Version 0.6.0 replaces the two earlier VAD APIs with one generic `Vad`.
See the [0.6.0 migration table](CHANGELOG.md#060---2026-10-15).
The 0.5.x series is deprecated.

Rust capture migration from 0.4: `AudioChunk` and `SecondaryChunk` struct literals must
include the new public `frame_index: u64` field. It is the first frame's index
on the tap's canonical 48 kHz timeline, continues across reopen, and exposes
queue drops as gaps. Set it from the producing timeline when constructing
chunks; use `frame_index: 0` for a single synthetic chunk starting at zero.
The C `FlexChunk` v1 layout is unchanged.
Rust `DeviceInfo` struct literals must also supply `format_available` (0.5.4).

**English** | [Japanese](README.ja.md)

**General-purpose, flexible, cross-platform audio capture for Rust.**

`flexaudio` provides one unified API for capturing audio from **microphones**,
**system output (loopback)**, **individual processes**, and a **microphone + system mix** — across **Linux**,
**Windows**, and **macOS**. It normalizes every source to an interleaved
`f32` stream at an output format you choose, and hands you chunks plus
device/stream events through a simple poll loop.

```rust
use flexaudio::{open, StreamConfig, SourceKind};

let mut stream = open(StreamConfig {
    kind: SourceKind::Mic,
    ..Default::default()
})?;
stream.start()?;
while let Some(chunk) = stream.poll_chunk() {
    // chunk.data is interleaved f32 in your chosen OutputFormat
    let _ = (chunk.frames, chunk.peak, chunk.rms);
}
stream.stop();
# Ok::<(), flexaudio::Error>(())
```

---

## Capability matrix (the "9 cells")

Three capture sources × three operating systems. ✅ = implemented and verified,
— = not available on that platform.

| Source              | Linux            | Windows           | macOS                       |
|---------------------|------------------|-------------------|-----------------------------|
| **Microphone**      | ✅ (cpal/ALSA)   | ✅ (cpal/WASAPI)  | ✅ (cpal/CoreAudio)         |
| **System output**   | ✅ (PipeWire)    | ✅ (WASAPI loopback) | ✅ (CoreAudio process taps) |
| **Per-process**     | ✅ (PipeWire)    | ✅ (WASAPI process loopback) | ✅ (CoreAudio process taps) |

- **Microphone** works on all platforms via [`cpal`].
- **System / per-process** capture uses the native OS backend selected at
  compile time; calling an unsupported source on a given OS returns
  `Error::Unsupported`.
- Per-process capture requires a `target_pid` in `StreamConfig`.
- `SourceKind::Mix` combines microphone and system capture on all three platforms.
- **0.6 VAD:** one generic `Vad` provides streaming segmentation, frame
  probabilities, provisional hints and an attached capture tap in Rust, Node.js,
  Python and C. Its embedded Silero model runs offline.
- **Typed diagnostics:** inspect loss, clipping, permission and recoverable/fatal
  events, plus retained capture and cleanup failures.

---

## Binding feature matrix

These are the capture interfaces; standalone add-ons are separate. Attached VAD
always processes the shared canonical capture branch; primary/secondary selects
which output carries its events. — = not exposed.

| Feature | Rust | Node | Python | C | CLI |
|---|---|---|---|---|---|
| Delivery model | Poll | Callbacks over internal polling | Poll | Poll | Internal polling → stdout/WAV |
| Secondary output tap | Yes | Yes (`chunk.secondary`) | Yes (`poll_secondary`, f32/s16) | Yes (v3, f32/s16) | — |
| Attached VAD (primary/secondary) | Canonical capture via `VadTap` | Primary or secondary carrier | Primary or secondary carrier | Primary (v2/v3) or secondary (v3) carrier | — |
| Denoise | Canonical 48 kHz; any output rate | Requires 48 kHz output | Requires 48 kHz output | v1/v2: 48 kHz output; v3: any output rate | — |
| Ring capacity | Configurable; default 50 chunks | 50 chunks | 50 chunks | 50 chunks | 50 chunks |
| Source switching | `switch_source` | `switchSource`; retains add-on settings | `switch_source` | `flexaudio_switch_source` | `--sources` schedule |
| Event detail | Typed events/errors/loss | Typed events/errors/loss | Typed events/errors/loss | v1 flattened; v2 detailed | stderr diagnostics/loss summaries |
| Silence probe control | `silence_probe` | `silenceProbe` | `silence_probe` | `silence_probe`: 0/1 on, 2 off | `--no-silence-probe` |
| Capability snapshot | `capabilities()` | `await capabilities()` | `capabilities()` | `flexaudio_capabilities_v1` | `--capabilities` (JSON) |
| Windows classic loopback keepalive | `OpenOptions` | `windowsClassicLoopbackKeepalive` | `windows_classic_loopback_keepalive` | v3 `windows_classic_loopback_keepalive` | `--no-classic-loopback-keepalive` |
| Gain 0 | Mute | Mute | Mute | Config gains: unity (1.0); runtime `flexaudio_set_gain(s, 0)`: mute | Mute |

The optional silence diagnosis probe is macOS-only and enabled by default.

---

## Capability discovery

`capabilities()` reports source backends and availability, OS version when known,
PID/exclusion scope and limits, output-device selection behavior, watcher support,
and optional rendering side effects. It opens no capture or render streams,
plays no audio, and requests no recording permission.

```rust
use flexaudio::capabilities;

let support = capabilities();
println!("{:?}", support.sources.system.availability);
println!("{:?}", support.system_exclusions);
println!("{:?}", support.side_effects);
```

`Supported` describes platform prerequisites, not permission or a usable device.
`Unknown { error }` means a prerequisite query failed or could not finish; inspect
its typed error rather than treating it as supported or confirmed unavailable.
On Linux, a missing PipeWire socket is `Unavailable`; failed socket inspection,
connection or server sync is `Unknown`. The bounded probe may wait for the audio
service, so use a worker thread in GUI applications. Node and CLI JSON spell the
statuses `supported`, `unavailable`, and `unknown`; Python uses the same tags.

---

## Install

```toml
[dependencies]
flexaudio = "0.6"
```

or:

```sh
cargo add flexaudio@0.6
```

The Voice Activity Detection add-on is a separate crate:

```sh
cargo add flexaudio-vad@0.6
```

---

## Minimal example

```rust
use flexaudio::{open, StreamConfig, SourceKind, OutputFormat};

let config = StreamConfig {
    kind: SourceKind::Mic,
    output: OutputFormat { sample_rate: 16_000, channels: 1 },
    ..Default::default()
};
let mut stream = open(config)?;
stream.start()?;

// Pull chunks (interleaved f32) and stream-level events.
while let Some(chunk) = stream.poll_chunk() {
    let _ = chunk; // chunk.data, chunk.frames, chunk.peak, chunk.rms, chunk.seq, ...
}
while let Some(event) = stream.poll_event() {
    let _ = event; // ChunkDropped / StreamStalled / PermissionDenied / DeviceLost / Error / ...
}
stream.stop();
# Ok::<(), flexaudio::Error>(())
```

---

## Public API at a glance

The facade crate `flexaudio` re-exports everything you need:

- `flexaudio::open(StreamConfig) -> Result<Stream>` — pick a backend by source +
  OS and build a (not-yet-started) capture stream.
- `Stream::start` / `Stream::stop` — control capture. `stop_checked()` returns
  capture and cleanup failures; `shutdown_report()` retains the completed outcome.
  A stopped stream is spent (`start()` returns `InvalidState`); open a new stream
  to capture again. This lifecycle rule already applied in 0.4.
  Repeated stop does not repeat teardown or duplicate events.
- `Stream::poll_chunk` / `Stream::poll_event` — pull `AudioChunk`s and `Event`s.
- `Stream::terminal_error() -> Option<Error>` — inspect a stored terminal failure,
  including after stop. `Stream::resume()` returns `Result<()>`.
- `Stream::switch_source` — hot-swap the input source without stopping the
  stream (chunk `seq` stays continuous; the first chunk after a switch carries a
  discontinuity flag).
- `flexaudio::devices() -> Result<Vec<DeviceInfo>>` — enumerate microphones
  (CPAL, all platforms) and system output endpoints (Linux: PipeWire sinks;
  Windows: active render endpoints; macOS: output devices) in one list.
  Linux microphone IDs are CPAL IDs; PipeWire Audio/Source nodes are not advertised
  as microphones. Collection or count failure returns an error, including
  when the PipeWire daemon is unreachable on Linux.
  On macOS, enumeration never prompts: undecided microphone consent returns a
  typed `Backend` error before any HAL/device/format query. Grant the responsible
  host access or use the bounded capture-open consent flow, then retry discovery.
  Denied/restricted consent returns `PermissionDenied`. Unreadable mandatory formats
  use the 0/0 sentinel and `format_available=false`; other devices remain usable.
  Linux PipeWire output formats retain the compatibility defaults described in
  [Failure reporting](#failure-reporting-in-054).
  Windows enumeration does not preflight microphone permission; native enumeration
  proceeds. Microphone capture retains its separate existing permission preflight.
- `flexaudio::processes() -> Result<Vec<ProcessInfo>>` — list audio output
  session/stream owners on Linux/Windows and Core Audio processes on macOS,
  including input-only processes (see
  [Listing capturable processes](#listing-capturable-processes)). Idle/stopped
  processes are included; use `is_output_active` to check current playback and
  `pid` as `target_pid` for per-process capture.
- `flexaudio::watch_devices() -> Result<DeviceWatcher>` — pull-style device
  notifications. Linux reports PipeWire system output additions, removals and
  default changes using the same output IDs as `devices()`. Microphone changes
  are not reported: CPAL microphone IDs cannot be resolved from PipeWire nodes.
  Linux startup failure returns an error; later daemon, callback,
  or shutdown failures produce `DeviceEvent::TerminalError { error }`. A later
  stop/join failure can emit another notification with accumulated causes.
  Node.js/Python expose `terminalError` with a typed `error`; C v2 exposes device
  event kind 6 and `flexaudio_device_event_error_v2`. C v1 polling returns the
  root negative error code and sets last_error. Windows/macOS currently
  provide an intentional no-op watcher.
- Re-exported types: `StreamConfig`, `SourceKind`, `ProcessMode`, `OutputFormat`,
  `AudioChunk`, `SecondaryChunk`, `ChunkFlags`, `DeviceInfo`, `ProcessInfo`,
  `DeviceEvent`, `DefaultDeviceKind`, `Event`, `Permission`, `Error`, `ErrorKind`,
  `ErrorContext`, `AddonErrorKind`, `AudioLoss`, `ShutdownReport`, `Result`.

Voice activity detection (`flexaudio-vad`) uses one `Vad` engine for standalone
and attached processing. `VadPostProcessor` and `speech_segments` expose the same
segmentation independently of capture. The embedded Silero v6 model runs fully
offline. Segmentation is ported from whisper.cpp `85a69493`; final segment endpoints
are integer milliseconds on its 10 ms grid. Compatibility covers postprocessing
for identical frame probabilities, rather than probabilities from different models.

```rust
use flexaudio_vad::{Vad, VadOptions, VadRuntimeOptions};

let mut vad = Vad::new(VadOptions::default(), VadRuntimeOptions::default())?;
let events = vad.process(&vec![0.0; 16_000])?; // Default: 16 kHz mono.
let final_events = vad.finish()?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

`VadOptions` defaults are `threshold: 0.5`, `min_speech_ms: 250`,
`min_silence_ms: 100`, `speech_pad_ms: 30`, and `max_speech_ms: 0` (unbounded).
`VadRuntimeOptions.provisional` defaults to false. Attached and standalone use the
same defaults; applications explicitly select their own duration caps.

`maxSpeechMs` (`max_speech_ms`) is an integer number of milliseconds. Omitted,
zero, or values above 100,000,000 ms use the legacy `INT_MAX/2`-sample sentinel
(1,073,741,823 samples), which is effectively unbounded. Any other nonzero
value uses the exact split budget `16 * maxSpeechMs - 512 - 32 * speechPadMs`
in 16 kHz samples, including for subsecond values: 900 ms is 900 ms, with no
seconds truncation. Negative budgets are rejected with `InvalidParameter`;
a zero split budget is valid (for example, 1000 ms with 484 ms padding).
Segmentation can merge neighboring splits, so this is not a strict bound on final
segment duration or delivery latency.

In 0.5.x, fractional `maxSpeechDurationS` values were truncated to whole seconds.
To preserve identical segmentation, migrate with
`maxSpeechMs = trunc(seconds) * 1000` (for example, 30.9 seconds becomes 30000 ms).

Standalone Node.js `process(samples, inputSampleRate?, inputChannels?)` defaults
to 16000 Hz and one channel. Python uses `input_sample_rate` and `input_channels`
with the same defaults. Rust uses `process_pcm(samples,
input_sample_rate, input_channels)` for converted input and `process(samples)` for
16 kHz mono. Accepted input formats are converted internally; event times remain
milliseconds on the input timeline, with converter group delay compensated.
Finite resampler overshoot is clipped internally to normalized PCM; raw input must
remain finite and within [-1, 1]. Golden equality applies to 16 kHz mono input.

`VadTap` attaches to canonical capture with `epochStart` anchors (`captureSample`
and `ptsNs`), final `segment` events, optional `provisionalSpeechStart`,
`provisionalSpeechEnd`, and `provisionalCut` hints, and `epochEnd` closure.
`StreamVadOptions` configures the attachment; Node.js uses `OpenOptions.vad`.
Flush/stop delivers closure events. Capture index gaps and discontinuities close
the intact prefix and anchor the next accepted frame in a new epoch; missing audio
never becomes padding. A VAD session failure latches that tap until a discontinuity
or successful flush resets it. Capture and PCM delivery continue; the affected
epoch closes once with a typed error. Capture chunks remain fixed at 20 ms because
the canonical 960-frame capture blocks and converter phase alignment depend on it.

---

## Failure reporting in 0.5.4

Collection/enumeration or count failures fail the whole call with a typed error.
Unreadable per-entry optional metadata stays unknown; process names use the existing
`pid N` fallback and absent executable/bundle ID. Entries with no readable selectable
identity may be omitted. Windows/macOS retain existing name-based IDs when the name
is readable; an unreadable name uses the native endpoint ID/UID with `name=""`
(explicit unknown). IDs can change between discovery calls: the native fallback may
be replaced by the recovered name. A name-based ID cannot be opened while its name
is unreadable; the native ID remains openable in both name states. macOS devices
with unreadable output direction are omitted from discovery; the typed direction
cause is returned for the requested device. Explicit-output scans skip vanished or
unrelated unreadable devices. If the requested macOS device's direction query fails
and neither its name nor UID is readable, it cannot be matched and open returns
`DeviceNotFound`. An unreadable default query leaves
`is_default=false` and does not fail discovery. Windows/macOS default opens validate
the resolved default format. On Linux, `device_id=None` leaves `target.object` unset:
PipeWire resolves the default sink and negotiates the format at open, without an
inventory pre-check. Linux exclusion fan-in is also not device-scoped and uses its
negotiated canonical format.
An unreadable mandatory device format keeps the device with
`format_available: false` (JS `formatAvailable`) and **sample rate = 0, channels = 0**.
Windows/macOS output and microphone opens perform a fresh checked format query.
Explicit Linux output IDs use a fresh checked inventory and format-availability gate.
These checks fail with a typed cause and retry remedy if the format remains unreadable;
Linux default output opens rely on PipeWire resolution and negotiation instead. C keeps the v1 struct layout and exposes
`flexaudio_device_format_available_v2`. Linux PipeWire output discovery and watcher
notifications retain their independent 48000 Hz / 2-channel compatibility defaults
for missing optional properties; those defaults are available, not measured.
Windows process ERROR_INVALID_PARAMETER (87 / HRESULT 0x80070057) means vanished;
ERROR_ACCESS_DENIED (5 / 0x80070005) and other image/name failures mean unknown metadata.
Windows process discovery omits invalidated or vanished render endpoints
(AUDCLNT_E_DEVICE_INVALIDATED, E_INVALIDARG, ERROR_NOT_FOUND, ERROR_FILE_NOT_FOUND);
access denied and other endpoint query failures terminate the call with a typed cause.
Windows device discovery applies the same omission/terminal-error rule to `Item()` failures.
Linux process discovery omits a failed node bind only when a registry removal confirms
the node vanished; other bind failures reject the collection with the first cause.
macOS kAudioHardwareBadObjectError (`!obj`, 0x216f626a), ESRCH (3), and ENOENT (2)
mean vanished; other property/path failures mean unknown metadata.

Windows native waits distinguish
an actual timeout from a failed wait, and COM initialization reports unexpected
HRESULT failures.

Primary and secondary `ptsNs` (`pts_ns` in Rust/Python/C) are arrival-time labels
assigned when samples are ingested and re-anchored per ingest. Microphone and
loopback use the same timing origin on every OS; device timestamps are not used.
Labels are clamped non-decreasing, but chunks can overlap or repeat timestamps;
this is not a media clock. `frameIndex` / `frame_index` is the authoritative
position; gaps report loss. VAD's exact sample-clock mapping uses
`epochStart.ptsNs + tMs * 1_000_000`, or `epochStart.captureSample` plus `tMs * 48`.
`captureSample` / `capture_sample` counts 48 kHz frames on the dedicated capture
timeline, independent of primary/secondary formats and `tap`. It equals primary
`frameIndex` for the same sample when primary is 48 kHz, including generation
replacement, recovery, loss rebuild and startup PCM release. At other primary rates
the axes share units but not an exact origin: resampler group delay is not compensated,
and unreconciled startup-release padding can grow the offset by up to one primary
chunk per boundary.
Neither index is an offset into concatenated delivered PCM: pause and queue drops
advance indices without delivering samples.

An arrival gap without reopen emits no event or `DISCONTINUITY`: `frameIndex`
remains contiguous, only primary `ptsNs` jumps, and attached Vad keeps its
epoch open. A stall (at least 2 s without consumed samples, checked every 250 ms)
emits `stalled` and attempts to reopen. Each failed reopen attempt emits
`recoverableError` and backs off from 250 ms to 5 s. After a successful reopen,
`recovered` is emitted when new PCM arrives. The first new-generation primary data chunk has
`RECOVERED|DISCONTINUITY`; Vad closes with `epochEnd(finish)` and starts a
new epoch when new PCM arrives. Downtime does not become frames. These three
advisories are unordered relative to `onChunk` and epoch events.
Attached Vad epochs strictly increase within a stream (checked epoch + 1
on rebuild); each new `openStream` starts at 0 and `(epoch, seq)` is unique per
stream. In Node.js, `epochEnd(error)` precedes `vadFailed`.
Every successful `switchSource` creates a generation with `DISCONTINUITY`, with
the epoch transition deferred until new PCM. The synchronous call does not invoke
`onChunk` re-entrantly; return means the backend was replaced, not that data was
delivered. Old `epochEnd` is not guaranteed before the first new-generation primary
chunk; consumers use primary `DISCONTINUITY` as the boundary.

Linux routing changes mark the next delivered chunk `DISCONTINUITY`. Failed binds,
malformed/missing buffers, rejected renegotiation and callback panics surface through
the existing readiness, loss and terminal paths. A failed watcher ends monitoring;
`RescanRequired` remains an inventory invalidation, independent of watcher failure.
Stop retains worker join failures and queued terminal notifications.

`Error::Diagnostic { source, detail }` preserves opt-in native detail while safe
`Display` shows only its typed source. Node.js `diagnosticDetails`, Python
`diagnostic_details` and C v2 diagnostic count/detail accessors expose that detail;
it may contain private host data and should not be logged automatically.
`Error::Addon { kind, message }` preserves `AddonErrorKind` for VAD model load,
inference, invalid configuration/format, resampling/reset and FLAC I/O,
unsupported input or encoder failure. Bindings expose `addonKind`/`addonMessage`
(Python: `addon_kind`/`addon_message`; C v2: addon kind/message accessors).
Later terminal failures, combined flush/reset failures, VAD tails and VAD
model/conversion causes remain ordered related errors rather than replacing the
primary. Rust `InferenceError::Multiple`, `VadError::Cause` and
`VadTapError::Cause` arms retain those typed causes.

VAD failures preserve typed model, converter and session causes. Attached source
switches close the old epoch through the shared gap lifecycle before replacing
the attachment. Event-only carriers deliver closure without inventing PCM.

An inconclusive macOS self-probe preserves its native reason and emits an existing
`RecoverableError` alongside the silence advisory; capture remains usable.
Worker creation errors retain their I/O kind and OS code with retry guidance.
The Node.js native loader retains every attempted candidate and its original failure
in `attempts`/`errors`; failed stop error creation still rejects the Promise.
FFI boolean panic guards set a typed last error. Python invalid PID diagnostics
include the offending representation, using hexadecimal for integers too large for
decimal formatting. Clock translation saturates at the `i64` boundary; the CLI
names the failed operation and checks WAV finalization even after capture failure,
retaining both failures.

## Migrating to 0.5

See [the 0.5.0 changelog](CHANGELOG.md#050---2026-10-11) for the complete
migration. Use 20 ms chunks and mono/stereo native input; other chunk sizes
return `InvalidArg`, and native input above two channels is `UnsupportedFormat`.
A fatal Mix lane error stops both lanes and retains its lane context.

`Error::kind()` / `root()` / `permission()` classify wrapped errors without
parsing messages. `Display` keeps library-authored explanations while redacting
device names, permission detail and internal call labels; structured access
provides opt-in diagnostics. Third-party legacy backend strings are replaced by
a safe explanation. Handle `AudioLoss`, `Clipped`, `RecoverableError`,
`ShutdownError` events, plus device `DefaultCleared` /
`RescanRequired`. `DefaultChanged.kind` is now `DefaultDeviceKind`. Loss marks
subsequent output `DISCONTINUITY`; `PADDED=8` and `CLIPPED=16` describe output
integrity. Peak/RMS reflect delivered samples after denoise/gain in every binding.

For Node.js, use bigint arithmetic for u64 counters (including VAD `seq`,
chunk-drop and loss counts); PTS stays number. Chunk interfaces require
`frameIndex`, and event types are closed discriminated unions. Lifecycle getters
`terminalError()` / `shutdownReport()` return `undefined` when absent. Callback
exceptions surface as `uncaughtException`.

For Python, core failures raise typed exceptions carrying `.audio_error`:
`InvalidArgumentError` / `UnsupportedFormatError` are `ValueError` subclasses;
others are `RuntimeError` subclasses. `terminal_error()` returns `AudioError`.
Discovery/watcher failures raise; `stop()` and normal context exit may raise
cleanup failures. If the body already failed, it stays primary with cleanup
context attached. Drain queued output to receive the denoise tail after stop.

For C, v1 physical layouts and legacy constants remain, but enum fields are
`int32_t` and input booleans are `uint8_t`. Unknown discriminants are invalid
arguments. Handle all negative root result codes, including the new -5..-12
codes, and use opaque v2 error/event/watcher/shutdown accessors for details.

The CLI continues on transient failures, loss and clipping, fails on terminal
or cleanup errors, and prints structured permission detail after safe guidance.

---

## Microphone + system mix

`SourceKind::Mix` combines microphone input and system output into one stream.
Select each side with `mix_mic_device_id` and `mix_system_device_id`, using IDs
from `devices()`; `None` selects the default input/output. `device_id` is ignored
for Mix. The linear pre-mix gains `mix_mic_gain` and `mix_system_gain` default to
`1.0`; `gain` is the global multiplier applied after mixing.

```rust
use flexaudio::{open, SourceKind, StreamConfig};

let mut stream = open(StreamConfig {
    kind: SourceKind::Mix,
    mix_mic_gain: 1.0,
    mix_system_gain: 0.5,
    ..Default::default()
})?;
stream.start()?;
stream.stop();
# Ok::<(), flexaudio::Error>(())
```

---

## Microphone startup samples

At each open, recovery reopen, and source switch, microphone capture (including
its Mix lane) suppresses a leading invalid prefix: non-finite samples or samples
outside [-1, 1]. All channels must remain valid for 10 ms of native
frames; delivery includes the start of that confirmation window. Valid input
from the first frame, including zeros, is buffered for confirmation and replayed
unchanged; an entirely valid shorter input is also delivered when stopped or when its
generation is replaced during confirmation. Once
startup is confirmed, samples are untouched; system/process loopback is not filtered.

The first delivered chunk after a drop carries `DISCONTINUITY`. Discarded frames
advance the capture clock and frame indices rather than compacting recording
time. `StartupSamplesDropped { frames, reason: OutOfRange }` reports once per
affected microphone generation (native frame count), including in Mix. Node.js
uses `startupSamplesDropped` with bigint `frames` and `reason: 'outOfRange'`;
Python uses the same tag and integer count. C v2 exposes event kind 15 and typed
frame/reason accessors; the CLI prints a safe warning. The startup notice precedes
that generation's first Node.js `onChunk` callback and attached VAD
`epochStart`. Provide `onEvent` to receive the Node.js notice. Recovered and other
advisories use independent callbacks and may arrive before or after the startup
notice; this ordering guarantee covers only startup notice versus affected PCM.
For Rust, Python and C pull APIs, the notice is enqueued chronologically before
the generation's first chunk is published. Event-first callers drain the ordinary
FIFO event queue before PCM to observe that notice first. PCM polling does not
wait for event polling; PCM-only callers continue to receive chunks, including
after stop. C v1 projects the advisory as `Unknown` with count zero.

If capture fails terminally while holding a valid native startup prefix, `AudioLoss`
with reason `generationEnded` reports the discarded scalar interleaved samples
at their native sample rate and channel count. After invalid samples, if valid input is not
confirmed within two seconds, capture terminates with an actionable typed
backend error. No fixed period of valid audio is discarded.

---

## Windows system capture at idle

Windows system capture without `exclude_self` / `exclude_pids` keeps delivering
continuous device-clocked silence when no application is playing audio,
including idle time at the start of capture. Classic WASAPI loopback keeps a
shared-mode render stream on the same output endpoint active with silence only,
using the endpoint's mix format. It does not change endpoint volume or mute.
Failure to create or start this silent stream fails capture start; a runtime
failure terminates capture with a typed `Event::TerminalError` and stops both
streams. It does not enter stall/reopen recovery; open a new stream to retry.
Process loopback, including system capture with exclusions, does not use it.

Keepalive is enabled by default. Disable it in Rust with
`open_with_options(config, OpenOptions::new().windows_classic_loopback_keepalive(false))`.
`Stream::switch_source_with_options(config, options)` replaces it with the source;
ordinary `switch_source` retains it, and failed switches retain the previous options.
Node uses `windowsClassicLoopbackKeepalive: false`, Python
`windows_classic_loopback_keepalive=False`, C uses the v3 envelope field
`windows_classic_loopback_keepalive = 2` (0 default/retain, 1 enabled), and the CLI
uses `--no-classic-loopback-keepalive`. Node/Python switches retain it when omitted;
C switches retain it with 0. The option also applies to the system lane of Mix,
and is ignored on other platforms and process/exclusion paths.

With keepalive disabled, idle classic loopback delivers no packets. The unchanged
2 s watchdog reports `Event::StreamStalled` and reopens repeatedly until audio
plays. Silent rendering and its Windows Mixer session are absent on this path.

The capturing process can appear as an audio session in Windows Volume Mixer
while capturing, and possibly briefly afterward. The silent render stream creates
an active rendering session; Microsoft documents Mixer controls for active and
recently active rendering sessions in [Audio Sessions](https://learn.microsoft.com/en-us/windows/win32/coreaudio/audio-sessions).
The exact display and removal timing depend on the Windows Mixer UI.

---

## Excluding playback by PID

`StreamConfig::exclude_pids` excludes playback from **system capture and the
system side of Mix**, combined with `exclude_self`. Microphone and per-process
sources ignore valid exclusions. The same controls are available in each binding:

| Surface | PID exclusion |
|---|---|
| Rust | `StreamConfig { exclude_pids: vec![1234], ..Default::default() }` |
| N-API | `openStream({ kind: 'system', excludePids: [1234] }, onChunk)` |
| C | `flexaudio_open_with_exclude_pids(&config, pids, count)`; source switching uses `flexaudio_switch_source_with_exclude_pids` |
| Python | `flexaudio.open("system", exclude_pids=[1234])`; also accepted by `Stream.switch_source()` |
| CLI | `flexaudio-cli --source system --exclude-pid 1234`; repeat `--exclude-pid` for multiple PIDs |

- **Windows:** exclusion covers one process tree root. With `exclude_self`,
  the root is the calling process; otherwise it is the first listed PID.
  Every listed PID must equal that root; distinct PIDs are rejected with
  `Error::InvalidArg`, even if they are descendants. Pass the root once.
  The requested system device is not used while exclusion is active because
  WASAPI process loopback cannot target an output endpoint.
- **macOS:** PIDs are resolved to Core Audio process objects once at capture
  start (a snapshot). Unresolved exclusions fail closed, including live processes
  without an audio object. Only a process confirmed gone (`ESRCH`) is omitted.
  Reopen capture when a new audio helper appears. PIDs must fit `1..=2147483647`.
  A requested system device is honored alongside exclusion.
- **Linux:** exact PID matching, without descendants. Pulse-proxied streams
  use `application.process.id`; native clients use `pipewire.sec.pid`.
  Pulse-proxied streams are not captured during exclusion until their PID is
  known. The requested system device is not used while exclusion is active:
  application-stream fan-in is not device-scoped.
  Eligible process/exclusion capture targets must negotiate format within two
  seconds or opening fails with a typed, actionable backend error. A completed
  target snapshot with no eligible source opens successfully and keeps watching.

The device rules also apply to `mix_system_device_id`; microphone selection
is unaffected.

---

<a id="listing-capturable-processes"></a>

## Listing capturable processes

`flexaudio::processes()` lists audio output session/stream owners on
Linux/Windows. On macOS, it lists all processes known to Core Audio, including
input-only processes; use `is_output_active` to check current playback. Pass a
PID to per-process capture (`SourceKind::ProcessLoopback` with `target_pid`).
The calling process is excluded, entries are deduplicated by PID, and the list
is sorted with actively-playing processes first, then by name, then by PID.

```rust
use flexaudio::{open, processes, SourceKind, StreamConfig};

for p in processes()? {
    println!("{:>7} {} {:?} active={:?}", p.pid, p.name, p.executable, p.is_output_active);
}
let target = processes()?.into_iter().next();
if let Some(p) = target {
    let mut stream = open(StreamConfig {
        kind: SourceKind::ProcessLoopback,
        target_pid: Some(p.pid),
        ..Default::default()
    })?;
    stream.start()?;
    stream.stop();
}
# Ok::<(), flexaudio::Error>(())
```

| Field / platform | Linux (PipeWire) | Windows (WASAPI) | macOS (Core Audio) |
|---|---|---|---|
| What is listed | Clients that own a `Stream/Output/Audio` node | Audio sessions on every active render endpoint (system-sounds and expired sessions skipped) | Process objects Core Audio knows about (`kAudioHardwarePropertyProcessObjectList`; includes input-only processes) |
| `pid` | `application.process.id` for pulse-proxied streams; `pipewire.sec.pid` for native clients (the same resolution the capture backend uses) | `IAudioSessionControl2::GetProcessId` | `kAudioProcessPropertyPID` |
| `name` | `application.name` of the node, else of the client | Image file name without `.exe` | Executable name |
| `executable` | Basename of `/proc/<pid>/exe`, falling back to `/proc/<pid>/comm` when `exe` is unreadable | Basename of the process image | Basename from `proc_pidpath` |
| `bundle_id` | — | — | `kAudioProcessPropertyBundleID` |
| `is_output_active` | Node state is `Running` | Session state is `Active` | `kAudioProcessPropertyIsRunningOutput` |
| Requirement | A running PipeWire session | Windows build 20348 or later (Windows 11 / Windows Server 2022) | macOS 14.4+, otherwise `Error::UnsupportedOsVersion` |

`name` is always non-empty (falling back to the executable, the bundle ID, then
`pid <N>`); `executable`, `bundle_id`, and `is_output_active` are `None` when the
OS does not expose them. Names are self-reported by applications and are for
display only — the PID is the key.

How to read the result:

- `Ok(non-empty)` — per-process capture is available, and there are audio output
  session/stream owners (Linux/Windows) or Core Audio processes (macOS, including
  input-only processes). Idle/stopped processes are listed; use
  `is_output_active` to check current playback.
- `Ok(empty)` — per-process capture is available, but no such process exists
  right now (this is **not** "nothing is playing").
- `Err(..)` — per-process capture is not available here (Linux: PipeWire is not
  reachable → `Error::Backend`; macOS before 14.4 or Windows build before 20348
  → `Error::UnsupportedOsVersion`; other OSes → `Error::Unsupported`),
  permission was denied (`Error::PermissionDenied`), the OS did not answer
  in time, or a previous enumeration is still in progress (`Error::Backend`).

The call is read-only, never triggers a permission prompt, and is bounded: it
returns within 3 seconds even if the OS audio service hangs. The N-API binding
exposes `await processes()` and `await stream.stop()` so they do not block the
JS event loop.

---

## OS-specific permission requirements

Applications must declare the usage descriptions or capabilities required by
their host OS. Confirmed recording denial returns
`Error::PermissionDenied { permission, detail }`; safe `Display` identifies the
permission and gives platform privacy-setting/restart guidance. The original
detail is available through structured access; the CLI prints it explicitly. A denial detected during capture emits
`Event::PermissionDenied { permission, detail }`, terminates capture (both lanes
for Mix), and suppresses further audio and automatic reopening. Create a new
stream after correcting permission; `start`, `resume`, and `switch_source` on a
terminally failed stream return its stored error.

Built-in backends do not emit `PermissionPending`; that event remains available
for custom Rust backends. `PermissionGranted` is removed in 0.6.0; its C event
number is reserved. A macOS authorization-query
failure returns a typed backend error before capture starts; it does not invent
a permission denial. A microphone configuration that
differs from its advertised native format is rejected before capture builds with
`Error::NativeFormatChanged { advertised, actual }`; recreate the stream using the
current device format instead of delivering incorrectly interpreted samples.

Rust exposes `Stream::terminal_error()`. N-API exposes `terminalError()` and
rejects `stop()` on capture or cleanup failure; provide `onEvent` for immediate
notifications and inspect `shutdownReport()`. Python `poll_chunk()` raises a typed
`RuntimeError` subclass on terminal failure; `terminal_error()` returns the
stored `AudioError` without consuming events. C v1 `flexaudio_poll_chunk()` returns
`FLEX_FAILURE` (-2) for a terminal capture error. C v2 `flexaudio_poll_chunk_v2()`
and `flexaudio_terminal_error()` return the primary root result code (for example,
`FLEX_PERMISSION_DENIED` (-7) or `FLEX_FAILURE` (-2) for a backend failure), with
the safe explanation in `flexaudio_last_error()`. Permission event kind remains 3 in C. N-API/Python
permission events keep type `permissionDenied` and add `permission`
(`microphone` or `systemAudio`) and `message`.

### macOS

- **Microphone** (including the mic lane of Mix): flexaudio checks AVFoundation
  authorization before any HAL/device-format query. Denied or restricted access
  fails with a permission error. With undecided authorization and a nonempty
  `NSMicrophoneUsageDescription`, flexaudio requests consent and waits at most
  30 seconds. An unanswered prompt returns a typed `Backend` error without
  starting capture; answer the prompt and retry. Opening is synchronous, so GUI
  hosts should call it on a worker thread. The bound covers the undecided consent
  wait, not arbitrary native queries after authorization.
- Undecided authorization without a main-bundle microphone usage description
  fails immediately, avoiding an unbounded responsible-host HAL prompt and an
  unsafe direct AVFoundation request. Use a configured application bundle, or
  grant the responsible host microphone access before retrying. An unanswered
  prompt is not a permission denial. From 0.5.3, each microphone capture start
  requires decided consent. Microphone capture does not continuously monitor
  authorization changes or permission revocation after capture starts. In 0.5.2,
  `ConsentPoll` was wired for pending `NotDetermined` consent when preflight
  returned `Ok(true)`, but an earlier `query_native_format` could block before
  reaching the capture owner and its monitor. Initially `Authorized` capture
  had no continuous revocation monitor. Native capture failures follow the
  backend's normal error path.
- **System and per-process audio** use Core Audio process taps (macOS 14.4+).
  Add a usage description to your app's `Info.plist`:
  ```xml
  <key>NSAudioCaptureUsageDescription</key>
  <string>This app records system and application audio.</string>
  ```
  The OS controls the consent prompt. There is no public system-audio permission
  status API, and flexaudio does not use private TCC APIs. A native illegal
  operation is mapped to a `SystemAudio` permission error as a best-effort
  diagnosis; this native result is not exclusive to consent failures.
- A tap may deliver zeros without a native permission error. After five
  continuous seconds of bit-exact zero native samples while Core Audio reports
  another eligible captured process with `IsRunningOutput` true (honoring
  exclusions and device selection), flexaudio runs an active self-probe once per
  capture generation. Zero samples plus output activity alone cannot distinguish
  missing recording permission from genuine digital silence.
- The self-probe renders a roughly 300 ms, phase-coded 1 kHz signal at amplitude
  `1e-5` on the default output, designed to be inaudible. A separate private tap
  captures only our own process to check for that diagnostic signal. Recognizing
  the signal suppresses the silence warning. If native output callbacks prove
  the signal was submitted while the diagnostic tap continuously captures only
  exact zeros, flexaudio emits `Event::PermissionDenied` for `SystemAudio` and
  terminates capture, including both lanes of Mix. The existing binding denial
  event and terminal-error APIs retain the cause and privacy-setting remedy.
  The diagnostic signal can appear in your recording if your capture includes
  our own process (for example, system capture with `excludeSelf: false`).
- The probe is enabled by default. Set Rust `StreamConfig.silence_probe: false`,
  Node.js `silenceProbe: false`, Python `silence_probe=False`, or the C open option
  to disable diagnostic signal playback. This opt-out applies to system,
  per-process and the system lane of Mix capture; microphone consent is unchanged.
- If the self-probe cannot establish either outcome (for example, setup failure,
  missing render/capture callbacks, or timeout), flexaudio emits
  `Event::SilenceWhileSourceActive { detail }` once for that generation
  (N-API/Python type `silenceWhileSourceActive`; C event kind 7) and continues
  capture. This advisory explains that permission may be missing or the source
  may be genuinely silent. Query failures, unknown device routing, inactive
  sources, absent samples, or nonzero/negative-zero samples prevent the initial
  trigger. Stopping capture cancels the probe and suppresses late notifications.
  Absence of a denial or advisory does not establish that permission was granted.
- Check **System Settings > Privacy & Security > Microphone** for microphone
  access and **Screen & System Audio Recording** for **System Audio Recording**.
  Enable access for the responsible host app (for example Terminal), restart
  that app, and retry with a new stream.

### Windows

- Microphone capture, including the mic lane of Mix, checks the public
  `AppCapability` consent API. User/system denial becomes a `Microphone`
  permission error. Consent is checked again after native stream build/play
  failures. Unsupported/ambiguous statuses or query failures permit the native
  capture attempt but are not treated as proof that access is authorized.
- Enable **Settings > Privacy & security > Microphone**, including
  **Let desktop apps access your microphone**, then restart the host app and
  retry with a new stream. An administrator-controlled restriction may require
  the administrator to change the policy.
- System (WASAPI loopback) and per-process loopback capture use the standard
  WASAPI render-endpoint loopback / process-loopback APIs (Windows 10/11).
  Desktop loopback capture does not use the microphone consent preflight.

### Linux

- Microphone capture goes through ALSA/PipeWire via `cpal`; the user must have
  access to the audio device (typically the `audio` group / a running PipeWire
  or PulseAudio session).
- System and per-process capture require a running **PipeWire** session. If
  PipeWire is absent or watcher startup fails, `watch_devices()` returns an error.
  Device discovery requires a collection from every provider; a collection/count
  failure returns an error. Under a portal-based desktop, the user may be
  prompted to grant capture access.

---

## Supported Rust version (MSRV)

- **Core / facade / OS backends / mic:** Rust **1.85**.
- **`flexaudio-vad`, `flexaudio-napi`, `flexaudio-ffi`, and `flexaudio-py`:**
  Rust **1.91** (required by `tract-onnx` 0.23.7).

The workspace pins MSRV via `rust-version` in each crate.

---

## Versioning policy (SemVer / 0.x)

flexaudio follows [Semantic Versioning](https://semver.org/). While the crate is
in the **0.x** series, the public API is **not yet stable**: per SemVer, a bump
of the **minor** version (`0.2 → 0.3`) may contain breaking changes, while
**patch** bumps (`0.2.0 → 0.2.1`) are backward-compatible. Pin to `0.6` to opt
into compatible updates only. See [`CHANGELOG.md`](CHANGELOG.md).

---

## Workspace layout

| Crate | crates.io | Description |
|-------|-----------|-------------|
| `flexaudio` | ✅ | Facade: unified `open()` / `devices()` / `processes()` / `watch_devices()`. |
| `flexaudio-core` | ✅ | Source-agnostic stream engine, types, resampling/normalizer. |
| `flexaudio-mic` | ✅ | Microphone backend (cpal), all platforms. |
| `flexaudio-os-linux` | ✅ | PipeWire system / per-process backend (Linux). |
| `flexaudio-os-windows` | ✅ | WASAPI loopback / process backend (Windows). |
| `flexaudio-os-macos` | ✅ | Core Audio process-tap backend (macOS). |
| `flexaudio-vad` | ✅ | Silero VAD add-on (offline, embedded model). |
| `flexaudio-encode` | ✅ | Streaming FLAC encoding (flacenc). |
| `flexaudio-denoise` | ✅ | RNNoise noise suppression (nnnoiseless). |
| `flexaudio-cli` | — | Reference CLI / streaming capture tool. |
| `flexaudio-napi` | — (npm) | Node.js N-API addon (published on npm as `@studio-sadola/flexaudio`). |
| `flexaudio-ffi` | — | C ABI (pull-based capture, VAD / FLAC / denoise, `flexaudio_processes`). |
| `bindings/flexaudio-py` | — | PyO3 Python binding (`open` / `devices` / `processes` / add-ons). |

The nine crates marked ✅ are published on crates.io. The workspace has 13 members:

| Workspace member |
|---|
| `crates/flexaudio-core` |
| `crates/flexaudio-os-windows` |
| `crates/flexaudio-os-macos` |
| `crates/flexaudio-os-linux` |
| `crates/flexaudio-mic` |
| `crates/flexaudio` |
| `crates/flexaudio-cli` |
| `crates/flexaudio-ffi` |
| `crates/flexaudio-napi` |
| `crates/flexaudio-vad` |
| `crates/flexaudio-encode` |
| `crates/flexaudio-denoise` |
| `bindings/flexaudio-py` |

---

## License

[MIT](LICENSE) © 2026 tubome / Studio Sadola.

This project bundles / links third-party software (Silero VAD model and
pure-Rust tract inference for VAD, RNNoise via nnnoiseless for denoise,
PipeWire, and other Rust crates). See
[`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md) for the required notices.
