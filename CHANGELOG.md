# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

> **0.x stability note:** while in the `0.x` series the public API is not yet
> stable. Per SemVer, a **minor** bump (`0.2 → 0.3`) may include breaking
> changes; **patch** bumps (`0.2.0 → 0.2.1`) remain backward-compatible. Pin to
> `0.3` to receive only compatible updates.

## [Unreleased]

## [0.3.1] - 2026-10-08

### Fixed

- Python wheel license files now install into `.dist-info/licenses/` through
  PEP 639 instead of the top of `site-packages`.
- Documentation now reflects that npm packages are published instead of pending.

### Packaging

- npm publishing moved to Trusted Publishing via GitHub OIDC, removing the
  need for a long-lived npm token.

## [0.3.0] - 2026-10-07

### Added

- **Process enumeration: `flexaudio::processes() -> Result<Vec<ProcessInfo>>`.**
  Lists the processes that have an audio output session/stream and can be
  passed to per-process capture as `target_pid`. The calling process is
  excluded, entries are deduplicated by PID, and the list is sorted with
  actively-playing processes first. Idle/stopped processes stay on the list;
  whether something is playing now is `is_output_active`. `ProcessInfo`
  carries `pid`, `name` (always non-empty), and, when the OS exposes them,
  `executable`, `bundle_id` (macOS), and `is_output_active`.
  - **Linux:** PipeWire clients that own a `Stream/Output/Audio` node; the PID
    comes from `application.process.id` for pulse-proxied streams and
    `pipewire.sec.pid` for native clients (the same resolution the capture
    backend uses); activity = node state `Running`. `executable` is
    the basename of `/proc/<pid>/exe`, falling back to `/proc/<pid>/comm`.
  - **Windows:** audio sessions on every active render endpoint
    (`IAudioSessionManager2`); activity = session state `Active`. Listing and
    capturing both require Windows build 20348 or later (Windows 11 /
    Windows Server 2022).
  - **macOS 14.4+:** process objects Core Audio knows about (including
    input-only processes), with bundle ID and
    `kAudioProcessPropertyIsRunningOutput`; older macOS returns
    `Error::UnsupportedOsVersion`.
  - The call is read-only, triggers no permission prompt, and always returns
    within 3 seconds. `Ok(empty)` means per-process capture is available but
    no such process exists right now (not "nothing is playing"). `Err` means
    per-process capture is unavailable here, permission was denied, the OS
    did not answer in time, or a previous enumeration is still in progress.
  - Exposed in every binding: N-API `processes(): Promise<JsProcessInfo[]>`
    (libuv thread pool; does not block the JS event loop), C
    `flexaudio_processes` / `flexaudio_processes_free` (`FlexProcessInfo`,
    `FlexOutputActivity`), Python `flexaudio.processes()` (`ProcessInfo`, still
    synchronous), and `flexaudio-cli --list-processes`.
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
- **`StreamConfig::exclude_pids` / N-API `excludePids`.** System-loopback
  capture can exclude a set of pids in addition to `exclude_self`. Electron
  hosts render audio from a helper process, so excluding the addon's own pid
  was not enough on Linux and macOS. Linux excludes every listed pid. macOS:
  every pid is added to the tap's exclude list,
  resolved to its Core Audio process object once at capture start — a helper
  that has not yet rendered audio has no object and is not excluded, so open
  the capture while the app is already playing or reopen it when a helper
  appears. Windows: one process tree — `exclude_self` wins, otherwise the first
  pid; any other listed pid is rejected with `Error::InvalidArg`.
- **Microphone + system mix:** `SourceKind::Mix` combines both inputs, with
  independent device selection and pre-mix gains plus a global post-mix gain.
- **PID exclusion across bindings:** C adds `flexaudio_open_with_exclude_pids`
  and `flexaudio_switch_source_with_exclude_pids` without changing `FlexConfig`;
  Python adds keyword-only `exclude_pids` to `open()` and `switch_source()`;
  the CLI adds repeatable `--exclude-pid`. Exclusion applies to system capture
  and the system side of Mix, combined with self-exclusion.
- **Windows ARM64 npm binaries:** prebuilt npm artifacts now cover five
  platforms (Linux x64/arm64, macOS arm64, Windows x64/arm64). Python wheels
  remain available on four platforms (no Windows ARM64 wheel).

