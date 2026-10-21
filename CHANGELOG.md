# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

> **0.x stability note:** while in the `0.x` series the public API is not yet
> stable. Per SemVer, a **minor** bump (`0.2 → 0.3`) may include breaking
> changes; **patch** bumps (`0.2.0 → 0.2.1`) remain backward-compatible. Pin to
> `0.9` to receive only compatible updates.

## [Unreleased]

### Added

- Rust `validate_open` exposes the same device-independent argument validation
  and strict preflight used by open, allowing model loading before backend access.
- The non-default `test-support` feature is internal and test-only, with no
  stability guarantee.
- C v5 open/switch envelopes with presence-aware output and mix-lane gains:
  NULL pointers select unity (1.0), while explicit zero mutes. They reuse v4
  controls, strict defaults and settings reports. Output gain remains fixed at
  open on source switches; v1-v4 keep interpreting configuration zero as unity.
  Both v5 entry points reject nonzero legacy `FlexConfig` gain fields with
  `FLEX_INVALID_ARG` before native access and direct callers to `FlexGainsV5`.
  Gain validation uses the Rust core path: output gain is validated on open,
  Mix gains only for Mix sources; inapplicable gains are reported as ignored.
- Rust constructors `AudioChunk::new(data, frames)`,
  `DeviceInfo::new(id, name, source_kind)` and `StreamConfig::new(kind)`.
  Public fields remain writable, and `StreamConfig::default()` remains available.

### Changed

- Capture denoise now accepts any supported output rate in every binding,
  including Node, Python and C v1/v2. All entry points use the shared canonical
  48 kHz processing path before output conversion; settings reports mark denoise
  as applied without a rate adjustment. No migration is needed, except that code
  relying on the former non-48 kHz rejection no longer receives an error.
- Python `Stream.switch_source()` now retains the current denoise setting when
  `denoise` is omitted or `None`; explicit `True`/`False` enables/disables it.
  Migration: pass `denoise=False` explicitly to keep the old behavior.
- Capture opens strict by default in Rust, Node, Python, C v1-v4 and the CLI.
  Ignored settings are rejected only when their decoded values differ from defaults;
  C zero-initialized microphone configurations still use defaults and remain valid.
  Migration: opt out with Rust `OpenOptions::new().strict(false)`, Node
  `strict: false`, Python `strict=False`, C v4 `strict = 2`, or CLI `--no-strict`.
  C v1-v3 users must migrate to v4 to opt out. C v4 `strict = 0` selects strict
  on open and retains the current value on switch. All bindings retain strictness
  on switches when omitted. CLI `--strict` remains accepted; combining it with
  `--no-strict` is a usage error.
- Rust `AudioChunk`, `DeviceInfo` and `StreamConfig` are now `#[non_exhaustive]`
  so future fields can be added without requiring downstream literal updates.
  Migration: replace struct literals with the corresponding constructor, then
  assign any customized public fields. For example, use
  `let mut config = StreamConfig::new(SourceKind::Mic); config.gain = 0.5;`.
  Destructure with `..` to allow additional fields.

## [0.8.1] - 2026-10-20

### Fixed

- Documentation: refresh installation pins and release-neutral introductions;
  correct capture, exclusion, arrival-label, VAD, error-code, and denoise examples
  against the current implementation.
- Documentation: cover Rust settings and metadata APIs, use release-tagged links
  on registry pages, and describe CLI diagnostics through public capture flags.

## [0.8.0] - 2026-10-20

### Added

- Effective settings reports and opt-in strict validation in every binding.
  Only decoded values differing from documented defaults count as requests;
  explicit defaults never cause strict rejection, including on source switches.
  Reports distinguish applied, not requested, ignored and adjusted settings.
  Ignored reasons are `wrongSourceKind`, `unsupportedPlatform`,
  `exclusionDeviceUnscoped`, `processLoopbackPath`, and `fixedAtOpen`.
  Strict rejection includes the complete candidate report before device access,
  permission requests, model loading, or stopping the current source.
- Rust `resolve_settings`, `SettingsRequest`, and `SettingsAttachments` for
  device-independent settings resolution shared by all front ends.
- Helper lifecycle events with stop reasons. Rust uses its existing event channel;
  Node uses `onLifecycleEvent`, Python `poll_lifecycle_event`, C
  `flexaudio_poll_lifecycle_event_v4`, and CLI `--lifecycle-events` uses stderr JSON.
  Binding lifecycle delivery is separate from ordinary events. Every emitted
  Started has exactly one Stopped, including failure and cancellation paths.
  macOS Started means a diagnostic attempt; Windows Started means successful
  keepalive render start. Stopped ends ownership after teardown attempts, without
  promising native cleanup succeeded.
- `arrival_ns` / Node `arrivalNs` aliases for primary and secondary chunk arrival
  labels, including C v4 accessors; values are identical to the existing PTS labels.
- Namespaced device listing: Rust/Python `devices_with_namespaces`, Node
  `devicesWithNamespaces`, C `flexaudio_devices_with_namespaces_v4`, and CLI
  `--list-devices-json`. Selector IDs remain unchanged.
- Configurable ring capacity in Node, Python, C v4, and CLI, joining Rust;
  the default remains 50 chunks and capacity stays fixed at open.
- CLI `--strict`, `--settings-json`, `--lifecycle-events`, `--list-devices-json`,
  `--ring-capacity-chunks`, and `--denoise`. Settings JSON includes successful
  open/switch snapshots or the candidate report on strict rejection.

### Deprecated

- Chunk `pts_ns` / `ptsNs` naming is documentation-only deprecated in favor of
  `arrival_ns` / `arrivalNs`. Existing names keep working throughout 0.9 under
  the policy in `RELEASING.md`; frozen C layout fields remain permanently.
  Use frame indices for audio continuity rather than arrival labels.

## [0.7.0] - 2026-10-19

### Added

- Capability snapshots in Rust (`capabilities()`), Node (`await capabilities()`),
  Python (`capabilities()`), C (`flexaudio_capabilities_v1` and accessors), and
  CLI (`--capabilities`, JSON). They report source backends and availability,
  OS version when known, PID/exclusion semantics and limits, device selection,
  watcher support, and optional rendering side effects. They open no capture or
  render streams, play no audio, and request no recording permission. Linux uses
  a bounded PipeWire socket/connection check: a missing socket is `unavailable`;
  failed socket inspection, connection or server sync is `unknown` with a typed
  error. Support is not a guarantee of permission or a usable device.
