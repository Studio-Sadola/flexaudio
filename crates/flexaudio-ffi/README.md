# flexaudio-ffi

**C ABI bindings** for using flexaudio from C (pull-based). This is the third way for a C app
to use flexaudio in-process (the first is the CLI pipe; the second is the N-API addon).
The caller periodically invokes `flexaudio_poll_chunk` / `flexaudio_poll_event` to retrieve
chunks and events.

Use each `FlexStream` from one thread at a time, or externally serialize all calls
on that handle, including ordinary, PCM, and lifecycle polls, stop, switch, and free.
The event queues synchronize draining and consumption internally; this does not
make other stream operations thread-safe.

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
(each entry is a `FlexProcessInfo`). Pass `pid` to `FlexConfig.process_id`. If unavailable,
`executable` / `bundle_id` are NULL; `output_activity` is one of
`FLEX_OUTPUT_ACTIVITY_UNKNOWN|INACTIVE|ACTIVE`.
An empty result is successful (the feature is available, but there are no candidates right
now). The call returns a typed negative result code, with the reason in
`flexaudio_last_error()`, if per-process capture is unavailable in the current
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
mic and process sources; nonempty lists there are rejected by default strict mode
(`FLEX_IGNORED_SETTINGS`). To permit ignoring them, open through v4 with `strict = 2`.
Legacy `flexaudio_open` and `flexaudio_switch_source` use the same implementation
with an empty list.

Exclusion applies to system capture and the system side of mix, combined with `exclude_self`.
Windows requires every listed PID to equal one process tree root (the current process when
`exclude_self` is true, otherwise the first listed PID). macOS resolves PIDs once at start;
Linux matches exact PIDs without descendants. macOS honors the selected device alongside
exclusion; Linux and Windows ignore device selection while exclusion is active.
Default strict mode rejects a non-default selected system device in that case;
use v4 with `strict = 2` to permit ignoring it.

## Capabilities and Version 3 Capture

`flexaudio_capabilities_v1(&snapshot)` returns an owned, read-only
`FlexCapabilitiesV1`. It queries backend prerequisites without opening capture,
enumerating microphones, requesting recording permission or playing audio.
Run the bounded query on a worker thread in GUI applications. Supported means an
implemented backend with satisfied prerequisites; device availability and permission
are checked separately when opening capture.

On Linux, a missing PipeWire socket means unavailable. Failed socket inspection,
connection or server sync means unknown, with the structured query error retained.

The `flexaudio_capabilities_*_v1` accessors copy records for each source kind,
system exclusions, process scopes, watcher, side effect and running OS version.
Watcher scopes and side effect sources have indexed accessors. Records expose every
Rust snapshot field, including OS version prerequisites, optional PID limits and
device selection semantics. Optional version components use explicit `has_*` words.
Record accessors return 1 when present, 0 when absent, or a negative typed error.
An absent result leaves the output untouched. Numeric tags use the header's
`FLEX_CAP_*`, `FLEX_BACKEND_*`, `FLEX_PID_*`, `FLEX_WATCH_*`,
`FLEX_WATCHER_*`, `FLEX_SIDE_EFFECT_*`, `FLEX_CONDITION_*` and
`FLEX_DEVICE_SELECTION_*` defines. Accept only known codes; `FLEX_CAP_OTHER`
and any future code never imply support.

An availability with `FLEX_CAP_UNKNOWN` carries a borrowed `FlexErrorInfoV2`
in `unknown`, readable using the existing structured error accessors. This error
is independent of thread-local last_error and lives until
`flexaudio_capabilities_free_v1(snapshot)`. Never free a borrowed error separately.

Use `FlexStreamConfigV3` to combine attached VAD, exclusion PIDs and secondary
output. All versioned envelopes require their exact `sizeof` and version 3;
NULL nested options disable the corresponding feature. PID rules match the
existing exclusion entry points, including the 4096-entry bound.

