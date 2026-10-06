# flexaudio-ffi

**C ABI bindings** for using flexaudio from C (pull-based). This is the third way for a C app
to use flexaudio in-process (the first is the CLI pipe; the second is the N-API addon).
The caller periodically invokes `flexaudio_poll_chunk` / `flexaudio_poll_event` to retrieve
chunks and events.

In addition to core capture, this exposes three add-ons to C:

- **VAD** (voice activity detection; Silero VAD / ONNX) — `flexaudio_vad_*`
- **FLAC** (streaming lossless compression of recording chunks, with rotation) — `flexaudio_flac_*`
- **denoise** (noise suppression with RNNoise) — `flexaudio_denoise_*`

VAD / denoise can also be built into a stream (`has_vad` / `denoise` in `FlexConfig`).
Device connection, disconnection, and default-device changes (hotplug) are available through
the `flexaudio_watch_devices` family.
Use `flexaudio_processes` to list candidates for per-process capture: processes with audio
output sessions / streams, excluding the current process. Processes whose audio sessions are
stopped or idle are also included. Free it **exactly once** with `flexaudio_processes_free`
(each entry is a `FlexProcessInfo`). Pass `pid` to `FlexConfig::process_id`. If unavailable,
`executable` / `bundle_id` are NULL; `output_activity` is one of
`FLEX_OUTPUT_ACTIVITY_UNKNOWN|INACTIVE|ACTIVE`.
An empty result is successful (the feature is available, but there are no candidates right
now). The call returns `FLEX_FAILURE` if per-process capture is unavailable in the current
environment (PipeWire is unreachable on Linux; macOS is earlier than 14.4; Windows is older than
build 20348 (Windows 11 / Windows Server 2022 or later is required); the OS is unsupported; or
access is denied), if the OS does not respond within 3 seconds, or if the previous query is still
running.

## Build and Generate the Header

This builds both `staticlib` (`.a`) and `cdylib` (`.so` / `.dll` / `.dylib`).

```sh
cargo build -p flexaudio-ffi --release
# Regenerate the header whenever the ABI changes.
cbindgen --config cbindgen.toml --crate flexaudio-ffi --output include/flexaudio.h
```

Link the build artifacts with `include/flexaudio.h`. Functions do not unwind panics across the
FFI boundary (`catch_unwind`). On failure, they return a negative code and leave a message in
`flexaudio_last_error()`. Memory passed to C (such as `FlexChunk::data`, VAD event arrays, and
device strings) must be freed by flexaudio through the corresponding free function. Do not use
C's `free`.

## Basic Capture

```c
#include "flexaudio.h"
#include <stdio.h>

int main(void) {
    FlexConfig cfg = {0};              // All zeros = defaults (mic / 48k / stereo / 20ms)
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
        if (r < 0) break;              // Error (flexaudio_last_error)
        if (r == 0) continue;          // Nothing available yet (wait briefly and retry)
        // chunk.data is interleaved f32 (chunk.len elements = frames * channels)
        printf("frames=%u peak=%.3f\n", chunk.frames, chunk.peak);
        flexaudio_chunk_free(&chunk);  // Free data
    }

    flexaudio_free(s);
    return 0;
}
```

## Add denoise / VAD to a Stream

When `denoise` / `has_vad` is enabled, each chunk passes through **denoise → VAD** just before
`flexaudio_poll_chunk` returns it. denoise requires 48 kHz output (`flexaudio_open` returns
NULL if `output_rate` is not 48000). Confirmed VAD events are stored in
`FlexChunk::vad_events` and freed along with `data` by `flexaudio_chunk_free`.

```c
FlexConfig cfg = {0};
cfg.kind = FLEX_SOURCE_KIND_MIC;
cfg.denoise = true;      // Requires 48k output (output_rate=0 means 48000)
cfg.has_vad = true;      // All-zero cfg.vad uses Silero defaults (threshold 0.5, etc.)

FlexStream *s = flexaudio_open(&cfg);
/* ... start / poll ... */
if (flexaudio_poll_chunk(s, &chunk) == 1) {
    for (size_t i = 0; i < chunk.vad_events_len; i++) {
        FlexVadEvent ev = chunk.vad_events[i];   // kind: 0=start / 1=end
        printf("%s @ %lld\n", ev.kind == 0 ? "speech-start" : "speech-end",
               (long long)ev.at_sample);
    }
    flexaudio_chunk_free(&chunk);    // Free both data and vad_events
}
```

## Standalone Handles (Without a Stream)

### VAD

You can pass f32 samples in any format; they are converted to mono and resampled to the VAD
rate internally. Free the event array with `flexaudio_vad_events_free`.

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

This streams lossless compression of interleaved f32 samples (you can pass flexaudio's
canonical 48 kHz / stereo format directly). When `split_seconds` is 1 or greater, files rotate
at that interval with sequential names such as `rec-001.flac`, `rec-002.flac`, … (0 creates a
single file).

```c
// Single file
FlexFlac *flac = flexaudio_flac_create("rec.flac", 48000, 2, 0);
flexaudio_flac_write(flac, samples, len);       // Append as many times as needed
flexaudio_flac_finalize(flac);                  // Finalize header (no more writes allowed)
flexaudio_flac_free(flac);

// Split every 5 minutes -> rec-001.flac, rec-002.flac, ...
FlexFlac *split = flexaudio_flac_create("rec.flac", 48000, 2, 300);
```

### denoise

This applies in-place noise suppression to interleaved f32 samples (48 kHz, normalized to
±1.0). The output is delayed by 480 samples per channel, so the beginning is silent for that
duration (streaming latency).

```c
FlexDenoiser *dn = flexaudio_denoise_new(1);    // 1 = mono / 2 = stereo
flexaudio_denoise_process(dn, samples, len);    // In-place (requires 48 kHz)
flexaudio_denoise_free(dn);
```

## Monitor Device Changes (Hotplug)

Device connection, disconnection, and default-device changes are available through a pull-based
API. Free `id` / `name` with `flexaudio_device_event_free`.

```c
FlexWatcher *w = flexaudio_watch_devices();     // Degrades to a no-op on unsupported systems
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

## License and Third-Party Components

This crate is MIT licensed, like the rest of the workspace (see `LICENSE`). The add-ons exposed
through the C ABI depend on the offline processing libraries / models below. They require no
network access at runtime and do not distribute model files; weights and models are embedded in
the binary.

| Component | Purpose | License |
| --- | --- | --- |
| [flacenc](https://crates.io/crates/flacenc) | FLAC encoding (`flexaudio-encode`) | Apache-2.0 |
| [nnnoiseless](https://crates.io/crates/nnnoiseless) (RNNoise port) | Noise suppression (`flexaudio-denoise`) | BSD-3-Clause |
| [tract-onnx](https://crates.io/crates/tract-onnx) | Pure Rust inference for VAD (`flexaudio-vad`) | MIT OR Apache-2.0 |
| Silero VAD model | VAD model weights (embedded in the binary) | MIT |

When redistributing, include the copyright notices and license terms listed above.