- Rust `OpenOptions`, `open_with_options`, and `Stream::switch_source_with_options`,
  with a Windows classic loopback keepalive option, enabled by default. Node
  exposes `windowsClassicLoopbackKeepalive`, Python
  `windows_classic_loopback_keepalive`, C the v3 envelope's
  `windows_classic_loopback_keepalive` (0 default/retain, 1 enabled, 2 disabled),
  and CLI `--no-classic-loopback-keepalive`. The option applies to system capture
  and the system lane of Mix without exclusions; other platforms and
  process/exclusion capture ignore it. Disabling it means idle classic loopback
  delivers no packets: the 2 s stall watchdog reports `StreamStalled` and reopens
  repeatedly until audio plays. Source switches retain the current option unless
  explicitly replaced; failed switches retain the previous value.
- Python and C secondary output with `f32` or `s16` encoding, independent polling,
  sequence/frame counters and drop accounting, and attached VAD events on the
  secondary tap. Secondary VAD requires secondary output; VAD still processes
  shared canonical capture. Output format and encoding are fixed at open and
  retained on source switches; meters describe floats before integer quantization.
- C v3 open/switch envelopes (`FlexStreamConfigV3`, `FlexSwitchConfigV3`,
  `flexaudio_open_v3`, `flexaudio_switch_source_v3`) support exclusion PIDs
  together with attached VAD, secondary output and keepalive control. Switching
  retains the VAD attachment and output settings. Existing v1/v2 layouts,
  signatures and symbols are unchanged.

### Changed

- Python and C stream denoise now always runs in the capture worker on canonical
  48 kHz stereo before downmix, output conversion and output gain/clipping, feeding both
  taps. Streams with attached VAD already used this path in 0.6.2; the following
  differences concern denoise without attached VAD:
  - Mono output and non-unity gain can produce different samples and meters.
    Clipping flags now describe gain applied after denoise, rather than clipping
    before denoise. Stereo at unity gain can also differ at lifecycle boundaries
    or after dropped output; there is no blanket sample-identity guarantee.
  - Stop flushes the 480-frame denoise tail through the facade and zero-pads the
    final output to a full 20 ms chunk, with `PADDED` when padding is added,
    instead of returning a separate unpadded 10 ms tail. Padding occurs after
    denoise, so partial final input can also produce different samples. For
    example, 9600 input frames at 48 kHz without loss now yield 11040 output
    frames/channel after stop, rather than 10080. Even a started stream with no
    input can yield a padded silent chunk on stop. Drain after stopping to retain
    the final output. Tail chunks now use the facade's normal sequence/frame
    indices, arrival-time PTS, flags and bounded queue/drop accounting rather
    than binding-synthesized tail metadata.
  - Denoise continues through paused capture and retains its history on resume,
    instead of resetting on the first resumed `DISCONTINUITY` chunk. It also runs
    before output queue drops, so dropped output no longer resets or skips
    denoise history. A `DISCONTINUITY` flag alone no longer resets it; capture-worker
    generation/loss rebuilds own resets. Starts, source switches and reopens create
    fresh denoisers; established
    generations discard their buffered remainder on replacement. Python no
    longer flushes a separate local denoise tail before attempting a source switch.
  - Processing no longer waits for PCM polling. The fixed algorithmic delay
    remains 480 frames/channel (10 ms at 48 kHz); output chunks remain 20 ms.
    Python and legacy C opens still require 48 kHz primary output for denoise;
    C v3 can convert the denoised signal to other output rates.

## [0.6.2] - 2026-10-18

### Fixed

- Linux: device watching no longer reports PipeWire microphone nodes
  (`Audio/Source`) or default-microphone changes. Their IDs are PipeWire node
  names, which `devices()` never returns and the microphone path (CPAL/ALSA
  device names) cannot open. The watcher now reports system output additions,
  removals and default changes only, using the same IDs as `devices()`.
- `config()` after a successful `switch_source` now returns the Mix device IDs
  and gains that the new source actually uses, instead of the values from
  before the switch.
- Linux: process capture with target PID 0 now fails with `InvalidArg`, as on
  Windows and macOS, instead of starting a capture that can never match.

### Documentation

- Windows: a runtime failure of the silent keepalive render stream terminates
  capture with `Event::TerminalError`; the README previously described
  stall/reopen recovery.
- `ProcessMode` and the CLI `--mode` help now describe the per-platform scope:
  a process tree on Windows, the exact PID on Linux and macOS.
- `pts_ns` is documented as an arrival-time label assigned when samples are
  ingested, with the same origin for microphone and loopback sources;
  `frame_index` remains the authoritative timeline.
- Node: `switchSource` keeps the existing denoise and VAD settings; `denoise`
  and `vad` passed to it are ignored.
- README: added a binding feature matrix (Rust, Node, Python, C, CLI).

## [0.6.1] - 2026-10-18

### Fixed

- Linux: a `system` capture with `excludeSelf` no longer fails with
  "PipeWire rejected capture setup" when another application's playback
  stream appears or disappears while the capture starts or runs. Two races
  were treated as fatal: destroying a proxy the server had already removed
  (`unknown resource N op:7`) and linking to an output port that vanished
  before the link was created (`unknown output port N`). Proxy release is now
  deferred to a server sync, and these errors are recoverable only when they
  are attributed to this capture's own released proxy or to a link target
  verified as gone. Permission errors, surviving targets and unrelated errors
  still stop the capture. Startup succeeds when no playback stream remains.
- Linux: errors that occur after a capture has started no longer use the
  "capture setup" wording, and the native PipeWire error tuple is kept in
  `diagnosticDetails`.

### Known issues

- Linux: with PipeWire's ALSA plugin (observed on PipeWire 1.0.5), each
  `snd_pcm_open`/`snd_pcm_close` cycle for a PipeWire-backed PCM retains about
  4 KB inside the plugin. Opening a microphone through cpal performs several
  such opens, so applications that reopen microphone streams very frequently
  see slow memory growth that is not caused by flexaudio.
  Reuse an open microphone stream instead of reopening it in a loop, especially
  in long-running processes. For system-only capture, prefer the native PipeWire
  system source, which does not use the CPAL/ALSA microphone path. Mix still
  opens a microphone lane and therefore does not avoid this plugin issue.

