"""Public API for the native flexaudio extension (Python 3.8 and later).

Argument extraction can raise TypeError or OverflowError. Core invalid arguments
and unsupported formats raise distinct ValueError subclasses; other core errors
raise distinct RuntimeError subclasses. Their read-only audio_error preserves
root kinds, contexts, related errors and native format records.
VAD construction raises ValueError for invalid settings and RuntimeError for
model/inference failures. Denoiser construction/process raise ValueError for
invalid channel counts/sample lengths. FLAC operations raise ValueError for
unsupported parameters, OSError for I/O failures, and RuntimeError for encoder
failures or writes after finalization.

open/switch_source validate exclude_pids before addons/device access: non-integer
entries (including bool) or non-sequences raise TypeError; PIDs outside
1..=4294967295 or more than 4096 entries raise ValueError. Integrated denoise
supports any output rate through canonical 48 kHz processing. Invalid integrated VAD field types propagate
PyO3 extraction errors. processes raises RuntimeError when unavailable, timed
out, or already in progress. Context exits return False and preserve the block's
exception; FlacEncoder propagates finalization errors only on normal exit.
"""

from os import PathLike
from typing import List, Literal, Optional, Sequence, Tuple, TypedDict, Union

StreamEventType = Literal["startupSamplesDropped",
    "chunkDropped", "stalled", "recovered", "permissionDenied", "permissionPending",
    "silenceWhileSourceActive", "deviceLost", "error", "terminalError",
    "recoverableError", "shutdownError", "audioLoss", "clipped", "unknown"
]
RecordingPermission = Literal["microphone", "systemAudio"]
VadErrorCode = Literal["InvalidParameter", "InvalidProbability", "InvalidPcm", "Overflow", "SessionFinished", "Inference", "ModelLoad", "FailedSession", "InvalidStereoLength", "CaptureSampleOverflow", "PtsOutOfRange", "UnsupportedTap", "UnsupportedConversionClock", "Conversion", "Stopped"]

MixLane = Literal["microphone", "systemAudio"]

SettingKey = Literal[
    "kind", "deviceId", "targetPid", "mode", "chunkMs", "ringCapacityChunks",
    "excludeSelf", "excludePids", "output", "secondaryOutput", "secondaryEncoding",
    "gain", "mixMicDeviceId", "mixSystemDeviceId", "mixMicGain", "mixSystemGain",
    "silenceProbe", "windowsClassicLoopbackKeepalive", "denoise", "vad"
]
IgnoreReason = Literal["wrongSourceKind", "unsupportedPlatform", "exclusionDeviceUnscoped", "processLoopbackPath", "fixedAtOpen"]
AdjustmentReason = Literal["exclusionSetNormalized", "legacySentinel"]
SettingValue = Union[None, bool, int, float, str, List[int], Tuple[int, int]]
class SettingOutcome(TypedDict):
    status: Literal["notRequested", "applied", "ignored", "adjusted"]
    reason: Optional[Union[IgnoreReason, AdjustmentReason]]
class SettingResolution(TypedDict):
    setting: SettingKey
    requested: SettingValue
    effective: SettingValue
    outcome: SettingOutcome
class SettingsReport(TypedDict):
    revision: int
    entries: List[SettingResolution]
class IgnoredSettingsError(ValueError):
    @property
    def report(self) -> SettingsReport: ...

HelperStopReason = Literal[
    "signalObserved", "permissionDenied", "inconclusive", "timedOut",
    "streamStopped", "sourceSwitched", "recovery", "startupFailed",
    "runtimeFailed", "cancelled", "unknown"
]
class LifecycleEvent:
    @property
    def type(self) -> Literal["silenceProbeStarted", "silenceProbeStopped", "classicLoopbackKeepaliveStarted", "classicLoopbackKeepaliveStopped", "unknown"]: ...
    @property
    def reason(self) -> Optional[HelperStopReason]: ...
    @property
    def message(self) -> Optional[str]: ...

DeviceIdNamespace = Literal["cpalDeviceName", "pipeWireNodeName", "wasapiOutputSelector", "coreAudioOutputSelector", "unknown"]
class NamespacedDevice:
    @property
    def device(self) -> "DeviceInfo": ...
    @property
    def id_namespace(self) -> DeviceIdNamespace: ...

def devices_with_namespaces() -> List[NamespacedDevice]: ...

class NativeFormatDict(TypedDict):
    sample_rate: int
    channels: int
class HResultDict(TypedDict):
    type: Literal["hresult"]
    call: str
    bits: int
class OsStatusDict(TypedDict):
    type: Literal["osStatus"]
    call: str
    value: int
NativeStatus = Union[HResultDict, OsStatusDict]
class ErrorContextDict(TypedDict):
    operation: Literal["enumerate", "start", "normalize", "flush", "reopen", "rollback", "stop", "join", "link"]
    lane: Optional[MixLane]
    native_status: Optional[NativeStatus]
