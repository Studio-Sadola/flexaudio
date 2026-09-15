# flexaudio

**English** | [日本語](README.ja.md)

**General-purpose, flexible, cross-platform audio capture for Rust.**

`flexaudio` provides one unified API for capturing audio from **microphones**,
**system output (loopback)**, and **individual processes** — across **Linux**,
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

---

## Install

```toml
[dependencies]
flexaudio = "0.3"
```

or:

```sh
cargo add flexaudio
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
- `flexaudio::processes() -> Result<Vec<ProcessInfo>>` — list the processes that
  have an audio output session/stream and can be captured per process (see
  [Listing capturable processes](#listing-capturable-processes)). Idle/stopped
  processes are included; whether something is playing now is `is_output_active`.
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

## Listing capturable processes

`flexaudio::processes()` returns the processes you can hand to per-process
capture (`SourceKind::ProcessLoopback` with `target_pid`). The calling process is
excluded, entries are deduplicated by PID, and the list is sorted with
actively-playing processes first, then by name, then by PID.

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
| `pid` | The client's `pipewire.sec.pid` (the same resolution the capture backend uses) | `IAudioSessionControl2::GetProcessId` | `kAudioProcessPropertyPID` |
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

- `Ok(non-empty)` — per-process capture is available, and there are processes
  that have an audio output session/stream. Idle/stopped processes are listed;
  whether something is playing now is `is_output_active`.
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
  (Settings → Privacy & security → Microphone); a denied app yields
  `PermissionDenied`.
- System (WASAPI loopback) and per-process loopback capture use the standard
  WASAPI render-endpoint loopback / process-loopback APIs (Windows 10/11). No
  special manifest capability is required for a desktop app, but the microphone
  privacy gate still applies to mic capture.

### Linux

- Microphone capture goes through ALSA/PipeWire via `cpal`; the user must have
  access to the audio device (typically the `audio` group / a running PipeWire
  or PulseAudio session).
- System and per-process capture require a running **PipeWire** session. If
  PipeWire is absent, `devices()` returns an empty list and `watch_devices()`
  degrades to a no-op rather than failing. Under a portal-based desktop, the
  user may be prompted to grant capture access.

---

## Supported Rust version (MSRV)

- **Core / facade / OS backends / mic:** Rust **1.85**.
- **`flexaudio-vad` and `flexaudio-napi`:** Rust **1.88** (required by their
  `ort` / `napi-build` toolchain dependencies).

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
| `flexaudio-cli` | — | Reference CLI / streaming capture tool. |
| `flexaudio-napi` | — (npm) | Node.js N-API addon (published to npm, not crates.io). |
| `flexaudio-ffi` | — | C ABI (pull-based capture, VAD / FLAC / denoise, `flexaudio_processes`). |
| `bindings/flexaudio-py` | — | PyO3 Python binding (`open` / `devices` / `processes` / add-ons). |

---

## License

[MIT](LICENSE) © 2026 tubome / Studio Sadola.

This project bundles / links third-party software (Silero VAD model, ONNX
Runtime, PipeWire, and permissively-licensed Rust crates). See
[`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md) for the required notices.
