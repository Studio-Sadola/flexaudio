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
#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>

#define FLEX_CAP_OTHER 4294967295

#define FLEX_CAP_SUPPORTED 1

#define FLEX_CAP_UNAVAILABLE 2

#define FLEX_CAP_UNKNOWN 3

#define FLEX_CAP_REASON_NONE 0

#define FLEX_CAP_UNSUPPORTED_OS 1

#define FLEX_CAP_OS_VERSION_TOO_OLD 2

#define FLEX_CAP_PIPEWIRE_UNAVAILABLE 3

#define FLEX_CAP_NOT_APPLICABLE 4

#define FLEX_BACKEND_CPAL 1

#define FLEX_BACKEND_PIPEWIRE 2

#define FLEX_BACKEND_WASAPI_LOOPBACK 3

#define FLEX_BACKEND_WASAPI_PROCESS_LOOPBACK 4

#define FLEX_BACKEND_CORE_AUDIO_PROCESS_TAP 5

#define FLEX_BACKEND_COMPOSITE 6

#define FLEX_BACKEND_UNSUPPORTED 7

#define FLEX_PID_EXACT 1

#define FLEX_PID_PROCESS_TREE 2

#define FLEX_PID_NOT_APPLICABLE 3

#define FLEX_DEVICE_SELECTION_HONORED 1

#define FLEX_DEVICE_SELECTION_IGNORED 2

#define FLEX_DEVICE_SELECTION_NOT_APPLICABLE 3

#define FLEX_WATCHER_NATIVE 1

#define FLEX_WATCHER_NOOP 2

#define FLEX_WATCH_SYSTEM_OUTPUTS 1

#define FLEX_WATCH_DEFAULT_SYSTEM_OUTPUT 2

#define FLEX_SIDE_EFFECT_SILENCE_PROBE 1

#define FLEX_SIDE_EFFECT_CLASSIC_LOOPBACK_KEEPALIVE 2

#define FLEX_CONDITION_SILENT_ACTIVE_SYSTEM_AUDIO 1

#define FLEX_CONDITION_CLASSIC_LOOPBACK_WITHOUT_EXCLUSIONS 2

// Strict validation rejected ignored capture settings.
#define FLEX_IGNORED_SETTINGS -13

// Device lookup completed without a match.
#define FLEX_DEVICE_NOT_FOUND -5

// Active device lost.
#define FLEX_DEVICE_LOST -6

// Recording permission denied.
#define FLEX_PERMISSION_DENIED -7

// Unsupported OS version.
#define FLEX_UNSUPPORTED_OS_VERSION -8

// Unsupported operation.
#define FLEX_UNSUPPORTED -9

// Unsupported PCM format.
#define FLEX_UNSUPPORTED_FORMAT -10

// Negotiated native format changed.
#define FLEX_NATIVE_FORMAT_CHANGED -11

// Device selection is ambiguous.
#define FLEX_AMBIGUOUS_DEVICE_NAME -12

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

#define FLEX_SOURCE_KIND_MIC 0

#define FLEX_SOURCE_KIND_SYSTEM 1

#define FLEX_SOURCE_KIND_PROCESS 2

#define FLEX_SOURCE_KIND_MIX 3

#define FLEX_PROCESS_MODE_INCLUDE 0

#define FLEX_PROCESS_MODE_EXCLUDE 1

#define FLEX_EVENT_KIND_CHUNK_DROPPED 0

#define FLEX_EVENT_KIND_STALLED 1

#define FLEX_EVENT_KIND_RECOVERED 2

#define FLEX_EVENT_KIND_PERMISSION_DENIED 3

#define FLEX_EVENT_KIND_DEVICE_LOST 4

#define FLEX_EVENT_KIND_ERROR 5

#define FLEX_EVENT_KIND_UNKNOWN 6

#define FLEX_EVENT_KIND_SILENCE_WHILE_SOURCE_ACTIVE 7

#define FLEX_EVENT_KIND_PERMISSION_PENDING 8

#define FLEX_OUTPUT_ACTIVITY_UNKNOWN 0

#define FLEX_OUTPUT_ACTIVITY_INACTIVE 1

#define FLEX_OUTPUT_ACTIVITY_ACTIVE 2

#define FLEX_DEVICE_EVENT_KIND_ADDED 0

#define FLEX_DEVICE_EVENT_KIND_REMOVED 1

#define FLEX_DEVICE_EVENT_KIND_DEFAULT_CHANGED 2

#define FLEX_DEVICE_EVENT_KIND_UNKNOWN 3

#define FLEX_LIFECYCLE_UNKNOWN 0

#define FLEX_LIFECYCLE_SILENCE_PROBE_STARTED 1

#define FLEX_LIFECYCLE_SILENCE_PROBE_STOPPED 2

#define FLEX_LIFECYCLE_CLASSIC_LOOPBACK_KEEPALIVE_STARTED 3

#define FLEX_LIFECYCLE_CLASSIC_LOOPBACK_KEEPALIVE_STOPPED 4

// Event kind 9 is reserved and must not be reused.
// Validated interval sample loss.
#define FLEX_EVENT_KIND_AUDIO_LOSS 10

// Cleanup failure; does not replace the capture primary.
#define FLEX_EVENT_KIND_SHUTDOWN_ERROR 11

// Fatal typed capture failure.
#define FLEX_EVENT_KIND_TERMINAL_ERROR 12

// Advisory typed failure; capture may continue.
#define FLEX_EVENT_KIND_RECOVERABLE_ERROR 13

// Coalesced upstream clipping, without exact chunk attribution.
#define FLEX_EVENT_KIND_CLIPPED 14

// The default endpoint no longer exists; no fabricated device ID.
#define FLEX_DEVICE_EVENT_KIND_DEFAULT_CLEARED 4

// Incremental inventory is invalid; obtain a complete inventory again.
#define FLEX_DEVICE_EVENT_KIND_RESCAN_REQUIRED 5

// Leading invalid microphone frames suppressed; v2 accessor gives frames/reason.
// Enqueued chronologically before publishing the generation's first chunk.
// Event-first callers can observe it before PCM; PCM polling does not wait for event polling.
#define FLEX_EVENT_KIND_STARTUP_SAMPLES_DROPPED 15

// Startup validation saw non-finite or out-of-range microphone samples.
#define FLEX_STARTUP_DROP_OUT_OF_RANGE 0

// Device monitoring terminated; the v2 error accessor returns the retained failure.
#define FLEX_DEVICE_EVENT_KIND_TERMINAL_ERROR 6

#define FLEX_STREAM_VERSION_3 3

#define FLEX_SECONDARY_F32 1

#define FLEX_SECONDARY_S16 2

#define FLEX_KEEPALIVE_DEFAULT 0

#define FLEX_KEEPALIVE_ON 1

#define FLEX_KEEPALIVE_OFF 2

#define FLEX_STREAM_VERSION_4 4

#define FLEX_SETTING_NOT_REQUESTED 0

#define FLEX_SETTING_APPLIED 1

#define FLEX_SETTING_IGNORED 2

#define FLEX_SETTING_ADJUSTED 3

#define FLEX_STREAM_VERSION_5 5

#define FLEX_STREAM_VERSION_2 2

#define FLEX_VAD_PRIMARY 1

#define FLEX_VAD_SECONDARY 2

#define FLEX_VAD_UNSUPPORTED_TAP -100

#define FLEX_VAD_UNSUPPORTED_CONVERSION_CLOCK -101

#define FLEX_VAD_EPOCH_START 6

#define FLEX_VAD_SEGMENT 1

#define FLEX_VAD_SPEECH_START 2

#define FLEX_VAD_SPEECH_END 3

#define FLEX_VAD_CUT 4

#define FLEX_VAD_EPOCH_END 5

#define FLEX_VAD_HYSTERESIS 1

#define FLEX_VAD_FINISH 2

#define FLEX_VAD_RESET 3

#define FLEX_VAD_ERROR 4

#define FLEX_VAD_LIMIT 5

typedef enum FlexValueKindV4 {
    FLEX_VALUE_ABSENT = 0,
    FLEX_VALUE_BOOL = 1,
    FLEX_VALUE_UNSIGNED = 2,
    FLEX_VALUE_FLOAT = 3,
    FLEX_VALUE_TEXT = 4,
    FLEX_VALUE_PIDS = 5,
    FLEX_VALUE_FORMAT = 6,
} FlexValueKindV4;

// Owned immutable platform snapshot. Accessor views and errors live until capabilities_free_v1.
typedef struct FlexCapabilitiesV1 FlexCapabilitiesV1;

// Opaque noise suppression handle (`FlexDenoiser`).
// Create it with `flexaudio_denoise_new` and release it with `flexaudio_denoise_free`.
typedef struct FlexDenoiser FlexDenoiser;

// Owned watcher event. Device and string views are borrowed until explicit free.
typedef struct FlexDeviceEventV2 FlexDeviceEventV2;

// Owned immutable error tree. Borrowed views live until this tree's explicit free.
typedef struct FlexErrorInfoV2 FlexErrorInfoV2;

// Owned event with immutable payloads. Free with flexaudio_event_free_v2.
typedef struct FlexEventV2 FlexEventV2;

