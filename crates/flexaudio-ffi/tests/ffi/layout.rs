//! Physical layouts for the 0.6 C ABI for the supported 64-bit C ABIs.
//!
//! These expectations apply to Linux x86_64/aarch64, Windows x86_64 MSVC,
//! and macOS x86_64/aarch64: four-byte enums, one-byte bool, eight-byte
//! pointers and i64 alignment. Each host executes its own assertions; a Linux
//! run makes no claim that Windows or macOS execution occurred. C builds using
//! -fshort-enums are outside the supported ABI.

use crate::types::*;
use crate::watch::FlexDeviceEvent;
use std::mem::{align_of, offset_of, size_of, MaybeUninit};
use std::os::raw::c_char;

fn field_layout<T>(_: *const T) -> (usize, usize) {
    (size_of::<T>(), align_of::<T>())
}

macro_rules! assert_layout {
    ($ty:ty, $size:expr, $align:expr; $($field:ident = $offset:expr => $old:ty),+ $(,)?) => {
        assert_eq!(size_of::<$ty>(), $size, stringify!($ty));
        assert_eq!(align_of::<$ty>(), $align, stringify!($ty));
        $(assert_eq!(offset_of!($ty, $field), $offset,
            concat!(stringify!($ty), ".", stringify!($field)));
            let value = MaybeUninit::<$ty>::uninit();
            // SAFETY: addr_of forms a field pointer without reading uninitialized memory.
            let field = unsafe { std::ptr::addr_of!((*value.as_ptr()).$field) };
            assert_eq!(field_layout(field), (size_of::<$old>(), align_of::<$old>()),
                concat!(stringify!($ty), ".", stringify!($field), " field layout"));)+
    };
}

#[test]
#[cfg(all(
    target_pointer_width = "64",
    any(target_os = "linux", target_os = "windows", target_os = "macos")
))]
fn all_fields_have_supported_64_bit_layout() {
    assert_eq!(size_of::<i32>(), 4);
    assert_eq!(size_of::<u8>(), 1);
    assert_eq!(size_of::<FlexSourceKind>(), 4);
    assert_eq!(size_of::<FlexProcessMode>(), 4);
    assert_eq!(size_of::<FlexEventKind>(), 4);
    assert_eq!(size_of::<FlexOutputActivity>(), 4);
    assert_layout!(FlexConfig, 80, 8;
        kind = 0 => FlexSourceKind, device_id = 8 => *const c_char, process_id = 16 => u32, mode = 20 => FlexProcessMode, exclude_self = 24 => bool,
        output_rate = 28 => u32, output_channels = 32 => u16, chunk_ms = 36 => u32, gain = 40 => f32,
        mix_mic_device_id = 48 => *const c_char, mix_system_device_id = 56 => *const c_char, mix_mic_gain = 64 => f32,
        mix_system_gain = 68 => f32, denoise = 72 => u8, silence_probe = 73 => u8);
    assert_layout!(FlexChunk, 56, 8;
        data = 0 => *mut f32, len = 8 => usize, frames = 16 => u32, pts_ns = 24 => i64, seq = 32 => u64, flags = 40 => u32,
        dropped_before = 44 => u32, peak = 48 => f32, rms = 52 => f32);
    assert_layout!(FlexEvent, 16, 8; kind = 0 => FlexEventKind, count = 8 => i64);
    assert_layout!(FlexDeviceInfo, 32, 8;
        id = 0 => *mut c_char, name = 8 => *mut c_char, source_kind = 16 => FlexSourceKind, sample_rate = 20 => u32, channels = 24 => u16,
        is_loopback = 26 => bool, is_default = 27 => bool);
    assert_layout!(FlexProcessInfo, 40, 8;
        pid = 0 => u32, name = 8 => *mut c_char, executable = 16 => *mut c_char, bundle_id = 24 => *mut c_char, output_activity = 32 => FlexOutputActivity);
    assert_layout!(FlexDeviceEvent, 40, 8;
        kind = 0 => crate::watch::FlexDeviceEventKind, id = 8 => *mut c_char, name = 16 => *mut c_char, source_kind = 24 => FlexSourceKind, sample_rate = 28 => u32,
        channels = 32 => u16, is_loopback = 34 => bool, is_default = 35 => bool);
}

