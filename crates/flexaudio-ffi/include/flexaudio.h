/*
 * flexaudio C API — pull-based audio capture bindings.
 *
 * This header is generated from crates/flexaudio-ffi by cbindgen. Do not edit by hand;
 * regenerate it after changing the Rust ABI.
 */


#ifndef FLEXAUDIO_H
#define FLEXAUDIO_H

#pragma once

#include <stdarg.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>

// Success.
#define FLEX_OK 0

// Invalid argument (NULL pointer, invalid UTF-8, unknown enum value, etc.).
#define FLEX_INVALID_ARG -1

// A flexaudio operation failed (the message is stored in last_error).
#define FLEX_FAILURE -2

// A panic was caught at the FFI boundary (the message is stored in last_error).
#define FLEX_PANIC -3

// The handle's state does not fit the operation (such as a write to a finalized FLAC).
#define FLEX_INVALID_STATE -4

// Kind of audio source to record (corresponds to [`flexaudio::SourceKind`]).
typedef enum FlexSourceKind {
    // Microphone input.
    FLEX_SOURCE_KIND_MIC = 0,
    // Loopback of the whole system output.
    FLEX_SOURCE_KIND_SYSTEM = 1,
    // Output loopback of a specific process.
    FLEX_SOURCE_KIND_PROCESS = 2,
    // Records the microphone and system audio mixed into one stream.
    FLEX_SOURCE_KIND_MIX = 3,
} FlexSourceKind;

// Whether a process source includes or excludes the target PID (corresponds to
// [`flexaudio::ProcessMode`]).
typedef enum FlexProcessMode {
    // Records only the target PID (and its process tree).
    FLEX_PROCESS_MODE_INCLUDE = 0,
    // Records all system audio except the target PID.
    FLEX_PROCESS_MODE_EXCLUDE = 1,
} FlexProcessMode;

// Kind of stream event (corresponds to [`flexaudio::Event`]).
typedef enum FlexEventKind {
    // Chunks were dropped because the chunk ring was full (the count is `FlexEvent::count`).
    FLEX_EVENT_KIND_CHUNK_DROPPED = 0,
    // Data stopped arriving and the stream stalled.
    FLEX_EVENT_KIND_STALLED = 1,
    // Data resumed arriving after a stall.
    FLEX_EVENT_KIND_RECOVERED = 2,
    // A required permission was denied.
    FLEX_EVENT_KIND_PERMISSION_DENIED = 3,
    // The capture device was lost.
    FLEX_EVENT_KIND_DEVICE_LOST = 4,
    // Any other backend error (get the message with `flexaudio_last_error`).
    FLEX_EVENT_KIND_ERROR = 5,
    // An event that matches none of the known ones (in preparation for future variants).
    FLEX_EVENT_KIND_UNKNOWN = 6,
} FlexEventKind;

// Whether the process is outputting audio right now (corresponds to
// [`flexaudio::ProcessInfo::is_output_active`]).
//
// `Unknown` (Rust's `None`) when the OS does not expose that state.
typedef enum FlexOutputActivity {
    // The OS does not expose the state / it could not be read.
    FLEX_OUTPUT_ACTIVITY_UNKNOWN = 0,
    // Not outputting (Linux = node is not Running / Windows = session is Inactive /
    // macOS = IsRunningOutput is 0).
    FLEX_OUTPUT_ACTIVITY_INACTIVE = 1,
    // Outputting.
    FLEX_OUTPUT_ACTIVITY_ACTIVE = 2,
} FlexOutputActivity;

// Kind of device hotplug event (corresponds to [`flexaudio::DeviceEvent`]).
typedef enum FlexDeviceEventKind {
    // A device was added (`device`/`name` etc. are filled).
    FLEX_DEVICE_EVENT_KIND_ADDED = 0,
    // A device was removed (`id` only).
    FLEX_DEVICE_EVENT_KIND_REMOVED = 1,
    // The OS default device changed (`id` and `source_kind`).
    FLEX_DEVICE_EVENT_KIND_DEFAULT_CHANGED = 2,
    // An event that matches none of the known ones (in preparation for future variants).
    FLEX_DEVICE_EVENT_KIND_UNKNOWN = 3,
} FlexDeviceEventKind;