// Opaque handle for FLAC output. Create it with `flexaudio_flac_create`, append chunks with
// `flexaudio_flac_write`, finalize with `flexaudio_flac_finalize`, and release with `flexaudio_flac_free`.
typedef struct FlexFlac FlexFlac;

// Owned immutable lifecycle notification, independent of stream lifetime.
typedef struct FlexLifecycleEventV4 FlexLifecycleEventV4;

// Immutable secondary chunk owner. PCM, metadata and events live until chunk_free_v3.
// Borrowed arrays must never be freed separately. Release each owner exactly once.
typedef struct FlexSecondaryChunkV3 FlexSecondaryChunkV3;

// Immutable snapshot owner. Item strings and arrays borrow from this object.
// Standalone snapshots must be freed exactly once; freeing an error-owned view is a no-op.
typedef struct FlexSettingsV4 FlexSettingsV4;

// Owned completed shutdown report; all child pointers are borrowed.
typedef struct FlexShutdownReportV2 FlexShutdownReportV2;

// Opaque recording stream handle (`FlexStream`) with any enabled
// addons (denoise / VAD); C code holds only a pointer. Create with `flexaudio_open` and
// release with `flexaudio_free`.
//
// The facade owns denoise. VAD consumes canonical capture before
// output conversion. `flexaudio_switch_source` replaces only the source;
// addons retain their configuration from open (same as gain).
typedef struct FlexStream FlexStream;

// Opaque exclusively owned session. Construct off the capture callback.
typedef struct FlexVad FlexVad;

// Opaque probability-only segmentation owner.
typedef struct FlexVadPostProcessor FlexVadPostProcessor;

// Opaque device watcher handle (`FlexWatcher`). Create with
// `flexaudio_watch_devices` and release with `flexaudio_watcher_free`.
typedef struct FlexWatcher FlexWatcher;

// Configuration for opening a stream. Passed to `flexaudio_open` / `flexaudio_switch_source`.
//
// Sentinel values mean "unspecified" for strings and optional values (`device_id` NULL selects the default device,
// `process_id` 0 means none, and `output_rate`/`output_channels`/`chunk_ms` 0 select defaults).
typedef struct FlexConfig {
    // Source kind integer (FlexSourceKind code); unknown values are rejected.
    int32_t kind;
    // ID of the selected device (UTF-8, NUL-terminated). NULL selects the default device.
    const char *device_id;
    // Target PID for a process source. 0 means none (may cause an error when starting a process source).
    uint32_t process_id;
    // Include/exclude integer (FlexProcessMode code), validated even for other sources.
    int32_t mode;
    // Whether to exclude this process's playback from system audio (system source only;
    // for mix, applies to the system side).
    // Boolean integer: only 0 and 1 are valid.
    uint8_t exclude_self;
    // Output sample rate (Hz). 0 selects 48000.
    uint32_t output_rate;
    // Output channel count. 0 selects 2.
    uint16_t output_channels;
    // Chunk duration (ms). 0 selects 20; all other non-20 values are rejected.
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
    // Whether to apply noise suppression (RNNoise) to the stream. Enabled when `true`.
    // Runs once in the library on canonical 48 kHz stereo capture before downmix and gain,
    // feeding output conversion and attached VAD before polling. Every C version supports
    // denoise at any supported primary output rate.
    // Boolean integer: only 0 and 1 are valid.
    uint8_t denoise;
    // Silence diagnosis probe: 0 = default (enabled), 1 = enabled, 2 = disabled.
    uint8_t silence_probe;
} FlexConfig;

// One captured audio chunk, populated by `flexaudio_poll_chunk`.
//
// `data` is flexaudio-owned interleaved f32 with length `len` (= `frames * channels`).
// Always release it with `flexaudio_chunk_free` when finished (do not use C `free`).
typedef struct FlexChunk {
    // Pointer to interleaved f32 samples. Release with `flexaudio_chunk_free`.
    float *data;
    // Number of elements in `data` (= `frames * channels`).
    size_t len;
    // Number of frames in the chunk.
    uint32_t frames;
    // Deprecated naming since 0.8; use flexaudio_chunk_arrival_ns_v4. This frozen
    // field remains permanently in the C layout (other bindings retain it through 0.9).
    // Arrival-time label assigned at facade ingest and clamped non-decreasing. Primary/secondary
    // ranges may overlap, repeat labels or leave gaps by callback jitter; this is not a media clock.
    // flexaudio_chunk_frame_index is authoritative (gaps are loss). Exact VAD mapping uses
    // epoch_start.pts_ns + t_ms*1000000 or epoch_start.capture_sample + t_ms*48.
    int64_t pts_ns;
    // Monotonically increasing sequence number assigned by the stream layer.
    uint64_t seq;
    // Chunk state flags (unsigned 32-bit bitmask).
    uint32_t flags;
    // Number of chunks dropped before this chunk arrived.
    uint32_t dropped_before;
    // Maximum absolute delivered sample value after denoise and gain (linear amplitude).
    float peak;
    // Root-mean-square of delivered samples after denoise and gain (linear).
    float rms;
} FlexChunk;

// One captured event, populated by `flexaudio_poll_event`.
//
// For Error, PermissionDenied, SilenceWhileSourceActive, and PermissionPending,
// the message is stored in flexaudio_last_error. PermissionDenied retains kind 3;
// PermissionPending has kind 8 and does not stop capture. This arm remains for
// custom Rust backends and compatibility; built-in sources on every OS emit
// neither pending nor granted notices.
typedef struct FlexEvent {
    // Event kind.
    int32_t kind;
    // Number dropped for `ChunkDropped`; 0 for other kinds.
    int64_t count;
} FlexEvent;

// Information for one enumerated device (`FlexDeviceInfo`).
//
// `id` / `name` are flexaudio-owned, NUL-terminated UTF-8 strings. Release the entire array with
// `flexaudio_devices_free` (do not use C `free`). Unknown names are empty strings;
// Windows/macOS use selectable native IDs when names are unreadable. IDs may change
// between queries: a native fallback may become the recovered name. A name-based ID
// cannot be opened while its name is unreadable; the native ID stays openable in both
// name states. macOS omits unreadable output direction from discovery; the typed
// direction cause is returned for the requested device. Explicit-output scans skip
// vanished or unrelated unreadable devices. If the requested device's direction query
// fails and neither its name nor UID is readable, open returns FLEX_DEVICE_NOT_FOUND because
// it cannot be matched. Unreadable default queries leave is_default=false.
typedef struct FlexDeviceInfo {
    // Selectable ID (released by `flexaudio_devices_free`).
    char *id;
    // Human-readable display name (released by `flexaudio_devices_free`).
    char *name;
    // Source kind used to capture this device.
    int32_t source_kind;
    // Native rate (Hz), or 0 if unavailable; use flexaudio_device_format_available_v2.
    uint32_t sample_rate;
    // Native channel count, or 0 if unavailable (both rate/channels are 0).
    uint16_t channels;
    // True for loopback (system-output monitor).
    bool is_loopback;
    // True if this is the OS default device.
    bool is_default;
} FlexDeviceInfo;

// Information for one enumerated process (`FlexProcessInfo`).
//
// Pass `pid` to `FlexConfig.process_id` to capture that process. Strings are flexaudio-owned,
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
    int32_t output_activity;
} FlexProcessInfo;

// Optional version components are present only when their has_* word equals 1.
typedef struct FlexOsVersionV1 {
    uint32_t major;
    uint32_t minor;
    uint32_t has_patch;
    uint32_t patch;
    uint32_t has_build;
    uint32_t build;
} FlexOsVersionV1;

// Availability with optional OS prerequisite and borrowed structured query error.
typedef struct FlexAvailabilityV1 {
    uint32_t status;
    uint32_t reason;
    // 1 only for OS_VERSION_TOO_OLD, otherwise required/actual are absent.
    uint32_t has_os_versions;
    struct FlexOsVersionV1 required;
    struct FlexOsVersionV1 actual;
    // Borrowed until capabilities_free_v1. Never free this error separately.
    const struct FlexErrorInfoV2 *unknown;
} FlexAvailabilityV1;

typedef struct FlexSourceCapabilityV1 {
    uint32_t backend;
    struct FlexAvailabilityV1 availability;
} FlexSourceCapabilityV1;

typedef struct FlexSystemExclusionCapabilitiesV1 {
    uint32_t backend;
    struct FlexAvailabilityV1 availability;
    uint32_t scope;
    // 0 means no distinct PID count limit; 1 means max_distinct_pids is present.
    uint32_t has_max_distinct_pids;
    uint32_t max_distinct_pids;
    uint32_t device_selection;
} FlexSystemExclusionCapabilitiesV1;

typedef struct FlexProcessCapabilitiesV1 {
    uint32_t include_scope;
    uint32_t exclude_scope;
} FlexProcessCapabilitiesV1;

typedef struct FlexDeviceWatcherCapabilitiesV1 {
    struct FlexAvailabilityV1 availability;
    uint32_t mode;
    // Read individual scopes with capabilities_watcher_scope_v1.
    size_t scopes_len;
} FlexDeviceWatcherCapabilitiesV1;

