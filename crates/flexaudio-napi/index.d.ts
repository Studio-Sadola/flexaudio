/* tslint:disable */
/* eslint-disable */

/* Public declarations maintained to match the strict binding boundary contract. */

/** JS device information (`JsDeviceInfo`). `sourceKind` is a string ("mic"|"system"|"process").
 * Unreadable mandatory formats use sampleRate=0, channels=0 and formatAvailable=false.
 * Unknown device display names are empty strings. Windows/macOS use a selectable
 * native endpoint ID/UID when the name is unreadable. IDs may change between queries:
 * a native fallback may become the recovered name. A name-based ID cannot be opened
 * while its name is unreadable; the native ID stays openable in both name states.
 * Unreadable direction on macOS is omitted from discovery; the typed direction cause is returned for the requested device.
 * An unreadable default query leaves isDefault=false. */
export interface JsDeviceInfo {
  id: string
  name: string
  sourceKind: string
  sampleRate: number
  channels: number
  /** False means the mandatory capture format could not be read; retry discovery/open.
   * Linux default-output opens skip this pre-check; PipeWire resolves the sink and negotiates format. */
  formatAvailable: boolean
  isLoopback: boolean
  isDefault: boolean
}
/**
 * JS process information (`JsProcessInfo`, an element of `processes()`). A candidate for per-process capture.
 *
 * Pass `pid` to `openStream({ kind: 'process', processId: pid })` to record that process.
 * `name` / `executable` / `bundleId` are for display (including self-reported app values);
 * the identity key is `pid`. Unreadable optional metadata stays absent; unknown names use pid N.
 * Windows 87/0x80070057 is vanished; 5/0x80070005 and other image failures mean unknown.
 * macOS !obj/0x216f626a, ESRCH=3 and ENOENT=2 are vanished; other metadata failures mean unknown.
 */
export interface JsProcessInfo {
  /** OS process ID (nonzero). Pass to `openStream` as `processId`. */
  pid: number
  /** Display name (always nonempty). OS-reported name → executable name → bundle ID → `"pid <N>"`. */
  name: string
  /** Executable basename; unreadable optional metadata (including access denied) stays absent. */
  executable?: string
  /** macOS bundle ID (e.g. `com.apple.Music`). Present only when available on macOS. */
  bundleId?: string
  /**
   * Whether audio is currently playing. Only when exposed by the OS (Linux=node Running /
   * Windows=session Active / macOS=IsRunningOutput). `undefined` means unknown.
   */
  isOutputActive?: boolean
}
/**
 * JS audio chunk (`JsAudioChunk`). `data` is interleaved f32 (len = frames * channels).
 * `seq` (u64) uses BigInt to avoid precision loss. `flags` is an unsigned 32-bit bitmask.
 *
 * Delivery shape: `onChunk` in `openStream(options, onChunk)` receives **one argument**
 * (this primary chunk). Secondary tap (`secondaryOutput`) chunks arrive in the primary chunk's
 * `secondary` property, rather than a second argument. Finalized VAD events also arrive on
 * the selected `vad.tap` chunk's `vadEvents`, rather than a separate callback
 * ('primary': `chunk.vadEvents`; 'secondary': `chunk.secondary?.vadEvents`).
 *
 * `vadEvents` is populated only when `vad` is specified in `openStream`. When VAD is disabled,
 * it is unset (`undefined`). When enabled with no finalized events in this chunk, it is an empty array.
 */
export interface JsAudioChunk {
  data: Float32Array
  /** Producer-assigned first frame in 48 kHz units; continues across recovery and source switches.
   * At rates divisible by 50 each full chunk advances 960 before queue drops; other rates
   * project cumulative produced frames by floor(totalFrames*48000/outputRate).
   * Secondary uses its own tap timeline. */
  frameIndex: bigint
  frames: number
  /** Arrival-time label assigned at facade ingest and clamped non-decreasing.
   * Callback jitter may cause overlapping ranges, repeated labels or gaps; this is not a media clock.
   * frameIndex is authoritative (gaps are loss). VAD uses epochStart.ptsNs + tMs*1e6,
   * or epochStart.captureSample + tMs*48 for the exact sample-clock mapping. */
  /** @deprecated Since 0.8; use arrivalNs. Retained throughout 0.9. */
  ptsNs: number
  /** Arrival-time label; frameIndex remains the authoritative media clock. */
  readonly arrivalNs?: number
  seq: bigint
  flags: number
  droppedBefore: number
  /** Final float PCM after denoise/gain and before integer encoding. */
  peak: number
  rms: number
  /** Attached events on the selected tap only; absent when vad is disabled. */
  vadEvents?: AttachedVadEvent[]
  /**
   * Timestamp-matched secondary tap chunk (only with `secondaryOutput`). Delivered as a pair in
   * the same callback (`primary.secondary` in `onChunk(primary)`, not a second argument). It is
   * `undefined` when no secondary is available in the pairing window.
   * Delay depends on the secondary format and buffering; a 48 kHz stereo tap
   * bypasses secondary conversion and has no conversion delay. Node expands the
   * primary chunk's 20 ms interval by 30 ms on each side (60 ms pairing tolerance).
   * Arrival labels are not an exact pairing key; each tap has its own `seq`.
   */
  secondary?: JsSecondaryChunk
}
/**
 * JS secondary tap chunk (only with `secondaryOutput`).
 *
 * `data` is a typed array matching `encoding` (`Int16Array` for `'s16'`,
 * `Float32Array` for `'f32'`). Sample values use host native endianness. Serialization to s16le
 * wire format is the receiver's (consumer's) responsibility. `ptsNs` uses the same recording-zero
 * clock as the primary but is independent. Delay depends on the secondary format and
 * buffering; a 48 kHz stereo tap bypasses secondary conversion and has no conversion delay.
 * Node expands the primary chunk's 20 ms interval by 30 ms on each side
 * (60 ms pairing tolerance). Arrival labels are not an exact pairing key.
 */
