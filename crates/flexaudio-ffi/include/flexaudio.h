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

// The handle state does not allow the operation (such as writing to finalized FLAC).
#define FLEX_INVALID_STATE -4

// Audio source kind to record (corresponds to [`flexaudio::SourceKind`]).
typedef enum FlexSourceKind {
    // Microphone input.
    FLEX_SOURCE_KIND_MIC = 0,
    // Loopback of all system output.
    FLEX_SOURCE_KIND_SYSTEM = 1,
    // Output loopback for a specific process.
    FLEX_SOURCE_KIND_PROCESS = 2,
    // Record microphone and system audio mixed into one stream.
    FLEX_SOURCE_KIND_MIX = 3,
} FlexSourceKind;

// Whether to include or exclude the target PID for process sources (corresponds to [`flexaudio::ProcessMode`]).
typedef enum FlexProcessMode {
    // Capture only the target PID (and its process tree).
    FLEX_PROCESS_MODE_INCLUDE = 0,
    // Capture all system audio except the target PID.
    FLEX_PROCESS_MODE_EXCLUDE = 1,
} FlexProcessMode;

// Stream event kind (corresponds to [`flexaudio::Event`]).
typedef enum FlexEventKind {
    // Chunks were dropped because the chunk ring was full (count in `FlexEvent::count`).
    FLEX_EVENT_KIND_CHUNK_DROPPED = 0,
    // Data stopped arriving and the stream stalled.
    FLEX_EVENT_KIND_STALLED = 1,
    // Data resumed after a stall.
    FLEX_EVENT_KIND_RECOVERED = 2,
    // A required permission was denied.
    FLEX_EVENT_KIND_PERMISSION_DENIED = 3,
    // The capture device was lost.
    FLEX_EVENT_KIND_DEVICE_LOST = 4,
    // Other backend error (retrieve the message with `flexaudio_last_error`).
    FLEX_EVENT_KIND_ERROR = 5,
    // Event not matching a known kind (reserved for future variants).
    FLEX_EVENT_KIND_UNKNOWN = 6,
} FlexEventKind;

// Whether the process is currently outputting audio (corresponds to [`flexaudio::ProcessInfo::is_output_active`]).
//
// `Unknown` when the OS does not expose this state (Rust `None`).
typedef enum FlexOutputActivity {
    // The OS does not expose the state or it could not be read.
    FLEX_OUTPUT_ACTIVITY_UNKNOWN = 0,
    // Not outputting (Linux = node is not Running / Windows = session is Inactive /
    // macOS = IsRunningOutput is 0).
    FLEX_OUTPUT_ACTIVITY_INACTIVE = 1,
    // Outputting audio.
    FLEX_OUTPUT_ACTIVITY_ACTIVE = 2,
} FlexOutputActivity;

// Device connection event kind (corresponds to [`flexaudio::DeviceEvent`]).
typedef enum FlexDeviceEventKind {
    // A device was added (`device`/`name`, etc. are populated).
    FLEX_DEVICE_EVENT_KIND_ADDED = 0,
    // A device was removed (`id` only).
    FLEX_DEVICE_EVENT_KIND_REMOVED = 1,
    // The OS default device changed (`id` and `source_kind`).
    FLEX_DEVICE_EVENT_KIND_DEFAULT_CHANGED = 2,
    // An event that does not match a known kind (for future variants).
    FLEX_DEVICE_EVENT_KIND_UNKNOWN = 3,
} FlexDeviceEventKind;

// Opaque noise suppression handle containing [`flexaudio_denoise::Denoiser`].
// Create it with `flexaudio_denoise_new` and release it with `flexaudio_denoise_free`.
typedef struct FlexDenoiser FlexDenoiser;

// Opaque handle for FLAC output. Create it with `flexaudio_flac_create`, append chunks with
// `flexaudio_flac_write`, finalize with `flexaudio_flac_finalize`, and release with `flexaudio_flac_free`.
typedef struct FlexFlac FlexFlac;