typedef struct FlexSideEffectCapabilityV1 {
    struct FlexAvailabilityV1 availability;
    // Only 1 means enabled; unknown future conditions fail closed.
    uint32_t enabled_by_default;
    // Read individual kinds with capabilities_side_effect_source_v1.
    size_t sources_len;
    uint32_t condition;
} FlexSideEffectCapabilityV1;

// Owned device strings; namespace strings have static lifetime.
typedef struct FlexNamespacedDeviceV4 {
    struct FlexDeviceInfo device;
    const char *id_namespace;
} FlexNamespacedDeviceV4;

// Secondary metadata; meters describe float samples before integer quantization.
typedef struct FlexSecondaryMetadataV3 {
    uint32_t size;
    uint32_t version;
    uint32_t rate;
    uint16_t channels;
    uint32_t encoding;
    uint32_t frames;
    uint64_t frame_index;
    // Deprecated naming since 0.8; use flexaudio_secondary_arrival_ns_v4.
    // This field remains permanently in the frozen C layout.
    int64_t pts_ns;
    uint64_t seq;
    uint32_t flags;
    uint32_t dropped_before;
    float peak;
    float rms;
} FlexSecondaryMetadataV3;

// Half-open final interval on the 10 ms grid; its end may exceed physical EOF.
typedef struct FlexSpeechSegment {
    uint64_t start_ms;
    uint64_t end_ms;
} FlexSpeechSegment;

typedef struct FlexVadSpeechStart {
    uint64_t at_ms;
} FlexVadSpeechStart;

typedef struct FlexVadSpeechEnd {
    uint64_t at_ms;
    uint32_t reason;
} FlexVadSpeechEnd;

typedef struct FlexVadCut {
    uint64_t start_ms;
    uint64_t end_ms;
    uint32_t reason;
} FlexVadCut;

typedef struct FlexVadEpochEnd {
    uint32_t reason;
} FlexVadEpochEnd;

// Read only the payload selected by the event's type tag.
typedef union FlexVadPayload {
    struct FlexSpeechSegment segment;
    struct FlexVadSpeechStart speech_start;
    struct FlexVadSpeechEnd speech_end;
    struct FlexVadCut cut;
    struct FlexVadEpochEnd epoch_end;
} FlexVadPayload;

typedef struct FlexVadEpochStart {
    // 48 kHz frames on the dedicated capture timeline, independent of primary/secondary
    // format and tap. capture_sample + t_ms*48 is exact. Equal to primary frame_index
    // for the same sample only when primary is 48 kHz. Other rates share units but not
    // the origin (resampler group delay); unreconciled startup-release padding can
    // grow the offset by up to one primary chunk per boundary. Neither index is an offset into concatenated
    // delivered PCM; pause and queue drops advance indices without delivering.
    uint64_t capture_sample;
    int64_t pts_ns;
} FlexVadEpochStart;

// Attached union is distinct from the standalone event union.
typedef union FlexAttachedVadPayload {
    union FlexVadPayload vad;
    struct FlexVadEpochStart epoch_start;
} FlexAttachedVadPayload;

// Versioned attached event. Epoch start is sequence 0 before all VAD payloads.
typedef struct FlexAttachedVadEvent {
    uint32_t size;
    uint32_t version;
    uint32_t type;
    uint32_t epoch;
    uint64_t seq;
    union FlexAttachedVadPayload data;
} FlexAttachedVadEvent;

typedef struct FlexPidsV4 {
    const uint32_t *data;
    size_t len;
} FlexPidsV4;

typedef struct FlexFormatV4 {
    uint32_t sample_rate;
    uint16_t channels;
} FlexFormatV4;

// Read only the union member selected by kind.
typedef union FlexValuePayloadV4 {
    bool boolean;
    uint64_t unsigned_value;
    float float_value;
    const char *text;
    struct FlexPidsV4 pids;
    struct FlexFormatV4 format;
} FlexValuePayloadV4;

typedef struct FlexValueV4 {
    enum FlexValueKindV4 kind;
    union FlexValuePayloadV4 value;
} FlexValueV4;

// Borrowed views valid until their settings owner is freed. reason is NULL for
// not-requested/applied, otherwise a shared reason tag. Never free item pointers.
typedef struct FlexSettingResolutionV4 {
    const char *setting;
    uint32_t status;
    const char *reason;
    struct FlexValueV4 requested;
    struct FlexValueV4 effective;
} FlexSettingResolutionV4;

typedef struct FlexAudioLossV2 {
    // 0 capture, 1 Mix FIFO, 2 output.
    uint32_t path;
    // 0 none, 1 microphone, 2 system audio.
    uint32_t lane;
    // 0 primary, 1 secondary; used only for output path.
    uint32_t tap;
    // 0 raw overflow, 1 FIFO overflow, 2 corrupt, 3 malformed, 4 callback rejected,
    // 5 output overflow, 6 generation ended: terminal capture discarded a held valid native startup prefix.
    uint32_t reason;
    // 0 unknown, 1 known positive scalar interleaved sample count.
    uint32_t count_known;
    uint64_t samples;
    uint32_t sample_rate;
    uint16_t channels;
} FlexAudioLossV2;

typedef struct FlexNativeFormatV2 {
    uint32_t sample_rate;
    uint16_t channels;
} FlexNativeFormatV2;

typedef struct FlexNativeFormatChangeV2 {
    struct FlexNativeFormatV2 advertised;
    struct FlexNativeFormatV2 actual;
} FlexNativeFormatChangeV2;

typedef struct FlexErrorContextV2 {
    // 0 enumerate, 1 start, 2 normalize, 3 flush, 4 reopen, 5 rollback, 6 stop, 7 join, 8 link.
    uint32_t operation;
    // 0 none, 1 microphone, 2 system audio.
    uint32_t lane;
    // 0 none, 1 HRESULT, 2 OSStatus.
    uint32_t native_code_kind;
    int64_t native_code;
    // Borrowed from the error owner; never free separately.
    const char *native_call;
} FlexErrorContextV2;

// Pinned segmentation parameters. NULL uses defaults; every supplied zero is literal.
typedef struct FlexVadOptions {
    float threshold;
    int32_t min_speech_ms;
    int32_t min_silence_ms;
    // Maximum segmentation budget in integer milliseconds. Zero or values above
    // 100000000 use the effectively unbounded INT_MAX/2-sample sentinel (1073741823 samples).
    // Other nonzero values must be at least 32 + 2 * speech_pad_ms.
    // Zero split budgets are valid; negative budgets are rejected.
    // The final segment merger can join neighboring splits.
    uint32_t max_speech_ms;
    int32_t speech_pad_ms;
} FlexVadOptions;

// Fixed-width stream VAD attachment settings. v1 has no attached VAD; v2 supports
// the primary tap. v3/v4 also support the secondary tap when secondary output is configured.
typedef struct FlexStreamVadOptions {
    struct FlexVadOptions params;
    // 0 or 1, never a default sentinel.
    uint8_t provisional;
    // FLEX_VAD_PRIMARY or FLEX_VAD_SECONDARY.
    uint32_t tap;
} FlexStreamVadOptions;

// Secondary format, fixed for the stream lifetime. Rate/channels are literal, not sentinels.
typedef struct FlexSecondaryOutputV3 {
    uint32_t size;
    uint32_t version;
    uint32_t rate;
    uint16_t channels;
    // FLEX_SECONDARY_F32 or FLEX_SECONDARY_S16. Unknown values are rejected.
    uint32_t encoding;
} FlexSecondaryOutputV3;

// Borrowed open envelope. Initialize size to sizeof(FlexStreamConfigV3) and version to 3.
typedef struct FlexStreamConfigV3 {
    uint32_t size;
    uint32_t version;
    const struct FlexConfig *config;
    // NULL disables attached VAD.
    const struct FlexStreamVadOptions *vad;
    const uint32_t *exclude_pids;
    size_t exclude_pids_len;
    // NULL disables secondary output.
    const struct FlexSecondaryOutputV3 *secondary_output;
    // 0 default (enabled), 1 enabled, 2 disabled.
    uint32_t windows_classic_loopback_keepalive;
} FlexStreamConfigV3;

// Borrowed source switch envelope. Output formats, gain and denoise stay fixed at open.
typedef struct FlexSwitchConfigV3 {
    uint32_t size;
    uint32_t version;
    const struct FlexConfig *config;
    const uint32_t *exclude_pids;
    size_t exclude_pids_len;
    // 0 retain current setting, 1 enabled, 2 disabled.
    uint32_t windows_classic_loopback_keepalive;
} FlexSwitchConfigV3;

// Borrowed controls. Strict: 0 strict on open/retain on switch, 1 strict, 2 permissive.
// NULL ring capacity uses 50 on open and retains capacity on switch.
typedef struct FlexOptionsV4 {
    uint32_t size;
    uint32_t version;
    uint32_t strict;
    const uint64_t *ring_capacity_chunks;
} FlexOptionsV4;