// Opaque noise suppression handle. It contains a [`flexaudio_denoise::Denoiser`].
// Create it with `flexaudio_denoise_new` and free it with `flexaudio_denoise_free`.
typedef struct FlexDenoiser FlexDenoiser;

// Opaque handle for FLAC writing. Create it with `flexaudio_flac_create`, append chunks with
// `flexaudio_flac_write`, finalize with `flexaudio_flac_finalize`, and free it with
// `flexaudio_flac_free`.
typedef struct FlexFlac FlexFlac;

// Opaque handle to a recording stream. It contains a [`flexaudio::Stream`] and, when enabled,
// the addons (denoise / VAD) living alongside it; the C side holds only the pointer. Create it
// with `flexaudio_open` and free it with `flexaudio_free`.
//
// The addons are enclosed here as stream state (a thin wrapper). `poll_chunk` passes chunks
// through denoise → VAD in that order before returning them. `flexaudio_switch_source`
// replaces only the source and keeps the addons as configured at open time (same treatment
// as gain).
typedef struct FlexStream FlexStream;

// Opaque VAD handle. It contains a [`flexaudio_vad::Vad`] (which holds one ONNX session).
// Create it with `flexaudio_vad_new` and free it with `flexaudio_vad_free`.
typedef struct FlexVad FlexVad;

// Opaque device watcher handle. It contains a [`flexaudio::DeviceWatcher`].
// Create it with `flexaudio_watch_devices` and free it with `flexaudio_watcher_free`.
typedef struct FlexWatcher FlexWatcher;

// VAD (speech segment detection) settings. Passed to `FlexConfig::vad` and
// `flexaudio_vad_new`.
//
// Each value uses the sentinel 0 for the default (mapped to the defaults of
// [`flexaudio_vad::VadConfig`]). `threshold` 0 → 0.5, `neg_threshold` 0 → the silero formula
// `max(threshold-0.15, 0.01)`, `min_speech_ms` 0 → 250, `min_silence_ms` 0 → 100,
// `speech_pad_ms` 0 → 30, `sample_rate` 0 → 16000. For `max_speech_ms`, 0 itself means
// "unlimited" (the default).
//
// [`flexaudio_vad_new`]: crate::flexaudio_vad_new
typedef struct FlexVadConfig {
    // Probability threshold for treating speech as started (>=). 0 means 0.5.
    float threshold;
    // Negative threshold for treating silence as started (<). 0 means determined
    // automatically by the silero formula.
    float neg_threshold;
    // Minimum length (ms) of speech to accept. Shorter segments are discarded. 0 means 250.
    uint32_t min_speech_ms;
    // Length of silence (ms) needed to finalize the end of speech. 0 means 100.
    uint32_t min_silence_ms;
    // Padding (ms) that widens segment boundaries on both sides. 0 means 30.
    uint32_t speech_pad_ms;
    // Maximum length (ms) of one segment. 0 is unlimited (the default). Longer segments are
    // forcibly split.
    uint32_t max_speech_ms;
    // Sample rate (8000 or 16000). 0 means 16000.
    uint32_t sample_rate;
} FlexVadConfig;

// Config for opening a stream. Passed to `flexaudio_open` / `flexaudio_switch_source`.
//
// Strings and optional values use sentinels for "unspecified" (`device_id` NULL means the
// default device, `process_id` 0 means none, `output_rate`/`output_channels`/`chunk_ms` 0
// means the default).
typedef struct FlexConfig {
    // Source kind.
    enum FlexSourceKind kind;
    // ID of the device to select (UTF-8, NUL-terminated). NULL means the default device.
    const char *device_id;
    // Target PID of a process source. 0 means none (a process source may fail at start).
    uint32_t process_id;
    // Whether to include or exclude the target PID (process source only).
    enum FlexProcessMode mode;
    // Whether to exclude the host's own playback from system audio (system source only;
    // for mix it applies to the system side).
    bool exclude_self;
    // Output sample rate (Hz). 0 means 48000.
    uint32_t output_rate;
    // Number of output channels. 0 means 2.
    uint16_t output_channels;
    // Chunk length (milliseconds). 0 means 20.
    uint32_t chunk_ms;
    // Input gain (linear multiplier) at start. 0 means 1.0 (the default). To mute at runtime,
    // use `flexaudio_set_gain(s, 0.0)`.
    float gain;
    // ID of the input device to select for the mic side of mix (UTF-8, NUL-terminated; mix
    // only). NULL means the default input.
    const char *mix_mic_device_id;
    // ID of the output endpoint to select for the system side of mix (UTF-8, NUL-terminated;
    // mix only). NULL means the default output.
    const char *mix_system_device_id;
    // Pre-mix multiplier for the mic side of mix (linear; mix only). 0 means 1.0 (the
    // default). The global `gain` is applied after mixing.
    float mix_mic_gain;
    // Pre-mix multiplier for the system side of mix (linear; mix only). 0 means 1.0 (the
    // default).
    float mix_system_gain;
    // Whether to insert noise suppression (RNNoise) into the stream. `true` enables it. When
    // enabled, `flexaudio_open` fails (NULL + last_error) unless the output rate is 48000.
    // denoise processes data in place right before `poll_chunk` returns (before VAD).
    bool denoise;
    // Whether to insert VAD (speech segment detection) into the stream. `true` follows the
    // `vad` settings, passes each polled chunk through the VAD, and fills
    // `FlexChunk::vad_events`.
    bool has_vad;
    // VAD settings (used only when `has_vad` is `true`; ignored when `false`).
    struct FlexVadConfig vad;
} FlexConfig;