AddonErrorKind = Literal["vadModelLoad", "vadInference", "vadInvalidConfig", "vadInvalidFormat", "vadResample", "vadReset", "flacIo", "flacUnsupported", "flacEncoder"]
class _AudioErrorFields(TypedDict):
    message: str
    contexts: List[ErrorContextDict]
    secondary: List["AudioErrorDict"]
    diagnostic_details: List[str]
    addon_kind: Optional[AddonErrorKind]
    addon_message: Optional[str]
class InvalidArgumentErrorDict(_AudioErrorFields):
    kind: Literal["invalidArg"]
class InvalidStateErrorDict(_AudioErrorFields):
    kind: Literal["invalidState"]
class DeviceNotFoundErrorDict(_AudioErrorFields):
    kind: Literal["deviceNotFound"]
class RecordingPermissionErrorDict(_AudioErrorFields):
    kind: Literal["permissionDenied"]
    permission: RecordingPermission
class UnsupportedOsVersionErrorDict(_AudioErrorFields):
    kind: Literal["unsupportedOsVersion"]
class DeviceLostErrorDict(_AudioErrorFields):
    kind: Literal["deviceLost"]
class _BackendErrorRequiredFields(_AudioErrorFields):
    kind: Literal["backend"]
class BackendErrorDict(_BackendErrorRequiredFields, total=False):
    vad_code: VadErrorCode
class UnsupportedFormatErrorDict(_AudioErrorFields):
    kind: Literal["unsupportedFormat"]
class NativeFormatChangedErrorDict(_AudioErrorFields):
    kind: Literal["nativeFormatChanged"]
    advertised: NativeFormatDict
    actual: NativeFormatDict
class UnsupportedErrorDict(_AudioErrorFields):
    kind: Literal["unsupported"]
class AmbiguousDeviceNameErrorDict(_AudioErrorFields):
    kind: Literal["ambiguousDeviceName"]
class IgnoredSettingsErrorDict(_AudioErrorFields):
    kind: Literal["ignoredSettings"]
AudioErrorDict = Union[InvalidArgumentErrorDict, InvalidStateErrorDict, DeviceNotFoundErrorDict, RecordingPermissionErrorDict, UnsupportedOsVersionErrorDict, DeviceLostErrorDict, BackendErrorDict, UnsupportedFormatErrorDict, NativeFormatChangedErrorDict, UnsupportedErrorDict, AmbiguousDeviceNameErrorDict, IgnoredSettingsErrorDict]

CapabilityBackend = Literal["cpal", "pipeWire", "wasapiLoopback", "wasapiProcessLoopback", "coreAudioProcessTap", "composite", "unsupported", "other"]
PidScope = Literal["exactPid", "processTree", "notApplicable", "other"]
class OsVersionDict(TypedDict):
    major: int
    minor: int
    patch: Optional[int]
    build: Optional[int]
class SimpleUnavailableReason(TypedDict):
    kind: Literal["unsupportedOs", "pipeWireUnavailable", "notApplicable"]
class OsVersionTooOldReason(TypedDict):
    kind: Literal["osVersionTooOld"]
    required: OsVersionDict
    actual: OsVersionDict
class OtherUnavailableReason(TypedDict):
    kind: Literal["other"]
    code: str
UnavailableReason = Union[SimpleUnavailableReason, OsVersionTooOldReason, OtherUnavailableReason]
class SupportedAvailability(TypedDict):
    status: Literal["supported"]
class UnavailableAvailability(TypedDict):
    status: Literal["unavailable"]
    reason: UnavailableReason
class UnknownAvailability(TypedDict):
    status: Literal["unknown"]
    error: AudioErrorDict
Availability = Union[SupportedAvailability, UnavailableAvailability, UnknownAvailability]
class SourceCapabilityDict(TypedDict):
    backend: CapabilityBackend
    availability: Availability
class SourceCapabilitiesDict(TypedDict):
    mic: SourceCapabilityDict
    system: SourceCapabilityDict
    process: SourceCapabilityDict
    mix: SourceCapabilityDict
class SystemExclusionCapabilitiesDict(TypedDict):
    availability: Availability
    backend: CapabilityBackend
    scope: PidScope
    max_distinct_pids: Optional[int]
    device_selection: Literal["honored", "ignored", "notApplicable", "other"]
class ProcessCapabilitiesDict(TypedDict):
    include_scope: PidScope
    exclude_scope: PidScope
class DeviceWatcherCapabilitiesDict(TypedDict):
    availability: Availability
    mode: Literal["native", "noop", "other"]
    scopes: List[Literal["systemOutputs", "defaultSystemOutput", "other"]]