export interface JsSecondaryChunk {
  data: Int16Array | Float32Array
  /** 'f32' | 's16' (discriminator for narrowing the type of `data`). */
  encoding: string
  /** Producer-assigned first frame in 48 kHz units; continues across recovery and source switches.
   * At rates divisible by 50 each full chunk advances 960 before queue drops; other rates
   * project cumulative produced frames by floor(totalFrames*48000/outputRate).
   * Secondary uses its own tap timeline. */
  frameIndex: bigint
  frames: number
  /** Arrival-time label assigned at facade ingest and clamped non-decreasing.
   * Callback jitter may cause overlapping ranges, repeated labels or gaps; this is not a media clock.
   * frameIndex is authoritative (gaps are loss). VAD uses epochStart.ptsNs + tMs*1e6,
   * or epochStart.captureSample + tMs*48 for the exact sample-clock mapping. */
  /** @deprecated Since 0.8; use arrivalNs. Retained throughout 0.9. */
  ptsNs: number
  /** Exact alias of ptsNs on library-created chunks. */
  readonly arrivalNs?: number
  seq: bigint
  flags: number
  droppedBefore: number
  /** Computed from f32 before quantization (preserves meter precision even with s16). */
  /** Final float PCM after denoise/gain and before integer encoding. */
  peak: number
  rms: number
  /** Attached events on the selected tap only; absent when vad is disabled. */
  vadEvents?: AttachedVadEvent[]
}
/** JS native format (returned by `FlexStream.nativeFormat`). */
export interface JsNativeFormat {
  sampleRate: number
  channels: number
}
/** Streaming segmentation settings, shared by standalone and attached VAD. */
export interface VadOptions {
  /** Finite [0,1], default 0.5. */
  threshold?: number
  /** Integer 0..134217 ms, default 250. */
  minSpeechMs?: number
  /** Integer 0..134217 ms, default 100. */
  minSilenceMs?: number
  /** Integer milliseconds. Zero (default) or values above 100000000 use the effectively unbounded INT_MAX/2-sample sentinel (1073741823 samples). Other positive values must be at least 32 + 2*speechPadMs; the split budget is maxSpeechMs*16 - 512 - 2*speechPadMs*16 samples. Zero budgets are valid; negative budgets throw InvalidParameter. Merging can undo splits. */
  maxSpeechMs?: number
  /** Integer 0..134217 ms, default 30. */
  speechPadMs?: number
  /** Add immediate hints and nonoverlapping pieces <=30000 ms; default false. */
  provisional?: boolean
}

export type VadCloseReason = 'hysteresis' | 'finish' | 'reset' | 'error'
/** Epoch-relative integer milliseconds. Final segment endpoints use the 10 ms grid. */
export type VadEvent = { epoch: number; seq: number } & (
  | { type: 'segment'; startMs: number; endMs: number }
  | { type: 'provisionalSpeechStart'; atMs: number }
  | { type: 'provisionalSpeechEnd'; atMs: number; reason: VadCloseReason }
  | { type: 'provisionalCut'; startMs: number; endMs: number; reason: 'limit' | VadCloseReason }
  | { type: 'epochEnd'; reason: 'finish' | 'reset' | 'error' }
)

/** Capture origins appear only on attached streams, before any VAD payload in that epoch. */
export type AttachedVadEvent = VadEvent
  /** captureSample counts 48 kHz frames on the dedicated capture timeline, independent
   * of primary/secondary format and tap. captureSample + tMs*48 is exact on that timeline.
   * Equal to primary frameIndex for the same sample only for a 48 kHz primary.
   * At other rates units match but resampler group delay leaves origins unequal.
   * Unreconciled startup-release padding can grow the offset by up to one primary chunk per boundary.
   * Neither index is an offset into concatenated PCM; pause/queue drops advance them. */
  | { type: 'epochStart'; epoch: number; seq: 0; captureSample: bigint; ptsNs: number }

/** Attach one canonical VAD owner; tap selects only its event carrier. */
export interface StreamVadOptions {
  params?: Omit<VadOptions, 'provisional'>
  provisional?: boolean
  tap: 'primary' | 'secondary'
}

export interface FrameProbabilities {
  firstFrameIndex: number
  /** Owned copy from the latest successful call; one probability per 512 samples. */
  values: Float32Array
}

export type VadErrorCode = 'InvalidParameter' | 'InvalidPcm' | 'InvalidProbability'
  | 'Overflow' | 'SessionFinished' | 'Inference' | 'ModelLoad' | 'FailedSession'
  | 'UnsupportedTap' | 'UnsupportedConversionClock'
  | 'InvalidStereoLength' | 'CaptureSampleOverflow' | 'PtsOutOfRange' | 'Conversion' | 'Stopped'

