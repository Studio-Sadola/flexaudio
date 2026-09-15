# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

> **0.x stability note:** while in the `0.x` series the public API is not yet
> stable. Per SemVer, a **minor** bump (`0.2 → 0.3`) may include breaking
> changes; **patch** bumps (`0.2.0 → 0.2.1`) remain backward-compatible. Pin to
> `0.2` to receive only compatible updates.

## [Unreleased]

## [0.3.0] - not yet released

### Added
- **Process enumeration: `flexaudio::processes() -> Result<Vec<ProcessInfo>>`.**
  Lists the processes that currently have an audio output stream and can be
  passed to per-process capture as `target_pid`. The calling process is
  excluded, entries are deduplicated by PID, and the list is sorted with
  actively-playing processes first. `ProcessInfo` carries `pid`, `name`
  (always non-empty), and, when the OS exposes them, `executable`, `bundle_id`
  (macOS), and `is_output_active`.
  - **Linux:** PipeWire clients that own a `Stream/Output/Audio` node; the PID
    comes from the client's `pipewire.sec.pid` (the same resolution the
    capture backend uses); activity = node state `Running`.
  - **Windows:** audio sessions on every active render endpoint
    (`IAudioSessionManager2`); activity = session state `Active`. Capturing a
    listed PID needs process loopback (Windows 11 / build 20348+).
  - **macOS 14.4+:** Core Audio process objects with bundle ID and
    `kAudioProcessPropertyIsRunningOutput`; older macOS returns
    `Error::UnsupportedOsVersion`.
  - The call is read-only, triggers no permission prompt, and always returns
    within 3 seconds. `Ok(empty)` means "available, nothing playing";
    `Err` means per-process capture is unavailable in this environment.
  - Exposed in every binding: N-API `processes(): JsProcessInfo[]`, C
    `flexaudio_processes` / `flexaudio_processes_free` (`FlexProcessInfo`,
    `FlexOutputActivity`), Python `flexaudio.processes()` (`ProcessInfo`), and
    `flexaudio-cli --list-processes`.
- **Secondary output tap:** `StreamConfig::secondary_output` renders the same
  capture in a second format (for example 48 kHz stereo for saving plus 16 kHz
  mono for recognition), pulled with `Stream::poll_secondary` as
  `SecondaryChunk`. The N-API binding can deliver it as signed 16-bit
  (`secondaryOutput.encoding: 's16'`, an `Int16Array`); quantization goes
  through the shared NaN/Inf-safe `flexaudio_core::quantize_i16`.
- **Recording clock:** `pts_ns` is a zero-based recording clock (0 at the first
  delivered chunk) that stays continuous across pause/resume and source
  switches. Primary and secondary chunks share the clock, so pair them by
  `pts_ns`, never by `seq` (each tap has its own counter).
- **Integrated VAD control (N-API):** `vadTap: 'primary' | 'secondary'`,
  `FlexStream.flushVad()` to close the open utterance (run automatically by
  `stop()`), and a 30 s `maxSpeechMs` default for the integrated VAD when the
  option is left unset (the standalone `Vad` keeps silero's unbounded default).
- Denoise now runs once on the shared 48 kHz normalized signal, so both the
  primary and the secondary tap receive denoised audio.

### Changed
- The N-API TypeScript declarations now type the callbacks:
  `openStream(options, onChunk: (chunk: JsAudioChunk) => void, onEvent?)` and
  `watchDevices(onEvent: (event: JsDeviceEvent) => void)` instead of the
  undefined `ChunkTsfn` / `EventTsfn` / `DeviceTsfn` names.

### Migration from 0.2
- **Rust `StreamConfig` literals:** the struct gained `secondary_output`. A
  literal that lists every field without `..Default::default()` no longer
  compiles; add `secondary_output: None` or end the literal with
  `..Default::default()`.
- **N-API chunk delivery:** `onChunk` receives **one** argument. With
  `secondaryOutput` set, the paired secondary chunk is `chunk.secondary` (it is
  `undefined` on rounds where it has not arrived yet) — it is **not** a second
  callback argument. Code written as `(primary, secondary) => …` always sees
  `secondary === undefined`. VAD events ride on the chunk of the tap chosen by
  `vadTap`: `chunk.vadEvents` for `'primary'`, `chunk.secondary?.vadEvents` for
  `'secondary'`.
- **Timestamps:** treat `pts_ns` as time since the recording started, not as a
  host monotonic timestamp.
- **Integrated VAD:** if you relied on unbounded utterances, pass
  `vad.maxSpeechMs: 0` explicitly.
- **Process pickers:** list candidates with `processes()` instead of deriving
  them from `devices()` (which lists endpoints, never processes).

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

[Unreleased]: https://github.com/Studio-Sadola/flexaudio/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/Studio-Sadola/flexaudio/releases/tag/v0.2.0