class SideEffectCapabilityDict(TypedDict):
    availability: Availability
    enabled_by_default: bool
    sources: List[Literal["mic", "system", "process", "mix", "other"]]
    condition: Literal["silentActiveSystemAudio", "classicLoopbackWithoutExclusions", "other"]
class SideEffectCapabilitiesDict(TypedDict):
    silence_probe: SideEffectCapabilityDict
    classic_loopback_keepalive: SideEffectCapabilityDict
class Capabilities(TypedDict):
    os_version: Optional[OsVersionDict]
    sources: SourceCapabilitiesDict
    system_exclusions: SystemExclusionCapabilitiesDict
    process: ProcessCapabilitiesDict
    device_watcher: DeviceWatcherCapabilitiesDict
    side_effects: SideEffectCapabilitiesDict

def capabilities() -> Capabilities:
    """Probe backend support without capture or consent; releases the GIL.

    Supported describes prerequisites, not recording permission or device availability.
    Query failures retain structured error details under availability.error.
    """
    ...

class NativeFormat:
    @property
    def sample_rate(self) -> int: ...
    @property
    def channels(self) -> int: ...
    def to_dict(self) -> NativeFormatDict: ...
class ErrorContext:
    @property
    def operation(self) -> Literal["enumerate", "start", "normalize", "flush", "reopen", "rollback", "stop", "join", "link"]: ...
    @property
    def lane(self) -> Optional[MixLane]: ...
    @property
    def native_status(self) -> Optional[NativeStatus]: ...
    def to_dict(self) -> ErrorContextDict: ...
class AudioError:
    @property
    def kind(self) -> Literal["invalidArg", "invalidState", "deviceNotFound", "permissionDenied", "unsupportedOsVersion", "deviceLost", "backend", "unsupportedFormat", "nativeFormatChanged", "unsupported", "ambiguousDeviceName", "ignoredSettings"]: ...
    @property
    def message(self) -> str: ...
    @property
    def contexts(self) -> List[ErrorContext]: ...
    @property
    def secondary(self) -> List[AudioError]: ...
    @property
    def permission(self) -> Optional[RecordingPermission]: ...
    @property
    def vad_code(self) -> Optional[VadErrorCode]: ...
    @property
    def advertised(self) -> Optional[NativeFormat]: ...
    @property
    def actual(self) -> Optional[NativeFormat]: ...
    @property
    def diagnostic_details(self) -> List[str]: ...
    @property
    def addon_kind(self) -> Optional[AddonErrorKind]: ...
    @property
    def addon_message(self) -> Optional[str]: ...
    def to_dict(self) -> AudioErrorDict: ...
class InvalidArgumentError(ValueError):
    @property
    def audio_error(self) -> AudioError: ...
class UnsupportedFormatError(ValueError):
    @property
    def audio_error(self) -> AudioError: ...
class InvalidStateError(RuntimeError):
    @property
    def audio_error(self) -> AudioError: ...
class DeviceNotFoundError(RuntimeError):
    @property
    def audio_error(self) -> AudioError: ...
class RecordingPermissionError(RuntimeError):
    @property
    def audio_error(self) -> AudioError: ...
class UnsupportedOsVersionError(RuntimeError):
    @property
    def audio_error(self) -> AudioError: ...
class DeviceLostError(RuntimeError):
    @property
    def audio_error(self) -> AudioError: ...
class BackendError(RuntimeError):
    @property
    def audio_error(self) -> AudioError: ...
class NativeFormatChangedError(RuntimeError):
    @property
    def audio_error(self) -> AudioError: ...
class UnsupportedError(RuntimeError):
    @property
    def audio_error(self) -> AudioError: ...
class AmbiguousDeviceNameError(RuntimeError):
    @property
    def audio_error(self) -> AudioError: ...

class CapturePathDict(TypedDict):
    type: Literal["capture"]
    lane: Optional[MixLane]
class MixFifoPathDict(TypedDict):
    type: Literal["mixFifo"]
    lane: MixLane
class OutputPathDict(TypedDict):
    type: Literal["output"]
    tap: Literal["primary", "secondary"]
AudioPath = Union[CapturePathDict, MixFifoPathDict, OutputPathDict]
LossReason = Literal["rawOverflow", "mixFifoOverflow", "corruptBuffer", "malformedBuffer", "callbackRejected", "outputOverflow", "generationEnded"]
class AudioLossDict(TypedDict):
    path: AudioPath
    reason: LossReason
    samples: Optional[int]
    sample_rate: int
    channels: int
class AudioLoss:
    """Discarded audio with a count at the reported rate and channel count.

    generationEnded reports a held valid native startup prefix discarded when
    capture fails terminally. samples counts scalar interleaved native samples.
    """
    @property
    def path(self) -> AudioPath: ...
    @property
    def reason(self) -> LossReason: ...
    @property
    def samples(self) -> Optional[int]: ...
    @property
    def sample_rate(self) -> int: ...
    @property
    def channels(self) -> int: ...
    def to_dict(self) -> AudioLossDict: ...
