# flexaudio (Python)

Python bindings for [flexaudio](https://github.com/Studio-Sadola/flexaudio), a
cross-platform audio capture library (microphone, system loopback, and
per-process loopback) written in Rust. Built with PyO3 and maturin.

## Install

```sh
pip install "flexaudio>=0.5,<0.6"
```

## Usage

```python
import flexaudio
import numpy as np

# List a complete device inventory (incomplete or failed discovery raises typed exceptions).
for d in flexaudio.devices():
    print(d.id, d.name, d.source_kind, d.is_default)

# Open a microphone stream. open() starts capture before returning.
with flexaudio.open("mic") as stream:
    chunk = stream.poll_chunk()      # None if nothing is ready yet
    if chunk is not None:
        # data is interleaved little-endian f32 bytes.
        samples = np.frombuffer(chunk.data, dtype=np.float32)
        print(chunk.frames, chunk.peak, chunk.rms, samples.shape)

    event = stream.poll_event()      # None if no event is pending
    if event is not None:
        print(event.type, event.count, event.message)
# leaving the `with` block stops the stream
```

Incomplete or failed device discovery raises a typed exception, including when
Linux has no reachable PipeWire daemon. Watcher startup failure also raises.

### Sources

- `flexaudio.open("mic")` — microphone input.
- `flexaudio.open("system")` — full system output loopback. Pass
  `exclude_self=True` to drop this process's own playback.
- `flexaudio.open("process", process_id=<pid>)` — a single process's output.
  Pass `mode="exclude"` to capture everything *except* that process.

To find a `process_id`, list the processes that have an audio output
session/stream (the calling process is never listed). Idle/stopped processes
are included; whether something is playing now is `is_output_active`:

```python
for p in flexaudio.processes():
    print(p.pid, p.name, p.executable, p.bundle_id, p.is_output_active)
```

An empty list means per-process capture works but no such process exists right
now (not "nothing is playing"). `RuntimeError` means per-process capture is
unavailable here (Linux without a reachable PipeWire session, macOS before 14.4,
an unsupported OS, or a permission denial), the OS did not answer within 3
seconds, or a previous enumeration is still in progress. On Windows, listing
and capturing both need Windows build 20348 or later (Windows 11 / Windows
Server 2022). `executable`, `bundle_id` (macOS only), and `is_output_active`
are `None` when the OS does not expose them. On Linux, `executable` is
`/proc/<pid>/exe` and falls back to `/proc/<pid>/comm` when `exe` is
unreadable.

Optional keyword arguments: `device_id`, `output_rate` (default 48000),
`output_channels` (default 2), `chunk_ms` (only 20 is supported; other values
raise `InvalidArgumentError`), plus the integrated
add-ons `vad` and `denoise` (see below).

`Stream.switch_source(...)` hot-swaps the input source without stopping the
stream. `pause()` / `resume()` / `is_paused()` control delivery.
`stop()` spends the stream; open a new stream to capture again.
`Stream.native_format()` returns the source's native `(sample_rate, channels)`
and `Stream.dropped_chunks()` returns the cumulative number of dropped chunks.

### Excluding playback by PID

`open()` and `Stream.switch_source()` accept keyword-only `exclude_pids=None`.
Pass a list, tuple, or another integer sequence to exclude playback from system
capture or the system side of `"mix"`. It combines with `exclude_self=True`:

```python
with flexaudio.open("system", exclude_pids=[1234]) as stream:
    stream.switch_source("mix", exclude_pids=(1234,))
```

Every PID must be an integer in `1..=4294967295`; booleans, floats, and strings
raise `TypeError`. Out-of-range integers raise `ValueError` identifying the
entry's index, including integers larger than a native integer can hold.
Strings, bytes, dictionaries, sets, and generators are not accepted as the
sequence (`TypeError`). More than 4096 entries raises `ValueError`. `None` and
an empty sequence exclude no explicit PIDs. Order and duplicates are preserved.
Validation happens before any device access, including for `"mic"` and
`"process"`, which ignore valid exclusions.

Platform behavior:

- Windows supports one process tree root: when `exclude_self=True`, every
  listed PID must equal the calling process's PID; otherwise every listed PID
  must equal the first one. Distinct roots raise `ValueError`.
- macOS resolves PIDs once when capture starts and honors a selected system
  device together with exclusions.
- Linux matches exact PIDs, without excluding descendants. Linux and Windows
  ignore the selected system device while exclusion is active.

### Integrated denoise and VAD

`open()` (and `switch_source()`) accept `denoise=True` and `vad={...}` to run
noise suppression and voice-activity detection inside `poll_chunk()`. The
processing order is denoise -> VAD. `peak` and `rms` measure the final delivered
float PCM after denoise and gain, before integer encoding. Graceful `stop()`
queues the actual 480-frame (10 ms) denoiser tail and pending VAD boundaries;
continue polling after stop to receive them. Repeated stop does not repeat tails.
Capture terminal failure suppresses remaining PCM and audio tails.

```python
with flexaudio.open("mic", denoise=True, vad={"threshold": 0.5}) as stream:
    chunk = stream.poll_chunk()
    if chunk is not None:
        samples = np.frombuffer(chunk.data, dtype=np.float32)  # denoised audio
        for ev in chunk.vad_events:                            # speech boundaries
            print(ev.type, ev.at_sample)                       # "speech_start"/"speech_end"
```

`denoise` requires a 48000 Hz output (`denoise=True` with any other
`output_rate` raises `ValueError`); the first 480 samples/channel are silence
from the denoiser's fixed delay. `vad` is a dict with the same keys as the
standalone `Vad` (below). `at_sample` is measured at the VAD's internal rate
(16000 or 8000 Hz), not the input rate.

VAD emits `speech_start` and `speech_end` together when a segment is finalized,
not when speech first begins. On `DISCONTINUITY` (`chunk.flags & 1`), an open
pre-gap segment is flushed before the post-gap PCM is processed. Flushed events
come first and retain their pre-gap `at_sample` values; VAD then restarts its
sample clock at zero. These positions are separate from `chunk.pts_ns`.

Consumers distinguish timelines by the discontinuity chunk: the core always
delivers 20 ms chunks, which cannot complete a fresh 32 ms VAD inference frame.
Consequently **all events on the discontinuity chunk belong to the pre-gap
timeline**, and all events on subsequent chunks belong to the new timeline.
Handle that chunk's events under the previous timeline before advancing your
VAD timeline; the chunk's PCM itself already belongs to the post-gap timeline.
For example, a flushed pair at 0/512 on the discontinuity chunk is old; a pair
at 0/1024 delivered later is new. Do not infer the boundary from a decrease in
`at_sample`: delayed post-gap speech need not produce one. A mixed event list
from larger chunks would require an explicit flushed-event count or timeline
identifier; larger chunks are not supported by the current core.

If the discontinuity flush fails, VAD reset is still attempted and denoise is
still reset before `poll_chunk()` raises the flush error through its existing
exception path. That poll consumes the chunk. When reset succeeds, later
chunks process normally without repeating that flush error. If VAD reset
itself fails, its failure remains latched and subsequent processing reports
it until a reset succeeds.

### Standalone add-ons

The same building blocks are available as independent classes.

```python
# Voice activity detection over arbitrary-format PCM (mono/stereo, any rate).
vad = flexaudio.Vad(threshold=0.5, min_silence_ms=100)  # silero defaults
for ev in vad.process(samples, input_sample_rate=48000, input_channels=2):
    print(ev.type, ev.at_sample)

# Noise suppression (48 kHz, +/-1.0 normalized interleaved f32).
den = flexaudio.Denoiser(channels=1)
clean = den.process(samples)        # returns a same-length list
tail = den.flush()                  # trailing 480 samples/channel

# Streaming FLAC encoding, optionally rotating into rec-001.flac, rec-002.flac...
with flexaudio.FlacEncoder("rec.flac", sample_rate=48000, channels=2,
                           split_seconds=60) as enc:
    enc.write_chunk(samples)        # interleaved f32; finalize() runs on exit

# Device hot-plug monitoring (poll-based, like poll_chunk / poll_event).
with flexaudio.watch_devices() as watcher:
    ev = watcher.poll_event()       # None when nothing is pending
    if ev is not None:
        print(ev.type, ev.id, ev.device, ev.source_kind)
```

Linux watcher startup failure raises a typed exception; unsupported operating
systems retain the no-op watcher. `stop()` retains queued device events for draining.
`defaultCleared` means no default device exists. `rescanRequired` carries a
cumulative `dropped_events` count: discard the incremental inventory and call
`devices()` again. A failed query cannot make stale inventory authoritative.

### Typed failures and events

Core failures have distinct exception classes: `InvalidArgumentError` and
`UnsupportedFormatError` inherit `ValueError`; `InvalidStateError`,
`DeviceNotFoundError`, `RecordingPermissionError`, `UnsupportedOsVersionError`,
`DeviceLostError`, `BackendError`, `NativeFormatChangedError`, `UnsupportedError`,
and `AmbiguousDeviceNameError` inherit `RuntimeError`. Each exposes read-only
`audio_error`, including `kind`, a safe `message`, outer-to-inner `contexts`,
and ordered `secondary` failures. Native format changes retain `advertised`
and `actual` format records; native call/status details are explicit structured
access only. Existing Python argument extraction and addon exceptions keep their types.

`StreamEvent.to_dict()` and `DeviceEvent.to_dict()` produce closed variant records
with the same type tags as JavaScript and snake_case field names. Typed terminal,
recoverable, and shutdown events carry `error`; `audioLoss` carries `loss` with
path, reason, optional positive scalar-interleaved sample count, rate, and channels.
`None` counts mean unknown. `clipped` is a coalesced upstream Mix advisory without
exact chunk attribution; flags 8 (`PADDED`) and 16 (`CLIPPED`) instead describe
that delivered chunk. Padding alone does not imply a gap.

`permissionGranted` reports microphone consent once after `permissionPending`
in the same live capture generation. It remains advisory and does not guarantee
nonzero PCM. `error` and `terminalError` are terminal; `recoverableError` continues
capture. `terminal_error()` retains the typed capture primary. `shutdown_report()`
returns the final typed primary and ordered `cleanup_errors`, or `None` before
shutdown. Checked `stop()` raises any capture or cleanup failure, including on
repeat calls. During context exit an active body exception remains primary and
receives the cleanup failure as its exception context.

`samples` may be a Python `list`, an `array.array`, or a NumPy `ndarray`.

## License

MIT.

This binding statically links the following flexaudio add-ons, which embed
their models/tables and require no runtime files or network access:

- **flexaudio-vad** — uses pure-Rust [tract](https://github.com/snipsco/tract)
  (MIT OR Apache-2.0) and the embedded [Silero VAD](https://github.com/snakers4/silero-vad)
  model (MIT).
- **flexaudio-encode** — uses [flacenc](https://github.com/yotarok/flacenc-rs)
  (Apache-2.0) for pure-Rust FLAC encoding.
- **flexaudio-denoise** — uses [nnnoiseless](https://github.com/jneem/nnnoiseless)
  (BSD-3-Clause), a Rust port of RNNoise with embedded model weights.

### Whisper-compatible VAD

`WhisperVad` runs embedded Silero v6 on normalized mono float32 at 16 kHz, with
segmentation matching whisper.cpp pin `85a69493a601d4ff5a834064f7b7bac250bd8739`.
It accepts a sequence or a contiguous one-dimensional native float32 buffer;
NumPy is optional. Buffer input is borrowed for the call, and sequence input is
copied for that feed. Instances stay on their creating thread.

```python
import array
import flexaudio

vad = flexaudio.WhisperVad(provisional=True)
events = vad.process(memoryview(array.array('f', [0.0] * 513)))
events += vad.finish()
for event in events:
    if event.type == 'segment':
        print(event.start_ms, event.end_ms)
```

Parameters are `threshold=0.5`, `min_speech_duration_ms=250`,
`min_silence_duration_ms=100`, `max_speech_duration_s=3.4028234663852886e38`, and
`speech_pad_ms=30`. Supplied zero values stay zero. Durations must be integers
in 0..134217; bool durations, unknown keys, invalid PCM and nonfinite parameters
are rejected. The seconds budget is truncated by the pinned algorithm, and
200 ms merging can undo splits: it does **not** bound final segment duration or
latency. Optional provisional cuts independently cap pieces at 30000 ms.

Events are read-only variant objects with snake_case fields and type tags:
`segment`, `provisional_speech_start`, `provisional_speech_end`,
`provisional_cut`, and `epoch_end`. Times are epoch-relative integer milliseconds;
final endpoints use the 10 ms grid and can exceed physical EOF. `finish()`
infers one zero-padded partial frame when needed, closes the epoch, and is
idempotent. Call `reset()` to start another epoch; its returned closure events
must also be consumed. Processing after finish raises `WhisperVadRuntimeError`.
Typed errors carry `code` and `terminal_events`; drain those events even when
processing fails. `last_frame_probabilities()` returns the latest call's frame
index and an owned read-only float32 memoryview, independent of future calls.

`WhisperVadPostProcessor(WhisperVadParams(...))` accepts probabilities from the
16 kHz/512-sample frame geometry and delegates to the same segmentation kernel.
`whisper_speech_segments(samples, params=None)` is the single whole-PCM helper
and uses the same streaming process/finish path with preview disabled.

The additive capture option is `open(...,
whisper_vad=WhisperVadStreamOptions(params=..., provisional=True, tap='primary'))`.
It rejects simultaneous legacy `vad` with `ConflictingVad`, and secondary with
`UnsupportedTap`. The producer's shared 48 kHz stereo branch supplies valid PCM
and authoritative frame indices before output conversion. Attached chunks carry
`whisper_vad_events`; each epoch begins with its saved `capture_sample` and `pts_ns`
origin. `AudioChunk.frame_index` is a producer-owned 48 kHz timeline position.
`flush_whisper_vad()` and `stop()` queue ordered closing carriers even without new
PCM. Poll until the terminal carrier is consumed before releasing the stream.
Disabled attachment has `AudioChunk.whisper_vad_events=None` and flush is a no-op.
Legacy `Vad` and its integration keep their existing behavior.