## [0.6.0] - 2026-10-15

### Breaking migration

The 0.5.x series is deprecated. One generic `Vad` now provides the previous
streaming segmenter, embedded Silero v6 inference, per-frame probabilities,
provisional hints and capture attachment across Rust, Node.js, Python and C.
The earlier speech-start/speech-end implementation is removed without aliases.
Golden segment endpoints remain unchanged for the existing 16 kHz mono path.

| Binding | Previous public API | 0.6.0 API or migration |
|---|---|---|
| Rust | `WhisperVad` | `Vad` |
| Rust | `WhisperVadParams` | `VadOptions` |
| Rust | `WhisperVadOptions` | `VadRuntimeOptions` |
| Rust | `WhisperVadPostProcessor`, `whisper_speech_segments` | `VadPostProcessor`, `speech_segments` |
| Rust | `WhisperSpeechSegment` | `SpeechSegment` |
| Rust | `WhisperVadEvent`, `WhisperVadEventKind`, `AttachedWhisperVadEvent` | `VadEvent`, `VadEventKind`, `AttachedVadEvent` |
| Rust | `WhisperVadTap`, `WhisperVadTapError`, `WhisperVadTapFailure` | `VadTap`, `VadTapError`, `VadTapFailure` |
| Rust | `WhisperVadError`, `WhisperVadFailure`, `WhisperVadParameter`, `WhisperVadParameterReason`, `WhisperVadOperation` | `VadError`, `VadFailure`, `VadParameter`, `VadParameterReason`, `VadOperation` |
| Rust | `WhisperCode`, `Error::WhisperVad`, `Error::whisper_code()` | `VadErrorCode`, `Error::Vad`, `Error::vad_code()` |
| Rust | Earlier `Vad`, `VadConfig`, `Segment`, `get_speech_timestamps`, `VadCaptureEngine`, `VadCaptureOutcome`, `process_capture_chunk`, `finish_capture_gap` | Removed; use the unified session, `VadOptions`, `SpeechSegment`, `speech_segments`, and `VadTap` |
| Rust | Earlier `Vad::{flush,config,converted_sample_position,take_pending_events,inject_failure,inject_reset_failure}`, `VadConfig::{aggressive,balanced,conservative,resolved_neg_threshold}`, `Segment::{start_ms,end_ms,len_samples}` | Removed with the earlier engine; use the surviving session lifecycle and explicit options |
| Rust | Earlier `VadError` | Backend/converter causes are `InferenceError`; session failures are `VadError` |
| Rust | `min_speech_duration_ms`, `min_silence_duration_ms`, `max_speech_duration_s` | `min_speech_ms`, `min_silence_ms`, `max_speech_ms` |
| Node.js | `WhisperVad`, `WhisperVadParams`, `WhisperVadEvent`, `AttachedWhisperVadEvent`, `WhisperVadCloseReason` | `Vad`, `VadOptions`, `VadEvent`, `AttachedVadEvent`, `VadCloseReason` |
| Node.js | `WhisperVadStreamOptions`, `OpenOptions.whisperVad` | `StreamVadOptions`, `OpenOptions.vad` |
| Node.js | `minSpeechDurationMs`, `minSilenceDurationMs`, `maxSpeechDurationS` | `minSpeechMs`, `minSilenceMs`, integer `maxSpeechMs` |
| Node.js | `flushWhisperVad()`, `whisperVadFailed`, `whisperCode`, `WhisperVadErrorCode` | `flushVad()`, `vadFailed`, `vadCode`, `VadErrorCode` |
| Node.js | `WhisperVadError` | `VadError` |
| Node.js | `JsAudioChunk.whisperVadEvents`, `JsSecondaryChunk.whisperVadEvents` | `vadEvents` with the unified event payloads |
| Node.js | Earlier `Vad`, `JsVadEvent`, `VadFlushException`, `vadThreshold`, `vadTap`, `negThreshold`, `sampleRate` VAD mode, earlier `flushVad()` reset semantics | Removed; use final millisecond `segment` events and explicit `finish()`/`reset()` |
| Node.js | `__openMockStream`, `__whisperVadTestFaults`, internal fault arguments | Removed from the published loader, declarations and native exports; device-free tests use a separate loader/build |
| Python | `WhisperVad`, `WhisperVadParams`, `WhisperVadPostProcessor`, `whisper_speech_segments` | `Vad`, `VadOptions`, `VadPostProcessor`, `speech_segments` |
| Python | `WhisperVadEvent`, `AttachedWhisperVadEvent`, `WhisperCode` and prefixed error types | Generic `VadEvent`, `AttachedVadEvent`, `VadErrorCode` and VAD error names |
| Python | `WhisperVadStreamOptions`, `WhisperSpeechSegment`, `WhisperCloseReason`, `WhisperCutReason`, `WhisperVadValidationError`, `WhisperVadRuntimeError` | `StreamVadOptions`, `SpeechSegment`, `VadCloseReason`, `VadCutReason`, `VadValidationError`, `VadRuntimeError` |
| Python | `AudioChunk.whisper_vad_events`, `AudioError.whisper_code`, `BackendErrorDict.whisper_code` | `vad_events`, `vad_code`, `vad_code` |
| Python | `whisper_vad=`, `flush_whisper_vad()`, `whisper_code`, duration fields | `vad=` on `open()`, `flush_vad()`, `vad_code`, `min_speech_ms`, `min_silence_ms`, `max_speech_ms`; the VAD attachment persists across source switches |
| Python | Earlier `Vad`/`VadSettings`, `_VadFlushErrorFields`, start/end sample events and legacy integrated controls | Removed; use the unified session and attached stream options |
| C | `FlexWhisperVad*`, `FlexWhisper*` payloads, `flexaudio_whisper_*` | Generic `FlexVad*` types and `flexaudio_vad_*` functions |
| C | `FLEX_WHISPER_*`, `flexaudio_error_whisper_code_v2` | `FLEX_VAD_*` in a distinct status range and `flexaudio_error_vad_code_v2` |
| C | `flexaudio_whisper_vad_default_params` | `flexaudio_vad_default_options` |
| C | `flexaudio_whisper_vad_{new,process,finish,reset,probabilities,free}` | `flexaudio_vad_{new,process,finish,reset,probabilities,free}` |
| C | `flexaudio_whisper_postprocessor_{new,process,finish,reset,free}` | `flexaudio_vad_postprocessor_{new,process,finish,reset,free}` |
| C | `flexaudio_whisper_events_free`, `flexaudio_whisper_segments_free` | `flexaudio_vad_events_free`, `flexaudio_vad_segments_free` |
| C | `flexaudio_flush_whisper_vad`, `flexaudio_chunk_whisper_vad_events`, `flexaudio_error_whisper_code_v2` | `flexaudio_flush_vad`, `flexaudio_chunk_vad_events`, `flexaudio_error_vad_code_v2` |
| C | `FlexWhisperVad`, `FlexWhisperVadParams`, `FlexWhisperVadOptions`, `FlexWhisperVadPostProcessor`, `FlexWhisperVadStreamOptions`, `FlexWhisperSpeechSegment` | `FlexVad`, `FlexVadOptions`, `FlexVadRuntimeOptions`, `FlexVadPostProcessor`, `FlexStreamVadOptions`, `FlexSpeechSegment` |
| C | `FlexWhisper{SpeechStart,SpeechEnd,Cut,EpochEnd,EpochStart}`, `FlexWhisperVad{Payload,Event}`, `FlexAttachedWhisperVad{Payload,Event}` | `FlexVad{SpeechStart,SpeechEnd,Cut,EpochEnd,EpochStart}`, `FlexVad{Payload,Event}`, `FlexAttachedVad{Payload,Event}` |
| C | `FlexStreamConfigV2.whisper_vad`, `FlexChunkV2.whisper_vad_events{,_len}` | `vad`, `vad_events{,_len}` |
| C | Earlier `flexaudio_vad_flush`, `FlexConfig.has_vad`, `FlexConfig.vad`, `FlexChunk.vad_events{,_len}`, `FLEX_WHISPER_CONFLICTING_VAD` | Removed; use attached v2 configuration and surviving finish/reset lifecycle |
| C | Earlier `FlexVadConfig`, `FlexVadEvent` sample events and integrated configuration | Removed; use unified millisecond options/events and v2 attachment configuration |
| All | `PermissionGranted` / `permissionGranted` | Removed; C event number 9 is reserved and never reused |