// Opaque handle for a recording stream. It contains [`flexaudio::Stream`] and any enabled
// addons (denoise / VAD); C code holds only a pointer. Create with `flexaudio_open` and
// release with `flexaudio_free`.
//
// Keep addons here as part of the stream state (thin wrapper). Before `poll_chunk`
// returns, process data through denoise → VAD. `flexaudio_switch_source` replaces only the source;
// addons retain their configuration from open (same as gain).
typedef struct FlexStream FlexStream;

// Opaque VAD handle containing [`flexaudio_vad::Vad`] (which holds one ONNX session).
// Create it with `flexaudio_vad_new` and free it with `flexaudio_vad_free`.
typedef struct FlexVad FlexVad;

// Opaque device watcher handle containing [`flexaudio::DeviceWatcher`]. Create with
// `flexaudio_watch_devices` and release with `flexaudio_watcher_free`.
typedef struct FlexWatcher FlexWatcher;

// VAD (voice activity detection) configuration. Passed to `FlexConfig::vad` and `flexaudio_vad_new`.
//
// For each value, the sentinel 0 selects the default from [`flexaudio_vad::VadConfig`].
// `threshold` 0 → 0.5, `neg_threshold` 0 → Silero formula `max(threshold-0.15, 0.01)`,
// `min_speech_ms` 0 → 250, `min_silence_ms` 0 → 100, `speech_pad_ms` 0 → 30,
// `sample_rate` 0 → 16000. `max_speech_ms` 0 means unlimited (the default).
//
// [`flexaudio_vad_new`]: crate::flexaudio_vad_new
typedef struct FlexVadConfig {
    // Probability threshold (>=) for speech start. 0 selects 0.5.
    float threshold;
    // Lower (silence-side) threshold (<) for silence start.
    // 0 selects the Silero formula automatically.
    float neg_threshold;
    // Minimum accepted speech duration (ms). Shorter segments are discarded. 0 selects 250.
    uint32_t min_speech_ms;
    // Silence duration (ms) required to finalize speech end. 0 selects 100.
    uint32_t min_silence_ms;
    // Padding (ms) added before and after segment boundaries. 0 selects 30.
    uint32_t speech_pad_ms;
    // Maximum segment duration (ms). 0 is unlimited (default). Longer segments are forcibly split.
    uint32_t max_speech_ms;
    // Sample rate (8000 or 16000). 0 selects 16000.
    uint32_t sample_rate;
} FlexVadConfig;

// Configuration for opening a stream. Passed to `flexaudio_open` / `flexaudio_switch_source`.
//
// Sentinel values mean "unspecified" for strings and optional values (`device_id` NULL selects the default device,
// `process_id` 0 means none, and `output_rate`/`output_channels`/`chunk_ms` 0 select defaults).
typedef struct FlexConfig {
    // Source kind.
    enum FlexSourceKind kind;
    // ID of the selected device (UTF-8, NUL-terminated). NULL selects the default device.
    const char *device_id;
    // Target PID for a process source. 0 means none (may cause an error when starting a process source).
    uint32_t process_id;
    // Whether to include or exclude the target PID (process sources only).
    enum FlexProcessMode mode;
    // Whether to exclude this process's playback from system audio (system source only;
    // for mix, applies to the system side).
    bool exclude_self;
    // Output sample rate (Hz). 0 selects 48000.
    uint32_t output_rate;
    // Output channel count. 0 selects 2.
    uint16_t output_channels;
    // Chunk duration (ms). 0 selects 20.
    uint32_t chunk_ms;
    // Input gain at start (linear multiplier). 0 selects 1.0 (default). For runtime mute, use
    // `flexaudio_set_gain(s, 0.0)`.
    float gain;
    // Input device ID (UTF-8, NUL-terminated) for the mic side of mix (mix only).
    // NULL selects the default input.
    const char *mix_mic_device_id;
    // Output endpoint ID (UTF-8, NUL-terminated) for the system side of mix (mix only).
    // NULL selects the default output.
    const char *mix_system_device_id;
    // Pre-mix linear gain for the mic side of mix (mix only). 0 selects 1.0 (default).
    // Global `gain` is applied after mixing.
    float mix_mic_gain;
    // Pre-mix linear gain for the system side of mix (mix only). 0 selects 1.0 (default).
    float mix_system_gain;
    // Whether to apply noise suppression (RNNoise) to the stream. Enabled when `true`. The output rate must be
    // 48000 or `flexaudio_open` fails (NULL + last_error). denoise
    // processes data in place just before `poll_chunk` returns (before VAD).
    bool denoise;
    // Whether to apply VAD (voice activity detection) to the stream. When `true`, each polled chunk is processed
    // by VAD according to `vad` and populates `FlexChunk::vad_events`.
    bool has_vad;
    // VAD configuration (used only when `has_vad` is `true`; ignored when `false`).
    struct FlexVadConfig vad;
} FlexConfig;

