//! Frozen 0.4 v1 physical layouts for the supported 64-bit C ABIs.
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
fn v1_all_fields_retain_04_supported_64_bit_layout() {
    assert_eq!(size_of::<i32>(), 4);
    assert_eq!(size_of::<u8>(), 1);
    assert_eq!(size_of::<FlexSourceKind>(), 4);
    assert_eq!(size_of::<FlexProcessMode>(), 4);
    assert_eq!(size_of::<FlexEventKind>(), 4);
    assert_eq!(size_of::<FlexOutputActivity>(), 4);
    assert_layout!(FlexVadConfig, 28, 4;
        threshold = 0 => f32, neg_threshold = 4 => f32, min_speech_ms = 8 => u32,
        min_silence_ms = 12 => u32, speech_pad_ms = 16 => u32, max_speech_ms = 20 => u32, sample_rate = 24 => u32);
    assert_layout!(FlexVadEvent, 16, 8; kind = 0 => i32, at_sample = 8 => i64);
    assert_layout!(FlexConfig, 104, 8;
        kind = 0 => FlexSourceKind, device_id = 8 => *const c_char, process_id = 16 => u32, mode = 20 => FlexProcessMode, exclude_self = 24 => bool,
        output_rate = 28 => u32, output_channels = 32 => u16, chunk_ms = 36 => u32, gain = 40 => f32,
        mix_mic_device_id = 48 => *const c_char, mix_system_device_id = 56 => *const c_char, mix_mic_gain = 64 => f32,
        mix_system_gain = 68 => f32, denoise = 72 => bool, has_vad = 73 => bool, vad = 76 => FlexVadConfig);
    assert_layout!(FlexChunk, 72, 8;
        data = 0 => *mut f32, len = 8 => usize, frames = 16 => u32, pts_ns = 24 => i64, seq = 32 => u64, flags = 40 => u32,
        dropped_before = 44 => u32, peak = 48 => f32, rms = 52 => f32, vad_events = 56 => *mut FlexVadEvent, vad_events_len = 64 => usize);
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
