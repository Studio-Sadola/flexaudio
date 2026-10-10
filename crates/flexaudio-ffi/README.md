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
        /* ... poll audio; later replace the source and its exclusion list ... */
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

## Add denoise / VAD to a Stream

When `denoise` / `has_vad` is enabled, each chunk passes through **denoise → VAD** just before
`flexaudio_poll_chunk` returns it. denoise requires 48 kHz output (`flexaudio_open` returns
NULL if `output_rate` is not 48000). Confirmed VAD events are stored in
`FlexChunk::vad_events` and freed along with `data` by `flexaudio_chunk_free`.

VAD emits `SpeechStart` and `SpeechEnd` together when a segment is finalized,
not when speech first begins. On `DISCONTINUITY` (`flags & 1`), an open pre-gap
segment is flushed before the post-gap PCM is processed. Flushed events come
first and retain their pre-gap `at_sample` values; VAD then restarts its sample
clock at zero. Positions are measured at the VAD rate (16000 or 8000 Hz), not
in the chunk's PCM or `pts_ns` timeline.

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
still reset before `flexaudio_poll_chunk` returns `FLEX_FAILURE` and records the
flush error in `flexaudio_last_error()`. That poll consumes the chunk. When
reset succeeds, later chunks process normally without repeating that flush
error. If VAD reset itself fails, its failure remains latched and subsequent
processing reports it until a reset succeeds.

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

When redistributing the C static or shared library, ship
[`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md) alongside the library and header.
It contains the complete dependency license texts, MPL source-availability links,
and the embedded Silero model notice. From the repository root, regenerate it with
`scripts/gen-third-party-notices.sh`; CI checks it with
`scripts/gen-third-party-notices.sh --check`.

## Whisper-compatible VAD (additive ABI)

`flexaudio_whisper_vad_new(NULL, NULL)` creates a standalone mono16k normalized
float32 session with the five pinned whisper.cpp segmentation defaults and
preview disabled. The embedded inference model is Silero v6; segmentation is
pinned to `85a69493a601d4ff5a834064f7b7bac250bd8739`. Use
`flexaudio_whisper_vad_default_params()` to initialize parameters. An actual
parameter struct uses literal values, including zero; signed durations reject
negatives and values above 134217 ms. `FlexWhisperVadOptions.provisional` is a
fixed-width byte accepting only 0 or 1.

```c
FlexWhisperVad *vad = flexaudio_whisper_vad_new(NULL, NULL);
if (vad) {
    FlexWhisperVadEvent *events = NULL;
    size_t count = 0;
    float silence[513] = {0};
    int32_t rc = flexaudio_whisper_vad_process(vad, silence, 513, &events, &count);
    /* Consume/free events even when rc < 0: failure can carry terminal closure. */
    flexaudio_whisper_events_free(events, count);
    rc = flexaudio_whisper_vad_finish(vad, &events, &count);
    flexaudio_whisper_events_free(events, count);
    flexaudio_whisper_vad_free(vad);
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

`flexaudio_whisper_vad_probabilities` borrows the latest call's probability
array and first frame index until the next mutation/free. Copy it for longer
retention. `flexaudio_whisper_postprocessor_*` processes 16k/512 frame
probabilities without inference; its owned segment arrays use
`flexaudio_whisper_segments_free`, whereas event arrays use
`flexaudio_whisper_events_free`. Pass the exact original lengths and never C
`free`. A handle cannot be mutated concurrently. NULL input is allowed only at
zero length; mandatory output pointers cannot be NULL.

Maximum speech seconds retain pinned truncation/sentinel behavior. Fixed 200 ms
merging can undo maximum-duration splits, so final segment duration/finalization
latency is unbounded. Optional provisional pieces have a separate 30000 ms cap.
There are no audio-copying or timestamp-map APIs.

`FlexStreamConfigV2`, `FlexChunkV2`, `flexaudio_open_v2`,
`flexaudio_poll_chunk_v2`, and `flexaudio_chunk_free_v2` preserve the frozen v1
config/chunk layouts. Initialize the versioned config's size to `sizeof` and
version to `FLEX_STREAM_VERSION_2`; its config pointer references the unchanged
v1 struct. A NULL `whisper_vad` selects ordinary capture. New-mode options reject
legacy-VAD conflicts and secondary taps with explicit codes. **Primary attachment
currently returns `FLEX_WHISPER_UNSUPPORTED_CONVERSION_CLOCK` before opening a
device** because the producer has not exposed authoritative canonical capture
indices/valid tail lengths. Polled-frame counts are never treated as exact
capture origins. `flexaudio_flush_whisper_vad` is a no-op when disabled. Once
producer provenance is supplied, attached events have their own versioned union,
including EPOCH_START=6 and the u64 capture/i64 PTS origin.