#[test]
#[cfg(all(
    target_pointer_width = "64",
    any(target_os = "linux", target_os = "windows", target_os = "macos")
))]
fn additive_capability_and_v3_layouts() {
    use crate::capability_records::*;
    use crate::v3_types::*;
    assert_layout!(FlexStreamConfigV3, 56, 8;
        size = 0 => u32, version = 4 => u32, config = 8 => *const FlexConfig,
        vad = 16 => *const crate::vad_integration::FlexStreamVadOptions,
        exclude_pids = 24 => *const u32, exclude_pids_len = 32 => usize,
        secondary_output = 40 => *const FlexSecondaryOutputV3,
        windows_classic_loopback_keepalive = 48 => u32);
    assert_layout!(FlexSwitchConfigV3, 40, 8;
        size = 0 => u32, version = 4 => u32, config = 8 => *const FlexConfig,
        exclude_pids = 16 => *const u32, exclude_pids_len = 24 => usize,
        windows_classic_loopback_keepalive = 32 => u32);
    assert_layout!(FlexSecondaryOutputV3, 20, 4;
        size = 0 => u32, version = 4 => u32, rate = 8 => u32,
        channels = 12 => u16, encoding = 16 => u32);
    assert_layout!(FlexSecondaryMetadataV3, 64, 8;
        size = 0 => u32, version = 4 => u32, rate = 8 => u32, channels = 12 => u16,
        encoding = 16 => u32, frames = 20 => u32, frame_index = 24 => u64,
        pts_ns = 32 => i64, seq = 40 => u64, flags = 48 => u32,
        dropped_before = 52 => u32, peak = 56 => f32, rms = 60 => f32);
    assert_layout!(FlexOsVersionV1, 24, 4;
        major = 0 => u32, minor = 4 => u32, has_patch = 8 => u32,
        patch = 12 => u32, has_build = 16 => u32, build = 20 => u32);
    assert_layout!(FlexAvailabilityV1, 72, 8;
        status = 0 => u32, reason = 4 => u32, has_os_versions = 8 => u32,
        required = 12 => FlexOsVersionV1, actual = 36 => FlexOsVersionV1,
        unknown = 64 => *const crate::v2_storage::FlexErrorInfoV2);
    assert_layout!(FlexSourceCapabilityV1, 80, 8;
        backend = 0 => u32, availability = 8 => FlexAvailabilityV1);
    assert_layout!(FlexSystemExclusionCapabilitiesV1, 96, 8;
        backend = 0 => u32, availability = 8 => FlexAvailabilityV1, scope = 80 => u32,
        has_max_distinct_pids = 84 => u32, max_distinct_pids = 88 => u32, device_selection = 92 => u32);
    assert_layout!(FlexProcessCapabilitiesV1, 8, 4;
        include_scope = 0 => u32, exclude_scope = 4 => u32);
    assert_layout!(FlexDeviceWatcherCapabilitiesV1, 88, 8;
        availability = 0 => FlexAvailabilityV1, mode = 72 => u32, scopes_len = 80 => usize);
    assert_layout!(FlexSideEffectCapabilityV1, 96, 8;
        availability = 0 => FlexAvailabilityV1, enabled_by_default = 72 => u32,
        sources_len = 80 => usize, condition = 88 => u32);
}

#[test]
#[cfg(all(
    target_pointer_width = "64",
    any(target_os = "linux", target_os = "windows", target_os = "macos")
))]
fn existing_v2_envelopes_and_records_keep_their_layouts() {
    use crate::{v2_records::*, vad_integration::*, vad_types::*};
    assert_layout!(FlexStreamConfigV2, 24, 8;
        size = 0 => u32, version = 4 => u32, config = 8 => *const FlexConfig,
        vad = 16 => *const FlexStreamVadOptions);
    assert_layout!(FlexStreamVadOptions, 28, 4;
        params = 0 => FlexVadOptions, provisional = 20 => u8, tap = 24 => u32);
    assert_layout!(FlexChunkV2, 80, 8;
        size = 0 => u32, version = 4 => u32, chunk = 8 => FlexChunk,
        vad_events = 64 => *mut FlexAttachedVadEvent, vad_events_len = 72 => usize);
    assert_layout!(FlexAttachedVadEvent, 48, 8;
        size = 0 => u32, version = 4 => u32, r#type = 8 => u32,
        epoch = 12 => u32, seq = 16 => u64, data = 24 => FlexAttachedVadPayload);
    assert_layout!(FlexVadEpochStart, 16, 8;
        capture_sample = 0 => u64, pts_ns = 8 => i64);
    assert_layout!(FlexAudioLossV2, 40, 8;
        path = 0 => u32, lane = 4 => u32, tap = 8 => u32, reason = 12 => u32,
        count_known = 16 => u32, samples = 24 => u64,
        sample_rate = 32 => u32, channels = 36 => u16);
    assert_layout!(FlexNativeFormatV2, 8, 4;
        sample_rate = 0 => u32, channels = 4 => u16);
    assert_layout!(FlexNativeFormatChangeV2, 16, 4;
        advertised = 0 => FlexNativeFormatV2, actual = 8 => FlexNativeFormatV2);
    assert_layout!(FlexErrorContextV2, 32, 8;
        operation = 0 => u32, lane = 4 => u32, native_code_kind = 8 => u32,
        native_code = 16 => i64, native_call = 24 => *const c_char);
}

