# flexaudio-ffi

**C ABI bindings** for using flexaudio from C (pull-based). This is the third way for a C app
to use flexaudio in-process (the first is the CLI pipe; the second is the N-API addon).
The caller periodically invokes `flexaudio_poll_chunk` / `flexaudio_poll_event` to retrieve
chunks and events.

In addition to core capture, this exposes three add-ons to C:

- **VAD** (voice activity detection; Silero VAD / ONNX) — `flexaudio_vad_*`
- **FLAC** (streaming lossless compression of recording chunks, with rotation) — `flexaudio_flac_*`
- **denoise** (noise suppression with RNNoise) — `flexaudio_denoise_*`

Denoise uses `FlexConfig.denoise`; VAD attaches through `FlexStreamConfigV2.vad`.
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

static int drain_events(FlexStream *s) {
    FlexEvent event;
    int result;
    while ((result = flexaudio_poll_event(s, &event)) == 1)
        printf("event=%d\n", event.kind);
    return result;
}

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
        if (drain_events(s) < 0) break;
        int r = flexaudio_poll_chunk(s, &chunk);
        if (r < 0) break;              // Error (flexaudio_last_error)
        if (r == 0) continue;          // Nothing available yet (wait briefly and retry)
        // chunk.data is interleaved f32 (chunk.len elements = frames * channels)
        printf("frames=%u peak=%.3f\n", chunk.frames, chunk.peak);
        flexaudio_chunk_free(&chunk);  // Free data
    }

    if (flexaudio_stop(s) < 0)
        fprintf(stderr, "stop failed: %s\n", flexaudio_last_error());
    for (;;) {
        if (drain_events(s) < 0) break;
        int r = flexaudio_poll_chunk(s, &chunk);
        if (r <= 0) break;
        printf("final frames=%u\n", chunk.frames);
        flexaudio_chunk_free(&chunk);
    }
    flexaudio_free(s);
    return 0;
}
```

Save this example as `capture.c`. From the repository root, check its identifiers and types with:

```sh
cc -std=c11 -Wall -Wextra -Werror -fsyntax-only -I crates/flexaudio-ffi/include capture.c
```

The capture example is also compiled against the generated header by the Rust test suite.
PCM polling does not wait for event polling, including after stop. Startup notices
are enqueued chronologically before publishing their generation's first chunk.
Event-first callers can drain the ordinary FIFO event queue before PCM to observe
the notice first; PCM-only callers can omit event polling. C v1 projects
the notice as `Unknown`, while C v2 retains its native frame count and reason.

## Exclude Playback by PID

Use the extended entry points without changing the `FlexConfig` layout:

```c
FlexConfig cfg = {0};
cfg.kind = FLEX_SOURCE_KIND_SYSTEM;
uint32_t excluded[] = {12345}; /* Replace with the process tree root to exclude. */
size_t count = sizeof excluded / sizeof excluded[0];

FlexStream *s = flexaudio_open_with_exclude_pids(&cfg, excluded, count);
if (!s) {
    fprintf(stderr, "open failed: %s\n", flexaudio_last_error());
} else {
    if (flexaudio_start(s) == FLEX_OK) {
        /* ... drain_events(s) before each PCM poll; later replace the source ... */
        if (flexaudio_switch_source_with_exclude_pids(s, &cfg, excluded, count) < 0)
            fprintf(stderr, "switch failed: %s\n", flexaudio_last_error());
    }
    flexaudio_free(s);
}
```

Lists are copied during each call; the array can be changed or released afterward. Each PID
must be in `1..=4294967295`, and the list can contain at most 4096 entries. Order and duplicates
are preserved. NULL is allowed with length zero; NULL with a nonzero length or a misaligned
pointer is invalid. Zero-PID errors identify the offending index. Validation applies even to
mic and process sources, which ignore valid lists. Legacy `flexaudio_open` and
`flexaudio_switch_source` use the same implementation with an empty list.

Exclusion applies to system capture and the system side of mix, combined with `exclude_self`.
Windows requires every listed PID to equal one process tree root (the current process when
`exclude_self` is true, otherwise the first listed PID). macOS resolves PIDs once at start;
Linux matches exact PIDs without descendants. macOS honors the selected device alongside
exclusion; Linux and Windows ignore device selection while exclusion is active.

## Noise Suppression and Attached VAD

Set `FlexConfig.denoise=1` to enable RNNoise on 48000 Hz output. With attached
VAD, noise suppression runs on the shared capture branch before segmentation
and output conversion. Otherwise it processes delivered chunks during polling.
It introduces a 480-sample delay per channel; stop and poll to retain its tail.
See the generic VAD section below for canonical capture attachment.

`FlexConfig.silence_probe` accepts 0 (default enabled), 1 (enabled), or 2
(disabled); other values are rejected. The macOS silence diagnosis probe plays
a 1 kHz, 300 ms tone at amplitude 1e-5 after five seconds of exact-zero system
capture. Set 2 to disable this optional diagnostic.

## Standalone Handles (Without a Stream)

### VAD

The generic VAD section below describes standalone sessions and capture
attachment. Standalone input accepts arbitrary positive rates/channel counts,
converts internally to mono16k, and reports integer milliseconds on the input
timeline with converter group delay compensated. After raw input validation,
finite resampler overshoot is clipped internally to [-1, 1].

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

When redistributing the C static or shared library, ship
[`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md) alongside the library and header.
It contains the complete dependency license texts, MPL source-availability links,
and the embedded Silero model notice. From the repository root, regenerate it with
`scripts/gen-third-party-notices.sh`; CI checks it with
`scripts/gen-third-party-notices.sh --check`.