In 0.5.x, fractional `maxSpeechDurationS` values were truncated to whole seconds.
To preserve identical segmentation, migrate with
`maxSpeechMs = trunc(seconds) * 1000` (for example, 30.9 seconds becomes 30000 ms).

### Defaults and behavior

- Attached and standalone VAD share defaults: threshold 0.5, minimum speech
  250 ms, minimum silence 100 ms, padding 30 ms, unbounded maximum, provisional
  hints disabled. The earlier Node.js integrated 30000 ms maximum is removed.
- C supplied `FlexVadOptions` values preserve literal zeros. The earlier
  `FlexVadConfig` zero-sentinel defaults are removed; use NULL parameters or
  `flexaudio_vad_default_options()` for the shared defaults.
- Maximum speech duration is integer milliseconds; omitted, zero, or values above
  100,000,000 ms use the legacy `INT_MAX/2`-sample sentinel (1,073,741,823 samples),
  which is effectively unbounded. Other nonzero values use the exact 16 kHz split
  budget `16 * maxSpeechMs - 512 - 32 * speechPadMs`; negative budgets return
  `InvalidParameter`, while zero budgets are valid (1000 ms with 484 ms padding,
  for example). Subsecond values are never truncated to whole seconds.
  Later merging can reunite splits, so this is not a final-duration/latency bound.
- Standalone VAD accepts optional input rate/channels (16000/1 by default),
  converts internally, and reports milliseconds on the input timeline with
  converter group delay compensated. Finite resampler overshoot is clipped only
  inside the standalone converter; raw normalized PCM validation stays strict.
  Rust uses `process_pcm` for other formats.
- Capture retains the 20 ms chunk requirement because its 960-frame canonical
  blocks and converter phase alignment depend on it. Epochs, source-clock
  anchors, provisional hints, EOF padding and failure isolation remain available.
- macOS silence probing stays enabled by default. `silence_probe` / `silenceProbe`
  lets applications disable its diagnostic tone without changing microphone consent.
  C `FlexConfig.silence_probe` uses 0/default or 1/enabled and 2/disabled; other
  values are invalid. The CLI exposes `--no-silence-probe`.
- `PermissionPending` remains available to custom backends; built-in backends
  do not emit it.

### Fixed and packaging

- VAD C status values no longer collide with core error codes; uniqueness is checked.
- `flexaudio_free` always releases the stream and drops unread outputs. Stop and
  drain first when final PCM or VAD events are needed.
- Adapter contract fixtures inherit toolchain locations from the environment.
- Public documentation, notices and fixture names use product-neutral language;
  upstream attribution and all general-purpose capture/diagnostic fixes remain.
- All distributed package versions are 0.6.0.

[0.6.0]: https://github.com/Studio-Sadola/flexaudio/compare/v0.5.4...v0.6.0

## [0.5.4] - 2026-10-13

### Fixed

- Discovery keeps devices with unreadable mandatory formats as unavailable (0/0),
  and keeps unreadable optional process metadata unknown instead of rejecting neighbors.
  Collection/count failures still fail the whole call. Entries without a readable
  selectable identity may be omitted; macOS also omits unreadable output direction.
  Windows/macOS keep unknown device names with selectable native-ID fallbacks.
  Optional default queries leave no default instead of failing discovery. Windows/macOS
  output and microphone opens re-query format; explicit Linux output IDs use a fresh
  inventory/format gate. Linux default opens leave `target.object` unset and let PipeWire
  resolve the default sink and negotiate format without an inventory pre-check;
  exclusion fan-in also uses negotiation. Linux PipeWire output discovery and watcher
  notifications retain independent 48000 Hz / 2-channel compatibility defaults.
- Linux process node-bind failures reject the collection with the first cause unless
  a registry removal confirms the node vanished. Windows device and process discovery omit
  invalidated/vanished endpoints but retain access-denied and other typed query errors.
  macOS explicit-output scans skip vanished or unrelated unreadable devices; the typed
  direction cause is returned for the requested device. If that direction query fails
  and neither the device's name nor UID is readable, it cannot be matched and open
  returns `DeviceNotFound`.
