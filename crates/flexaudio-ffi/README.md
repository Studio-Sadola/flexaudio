# flexaudio-ffi

**C ABI bindings** for using flexaudio from C (pull-based). The third way for a C application
to call flexaudio in-process (the first is the CLI pipe, the second is the N-API addon).
The caller periodically calls `flexaudio_poll_chunk` / `flexaudio_poll_event` to take out
chunks and events.

In addition to capture itself, three addons are exposed to C:

- **VAD** (speech segment detection, silero VAD / ONNX) — `flexaudio_vad_*`
- **FLAC** (streaming lossless compression of recorded chunks, with rotation) — `flexaudio_flac_*`
- **denoise** (noise suppression with RNNoise) — `flexaudio_denoise_*`

VAD / denoise can also be built into a stream (`has_vad` / `denoise` in `FlexConfig`).
Device hotplug watching is available through the `flexaudio_watch_devices` family.
Candidate targets for per-process capture (processes that have an audio output
session / stream, excluding the own process; stopped and Idle ones are included) are
enumerated with `flexaudio_processes` and freed **exactly once** with
`flexaudio_processes_free` (each element is a `FlexProcessInfo`; pass `pid` to
`FlexConfig::process_id`; `executable` / `bundle_id` are NULL if they could not be obtained,
and `output_activity` is `FLEX_OUTPUT_ACTIVITY_UNKNOWN|INACTIVE|ACTIVE`).
0 entries is success (usable, but there are no candidates right now). It returns
`FLEX_FAILURE` in an environment where per-process capture itself is not usable
(PipeWire unreachable on Linux, macOS older than 14.4, Windows that is not Windows build 20348
or later (Windows 11 / Windows Server 2022), unsupported OS, permission denied), when the OS
did not respond within 3 seconds, or when a previous query has not finished yet.

## Building and generating the header

Both a `staticlib` (`.a`) and a `cdylib` (`.so` / `.dll` / `.dylib`) are built.

```sh
cargo build -p flexaudio-ffi --release
# Regenerate the header (always do this after changing the ABI)
cbindgen --config cbindgen.toml --crate flexaudio-ffi --output include/flexaudio.h
```