### Changed

- The N-API TypeScript declarations now type the callbacks:
  `openStream(options, onChunk: (chunk: JsAudioChunk) => void, onEvent?)` and
  `watchDevices(onEvent: (event: JsDeviceEvent) => void)` instead of the
  undefined `ChunkTsfn` / `EventTsfn` / `DeviceTsfn` names.
- **N-API `processes()` is async:** it returns `Promise<JsProcessInfo[]>`.
  Rejection uses the same error type and message as the former thrown error.
- **N-API `FlexStream.stop()` is async:** it returns `Promise<void>`. The
  promise resolves only after every `onChunk` queued before stop — including
  the last PCM and the `frames:0` terminator — has been delivered to JS. Do
  not block synchronously inside `onChunk`, or terminator delivery and
  `stop()` resolution can stall.
- **Exclusion is fail-closed.** Windows rejects any listed PID different from the
  single excluded process tree root with `Error::InvalidArg`. On macOS, every PID
  must fit 1..=2147483647 (`i32::MAX`); otherwise capture fails with
  `Error::InvalidArg`. macOS also fails capture start when a requested PID
  lookup fails, unless the process is confirmed gone. N-API rejects non-integer, zero, negative, and out-of-range
  `processId` / `excludePids` values instead of coercing them. On Linux,
  pulse-proxied streams are matched by `application.process.id` and remain out
  of exclusion-mode captures until that PID is known.
- **macOS device + exclusion:** capture honors a requested system output device
  together with PID exclusion, including the system side of Mix. Linux and
  Windows retain their existing behavior: the requested system device is not
  used while exclusion is active.
- **Pure-Rust VAD inference:** replace ONNX Runtime with tract-onnx 0.23.7 and
  an embedded Silero 16 kHz model; sinc-resample 8 kHz input while retaining
  input-based event timing. VAD and bindings that include it require Rust 1.91.
- **English codebase:** translate code comments, documentation, and user-facing
  text into English; `README.ja.md` is the Japanese translation of the canonical
  English README.

### Fixed

- **Linux: fan-in capture no longer latches a half-linked node.** `try_link`
  now commits a target only once the capture stream's own input ports have all
  arrived, the target has every output port its node info declares (or, when
  the node has not declared a count, its currently visible ports are fully
  paired), and each channel the capture can take is paired; a `try_link` fired by the first
  input-port global used to link FL alone and never revisit the node, so stereo
  sources came through at half level with one channel missing.
- **Linux: libpulse clients now resolve to their own pid.** Stream nodes are
  bound and `application.process.id` is read from their info props (the
  registry `global` event omits it). Pulse-proxied streams (`client.api =
  pipewire-pulse`) are matched only by that property and are not captured in
  exclude mode until it is known. Native PipeWire clients still fall back to
  `pipewire.sec.pid`. Previously, PulseAudio clients such as Electron/Chromium
  and Zoom shared pipewire-pulse's pid in `processes()` and could not be
  excluded individually.
- **Windows process-loopback format conversion:** enable WASAPI automatic PCM
  conversion to the fixed 48 kHz stereo capture format instead of relying on
  an unavailable process-loopback mix format.
- **Capture lifecycle:** mark the first resumed chunk on both taps with
  `DISCONTINUITY` under the delivery lock. Keep cpal's Windows WASAPI enumerator
  on a process-lifetime thread so later microphone calls survive the first
  caller's exit.
- **macOS device errors:** propagate output-device enumeration and UID lookup
  failures through the shared OSStatus mapping instead of masking them.

### Tests

- The real-PipeWire smoke test checks each channel using distinct left and
  right tones.

### Packaging

- **Shared release version gate:** all three release workflows check the tag
  or explicit dispatch version against Rust, Python, and npm manifests before
  uploads or publication. Manual dry runs default to enabled.
- **npm publication hardening:** skip only an exact platform package version
  already present in the registry; other failures stop publication. Include
  `LICENSE` and `THIRD_PARTY_NOTICES.md` in every platform package. The first
  npm publication remains pending and requires an interactive human with 2FA.
- **Windows binary checks:** use a Node.js PE parser to inspect normal and
  delayed imports, enforce the documented DLL allowlist, and validate names
  containing `+`. Windows npm builds use a static CRT.