// One event finalized by VAD. Stored in the output array of `flexaudio_vad_process` and in `FlexChunk::vad_events`
//.
//
// `at_sample` is measured at the internal VAD rate (`sample_rate` = 8000/16000), not at the input
// sample rate (same as [`flexaudio_vad::VadEvent`]).
typedef struct FlexVadEvent {
    // Kind. 0 = speech start (SpeechStart), 1 = speech end (SpeechEnd).
    int32_t kind;
    // Event sample position at the internal VAD rate (start inclusive / end exclusive).
    int64_t at_sample;
} FlexVadEvent;

// One captured audio chunk, populated by `flexaudio_poll_chunk`.
//
// `data` is flexaudio-owned interleaved f32 with length `len` (= `frames * channels`).
// Always release it with `flexaudio_chunk_free` when finished (do not use C `free`).
typedef struct FlexChunk {
    // Pointer to interleaved f32 samples. Release with `flexaudio_chunk_free`.
    float *data;
    // Number of elements in `data` (= `frames * channels`).
    uintptr_t len;
    // Number of frames in the chunk.
    uint32_t frames;
    // Monotonic presentation timestamp (ns) of the first sample.
    int64_t pts_ns;
    // Monotonically increasing sequence number assigned by the stream layer.
    uint64_t seq;
    // Chunk state flags (ChunkFlags bits).
    uint32_t flags;
    // Number of chunks dropped before this chunk arrived.
    uint32_t dropped_before;
    // Maximum absolute sample value (linear amplitude).
    float peak;
    // Root-mean-square value of all samples (linear).
    float rms;
    // Events finalized by VAD for this chunk. NULL when VAD is disabled or there are no events
    // (`vad_events_len = 0`). When non-NULL, `flexaudio_chunk_free` releases it
    // together with `data`.
    struct FlexVadEvent *vad_events;
    // Number of `vad_events`. 0 when VAD is disabled or there are no events.
    uintptr_t vad_events_len;
} FlexChunk;

// One captured event, populated by `flexaudio_poll_event`.
//
// For `Error`, the message is stored in `flexaudio_last_error`.
typedef struct FlexEvent {
    // Event kind.
    enum FlexEventKind kind;
    // Number dropped for `ChunkDropped`; 0 for other kinds.
    int64_t count;
} FlexEvent;

// Information for one enumerated device (corresponds to [`flexaudio::DeviceInfo`]).
//
// `id` / `name` are flexaudio-owned, NUL-terminated UTF-8 strings. Release the entire array with
// `flexaudio_devices_free` (do not use C `free`).
typedef struct FlexDeviceInfo {
    // Stable ID (released by `flexaudio_devices_free`).
    char *id;
    // Human-readable display name (released by `flexaudio_devices_free`).
    char *name;
    // Source kind used to capture this device.
    enum FlexSourceKind source_kind;
    // Native (default) sample rate (Hz).
    uint32_t sample_rate;
    // Native (default) channel count.
    uint16_t channels;
    // True for loopback (system-output monitor).
    bool is_loopback;
    // True if this is the OS default device.
    bool is_default;
} FlexDeviceInfo;