```c
FlexConfig base = {0};
base.kind = FLEX_SOURCE_KIND_SYSTEM;
uint32_t excluded[] = {12345};
FlexSecondaryOutputV3 secondary = {
    .size = sizeof secondary, .version = FLEX_STREAM_VERSION_3,
    .rate = 16000, .channels = 1, .encoding = FLEX_SECONDARY_S16
};
FlexStreamVadOptions vad = {
    .params = flexaudio_vad_default_options(),
    .provisional = 1, .tap = FLEX_VAD_SECONDARY
};
FlexStreamConfigV3 options = {
    .size = sizeof options, .version = FLEX_STREAM_VERSION_3,
    .config = &base, .vad = &vad,
    .exclude_pids = excluded, .exclude_pids_len = 1,
    .secondary_output = &secondary,
    .windows_classic_loopback_keepalive = FLEX_KEEPALIVE_DEFAULT
};
FlexStream *stream = NULL;
int result = flexaudio_open_v3(&options, &stream);
/* On success, start with flexaudio_start and release with flexaudio_free. */
(void)result;
```

Keepalive accepts 0 (default enabled at open; retain current value on switch),
1 (enabled), or 2 (disabled). Other values are InvalidArg. The option applies to
Windows classic system loopback without exclusions, including the system lane of
mix. The example keeps the default because its exclusions bypass classic loopback.
Disabling keepalive on other platforms or process/exclusion paths is an ignored
non-default request: default strict mode rejects it with `FLEX_IGNORED_SETTINGS`.
To permit that request, use v4 with `strict = 2`. Use
`FlexSwitchConfigV3` with `flexaudio_switch_source_v3` to change it together with
the source. The switch retains output formats, encoding, attached VAD, gain and
denoise from open; failed switches retain the previous options. With keepalive
disabled, idle classic loopback delivers no packets; the 2 s watchdog reports
`StreamStalled` (C stream event kind 1) and reopens repeatedly until audio plays.

Poll primary PCM with the existing APIs (`flexaudio_poll_chunk_v2` for attached
VAD). Independently poll secondary PCM with
`flexaudio_poll_secondary_chunk_v3(stream, &chunk)`: 1 means one owned chunk,
0 means none, and a negative result is an error. Read its metadata using
`flexaudio_secondary_metadata_v3`, then borrow native `float` or `int16_t`
samples with `flexaudio_secondary_f32_v3` or `flexaudio_secondary_s16_v3`.
Using the wrong accessor returns NULL/0 and a typed InvalidArg. The format and
encoding remain fixed throughout the stream. Rates/channels in secondary options
are literal; no zero defaults apply.

Secondary meters describe processed float PCM before integer quantization; s16
uses the shared saturating quantizer. Each tap has independent sequence numbers,
frame indices and drops. PTS labels can help compare arrival times; they are not
an exact media clock. Attached VAD always uses the dedicated canonical capture
timeline. Selecting `FLEX_VAD_SECONDARY` carries its events only on secondary
chunks via `flexaudio_secondary_vad_events_v3`, and requires secondary output.

Version 3 denoise runs once on canonical 48 kHz stereo capture before both taps
are converted, including when the delivered output rates differ from 48 kHz.
`flexaudio_stop` drains both taps and the denoiser tail. `flexaudio_flush_vad`
drains pending primary/secondary chunks before closing the current VAD epoch.
Continue polling both taps after stop/flush to collect their final chunks and
zero-frame event carriers. Secondary chunks own all their arrays and remain
valid after stream release. Free each exactly once with
`flexaudio_secondary_chunk_free_v3`; never free its borrowed arrays separately.
All v1/v2 layouts, signatures and symbols remain unchanged.

## Noise Suppression and Attached VAD