// Borrowed envelope; size/version must exactly match before other fields are read.
typedef struct FlexStreamConfigV4 {
    uint32_t size;
    uint32_t version;
    const struct FlexStreamConfigV3 *base;
    const struct FlexOptionsV4 *options;
} FlexStreamConfigV4;

// Borrowed switch envelope. NULL options retains controls and capacity.
typedef struct FlexSwitchConfigV4 {
    uint32_t size;
    uint32_t version;
    const struct FlexSwitchConfigV3 *base;
    const struct FlexOptionsV4 *options;
} FlexSwitchConfigV4;

// Borrowed gains. Each NULL pointer selects 1.0; each supplied float is literal,
// including 0.0 (mute). Values must be finite and nonnegative, with no upper bound.
// These replace all three gain fields in the nested FlexConfig, which must be zero.
// Nonzero legacy gain fields return InvalidArg; use FlexGainsV5 instead.
typedef struct FlexGainsV5 {
    uint32_t size;
    uint32_t version;
    const float *gain;
    const float *mix_mic_gain;
    const float *mix_system_gain;
} FlexGainsV5;

// Borrowed v5 envelope; exact size/version required. base uses unchanged v4/v3
// layouts and controls. NULL gains omits all gains. Nested FlexConfig gain,
// mix_mic_gain and mix_system_gain must be zero; use FlexGainsV5 for gain values.
typedef struct FlexStreamConfigV5 {
    uint32_t size;
    uint32_t version;
    const struct FlexStreamConfigV4 *base;
    const struct FlexGainsV5 *gains;
} FlexStreamConfigV5;

// Borrowed v5 switch envelope. Omitted gain retains the current output gain;
// explicit non-default gain is fixedAtOpen (strict rejects, permissive ignores).
// Mix gains select 1.0 when omitted and literal values when supplied on switch.
// Nested FlexConfig gain fields must be zero, as in FlexStreamConfigV5.
typedef struct FlexSwitchConfigV5 {
    uint32_t size;
    uint32_t version;
    const struct FlexSwitchConfigV4 *base;
    const struct FlexGainsV5 *gains;
} FlexSwitchConfigV5;

// Preview policy. provisional must be 0 (disabled) or 1 (enabled).
typedef struct FlexVadRuntimeOptions {
    uint8_t provisional;
} FlexVadRuntimeOptions;

// Standalone ordered event. All times are epoch-relative integer milliseconds.
typedef struct FlexVadEvent {
    uint32_t type;
    uint32_t epoch;
    uint64_t seq;
    union FlexVadPayload data;
} FlexVadEvent;

// Versioned envelope around the frozen v1 configuration. All pointers are borrowed during open.
typedef struct FlexStreamConfigV2 {
    uint32_t size;
    uint32_t version;
    const struct FlexConfig *config;
    // NULL disables vad_tap attachment.
    const struct FlexStreamVadOptions *vad;
} FlexStreamConfigV2;

// Owns v1 PCM/events and attached events until flexaudio_chunk_free_v2.
typedef struct FlexChunkV2 {
    uint32_t size;
    uint32_t version;
    struct FlexChunk chunk;
    struct FlexAttachedVadEvent *vad_events;
    size_t vad_events_len;
} FlexChunkV2;

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
    int32_t kind;
    // Selectable ID (= FlexDeviceInfo.id; valid for `Added`/`Removed`/`DefaultChanged`; release with
    // `flexaudio_device_event_free`). NULL for `Unknown`.
    char *id;
    // Display name (`Added` only; release with `flexaudio_device_event_free`). NULL for other kinds.
    char *name;
    // For `Added`, the device source kind. For `DefaultChanged`, the side whose default changed
    // (`Mic` = default source / `System` = default sink). Unused for other kinds (`Mic`).
    int32_t source_kind;
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
// For macOS microphone/Mix, consent is checked before any HAL/device-format query.
// An undecided bundled prompt waits at most 30 seconds; an unanswered prompt returns
// NULL with a typed Backend error without querying HAL or starting capture. Answer
// the prompt and retry. Undecided consent without microphone usage metadata fails
// immediately; configure the bundle or grant the responsible host access first.
// The bound covers the undecided consent wait, not authorized native queries.
// Opening is synchronous; GUI hosts should use a worker thread.
//
// `config.denoise` enables noise suppression on delivered PCM. Use `flexaudio_open_v2`
// with `FlexStreamVadOptions` to attach VAD on the canonical capture branch.
// Denoise runs on canonical 48 kHz capture before conversion to any supported output rate.
// Free the returned handle with `flexaudio_free`.
// This is equivalent to `flexaudio_open_with_exclude_pids(config, NULL, 0)`.
// Opens strict by default: ignored non-default settings fail with FLEX_IGNORED_SETTINGS.
// Zero-initialized microphone settings use defaults. To opt out, use v4 strict = 2.
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
                                                    size_t exclude_pids_len);

// Run checked stop, then free the stream. NULL-safe. Cleanup failure remains in last_error.
// Unread PCM and VAD events are dropped. Stop and drain first to retain final outputs.
//
// # Safety
// `s` must be a handle returned by `flexaudio_open` (or NULL). Do not use `s` after freeing it.
void flexaudio_free(struct FlexStream *s);

// Start capture. Repeated calls while running succeed without starting again.
// After stop, the stream is spent: start returns FLEX_INVALID_STATE; open a new stream.
//
// # Safety
// `s` must be a valid handle (NULL is InvalidArg).
int32_t flexaudio_start(struct FlexStream *s);

// Stop capture and finish denoise/VAD once. Graceful PCM tails remain pollable.
// Return the primary root error code for capture or cleanup failure. Repeated stop
// returns the retained outcome without duplicate tails or cleanup events.
// Inspect shutdown_report_v2 for the separate capture primary and cleanup errors.
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
// negative value on error. A terminal permission denial returns FLEX_FAILURE (-2)
// with actionable guidance in flexaudio_last_error, including after stop.
// `out.data` is owned by flexaudio; free it with
// `flexaudio_chunk_free` when done.
// PCM polling does not wait for event polling, including after stop. Startup notices
// enter the ordinary FIFO event queue before the generation's first chunk is published;
// event-first callers can observe them before polling that PCM.
//
// Optional noise suppression updates the returned PCM. Attached VAD streams require
// `flexaudio_poll_chunk_v2`, which also delivers ordered VAD events and epoch origins.
//
// # Safety
// `s` must be a valid handle, and `out` must point to a valid `FlexChunk` destination.
int32_t flexaudio_poll_chunk(struct FlexStream *s, struct FlexChunk *out);

// Return the producer frame index in canonical 48 kHz units without changing v1 layout.
// NULL, a freed chunk, or caller-owned PCM returns zero and sets last_error.
// # Safety
// Non-NULL must point to an aligned, readable FlexChunk. PCM is never dereferenced.
uint64_t flexaudio_chunk_frame_index(const struct FlexChunk *chunk);

// Free the `data` filled by `flexaudio_poll_chunk` and set `data=NULL` / `len=0`.
// Safe for NULL and repeated calls.
//
// # Safety
// `chunk` must point to a `FlexChunk` filled by `flexaudio_poll_chunk` (or be NULL).
void flexaudio_chunk_free(struct FlexChunk *chunk);

// Retrieve one event and fill `out`.
//
// Return 1 when an event is retrieved, 0 when none is available, or a negative value on error.
// Error, PermissionDenied (kind 3), SilenceWhileSourceActive (kind 7), and
// PermissionPending (kind 8, advisory only: capture continues)
// store safe explanations in last_error. Typed failures project to Error (5):
// Pending/granted arms remain for custom Rust backends and compatibility;
// built-in sources on every OS emit neither notice.
// v1 cannot distinguish terminal/recoverable/cleanup; inspect terminal_error or use v2.
// Counts above INT64_MAX saturate with a range-loss diagnostic. PermissionDenied is terminal, including
// a confirmed macOS self-probe failure. SilenceWhileSourceActive means the
// permission diagnosis is inconclusive; both advisory kinds continue capture.
//
// # Safety
// `s` must be a valid handle, and `out` must point to a valid `FlexEvent` destination.
int32_t flexaudio_poll_event(struct FlexStream *s,
                             struct FlexEvent *out);

// Hot-swap the input source without stopping capture. `config.gain` is ignored because gain is
// stream state; change it with `flexaudio_set_gain`. Add-on settings from open remain
// in use; `config.denoise` is ignored. The output format cannot change on a switch.
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
                                                  size_t exclude_pids_len);

// Return FLEX_OK when no terminal failure is stored, or FLEX_FAILURE (-2) and
// set flexaudio_last_error to the terminal reason. Does not consume events and
// remains available after flexaudio_stop.
//
// # Safety
// s must be a valid handle (NULL is InvalidArg).
int32_t flexaudio_terminal_error(const struct FlexStream *s);