class ShutdownReportDict(TypedDict):
    primary: Optional[AudioErrorDict]
    cleanup_errors: List[AudioErrorDict]
class ShutdownReport:
    @property
    def primary(self) -> Optional[AudioError]: ...
    @property
    def cleanup_errors(self) -> List[AudioError]: ...
    def to_dict(self) -> ShutdownReportDict: ...

class ChunkDroppedEventDict(TypedDict):
    type: Literal["chunkDropped"]
    count: int
class StalledEventDict(TypedDict):
    type: Literal["stalled"]
class RecoveredEventDict(TypedDict):
    type: Literal["recovered"]
class PermissionDeniedEventDict(TypedDict):
    type: Literal["permissionDenied"]
    permission: RecordingPermission
    message: str
class PermissionPendingEventDict(TypedDict):
    type: Literal["permissionPending"]
    permission: RecordingPermission
    message: str

class SilenceWhileSourceActiveEventDict(TypedDict):
    type: Literal["silenceWhileSourceActive"]
    message: str
class DeviceLostEventDict(TypedDict):
    type: Literal["deviceLost"]
class LegacyErrorEventDict(TypedDict):
    type: Literal["error"]
    message: str
class TerminalErrorEventDict(TypedDict):
    type: Literal["terminalError"]
    error: AudioErrorDict
class RecoverableErrorEventDict(TypedDict):
    type: Literal["recoverableError"]
    error: AudioErrorDict
class ShutdownErrorEventDict(TypedDict):
    type: Literal["shutdownError"]
    error: AudioErrorDict
class AudioLossEventDict(TypedDict):
    type: Literal["audioLoss"]
    loss: AudioLossDict
class ClippedEventDict(TypedDict):
    type: Literal["clipped"]
class UnknownEventDict(TypedDict):
    type: Literal["unknown"]
    message: str
class StartupSamplesDroppedEventDict(TypedDict):
    type: Literal["startupSamplesDropped"]
    frames: int
    reason: Literal["outOfRange"]

StreamEventDict = Union[StartupSamplesDroppedEventDict, ChunkDroppedEventDict, StalledEventDict, RecoveredEventDict, PermissionDeniedEventDict, PermissionPendingEventDict, SilenceWhileSourceActiveEventDict, DeviceLostEventDict, LegacyErrorEventDict, TerminalErrorEventDict, RecoverableErrorEventDict, ShutdownErrorEventDict, AudioLossEventDict, ClippedEventDict, UnknownEventDict]

class DeviceInfoDict(TypedDict):
    """Unknown display names are empty. Windows/macOS use native IDs if names are unreadable.
    IDs may change between queries: a native fallback may become the recovered name.
    A name-based ID cannot be opened while its name is unreadable; the native ID stays openable
    in both name states. macOS unreadable output direction is omitted from discovery;
    the typed direction cause is returned for the requested device.
    Unreadable default queries leave is_default=False.
    Linux default-output opens skip the inventory/format pre-check; PipeWire resolves the sink
    and negotiates format.
    """
    id: str
    name: str
    source_kind: Literal["mic", "system", "process", "mix"]
    sample_rate: int
    channels: int
    format_available: bool
    is_loopback: bool
    is_default: bool
class DeviceAddedEventDict(TypedDict):
    type: Literal["added"]
    device: DeviceInfoDict
class DeviceRemovedEventDict(TypedDict):
    type: Literal["removed"]
    id: str
class DefaultChangedEventDict(TypedDict):
    type: Literal["defaultChanged"]
    source_kind: Literal["mic", "system"]
    id: str
class DefaultClearedEventDict(TypedDict):
    type: Literal["defaultCleared"]
    source_kind: Literal["mic", "system"]
class RescanRequiredEventDict(TypedDict):
    type: Literal["rescanRequired"]
    dropped_events: int
class DeviceTerminalErrorDict(TypedDict):
    type: Literal["terminalError"]
    error: AudioErrorDict
DeviceEventDict = Union[DeviceTerminalErrorDict, DeviceAddedEventDict, DeviceRemovedEventDict, DefaultChangedEventDict, DefaultClearedEventDict, RescanRequiredEventDict, UnknownEventDict]

