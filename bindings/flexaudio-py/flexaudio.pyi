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
requires 48000 Hz (ValueError). Invalid integrated VAD field types propagate
PyO3 extraction errors. processes raises RuntimeError when unavailable, timed
out, or already in progress. Context exits return False and preserve the block's
exception; FlacEncoder propagates finalization errors only on normal exit.
"""

from os import PathLike
from typing import List, Literal, Optional, Protocol, Sequence, Tuple, TypedDict, Union

StreamEventType = Literal["startupSamplesDropped",
    "chunkDropped", "stalled", "recovered", "permissionDenied", "permissionPending",
    "silenceWhileSourceActive", "deviceLost", "error", "terminalError",
    "recoverableError", "shutdownError", "audioLoss", "clipped", "permissionGranted", "unknown"
]
RecordingPermission = Literal["microphone", "systemAudio"]
WhisperCode = Literal["InvalidParameter", "InvalidProbability", "InvalidPcm", "Overflow", "SessionFinished", "Inference", "ModelLoad", "FailedSession", "InvalidStereoLength", "CaptureSampleOverflow", "PtsOutOfRange", "UnsupportedConversionClock", "Conversion", "Stopped"]

MixLane = Literal["microphone", "systemAudio"]

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
    whisper_code: WhisperCode
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
AudioErrorDict = Union[InvalidArgumentErrorDict, InvalidStateErrorDict, DeviceNotFoundErrorDict, RecordingPermissionErrorDict, UnsupportedOsVersionErrorDict, DeviceLostErrorDict, BackendErrorDict, UnsupportedFormatErrorDict, NativeFormatChangedErrorDict, UnsupportedErrorDict, AmbiguousDeviceNameErrorDict]

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
    def kind(self) -> Literal["invalidArg", "invalidState", "deviceNotFound", "permissionDenied", "unsupportedOsVersion", "deviceLost", "backend", "unsupportedFormat", "nativeFormatChanged", "unsupported", "ambiguousDeviceName"]: ...
    @property
    def message(self) -> str: ...
    @property
    def contexts(self) -> List[ErrorContext]: ...
    @property
    def secondary(self) -> List[AudioError]: ...
    @property
    def permission(self) -> Optional[RecordingPermission]: ...
    @property
    def whisper_code(self) -> Optional[WhisperCode]: ...
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
class PermissionGrantedEventDict(TypedDict):
    type: Literal["permissionGranted"]
    permission: Literal["microphone"]
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

StreamEventDict = Union[StartupSamplesDroppedEventDict, ChunkDroppedEventDict, StalledEventDict, RecoveredEventDict, PermissionDeniedEventDict, PermissionPendingEventDict, PermissionGrantedEventDict, SilenceWhileSourceActiveEventDict, DeviceLostEventDict, LegacyErrorEventDict, TerminalErrorEventDict, RecoverableErrorEventDict, ShutdownErrorEventDict, AudioLossEventDict, ClippedEventDict, UnknownEventDict]

class DeviceInfoDict(TypedDict):
    """Unknown display names are empty. Windows/macOS use native IDs if names are unreadable.
    macOS unreadable output direction is omitted; unreadable default queries leave is_default=False.
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

# Structural fields on a typed exception raised specifically by standalone Vad.flush.
# This typing helper does not represent an instantiable native class.
class _VadFlushErrorFields(Protocol):
    audio_error: AudioError
    pending_events: List["VadEvent"]

class VadSettings(TypedDict, total=False):
    threshold: float
    neg_threshold: Optional[float]
    min_speech_ms: int
    min_silence_ms: int
    speech_pad_ms: int
    max_speech_ms: int
    sample_rate: int

class DeviceInfo:
    """Unreadable mandatory formats use sample_rate=0, channels=0, format_available=False."""
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

class VadEvent:
    def __repr__(self) -> str: ...
    @property
    def type(self) -> str: ...
    @property
    def at_sample(self) -> int: ...

