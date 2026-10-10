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
from typing import List, Literal, Optional, Sequence, Tuple, TypedDict, Union

StreamEventType = Literal[
    "chunkDropped", "stalled", "recovered", "permissionDenied", "permissionPending",
    "silenceWhileSourceActive", "deviceLost", "error", "terminalError",
    "recoverableError", "shutdownError", "audioLoss", "clipped", "permissionGranted", "unknown"
]
RecordingPermission = Literal["microphone", "systemAudio"]

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
class _AudioErrorFields(TypedDict):
    message: str
    contexts: List[ErrorContextDict]
    secondary: List["AudioErrorDict"]
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
class BackendErrorDict(_AudioErrorFields):
    kind: Literal["backend"]
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
    def advertised(self) -> Optional[NativeFormat]: ...
    @property
    def actual(self) -> Optional[NativeFormat]: ...
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
LossReason = Literal["rawOverflow", "mixFifoOverflow", "corruptBuffer", "malformedBuffer", "callbackRejected", "outputOverflow"]
class AudioLossDict(TypedDict):
    path: AudioPath
    reason: LossReason
    samples: Optional[int]
    sample_rate: int
    channels: int
class AudioLoss:
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
StreamEventDict = Union[ChunkDroppedEventDict, StalledEventDict, RecoveredEventDict, PermissionDeniedEventDict, PermissionPendingEventDict, PermissionGrantedEventDict, SilenceWhileSourceActiveEventDict, DeviceLostEventDict, LegacyErrorEventDict, TerminalErrorEventDict, RecoverableErrorEventDict, ShutdownErrorEventDict, AudioLossEventDict, ClippedEventDict, UnknownEventDict]

class DeviceInfoDict(TypedDict):
    id: str
    name: str
    source_kind: Literal["mic", "system", "process", "mix"]
    sample_rate: int
    channels: int
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
DeviceEventDict = Union[DeviceAddedEventDict, DeviceRemovedEventDict, DefaultChangedEventDict, DefaultClearedEventDict, RescanRequiredEventDict, UnknownEventDict]

class VadSettings(TypedDict, total=False):
    threshold: float
    neg_threshold: Optional[float]
    min_speech_ms: int
    min_silence_ms: int
    speech_pad_ms: int
    max_speech_ms: int
    sample_rate: int

class DeviceInfo:
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
    def is_loopback(self) -> bool: ...
    @property
    def is_default(self) -> bool: ...

class ProcessInfo:
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
    """Stream notification; permissionPending and silenceWhileSourceActive are advisories.

    permissionPending carries permission and an actionable message; capture
    continues and may remain silent until granted. permissionDenied is terminal
    and carries permission and a cause/remedy message. silenceWhileSourceActive
    means the system-audio diagnosis is inconclusive; capture continues because
    missing permission and genuine digital silence remain possible. error carries
    a safe message and is terminal. terminalError preserves the typed capture failure;
    recoverableError is advisory, while shutdownError reports cleanup failure.
    permissionGranted reports late microphone consent following Pending.

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
    def to_dict(self) -> DeviceEventDict: ...

class Stream:
    def stop(self) -> None: ...
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

        On DISCONTINUITY (flags & 1), flushed pre-gap events come first and
        retain the old at_sample clock. Fixed 20 ms chunks cannot complete a
        fresh 32 ms VAD frame: every event on that chunk is pre-gap, and events
        on later chunks use the new clock, restarted at zero. A timestamp
        decrease is not a reliable timeline marker. Flush errors still reset
        VAD and denoise before being raised; that poll consumes the chunk.
        After a successful reset, later polls do not repeat the flush error.
        """
        ...
    def poll_event(self) -> Optional[StreamEvent]: ...
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
    ) -> None: ...
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
    code: str
    terminal_events: List[WhisperVadEvent]

class WhisperVadRuntimeError(RuntimeError):
    code: str
    terminal_events: List[WhisperVadEvent]

class WhisperVad:
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

def devices() -> List[DeviceInfo]: ...
def processes() -> List[ProcessInfo]: ...
def watch_devices() -> DeviceWatcher: ...

def open(
    kind: str, *, device_id: Optional[str] = None,
    process_id: Optional[int] = None, mode: str = "include", exclude_self: bool = False,
    exclude_pids: Optional[Sequence[int]] = None, output_rate: int = 48000,
    output_channels: int = 2, chunk_ms: int = 20, gain: float = 1.0,
    mic_device_id: Optional[str] = None, system_device_id: Optional[str] = None,
    mic_gain: float = 1.0, system_gain: float = 1.0,
    vad: Optional[VadSettings] = None, denoise: bool = False,
    whisper_vad: Optional[WhisperVadStreamOptions] = None
) -> Stream: ...