/** Drain terminalEvents through the normal consumer even when a call throws. */
export interface VadError extends Error {
  code: VadErrorCode
  terminalEvents: VadEvent[]
  /** Present on checked capture shutdown; includes every primary and cleanup cause. */
  audioError?: AudioError
}

/** Synchronous, serialized inference; use a Node Worker for realtime processing. */
export declare class Vad {
  constructor(params?: VadOptions)
  /** Normalized [-1,1] interleaved float32 PCM; defaults to 16000 Hz, mono.
   * Rates of 8000–192000 Hz and positive channel counts are converted internally. Events use the input timeline in
   * milliseconds, compensated for converter group delay. Finite converter overshoot
   * is clipped to [-1,1]; invalid source PCM and nonfinite converter output are rejected.
   * Input is not retained. */
  process(samples: Float32Array, inputSampleRate?: number, inputChannels?: number): VadEvent[]
  /** True EOF pads one partial frame; repeated finish returns []. */
  finish(): VadEvent[]
  /** Closes hints and discards pending finals, then starts a fresh epoch. */
  reset(): VadEvent[]
  lastFrameProbabilities(): FrameProbabilities
}
/**
 * Stream notification. permissionDenied is terminal, with permission and
 * cause/remedy in message. permissionPending is retained for
 * custom Rust backends; built-in backends emit no permissionPending on
 * any OS or capture source. A custom permissionPending is advisory. silenceWhileSourceActive
 * is advisory: the system-audio diagnosis is inconclusive and capture continues.
 * Missing permission and genuine digital silence remain possible in that case.
 */
export type AudioPath =
  | { type: 'capture'; lane: 'microphone' | 'systemAudio' | null }
  | { type: 'mixFifo'; lane: 'microphone' | 'systemAudio' }
  | { type: 'output'; tap: 'primary' | 'secondary' }

export type NativeStatus =
  | { type: 'hresult'; call: string; bits: number }
  | { type: 'osStatus'; call: string; value: number }

export interface ErrorContext {
  operation: 'enumerate' | 'start' | 'normalize' | 'flush' | 'reopen'
    | 'rollback' | 'stop' | 'join' | 'link'
  lane: 'microphone' | 'systemAudio' | null
  nativeStatus: NativeStatus | null
}

/** Library-authored message, outer-to-inner contexts, ordered related failures. */
export type AddonErrorKind = 'vadModelLoad' | 'vadInference' | 'vadInvalidConfig' | 'vadInvalidFormat'
  | 'vadResample' | 'vadReset' | 'flacIo' | 'flacUnsupported' | 'flacEncoder'

export type AudioError = {
  message: string
  contexts: readonly ErrorContext[]
  secondary: readonly AudioError[]
  /** Opt-in native details may contain private host data; excluded from message. */
  diagnosticDetails: readonly string[]
  addonKind?: AddonErrorKind
  addonMessage?: string
} & (
  | { kind: 'invalidArg' }
  | { kind: 'invalidState' }
  | { kind: 'deviceNotFound' }
  | { kind: 'permissionDenied'; permission: 'microphone' | 'systemAudio' }
  | { kind: 'unsupportedOsVersion' }
  | { kind: 'deviceLost' }
  | { kind: 'backend'; vadCode?: VadErrorCode }
  | { kind: 'unsupportedFormat' }
  | { kind: 'nativeFormatChanged'; advertised: JsNativeFormat; actual: JsNativeFormat }
  | { kind: 'unsupported' }
  | { kind: 'ambiguousDeviceName' }
  | { kind: 'unknown' }
)

export interface AudioException extends Error {
  code: 'FLEX_INVALID_ARG' | 'FLEX_INVALID_STATE' | 'FLEX_DEVICE_NOT_FOUND'
    | 'FLEX_PERMISSION_DENIED' | 'FLEX_UNSUPPORTED_OS_VERSION' | 'FLEX_DEVICE_LOST'
    | 'FLEX_FAILURE' | 'FLEX_UNSUPPORTED_FORMAT' | 'FLEX_NATIVE_FORMAT_CHANGED'
    | 'FLEX_UNSUPPORTED' | 'FLEX_AMBIGUOUS_DEVICE_NAME'
  audioError: AudioError
}

export interface AudioLoss {
  path: AudioPath
  /** generationEnded means a terminal capture discarded a held valid native startup
   * prefix; its scalar sample count, rate and channels describe that native input. */
  reason: 'rawOverflow' | 'mixFifoOverflow' | 'corruptBuffer' | 'malformedBuffer'
    | 'callbackRejected' | 'outputOverflow' | 'generationEnded'
  /** Interval count of scalar interleaved samples at the discard point; null means unknown. */
  samples: bigint | null
  sampleRate: number
  channels: number
}

export interface ShutdownReport {
  /** Capture failure only; an attached streaming VAD failure is never the capture
   * primary, so it stays null for an attached VAD failure (see vadFailed). */
  primary: AudioError | null
  /** Core teardown, then the attached VAD failure, then addon final flush failures,
   * retained in order. An attached VAD failure keeps its typed vadCode here. */
  cleanupErrors: readonly AudioError[]
}