- Primary/secondary PTS steps use rational arithmetic while retaining per-callback
  arrival re-anchoring. At 48 kHz primary, frame_index and capture_sample identify
  the same sample across replacement, recovery, loss rebuild and startup release.
- Windows waits distinguish timeout from native failure; COM initialization accepts
  only successful initialization or the documented already-initialized cases.
- Linux PipeWire capture bind failures, malformed/missing buffers, rejected renegotiation,
  callback panics and watcher daemon errors become readiness, loss or terminal
  diagnostics. Routing changes mark the next chunk discontinuous; watcher stop
  retains join failures and terminal notifications.
- Later Mix terminal causes and concurrent VAD flush/reset failures remain ordered
  related causes. VAD rejects format changes while input is pending and retains
  completed speech events when reset fails. Standalone flush errors retain pending
  events in Node.js/Python and populated C output; attached Python source switches
  use the shared gap lifecycle before replacing addons.
- Shared typed addon projection retains VAD/FLAC kind and message in bindings and
  shutdown reports. Whisper model, inference, reset and converter errors preserve
  their causes. Worker creation preserves I/O kind and OS code; Linux query errors
  preserve opt-in daemon details, and inconclusive macOS self-probes retain their
  native cause in an existing recoverable-error event alongside the silence advisory.
- FFI boolean guards set typed last errors on panic. Failed Node.js stop-error
  creation still settles the Promise; the loader retains all attempted failures.
  Secondary event carriers retain sequence, frame index and dropped counters.
  Python invalid PID errors include the value's representation.
- Clock translation documents saturating arithmetic; CLI errors name the failed
  operation and check started WAV finalization, retaining runtime and finalization errors.
  Failure before the first chunk never creates or truncates the WAV destination.
- Microphone discovery validates native rates/channels through the capture configuration
  helper. Data callback panics close PCM and publish a typed terminal failure through
  the lock-free callback mailbox.
- C error storage retains the real typed causes for FLAC creation, processed polling
  and VAD construction; message-only legacy errors no longer invent InvalidArg causes.
  C v1 FLAC messages include the addon cause; C v2 retains its structured fields.
  Python exceptions from `EncodeError` use the shared addon projection and expose kind/message.

### Added

- Public `flexaudio_core::inventory` helpers share entry omission, optional metadata,
  collection error propagation and checked device-format policy across OS adapters.
- Rust/Python `DeviceInfo.format_available`, Node.js `JsDeviceInfo.formatAvailable`,
  and C `flexaudio_device_format_available_v2` expose unreadable mandatory formats.
  False uses sample rate 0 and channels 0; checked open retains typed cause/remedy
  (Linux default output relies on PipeWire negotiation without a pre-check).
  ProcessInfo retains the existing `pid N` fallback and absent optional fields.
  C v1 device structure layout is unchanged. Rust DeviceInfo struct literals must
  supply the new field.
- Clarify device IDs can change between discovery calls: recovered names replace native
  fallbacks; name-based IDs cannot open while names are unreadable, while native IDs
  remain openable in both states. The public ID scheme is unchanged.
- Document arrival PTS limits, canonical capture origins, stall/recovery ordering,
  increasing attached WhisperVad epochs and synchronous source-switch boundaries.
- `DeviceEvent::TerminalError { error }`: terminal watcher failure, exposed as
  `terminalError` in Node.js/Python and C v2 device event kind 6 with its error accessor.
  C v1 watcher polling returns the negative typed error code and sets last_error.
- `Error::Diagnostic { source, detail }` and binding diagnostic-detail accessors keep
  native details opt-in and excluded from safe display.
- `Error::Addon { kind, message }` and `AddonErrorKind` distinguish `VadModelLoad`,
  `VadInference`, `VadInvalidConfig`, `VadInvalidFormat`, `VadResample`, `VadReset`,
  `FlacIo`, `FlacUnsupported` and `FlacEncoder`; bindings expose kind/message fields
  and C v2 accessors. Existing `ErrorKind` variants are reused; FLAC I/O/encoder
  failures now receive their correct root classifications.
- `VadError::Multiple`, `WhisperVadError::Cause` and `WhisperVadTapError::Cause`
  retain related typed addon failures; `Vad::take_pending_events()` drains completed
  speech events after a failed reset.

[0.5.4]: https://github.com/Studio-Sadola/flexaudio/compare/v0.5.3...v0.5.4

## [0.5.3] - 2026-10-12

### Fixed

- Microphone startup suppresses non-finite or out-of-range leading frames at each
  capture generation, including the mic lane of Mix. After a drop, 10 ms of valid
  native frames confirms startup and is delivered in full. Valid zeros pass
  unchanged, capture timestamps and frame indices preserve suppressed time, and
  the first delivered chunk carries `DISCONTINUITY`. Persistent invalid startup
  fails capture with safe driver/settings/reopen guidance after two seconds.
  Valid startup audio shorter than the confirmation window survives immediate
  source switches and recovery replacement. Mid-stream and system/process-loopback
  samples are unchanged.
- macOS microphone and Mix check consent before HAL/cpal format queries.
  Unanswered bundled prompts return an actionable typed backend error within
  30 seconds; undecided consent without usage metadata fails immediately.
  Answer the prompt or configure the host and retry instead of blocking indefinitely.
  Every microphone capture start requires decided consent. Built-in backends
  emit neither `PermissionPending` nor `PermissionGranted` for any source on any
  OS; those event arms remain for custom Rust backends and compatibility.
  In 0.5.2, `ConsentPoll` was wired for `NotDetermined` consent after preflight
  returned `Ok(true)`, but the earlier `query_native_format` could block before
  reaching the capture owner and its pending-consent monitor. Initially
  `Authorized` capture had no continuous revocation monitor. The old pending
  route is removed in 0.5.3; authorization-query failures retain their typed cause.
  macOS device enumeration never prompts: undecided consent fails with a typed
  backend error before any HAL/device/format query. DeviceInfo formats remain
  mandatory, so discovery does not fabricate partial or unknown-format records.
  Windows enumeration does not preflight microphone permission; native discovery
  proceeds. Microphone capture retains its separate existing permission preflight.
