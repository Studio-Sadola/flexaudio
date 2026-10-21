# flexaudio

**Flexible, cross-platform audio capture for Rust.** Capture from microphones,
system output (loopback), individual processes, and microphone + system mix on **Linux**, **Windows**,
and **macOS** through one unified API.

```rust
use flexaudio::{open, StreamConfig, SourceKind};

let mut stream = open(StreamConfig::new(SourceKind::Mic))?;
stream.start()?;
while let Some(chunk) = stream.poll_chunk() {
    let _ = chunk; // interleaved f32, plus frames / peak / rms / seq
}
stream.stop();
# Ok::<(), flexaudio::Error>(())
```

## Capability matrix

In 0.9, `AudioChunk`, `DeviceInfo` and `StreamConfig` are `#[non_exhaustive]`.
Use `AudioChunk::new(data, frames)`, `DeviceInfo::new(id, name, source_kind)` or
`StreamConfig::new(kind)`, then assign customized public fields.
`StreamConfig::default()` remains available; destructuring requires `..`.

| Source            | Linux         | Windows               | macOS                 |
|-------------------|---------------|-----------------------|-----------------------|
| Microphone        | ✅ cpal/ALSA  | ✅ cpal/WASAPI        | ✅ cpal/CoreAudio     |
| System output     | ✅ PipeWire   | ✅ WASAPI loopback    | ✅ CoreAudio taps     |
| Per-process       | ✅ PipeWire   | ✅ WASAPI process     | ✅ CoreAudio taps     |

## Highlights

- `open(StreamConfig)` selects the right backend by source + OS.
- `Stream::poll_chunk` / `poll_event` — simple pull loop; no callbacks required.
- `Stream::switch_source` — hot-swap source without stopping the stream.
- `devices()` enumerates devices; `watch_devices()` reports Linux PipeWire system
  output additions, removals and default changes using the same output IDs as
  `devices()`. Microphone and default-microphone changes are not reported: CPAL
  microphone IDs cannot be resolved from PipeWire nodes. Windows/macOS return a
  no-op watcher.
  Discovery returns a complete inventory or an error, including when PipeWire is
  unavailable. Linux microphone entries use only CPAL device-name IDs; PipeWire
  source-node IDs are omitted because the microphone backend cannot open them.
- `processes()` lists audio output session/stream owners on Linux/Windows and
  Core Audio processes on macOS, including input-only processes. Idle/stopped
  processes are included and the caller is excluded; use `pid` as `target_pid`
  for capture and `is_output_active` to check current playback.
- `SourceKind::Mix` combines mic + system audio. Select each side with
  `mix_mic_device_id` / `mix_system_device_id` (`None` uses defaults), and set
  `mix_mic_gain` / `mix_system_gain` before mixing; `gain` applies afterward.
  `device_id` is ignored for Mix.
- `exclude_pids` combines with `exclude_self` for system capture and the system
  side of Mix. Windows excludes one process-tree root (self when `exclude_self`
  is true, otherwise the first PID); every entry must equal that root, and
  distinct PIDs are rejected. macOS resolves audio objects once at start and
  honors device selection. Linux matches exact PIDs (`application.process.id`
  for pulse-proxied streams, `pipewire.sec.pid` for native clients), without
  descendants. Linux and Windows ignore system-device selection during exclusion.
- Output is normalized interleaved `f32` at a sample rate / channel count you
  choose (two-stage resampling internally).

## Capability discovery

`capabilities()` returns a read-only snapshot of source backends and availability,
OS version when known, PID/exclusion scopes and limits, output-device selection
semantics, watcher support, and optional rendering side effects. It opens no
capture or render streams, enumerates no microphones, requests no recording
permission, and plays no audio.

```rust
let support = flexaudio::capabilities();
println!("{:?}", support.sources.system.availability);
println!("{:?}", support.system_exclusions);
```

`Availability::Supported` describes backend prerequisites, not permission or a
usable device. `Availability::Unknown { error }` retains a typed prerequisite
query failure; do not assume support or confirmed unavailability. On Linux,
missing PipeWire sockets are unavailable; failed socket inspection, connection
or server sync is unknown. The bounded query can wait for the audio service;
run it on a worker thread in GUI applications.

## Capture open options

Use `open_with_options` to select optional backend behavior without changing
`StreamConfig`. Existing `open` calls keep the default behavior.

```rust
use flexaudio::{open_with_options, OpenOptions, SourceKind, StreamConfig};

let mut config = StreamConfig::new(SourceKind::SystemLoopback);
config.exclude_self = false;
let mut stream = open_with_options(
    config.clone(),
    OpenOptions::new().windows_classic_loopback_keepalive(false),
)?;
stream.start()?;
stream.switch_source_with_options(config, OpenOptions::new())?;
stream.stop();
# Ok::<(), flexaudio::Error>(())
```

Windows classic loopback keepalive is enabled by default and renders silence to
keep idle system capture delivering packets. It also applies to the system lane
of Mix without PID exclusions. Other platforms and process/exclusion paths ignore
it. Disabling it means idle classic loopback delivers no packets: the 2 s watchdog
reports `Event::StreamStalled` and reopens repeatedly until audio plays.

`Stream::open_options()` reads the current settings. `switch_source_with_options`
replaces them together with the source; ordinary `switch_source` retains them.
Construction/startup failures retain the previous settings.

## Settings and metadata in 0.8

- `OpenOptions::new().strict(true)` rejects non-default settings that would be ignored
  before native access. `Stream::effective_settings()` exposes the last successful
  facade open/switch report, including after stop; custom-backend streams return
  `None`. Failed switches retain the previous report.
- `resolve_settings(&SettingsRequest)` resolves settings without device access or
  model loading. `SettingsAttachments` describes processing attachments and switch
  policy; it does not install those attachments. The resolver reports applicability
  but does not validate capture arguments or enforce strictness itself.
- `Stream::poll_event()` carries `Event::SilenceProbeStarted`,
  `Event::SilenceProbeStopped`, `Event::ClassicLoopbackKeepaliveStarted`, and
  `Event::ClassicLoopbackKeepaliveStopped`. Every emitted Started has exactly one
  Stopped with a reason. Stopped ends helper ownership after teardown attempts;
  it does not guarantee native cleanup succeeded.
- `AudioChunk::arrival_ns()` and `SecondaryChunk::arrival_ns()` alias `pts_ns`
  exactly. These arrival labels are clamped non-decreasing; use `frame_index`
  for audio continuity. The old naming is documentation-only deprecated since
  0.8 and retained throughout 0.9.
- `devices_with_namespaces()` preserves selectable device IDs and adds provider
  provenance; namespace tags do not promise persistent hardware identity.
- `StreamConfig::ring_capacity_chunks` sets the queue capacity at open; the default
  is 50 chunks.

## Install

```sh
cargo add flexaudio@0.9
```

## Permissions

Audio capture requires user consent: macOS TCC (`kTCCServiceAudioCapture`, add
`NSAudioCaptureUsageDescription` to `Info.plist`), the Windows Microphone privacy
setting, and a running PipeWire session on Linux for system/process capture. See
the [workspace README](https://github.com/Studio-Sadola/flexaudio/blob/v0.9.0/README.md#os-specific-permission-requirements).

On macOS, system/process loopback (Core Audio process taps) requires macOS 14.4
or later.

## MSRV

Rust **1.85**.

## License

[MIT](LICENSE) © 2026 tubome / Studio Sadola. Third-party notices:
[`THIRD_PARTY_NOTICES.md`](https://github.com/Studio-Sadola/flexaudio/blob/v0.9.0/THIRD_PARTY_NOTICES.md).