// List available devices, allocate an array, and set `out_array` / `out_count`.
//
// Return 0 only for complete discovery. An empty complete inventory sets
// `out_array=NULL` / `out_count=0`. Collection/count failure (including an
// unreachable PipeWire daemon on Linux) returns a typed error code and sets
// `flexaudio_last_error` / `flexaudio_last_error_info_v2`; it is not an empty success.
// Free the allocated array with `flexaudio_devices_free`.
// On macOS, enumeration never prompts. With undecided microphone consent it returns
// a typed Backend error before any HAL/device/format query. Grant the responsible
// host access or use the bounded capture-open consent flow, then retry enumeration.
// Denied/restricted consent returns a permission error. Unreadable mandatory FlexDeviceInfo formats
// use format_available=false and the 0/0 sentinel; other entries remain usable.
// Windows enumeration does not preflight microphone permission; native enumeration
// proceeds. Microphone capture retains its separate existing permission preflight.
//
// # Safety
// `out_array` / `out_count` must be valid output pointers (NULL is InvalidArg).
int32_t flexaudio_devices(struct FlexDeviceInfo **out_array, size_t *out_count);

// Free the array allocated by `flexaudio_devices` and each `id` / `name`. NULL-safe.
//
// # Safety
// `arr` / `count` must be values returned by `flexaudio_devices` (or NULL/0).
void flexaudio_devices_free(struct FlexDeviceInfo *arr, size_t count);

// List processes with audio output sessions (streams) that can be captured individually,
// allocate an array, and set `out_array` / `out_count`. The calling process is excluded.
// Processes whose audio sessions are stopped or idle are also included. Check `output_activity`
// to see whether a process is currently producing audio.
//
// Return 0 on success. If there are no candidates, an empty result (`out_array=NULL` /
// `out_count=0`) is still successful: per-process capture is available, but there are no
// matching processes now. Free the allocated array **once only** with
// `flexaudio_processes_free`. Return a typed negative result code (with the reason in `flexaudio_last_error`)
// if per-process capture is unavailable in this environment (PipeWire is unreachable on
// Linux; macOS is earlier than 14.4; Windows is older than build 20348 (Windows 11 / Windows
// Server 2022 or later is required); the OS is unsupported; or access is denied), if the OS does
// not respond within 3 seconds, or if the previous query is still running.
//
// # Safety
// `out_array` / `out_count` must be valid output pointers (NULL is InvalidArg).
int32_t flexaudio_processes(struct FlexProcessInfo **out_array,
                            size_t *out_count);

// Free the array allocated by `flexaudio_processes` and each string. NULL-safe.
// Call **once only**; calling twice with the same pointer causes a double-free and undefined
// behavior.
//
// # Safety
// `arr` / `count` must be values returned by `flexaudio_processes` (or NULL/0). Call this
// function only once for the same `arr`.
void flexaudio_processes_free(struct FlexProcessInfo *arr, size_t count);

// Return the most recent error message for the current thread.
//
// Valid until the next FFI call on the same thread updates last_error. Returns NULL if there
// is no error. The returned pointer is owned by flexaudio; do not free it from C.
const char *flexaudio_last_error(void);

// Whether this device's capture format is available. Additive v2 accessor; the
// v1 FlexDeviceInfo layout is unchanged. False means sample_rate=0 and channels=0.
// Windows/macOS output and microphone opens perform a fresh checked format query.
// Explicit Linux output IDs use a fresh checked inventory and format-availability gate.
// These checks fail with a typed cause/remedy error if the format remains unreadable.
// Linux output opens without a device ID rely on PipeWire to resolve the default sink
// and negotiate the format, without an inventory/format pre-check. Linux PipeWire's
// documented compatibility defaults remain available.
// NULL/misaligned pointers return false and set a typed InvalidArg last error.
//
// # Safety
// `device` must point to a live FlexDeviceInfo returned by flexaudio_devices.
bool flexaudio_device_format_available_v2(const struct FlexDeviceInfo *device);

// Probe capture support without opening capture, enumerating microphones or playing audio.
// Returns FLEX_OK and an owned snapshot, including structured unknown query failures.
// The bounded platform probe can block; GUI hosts should use a worker thread.
// # Safety
// out must be writable and aligned. Release the owner with capabilities_free_v1.
int32_t flexaudio_capabilities_v1(struct FlexCapabilitiesV1 **out);

// Free an owned snapshot and invalidate all borrowed views. NULL is safe.
// # Safety
// value must be a live library owner or NULL; release exactly once.
void flexaudio_capabilities_free_v1(struct FlexCapabilitiesV1 *value);

// Copy the running OS version: 1 present, 0 unknown, negative invalid argument.
// All record accessors leave out untouched when absent. Unknown codes never imply support.
// # Safety
// value must be a live snapshot; out must be writable and aligned.
int32_t flexaudio_capabilities_os_version_v1(const struct FlexCapabilitiesV1 *value,
                                             struct FlexOsVersionV1 *out);

// Copy one source's backend and complete availability. kind uses FLEX_SOURCE_KIND_*.
// Returns 1 or a negative typed error. Borrowed unknown error lives until snapshot free.
// # Safety
// value must be a live snapshot; out must be writable and aligned.
int32_t flexaudio_capabilities_source_v1(const struct FlexCapabilitiesV1 *value,
                                         int32_t kind,
                                         struct FlexSourceCapabilityV1 *out);

// Copy system exclusion availability, backend, PID semantics, limit and device selection.
// Returns 1 or a negative typed error; all error pointers are borrowed until snapshot free.
// # Safety
// value must be a live snapshot; out must be writable and aligned.
int32_t flexaudio_capabilities_system_exclusions_v1(const struct FlexCapabilitiesV1 *value,
                                                    struct FlexSystemExclusionCapabilitiesV1 *out);

// Copy include/exclude process PID scopes. Returns 1 or a negative typed error.
// # Safety
// value must be a live snapshot; out must be writable and aligned.
int32_t flexaudio_capabilities_process_v1(const struct FlexCapabilitiesV1 *value,
                                          struct FlexProcessCapabilitiesV1 *out);

// Copy watcher availability, mode and scope count. Returns 1 or a negative typed error.
// # Safety
// value must be a live snapshot; out must be writable and aligned.
int32_t flexaudio_capabilities_watcher_v1(const struct FlexCapabilitiesV1 *value,
                                          struct FlexDeviceWatcherCapabilitiesV1 *out);

// Read an indexed watcher scope. Returns 1 or a negative typed error (including out of range).
// # Safety
// value must be a live snapshot; out must be writable and aligned.
int32_t flexaudio_capabilities_watcher_scope_v1(const struct FlexCapabilitiesV1 *value,
                                                size_t index,
                                                uint32_t *out);

// Copy side effect availability, default state, source count and triggering condition.
// kind uses FLEX_SIDE_EFFECT_*. Returns 1 or a negative typed error.
// # Safety
// value must be a live snapshot; out must be writable and aligned.
int32_t flexaudio_capabilities_side_effect_v1(const struct FlexCapabilitiesV1 *value,
                                              uint32_t kind,
                                              struct FlexSideEffectCapabilityV1 *out);

// Read a side effect source as FLEX_SOURCE_KIND_*. Invalid kind/index returns InvalidArg.
// # Safety
// value must be a live snapshot; out must be writable and aligned.
int32_t flexaudio_capabilities_side_effect_source_v1(const struct FlexCapabilitiesV1 *value,
                                                     uint32_t kind,
                                                     size_t index,
                                                     int32_t *out);

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
int32_t flexaudio_denoise_process(struct FlexDenoiser *d, float *samples, size_t len);

// Drain actual interleaved delayed samples into a library-owned array.
//
// Valid output destinations initialize to NULL/0, including on errors. Repeated
// flush without new nonempty input returns NULL/0. Release samples with
// `flexaudio_denoise_samples_free`, never C free.
//
// # Safety
// `d` must be a live handle; `out`/`out_len` must be aligned writable destinations.
int32_t flexaudio_denoise_flush(struct FlexDenoiser *d, float **out, size_t *out_len);

// Release the sample array returned by `flexaudio_denoise_flush`. NULL/0 is safe.
//
// # Safety
// `samples`/`len` must be the exact live allocation returned by flush, or NULL/0.
// Release each allocation once.
void flexaudio_denoise_samples_free(float *samples, size_t len);

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
// Writing to a finalized handle returns `FLEX_INVALID_STATE`.
// Returns 0 on success and a negative value on error.
//
// # Safety
// `f` must be a valid handle and `samples` a valid array of `len` elements (NULL is allowed when `len=0`).
int32_t flexaudio_flac_write(struct FlexFlac *f,
                             const float *samples,
                             size_t len);

// Write any remaining data, finalize and close the current file. Further writes return InvalidState.
//
// Calling finalize more than once after success is safe (no-op returning 0). A failed write or
// finalize retains its first error for all later writes and finalization attempts.
// Returns 0 on success and a negative value on error.
//
// # Safety
// `f` must be a valid handle (NULL is InvalidArg).
int32_t flexaudio_flac_finalize(struct FlexFlac *f);

// Release a FLAC handle. NULL-safe.
//
// If freed without finalize, `FlexFlac` still makes a best-effort attempt to
// write remaining data and finalize the header on release (errors are swallowed; call
// `flexaudio_flac_finalize` first to detect them reliably).
//
// # Safety
// `f` must be a handle returned by `flexaudio_flac_create` (or NULL).
// Do not use `f` after release.
void flexaudio_flac_free(struct FlexFlac *f);