class AudioChunk:
    """pts_ns is an arrival-time label re-anchored on every native callback.

    Callback jitter may overlap chunks or leave gaps; no non-overlap or bounded-monotonic
    guarantee. frame_index is authoritative (gaps are loss). Exact VAD mapping uses
    epoch_start.pts_ns + t_ms*1_000_000, or epoch_start.capture_sample + t_ms*48.
    """
    @property
    def frame_index(self) -> int: ...
    def __repr__(self) -> str: ...
    @property
    def data(self) -> bytes: ...
    @property
    def vad_events(self) -> List[VadEvent]: ...
    @property
    def whisper_vad_events(self) -> Optional[List[AttachedWhisperVadEvent]]: ...
    @property
    def frames(self) -> int: ...
    @property
    def pts_ns(self) -> int: ...
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

    Built-in backends emit neither permissionPending nor permissionGranted on any
    OS or capture source. Their event arms remain for custom Rust backends and
    compatibility. A custom permissionPending is advisory; a custom
    permissionGranted can report late microphone consent. permissionDenied is terminal
    and carries permission and a cause/remedy message. silenceWhileSourceActive
    means the system-audio diagnosis is inconclusive; capture continues because
    missing permission and genuine digital silence remain possible. error carries
    a safe message and is terminal. terminalError preserves the typed capture failure;
    recoverableError is advisory, while shutdownError reports cleanup failure.
    startupSamplesDropped is enqueued chronologically before publishing the
    generation's first chunk. Event-first callers drain the ordinary FIFO event
    queue before PCM to observe the notice first. PCM polling does not wait for event polling.
    Arrival gaps without reopen emit no event/DISCONTINUITY: frame_index stays
    contiguous, only primary pts_ns jumps, and attached WhisperVad keeps its epoch open.
    Stall: >=2 s without consumed samples, checked every 250 ms; stalled, reopen,
    each failed reopen attempt emits recoverableError and backs off 250 ms..5 s.
    On success, recovered is emitted when new PCM arrives. First new-generation primary
    data has RECOVERED|DISCONTINUITY. WhisperVad epoch_end(finish) and a new epoch_start
    wait for new PCM. Downtime does not become frames. These advisories are unordered
    relative to PCM and epoch events.
    macOS microphone capture requires decided consent before each start; it does
    not continuously monitor authorization changes or permission revocation.

    On macOS, system/process capture can confirm SystemAudio denial with an active
    self-probe after sustained exact zeros and eligible external output activity.
    Its separate private tap captures our own diagnostic output; this signal may
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

class Stream:
    def stop(self) -> None:
        """Stop once and retain the outcome; open a new stream to capture again."""
        ...
    def shutdown_report(self) -> Optional[ShutdownReport]: ...
    def flush_whisper_vad(self) -> None: ...
    def pause(self) -> None: ...
    def resume(self) -> None: ...
    def is_paused(self) -> bool: ...
    def set_gain(self, gain: float) -> None: ...
    def gain(self) -> float: ...
    def native_format(self) -> Tuple[int, int]: ...
    def dropped_chunks(self) -> int: ...
    def poll_chunk(self) -> Optional[AudioChunk]:
        """Poll PCM and finalized VAD boundary pairs (start and end together).

        PCM polling does not wait for event polling. Event-first callers can
        drain poll_event before PCM to observe startup notices already enqueued
        before the affected generation's first chunk was published.

        On DISCONTINUITY (flags & 1), flushed pre-gap events come first and
        retain the old at_sample clock. Fixed 20 ms chunks cannot complete a
        fresh 32 ms VAD frame: every event on that chunk is pre-gap, and events
        on later chunks use the new clock, restarted at zero. A timestamp
        decrease is not a reliable timeline marker. Flush errors still reset
        VAD and denoise before being raised; that poll consumes the chunk.
        After a successful reset, later polls do not repeat the flush error.
        """
        ...
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
        vad: Optional[VadSettings] = None, denoise: bool = False
    ) -> None:
        """Every successful switch creates a generation with DISCONTINUITY; epoch
        transition waits for new PCM. Return means backend replaced, not delivered.
        Old epoch_end need not precede new primary PCM; use its DISCONTINUITY boundary.
        """
        ...
    def __enter__(self) -> Stream: ...
    def __exit__(self, _exc_type: Optional[object], _exc_value: Optional[object], _traceback: Optional[object]) -> bool: ...

class Vad:
    def __init__(self, threshold: float = 0.5, min_speech_ms: int = 250,
                 min_silence_ms: int = 100, speech_pad_ms: int = 30,
                 max_speech_ms: int = 0, sample_rate: int = 16000,
                 neg_threshold: Optional[float] = None) -> None: ...
    def process(self, samples: Sequence[float], input_sample_rate: int, input_channels: int) -> List[VadEvent]: ...
    def flush(self) -> List[VadEvent]: ...
    def reset(self) -> None: ...


WhisperCloseReason = Literal["hysteresis", "finish", "reset", "error"]
WhisperCutReason = Literal["limit", "hysteresis", "finish", "reset", "error"]

