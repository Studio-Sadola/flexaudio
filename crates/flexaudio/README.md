# flexaudio

**Flexible, cross-platform audio capture for Rust.** Capture from microphones,
system output (loopback), individual processes, and microphone + system mix on **Linux**, **Windows**,
and **macOS** through one unified API.

```rust
use flexaudio::{open, StreamConfig, SourceKind};

let mut stream = open(StreamConfig {
    kind: SourceKind::Mic,
    ..Default::default()
})?;
stream.start()?;
while let Some(chunk) = stream.poll_chunk() {
    let _ = chunk; // interleaved f32, plus frames / peak / rms / seq
}
stream.stop();
# Ok::<(), flexaudio::Error>(())
```

## Capability matrix

| Source            | Linux         | Windows               | macOS                 |
|-------------------|---------------|-----------------------|-----------------------|
| Microphone        | ✅ cpal/ALSA  | ✅ cpal/WASAPI        | ✅ cpal/CoreAudio     |
| System output     | ✅ PipeWire   | ✅ WASAPI loopback    | ✅ CoreAudio taps     |
| Per-process       | ✅ PipeWire   | ✅ WASAPI process     | ✅ CoreAudio taps     |

## Highlights

- `open(StreamConfig)` selects the right backend by source + OS.
- `Stream::poll_chunk` / `poll_event` — simple pull loop; no callbacks required.
- `Stream::switch_source` — hot-swap source without stopping the stream.
- `devices()` / `watch_devices()` — enumeration and hotplug notifications.
- `processes()` lists audio-session/stream owners (including idle/stopped
  processes), excluding the caller; use `pid` as `target_pid` for capture and
  `is_output_active` to check current playback.
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

## Install

```sh
cargo add flexaudio@0.3
```

## Permissions

Audio capture requires user consent: macOS TCC (`kTCCServiceAudioCapture`, add
`NSAudioCaptureUsageDescription` to `Info.plist`), the Windows Microphone privacy
setting, and a running PipeWire session on Linux for system/process capture. See
the [workspace README](https://github.com/Studio-Sadola/flexaudio#os-specific-permission-requirements).

On macOS, system/process loopback (Core Audio process taps) requires macOS 14.4
or later.

## MSRV

Rust **1.85**.

## License

[MIT](LICENSE) © 2026 tubome / Studio Sadola. Third-party notices:
[`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md).