// Information for one enumerated process (corresponds to [`flexaudio::ProcessInfo`]).
//
// Pass `pid` to `FlexConfig::process_id` to capture that process. Strings are flexaudio-owned,
// NUL-terminated UTF-8 and released with the entire array by `flexaudio_processes_free` (do not use C `free`).
// `executable` / `bundle_id` are NULL when unavailable.
typedef struct FlexProcessInfo {
    // OS process ID (nonzero).
    uint32_t pid;
    // Display name (never empty; released by `flexaudio_processes_free`).
    char *name;
    // Executable basename, or NULL if unavailable.
    char *executable;
    // macOS bundle ID, or NULL if unavailable (always NULL outside macOS).
    char *bundle_id;
    // Whether it is outputting audio (`Unknown` if unavailable).
    enum FlexOutputActivity output_activity;
} FlexProcessInfo;

// One retrieved device event, populated by `flexaudio_watcher_poll`.
//
// Valid fields depend on `kind`:
// - `Added`: all of `id`/`name` and `source_kind`/`sample_rate`/`channels`/`is_loopback`/`is_default`
//   are populated (full details for the added device).
// - `Removed`: only `id` (`name` is NULL and numeric fields are 0).
// - `DefaultChanged`: only `id` and `source_kind` (the side whose default changed).
//
// `id`/`name` are UTF-8 NUL-terminated strings owned by flexaudio; release them with
// [`flexaudio_device_event_free`] (do not use C `free`).
typedef struct FlexDeviceEvent {
    // Event kind.
    enum FlexDeviceEventKind kind;
    // Stable ID (valid for `Added`/`Removed`/`DefaultChanged`; release with
    // `flexaudio_device_event_free`). NULL for `Unknown`.
    char *id;
    // Display name (`Added` only; release with `flexaudio_device_event_free`). NULL for other kinds.
    char *name;
    // For `Added`, the device source kind. For `DefaultChanged`, the side whose default changed
    // (`Mic` = default source / `System` = default sink). Unused for other kinds (`Mic`).
    enum FlexSourceKind source_kind;
    // Native sample rate (`Added` only; 0 otherwise).
    uint32_t sample_rate;
    // Native channel count (`Added` only; 0 otherwise).
    uint16_t channels;
    // Whether this is loopback (`Added` only).
    bool is_loopback;
    // Whether this is the OS default device (`Added` only).
    bool is_default;
} FlexDeviceEvent;

// Open a stream from the configuration (without starting it). On failure, return NULL and
// set last_error.
//
// If `config.denoise` / `config.has_vad` is enabled, build the corresponding add-ons (noise
// suppression / VAD) here and attach them to the stream (`poll_chunk` applies denoise → VAD).
// When denoise is enabled, the output rate must be 48000 (otherwise return NULL and set
// last_error; RNNoise requires 48 kHz). Free the returned handle with `flexaudio_free`.
// This is equivalent to `flexaudio_open_with_exclude_pids(config, NULL, 0)`.
//
// # Safety
// `config` must point to a valid `FlexConfig` (NULL is treated as a failure).
struct FlexStream *flexaudio_open(const struct FlexConfig *config);

// Open a stream with additional PIDs excluded from system capture (or the system side of
// mix), combined with `config.exclude_self`. Mic and process sources ignore valid lists.
// The stream is not started; free the returned handle with `flexaudio_free`.
//
// Every PID must be in 1..=4294967295, even for sources that ignore the list. At most 4096
// entries are accepted. Order and duplicates are preserved. The list is copied before
// returning; the caller may release or change the array after this call.
// On Windows, all listed PIDs must equal the process tree root (this process when
// `exclude_self` is true, otherwise the first listed PID). macOS resolves PIDs once at
// start; Linux matches exact PIDs without their child processes.
//
// Returns NULL and sets `flexaudio_last_error` on failure, including a NULL pointer with
// nonzero length, a misaligned PID pointer, an excessive length, or a zero PID (the message
// names its index). A zero length never dereferences `exclude_pids` and permits NULL.
// Add-on settings and output defaults follow `flexaudio_open`.
//
// # Safety
// `config` must point to a valid `FlexConfig` with valid NUL-terminated string fields or NULL.
// For a nonempty list of at most 4096 entries, `exclude_pids` must point to that many
// initialized uint32_t values in one allocation, readable and unchanged during this call.
// NULL and misaligned pointers are rejected before dereferencing.
struct FlexStream *flexaudio_open_with_exclude_pids(const struct FlexConfig *config,
                                                    const uint32_t *exclude_pids,
                                                    uintptr_t exclude_pids_len);

