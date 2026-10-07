# flexaudio

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

---

## Install

```toml
[dependencies]
flexaudio = "0.3"
```

or:

```sh
cargo add flexaudio@0.3
```

The Voice Activity Detection add-on is a separate crate:

```sh
cargo add flexaudio-vad
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
- `Stream::start` / `Stream::stop` — control capture.
- `Stream::poll_chunk` / `Stream::poll_event` — pull `AudioChunk`s and `Event`s.
- `Stream::switch_source` — hot-swap the input source without stopping the
  stream (chunk `seq` stays continuous; the first chunk after a switch carries a
  discontinuity flag).
- `flexaudio::devices() -> Result<Vec<DeviceInfo>>` — enumerate microphones
  (cpal, all platforms) and system output endpoints (Linux: PipeWire sinks and
  sources; Windows: active render endpoints; macOS: output devices) in one list.
- `flexaudio::processes() -> Result<Vec<ProcessInfo>>` — list audio output
  session/stream owners on Linux/Windows and Core Audio processes on macOS,
  including input-only processes (see
  [Listing capturable processes](#listing-capturable-processes)). Idle/stopped
  processes are included; use `is_output_active` to check current playback and
  `pid` as `target_pid` for per-process capture.
- `flexaudio::watch_devices() -> Result<DeviceWatcher>` — pull-style hotplug
  (added / removed / default-changed) notifications (Linux only; Windows/macOS
  return a no-op watcher).
- Re-exported types: `StreamConfig`, `SourceKind`, `ProcessMode`, `OutputFormat`,
  `AudioChunk`, `SecondaryChunk`, `ChunkFlags`, `DeviceInfo`, `ProcessInfo`,
  `DeviceEvent`, `Event`, `Error`, `Result`.

Voice activity detection (`flexaudio-vad`): `Vad::new` / `Vad::process` for
streaming `SpeechStart` / `SpeechEnd` events, and `get_speech_timestamps` for
batch segmentation. The Silero VAD model is embedded in the binary, so VAD runs
fully offline with no runtime model file or network access.

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

## Windows system capture at idle

Windows system capture without `exclude_self` / `exclude_pids` keeps delivering
continuous device-clocked silence when no application is playing audio,
including idle time at the start of capture. Classic WASAPI loopback keeps a
shared-mode render stream on the same output endpoint active with silence only,
using the endpoint's mix format. It does not change endpoint volume or mute.
Failure to create or start this silent stream fails capture start; a runtime
failure stops capture production and follows the existing stall/reopen error path.
Process loopback, including system capture with exclusions, does not use it.

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
  start (a snapshot). A process without an audio object then is not excluded;
  reopen capture when a new audio helper appears. Failed lookups fail capture
  unless the process has exited. PIDs must fit `1..=2147483647`.
  A requested system device is honored alongside exclusion.
- **Linux:** exact PID matching, without descendants. Pulse-proxied streams
  use `application.process.id`; native clients use `pipewire.sec.pid`.
  Pulse-proxied streams are not captured during exclusion until their PID is
  known. The requested system device is not used while exclusion is active:
  application-stream fan-in is not device-scoped.

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

flexaudio captures audio; every platform gates this behind user permission. Your
application is responsible for triggering the relevant prompt / declaring the
required entitlements.

### macOS

System and per-process audio capture use Core Audio process taps, which are
gated by the **TCC** privacy subsystem under `kTCCServiceAudioCapture`.

- Add a usage-description string to your app's `Info.plist`:
  ```xml
  <key>NSAudioCaptureUsageDescription</key>
  <string>This app records system and application audio.</string>
  ```
  (Microphone-only capture additionally requires `NSMicrophoneUsageDescription`.)
- The OS shows a one-time consent prompt; until the user approves, capture
  surfaces as a `PermissionDenied` event / error.
- Process taps require macOS 14.4 or later.

### Windows

- Microphone capture is gated by the **Microphone** privacy setting
  (Settings → Privacy & security → Microphone). If capture fails, check that
  access is enabled for your app. The cpal microphone backend maps an unavailable
  device or default input configuration to `Error::DeviceNotFound`, and input
  stream construction failures to `Error::Backend`; it does not map privacy
  denial to `Error::PermissionDenied`.
- System (WASAPI loopback) and per-process loopback capture use the standard
  WASAPI render-endpoint loopback / process-loopback APIs (Windows 10/11). No
  special manifest capability is required for a desktop app, but the microphone
  privacy gate still applies to mic capture.

### Linux

- Microphone capture goes through ALSA/PipeWire via `cpal`; the user must have
  access to the audio device (typically the `audio` group / a running PipeWire
  or PulseAudio session).
- System and per-process capture require a running **PipeWire** session. If
  PipeWire is absent, `devices()` still returns microphones found by cpal; only
  the PipeWire devices are missing. `watch_devices()` degrades to a no-op rather
  than failing. Under a portal-based desktop, the user may be prompted to grant
  capture access.

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
**patch** bumps (`0.2.0 → 0.2.1`) are backward-compatible. Pin to `0.3` to opt
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
