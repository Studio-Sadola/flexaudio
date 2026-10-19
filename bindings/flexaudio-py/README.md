# flexaudio (Python)

Python bindings for [flexaudio](https://github.com/Studio-Sadola/flexaudio/blob/v0.8.1/README.md), a
cross-platform audio capture library (microphone, system loopback, and
per-process loopback) written in Rust. Built with PyO3 and maturin.

## Install

```sh
pip install "flexaudio>=0.8,<0.9"
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
are `None` when the OS does not expose them. `executable` is a basename, not a
path. On Linux it comes from the `/proc/<pid>/exe` target's basename, falling
back to `/proc/<pid>/comm` when `exe` is unreadable.

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

`denoise=True` enables RNNoise on the canonical 48 kHz stereo capture signal
before downmix and gain, feeding both output taps and attached VAD.
It requires 48000 Hz output and introduces 480 samples/channel of delay.
Graceful stop queues the denoiser tail and zero-pads the final output to a full
20 ms chunk (`flags` bit 8 when padding is added); continue polling to receive it.
Denoise runs before output polling and queue drops, continues processing while
paused, and keeps its history on resume. Source switches/reopens start fresh
history and normally discard the previous generation's buffered remainder.

Attach the same generic VAD engine with
`vad=flexaudio.StreamVadOptions(provisional=True)`. Each chunk's `vad_events`
contains ordered segments, optional provisional hints, and epoch markers.
The fixed 20 ms chunk size preserves canonical capture conversion and ordering;
other chunk durations are rejected before opening a device.

On macOS, `silence_probe=False` disables the silence diagnosis tone. The default
is enabled for system/process capture and the system lane of Mix. After five
continuous seconds of bit-exact zero native samples while an eligible external
captured process has active output (honoring exclusions and device selection),
it probes once per capture generation. A 1 kHz signal at amplitude 1e-5 plays for
roughly 300 ms to help distinguish permission failure from silence. This low-level
signal is not guaranteed inaudible and may enter capture when the host is included.

### Standalone add-ons

The same building blocks are available as independent classes.

```python
# Voice activity detection over PCM (mono/stereo, 8000–192000 Hz).
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
`process(samples, input_sample_rate=16000, input_channels=1)` accepts input rates
of 8000–192000 Hz and positive channel counts, and converts to mono16k internally. Event
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
Use `tap='secondary'` to deliver events on secondary chunks; this requires
`secondary_output`. The tap selects the event carrier; VAD still reads canonical
capture before conversion or integer encoding. The producer's shared 48 kHz stereo branch supplies valid PCM
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


### Settings reports, strict mode, and lifecycle

`flexaudio.open('mic', strict=True, ring_capacity_chunks=100)` selects strict
applicability validation and a 100-chunk ring (default 50).
`stream.effective_settings()` returns a dictionary containing `revision` and
`entries` with shared lower-camel setting/reason tags, requested/effective values,
and outcomes. It snapshots successful open/switch commits. Failed switches retain
the previous report and options; recovery does not increment the revision.
Effective values describe configuration, not proof of running hardware.

Only decoded values differing from documented defaults are requests. Explicit
defaults are `notRequested`, including on switch. Strict rejects all requested
ignored settings before device access, model loading, or stopping the old source,
raising `IgnoredSettingsError` with its complete candidate `.report`.
`strict=None` means false on open and retention on switch. Gain and ring capacity
stay fixed at open; non-default switch requests are ignored with `fixedAtOpen`.
Use `set_gain()` for runtime gain changes. Python switch denoise remains applied.
See the root README's [ignored-setting table](https://github.com/Studio-Sadola/flexaudio/blob/v0.8.1/README.md#strict-mode-and-effective-settings).

`stream.poll_lifecycle_event()` consumes a separate ordered helper-event FIFO;
`poll_event()` never returns lifecycle notifications, even as unknown events.
The FIFO remains drainable after stop, independently of ordinary event polling.
It retains at most 256 events, dropping the oldest on overflow. Started/Stopped
pairing is guaranteed only while the caller drains it frequently enough to avoid
overflow. Every emitted Started has exactly one Stopped with a reason. macOS
Started means entry into a diagnostic attempt; Windows Started means successful
keepalive render start. Stopped ends ownership after teardown attempts; typed cleanup
errors remain authoritative. There is no ordering guarantee relative to PCM or
ordinary events. See [helper behavior and controls](https://github.com/Studio-Sadola/flexaudio/blob/v0.8.1/README.md#optional-playback-helpers-and-lifecycle).

Both chunk types expose `arrival_ns`, exactly equal to `pts_ns`. The old chunk
name is documentation-only deprecated since 0.8 and retained throughout 0.9.
Use `frame_index` for audio continuity. `devices_with_namespaces()` returns
entries with `.device` and `.id_namespace`, preserving selectable IDs. Namespace
tags describe provider selector domains, not persistent hardware identities.

### Capability snapshot and optional output

`capabilities()` returns a dictionary with `os_version`, `sources`,
`system_exclusions`, `process`, `device_watcher`, and `side_effects`.
Keys use snake_case and tag values match the other bindings. Availability has
`status='supported'`, `status='unavailable'` with a `reason`, or
`status='unknown'` with the same structured error dictionary as
`error.audio_error.to_dict()`. The bounded probe releases the GIL and never
opens capture, requests recording consent, or plays audio. Support describes
platform prerequisites rather than permission or device availability.

On Linux, a missing PipeWire socket means unavailable. Failed socket inspection,
connection or server sync means unknown, with the structured query error retained.

`open(..., windows_classic_loopback_keepalive=True)` enables the Windows
classic loopback silent keepalive. Set it to `False` to disable it.
`switch_source(..., windows_classic_loopback_keepalive=None)` retains the
current setting; an explicit boolean replaces it only on successful switching.
The option applies to system capture and the system lane of Mix without PID
exclusions. Other platforms and process/exclusion capture ignore it. With it
disabled, idle classic loopback delivers no packets; the 2 s stall watchdog reports
`stalled` (`Event::StreamStalled` in Rust) and reopens repeatedly until audio plays.

```python
secondary = flexaudio.SecondaryOutputOptions(rate=16000, channels=1, encoding='s16')
with flexaudio.open('mic', secondary_output=secondary,
                    vad=flexaudio.StreamVadOptions(tap='secondary')) as stream:
    primary = stream.poll_chunk()
    secondary_chunk = stream.poll_secondary()
```

`poll_secondary()` returns `None` when disabled or empty. Its format is fixed at
open and retained across source switches. Encoding defaults to `f32`; `s16`
uses the shared signed 16-bit quantizer. Both encodings return little-endian
`bytes`; NumPy dtypes are `<f4` and `<i2`. `peak` and `rms` always describe the
float samples before integer quantization. Each chunk also exposes `encoding`,
`frames`, `frame_index`, `pts_ns`, `seq`, `flags`, `dropped_before`, and
`vad_events`. The two polls do not consume each other's PCM. Counters and drops
are independent; align taps by time rather than sequence number. Stop and VAD
flush retain queued output from both taps; drain both polls after stopping.