// Stop the stream, then free it. NULL-safe.
//
// # Safety
// `s` must be a handle returned by `flexaudio_open` (or NULL). Do not use `s` after freeing it.
void flexaudio_free(struct FlexStream *s);

// Start capture.
//
// # Safety
// `s` must be a valid handle (NULL is InvalidArg).
int32_t flexaudio_start(struct FlexStream *s);

// Stop capture.
//
// # Safety
// `s` must be a valid handle (NULL is InvalidArg).
int32_t flexaudio_stop(struct FlexStream *s);

// Pause delivery while leaving the device running.
//
// # Safety
// `s` must be a valid handle (NULL is InvalidArg).
int32_t flexaudio_pause(struct FlexStream *s);

// Resume delivery after a pause.
//
// # Safety
// `s` must be a valid handle (NULL is InvalidArg).
int32_t flexaudio_resume(struct FlexStream *s);

// Return true if paused. Return false for NULL or on panic.
//
// # Safety
// `s` must be a valid handle (or NULL).
bool flexaudio_is_paused(const struct FlexStream *s);

// Change the input gain (linear multiplier): 1.0 leaves it unchanged, 2.0 is about +6 dB,
// and 0.0 is silent. Can be called during recording; it takes effect on the next chunk (20 ms
// granularity). Samples are clamped to ±1.0 after multiplication. Must be finite and at least
// 0, or FLEX_INVALID_ARG is returned.
//
// # Safety
// `s` must be a valid handle (NULL is InvalidArg).
int32_t flexaudio_set_gain(struct FlexStream *s, float gain);

// Return the current input gain (linear multiplier). Return 1.0 for NULL or on panic.
//
// # Safety
// `s` must be a valid handle (or NULL).
float flexaudio_gain(const struct FlexStream *s);

// Write the current backend's native format `(sample_rate, channels)` to `sr` / `ch`.
//
// These values come from the backend at open and are updated by `flexaudio_switch_source`.
// They are for display / diagnostics; the output format is set in `config`. Return 0 on
// success and a negative value on error.
//
// # Safety
// `s` must be a valid handle, and `sr` / `ch` must be valid output pointers (NULL is InvalidArg).
int32_t flexaudio_native_format(const struct FlexStream *s, uint32_t *sr, uint16_t *ch);

// Return the total number of chunks dropped by the chunk ring using DROP_OLDEST. Return 0 for
// NULL or on panic.
//
// # Safety
// `s` must be a valid handle (or NULL).
uint64_t flexaudio_dropped_chunks(const struct FlexStream *s);

// Retrieve one chunk and fill `out`.
//
// Return 1 when a chunk is retrieved and `out` is filled, 0 when none is available, or a
// negative value on error. `out.data` is owned by flexaudio; free it with
// `flexaudio_chunk_free` when done.
//
// If add-ons are enabled, the chunk passes through denoise → VAD before it is returned. When
// VAD is enabled, confirmed events are stored in `out.vad_events` (with count
// `out.vad_events_len`) and freed along with `data` by `flexaudio_chunk_free` (NULL/0 when
// disabled or when there are no events).
//
// # Safety
// `s` must be a valid handle, and `out` must point to a valid `FlexChunk` destination.
int32_t flexaudio_poll_chunk(struct FlexStream *s, struct FlexChunk *out);

// Free the `data` filled by `flexaudio_poll_chunk` and set `data=NULL` / `len=0`.
// Safe for NULL and repeated calls.
//
// # Safety
// `chunk` must point to a `FlexChunk` filled by `flexaudio_poll_chunk` (or be NULL).
void flexaudio_chunk_free(struct FlexChunk *chunk);