// One event finalized by the VAD. Goes into the output array of `flexaudio_vad_process` and
// into `FlexChunk::vad_events`.
//
// `at_sample` counts samples at the VAD internal rate (`sample_rate` = 8000/16000), not input
// samples (same as [`flexaudio_vad::VadEvent`]).
typedef struct FlexVadEvent {
    // Kind. 0 = speech start (SpeechStart), 1 = speech end (SpeechEnd).
    int32_t kind;
    // Sample position of the event (at the VAD internal rate; start is inclusive / end is
    // exclusive).
    int64_t at_sample;
} FlexVadEvent;

// Audio data of one retrieved chunk. Filled by `flexaudio_poll_chunk`.
//
// `data` is interleaved f32 owned by flexaudio, of length `len` (= `frames * channels`).
// Always free it with `flexaudio_chunk_free` when done (do not use C's free).
typedef struct FlexChunk {
    // Pointer to interleaved f32 samples. Free with `flexaudio_chunk_free`.
    float *data;
    // Element count of `data` (= `frames * channels`).
    uintptr_t len;
    // Number of frames in the chunk.
    uint32_t frames;
    // Monotonic presentation timestamp (ns) of the first sample.
    int64_t pts_ns;
    // Monotonically increasing sequence number assigned by the stream layer.
    uint64_t seq;
    // Chunk status flags (ChunkFlags bits).
    uint32_t flags;
    // Number of chunks dropped before this chunk arrived.
    uint32_t dropped_before;
    // Maximum absolute value over all samples (linear amplitude).
    float peak;
    // Root mean square over all samples (linear).
    float rms;
    // Array of events the VAD finalized in this chunk. NULL (`vad_events_len = 0`) when VAD
    // is disabled or there are no events. When non-NULL, `flexaudio_chunk_free` frees it
    // together with `data`.
    struct FlexVadEvent *vad_events;
    // Element count of `vad_events`. 0 when VAD is disabled or there are no events.
    uintptr_t vad_events_len;
} FlexChunk;

// One retrieved event. Filled by `flexaudio_poll_event`.
//
// For `Error`, the message goes into `flexaudio_last_error`.
typedef struct FlexEvent {
    // Event kind.
    enum FlexEventKind kind;
    // Drop count for `ChunkDropped`. 0 otherwise.
    int64_t count;
} FlexEvent;

// Information about one enumerated device (corresponds to [`flexaudio::DeviceInfo`]).
//
// `id` / `name` are UTF-8 NUL-terminated strings owned by flexaudio. Free them together with
// the array via `flexaudio_devices_free` (do not use C's free).
typedef struct FlexDeviceInfo {
    // Stable ID (freed by `flexaudio_devices_free`).
    char *id;
    // Human-readable display name (freed by `flexaudio_devices_free`).
    char *name;
    // Source kind to use when capturing this device.
    enum FlexSourceKind source_kind;
    // Native (default) sample rate (Hz).
    uint32_t sample_rate;
    // Native (default) channel count.
    uint16_t channels;
    // True if this is a loopback (a monitor of the system output).
    bool is_loopback;
    // True if this is the OS default device.
    bool is_default;
} FlexDeviceInfo;