Set `FlexConfig.denoise=1` to enable RNNoise at any output rate in every C version.
Noise suppression runs once in the library
on canonical 48 kHz stereo capture before downmix and gain, feeding both
output taps and attached VAD.
It introduces 10 ms of delay (480 samples per canonical channel). Stop queues its tail through the
facade and zero-pads the final output to a full 20 ms chunk (`flags` bit 8 when
padding is added); continue polling to retain it. Denoise runs before output
polling and queue drops, continues processing while paused, and keeps its history
on resume. Source switches/reopens start fresh history and normally discard the
previous generation's buffered remainder. These rules also apply to legacy opens.
See the generic VAD section below for canonical capture attachment.

`FlexConfig.silence_probe` accepts 0 (default enabled), 1 (enabled), or 2
(disabled); other values are rejected. On macOS, system/process capture and the
system lane of Mix probe once per capture generation after five continuous
seconds of bit-exact zero native samples while an eligible external captured
process has active output (honoring exclusions and device selection). The probe
plays a roughly 300 ms, 1 kHz signal at amplitude 1e-5. This low-level signal is
not guaranteed inaudible and may enter capture when the host is included.
Set 2 to disable this optional diagnostic on those macOS sources. On a microphone
or outside macOS, that non-default request is rejected by default strict mode;
use v4 with `strict = 2` to permit the ignored request.

## Standalone Handles (Without a Stream)

### VAD