// Retrieve one event and fill `out`.
//
// Return 1 when an event is retrieved, 0 when none is available, or a negative value on error.
// For an `Error` event, set `out.kind = Error` and store the message in last_error.
//
// # Safety
// `s` must be a valid handle, and `out` must point to a valid `FlexEvent` destination.
int32_t flexaudio_poll_event(struct FlexStream *s, struct FlexEvent *out);

// Hot-swap the input source without stopping capture. `config.gain` is ignored because gain is
// stream state; change it with `flexaudio_set_gain`. `config.denoise` / `config.has_vad` /
// `config.vad` are also ignored because the add-ons configured at open remain in use. Since
// `switch_source` cannot change the output format, the 48 kHz constraint and VAD settings do
// not change.
// This is equivalent to `flexaudio_switch_source_with_exclude_pids(s, config, NULL, 0)`.
//
// # Safety
// `s` must be a valid handle, and `config` must point to a valid `FlexConfig`.
int32_t flexaudio_switch_source(struct FlexStream *s, const struct FlexConfig *config);

// Hot-swap the source with additional PIDs excluded from system capture (or the system
// side of mix), combined with `config.exclude_self`. Mic and process sources ignore valid
// lists. Gain, add-ons, and output format follow `flexaudio_switch_source`.
//
// Every PID must be in 1..=4294967295 and at most 4096 entries are accepted. Order and
// duplicates are preserved. The list is copied before returning; the caller may release or
// change the array after this call. Platform exclusion rules follow
// `flexaudio_open_with_exclude_pids`.
//
// Returns FLEX_OK on success, FLEX_INVALID_ARG for invalid arguments, or another negative
// error code on failure, and sets `flexaudio_last_error`. A NULL pointer with nonzero length,
// a misaligned PID pointer, an excessive length, or a zero PID is invalid; zero-PID messages
// name the offending index. A zero length never dereferences `exclude_pids` and permits NULL.
// Invalid lists are rejected before replacing the source or accessing devices.
//
// # Safety
// `s` must be a valid stream handle and `config` must point to a valid `FlexConfig` with valid
// NUL-terminated string fields or NULL. For a nonempty list of at most 4096 entries,
// `exclude_pids` must point to that many initialized uint32_t values in one allocation,
// readable and unchanged during this call. NULL and misaligned pointers are rejected before
// dereferencing.
int32_t flexaudio_switch_source_with_exclude_pids(struct FlexStream *s,
                                                  const struct FlexConfig *config,
                                                  const uint32_t *exclude_pids,
                                                  uintptr_t exclude_pids_len);

// List available devices, allocate an array, and set `out_array` / `out_count`.
//
// Return 0 on success. Free the allocated array with `flexaudio_devices_free`. In a headless
// environment, an empty result (`out_array=NULL` / `out_count=0`) is still successful.
//
// # Safety
// `out_array` / `out_count` must be valid output pointers (NULL is InvalidArg).
int32_t flexaudio_devices(struct FlexDeviceInfo **out_array, uintptr_t *out_count);

// Free the array allocated by `flexaudio_devices` and each `id` / `name`. NULL-safe.
//
// # Safety
// `arr` / `count` must be values returned by `flexaudio_devices` (or NULL/0).
void flexaudio_devices_free(struct FlexDeviceInfo *arr, uintptr_t count);

// List processes with audio output sessions (streams) that can be captured individually,
// allocate an array, and set `out_array` / `out_count`. The calling process is excluded.
// Processes whose audio sessions are stopped or idle are also included. Check `output_activity`
// to see whether a process is currently producing audio.
//
// Return 0 on success. If there are no candidates, an empty result (`out_array=NULL` /
// `out_count=0`) is still successful: per-process capture is available, but there are no
// matching processes now. Free the allocated array **once only** with
// `flexaudio_processes_free`. Return `FLEX_FAILURE` (with the reason in `flexaudio_last_error`)
// if per-process capture is unavailable in this environment (PipeWire is unreachable on
// Linux; macOS is earlier than 14.4; Windows is older than build 20348 (Windows 11 / Windows
// Server 2022 or later is required); the OS is unsupported; or access is denied), if the OS does
// not respond within 3 seconds, or if the previous query is still running.
//
// # Safety
// `out_array` / `out_count` must be valid output pointers (NULL is InvalidArg).
int32_t flexaudio_processes(struct FlexProcessInfo **out_array, uintptr_t *out_count);