/** Arrival gaps without reopen emit no event/DISCONTINUITY: frameIndex stays contiguous,
 * primary and secondary arrival labels can jump, and attached Vad keeps its epoch open.
 * Stall: >=2 s without consumed samples, checked every 250 ms; stalled, reopen,
 * each failed reopen attempt emits recoverableError and backs off 250 ms..5 s.
 * On success, recovered is emitted when new PCM arrives. First new-generation primary
 * data carries RECOVERED|DISCONTINUITY. Vad epochEnd(finish) and new epochStart
 * wait for new PCM; downtime does not become frames. These advisories are unordered
 * relative to onChunk and epoch events. Attached epochs strictly increase within a
 * stream (checked epoch+1 on rebuild); new openStream starts at 0. (epoch, seq) is
 * unique per stream; epochEnd(error) precedes vadFailed. */
export type JsStreamEvent =
  | { type: 'chunkDropped'; count: bigint }
  | { type: 'stalled' }
  | { type: 'recovered' }
  | { type: 'permissionDenied'; permission: 'microphone' | 'systemAudio'; message: string }
  | { type: 'permissionPending'; permission: 'microphone' | 'systemAudio'; message: string }
  | { type: 'silenceWhileSourceActive'; message: string }
  | { type: 'deviceLost' }
  | { type: 'error'; message: string }
  | { type: 'terminalError'; error: AudioError }
  | { type: 'recoverableError'; error: AudioError }
  | { type: 'shutdownError'; error: AudioError }
  /** The attached streaming VAD session failed; capture, PCM delivery, pause/resume,
   * switchSource and stop keep working. `error.vadCode` names the typed cause. Delivered
   * after the carrier whose `vadEvents` end with `epochEnd(error)`, and before stop
   * resolves. Reported once per failure; the next discontinuity reinitializes the session
   * and the following failure is
   * reported again. `epoch` is the failed epoch (the one closed by that `epochEnd(error)`). */
  | { type: 'vadFailed'; error: AudioError; epoch: number }
  | { type: 'audioLoss'; loss: AudioLoss }
  /** Coalesced upstream clipping; no exact chunk, count, timestamp or lane attribution. */
  | { type: 'clipped' }
  /** Delivered before this generation's first onChunk and attached VAD epochStart. */
  | { type: 'startupSamplesDropped'; frames: bigint; reason: 'outOfRange' }
  | { type: 'unknown'; message: string }

export type JsDeviceEvent =
  | { type: 'added'; device: JsDeviceInfo }
  | { type: 'removed'; id: string }
  | { type: 'defaultChanged'; sourceKind: 'mic' | 'system'; id: string }
  | { type: 'defaultCleared'; sourceKind: 'mic' | 'system' }
  | { type: 'rescanRequired'; droppedEvents: bigint }
  | { type: 'terminalError'; error: AudioError }
  | { type: 'unknown'; message: string }

/**
 * Secondary output tap specification (`OpenOptions.secondaryOutput`).
 *
 * Returns the same capture as the primary output (`outputRate`/`outputChannels`) in another format simultaneously.
 * Used for paired outputs such as 48k/stereo storage + 16k/mono/s16 recognition.
 */