## Generic VAD

`flexaudio_vad_new(NULL, NULL)` creates a standalone mono16k normalized
float32 session with the five pinned whisper.cpp segmentation defaults and
preview disabled. The embedded inference model is Silero v6; segmentation is
pinned to `85a69493a601d4ff5a834064f7b7bac250bd8739`. Use
`flexaudio_vad_default_options()` to initialize parameters. An actual
parameter struct uses literal values, including zero; signed durations reject
negatives and values above 134217 ms. `FlexVadRuntimeOptions.provisional` is a
fixed-width byte accepting only 0 or 1.

```c
FlexVad *vad = flexaudio_vad_new(NULL, NULL);
if (vad) {
    FlexVadEvent *events = NULL;
    size_t count = 0;
    float silence[513] = {0};
    int32_t rc = flexaudio_vad_process(vad, silence, 513, 16000, 1, &events, &count);
    if (rc < 0) fprintf(stderr, "%s\n", flexaudio_last_error());
    /* Consume/free events even when rc < 0: failure can carry terminal closure. */
    flexaudio_vad_events_free(events, count);
    rc = flexaudio_vad_finish(vad, &events, &count);
    if (rc < 0) fprintf(stderr, "%s\n", flexaudio_last_error());
    flexaudio_vad_events_free(events, count);
    flexaudio_vad_free(vad);
}
```

Process, finish and reset return owned tagged events. Read only the payload
selected by `type`: SEGMENT=1, SPEECH_START=2, SPEECH_END=3, CUT=4, EPOCH_END=5.
Reason tags are HYSTERESIS=1, FINISH=2, RESET=3, ERROR=4, LIMIT=5; LIMIT is valid
only for cuts. Every event carries epoch and seq. All times are integer ms
relative to that epoch; final endpoints use a 10 ms grid and may exceed physical
EOF. `finish` infers one zero-padded partial tail, ends the epoch and is
idempotent. `reset` returns closure before starting a new epoch. Consume all
returned events, including typed fatal-error terminal batches on negative
results. Validation returns NULL/0 outputs and preserves session state.

`flexaudio_vad_probabilities` borrows the latest call's probability
array and first frame index until the next mutation/free. Copy it for longer
retention. `flexaudio_vad_postprocessor_*` processes 16k/512 frame
probabilities without inference; its owned segment arrays use
`flexaudio_vad_segments_free`, whereas event arrays use
`flexaudio_vad_events_free`. Pass the exact original lengths and never C
`free`. A handle cannot be mutated concurrently. NULL input is allowed only at
zero length; mandatory output pointers cannot be NULL.

`max_speech_ms` uses integer milliseconds; 0 means unbounded. Values below one
inference frame plus twice the padding budget are rejected. Fixed 200 ms
merging can undo maximum-duration splits, so final segment duration/finalization
latency is unbounded. Optional provisional pieces have a separate 30000 ms cap.
There are no audio-copying or timestamp-map APIs.

`FlexStreamConfigV2`, `FlexChunkV2`, `flexaudio_open_v2`,
`flexaudio_poll_chunk_v2`, and `flexaudio_chunk_free_v2` provide versioned
configuration and chunks. Initialize the versioned config's size to `sizeof` and
version to `FLEX_STREAM_VERSION_2`; its config pointer references `FlexConfig`. A NULL `vad` selects ordinary capture. Secondary taps are rejected with a dedicated VAD status code. Primary attachment
uses producer-assigned canonical capture indices and valid tail lengths; attached
events use a separate versioned union, including EPOCH_START=6 and its u64
capture/i64 PTS origin. `flexaudio_flush_vad` is a no-op when disabled.
Both flush and stop finish processing internally and retain versioned chunks.
PCM polling remains independent of event polling after flush or stop. Event-first
callers can drain stream events before retained chunks to observe startup notices
already enqueued before their generation's first chunk was published.

An attached VAD failure closes its epoch once while capture and polling
continue. `flexaudio_stop` returns `FLEX_OK` for a VAD-only failure, and the
shutdown report retains it among cleanup causes while its capture primary stays
absent. Borrow its stable typed code with `flexaudio_error_vad_code_v2` on
the cleanup error. The next discontinuity or a successful explicit flush
reinitializes the shared session before a new origin is published.

`flexaudio_free` always stops and releases a live stream, including unread PCM
and VAD events. Stop and drain first when final outputs are needed. Event kind 9
is reserved; it is never reused. `permissionPending` remains for custom backends.
VAD status codes occupy a distinct range starting at -100.