// Information about one enumerated process (corresponds to [`flexaudio::ProcessInfo`]).
//
// Passing `pid` to `FlexConfig::process_id` records that process. The strings are UTF-8
// NUL-terminated and owned by flexaudio; free them together with the array via
// `flexaudio_processes_free` (do not use C's free). `executable` / `bundle_id` are NULL when
// they could not be obtained.
typedef struct FlexProcessInfo {
    // OS process ID (non-zero).
    uint32_t pid;
    // Display name (always non-empty; freed by `flexaudio_processes_free`).
    char *name;
    // Base name of the executable. NULL if it could not be obtained.
    char *executable;
    // macOS bundle ID. NULL if it could not be obtained (always NULL outside macOS).
    char *bundle_id;
    // Whether it is outputting (`Unknown` if not known).
    enum FlexOutputActivity output_activity;
} FlexProcessInfo;

// One retrieved device event. Filled by `flexaudio_watcher_poll`.
//
// Which fields are valid depends on `kind`:
// - `Added`: `id`/`name` and `source_kind`/`sample_rate`/`channels`/`is_loopback`/`is_default`
//   are all filled (complete information about the added device).
// - `Removed`: `id` only (`name` is NULL, numbers are 0).
// - `DefaultChanged`: only `id` and `source_kind` (the side whose default switched).
//
// `id`/`name` are UTF-8 NUL-terminated strings owned by flexaudio and are freed with
// [`flexaudio_device_event_free`] (do not use C's free).
typedef struct FlexDeviceEvent {
    // Event kind.
    enum FlexDeviceEventKind kind;
    // Stable ID (valid for `Added`/`Removed`/`DefaultChanged`; freed by
    // `flexaudio_device_event_free`). NULL for `Unknown`.
    char *id;
    // Display name (`Added` only; freed by `flexaudio_device_event_free`). NULL otherwise.
    char *name;
    // For `Added`, the source kind of the device; for `DefaultChanged`, the side whose
    // default switched (`Mic` = default source / `System` = default sink). Unused otherwise
    // (`Mic`).
    enum FlexSourceKind source_kind;
    // Native sample rate (`Added` only; 0 otherwise).
    uint32_t sample_rate;
    // Native channel count (`Added` only; 0 otherwise).
    uint16_t channels;
    // Loopback (`Added` only).
    bool is_loopback;
    // OS default device (`Added` only).
    bool is_default;
} FlexDeviceEvent;

// Opens a stream from a config (does not start it yet). On failure returns NULL and sets
// last_error.
//
// If `config.denoise` / `config.has_vad` are enabled, the corresponding addons (noise
// suppression / VAD) are built here and live alongside the stream (`poll_chunk` passes chunks
// through denoise → VAD in that order). With denoise enabled, this fails unless the output
// rate is 48000 (NULL + last_error; RNNoise is fixed at 48kHz). Free the returned handle
// with `flexaudio_free`.
//
// # Safety
// `config` must point to a valid `FlexConfig` (NULL is treated as a failure).
struct FlexStream *flexaudio_open(const struct FlexConfig *config);

// Stops the stream and then frees it. NULL-safe.
//
// # Safety
// `s` must be a handle returned by `flexaudio_open` (or NULL).
// `s` must not be used after it is freed.
void flexaudio_free(struct FlexStream *s);

// Starts capture.
//
// # Safety
// `s` must be a valid handle (NULL is InvalidArg).
int32_t flexaudio_start(struct FlexStream *s);

// Stops capture.
//
// # Safety
// `s` must be a valid handle (NULL is InvalidArg).
int32_t flexaudio_stop(struct FlexStream *s);

// Pauses delivery (the device keeps running).
//
// # Safety
// `s` must be a valid handle (NULL is InvalidArg).
int32_t flexaudio_pause(struct FlexStream *s);

// Clears the pause and resumes delivery.
//
// # Safety
// `s` must be a valid handle (NULL is InvalidArg).
int32_t flexaudio_resume(struct FlexStream *s);

// Returns true while paused. Returns false on NULL or panic.
//
// # Safety
// `s` must be a valid handle (or NULL).
bool flexaudio_is_paused(const struct FlexStream *s);