// Free the array allocated by `flexaudio_processes` and each string. NULL-safe.
// Call **once only**; calling twice with the same pointer causes a double-free and undefined
// behavior.
//
// # Safety
// `arr` / `count` must be values returned by `flexaudio_processes` (or NULL/0). Call this
// function only once for the same `arr`.
void flexaudio_processes_free(struct FlexProcessInfo *arr, uintptr_t count);

// Return the most recent error message for the current thread.
//
// Valid until the next FFI call on the same thread updates last_error. Returns NULL if there
// is no error. The returned pointer is owned by flexaudio; do not free it from C.
const char *flexaudio_last_error(void);

// Creates a denoiser for the given channel count (1 = mono, 2 = interleaved stereo).
//
// Returns NULL and sets last_error if `channels` is outside 1..=2. Release the returned
// handle with `flexaudio_denoise_free`. See the module docs for the 48 kHz requirement.
struct FlexDenoiser *flexaudio_denoise_new(uint16_t channels);

// Suppresses noise in interleaved f32 samples (48 kHz, normalized to ±1.0) **in place**.
//
// `len` must be a multiple of the channel count (otherwise InvalidArg). `len=0` is a
// no-op. Output is delayed by 480 samples per channel, so the beginning is silent.
// Returns 0 on success and a negative value on error.
//
// # Safety
// `d` must be a valid handle. `samples` must point to a valid mutable array of `len`
// elements; NULL is allowed when `len=0`.
int32_t flexaudio_denoise_process(struct FlexDenoiser *d, float *samples, uintptr_t len);

// Resets the RNN state, carry buffer, and delay line to their initial state.
//
// # Safety
// `d` must be a valid handle (NULL is InvalidArg).
int32_t flexaudio_denoise_reset(struct FlexDenoiser *d);

// Releases a denoiser handle. NULL is safe.
//
// # Safety
// `d` must be a handle returned by `flexaudio_denoise_new`, or NULL. Do not use `d`
// after releasing it.
void flexaudio_denoise_free(struct FlexDenoiser *d);

// Open FLAC output at `path`. `split_seconds = 0` creates one file; values of 1 or more rotate to
// numbered files such as `name-001.flac` every `split_seconds` seconds.
//
// On failure (NULL, invalid UTF-8 path, unsupported `sr` or `ch`), return NULL and set last_error.
// `ch` must be 1..=2 and `sr` must be 1..=96000 Hz. Release the returned handle with
// `flexaudio_flac_free` (free without `flexaudio_flac_finalize` still attempts a best-effort close).
//
// # Safety
// `path` must point to a valid NUL-terminated UTF-8 C string (NULL is treated as failure).
struct FlexFlac *flexaudio_flac_create(const char *path,
                                       uint32_t sr,
                                       uint16_t ch,
                                       uint32_t split_seconds);

// Append interleaved f32 (length = frame count × channel count).
//
// `len` must be a multiple of the channel count (otherwise InvalidArg). `len=0` is a no-op.
// Writing to a finalized handle returns [`FLEX_INVALID_STATE`](code::FLEX_INVALID_STATE).
// Returns 0 on success and a negative value on error.
//
// # Safety
// `f` must be a valid handle and `samples` a valid array of `len` elements (NULL is allowed when `len=0`).
int32_t flexaudio_flac_write(struct FlexFlac *f,
                             const float *samples,
                             uintptr_t len);

// Write any remaining data, finalize and close the current file. Further writes return InvalidState.
//
// Calling finalize more than once is safe (no-op returning 0). Returns 0 on success and a negative value on error.
//
// # Safety
// `f` must be a valid handle (NULL is InvalidArg).
int32_t flexaudio_flac_finalize(struct FlexFlac *f);