export interface SecondaryOutputOptions {
  /** Secondary output sample rate (Hz). Example: 16000. */
  rate: number
  /** Secondary output channel count (1=mono / 2=stereo). Example: 1. */
  channels: number
  /**
   * Secondary chunk sample encoding. 'f32' (default) | 's16'. s16 is quantized after VAD
   * and returned as `Int16Array` (native-endian values).
   */
  encoding?: string
}
/** Options for openStream. */
export interface OpenOptions {
  /** Reject non-default settings that would be ignored. Retained on switch when omitted. */
  strict?: boolean
  /** Positive safe integer queue capacity. Default 50; fixed at open. */
  ringCapacityChunks?: number
  /**
   * Separate ordered helper lifecycle channel. Omission on switch retains the handler.
   * The handler must return without synchronously waiting for stop(): stop settles
   * only after queued lifecycle handlers return. Start asynchronous shutdown with
   * `void stream.stop().catch(handleError)` or await it outside the callback.
   * Returned promises are not awaited by the binding.
   */
  onLifecycleEvent?: (event: LifecycleEvent) => void
  /** "mic" | "system" | "process" | "mix" */
  kind: string
  deviceId?: string
  /**
   * Target process ID for `process` capture. Must be a finite positive integer
   * in 1..=4294967295; invalid values fail with InvalidArg.
   */
  processId?: number
  /**
   * How to handle the target PID for process capture (process only). "include" (default) | "exclude".
   * include=record only the target PID / exclude=all system audio except the target PID (process_id required).
   * Ignored for mic / system. Supported on all three OSes: Linux / Windows / macOS.
   */
  mode?: string
  /**
   * Exclude the host process from `system` capture (also the system side of
   * `mix`). Defaults to false; ignored by mic/process. Supported on Linux,
   * Windows and macOS. Windows excludes the host's entire process tree;
   * Electron's audio utility process is a direct child and is covered.
   */
  excludeSelf?: boolean
  /**
   * Process IDs whose playback is excluded from `system` capture (also the
   * system side of `mix`), in addition to `excludeSelf`. Each must be a finite
   * positive integer in 1..=4294967295; invalid values fail with InvalidArg.
   * On macOS, each PID must also fit a positive signed 32-bit integer
   * (1..=2147483647), or capture fails.
   * Duplicates are allowed. Ignored by mic/process after validation.
   * Linux and macOS exclude every listed PID. macOS resolves audio objects
   * at capture start: an unresolved live process fails startup; only a non-host
   * process confirmed gone is omitted. Unresolved host exclusion always fails startup.
   * On Linux, while exclusion is active, a pipewire-pulse-relayed stream
   * without a known application.process.id is not captured.
   * Windows can exclude only one process tree. For Electron, use
   * `excludeSelf: true` alone; any other PID fails with an error.
   */
  excludePids?: Array<number>
  /** Default 48000 */
  outputRate?: number
  /** Default 2 */
  outputChannels?: number
  /** Omitted selects 20; every other value is rejected with FLEX_INVALID_ARG. */
  chunkMs?: number
  /**
   * Initial input gain (linear multiplier). Default 1.0. 1.0=unchanged, 2.0=about +6dB, 0.0=silence.
   * Use `setGain` for runtime changes.
   */
  gain?: number
  /** Input device ID for the mic side of mix (mix only). Defaults to the default input. */
  micDeviceId?: string
  /** Output endpoint ID for the system side of mix (mix only). Defaults to the default output. */
  systemDeviceId?: string
  /** Pre-mix multiplier for the mic side (linear, mix only). Default 1.0. `gain` applies after mixing. */
  micGain?: number
  /** Pre-mix multiplier for the system side (linear, mix only). Default 1.0. */
  systemGain?: number
  /** Attach VAD to canonical capture; tap selects only the event carrier. */
  vad?: StreamVadOptions
  /** Enable the macOS silence diagnostic probe. Default true; ignored on other systems. */
  silenceProbe?: boolean
  /**
   * Render silence to keep Windows classic system loopback delivering idle packets.
   * Omitted on openStream defaults to true; omitted on switchSource keeps the current value.
   * Applies to system capture and the system lane of mix without PID exclusions.
   * Ignored on other platforms and process/exclusion paths. When disabled, idle endpoints
   * deliver no packets; the existing 2 s watchdog reports stalled and reopens until audio plays.
   */
  windowsClassicLoopbackKeepalive?: boolean
  /**
   * true enables recording-time noise suppression. **Only available with 48000 Hz output**
   * (RNNoise is fixed at 48kHz). Applied once to the internal 48kHz/stereo canonical form;
   * both primary and secondary taps receive denoised audio (+10ms fixed latency). Setting true
   * at any other rate makes `openStream` throw InvalidArg. Omission/false disables noise suppression.
   */
  denoise?: boolean
  /**
   * Secondary output tap. When specified, returns paired chunks in another format simultaneously
   * (`primary.secondary` in `onChunk`). Omission means no secondary tap, preserving previous behavior.
   */
  secondaryOutput?: SecondaryOutputOptions
}
/** Numeric OS version. Null components were not reported by the OS. */
export interface OsVersion {
  major: number
  minor: number
  patch: number | null
  build: number | null
}
export type BackendKind = 'cpal' | 'pipeWire' | 'wasapiLoopback' | 'wasapiProcessLoopback' | 'coreAudioProcessTap' | 'composite' | 'unsupported' | 'other'
export type CapabilityUnavailableReason =
  | { kind: 'unsupportedOs' }
  | { kind: 'osVersionTooOld'; required: OsVersion; actual: OsVersion }
  | { kind: 'pipeWireUnavailable' }
  | { kind: 'notApplicable' }
  | { kind: 'other'; code: string }
export type CapabilityAvailability =
  | { status: 'supported' }
  | { status: 'unavailable'; reason: CapabilityUnavailableReason }
  | { status: 'unknown'; error: AudioError }
export type PidScope = 'exactPid' | 'processTree' | 'notApplicable' | 'other'
export type DeviceSelectionBehavior = 'honored' | 'ignored' | 'notApplicable' | 'other'
export type WatcherMode = 'native' | 'noop' | 'other'
export type WatchScope = 'systemOutputs' | 'defaultSystemOutput' | 'other'
export type SideEffectCondition = 'silentActiveSystemAudio' | 'classicLoopbackWithoutExclusions' | 'other'
export interface SourceCapability {
  backend: BackendKind
  availability: CapabilityAvailability
}
export interface SourceCapabilities {
  mic: SourceCapability
  system: SourceCapability
  process: SourceCapability
  mix: SourceCapability
}
export interface SystemExclusionCapabilities {
  availability: CapabilityAvailability
  backend: BackendKind
  scope: PidScope
  /** Distinct PID limit including implicit self; null means no count limit. */
  maxDistinctPids: number | null
  deviceSelection: DeviceSelectionBehavior
}
export interface ProcessCapabilities {
  includeScope: PidScope
  excludeScope: PidScope
}
export interface DeviceWatcherCapabilities {
  availability: CapabilityAvailability
  mode: WatcherMode
  scopes: WatchScope[]
}
export interface SideEffectCapability {
  availability: CapabilityAvailability
  enabledByDefault: boolean
  sources: Array<'mic' | 'system' | 'process' | 'mix' | 'other'>
  condition: SideEffectCondition
}
export interface SideEffectCapabilities {
  silenceProbe: SideEffectCapability
  classicLoopbackKeepalive: SideEffectCapability
}
/** Platform support snapshot; support is independent of device availability and permission. */
export interface Capabilities {
  osVersion: OsVersion | null
  sources: SourceCapabilities
  systemExclusions: SystemExclusionCapabilities
  process: ProcessCapabilities
  deviceWatcher: DeviceWatcherCapabilities
  sideEffects: SideEffectCapabilities
}
/**
 * Query backend support and optional side effects without enumerating microphones,
 * opening streams, playing audio or requesting recording permission. The bounded
 * prerequisite probe runs on the libuv thread pool, keeping the JS thread free.
 * Probe failures resolve as unknown availability containing a structured AudioError.
 * Supported describes platform prerequisites, not device availability or permission.
 * Unknown future enum values use other; an unknown unavailable reason includes code.
 */
