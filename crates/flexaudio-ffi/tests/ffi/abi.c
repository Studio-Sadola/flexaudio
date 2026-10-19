/* Supported 64-bit C ABIs: frozen layouts plus additive v4 declarations. */
#include "flexaudio.h"
#include <stddef.h>

#define L(T, S, A, F, O) \
    _Static_assert(sizeof(T) == (S), #T " size"); \
    _Static_assert(_Alignof(T) == (A), #T " alignment"); \
    _Static_assert(offsetof(T, F) == (O), #T "." #F " offset")

#if UINTPTR_MAX == UINT64_MAX
L(FlexConfig, 80, 8, silence_probe, 73);
L(FlexChunk, 56, 8, rms, 52);
L(FlexEvent, 16, 8, count, 8);
L(FlexDeviceInfo, 32, 8, is_default, 27);
L(FlexProcessInfo, 40, 8, output_activity, 32);
L(FlexDeviceEvent, 40, 8, is_default, 35);
L(FlexOsVersionV1, 24, 4, build, 20);
L(FlexAvailabilityV1, 72, 8, unknown, 64);
L(FlexSourceCapabilityV1, 80, 8, availability, 8);
L(FlexSystemExclusionCapabilitiesV1, 96, 8, device_selection, 92);
L(FlexProcessCapabilitiesV1, 8, 4, exclude_scope, 4);
L(FlexDeviceWatcherCapabilitiesV1, 88, 8, scopes_len, 80);
L(FlexSideEffectCapabilityV1, 96, 8, condition, 88);
L(FlexVadOptions, 20, 4, speech_pad_ms, 16);
L(FlexVadRuntimeOptions, 1, 1, provisional, 0);
L(FlexSpeechSegment, 16, 8, end_ms, 8);
L(FlexVadSpeechStart, 8, 8, at_ms, 0);
L(FlexVadSpeechEnd, 16, 8, reason, 8);
L(FlexVadCut, 24, 8, reason, 16);
L(FlexVadEpochEnd, 4, 4, reason, 0);
L(FlexVadPayload, 24, 8, cut, 0);
L(FlexVadEvent, 40, 8, data, 16);
L(FlexStreamVadOptions, 28, 4, tap, 24);
L(FlexStreamConfigV2, 24, 8, vad, 16);
L(FlexVadEpochStart, 16, 8, pts_ns, 8);
L(FlexAttachedVadPayload, 24, 8, epoch_start, 0);
L(FlexAttachedVadEvent, 48, 8, data, 24);
L(FlexChunkV2, 80, 8, vad_events_len, 72);
L(FlexAudioLossV2, 40, 8, channels, 36);
L(FlexNativeFormatV2, 8, 4, channels, 4);
L(FlexNativeFormatChangeV2, 16, 4, actual, 8);
L(FlexErrorContextV2, 32, 8, native_call, 24);
L(FlexStreamConfigV3, 56, 8, windows_classic_loopback_keepalive, 48);
L(FlexSwitchConfigV3, 40, 8, windows_classic_loopback_keepalive, 32);
L(FlexSecondaryOutputV3, 20, 4, encoding, 16);
L(FlexSecondaryMetadataV3, 64, 8, rms, 60);
L(FlexOptionsV4, 24, 8, ring_capacity_chunks, 16);
L(FlexStreamConfigV4, 24, 8, options, 16);
L(FlexSwitchConfigV4, 24, 8, options, 16);
L(FlexPidsV4, 16, 8, len, 8);
L(FlexFormatV4, 8, 4, channels, 4);
L(FlexValuePayloadV4, 16, 8, pids, 0);
L(FlexValueV4, 24, 8, value, 8);
L(FlexSettingResolutionV4, 72, 8, effective, 48);
L(FlexNamespacedDeviceV4, 40, 8, id_namespace, 32);
#endif

_Static_assert(FLEX_STREAM_VERSION_4 == 4, "v4 version");
_Static_assert(FLEX_IGNORED_SETTINGS == -13, "strict error");
_Static_assert(FLEX_VALUE_FORMAT == 6, "value discriminator");
_Static_assert(sizeof(FlexValueKindV4) == 4, "value enum size");
_Static_assert(_Alignof(FlexValueKindV4) == 4, "value enum alignment");
_Static_assert(FLEX_LIFECYCLE_CLASSIC_LOOPBACK_KEEPALIVE_STOPPED == 4, "lifecycle kind");

/* Compiler-checked signatures, including constness and borrowed error views. */
int main(void) {
    int32_t (*open_v4)(const FlexStreamConfigV4 *, FlexStream **) = flexaudio_open_v4;
    int32_t (*switch_v4)(FlexStream *, const FlexSwitchConfigV4 *) = flexaudio_switch_source_v4;
    int32_t (*settings)(const FlexStream *, FlexSettingsV4 **) = flexaudio_settings_v4;
    int32_t (*count)(const FlexSettingsV4 *, uint64_t *, size_t *) = flexaudio_settings_count_v4;
    int32_t (*item)(const FlexSettingsV4 *, size_t, FlexSettingResolutionV4 *) = flexaudio_settings_item_v4;
    int32_t (*error)(const FlexErrorInfoV2 *, const FlexSettingsV4 **) = flexaudio_error_settings_v4;
    int32_t (*poll)(FlexStream *, FlexLifecycleEventV4 **) = flexaudio_poll_lifecycle_event_v4;
    int32_t (*kind)(const FlexLifecycleEventV4 *, uint32_t *) = flexaudio_lifecycle_event_kind_v4;
    int32_t (*reason)(const FlexLifecycleEventV4 *, uint32_t *) = flexaudio_lifecycle_event_reason_v4;
    int32_t (*message)(const FlexLifecycleEventV4 *, const char **) = flexaudio_lifecycle_event_message_v4;
    int32_t (*arrival)(const FlexChunk *, int64_t *) = flexaudio_chunk_arrival_ns_v4;
    int32_t (*secondary)(const FlexSecondaryChunkV3 *, int64_t *) = flexaudio_secondary_arrival_ns_v4;
    int32_t (*devices)(FlexNamespacedDeviceV4 **, size_t *) = flexaudio_devices_with_namespaces_v4;
    void (*free_settings)(FlexSettingsV4 *) = flexaudio_settings_free_v4;
    void (*free_event)(FlexLifecycleEventV4 *) = flexaudio_lifecycle_event_free_v4;
    void (*free_devices)(FlexNamespacedDeviceV4 *, size_t) = flexaudio_namespaced_devices_free_v4;
    (void)open_v4; (void)switch_v4; (void)settings; (void)count; (void)item; (void)error;
    (void)poll; (void)kind; (void)reason; (void)message; (void)arrival; (void)secondary;
    (void)devices; (void)free_settings; (void)free_event; (void)free_devices;
    return 0;
}