// Changes the input gain (linear multiplier). 1.0 leaves it unchanged, 2.0 is about +6dB, 0.0
// is silence. Can be called at any time during recording and takes effect from the next chunk
// (20ms granularity). Samples after multiplication are clamped to ±1.0. Returns
// FLEX_INVALID_ARG unless the value is finite and at least 0.
//
// # Safety
// `s` must be a valid handle (NULL is InvalidArg).
int32_t flexaudio_set_gain(struct FlexStream *s, float gain);

// Returns the current input gain (linear multiplier). Returns 1.0 on NULL or panic.
//
// # Safety
// `s` must be a valid handle (or NULL).
float flexaudio_gain(const struct FlexStream *s);

// Writes the current backend's native format `(sample_rate, channels)` to `sr`/`ch`.
//
// The value is obtained from the backend at open time and is updated by
// `flexaudio_switch_source`. For display and diagnostics (the output format is the value
// specified in `config`). Returns 0 = success / negative = error.
//
// # Safety
// `s` must be a valid handle and `sr`/`ch` must be valid write targets (NULL is InvalidArg).
int32_t flexaudio_native_format(const struct FlexStream *s, uint32_t *sr, uint16_t *ch);

// Returns the cumulative number of chunks the chunk ring has discarded so far via
// DROP_OLDEST. Returns 0 on NULL or panic.
//
// # Safety
// `s` must be a valid handle (or NULL).
uint64_t flexaudio_dropped_chunks(const struct FlexStream *s);

// Takes out one chunk and fills `out`.
//
// Returns 1 = got one and filled `out` / 0 = none right now / negative = error. `out.data`
// is owned by flexaudio; free it with `flexaudio_chunk_free` when done.
//
// If addons are enabled, the chunk is passed through denoise → VAD in that order before it
// is returned. With VAD enabled, the finalized events go into `out.vad_events` (element count
// `out.vad_events_len`), which `flexaudio_chunk_free` also frees together with `data`
// (NULL/0 when disabled or when there are no events).
//
// # Safety
// `s` must be a valid handle and `out` must be a valid `FlexChunk` write target.
int32_t flexaudio_poll_chunk(struct FlexStream *s, struct FlexChunk *out);

// Frees the `data` filled by `flexaudio_poll_chunk` and sets `data=NULL` / `len=0`.
// Safe for both NULL and double free.
//
// # Safety
// `chunk` must point to a `FlexChunk` filled by `flexaudio_poll_chunk` (or be NULL).
void flexaudio_chunk_free(struct FlexChunk *chunk);

// Takes out one event and fills `out`.
//
// Returns 1 = got one / 0 = none right now / negative = error. For an `Error` event,
// sets `out.kind = Error` and puts the message in last_error.
//
// # Safety
// `s` must be a valid handle and `out` must be a valid `FlexEvent` write target.
int32_t flexaudio_poll_event(struct FlexStream *s, struct FlexEvent *out);

// Hot-swaps the input source without stopping the recording. `config.gain` is ignored
// (gain is stream state; change it with `flexaudio_set_gain`). Likewise `config.denoise` /
// `config.has_vad` / `config.vad` are ignored (the addons keep what was fixed at open time;
// the output format cannot be changed by switch_source, so the 48k constraint and the VAD
// settings stay unchanged).
//
// # Safety
// `s` must be a valid handle and `config` must point to a valid `FlexConfig`.
int32_t flexaudio_switch_source(struct FlexStream *s, const struct FlexConfig *config);

// Enumerates the available devices, allocates an array, and sets `out_array` / `out_count`.
//
// Returns 0 on success. Free the allocated array with `flexaudio_devices_free`. In a headless
// environment, 0 devices (`out_array=NULL` / `out_count=0`) is also treated as success.
//
// # Safety
// `out_array` / `out_count` must be valid write targets (NULL is InvalidArg).
int32_t flexaudio_devices(struct FlexDeviceInfo **out_array, uintptr_t *out_count);

// Frees the array allocated by `flexaudio_devices` and each `id`/`name`. NULL-safe.
//
// # Safety
// `arr`/`count` must be what `flexaudio_devices` returned (or NULL/0).
void flexaudio_devices_free(struct FlexDeviceInfo *arr, uintptr_t count);

