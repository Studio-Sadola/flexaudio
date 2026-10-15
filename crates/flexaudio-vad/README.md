# flexaudio-vad

Offline voice activity detection with an embedded [Silero v6 model](https://github.com/snakers4/silero-vad)
and pure Rust [tract-onnx](https://github.com/snipsco/tract) inference. No model file or network access
is needed at runtime. The streaming segmenter is ported from whisper.cpp at commit `85a69493`.

```rust
use flexaudio_vad::{Vad, VadOptions, VadRuntimeOptions};

let mut vad = Vad::new(VadOptions::default(), VadRuntimeOptions::default())?;
let mut events = vad.process(&[0.0; 16_000])?;
events.extend(vad.finish()?);
# Ok::<(), Box<dyn std::error::Error>>(())
```

`process` accepts normalized 16 kHz mono f32 PCM. `process_pcm(samples, sample_rate, channels)`
accepts interleaved PCM at 8,000 through 192,000 Hz with any positive channel count. Channels are
averaged and resampled through the same DSP used by attached capture. Standalone conversion clips
finite resampler overshoot to [-1,1] before inference; nonfinite converter output fails the session
with a typed converter cause. Converter group delay is
removed before model framing: event times are milliseconds on the input timeline, starting at
zero. Source format is fixed within an epoch. Partial channel frames carry across calls and must
be complete at EOF; reset starts a new epoch and permits a new format.

`finish` drains the converter, infers one zero-padded partial model frame if needed, and closes
the epoch. Repeated finish returns no events. `reset` closes published hints, discards pending
final intervals, resets model/converter state, and starts a new epoch. Fatal failures carry
exactly-once terminal events in `VadFailure`; consume those events before reporting the cause.

Defaults are threshold 0.5, minimum speech 250 ms, minimum silence 100 ms, padding 30 ms,
unbounded maximum speech, and provisional hints disabled. Standalone and attached VAD share
these defaults. `max_speech_ms` is an integer. Zero or values above 100000000 use the effectively
unbounded legacy `INT_MAX/2`-sample sentinel (1073741823 samples). Other positive values must be
at least `32 + 2 * speech_pad_ms`. The exact raw split budget is
`max_speech_ms * 16 - 512 - 2 * speech_pad_ms * 16` samples. No seconds conversion or truncation
is applied. Zero split budgets are valid; negative budgets are rejected. Subsequent interval
merging can reunite splits, so this setting does not guarantee a
maximum final interval length or delivery latency. Final endpoints retain whisper.cpp's 10 ms
rounding behavior and may extend beyond physical EOF within the padded model frame.

`VadPostProcessor` accepts probabilities without inference. `last_frame_probabilities` exposes
the latest successful inference batch with its first frame index. `speech_segments` uses the
same process/finish path for whole-buffer segmentation. Optional provisional events provide
immediate speech hints and nonoverlapping pieces of at most 30 seconds without changing final
segmentation.

`VadTap` attaches this engine to canonical 48 kHz stereo capture and preserves epoch origins,
capture sample coordinates, and timestamps across gaps, flush, and stop. Construct and run VAD
on a serialized worker away from realtime audio callbacks. Capture uses fixed 20 ms chunks so
its conversion blocks and sample-clock mapping remain consistent with the 32 ms model frames.

## Install

```sh
cargo add flexaudio-vad
```

## License and third-party notices

[MIT](LICENSE), copyright 2026 tubome / Studio Sadola. The embedded model and adapted whisper.cpp
segmenter require the accompanying [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