// Release a FLAC handle. NULL-safe.
//
// If freed without finalize, the internal [`FlacWriter`] still makes a best-effort attempt to
// write remaining data and finalize the header on drop (errors are swallowed; call
// `flexaudio_flac_finalize` first to detect them reliably).
//
// # Safety
// `f` must be a handle returned by `flexaudio_flac_create` (or NULL).
// Do not use `f` after release.
void flexaudio_flac_free(struct FlexFlac *f);

// Create a VAD from a config. A NULL `config` uses the defaults (Silero-compatible).
//
// Returns NULL and sets last_error on failure (model load failure, invalid sample_rate, etc.).
// Free the returned handle with `flexaudio_vad_free`.
//
// # Safety
// `config` must be NULL or point to a valid `FlexVadConfig`.
struct FlexVad *flexaudio_vad_new(const struct FlexVadConfig *config);

// Process samples in any format (`in_rate` / `in_ch`, interleaved f32) through VAD,
// allocate the confirmed event array, and set `out` / `out_len`.
//
// Internally, convert to mono and resample to the VAD rate before processing ([`flexaudio_vad::Vad::process_pcm`]).
// If there are no events, set `out=NULL` / `out_len=0`. Free the allocated array with
// `flexaudio_vad_events_free`. Returns 0 on success or a negative value on error.
//
// # Safety
// `v` must be a valid handle; `samples` must be a valid array of `len` elements (NULL is allowed when `len=0`);
// `out` / `out_len` must point to valid writable locations.
int32_t flexaudio_vad_process(struct FlexVad *v,
                              const float *samples,
                              uintptr_t len,
                              uint32_t in_rate,
                              uint16_t in_ch,
                              struct FlexVadEvent **out,
                              uintptr_t *out_len);

// Free an event array allocated by `flexaudio_vad_process`. NULL / 0 is safe.
//
// # Safety
// `events` / `len` must come from `flexaudio_vad_process` (or be NULL / 0).
void flexaudio_vad_events_free(struct FlexVadEvent *events, uintptr_t len);

// Reset VAD state (internal state / context / remainder buffer / resampler).
//
// # Safety
// `v` must be a valid handle (NULL is InvalidArg).
int32_t flexaudio_vad_reset(struct FlexVad *v);

// Free a VAD handle. NULL is safe.
//
// # Safety
// `v` must be a handle returned by `flexaudio_vad_new` (or NULL).
// Do not use `v` after freeing it.
void flexaudio_vad_free(struct FlexVad *v);

// Start monitoring device connection and default changes, then return a watcher handle.
//
// On Linux, continuously monitor the PipeWire registry. If PipeWire is unavailable or the OS is
// unsupported, degrade to a no-op and return a valid handle (no device events arrive; poll always
// returns 0). Only failures return NULL + last_error. Release the returned handle with
// `flexaudio_watcher_free`.
struct FlexWatcher *flexaudio_watch_devices(void);

// Retrieve one device event into `out` (non-blocking).
//
// Return 1 = retrieved and filled `out` / 0 = none currently available / negative = error. Release
// a populated `out` with `flexaudio_device_event_free` when finished.
//
// # Safety
// `w` must be a valid handle and `out` must point to a valid `FlexDeviceEvent` destination.
int32_t flexaudio_watcher_poll(struct FlexWatcher *w, struct FlexDeviceEvent *out);

// Release `id`/`name` populated by `flexaudio_watcher_poll` and set them to NULL. Safe for NULL and
// repeated calls.
//
// # Safety
// `ev` must point to a `FlexDeviceEvent` populated by `flexaudio_watcher_poll`, or be NULL.
void flexaudio_device_event_free(struct FlexDeviceEvent *ev);

// Stop and release the watcher. NULL-safe.
//
// # Safety
// `w` must be a handle returned by `flexaudio_watch_devices`, or NULL. Do not use `w` after release.
void flexaudio_watcher_free(struct FlexWatcher *w);

#endif  /* FLEXAUDIO_H */