class DeviceInfo:
    """Unreadable mandatory formats use sample_rate=0, channels=0, format_available=False.
    Unknown display names are empty. Windows/macOS use native IDs if names are unreadable.
    IDs may change between queries: a native fallback may become the recovered name.
    A name-based ID cannot be opened while its name is unreadable; the native ID stays openable
    in both name states. macOS unreadable output direction is omitted from discovery;
    the typed direction cause is returned for the requested device.
    Unreadable default queries leave is_default=False.
    Linux default-output opens skip the inventory/format pre-check; PipeWire resolves the sink
    and negotiates format.
    """
    def __repr__(self) -> str: ...
    @property
    def id(self) -> str: ...
    @property
    def name(self) -> str: ...
    @property
    def source_kind(self) -> str: ...
    @property
    def sample_rate(self) -> int: ...
    @property
    def channels(self) -> int: ...
    @property
    def format_available(self) -> bool: ...
    @property
    def is_loopback(self) -> bool: ...
    @property
    def is_default(self) -> bool: ...

class ProcessInfo:
    """Unreadable optional metadata stays None; unknown display names use pid N.
    Windows 87/0x80070057 is vanished; 5/0x80070005 and other image failures mean unknown.
    macOS !obj/0x216f626a, ESRCH=3 and ENOENT=2 are vanished; other metadata failures mean unknown.
    """
    def __repr__(self) -> str: ...
    @property
    def pid(self) -> int: ...
    @property
    def name(self) -> str: ...
    @property
    def executable(self) -> Optional[str]: ...
    @property
    def bundle_id(self) -> Optional[str]: ...
    @property
    def is_output_active(self) -> Optional[bool]: ...

class AudioChunk:
    """pts_ns is an arrival-time label assigned at facade ingest and clamped non-decreasing.

    Callback jitter may cause overlapping ranges, repeated labels or gaps; this is not
    a media clock. frame_index is authoritative (gaps are loss). Exact VAD mapping uses
    epoch_start.pts_ns + t_ms*1_000_000, or epoch_start.capture_sample + t_ms*48.
    """
    @property
    def frame_index(self) -> int: ...
    def __repr__(self) -> str: ...
    @property
    def data(self) -> bytes: ...
    @property
    def vad_events(self) -> Optional[List[AttachedVadEvent]]: ...
    @property
    def frames(self) -> int: ...
    @property
    def pts_ns(self) -> int: ...
    @property
    def arrival_ns(self) -> int:
        """Arrival-time label, exactly equal to pts_ns; frame_index is authoritative.

        pts_ns is deprecated in 0.8 and retained throughout 0.9.
        """
        ...
    @property
    def seq(self) -> int: ...
    @property
    def flags(self) -> int: ...
    @property
    def dropped_before(self) -> int: ...
    @property
    def peak(self) -> float: ...
    @property
    def rms(self) -> float: ...

class StreamEvent:
    """Stream notification; silenceWhileSourceActive is advisory.

    permissionPending is advisory and remains available for custom Rust backends.
    Built-in sources do not emit it. permissionDenied is terminal
    and carries permission and a cause/remedy message. silenceWhileSourceActive
    means the system-audio diagnosis is inconclusive; capture continues because
    missing permission and genuine digital silence remain possible. error carries
    a safe message and is terminal. terminalError preserves the typed capture failure;
    recoverableError is advisory, while shutdownError reports cleanup failure.
    startupSamplesDropped is enqueued chronologically before publishing the
    generation's first chunk. Event-first callers drain the ordinary FIFO event
    queue before PCM to observe the notice first. PCM polling does not wait for event polling.
    Arrival gaps without reopen emit no event/DISCONTINUITY: frame_index stays
    contiguous, primary and secondary arrival labels can jump, and attached Vad keeps its epoch open.
    Stall: >=2 s without consumed samples, checked every 250 ms; stalled, reopen,
    each failed reopen attempt emits recoverableError and backs off 250 ms..5 s.
    On success, recovered is emitted when new PCM arrives. First new-generation primary
    data has RECOVERED|DISCONTINUITY. Vad epoch_end(finish) and a new epoch_start
    wait for new PCM. Downtime does not become frames. These advisories are unordered
    relative to PCM and epoch events.
    macOS microphone capture requires decided consent before each start; it does
    not continuously monitor authorization changes or permission revocation.

    On macOS, system/process capture, including the system lane of Mix, can confirm
    SystemAudio denial with a once-per-generation self-probe after five continuous
    seconds of exact zeros and eligible external output activity. Its low-level signal
    is not guaranteed inaudible. Its separate private tap captures our own diagnostic output; this signal may
    also enter user capture if our process is included. An inconclusive probe is
    advisory only. Stopping cancels the probe without a late event.
    """
    def __repr__(self) -> str: ...
    @property
    def type(self) -> StreamEventType: ...
    @property
    def permission(self) -> Optional[RecordingPermission]: ...
    @property
    def count(self) -> Optional[int]: ...
    @property
    def frames(self) -> Optional[int]: ...
    @property
    def reason(self) -> Optional[Literal["outOfRange"]]: ...
    @property
    def message(self) -> Optional[str]: ...
    @property
    def error(self) -> Optional[AudioError]: ...
    @property
    def loss(self) -> Optional[AudioLoss]: ...
    def to_dict(self) -> StreamEventDict: ...