// Enumerates the processes that have an audio output session (stream) and can therefore be
// targeted by per-process capture, allocates an array, and sets `out_array` / `out_count`.
// The calling process itself is not included. Stopped and Idle ones are listed too. Whether
// one is playing right now is shown by `output_activity`.
//
// Returns 0 on success. If there are no candidates, it succeeds with 0 entries
// (`out_array=NULL` / `out_count=0`) (per-process capture is usable, but no such process
// exists right now). Free the allocated array **exactly once** with
// `flexaudio_processes_free`. Returns `FLEX_FAILURE` (reason in `flexaudio_last_error`) when
// per-process capture is not usable in this environment (PipeWire unreachable on Linux,
// macOS older than 14.4, not Windows build 20348 or later (Windows 11 / Windows Server 2022),
// unsupported OS, permission denied), when the OS did not respond within 3 seconds, or when
// a previous query has not finished yet.
//
// # Safety
// `out_array` / `out_count` must be valid write targets (NULL is InvalidArg).
int32_t flexaudio_processes(struct FlexProcessInfo **out_array, uintptr_t *out_count);

// Frees the array allocated by `flexaudio_processes` and each string. NULL-safe.
// Call it **exactly once** (calling it twice on the same pointer is a double free =
// undefined behavior).
//
// # Safety
// `arr`/`count` must be what `flexaudio_processes` returned (or NULL/0), and this function
// is called exactly once for the same `arr`.
void flexaudio_processes_free(struct FlexProcessInfo *arr, uintptr_t count);

// Returns the most recent error message for the current thread.
//
// Valid until the next FFI call on the same thread that updates last_error. NULL if there is
// no error. The returned pointer is owned by flexaudio and must not be freed on the C side.
const char *flexaudio_last_error(void);

// Builds a denoiser for the given channel count (1 = mono / 2 = stereo interleaved).
//
// If `channels` is not in 1..=2, returns NULL and sets last_error. Free the returned handle
// with `flexaudio_denoise_free`. See the module docs for the 48kHz assumption.
struct FlexDenoiser *flexaudio_denoise_new(uint16_t channels);

// Applies noise suppression **in place** to interleaved f32 (48kHz, normalized to ±1.0).
//
// `len` must be a multiple of the channel count (InvalidArg otherwise). `len=0` is a no-op.
// The output is the input delayed by 480 samples/ch, and that much at the start of the
// stream is silence. Returns 0 = success / negative = error.
//
// # Safety
// `d` must be a valid handle and `samples` must be a valid mutable array of `len` elements
// (may be NULL if `len=0`).
int32_t flexaudio_denoise_process(struct FlexDenoiser *d, float *samples, uintptr_t len);

// Resets the RNN state, carry-over buffer, and delay line (back to the state right after
// creation).
//
// # Safety
// `d` must be a valid handle (NULL is InvalidArg).
int32_t flexaudio_denoise_reset(struct FlexDenoiser *d);

// Frees a denoiser handle. NULL-safe.
//
// # Safety
// `d` must be a handle returned by `flexaudio_denoise_new` (or NULL).
// `d` must not be used after it is freed.
void flexaudio_denoise_free(struct FlexDenoiser *d);

// Opens FLAC writing to `path`. `split_seconds = 0` means a single file; 1 or more rotates to
// numbered `name-001.flac` files every `split_seconds` seconds.
//
// On failure (NULL / invalid UTF-8 path / unsupported `sr` or `ch`) returns NULL and sets
// last_error. `ch` is 1..=2 and `sr` is 1..=96000 Hz. Free the returned handle with
// `flexaudio_flac_free` (even if it is freed without calling `flexaudio_flac_finalize`, it
// is closed on a best-effort basis).
//
// # Safety
// `path` must point to a valid NUL-terminated C string (UTF-8) (NULL is treated as a
// failure).
struct FlexFlac *flexaudio_flac_create(const char *path,
                                       uint32_t sr,
                                       uint16_t ch,
                                       uint32_t split_seconds);

// Appends interleaved f32 (length = frames × channels).
//
// `len` must be a multiple of the channel count (InvalidArg otherwise). `len=0` is a no-op.
// A write to a finalized handle returns [`FLEX_INVALID_STATE`](code::FLEX_INVALID_STATE).
// Returns 0 = success / negative = error.
//
// # Safety
// `f` must be a valid handle and `samples` must be a valid array of `len` elements (may be
// NULL if `len=0`).
int32_t flexaudio_flac_write(struct FlexFlac *f, const float *samples, uintptr_t len);

