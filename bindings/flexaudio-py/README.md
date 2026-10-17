# flexaudio (Python)

Python bindings for [flexaudio](https://github.com/Studio-Sadola/flexaudio), a
cross-platform audio capture library (microphone, system loopback, and
per-process loopback) written in Rust. Built with PyO3 and maturin.

## Install

```sh
pip install "flexaudio>=0.6,<0.7"
```

## Usage

```python
import flexaudio
import numpy as np
import time

def drain_events(stream):
    while True:
        event = stream.poll_event()
        if event is None:
            return
        print(event.type)

# List a complete device inventory (incomplete or failed discovery raises typed exceptions).
for d in flexaudio.devices():
    print(d.id, d.name, d.source_kind, d.is_default)

# Open a microphone stream. open() starts capture before returning.
with flexaudio.open("mic") as stream:
    for _ in range(100):
        drain_events(stream)         # Event-first callers observe startup notices before PCM.
        chunk = stream.poll_chunk()  # None if nothing is ready yet
        if chunk is None:
            time.sleep(0.005)
            continue
        # data is interleaved little-endian f32 bytes.
        samples = np.frombuffer(chunk.data, dtype=np.float32)
        print(chunk.frames, chunk.peak, chunk.rms, samples.shape)

    stream.stop()
    while True:
        drain_events(stream)         # Drain notices before final PCM too.
        chunk = stream.poll_chunk()
        if chunk is None:
            break
        print("final frames", chunk.frames)
# leaving the `with` block reuses the completed stop outcome
```

Incomplete or failed device discovery raises a typed exception, including when
Linux has no reachable PipeWire daemon. Watcher startup failure also raises.
PCM polling does not wait for event polling, including after stop. Startup notices
are enqueued chronologically before publishing their generation's first chunk.
Event-first callers can drain the ordinary FIFO event queue before PCM to observe
the notice first; PCM-only callers can omit event polling.

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

### Noise suppression and attached VAD

`denoise=True` enables RNNoise. Without VAD it processes delivered PCM during
polling; with VAD it processes the shared capture branch before VAD and output
conversion. It requires 48000 Hz output and introduces 480 samples/channel of delay.
Graceful stop queues the denoiser tail; continue polling to receive final output.

Attach the same generic VAD engine with
`vad=flexaudio.StreamVadOptions(provisional=True)`. Each chunk's `vad_events`
contains ordered segments, optional provisional hints, and epoch markers.
The fixed 20 ms chunk size preserves canonical capture conversion and ordering;
other chunk durations are rejected before opening a device.

On macOS, `silence_probe=False` disables the silence diagnosis tone. The default
is enabled: after five seconds of exact-zero system capture, a 1 kHz tone at
amplitude 1e-5 plays for 300 ms to help distinguish permission failure from silence.

### Standalone add-ons

The same building blocks are available as independent classes.

```python
# Voice activity detection over arbitrary-format PCM (mono/stereo, any rate).
vad = flexaudio.Vad(threshold=0.5, min_silence_ms=100)
for ev in vad.process(samples, input_sample_rate=48000, input_channels=2):
    print(ev.type)

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

On Linux, `watch_devices()` reports PipeWire system output additions, removals and
default changes using the same output IDs as `devices()`. Microphone and
default-microphone changes are not reported: CPAL microphone IDs cannot be resolved
from PipeWire nodes. Windows/macOS use a no-op watcher. Linux watcher startup
failure raises a typed exception. `stop()` retains queued device events for draining.
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

`permissionPending` remains available for custom Rust backends; built-in sources
do not emit it. From 0.5.3, macOS microphone capture requires decided consent
before each start and does not continuously monitor authorization or revocation.
`error` and `terminalError` are terminal; `recoverableError` continues
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

### Generic VAD

`Vad` runs embedded Silero v6 on normalized float32, with
segmentation matching whisper.cpp pin `85a69493a601d4ff5a834064f7b7bac250bd8739`.
`process(samples, input_sample_rate=16000, input_channels=1)` accepts arbitrary
positive input rates/channel counts and converts to mono16k internally. Event
times use the input timeline, compensated for converter group delay. After raw
input validation, finite resampler overshoot is clipped internally to [-1, 1]. It accepts
a sequence or a contiguous one-dimensional native float32 buffer;
NumPy is optional. Buffer input is copied through Python bytes into owned Rust
float32 storage for Python 3.8 stable-ABI compatibility, and sequence input is
copied for that feed. Instances stay on their creating thread.

```python
import array
import flexaudio

vad = flexaudio.Vad(provisional=True)
events = vad.process(memoryview(array.array('f', [0.0] * 513)))
events += vad.finish()
for event in events:
    if event.type == 'segment':
        print(event.start_ms, event.end_ms)
```

Parameters are `threshold=0.5`, `min_speech_ms=250`,
`min_silence_ms=100`, `max_speech_ms=0`, and
`speech_pad_ms=30`. Supplied zero values stay zero. Durations must be integers
in 0..134217; bool durations, unknown keys, invalid PCM and nonfinite parameters
are rejected. `max_speech_ms` is integer milliseconds; 0 or values above 100000000 use the
effectively unbounded legacy `INT_MAX/2`-sample sentinel (1073741823 samples). Other positive
values below one inference frame plus twice the padding budget are rejected; equality
(a zero split budget) is valid. Fixed
200 ms merging can undo splits: it does **not** bound final segment duration or
latency. Optional provisional cuts independently cap pieces at 30000 ms.

Events are read-only variant objects with snake_case fields and type tags:
`segment`, `provisional_speech_start`, `provisional_speech_end`,
`provisional_cut`, and `epoch_end`. Times are epoch-relative integer milliseconds;
final endpoints use the 10 ms grid and can exceed physical EOF. `finish()`
infers one zero-padded partial frame when needed, closes the epoch, and is
idempotent. Call `reset()` to start another epoch; its returned closure events
must also be consumed. Processing after finish raises `VadRuntimeError`.
Typed errors carry `code` and `terminal_events`; drain those events even when
processing fails. `last_frame_probabilities()` returns the latest call's frame
index and an owned read-only float32 memoryview, independent of future calls.

`VadPostProcessor(VadOptions(...))` accepts probabilities from the
16 kHz/512-sample frame geometry and delegates to the same segmentation kernel.
`speech_segments(samples, params=None)` is the single whole-PCM helper
and uses the same streaming process/finish path with preview disabled.

The capture option is `open(...,
vad=StreamVadOptions(params=..., provisional=True, tap='primary'))`.
Secondary taps are rejected with `UnsupportedTap`. The producer's shared 48 kHz stereo branch supplies valid PCM
and authoritative frame indices before output conversion. Attached chunks carry
`vad_events`; each epoch begins with its saved `capture_sample` and `pts_ns`
origin. `AudioChunk.frame_index` is a producer-owned 48 kHz timeline position.
`flush_vad()` and `stop()` queue ordered closing carriers even without new
PCM. Poll until the terminal carrier is consumed before releasing the stream.
An attached VAD failure is a failure of that session only: capture and PCM
delivery continue, and polling delivers its closing events without raising the
processor failure. `stop()` succeeds for a VAD-only failure; its shutdown report
retains the cause in `cleanup_errors` with `vad_code`, and `primary` remains
capture-only. The next discontinuity (or a successful `flush_vad()`)
reinitializes the session so its next frame starts a new anchored epoch.
Disabled attachment has `AudioChunk.vad_events=None` and flush is a no-op.