// Enumerate unchanged selectable devices with provider-assigned namespace tags.
// The array owns device strings; namespace strings have static lifetime.
// Both outputs initialize to empty on failure when their pointers are valid.
// # Safety
// out and len are writable; release the array using namespaced_devices_free_v4.
int32_t flexaudio_devices_with_namespaces_v4(struct FlexNamespacedDeviceV4 **out, size_t *len);

// Free the namespaced array and its device strings. NULL-safe.
// # Safety
// entries and len are exactly an unfreed result of devices_with_namespaces_v4, or NULL.
void flexaudio_namespaced_devices_free_v4(struct FlexNamespacedDeviceV4 *entries, size_t len);

// Return the exact arrival-time label. Also accepts &chunk_v2.chunk.
// # Safety
// chunk is a readable FlexChunk; out is writable.
int32_t flexaudio_chunk_arrival_ns_v4(const struct FlexChunk *chunk, int64_t *out);

// Return the secondary chunk's exact arrival-time label.
// # Safety
// chunk is a live secondary owner; out is writable.
int32_t flexaudio_secondary_arrival_ns_v4(const struct FlexSecondaryChunkV3 *chunk, int64_t *out);

// Poll the lifecycle FIFO independently of ordinary polls, including after stop.
// Return 1 with an owned event, 0 empty, or a negative error; out is NULL on absence/error.
// The FIFO retains at most 256 events, dropping the oldest on overflow. Started/Stopped
// pairing is guaranteed only while the caller drains it fast enough to avoid overflow.
// # Safety
// stream is live and exclusively borrowed, out is writable. Use a stream from one
// thread at a time or externally serialize its calls, including lifecycle polling.
// Internal event-queue locking does not make other stream operations thread-safe.
int32_t flexaudio_poll_lifecycle_event_v4(struct FlexStream *stream,
                                          struct FlexLifecycleEventV4 **out);

// Return the lifecycle kind, zero on success.
// # Safety
// event is live, out is writable.
int32_t flexaudio_lifecycle_event_kind_v4(const struct FlexLifecycleEventV4 *event, uint32_t *out);

// Return 1 and a stop-reason code (1-10, 0 unknown), or 0 without modifying out.
// # Safety
// event is live, out is writable.
int32_t flexaudio_lifecycle_event_reason_v4(const struct FlexLifecycleEventV4 *event,
                                            uint32_t *out);

// Return 1 and a borrowed safe message, or 0 without modifying out.
// # Safety
// event is live, out is writable. The string borrows from event.
int32_t flexaudio_lifecycle_event_message_v4(const struct FlexLifecycleEventV4 *event,
                                             const char **out);

// Free a lifecycle owner; NULL-safe.
// # Safety
// event is NULL or an unfreed owner returned by poll_lifecycle_event_v4.
void flexaudio_lifecycle_event_free_v4(struct FlexLifecycleEventV4 *event);

// Independently poll one secondary chunk: 1 available, 0 absent, negative error.
// Output initializes to NULL. Stop drains both taps; final chunks remain pollable.
// Free any previously returned owner before reusing out.
// # Safety
// Stream must be live and exclusively borrowed; out must be writable and aligned.
int32_t flexaudio_poll_secondary_chunk_v3(struct FlexStream *stream,
                                          struct FlexSecondaryChunkV3 **out);

// Copy immutable metadata. Returns 1, or a negative typed error.
// # Safety
// Chunk must be a live owner; out must be writable and aligned.
int32_t flexaudio_secondary_metadata_v3(const struct FlexSecondaryChunkV3 *chunk,
                                        struct FlexSecondaryMetadataV3 *out);

// Borrow native float PCM until chunk_free_v3. Wrong encoding returns NULL and
// sets a typed InvalidArg. A valid len initializes to zero on errors.
// # Safety
// Chunk must be live; len must be writable and aligned.
const float *flexaudio_secondary_f32_v3(const struct FlexSecondaryChunkV3 *chunk, size_t *len);

// Borrow native int16_t PCM until chunk_free_v3. Wrong encoding returns NULL and
// sets a typed InvalidArg. A valid len initializes to zero on errors.
// # Safety
// Chunk must be live; len must be writable and aligned.
const int16_t *flexaudio_secondary_s16_v3(const struct FlexSecondaryChunkV3 *chunk, size_t *len);

// Borrow canonical attached VAD events carried by the secondary tap, until chunk_free_v3.
// NULL/0 means no events. Events retain their dedicated capture timeline.
// # Safety
// Chunk must be live; len must be writable and aligned.
const struct FlexAttachedVadEvent *flexaudio_secondary_vad_events_v3(const struct FlexSecondaryChunkV3 *chunk,
                                                                     size_t *len);

// Release one owned chunk and all its borrowed views. NULL is safe.
// # Safety
// Chunk must be a live library owner or NULL. Release each owner exactly once.
void flexaudio_secondary_chunk_free_v3(struct FlexSecondaryChunkV3 *chunk);

// Copy the last committed settings into an independent owner. out is NULL on failure.
// # Safety
// stream is a live handle; out is writable. Free the returned snapshot with settings_free_v4.
int32_t flexaudio_settings_v4(const struct FlexStream *stream, struct FlexSettingsV4 **out);

// Return revision and item count, with zero on success.
// # Safety
// settings is a live owner/view, and both outputs are writable.
int32_t flexaudio_settings_count_v4(const struct FlexSettingsV4 *settings,
                                    uint64_t *revision,
                                    size_t *count);

// Borrow an item from a snapshot; all pointers remain owned by the snapshot.
// # Safety
// settings is live and out is writable. index must be less than the reported count.
int32_t flexaudio_settings_item_v4(const struct FlexSettingsV4 *settings,
                                   size_t index,
                                   struct FlexSettingResolutionV4 *out);

// Free a standalone settings snapshot; NULL and live error-owned views are no-ops.
// # Safety
// settings is NULL, an unfreed standalone snapshot from settings_v4, or a borrowed
// view from error_settings_v4 whose error owner is still live.
void flexaudio_settings_free_v4(struct FlexSettingsV4 *settings);

// Borrow the candidate report from an error: 1 present, 0 absent, negative on error.
// The view and its items live until the error owner is freed. out is NULL on absence/error.
// Passing this live borrowed view to settings_free_v4 is a no-op.
// # Safety
// error is a live error owner and out is writable.
int32_t flexaudio_error_settings_v4(const struct FlexErrorInfoV2 *value,
                                    const struct FlexSettingsV4 **out);

// Poll one owned event. Valid out initializes to NULL before the queue is touched.
// # Safety
// stream must be live and exclusively borrowed; out must be writable.
int32_t flexaudio_poll_event_v2(struct FlexStream *stream, struct FlexEventV2 **out);

// Poll one owned watcher event. Invalid output never consumes a delta.
// # Safety
// watcher must be live and exclusively borrowed; out must be writable.
int32_t flexaudio_watcher_poll_v2(struct FlexWatcher *watcher, struct FlexDeviceEventV2 **out);

// Owned clone of the calling thread's typed error, NULL when absent. Free explicitly.
struct FlexErrorInfoV2 *flexaudio_last_error_info_v2(void);

// Owned capture primary snapshot, valid independently of the stream and TLS.
// # Safety
// stream must be a live readable stream handle.
struct FlexErrorInfoV2 *flexaudio_terminal_error_info_v2(const struct FlexStream *stream);

// Owned completed report snapshot; NULL before shutdown completes.
// # Safety
// stream must be a live readable stream handle.
struct FlexShutdownReportV2 *flexaudio_shutdown_report_v2(const struct FlexStream *stream);

// Free the owned tree; NULL is safe. Borrowed children must never be freed separately.
// # Safety
// value must be a live owned FlexEventV2 returned by this library, or NULL.
void flexaudio_event_free_v2(struct FlexEventV2 *value);

// Free the owned tree; NULL is safe. Borrowed children must never be freed separately.
// # Safety
// value must be a live owned FlexDeviceEventV2 returned by this library, or NULL.
void flexaudio_device_event_free_v2(struct FlexDeviceEventV2 *value);

// Free the owned tree; NULL is safe. Borrowed children must never be freed separately.
// # Safety
// value must be a live owned FlexErrorInfoV2 returned by this library, or NULL.
void flexaudio_error_info_free_v2(struct FlexErrorInfoV2 *value);

// Free the owned tree; NULL is safe. Borrowed children must never be freed separately.
// # Safety
// value must be a live owned FlexShutdownReportV2 returned by this library, or NULL.
void flexaudio_shutdown_report_free_v2(struct FlexShutdownReportV2 *value);

// Return 1 when present, 0 for a different arm, negative InvalidArg for invalid pointers.
// Output is untouched when absent. Borrowed fields remain valid only while value lives.
// # Safety
// value must be a live owner; out must be writable and aligned.
int32_t flexaudio_event_kind_v2(const struct FlexEventV2 *value, int32_t *out);

// Return 1 when present, 0 for a different arm, negative InvalidArg for invalid pointers.
// Output is untouched when absent. Borrowed fields remain valid only while value lives.
// # Safety
// value must be a live owner; out must be writable and aligned.
int32_t flexaudio_event_count_v2(const struct FlexEventV2 *value, uint64_t *out);