- Legacy VAD closing events from a successful discontinuity flush survive a later
  reset or processing failure across Node.js, Python and C. The shared operation
  preserves the original timestamps and reports the original failure; Python/C
  retain closing events for one delivery on a zero-frame carrier in the next poll.
  Node.js keeps one timestamp anchor per legacy VAD epoch, so rounded later chunk
  times cannot move a delayed speech boundary before recording zero.
- Linux process/exclusion fan-in waits for eligible targets to negotiate capture
  format using the same two-second readiness deadline and explained error as
  monitor capture. A completed snapshot with no eligible target remains a
  successful open and continues watching for playback. Offline regression checks
  cover readiness policy and static callback/timer wiring; they do not exercise
  a live PipeWire format-negotiation callback.
- Noncanonical chunk durations now explain the canonical 20 ms frame contract
  and how to set the duration, using one shared validation message.

### Added

- Typed `StartupSamplesDropped` events report the suppressed native microphone
  frame count once per generation with reason `OutOfRange`. Node.js and Python
  expose `startupSamplesDropped` and `outOfRange`; C v2 adds event kind 15 with
  native-frame and reason accessors, and the CLI prints safe startup diagnostics.
  Node.js notice delivery precedes the generation's first PCM callback and attached
  Whisper VAD `epochStart`; other advisories use independent callbacks. Pull APIs
  enqueue the notice chronologically before publishing the generation's first
  chunk, preserving ordinary FIFO event order. PCM polling never waits for an event
  poll; event-first callers can observe the notice before PCM. C v1 projects the
  advisory as `Unknown` with count zero.
- `AudioLoss` reason `generationEnded` reports valid native startup samples
  discarded by a terminal capture failure before their delivery, with native-rate
  scalar interleaved sample counts. C v2 uses loss reason code 6.

[0.5.3]: https://github.com/Studio-Sadola/flexaudio/compare/v0.5.2...v0.5.3

## [0.5.2] - 2026-10-11

### Fixed

- A whisper-compatible VAD failure no longer fails the recording. The N-API
  addon threw the failure from its chunk thread, which surfaced as an
  `uncaughtException` and killed the consumer mid-recording. It is now reported
  once as `onEvent({ type: 'whisperVadFailed', error, epoch })`, delivered after
  the carrier whose `whisperVadEvents` end with `epochEnd(error)`;
  `error.whisperCode` names the typed cause. Capture, PCM delivery,
  `pause`/`resume`, `switchSource` and `stop` keep working, and `terminalError()`
  still means capture primary only.
- `shutdownReport()` keeps an attached VAD failure in `cleanupErrors` with its
  typed `whisperCode`, including a processing-time failure, and never promotes it
  to `primary`. A real capture failure keeps its precedence.
- A failed VAD session reinitializes at the next discontinuity: the
  discontinuity flag, a capture index gap, a PTS reanchor, or the first chunk after
  `resume`/`switchSource`. A successful reset starts a new anchored epoch
  (`epochStart` with `seq` 0) at that first post-gap frame; a failed reset keeps
  the session failed and reports once instead of repeating the event.
  `flushWhisperVad()` remains an explicit reinitialization. Python and C follow
  the same path through the shared capture owner.
- A capture index gap closes the intact prefix with its `epochEnd(finish)` and
  re-anchors the next accepted frame, instead of failing the epoch
  (`flexaudio-vad`). Dropped audio is never replaced by padding, and a
  discontinuity before the first accepted feed simply anchors that epoch.

## [0.5.1] - 2026-10-11

### Fixed

- Python: `WhisperVad` buffer input no longer calls the removed CPython
  `PyObject_AsReadBuffer`, which is not part of the abi3 stable ABI. The
  Windows x64 wheel failed to link, and on other platforms the symbol was only
  resolved at load time. Buffers are now read through stable-ABI APIs with the
  same float32 and layout validation. CI now builds and smoke-tests the Windows
  wheel the same way the release does.
- Fix the npm release workflow failing on the original tag push when GitHub's
  run listing had not indexed the current run yet. It now validates the current
  run directly and retries the listing with bounded backoff, failing closed if
  the run remains unlisted.
- Version 0.5.0 was published to crates.io only; npm and PyPI start at 0.5.1.
  The Rust crates are otherwise unchanged from 0.5.0.

## [0.5.0] - 2026-10-11

### Added

- **WhisperVad:** whisper.cpp-compatible VAD postprocessing ported from
  whisper.cpp, with standalone sessions, probability postprocessing and batch
  segmentation in Rust, N-API, Python and C. It uses the embedded Silero model;
  compatibility describes the pinned postprocessing algorithm, not identical
  probabilities from different model versions. Final segments use integer
  milliseconds on a 10 ms grid. Optional provisional speech/cut events support
  realtime previews; `finish()` infers a zero-padded partial final frame.
- Attached `WhisperVadTap` and binding capture options perform canonical
  48 kHz stereo-to-16 kHz mono conversion, carry capture-clock epoch anchors,
  and deliver closure events on flush/stop. Legacy VAD and WhisperVad cannot be
  enabled on the same stream. Capture transport gaps fail the WhisperVad epoch
  and close published hints without fabricating successful final segments.
- Typed `ErrorKind`, `Error::kind()`, `root()`, `permission()` and
  `with_context()`, `Error::Context`, nonempty `Error::Multiple` groups, and
  `AmbiguousDeviceName`. Errors retain operation, Mix lane and numeric native
  status alongside the primary cause and ordered related failures.
- `ShutdownReport`, `Stream::stop_checked()` and `shutdown_report()` retain
  capture and cleanup outcomes. Repeated stop reuses the result without repeated
  teardown, tails or events. A stopped stream is spent: `start()` returns
  `InvalidState`; this was already true in 0.4 and is now explicit.
- `AudioLoss`, `Clipped`, `RecoverableError`, `ShutdownError` and
  `PermissionGranted` stream events (the grant event is retired in 0.6.0).
  Loss includes path, reason, format and an optional scalar-sample count;
  capture-tap overflow is also reported. The next output after observed loss
  carries `DISCONTINUITY`. Mix clipping is a coalesced advisory without exact
  output-chunk attribution.
- `DeviceEvent::DefaultCleared` and `RescanRequired` allow consumers to clear
  defaults or rebuild an inventory after dropped/unreliable device deltas.
  `ChunkFlags::PADDED = 8` and `CLIPPED = 16` preserve existing flag values.