export declare function capabilities(): Promise<Capabilities>
/** Enumerate the complete inventory. Discovery failure throws AudioException.
 * On macOS, enumeration never prompts. With undecided microphone consent it
 * throws a typed Backend error before any HAL/device/format query. Grant the
 * responsible host access or use the bounded capture-open consent flow, then retry.
 * Denied/restricted consent throws PermissionDenied. Unreadable mandatory formats
 * use formatAvailable=false and 0/0. Optional metadata remains unknown; collection/count
 * failures throw. Entries with no readable selectable identity may be omitted.
 * Windows enumeration does not preflight microphone permission; native enumeration
 * proceeds. Microphone capture retains its separate existing permission preflight. */
export declare function devices(): Array<JsDeviceInfo>
export type DeviceIdNamespace =
  'cpalDeviceName' | 'pipeWireNodeName' | 'wasapiOutputSelector' |
  'coreAudioOutputSelector' | 'unknown'
export interface NamespacedDevice {
  device: JsDeviceInfo
  idNamespace: DeviceIdNamespace
}
/** Enumerate the same selectable IDs with provenance supplied by their providers. */
export declare function devicesWithNamespaces(): Promise<NamespacedDevice[]>

export type SettingKey =
  'kind' | 'deviceId' | 'targetPid' | 'mode' | 'chunkMs' | 'ringCapacityChunks' |
  'excludeSelf' | 'excludePids' | 'output' | 'secondaryOutput' | 'secondaryEncoding' |
  'gain' | 'mixMicDeviceId' | 'mixSystemDeviceId' | 'mixMicGain' | 'mixSystemGain' |
  'silenceProbe' | 'windowsClassicLoopbackKeepalive' | 'denoise' | 'vad'
export type IgnoreReason =
  'wrongSourceKind' | 'unsupportedPlatform' | 'exclusionDeviceUnscoped' |
  'processLoopbackPath' | 'fixedAtOpen'
export type AdjustmentReason = 'exclusionSetNormalized' | 'legacySentinel'
export type SettingValue =
  null | boolean | bigint | number | string | readonly number[] | JsNativeFormat
export type SettingOutcome =
  | { status: 'notRequested' | 'applied' }
  | { status: 'ignored'; reason: IgnoreReason }
  | { status: 'adjusted'; reason: AdjustmentReason }
export interface SettingResolution {
  setting: SettingKey
  requested: SettingValue
  effective: SettingValue
  outcome: SettingOutcome
}
export interface SettingsReport {
  revision: bigint
  entries: readonly SettingResolution[]
}
export interface IgnoredSettingsException extends Error {
  code: 'FLEX_IGNORED_SETTINGS'
  report: SettingsReport
}
export type HelperStopReason =
  'signalObserved' | 'permissionDenied' | 'inconclusive' | 'timedOut' |
  'streamStopped' | 'sourceSwitched' | 'recovery' | 'startupFailed' |
  'runtimeFailed' | 'cancelled' | 'unknown'
export type LifecycleEvent =
  | { type: 'silenceProbeStarted' | 'classicLoopbackKeepaliveStarted' }
  | { type: 'silenceProbeStopped' | 'classicLoopbackKeepaliveStopped'; reason: HelperStopReason }
  | { type: 'unknown'; message: string }
/**
 * Enumerate processes with audio output sessions (streams) that can be targeted by per-process
 * capture (`openStream({ kind: 'process', processId })`). Excludes the calling process itself.
 * Stopped/Idle sessions are included. Use `isOutputActive` to check whether audio is playing.
 *
 * Sorted by active output (`isOutputActive: true`) first → display name → pid, with each pid
 * deduplicated. Read-only, with no permission prompts. Returns within 3 seconds even if the OS
 * does not respond. Runs on the libuv thread pool without blocking the JS event loop.
 *
 * - Linux (PipeWire): clients with `Stream/Output/Audio` nodes. `executable` comes from
 *   `/proc/<pid>/exe`, falling back to `/proc/<pid>/comm` if unreadable.
 * - Windows: processes with audio sessions on enabled output devices. Both enumeration and capture
 *   require Windows build 20348 or later (Windows 11 / Windows Server 2022).
 * - macOS 14.4+: process objects known to Core Audio (with `bundleId`,
 *   including input-only processes).
 *
 * Interpreting results: an empty array means per-process capture is available but no qualifying
 * processes currently exist (not "nothing is playing"). Rejection means per-process capture is
 * unavailable (PipeWire unreachable on Linux; macOS below 14.4 / Windows below build 20348 return
 * `unsupported OS version`; other OSes return `unsupported`; permission denied), the OS did not
 * respond within the time limit, or the previous query is still pending (same `Error`
 * type and wording as the synchronous version).
 */
