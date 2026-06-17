# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

> **0.x stability note:** while in the `0.x` series the public API is not yet
> stable. Per SemVer, a **minor** bump (`0.2 → 0.3`) may include breaking
> changes; **patch** bumps (`0.2.0 → 0.2.1`) remain backward-compatible. Pin to
> `0.2` to receive only compatible updates.

## [Unreleased]

## [0.2.0] - 2026-06-17

The first Rust workspace release — a ground-up Rust rewrite of the earlier prototype.

### Added
- **Complete capture matrix ("9 cells"):** microphone, system-output loopback,
  and per-process capture across Linux, Windows, and macOS.
  - **Linux:** PipeWire backend for system and per-process capture
    (`flexaudio-os-linux`); cpal/ALSA microphone.
  - **Windows:** WASAPI loopback (system) and WASAPI process loopback
    (`flexaudio-os-windows`); cpal/WASAPI microphone.
  - **macOS:** Core Audio process taps for system and per-process capture
    (`flexaudio-os-macos`); cpal/CoreAudio microphone.
- **Unified facade `flexaudio`:** `open(StreamConfig)`, `devices()`, and
  `watch_devices()` pick the right backend by source + OS.
- **Pull-based streaming:** `Stream::poll_chunk` / `Stream::poll_event` deliver
  interleaved `f32` chunks (with `frames`, `peak`, `rms`, `seq`, `flags`) and
  stream events without callbacks.
- **Hot source switching:** `Stream::switch_source` swaps the input source while
  running; `seq` stays continuous and the first post-switch chunk is flagged as
  a discontinuity.
- **Device hotplug:** `watch_devices()` emits added / removed / default-changed
  events (PipeWire registry on Linux; no-op elsewhere for now).
- **Two-stage output formatting:** internal normal form resampled to a
  user-chosen `OutputFormat` (sample rate + channels) via `rubato`.
- **`flexaudio-vad`:** offline Silero VAD add-on with embedded ONNX model;
  streaming `SpeechStart`/`SpeechEnd` and batch `get_speech_timestamps`.
- **`flexaudio-cli`:** reference capture tool with WAV output and raw-PCM
  streaming to stdout (`--out -`) for real-time pipelines.
- **`flexaudio-napi`:** Node.js N-API addon (published to npm) for in-process
  use from TypeScript/Electron.

### Packaging
- Added `LICENSE` (MIT) at the workspace root and in each crate.
- Added `THIRD_PARTY_NOTICES.md` covering the bundled Silero VAD model,
  statically linked ONNX Runtime, dynamically linked PipeWire/libspa, and the
  permissive Rust dependency set.
- Filled in crate metadata (`description`, `keywords`, `categories`, `readme`,
  `documentation`, `authors`) for crates.io publication.
- Declared per-crate MSRV: `1.85` for core/facade/OS/mic crates, `1.88` for
  `flexaudio-vad` and `flexaudio-napi`.

[Unreleased]: https://github.com/tsubome/flexaudio/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/tsubome/flexaudio/releases/tag/v0.2.0