#[test]
#[cfg(target_pointer_width = "64")]
fn standalone_vad_records_and_unions_keep_their_layouts() {
    use crate::{vad_integration::FlexAttachedVadPayload, vad_types::*};
    assert_layout!(FlexVadOptions, 20, 4;
        threshold = 0 => f32, min_speech_ms = 4 => i32, min_silence_ms = 8 => i32,
        max_speech_ms = 12 => u32, speech_pad_ms = 16 => i32);
    assert_layout!(FlexVadRuntimeOptions, 1, 1; provisional = 0 => u8);
    assert_layout!(FlexSpeechSegment, 16, 8; start_ms = 0 => u64, end_ms = 8 => u64);
    assert_layout!(FlexVadSpeechStart, 8, 8; at_ms = 0 => u64);
    assert_layout!(FlexVadSpeechEnd, 16, 8; at_ms = 0 => u64, reason = 8 => u32);
    assert_layout!(FlexVadCut, 24, 8; start_ms = 0 => u64, end_ms = 8 => u64, reason = 16 => u32);
    assert_layout!(FlexVadEpochEnd, 4, 4; reason = 0 => u32);
    assert_layout!(FlexVadPayload, 24, 8;
        segment = 0 => FlexSpeechSegment, speech_start = 0 => FlexVadSpeechStart,
        speech_end = 0 => FlexVadSpeechEnd, cut = 0 => FlexVadCut, epoch_end = 0 => FlexVadEpochEnd);
    assert_layout!(FlexVadEvent, 40, 8;
        r#type = 0 => u32, epoch = 4 => u32, seq = 8 => u64, data = 16 => FlexVadPayload);
    assert_layout!(FlexAttachedVadPayload, 24, 8;
        vad = 0 => FlexVadPayload, epoch_start = 0 => crate::vad_integration::FlexVadEpochStart);
}

#[test]
#[cfg(target_pointer_width = "64")]
fn additive_v4_layouts_match_the_contract() {
    use crate::v4_types::*;
    assert_eq!(size_of::<FlexValueKindV4>(), 4);
    assert_eq!(align_of::<FlexValueKindV4>(), 4);
    assert_layout!(FlexOptionsV4, 24, 8;
        size = 0 => u32, version = 4 => u32, strict = 8 => u32, ring_capacity_chunks = 16 => *const u64);
    assert_layout!(FlexStreamConfigV4, 24, 8;
        size = 0 => u32, version = 4 => u32, base = 8 => *const crate::v3_types::FlexStreamConfigV3,
        options = 16 => *const FlexOptionsV4);
    assert_layout!(FlexSwitchConfigV4, 24, 8;
        size = 0 => u32, version = 4 => u32, base = 8 => *const crate::v3_types::FlexSwitchConfigV3,
        options = 16 => *const FlexOptionsV4);
    assert_layout!(FlexPidsV4, 16, 8; data = 0 => *const u32, len = 8 => usize);
    assert_layout!(FlexFormatV4, 8, 4; sample_rate = 0 => u32, channels = 4 => u16);
    assert_layout!(FlexValuePayloadV4, 16, 8;
        boolean = 0 => bool, unsigned_value = 0 => u64, float_value = 0 => f32,
        text = 0 => *const c_char, pids = 0 => FlexPidsV4, format = 0 => FlexFormatV4);
    assert_layout!(FlexValueV4, 24, 8; kind = 0 => FlexValueKindV4, value = 8 => FlexValuePayloadV4);
    assert_layout!(FlexSettingResolutionV4, 72, 8;
        setting = 0 => *const c_char, status = 8 => u32, reason = 16 => *const c_char,
        requested = 24 => FlexValueV4, effective = 48 => FlexValueV4);
    assert_layout!(FlexNamespacedDeviceV4, 40, 8;
        device = 0 => FlexDeviceInfo, id_namespace = 32 => *const c_char);
}