export declare function processes(): Promise<Array<JsProcessInfo>>
/**
 * Open and start a stream, returning a `FlexStream` that sends chunks/events to callbacks.
 * Confirmed microphone/system-audio denial throws actionable permission guidance.
 * Supply onEvent for startupSamplesDropped, runtime denial and silenceWhileSourceActive
 * advisories. Without onEvent, terminalError() and stop() expose terminal failures
 * only; they do not expose advisories. macOS microphone authorization is checked
 * before any HAL format query. Bundled prompts wait at most 30 s; an unanswered
 * prompt throws a typed backend error without starting capture. Answer the prompt
 * and retry. Undecided consent without a nonempty NSMicrophoneUsageDescription
 * fails immediately; configure the bundle or grant the responsible host access
 * before retrying. openStream is synchronous; use a worker thread for GUI hosts.
 * The 30 s bound covers undecided consent, not authorized native queries.
 * Built-in sources on every OS do not emit permissionPending;
 * that advisory remains for custom Rust backends. macOS microphone
 * capture requires decided consent before each start and does not continuously
 * monitor authorization changes or revocation. With onEvent, startupSamplesDropped
 * is delivered before the affected generation's first onChunk and attached epochStart.
 * Recovered and other advisories use independent callbacks and may arrive
 * before or after the startup notice; they have no ordering guarantee relative to it.
 * Windows checks Microphone privacy, including Let desktop apps access your microphone.
 *
 * macOS system/process capture, including the system lane of Mix, runs a once-per-generation self-probe after 5 s of
 * exact-zero samples with eligible external output active. It renders a roughly
 * 300 ms low-level diagnostic signal, not guaranteed inaudible, on the default output and captures only our own process
 * in a separate private tap. A confirmed failure emits terminal permissionDenied
 * with permission systemAudio; an inconclusive result emits silenceWhileSourceActive
 * and continues. The signal may enter capture when our own process is included
 * (e.g. excludeSelf: false). Stopping cancels the probe without a late event.
 *
 * `denoise`, `secondaryOutput` and `vad` are independent optional features.
 * The attached VAD consumes canonical capture; `vad.tap` selects its event carrier.
 * Both VAD modes default to an unbounded maximum speech duration. `flushVad()`
 * finishes/resets the current epoch and waits for closing-event delivery.
 * The 20 ms chunk size is fixed because conversion and ordered event delivery
 * share that cadence. The callback receives one primary chunk; secondary PCM is
 * in `chunk.secondary`. Avoid heavy synchronous work in callbacks.
 */
export declare function openStream(options: OpenOptions, onChunk: (chunk: JsAudioChunk) => void, onEvent?: ((event: JsStreamEvent) => void) | undefined | null): FlexStream
/**
 * Watch Linux PipeWire system output additions, removals and default changes using
 * the same output IDs as devices(). Microphone and default-microphone changes are
 * not reported: CPAL microphone IDs cannot be resolved from PipeWire nodes.
 * Windows/macOS use a no-op watcher. Linux startup failure throws AudioException.
 * RescanRequired invalidates incremental caches.
 */
export declare function watchDevices(onEvent: (event: JsDeviceEvent) => void): DeviceWatcherHandle
/**
 * Recording stream handle (`FlexStream`). Internally, the bridge thread polls captured audio
 * and sends chunks/events to JS through TSFN.
 */