- C opaque v2 accessors for typed stream/device events, watcher records,
  error information and shutdown reports; standalone `flexaudio_vad_flush`,
  `flexaudio_denoise_flush`, `flexaudio_denoise_samples_free`, and the
  standalone/attached Whisper VAD C API.

### Changed

- Error `Display` retains library-authored explanations and public operation,
  lane and numeric status context, while redacting device names, permission
  detail and internal call labels. Structured access retains opt-in diagnostics.
  Legacy backend error strings from third-party backends are replaced by a safe
  explanation at the facade boundary.
- Peak/RMS are measured from delivered float samples after denoise/gain in all
  bindings, including final tails, before quantization or encoding.
- The CLI warns and continues recording on transient/recoverable failures,
  loss and clipping; terminal and cleanup failures still fail the recording.
  Permission failures print structured permission detail after safe guidance.

### Breaking

These behavior changes are approved by the maintainer for the 0.5 minor release.
Pin to `0.5` for compatible patch updates and apply these migration notes:

- **Discovery and watchers:** incomplete or failed discovery returns an error
  instead of partial/empty success (for example, no PipeWire daemon on Linux).
  Linux watcher startup failure returns an error; C `flexaudio_watch_devices()`
  returns NULL with typed error information instead of a no-op handle. Handle
  discovery/startup failures explicitly. Intentional Windows/macOS no-op
  watchers remain unchanged. Linux `devices()` no longer lists PipeWire
  Audio/Source nodes as microphones because they could not be opened; use the
  advertised CPAL microphone IDs.
- **Capture validation:** `chunk_ms != 20` returns `InvalidArg` (omitted options
  and the C zero sentinel still select 20). Native inputs above two channels
  return `UnsupportedFormat`; select mono/stereo input rather than relying on
  silent front-channel selection. macOS unresolved exclusions fail closed,
  except for processes confirmed gone; resolve live audio helpers and reopen
  capture when the process set changes.
- **Rust:** `AudioChunk` and `SecondaryChunk` require `frame_index: u64` in
  struct literals: the first frame's index on that tap's canonical 48 kHz
  timeline, exposing queue drops as gaps. Use the producing timeline (zero for
  a synthetic chunk starting at zero). `Normalizer::flush()` returns
  `Result<()>`; handle failure. `DeviceEvent::DefaultChanged.kind` changes from
  `SourceKind` to `DefaultDeviceKind` (`Microphone` / `SystemAudio`); update matches and
  constructors. Handle new error/event variants. Fatal Mix lane errors stop
  both lanes and retain lane context; legacy `Event::Error` is terminal, so
  custom backends must use `RecoverableError` for retryable failures.
- **N-API / JavaScript:** u64 counters are `bigint`, including
  `JsVadEvent.atSample`, `chunkDropped.count` and loss counts. `seq` and
  `droppedChunks` were already bigint; PTS remains `number`. Avoid number
  coercion and update arithmetic/serialization. Chunk interfaces require
  `frameIndex`. Stream/device event types are closed discriminated unions with
  new arms; switch on `type` and narrow the payload. Use `shutdownReport()` for
  cleanup outcomes; `terminalError()` and `shutdownReport()` return `undefined`
  when absent. Callback exceptions surface through `uncaughtException`; hosts
  must handle them instead of relying on silently discarded exceptions.
- **Python:** failures use typed exceptions with `.audio_error`.
  `InvalidArgumentError` / `UnsupportedFormatError` subclass `ValueError`;
  the other core exception classes subclass `RuntimeError`.
  `Stream.terminal_error()` returns an `AudioError` instead of an event record.
  Discovery and watcher startup raise on failure, replacing the documented
  never-raises behavior. `stop()` and normal context exit can raise cleanup
  failures; if the context body already failed, its exception remains primary
  with cleanup context attached. Inspect typed errors/reports and drain queued
  output, including the denoise tail now delivered on stop.
- **C source/result compatibility:** v1 physical layouts are preserved, but
  enum-valued fields are now `int32_t` and input booleans are `uint8_t`.
  Update declarations/initializers; all legacy constants keep their values.
  Unknown discriminants and invalid boolean values return `FLEX_INVALID_ARG`.
  v1 result APIs now return the primary root code for core errors rather than
  collapsing them to `FLEX_FAILURE` (-2): `InvalidArg` (-1), `InvalidState` (-4),
  `DeviceNotFound` (-5), `DeviceLost` (-6), `PermissionDenied` (-7),
  `UnsupportedOsVersion` (-8), `Unsupported` (-9), `UnsupportedFormat` (-10),
  `NativeFormatChanged` (-11), and `AmbiguousDeviceName` (-12).
  The new constants -5 through -12 are `FLEX_DEVICE_NOT_FOUND`,
  `FLEX_DEVICE_LOST`, `FLEX_PERMISSION_DENIED`, `FLEX_UNSUPPORTED_OS_VERSION`,
  `FLEX_UNSUPPORTED`, `FLEX_UNSUPPORTED_FORMAT`, `FLEX_NATIVE_FORMAT_CHANGED`,
  and `FLEX_AMBIGUOUS_DEVICE_NAME`. Backend errors still use `FLEX_FAILURE`;
  caught panics retain `FLEX_PANIC` (-3). Treat all negative results as failures
  and use typed v2 accessors for details without parsing messages.

### Fixed

- Preserve final producer buffers and resampler/denoise tails on graceful stop,
  including short 44.1 kHz inputs; retain flush, owner/join and rollback failures
  in one checked shutdown path. Keep Windows capture failure primary when
  cleanup also fails. Terminal capture failures suppress invalid tails.
- Preserve padding/clipping metadata and output-loss discontinuities on both
  taps. Report PipeWire corrupt/short-stride/rejected callback buffers and Mix
  FIFO losses; honor padded PipeWire strides and bound startup/enumeration.
- Flush legacy VAD with the pre-gap anchor before reset across bindings, and
  preserve secondary/VAD closure delivery and terminator counters in N-API.
- Reject ambiguous microphone name selection and unreadable mandatory device
  metadata/configurations. Retain structured native errors and reject unsafe
  native format/layout changes instead of misinterpreting samples.
- Include the whisper.cpp third-party notice in Rust, npm, Python and C packages.

## [0.4.0] - 2026-10-08

### Breaking