- **CI coverage:** pin required checks to Rust 1.98.1, guard the pin, preview
  upstream stable, lint native Windows/macOS crates, and check all 13 members
  at their declared MSRV (nine at Rust 1.85, four at Rust 1.91).

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
- **N-API `processes()`:** `await processes()` (it is no longer synchronous).
- **N-API `FlexStream.stop()`:** `await stream.stop()` so the last PCM and the
  `frames:0` terminator have been delivered before you tear down.

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
- **`flexaudio-napi`:** Node.js N-API addon (distributed through npm; first publication pending) for in-process
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

## History before 0.3.0 (translated from Japanese commit messages)

### 2026-09-23 - Capture correctness and Windows dependency checks

#### Fixed

- Mark the first resumed chunk of each primary and secondary stream with
  DISCONTINUITY by checking resume generations under the delivery lock.
  The 300-round test checks both taps but did not reproduce the old
  race. (f6a30e7)
- Propagate macOS output-device enumeration and UID
  lookup failures instead of returning an empty list or masking errors
  as DeviceNotFound. Preserve the shared OSStatus error mapping.
  (d4eef38)
- Move the macOS device module's test-only Error import into
  its test module, fixing the Clippy gate without changing runtime
  behavior. (846d7d9)
- Parse DLL names containing '+' such as
  libc++.dll in the Windows PE checker. Add name-validation and CLI
  tests to N-API contract CI, keeping CLI execution unconditional on
  older Node.js versions. (480433e)
- Reject PE dependencies outside
  seven documented Windows DLLs and api-ms-win-* API sets, while
  retaining stronger forbidden-runtime rules. Add policy and
  synthetic-PE CLI tests to per-push CI. (55417c4)

### 2026-09-22 - Windows microphone lifetime

#### Fixed

- Initialize cpal's shared WASAPI enumerator on a process-lifetime
  keeper thread so later microphone calls survive the first caller's
  exit. Restore Windows microphone tests and add a two-thread regression
  test. (e36ca9f)

### 2026-09-21 - Rust toolchain and CI coverage

#### Fixed

- Update four VAD test chunking calls for the Rust 1.98 Clippy lint,
  preserving trailing-remainder behavior and Rust 1.91 compatibility.
  (b2375d3)

#### CI

- Pin required checks to Rust 1.98.1, add a toolchain-pin guard and a
  non-blocking upstream-stable preview, and lint Windows/macOS native
  crates. Expand MSRV coverage to nine Rust 1.85 crates and four Rust
  1.91 add-ons, including cross-checks for OS-only crates. (0e6cbea)

### 2026-09-20 - Windows npm artifact inspection

#### CI

- Replace dumpbin with a Node.js PE parser so missing runner tools no
  longer prevent Windows artifact uploads. Inspect normal and delayed
  imports, rejecting forbidden runtimes and unreadable inputs. (ab8df7f)

### 2026-09-18 - Pure-Rust VAD inference

#### Changed

- Replace ONNX Runtime with tract-onnx =0.23.7 and an embedded Silero 16
  kHz model; use sinc resampling for 8 kHz input while retaining
  input-based event timing. Raise the VAD Rust minimum to 1.91. Windows
  npm builds use a static CRT and check for runtime DLL dependencies.
  (804e641)

### 2026-07-05 - 0.2.0 publishing follow-up

#### CI

- Retry crates.io HTTP 429 publishing failures up to 10 attempts with
  660-second waits, failing immediately on other errors. Add
  RELEASING.md with the recorded 0.2.0 crates.io/PyPI publication
  status, npm blockers, and instructions for completing npm publication.
  (8985e37)
- Skip npm lifecycle scripts during real and dry-run
  publication and remove prepublishOnly, avoiding duplicate platform
  publication and GitHub release creation from napi prepublish.
  (924f009)
- Upgrade npm CLI before publishing to support staged
  publication and interactive approval of new packages in scopes
  requiring 2FA. (1840fbb)

[Unreleased]: https://github.com/Studio-Sadola/flexaudio/compare/v0.3.1...HEAD
[0.3.1]: https://github.com/Studio-Sadola/flexaudio/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/Studio-Sadola/flexaudio/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/Studio-Sadola/flexaudio/releases/tag/v0.2.0