// Return 1 when present, 0 for a different arm, negative InvalidArg for invalid pointers.
// Output is untouched when absent. Borrowed fields remain valid only while value lives.
// # Safety
// value must be a live owner; out must be writable and aligned.
int32_t flexaudio_event_permission_v2(const struct FlexEventV2 *value, int32_t *out);

// Return 1 when present, 0 for a different arm, negative InvalidArg for invalid pointers.
// Output is untouched when absent. Borrowed fields remain valid only while value lives.
// # Safety
// value must be a live owner; out must be writable and aligned.
int32_t flexaudio_event_loss_v2(const struct FlexEventV2 *value, struct FlexAudioLossV2 *out);

// Return 1 when present, 0 for a different arm, negative InvalidArg for invalid pointers.
// Output is untouched when absent. Borrowed fields remain valid only while value lives.
// # Safety
// value must be a live owner; out must be writable and aligned.
int32_t flexaudio_device_event_kind_v2(const struct FlexDeviceEventV2 *value, int32_t *out);

// Return 1 when present, 0 for a different arm, negative InvalidArg for invalid pointers.
// Output is untouched when absent. Borrowed fields remain valid only while value lives.
// # Safety
// value must be a live owner; out must be writable and aligned.
int32_t flexaudio_device_event_default_kind_v2(const struct FlexDeviceEventV2 *value, int32_t *out);

// Return 1 when present, 0 for a different arm, negative InvalidArg for invalid pointers.
// Output is untouched when absent. Borrowed fields remain valid only while value lives.
// # Safety
// value must be a live owner; out must be writable and aligned.
int32_t flexaudio_device_event_dropped_events_v2(const struct FlexDeviceEventV2 *value,
                                                 uint64_t *out);

// Return 1 when present, 0 for a different arm, negative InvalidArg for invalid pointers.
// Output is untouched when absent. Borrowed fields remain valid only while value lives.
// # Safety
// value must be a live owner; out must be writable and aligned.
int32_t flexaudio_error_kind_v2(const struct FlexErrorInfoV2 *value, int32_t *out);

// Return 1 when present, 0 for a different arm, negative InvalidArg for invalid pointers.
// Output is untouched when absent. Borrowed fields remain valid only while value lives.
// # Safety
// value must be a live owner; out must be writable and aligned.
int32_t flexaudio_error_permission_v2(const struct FlexErrorInfoV2 *value, int32_t *out);

// Return 1 when present, 0 for a different arm, negative InvalidArg for invalid pointers.
// Output is untouched when absent. Borrowed fields remain valid only while value lives.
// # Safety
// value must be a live owner; out must be writable and aligned.
int32_t flexaudio_error_context_count_v2(const struct FlexErrorInfoV2 *value, size_t *out);

// Return 1 when present, 0 for a different arm, negative InvalidArg for invalid pointers.
// Output is untouched when absent. Borrowed fields remain valid only while value lives.
// # Safety
// value must be a live owner; out must be writable and aligned.
int32_t flexaudio_error_secondary_count_v2(const struct FlexErrorInfoV2 *value, size_t *out);

// Return 1 when present, 0 for a different arm, negative InvalidArg for invalid pointers.
// Output is untouched when absent. Borrowed fields remain valid only while value lives.
// # Safety
// value must be a live owner; out must be writable and aligned.
int32_t flexaudio_shutdown_cleanup_count_v2(const struct FlexShutdownReportV2 *value, size_t *out);

// Return 1 when present, 0 for a different arm, negative InvalidArg for invalid pointers.
// Output is untouched when absent. Borrowed fields remain valid only while value lives.
// # Safety
// value must be a live owner; out must be writable and aligned.
int32_t flexaudio_error_native_format_v2(const struct FlexErrorInfoV2 *value,
                                         struct FlexNativeFormatChangeV2 *out);

// Retrieve an outer-to-inner context record. Invalid index returns InvalidArg.
// # Safety
// value must be a live owner; out must be writable and aligned.
int32_t flexaudio_error_context_v2(const struct FlexErrorInfoV2 *value,
                                   size_t index,
                                   struct FlexErrorContextV2 *out);

// Borrow a view until the owning tree is freed. NULL means absent/wrong arm.
// Invalid pointer/index sets last_error. Do not free the borrowed view separately.
// # Safety
// value must be a live readable owner.
const struct FlexErrorInfoV2 *flexaudio_event_error_v2(const struct FlexEventV2 *value);

// Borrow a view until the owning tree is freed. NULL means absent/wrong arm.
// Invalid pointer/index sets last_error. Do not free the borrowed view separately.
// # Safety
// value must be a live readable owner.
const char *flexaudio_event_message_v2(const struct FlexEventV2 *value);

// Borrow a view until the owning tree is freed. NULL means absent/wrong arm.
// Invalid pointer/index sets last_error. Do not free the borrowed view separately.
// # Safety
// value must be a live readable owner.
const struct FlexDeviceInfo *flexaudio_device_event_device_v2(const struct FlexDeviceEventV2 *value);

// Borrow a view until the owning tree is freed. NULL means absent/wrong arm.
// Invalid pointer/index sets last_error. Do not free the borrowed view separately.
// # Safety
// value must be a live readable owner.
const char *flexaudio_device_event_id_v2(const struct FlexDeviceEventV2 *value);

// Borrow a view until the owning tree is freed. NULL means absent/wrong arm.
// Invalid pointer/index sets last_error. Do not free the borrowed view separately.
// # Safety
// value must be a live readable owner.
const char *flexaudio_error_message_v2(const struct FlexErrorInfoV2 *value);

// Borrow the stable VAD cause code until the error owner is freed.
// NULL means this error has no VAD cause; invalid pointers set last_error.
// Codes: InvalidParameter, InvalidProbability, InvalidPcm, Overflow, SessionFinished,
// Inference, ModelLoad, FailedSession, InvalidStereoLength, CaptureSampleOverflow,
// PtsOutOfRange, UnsupportedTap, UnsupportedConversionClock, Conversion, Stopped.
// # Safety
// value must be a live readable error owner. Do not free the borrowed string.
const char *flexaudio_error_vad_code_v2(const struct FlexErrorInfoV2 *value);

// Borrow a view until the owning tree is freed. NULL means absent/wrong arm.
// Invalid pointer/index sets last_error. Do not free the borrowed view separately.
// # Safety
// value must be a live readable owner.
const struct FlexErrorInfoV2 *flexaudio_error_secondary_v2(const struct FlexErrorInfoV2 *value,
                                                           size_t index);

// Borrow a view until the owning tree is freed. NULL means absent/wrong arm.
// Invalid pointer/index sets last_error. Do not free the borrowed view separately.
// # Safety
// value must be a live readable owner.
const struct FlexErrorInfoV2 *flexaudio_shutdown_primary_v2(const struct FlexShutdownReportV2 *value);

// Borrow a view until the owning tree is freed. NULL means absent/wrong arm.
// Invalid pointer/index sets last_error. Do not free the borrowed view separately.
// # Safety
// value must be a live readable owner.
const struct FlexErrorInfoV2 *flexaudio_shutdown_cleanup_v2(const struct FlexShutdownReportV2 *value,
                                                            size_t index);

// Suppressed microphone native frames; 0 for other event arms, output untouched.
// # Safety
// value must be a live event; out must be writable and aligned.
int32_t flexaudio_event_startup_frames_v2(const struct FlexEventV2 *value, uint64_t *out);

// Startup-drop reason; currently FLEX_STARTUP_DROP_OUT_OF_RANGE (includes non-finite samples).
// # Safety
// value must be a live event; out must be writable and aligned.
int32_t flexaudio_event_startup_reason_v2(const struct FlexEventV2 *value, int32_t *out);

// Borrow the typed watcher failure; return 1 when present, 0 for other event arms.
// # Safety
// value must be live; out must be writable and aligned. The view lives until value is freed.
int32_t flexaudio_device_event_error_v2(const struct FlexDeviceEventV2 *value,
                                        const struct FlexErrorInfoV2 **out);

// Borrow the optional typed addon cause code.
// # Safety
// value must be live; out must be writable and aligned. Never free the string separately.
int32_t flexaudio_error_addon_kind_v2(const struct FlexErrorInfoV2 *value, const char **out);

// Borrow the optional opt-in addon cause message.
// # Safety
// value must be live; out must be writable and aligned. Never free the string separately.
int32_t flexaudio_error_addon_message_v2(const struct FlexErrorInfoV2 *value, const char **out);

// Count opt-in native diagnostic details, excluded from the safe display message.
// # Safety
// value must be live; out must be writable and aligned.
int32_t flexaudio_error_diagnostic_count_v2(const struct FlexErrorInfoV2 *value, uint32_t *out);

// Borrow a native diagnostic detail. Treat its contents as private host data.
// # Safety
// value must be live; out must be writable and aligned. Never free the string separately.
int32_t flexaudio_error_diagnostic_detail_v2(const struct FlexErrorInfoV2 *value,
                                             uint32_t index,
                                             const char **out);