class DeviceEvent:
    def __repr__(self) -> str: ...
    @property
    def type(self) -> str: ...
    @property
    def device(self) -> Optional[DeviceInfo]: ...
    @property
    def id(self) -> Optional[str]: ...
    @property
    def source_kind(self) -> Optional[Literal["mic", "system"]]: ...
    @property
    def dropped_events(self) -> Optional[int]: ...
    @property
    def message(self) -> Optional[str]: ...
    @property
    def error(self) -> Optional[AudioError]: ...
    def to_dict(self) -> DeviceEventDict: ...

class SecondaryOutputOptions:
    def __init__(self, *, rate: int, channels: int, encoding: Literal["f32", "s16"] = "f32") -> None: ...
    @property
    def rate(self) -> int: ...
    @property
    def channels(self) -> int: ...
    @property
    def encoding(self) -> Literal["f32", "s16"]: ...

class SecondaryChunk:
    """Independent tap counters; little-endian PCM and float meters before encoding.

    pts_ns is deprecated in 0.8; arrival_ns is identical. Retained throughout 0.9.
    frame_index is authoritative.
    """
    @property
    def data(self) -> bytes: ...
    @property
    def encoding(self) -> Literal["f32", "s16"]: ...
    @property
    def frame_index(self) -> int: ...
    @property
    def frames(self) -> int: ...
    @property
    def pts_ns(self) -> int: ...
    @property
    def arrival_ns(self) -> int: ...
    @property
    def seq(self) -> int: ...
    @property
    def flags(self) -> int: ...
    @property
    def dropped_before(self) -> int: ...
    @property
    def peak(self) -> float: ...
    @property
    def rms(self) -> float: ...
    @property
    def vad_events(self) -> Optional[List[AttachedVadEvent]]: ...

class Stream:
    def effective_settings(self) -> SettingsReport: ...
    def poll_lifecycle_event(self) -> Optional[LifecycleEvent]:
        """Consume only helper lifecycle notifications, including after stop.

        The FIFO retains at most 256 events, dropping the oldest on overflow.
        Started/Stopped pairing is guaranteed only while the caller drains it
        frequently enough to avoid overflow.
        """
        ...
    def stop(self) -> None:
        """Stop once and retain the outcome; open a new stream to capture again."""
        ...
    def shutdown_report(self) -> Optional[ShutdownReport]: ...
    def flush_vad(self) -> None: ...
    def pause(self) -> None: ...
    def resume(self) -> None: ...
    def is_paused(self) -> bool: ...
    def set_gain(self, gain: float) -> None: ...
    def gain(self) -> float: ...
    def native_format(self) -> Tuple[int, int]: ...
    def dropped_chunks(self) -> int: ...
    def poll_chunk(self) -> Optional[AudioChunk]:
        """Poll the next primary audio chunk, or None when none is queued."""
        ...
    def poll_secondary(self) -> Optional[SecondaryChunk]: ...
    def poll_event(self) -> Optional[StreamEvent]:
        """Poll the ordinary FIFO event queue; PCM polling remains independent.

        A startup notice is enqueued before the generation's first chunk is
        published, so event-first callers can observe it before polling PCM.
        """
        ...
    def terminal_error(self) -> Optional[AudioError]:
        """Retained terminal failure, including after stop; does not consume events.

        poll_chunk, resume and switch_source raise RuntimeError once terminal.
        A failed stream cannot be restarted; resolve the cause and open a new one.
        """
        ...
    def switch_source(
        self, kind: str, *, device_id: Optional[str] = None,
        process_id: Optional[int] = None, mode: str = "include", exclude_self: bool = False,
        exclude_pids: Optional[Sequence[int]] = None, output_rate: int = 48000,
        output_channels: int = 2, chunk_ms: int = 20, gain: float = 1.0,
        mic_device_id: Optional[str] = None, system_device_id: Optional[str] = None,
        mic_gain: float = 1.0, system_gain: float = 1.0,
        denoise: Optional[bool] = None, silence_probe: bool = True,
        windows_classic_loopback_keepalive: Optional[bool] = None,
        strict: Optional[bool] = None,
        ring_capacity_chunks: Optional[int] = None
    ) -> None:
        """Every successful switch creates a generation with DISCONTINUITY; epoch
        transition waits for new PCM. Return means backend replaced, not delivered.
        Old epoch_end need not precede new primary PCM; use its DISCONTINUITY boundary.
        The VAD attachment configured with vad at open() persists across switches;
        new PCM starts a new VAD epoch. Omitting denoise or passing None retains
        the current setting; explicit True/False enables/disables noise suppression.
        strict=None retains the current strictness; strict=False allows ignored settings.
        """
        ...
    def __enter__(self) -> Stream: ...
    def __exit__(self, _exc_type: Optional[object], _exc_value: Optional[object], _traceback: Optional[object]) -> bool: ...

