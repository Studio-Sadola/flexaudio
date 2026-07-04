# flexaudio (Python)

Python bindings for [flexaudio](https://github.com/Studio-Sadola/flexaudio), a
cross-platform audio capture library (microphone, system loopback, and
per-process loopback) written in Rust. Built with PyO3 and maturin.

## Install

```sh
pip install flexaudio
```

## Usage

```python
import flexaudio
import numpy as np

# List available devices (empty list on a headless machine, never raises).
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

### Sources

- `flexaudio.open("mic")` — microphone input.
- `flexaudio.open("system")` — full system output loopback. Pass
  `exclude_self=True` to drop this process's own playback.
- `flexaudio.open("process", process_id=<pid>)` — a single process's output.
  Pass `mode="exclude"` to capture everything *except* that process.

Optional keyword arguments: `device_id`, `output_rate` (default 48000),
`output_channels` (default 2), `chunk_ms` (default 20), plus the integrated
add-ons `vad` and `denoise` (see below).

`Stream.switch_source(...)` hot-swaps the input source without stopping the
stream. `pause()` / `resume()` / `is_paused()` control delivery.
`Stream.native_format()` returns the source's native `(sample_rate, channels)`
and `Stream.dropped_chunks()` returns the cumulative number of dropped chunks.

### Integrated denoise and VAD

`open()` (and `switch_source()`) accept `denoise=True` and `vad={...}` to run
noise suppression and voice-activity detection inside `poll_chunk()`. The
processing order is denoise -> VAD.

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

`samples` may be a Python `list`, an `array.array`, or a NumPy `ndarray`.

## License

MIT.

This binding statically links the following flexaudio add-ons, which embed
their models/tables and require no runtime files or network access:

- **flexaudio-vad** — uses [ONNX Runtime](https://github.com/microsoft/onnxruntime)
  (MIT) and the embedded [Silero VAD](https://github.com/snakers4/silero-vad)
  model (MIT).
- **flexaudio-encode** — uses [flacenc](https://github.com/yotarok/flacenc-rs)
  (Apache-2.0) for pure-Rust FLAC encoding.
- **flexaudio-denoise** — uses [nnnoiseless](https://github.com/jneem/nnnoiseless)
  (BSD-3-Clause), a Rust port of RNNoise with embedded model weights.