Link the build output together with `include/flexaudio.h`. No function lets a panic unwind
across the FFI boundary (`catch_unwind`); on failure it returns a negative code and leaves a
message in `flexaudio_last_error()`. Allocations handed to C (`FlexChunk::data` / VAD event
arrays / device strings, etc.) must always be freed by flexaudio through the matching
free function (do not use C's `free`).

## Basic capture

```c
#include "flexaudio.h"
#include <stdio.h>

int main(void) {
    FlexConfig cfg = {0};              // all 0 = defaults (mic / 48k / stereo / 20ms)
    cfg.kind = FLEX_SOURCE_KIND_MIC;

    FlexStream *s = flexaudio_open(&cfg);
    if (!s) {
        fprintf(stderr, "open failed: %s\n", flexaudio_last_error());
        return 1;
    }
    if (flexaudio_start(s) != FLEX_OK) {
        fprintf(stderr, "start failed: %s\n", flexaudio_last_error());
        flexaudio_free(s);
        return 1;
    }

    FlexChunk chunk;
    for (int i = 0; i < 100; i++) {
        int r = flexaudio_poll_chunk(s, &chunk);
        if (r < 0) break;              // error (flexaudio_last_error)
        if (r == 0) continue;          // none right now (wait a bit and retry)
        // chunk.data is interleaved f32 (chunk.len elements = frames * channels)
        printf("frames=%u peak=%.3f\n", chunk.frames, chunk.peak);
        flexaudio_chunk_free(&chunk);  // free data
    }

    flexaudio_free(s);
    return 0;
}
```

## Building denoise / VAD into the stream

When `denoise` / `has_vad` are set, each chunk is passed through **denoise → VAD** in that
order right before `flexaudio_poll_chunk` returns it. denoise assumes 48kHz output
(`flexaudio_open` returns NULL if `output_rate` is anything other than 48000). Events finalized
by the VAD go into `FlexChunk::vad_events`, and `flexaudio_chunk_free` frees them together
with `data`.

```c
FlexConfig cfg = {0};
cfg.kind = FLEX_SOURCE_KIND_MIC;
cfg.denoise = true;      // assumes 48k output (output_rate=0 is 48000)
cfg.has_vad = true;      // cfg.vad all 0 = silero defaults (threshold 0.5, etc.)

FlexStream *s = flexaudio_open(&cfg);
/* ... start / poll ... */
if (flexaudio_poll_chunk(s, &chunk) == 1) {
    for (size_t i = 0; i < chunk.vad_events_len; i++) {
        FlexVadEvent ev = chunk.vad_events[i];   // kind: 0=start / 1=end
        printf("%s @ %lld\n", ev.kind == 0 ? "speech-start" : "speech-end",
               (long long)ev.at_sample);
    }
    flexaudio_chunk_free(&chunk);    // frees both data and vad_events
}
```

## Standalone handles (use without a stream)

### VAD

You can feed in f32 samples of any format you have at hand (internally downmixed to mono and
resampled to the VAD rate). Free the event array with `flexaudio_vad_events_free`.

```c
FlexVad *vad = flexaudio_vad_new(NULL);        // NULL = default settings
FlexVadEvent *events = NULL;
size_t n = 0;
// samples: 48k/stereo interleaved f32 (len elements)
if (flexaudio_vad_process(vad, samples, len, 48000, 2, &events, &n) == FLEX_OK) {
    for (size_t i = 0; i < n; i++) { /* events[i].kind / at_sample */ }
    flexaudio_vad_events_free(events, n);
}
flexaudio_vad_free(vad);
```

### FLAC

Streams interleaved f32 (flexaudio's canonical 48k/stereo can be passed as is) through
lossless compression. If `split_seconds` is 1 or more, it rotates to numbered files
`rec-001.flac`, `rec-002.flac`, … every that many seconds (0 means a single file).

```c
// Single file
FlexFlac *flac = flexaudio_flac_create("rec.flac", 48000, 2, 0);
flexaudio_flac_write(flac, samples, len);       // append as many times as needed
flexaudio_flac_finalize(flac);                  // finalize the header (no writes afterwards)
flexaudio_flac_free(flac);

// Split every 5 minutes → rec-001.flac, rec-002.flac, ...
FlexFlac *split = flexaudio_flac_create("rec.flac", 48000, 2, 300);
```

### denoise

Applies noise suppression in place to interleaved f32 (48kHz, normalized to ±1.0). The output
is the input delayed by 480 samples/ch, and that much at the start is silence (streaming
latency).

```c
FlexDenoiser *dn = flexaudio_denoise_new(1);    // 1 = mono / 2 = stereo
flexaudio_denoise_process(dn, samples, len);    // in place (assumes 48kHz)
flexaudio_denoise_free(dn);
```

## Device hotplug watching

Device connects, disconnects, and default changes can be retrieved in a pull-based way.
Free `id` / `name` with `flexaudio_device_event_free`.

```c
FlexWatcher *w = flexaudio_watch_devices();     // degrades to a no-op on unsupported environments
FlexDeviceEvent ev;
int r = flexaudio_watcher_poll(w, &ev);
if (r == 1) {
    switch (ev.kind) {
        case FLEX_DEVICE_EVENT_KIND_ADDED:          /* ev.id / ev.name / ... */ break;
        case FLEX_DEVICE_EVENT_KIND_REMOVED:        /* ev.id only */ break;
        case FLEX_DEVICE_EVENT_KIND_DEFAULT_CHANGED:/* ev.id / ev.source_kind */ break;
        default: break;
    }
    flexaudio_device_event_free(&ev);
}
flexaudio_watcher_free(w);
```

## License and third-party components

This crate itself is MIT (same as the whole workspace; see `LICENSE`). The addons exposed
through the C ABI depend on the following offline processing libraries / models. None of them
needs network access at runtime or a separately distributed model file (weights and models
are embedded in the binary).

| Component | Purpose | License |
| --- | --- | --- |
| [flacenc](https://crates.io/crates/flacenc) | FLAC encoding (`flexaudio-encode`) | Apache-2.0 |
| [nnnoiseless](https://crates.io/crates/nnnoiseless) (RNNoise port) | Noise suppression (`flexaudio-denoise`) | BSD-3-Clause |
| [tract-onnx](https://crates.io/crates/tract-onnx) | Pure-Rust inference runtime for VAD (`flexaudio-vad`) | MIT OR Apache-2.0 |
| Silero VAD model | VAD model weights (embedded in the binary) | MIT |

When redistributing, include the copyright notices and license terms above.