- Added `Error::NativeFormatChanged { advertised, actual }` and
  `Event::TerminalError { error }` for format safety and typed terminal backend failures.
- Rust `Error::PermissionDenied` and `Event::PermissionDenied` now carry
  `permission: Permission` (`Microphone` / `SystemAudio`) and `detail: String`.
  Update unit-variant matches to struct-variant matches. `Stream::resume()` now
  returns `Result<()>`; a terminally failed stream also rejects start/resume/source
  switching with its stored error.
- Runtime permission denial terminates capture and suppresses buffered/tail audio
  and automatic retries. N-API `stop()` rejects on terminal failure; Python chunk
  polling raises `RuntimeError`; C chunk polling returns `FLEX_FAILURE` (-2).

### Fixed

- Reject stale microphone sink formats before building native capture, including
  after a macOS consent prompt. Authorization-query failures during consent
  monitoring are terminal, preserve their typed cause, and suppress delivery/retries.
  Permission remedies now reference only the current platform; Windows system/process
  access errors explain access restrictions instead of pointing to macOS settings.
  Python type stubs include terminal errors, permission fields, and the silence advisory.
- macOS microphone capture checks AVFoundation authorization and shares consent
  requests between concurrent opens. An unanswered 30-second consent wait proceeds
  with capture instead of claiming denial. Undecided consent emits the advisory
  `PermissionPending` after five seconds of capture, once per generation, with
  actionable guidance for background hosts. N-API/Python expose `permissionPending`
  with `permission` and `message`; C adds event kind 8 without changing existing
  codes; the CLI warns and continues. Consent monitoring runs every 500 ms for
  the first 60 seconds, then every 2 seconds throughout capture until resolved;
  late denial remains terminal. The mic lane of Mix uses the same policy.
  Windows microphone capture checks public AppCapability consent and rechecks
  native stream failures.
- Confirmed permission errors and events preserve their cause with OS privacy
  settings and restart/retry guidance across Rust, Node.js, Python, C, and CLI.
  Terminal errors remain queryable after stop; the CLI exits unsuccessfully.
- macOS system/process taps run one active self-probe per capture generation
  after five continuous seconds of exact-zero samples with another eligible
  process's output active. A separate private own-process tap checks a roughly
  300 ms, phase-coded 1 kHz diagnostic signal at amplitude `1e-5`, designed to be
  inaudible, on the default output. Capturing the signal suppresses the warning;
  proven rendering with continuous exact-zero diagnostic capture produces a
  terminal `SystemAudio` permission denial. Setup failures, missing render/capture
  evidence, and timeout retain the `SilenceWhileSourceActive` advisory and
  continue capture. Stopping cancels the probe without late events. The diagnostic
  signal may enter recordings that include our own process. No public events or
  binding codes were added, and no private TCC APIs are used.
- **Windows classic system loopback:** keep the selected endpoint's audio engine
  active with an inaudible shared-mode render stream so leading and subsequent
  idle time is captured as continuous device-clocked silence without idle-triggered
  watchdog reopens. Exclusion and per-process capture do not use the silent stream.
  Keepalive startup failures fail capture start; runtime failures stop production
  and are reported through the existing stall/reopen path. The capturing process
  can appear in Windows Volume Mixer while the silent rendering session is active.
- **Windows capture errors and readiness:** report readiness only after capture
  starts, propagate event-wait and capture-buffer-release failures, and reject a
  negotiated classic-loopback mix format that differs from the configured sink.
  Refresh the selected endpoint's mix format before each reopen so a changed
  default endpoint does not leave recovery using a stale rate or channel count.
  Preserve typed permission and device errors from silent-keepalive startup.
- FLAC encoding and I/O failures now retain the first error and prevent retries or
  successful finalization of failed output. Failed writers are never finalized by Drop.
- Encoder bindings validate chunks before lazy file creation, preserve closed and failed
  states across rotation, and reject directory or empty-stem destinations. CLI recording
  also rejects these destinations before opening capture.
- Python FLAC context managers retain close failures alongside exceptions from the body.
- Linux process discovery now rejects incomplete registry snapshots, bind/sync/callback
  failures, and output nodes without resolvable process IDs instead of returning partial
  success. Completed empty queries remain successful; executable and activity metadata
  remain optional.

### Changed

- VAD `process`, `process_pcm`, `reset`, and `flush` now return typed errors instead
  of substituting silence or empty events. Inference/conversion execution failures
  require a successful reset; setup failures preserve existing state. PCM input validates nonzero channels and 8,000–192,000 Hz before mutation.
- Added `Vad::converted_sample_position()` to anchor timestamps using actual
  cumulative converted samples. N-API uses it instead of rounding each chunk;
  Python, N-API, and C VAD callers propagate failures with their original cause.

### Packaging

- Windows MSVC release builds are reproducible with `/Brepro`, no incremental
  compilation or PDBs, path remapping, and pinned Rust toolchain and N-API CLI.
  The npm release CI builds each tag once; `workflow_dispatch` retries reuse and
  verify that run's artifacts via `SHA256SUMS` instead of recompiling. CI checks
  that two independent Windows builds have identical hashes.

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

[Unreleased]: https://github.com/Studio-Sadola/flexaudio/compare/v0.8.1...HEAD
[0.8.1]: https://github.com/Studio-Sadola/flexaudio/compare/v0.8.0...v0.8.1
[0.8.0]: https://github.com/Studio-Sadola/flexaudio/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/Studio-Sadola/flexaudio/compare/v0.6.2...v0.7.0
[0.6.2]: https://github.com/Studio-Sadola/flexaudio/compare/v0.6.1...v0.6.2
[0.6.1]: https://github.com/Studio-Sadola/flexaudio/compare/v0.6.0...v0.6.1
[0.5.2]: https://github.com/Studio-Sadola/flexaudio/compare/v0.5.1...v0.5.2
[0.5.1]: https://github.com/Studio-Sadola/flexaudio/compare/v0.5.0...v0.5.1
[0.5.0]: https://github.com/Studio-Sadola/flexaudio/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/Studio-Sadola/flexaudio/compare/v0.3.1...v0.4.0
[0.3.1]: https://github.com/Studio-Sadola/flexaudio/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/Studio-Sadola/flexaudio/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/Studio-Sadola/flexaudio/releases/tag/v0.2.0