// Writes out the remainder, then finalizes and closes the current file. Subsequent writes
// return InvalidState.
//
// A double finalize is safe (a no-op that returns 0). Returns 0 = success / negative = error.
//
// # Safety
// `f` must be a valid handle (NULL is InvalidArg).
int32_t flexaudio_flac_finalize(struct FlexFlac *f);

// Frees a FLAC handle. NULL-safe.
//
// Even when freed without finalize, the inner [`FlacWriter`] tries on drop, on a best-effort
// basis, to write out the remainder and finalize the header (errors are swallowed; to detect
// them reliably, call `flexaudio_flac_finalize` first).
//
// # Safety
// `f` must be a handle returned by `flexaudio_flac_create` (or NULL).
// `f` must not be used after it is freed.
void flexaudio_flac_free(struct FlexFlac *f);

// Builds a VAD from settings. If `config` is NULL, the default settings (silero-compliant)
// are used.
//
// On failure (model load failure, invalid sample_rate, etc.) returns NULL and sets
// last_error. Free the returned handle with `flexaudio_vad_free`.
//
// # Safety
// `config` must be NULL or point to a valid `FlexVadConfig`.
struct FlexVad *flexaudio_vad_new(const struct FlexVadConfig *config);

// Passes samples of any format (interleaved f32 at `in_rate` / `in_ch`) through the VAD,
// allocates an array of the finalized events, and sets `out` / `out_len`.
//
// Internally downmixes to mono and resamples to the VAD rate before processing
// ([`flexaudio_vad::Vad::process_pcm`]). If there are no events, `out=NULL` / `out_len=0`.
// Free the allocated array with `flexaudio_vad_events_free`. Returns 0 = success /
// negative = error.
//
// # Safety
// `v` must be a valid handle, `samples` must be a valid array of `len` elements (may be NULL
// if `len=0`), and `out` / `out_len` must be valid write targets.
int32_t flexaudio_vad_process(struct FlexVad *v,
                              const float *samples,
                              uintptr_t len,
                              uint32_t in_rate,
                              uint16_t in_ch,
                              struct FlexVadEvent **out,
                              uintptr_t *out_len);

// Frees the event array allocated by `flexaudio_vad_process`. NULL / 0 is safe.
//
// # Safety
// `events`/`len` must be what `flexaudio_vad_process` returned (or NULL/0).
void flexaudio_vad_events_free(struct FlexVadEvent *events, uintptr_t len);

// Resets the VAD's state (internal state / context / remainder buffer / resampler).
//
// # Safety
// `v` must be a valid handle (NULL is InvalidArg).
int32_t flexaudio_vad_reset(struct FlexVad *v);

// Frees a VAD handle. NULL-safe.
//
// # Safety
// `v` must be a handle returned by `flexaudio_vad_new` (or NULL).
// `v` must not be used after it is freed.
void flexaudio_vad_free(struct FlexVad *v);

// Starts watching device hotplug and default changes and returns a watcher handle.
//
// On Linux, the PipeWire registry is watched persistently. Without PipeWire or on an
// unsupported OS, it degrades to a no-op and returns a valid handle (hotplug events simply
// never arrive; poll always returns 0). NULL + last_error only on failure. Free the returned
// handle with `flexaudio_watcher_free`.
struct FlexWatcher *flexaudio_watch_devices(void);

// Takes out one device event and fills `out` (non-blocking).
//
// Returns 1 = got one and filled `out` / 0 = none right now / negative = error. Free the
// filled `out` with `flexaudio_device_event_free` when done.
//
// # Safety
// `w` must be a valid handle and `out` must be a valid `FlexDeviceEvent` write target.
int32_t flexaudio_watcher_poll(struct FlexWatcher *w, struct FlexDeviceEvent *out);

// Frees the `id`/`name` filled by `flexaudio_watcher_poll` and sets them to NULL. Safe for
// both NULL and double free.
//
// # Safety
// `ev` must point to a `FlexDeviceEvent` filled by `flexaudio_watcher_poll` (or be NULL).
void flexaudio_device_event_free(struct FlexDeviceEvent *ev);

// Stops and frees the watcher. NULL-safe.
//
// # Safety
// `w` must be a handle returned by `flexaudio_watch_devices` (or NULL).
// `w` must not be used after it is freed.
void flexaudio_watcher_free(struct FlexWatcher *w);

#endif  /* FLEXAUDIO_H */