// Open an unstarted stream with optional VAD, PID exclusions and secondary output.
// Denoise runs once on canonical capture, before both output formats and VAD.
// Valid out initializes to NULL, including on errors. All options are copied.
// Opens strict by default; use v4 strict = 2 to allow ignored non-default settings.
// # Safety
// Config has at least its size/version words, and complete readable storage when
// they match. Borrowed fields must be readable for the call; out must be writable.
int32_t flexaudio_open_v3(const struct FlexStreamConfigV3 *config, struct FlexStream **out);

// Switch source and optionally replace keepalive. Zero retains the current option.
// Output formats, attached VAD, gain and denoise retain their open-time settings.
// Failure retains the old options. No model is created by this operation.
// # Safety
// Stream must be live and exclusively borrowed. Config's header and, on a match,
// complete envelope and borrowed fields must be readable during this call.
int32_t flexaudio_switch_source_v3(struct FlexStream *stream,
                                   const struct FlexSwitchConfigV3 *config);

// Open using exact-size v4/v3 envelopes. Strict preflight precedes native/model work.
// out is NULL on failure. Options and all borrowed configuration are copied.
// # Safety
// Envelopes have readable header words and full storage only when those match.
// Their fields are readable for this call, and out is writable.
int32_t flexaudio_open_v4(const struct FlexStreamConfigV4 *config, struct FlexStream **out);

// Switch source with retained strictness unless explicitly replaced. Gain, capacity,
// denoise, VAD and output formats retain their open-time configuration.
// Failure preserves the previous source, options and settings report.
// # Safety
// stream is live/exclusively borrowed; envelopes and fields obey open_v4's requirements.
int32_t flexaudio_switch_source_v4(struct FlexStream *stream,
                                   const struct FlexSwitchConfigV4 *config);

// Open with presence-aware gains and unchanged v4 controls, reports and strict default.
// NULL gain pointers select unity; explicit zero mutes. Nested FlexConfig gain,
// mix_mic_gain and mix_system_gain must be zero; otherwise InvalidArg names the
// field and directs callers to FlexGainsV5 before any native access.
// out is NULL on failure. All borrowed configuration is copied.
// # Safety
// Envelopes have readable header words and full storage only when those match.
// Nested fields and supplied floats are readable for this call; out is writable.
int32_t flexaudio_open_v5(const struct FlexStreamConfigV5 *config, struct FlexStream **out);

// Switch with v4 strict semantics (0 retain, 1 on, 2 off). Omitted output gain
// retains the current gain. Explicit non-default output gain, including zero,
// returns FLEX_IGNORED_SETTINGS under strict mode, or is ignored in permissive mode.
// Mix gains use literal values or unity defaults. Failure preserves the old source/report.
// Nested FlexConfig gain fields must be zero, as on open_v5; nonzero fields
// return InvalidArg directing callers to FlexGainsV5 before any native access.
// # Safety
// stream is live/exclusively borrowed; envelopes and fields obey open_v5's requirements.
int32_t flexaudio_switch_source_v5(struct FlexStream *stream,
                                   const struct FlexSwitchConfigV5 *config);

// Five pinned defaults. Supplied structs use literal fields, including zero.
struct FlexVadOptions flexaudio_vad_default_options(void);

// Create a session; NULL params/options use defaults. Returns NULL plus last_error on error.
// # Safety
// Optional pointers must refer to initialized aligned structs.
struct FlexVad *flexaudio_vad_new(const struct FlexVadOptions *params,
                                  const struct FlexVadRuntimeOptions *options);

// Feed normalized interleaved PCM at 8000–192000 Hz and a positive channel count.
// Input is converted to mono16k internally; event times use the input timeline,
// compensated for converter group delay. After raw input validation, finite resampler
// overshoot is clipped internally to [-1, 1]. Use 16000/1 for direct mono16k input.
// On failure still drain/free the owned terminal event array.
// # Safety
// Handle is exclusively owned; input/output allocations must be valid for their lengths.
int32_t flexaudio_vad_process(struct FlexVad *v,
                              const float *samples,
                              size_t len,
                              uint32_t input_sample_rate,
                              uint16_t input_channels,
                              struct FlexVadEvent **out,
                              size_t *out_len);

// Return owned events, including terminal closure on failure.
// # Safety
// Handle and mandatory outputs must be valid and exclusively owned during mutation.
int32_t flexaudio_vad_finish(struct FlexVad *v, struct FlexVadEvent **out, size_t *out_len);

// Return owned events, including terminal closure on failure.
// # Safety
// Handle and mandatory outputs must be valid and exclusively owned during mutation.
int32_t flexaudio_vad_reset(struct FlexVad *v, struct FlexVadEvent **out, size_t *out_len);

// Borrow latest probabilities until the next session mutation/free. Empty arrays return NULL.
// # Safety
// Handle and all mandatory output pointers must be valid.
int32_t flexaudio_vad_probabilities(const struct FlexVad *v,
                                    uint64_t *first_frame,
                                    const float **out,
                                    size_t *out_len);

// Create the inference-independent 16k/512 probability processor. NULL uses defaults.
// # Safety
// Params must be NULL or a valid aligned struct.
struct FlexVadPostProcessor *flexaudio_vad_postprocessor_new(const struct FlexVadOptions *p);

// Feed finite [0,1] probabilities. Validation preserves processor state.
// # Safety
// Handle, input and output allocations must be valid for their lengths.
int32_t flexaudio_vad_postprocessor_process(struct FlexVadPostProcessor *v,
                                            const float *samples,
                                            size_t len,
                                            struct FlexSpeechSegment **out,
                                            size_t *out_len);

// Finish probability segmentation without inference.
// # Safety
// Handle and outputs must be valid and exclusively owned during mutation.
int32_t flexaudio_vad_postprocessor_finish(struct FlexVadPostProcessor *v,
                                           struct FlexSpeechSegment **out,
                                           size_t *out_len);

// Discard the probability timeline.
// # Safety
// Handle must be valid and exclusively owned.
int32_t flexaudio_vad_postprocessor_reset(struct FlexVadPostProcessor *v);

// Release an exclusively owned handle. NULL is safe.
// # Safety
// Non-NULL must be a live handle of this exact type returned by its constructor.
void flexaudio_vad_free(struct FlexVad *v);

// Release an exclusively owned handle. NULL is safe.
// # Safety
// Non-NULL must be a live handle of this exact type returned by its constructor.
void flexaudio_vad_postprocessor_free(struct FlexVadPostProcessor *v);

// Release an owned result array with its original length. NULL/0 is safe.
// # Safety
// Non-NULL must be an array of this exact type and original length returned by this API.
void flexaudio_vad_events_free(struct FlexVadEvent *v, size_t len);

// Release an owned result array with its original length. NULL/0 is safe.
// # Safety
// Non-NULL must be an array of this exact type and original length returned by this API.
void flexaudio_vad_segments_free(struct FlexSpeechSegment *v, size_t len);

// Open a versioned primary stream, returning a typed result code and last_error.
// Attachment uses the producer canonical branch before output conversion.
// Opens strict by default; use v4 strict = 2 to allow ignored non-default settings.
// # Safety
// Config and its borrowed fields must be valid; out must be writable.
int32_t flexaudio_open_v2(const struct FlexStreamConfigV2 *config, struct FlexStream **out);

// Poll a versioned chunk: 1 available, 0 absent, negative result on error.
// PCM polling does not wait for event polling, including after stop.
// Startup notices are enqueued before publishing their generation's first chunk;
// event-first callers can drain the ordinary FIFO event queue before PCM.
// # Safety
// Stream must be exclusively owned; out must be writable, with any prior chunk already freed.
int32_t flexaudio_poll_chunk_v2(struct FlexStream *s, struct FlexChunkV2 *out);

// Free all allocations in a versioned chunk, then clear its fields. NULL is safe.
// # Safety
// Non-NULL must be a chunk returned by poll_chunk_v2 and must not have been freed already.
void flexaudio_chunk_free_v2(struct FlexChunkV2 *chunk);

// Flush a vad_tap epoch. Disabled attachment is a no-op.
// Failures return their root code immediately; closing events remain available via poll_chunk_v2.
// # Safety
// s must be a valid, exclusively owned stream handle.
int32_t flexaudio_flush_vad(struct FlexStream *s);

// Borrow attached events until chunk_free_v2; NULL when none are present.
// # Safety
// chunk must point to a live versioned chunk; len must be writable when non-NULL.
const struct FlexAttachedVadEvent *flexaudio_chunk_vad_events(const struct FlexChunkV2 *chunk,
                                                              size_t *len);

// Start monitoring device connection and default changes, then return a watcher handle.
//
// Linux reports PipeWire system output additions, removals and default changes using
// the same output IDs as `devices()`. Microphone and default-microphone changes are
// not reported: CPAL microphone IDs cannot be resolved from PipeWire nodes.
// Windows, macOS and unsupported operating systems return a valid no-op handle.
// Linux startup failure returns NULL with `flexaudio_last_error` and typed
// `flexaudio_last_error_info_v2`, rather than a no-op handle. Release it with
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