The generic VAD section below describes standalone sessions and capture
attachment. Standalone input accepts rates of 8000–192000 Hz and positive channel counts,
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
// Write the processed samples to your sink, then retain the delayed tail.
float *tail = NULL;
size_t tail_len = 0;
if (flexaudio_denoise_flush(dn, &tail, &tail_len) == FLEX_OK) {
    // Write tail[0..tail_len] to the same sink before freeing it.
    flexaudio_denoise_samples_free(tail, tail_len);
}
flexaudio_denoise_free(dn);
```

## Monitor Device Changes (Hotplug)

The pull-based API reports Linux PipeWire system output additions, removals and
default changes using the same output IDs as `devices()`. Microphone and
default-microphone changes are not reported: CPAL microphone IDs cannot be resolved
from PipeWire nodes. Windows/macOS use a no-op watcher.
Free `id` / `name` with `flexaudio_device_event_free`.

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

`max_speech_ms` uses integer milliseconds; 0 or values above 100000000 use the effectively
unbounded legacy `INT_MAX/2`-sample sentinel (1073741823 samples). Other positive values below
one inference frame plus twice the padding budget are rejected; equality (a zero split
budget) is valid. Fixed 200 ms
merging can undo maximum-duration splits, so final segment duration/finalization
latency is unbounded. Optional provisional pieces have a separate 30000 ms cap.
There are no audio-copying or timestamp-map APIs.

`FlexStreamConfigV2`, `FlexChunkV2`, `flexaudio_open_v2`,
`flexaudio_poll_chunk_v2`, and `flexaudio_chunk_free_v2` provide versioned
configuration and chunks. Initialize the versioned config's size to `sizeof` and
version to `FLEX_STREAM_VERSION_2`; its config pointer references `FlexConfig`. A NULL `vad` selects ordinary capture.
v1 has no attached VAD. Primary attachment starts at v2; v2 rejects secondary VAD
attachment with a dedicated VAD status code. v3/v4 accept it when secondary output is configured. Primary attachment
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

## Version 4 settings and lifecycle

`FlexStreamConfigV4` and `FlexSwitchConfigV4` wrap the unchanged version 3
envelopes. Initialize every envelope's `size` to its `sizeof` and use its matching
version constant. All sizes and versions must match exactly. NULL `options` uses
defaults on open and retains controls on switch. `FlexOptionsV4.strict` accepts
only 0 (strict on open/retain on switch), 1 (strict), or 2 (permissive).
C v1-v3 also open strict, rejecting ignored non-default settings; migrate to
v4 and set `strict = 2` to opt out. Zero-initialized microphone configurations
still use defaults, including `silence_probe = 0` meaning enabled.
A NULL capacity pointer
selects 50 on open and retains capacity on switch; a supplied capacity must be
positive and fit `size_t`.

Only decoded values differing from documented defaults count as requested.
Explicit defaults are not requested, including on switch after retaining
non-default effective values; existing C sentinels are decoded before comparison.
For example, a supplied capacity of 50 is not requested and retains switch
capacity; another value is rejected with `fixedAtOpen` in strict mode. Permissive
mode (v4 `strict = 2` on open) permits ignoring it. Gain is also fixed at open,
as is `config.denoise` on source switch. Use `flexaudio_set_gain` for runtime gain.
See the root README's [ignored-setting table](https://github.com/Studio-Sadola/flexaudio/blob/v0.9.0/README.md#strict-mode-and-effective-settings)
and [complete C example](https://github.com/Studio-Sadola/flexaudio/blob/v0.9.0/README.md#strict-mode-and-effective-settings).
Strict requests that would be ignored return `FLEX_IGNORED_SETTINGS` (-13)
before device access, model loading, or stopping the current source. Obtain an
owned error with `flexaudio_last_error_info_v2`, then borrow its complete candidate
report using `flexaudio_error_settings_v4` (1 present, 0 absent). The report and
its item pointers belong to the error. Passing a live borrowed report to
`flexaudio_settings_free_v4` is a no-op; freeing the error releases the report.
The borrowed report must not be used after its error owner is freed.

`flexaudio_settings_v4` returns an independent snapshot of the last successful
open/switch. `flexaudio_settings_count_v4` supplies revision and entry count;
`flexaudio_settings_item_v4` borrows one entry. These three functions return zero
on success. Free standalone snapshots with `flexaudio_settings_free_v4`.
Snapshot storage remains valid after switching or freeing the stream. Item
strings and PID arrays remain valid until their snapshot owner is freed.

`flexaudio_poll_lifecycle_event_v4` consumes a separate ordered helper-event
queue (1 available, 0 empty). Ordinary v1/v2 event polls never return helper
lifecycle events. Both queues can be drained independently after stop. Each
stream retains at most 256 queued lifecycle events, dropping the oldest on
overflow, including when only ordinary events are polled. Started/Stopped pairing
is guaranteed only while the caller drains lifecycle events fast enough to avoid
overflow. Use the same serialized stream access described above for lifecycle
polling. Each
returned immutable event survives stream destruction and is freed with
`flexaudio_lifecycle_event_free_v4`. Kind access returns zero on success;
reason/message access returns 1 when present or 0 without changing the output.
Stop-reason codes are 1 signal observed, 2 permission denied, 3 inconclusive,
4 timed out, 5 stream stopped, 6 source switched, 7 recovery, 8 startup failed,
9 runtime failed, and 10 cancelled; 0 is unknown. Started events have no reason.
Every emitted Started has exactly one Stopped: macOS Started means entry into
a diagnostic attempt; Windows Started follows successful keepalive render start.
Stopped ends ownership after teardown attempts, without guaranteeing native
cleanup succeeded. Lifecycle ordering relative to PCM or ordinary events is not
guaranteed. See [helper behavior and controls](https://github.com/Studio-Sadola/flexaudio/blob/v0.9.0/README.md#optional-playback-helpers-and-lifecycle).

Use `flexaudio_chunk_arrival_ns_v4` and `flexaudio_secondary_arrival_ns_v4` for
arrival time. They return the existing `pts_ns` value exactly. The old naming is
documentation-only deprecated in 0.8 and retained through 0.9; frozen C layouts retain these fields
permanently. `frame_index` remains authoritative for audio continuity.

`flexaudio_devices_with_namespaces_v4` preserves selectable device IDs and adds
provider provenance. Namespace tags describe selector domains, not persistent
native IDs. Free the returned array and device strings with
`flexaudio_namespaced_devices_free_v4`; namespace strings have static lifetime.
All new owner-free functions are NULL-safe.