VadCloseReason = Literal["hysteresis", "finish", "reset", "error"]
VadCutReason = Literal["limit", "hysteresis", "finish", "reset", "error"]

class VadOptions:
    def __init__(self, *, threshold: float = 0.5,
                 min_speech_ms: int = 250, min_silence_ms: int = 100,
                 max_speech_ms: int = 0,
                 speech_pad_ms: int = 30) -> None: ...
    @property
    def threshold(self) -> float: ...
    @property
    def min_speech_ms(self) -> int: ...
    @property
    def min_silence_ms(self) -> int: ...
    @property
    def max_speech_ms(self) -> int: ...
    @property
    def speech_pad_ms(self) -> int: ...

class SpeechSegment:
    @property
    def start_ms(self) -> int: ...
    @property
    def end_ms(self) -> int: ...

class FrameProbabilities:
    @property
    def first_frame_index(self) -> int: ...
    @property
    def values(self) -> memoryview: ...

class SegmentEvent:
    @property
    def type(self) -> Literal["segment"]: ...
    @property
    def epoch(self) -> int: ...
    @property
    def seq(self) -> int: ...
    @property
    def start_ms(self) -> int: ...
    @property
    def end_ms(self) -> int: ...

class ProvisionalSpeechStartEvent:
    @property
    def type(self) -> Literal["provisional_speech_start"]: ...
    @property
    def epoch(self) -> int: ...
    @property
    def seq(self) -> int: ...
    @property
    def at_ms(self) -> int: ...

class ProvisionalSpeechEndEvent:
    @property
    def type(self) -> Literal["provisional_speech_end"]: ...
    @property
    def epoch(self) -> int: ...
    @property
    def seq(self) -> int: ...
    @property
    def at_ms(self) -> int: ...
    @property
    def reason(self) -> VadCloseReason: ...

class ProvisionalCutEvent:
    @property
    def type(self) -> Literal["provisional_cut"]: ...
    @property
    def epoch(self) -> int: ...
    @property
    def seq(self) -> int: ...
    @property
    def start_ms(self) -> int: ...
    @property
    def end_ms(self) -> int: ...
    @property
    def reason(self) -> VadCutReason: ...

class EpochEndEvent:
    @property
    def type(self) -> Literal["epoch_end"]: ...
    @property
    def epoch(self) -> int: ...
    @property
    def seq(self) -> int: ...
    @property
    def reason(self) -> Literal["finish", "reset", "error"]: ...

class EpochStartEvent:
    """capture_sample counts 48 kHz frames on the dedicated capture timeline,
    independent of primary/secondary format and tap. capture_sample + t_ms*48 is exact.
    Equal to primary frame_index for the same sample only when primary is 48 kHz.
    Other primary rates share units but not the origin (resampler group delay).
    Unreconciled startup-release padding can grow the offset by up to one primary chunk per boundary.
    Neither index is an offset into concatenated delivered PCM; pause/queue drops advance them.
    Attached epochs strictly increase within a stream (checked epoch+1 on rebuild).
    New open starts at 0; (epoch, seq) is unique per stream.
    """
    @property
    def type(self) -> Literal["epoch_start"]: ...
    @property
    def epoch(self) -> int: ...
    @property
    def seq(self) -> int: ...
    @property
    def capture_sample(self) -> int: ...
    @property
    def pts_ns(self) -> int: ...

VadEvent = Union[SegmentEvent, ProvisionalSpeechStartEvent,
                        ProvisionalSpeechEndEvent, ProvisionalCutEvent, EpochEndEvent]
AttachedVadEvent = Union[VadEvent, EpochStartEvent]

class VadValidationError(ValueError):
    audio_error: AudioError
    code: str
    terminal_events: List[VadEvent]

class VadRuntimeError(RuntimeError):
    audio_error: AudioError
    code: str
    terminal_events: List[VadEvent]

class Vad:
    """Accepts float sequences or contiguous 1-D native float32 buffers.

    Input defaults to mono16k. Rates of 8000-192000 Hz and positive channel counts are converted internally;
    event milliseconds use the input timeline with converter group delay compensated.
    After raw input validation, finite resampler overshoot is clipped internally to [-1, 1].
    max_speech_ms=0 or values above 100000000 use the effectively unbounded
    INT_MAX/2-sample sentinel (1073741823 samples). Other positive values must be
    at least 32+2*speech_pad_ms; zero split budgets are valid, negative ones rejected.
    Buffer input is copied into owned storage for Python 3.8 stable-ABI support.
    """
    def __init__(self, threshold: float = 0.5, min_speech_ms: int = 250,
                 min_silence_ms: int = 100,
                 max_speech_ms: int = 0,
                 speech_pad_ms: int = 30, provisional: bool = False) -> None: ...
    def process(self, samples: Union[Sequence[float], memoryview], input_sample_rate: int = 16000, input_channels: int = 1) -> List[VadEvent]: ...
    def finish(self) -> List[VadEvent]: ...
    def reset(self) -> List[VadEvent]: ...
    def last_frame_probabilities(self) -> FrameProbabilities: ...