export declare class FlexStream {
  /** Snapshot at successful open/switch commit; available after stop. */
  effectiveSettings(): SettingsReport
  /**
   * Stop recording. When the Promise resolves, all `onChunk` calls queued on the TSFN before
   * stop (the last PCM and `frames:0` terminator) have been delivered to JS.
   * Queued lifecycle callbacks have also completed.
   * Never synchronously wait for this Promise inside onLifecycleEvent: settlement
   * needs that handler to return. Start shutdown asynchronously or await outside it.
   * Repeated calls return the same retained success or failure after completion. Calling
   * inside `onChunk` does not freeze JS because joining happens off the JS thread.
   * An attached VAD-only failure resolves and remains in shutdownReport().cleanupErrors with its
   * typed vadCode. Rejects with AudioException on capture or other cleanup failure;
   * audioError retains all related causes. Capture failure suppresses
   * further PCM and the normal terminator. Cleanup-only failure preserves valid graceful output.
   * The stopped stream is spent; call openStream to capture again.
   */
  stop(): Promise<void>
  /** Stored terminal failure, including after stop; does not consume onEvent. */
  terminalError(): JsStreamEvent | undefined
  /** Retained final capture primary and ordered cleanup failures; undefined until completed. */
  shutdownReport(): ShutdownReport | undefined
  /**
   * Every successful switch creates a new generation with DISCONTINUITY. The epoch
   * transition waits for new PCM. No re-entrant onChunk occurs during this synchronous
   * call; return means backend replaced, not delivered. Old epochEnd need not precede
   * the first new-generation primary chunk; use primary DISCONTINUITY as the boundary.
   * Hot-swap the input source (mic/system/process) without stopping recording.
   *
   * Request a switch using `OpenOptions` on the bridge thread and return
   * synchronously (`undefined` on success, exception on failure). The output format (`outputRate`/
   * `outputChannels`) cannot change during a switch (it would change frames in a continuous stream).
   * Requesting a format change makes `switchSource` throw InvalidArg. Chunk
   * `seq` stays continuous across switches; the first chunk afterward has the DISCONTINUITY flag.
   * `options.gain` is ignored (gain is stream state; use `setGain` to change it).
   * An explicit `options.windowsClassicLoopbackKeepalive` replaces the current value;
   * omission retains it. `options.denoise` and `options.vad` are ignored;
   * the existing denoise and VAD settings remain in effect.
   *
   * Throws once `stop()` has been requested, including while shutdown is pending.
   */
  switchSource(options: OpenOptions): void
  /**
   * Pause recording. Keep the device running but stop delivery. `resume` restarts delivery;
   * the first chunk afterward has DISCONTINUITY. Throws if `stop()` has already completed.
   */
  pause(): void
  /** Unpause delivery. Throws the stored error on terminal failure. */
  resume(): void
  /** Drain/finish/reset an attached epoch; its closing carrier is delivered before settlement. */
  flushVad(): Promise<void>
  /**
   * Change input gain (linear multiplier). 1.0=unchanged, 2.0=about +6dB, 0.0=silence. Callable
   * any time during recording; takes effect on the next chunk (20ms granularity). Multiplied samples
   * are clamped to ±1.0. Throws unless finite and nonnegative, or if `stop()` has already completed.
   */
  setGain(gain: number): void
  /** Whether currently paused. Throws if `stop()` has already completed. */
  isPaused(): boolean
  /** Current input gain (linear multiplier). Throws if `stop()` has already completed. */
  gain(): number
  /**
   * Current backend's native format `{ sampleRate, channels }`, for display/diagnostics
   * (delivered chunks use output format `outputRate`/`outputChannels`).
   * Changing sources with `switchSource` updates this to the new backend's values. Throws if `stop()`
   * has already completed.
   */
  nativeFormat(): JsNativeFormat
  /**
   * Cumulative chunks discarded by the chunk ring's DROP_OLDEST policy (BigInt). Throws if `stop()`
   * has already completed.
   */
  droppedChunks(): bigint
}
/** Device attachment/removal watcher handle (`DeviceWatcherHandle`). */
export declare class DeviceWatcherHandle {
  /** Stop watching and join the bridge thread. Safe to call repeatedly. */
  stop(): void
}
/**
 * Writer that incrementally saves recording chunks to FLAC files with lossless compression.
 *
 * When `splitSeconds` is at least 1, reaching `splitSeconds × sampleRate` written frames closes
 * the current file and rotates to the next, using three-digit sequence numbers such as
 * `name-001.flac, name-002.flac, …` (same convention as CLI WAV splitting). Boundaries advance
 * at chunk granularity when the threshold is reached, so each file may exceed the specified duration
 * by up to one chunk; chunks are never split and no data is dropped. Omitted/0 `splitSeconds` uses one file.
 */
export declare class FlacEncoder {
  /**
   * Create a FLAC writer. Omitted/0 `splitSeconds` uses one file; at least 1 enables timed rotation.
   *
   * `channels` must be 1..=2, `sampleRate` 1..=96000 Hz (otherwise InvalidArg). When splitting,
   * the first file (`name-001.flac`) is created immediately.
   */
  static create(path: string, sampleRate: number, channels: number, splitSeconds?: number | undefined | null): FlacEncoder
  /**
   * Append interleaved f32 (length must be a multiple of `channels`, otherwise InvalidArg).
   *
   * After writing, if the current file's frame count reaches the threshold, finalize immediately and
   * rotate to the next file (the next `writeChunk` starts the new file).
   */
  writeChunk(samples: Float32Array): void
  /**
   * Write out the remainder, finalize the header, and close the open file. Safe to call repeatedly
   * (subsequent calls are no-ops). Releasing `FlacEncoder` without calling this still makes a
   * best-effort attempt to close the file, but call `finalize()` to detect write errors.
   */
  finalize(): void
}
/**
 * Offline noise suppressor (RNNoise via nnnoiseless, embedded weights). **Requires 48kHz**;
 * intended to reduce steady noise in microphone recordings (fans, air conditioning, typing, etc.).
 *
 * Fixed latency of 480 samples per channel (10ms at 48kHz). Output is
 * the input delayed by one frame. The first frame is silence padding;
 * retrieve the remaining trailing frame with `Denoiser.flush()`.
 */
export declare class Denoiser {
  /**
   * Construct with a channel count (1 = mono, 2 = stereo interleaved). Out-of-range values yield
   * InvalidArg.
   */
  constructor(channels: number)
  /**
   * Suppress noise in arbitrary-length interleaved f32 (±1.0 normalized, 48kHz, length a multiple of
   * channels), returning a **new array** (copied because in-place processing is awkward in napi).
   * A length not divisible by channels yields InvalidArg.
   */
  process(samples: Float32Array): Float32Array
  /**
   * Process the carried remainder, return the trailing delay (1 frame/ch), and close the stream.
   * Then return to the freshly constructed state, ready to process another stream.
   */
  flush(): Float32Array
  /** Initialize all RNN state, carryover, and delay lines. */
  reset(): void
}