class WhisperVadParams:
    def __init__(self, *, threshold: float = 0.5,
                 min_speech_duration_ms: int = 250, min_silence_duration_ms: int = 100,
                 max_speech_duration_s: float = 3.4028234663852886e38,
                 speech_pad_ms: int = 30) -> None: ...
    @property
    def threshold(self) -> float: ...
    @property
    def min_speech_duration_ms(self) -> int: ...
    @property
    def min_silence_duration_ms(self) -> int: ...
    @property
    def max_speech_duration_s(self) -> float: ...
    @property
    def speech_pad_ms(self) -> int: ...

class WhisperSpeechSegment:
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
    def reason(self) -> WhisperCloseReason: ...

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
    def reason(self) -> WhisperCutReason: ...

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

WhisperVadEvent = Union[SegmentEvent, ProvisionalSpeechStartEvent,
                        ProvisionalSpeechEndEvent, ProvisionalCutEvent, EpochEndEvent]
AttachedWhisperVadEvent = Union[WhisperVadEvent, EpochStartEvent]

class WhisperVadValidationError(ValueError):
    audio_error: AudioError
    code: str
    terminal_events: List[WhisperVadEvent]

class WhisperVadRuntimeError(RuntimeError):
    audio_error: AudioError
    code: str
    terminal_events: List[WhisperVadEvent]

class WhisperVad:
    """Accepts float sequences or contiguous 1-D native float32 buffers.

    Buffer input is copied into owned storage for Python 3.8 stable-ABI support.
    """
    def __init__(self, threshold: float = 0.5, min_speech_duration_ms: int = 250,
                 min_silence_duration_ms: int = 100,
                 max_speech_duration_s: float = 3.4028234663852886e38,
                 speech_pad_ms: int = 30, provisional: bool = False) -> None: ...
    def process(self, samples: Union[Sequence[float], memoryview]) -> List[WhisperVadEvent]: ...
    def finish(self) -> List[WhisperVadEvent]: ...
    def reset(self) -> List[WhisperVadEvent]: ...
    def last_frame_probabilities(self) -> FrameProbabilities: ...

class WhisperVadPostProcessor:
    def __init__(self, params: Optional[WhisperVadParams] = None) -> None: ...
    def process(self, probabilities: Union[Sequence[float], memoryview]) -> List[WhisperSpeechSegment]: ...
    def finish(self) -> List[WhisperSpeechSegment]: ...
    def reset(self) -> None: ...

class WhisperVadStreamOptions:
    def __init__(self, *, params: Optional[WhisperVadParams] = None,
                 provisional: bool = False, tap: Literal["primary", "secondary"] = "primary") -> None: ...
    @property
    def params(self) -> WhisperVadParams: ...
    @property
    def provisional(self) -> bool: ...
    @property
    def tap(self) -> Literal["primary"]: ...

def whisper_speech_segments(samples: Union[Sequence[float], memoryview],
                            params: Optional[WhisperVadParams] = None) -> List[WhisperSpeechSegment]: ...


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
    raises a typed Backend error before any HAL/device/format query. Grant the
    responsible host access or use the bounded capture-open consent flow, then retry.
    Denied/restricted consent raises PermissionDenied. Unreadable mandatory formats
    use format_available=False and 0/0; other entries stay usable. Collection/count
    failures raise; entries without readable selectable identity may be omitted.
    Windows enumeration does not preflight microphone permission; native enumeration
    proceeds. Microphone capture retains its separate existing permission preflight.
    """
    ...
def processes() -> List[ProcessInfo]: ...
def watch_devices() -> DeviceWatcher:
    """Start watching; Linux startup failure raises a typed exception."""
    ...

def open(
    kind: str, *, device_id: Optional[str] = None,
    process_id: Optional[int] = None, mode: str = "include", exclude_self: bool = False,
    exclude_pids: Optional[Sequence[int]] = None, output_rate: int = 48000,
    output_channels: int = 2, chunk_ms: int = 20, gain: float = 1.0,
    mic_device_id: Optional[str] = None, system_device_id: Optional[str] = None,
    mic_gain: float = 1.0, system_gain: float = 1.0,
    vad: Optional[VadSettings] = None, denoise: bool = False,
    whisper_vad: Optional[WhisperVadStreamOptions] = None
) -> Stream:
    """Open capture synchronously; macOS microphone consent must be decided.

    Bundled prompts wait at most 30 seconds; an unanswered prompt raises a typed
    backend error without starting capture. Answer the prompt and retry. Built-in
    backends emit neither permissionPending nor permissionGranted on any OS/source.
    The 30-second bound covers undecided consent, not authorized native queries.
    PCM polling does not wait for event polling. To observe startup notices
    first, drain the ordinary FIFO event queue before polling PCM.
    """
    ...