class VadPostProcessor:
    def __init__(self, params: Optional[VadOptions] = None) -> None: ...
    def process(self, probabilities: Union[Sequence[float], memoryview]) -> List[SpeechSegment]: ...
    def finish(self) -> List[SpeechSegment]: ...
    def reset(self) -> None: ...

class StreamVadOptions:
    def __init__(self, *, params: Optional[VadOptions] = None,
                 provisional: bool = False, tap: Literal["primary", "secondary"] = "primary") -> None: ...
    @property
    def params(self) -> VadOptions: ...
    @property
    def provisional(self) -> bool: ...
    @property
    def tap(self) -> Literal["primary", "secondary"]: ...

def speech_segments(samples: Union[Sequence[float], memoryview],
                            params: Optional[VadOptions] = None) -> List[SpeechSegment]: ...


class Denoiser:
    def __init__(self, channels: int) -> None: ...
    def process(self, samples: Sequence[float]) -> List[float]: ...
    def flush(self) -> List[float]: ...
    def reset(self) -> None: ...
    def channels(self) -> int: ...

class FlacEncoder:
    def __init__(self, path: Union[str, bytes, PathLike[str], PathLike[bytes]], sample_rate: int, channels: int, split_seconds: int = 0) -> None: ...
    def write_chunk(self, samples: Sequence[float]) -> None: ...
    def finalize(self) -> None: ...
    def __enter__(self) -> FlacEncoder: ...
    def __exit__(self, exc_type: Optional[object], _exc_value: Optional[object], _traceback: Optional[object]) -> bool: ...

class DeviceWatcher:
    def poll_event(self) -> Optional[DeviceEvent]: ...
    def stop(self) -> None: ...
    def __enter__(self) -> DeviceWatcher: ...
    def __exit__(self, _exc_type: Optional[object], _exc_value: Optional[object], _traceback: Optional[object]) -> bool: ...

def devices() -> List[DeviceInfo]:
    """Collection/count failure raises a typed exception; per-entry metadata stays unknown.

    On macOS, enumeration never prompts. With undecided microphone consent it
    raises BackendError before any HAL/device/format query. Grant the
    responsible host access or use the bounded capture-open consent flow, then retry.
    Denied/restricted consent raises RecordingPermissionError. Unreadable mandatory formats
    use format_available=False and 0/0; other entries stay usable. Collection/count
    failures raise; entries without readable selectable identity may be omitted.
    Windows enumeration does not preflight microphone permission; native enumeration
    proceeds. Microphone capture retains its separate existing permission preflight.
    """
    ...
def processes() -> List[ProcessInfo]: ...
def watch_devices() -> DeviceWatcher:
    """Watch Linux PipeWire system output additions, removals and default changes.

    Output IDs match devices(). Microphone and default-microphone changes are not
    reported: CPAL microphone IDs cannot be resolved from PipeWire nodes.
    Windows/macOS use a no-op watcher. Linux startup failure raises a typed exception.
    """
    ...

def open(
    kind: str, *, device_id: Optional[str] = None,
    process_id: Optional[int] = None, mode: str = "include", exclude_self: bool = False,
    exclude_pids: Optional[Sequence[int]] = None, output_rate: int = 48000,
    output_channels: int = 2, chunk_ms: int = 20, gain: float = 1.0,
    mic_device_id: Optional[str] = None, system_device_id: Optional[str] = None,
    mic_gain: float = 1.0, system_gain: float = 1.0,
    denoise: bool = False, silence_probe: bool = True,
    vad: Optional[StreamVadOptions] = None,
    windows_classic_loopback_keepalive: bool = True,
    secondary_output: Optional[SecondaryOutputOptions] = None,
    strict: Optional[bool] = None,
    ring_capacity_chunks: Optional[int] = None
) -> Stream:
    """Open capture synchronously; macOS microphone consent must be decided.

    strict=None uses strict mode by default, rejecting ignored non-default settings
    with IgnoredSettingsError. Set strict=False to allow ignored settings.

    Bundled prompts wait at most 30 seconds; an unanswered prompt raises a typed
    backend error without starting capture. Answer the prompt and retry. Built-in
    backends do not emit permissionPending; custom Rust backends may emit it.
    The 30-second bound covers undecided consent, not authorized native queries.
    PCM polling does not wait for event polling. To observe startup notices
    first, drain the ordinary FIFO event queue before polling PCM.
    """
    ...
