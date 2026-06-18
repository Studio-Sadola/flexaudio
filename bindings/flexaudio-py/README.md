# flexaudio (Python)

Python bindings for [flexaudio](https://github.com/tsubome/flexaudio), a
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
`output_channels` (default 2), `chunk_ms` (default 20).

`Stream.switch_source(...)` hot-swaps the input source without stopping the
stream. `pause()` / `resume()` / `is_paused()` control delivery.

## License

MIT
