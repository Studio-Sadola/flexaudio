# flexaudio

**Flexible, cross-platform audio capture for Rust.** Capture from microphones,
system output (loopback), and individual processes on **Linux**, **Windows**,
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
- Output is normalized interleaved `f32` at a sample rate / channel count you
  choose (two-stage resampling internally).

## Install

```sh
cargo add flexaudio
```

## Permissions

Audio capture requires user consent: macOS TCC (`kTCCServiceAudioCapture`, add
`NSAudioCaptureUsageDescription` to `Info.plist`), the Windows Microphone privacy
setting, and a running PipeWire session on Linux for system/process capture. See
the [workspace README](https://github.com/tsubome/flexaudio#os-specific-permission-requirements).

On macOS, system/process loopback (Core Audio process taps) requires macOS 14.4
or later.

## MSRV

Rust **1.85**.

## License

[MIT](LICENSE) © 2026 tsubome / Aratech. Third-party notices:
[`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md).
