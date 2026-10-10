//! Versioned capture boundary. Exact producer provenance is a prerequisite for attachment.
use crate::whisper_types::*;
use crate::whisper_vad::params_from_c;
use crate::{
    error::{clear_last_error, code, set_last_error},
    guard_i32,
    types::{FlexChunk, FlexConfig, FlexStream},
};
use std::{mem, ptr};

pub const FLEX_STREAM_VERSION_2: u32 = 2;
pub const FLEX_WHISPER_PRIMARY: u32 = 1;
pub const FLEX_WHISPER_SECONDARY: u32 = 2;
pub const FLEX_WHISPER_UNSUPPORTED_TAP: i32 = -5;
pub const FLEX_WHISPER_UNSUPPORTED_CONVERSION_CLOCK: i32 = -6;
pub const FLEX_WHISPER_CONFLICTING_VAD: i32 = -7;
pub const FLEX_WHISPER_EPOCH_START: u32 = 6;

/// Fixed-width new-mode attachment settings. Only primary is supported by this binding.
#[repr(C)]
pub struct FlexWhisperVadStreamOptions {
    pub params: FlexWhisperVadParams,
    /// 0 or 1, never a default sentinel.
    pub provisional: u8,
    /// FLEX_WHISPER_PRIMARY or FLEX_WHISPER_SECONDARY.
    pub tap: u32,
}
/// Versioned envelope around the frozen v1 configuration. All pointers are borrowed during open.
#[repr(C)]
pub struct FlexStreamConfigV2 {
    pub size: u32,
    pub version: u32,
    pub config: *const FlexConfig,
    /// NULL disables whisper attachment.
    pub whisper_vad: *const FlexWhisperVadStreamOptions,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FlexWhisperEpochStart {
    pub capture_sample: u64,
    pub pts_ns: i64,
}
/// Attached union is distinct from the standalone event union.
#[repr(C)]
#[derive(Clone, Copy)]
pub union FlexAttachedWhisperVadPayload {
    pub vad: FlexWhisperVadPayload,
    pub epoch_start: FlexWhisperEpochStart,
}
/// Versioned attached event. Epoch start is sequence 0 before all VAD payloads.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FlexAttachedWhisperVadEvent {
    pub size: u32,
    pub version: u32,
    pub r#type: u32,
    pub epoch: u32,
    pub seq: u64,
    pub data: FlexAttachedWhisperVadPayload,
}
/// Owns v1 PCM/events and attached events until flexaudio_chunk_free_v2.
#[repr(C)]
pub struct FlexChunkV2 {
    pub size: u32,
    pub version: u32,
    pub chunk: FlexChunk,
    pub whisper_vad_events: *mut FlexAttachedWhisperVadEvent,
    pub whisper_vad_events_len: usize,
}

pub(crate) fn attached_to_c(
    e: flexaudio_vad::AttachedWhisperVadEvent,
) -> FlexAttachedWhisperVadEvent {
    let mut out: FlexAttachedWhisperVadEvent = unsafe { mem::zeroed() };
    out.size =
        u32::try_from(mem::size_of::<FlexAttachedWhisperVadEvent>()).expect("fixed ABI size");
    out.version = FLEX_STREAM_VERSION_2;
    out.epoch = e.epoch();
    out.seq = e.seq();
    match e {
        flexaudio_vad::AttachedWhisperVadEvent::EpochStart {
            capture_sample,
            pts_ns,
            ..
        } => {
            out.r#type = FLEX_WHISPER_EPOCH_START;
            out.data.epoch_start.capture_sample = capture_sample;
            out.data.epoch_start.pts_ns = pts_ns;
        }
        flexaudio_vad::AttachedWhisperVadEvent::Vad(e) => {
            let event = crate::whisper_vad::event_to_c(e);
            out.r#type = event.r#type;
            out.data.vad = event.data;
        }
    }
    out
}

fn reject(status: i32, message: &str) -> i32 {
    set_last_error(message);
    status
}

unsafe fn validate<'a>(config: *const FlexStreamConfigV2) -> Result<&'a FlexConfig, i32> {
    if config.is_null() || !config.is_aligned() {
        return Err(reject(
            code::FLEX_INVALID_ARG,
            "InvalidArgument: invalid versioned config pointer",
        ));
    }
    let config = &*config;
    if usize::try_from(config.size).ok() != Some(mem::size_of::<FlexStreamConfigV2>())
        || config.version != FLEX_STREAM_VERSION_2
    {
        return Err(reject(
            code::FLEX_INVALID_ARG,
            "InvalidArgument: config size/version mismatch",
        ));
    }
    if config.config.is_null() || !config.config.is_aligned() {
        return Err(reject(
            code::FLEX_INVALID_ARG,
            "InvalidArgument: invalid legacy config pointer",
        ));
    }
    let base = &*config.config;
    if !config.whisper_vad.is_null() {
        if !config.whisper_vad.is_aligned() {
            return Err(reject(
                code::FLEX_INVALID_ARG,
                "InvalidArgument: misaligned whisper options",
            ));
        }
        let options = &*config.whisper_vad;
        if base.has_vad {
            return Err(reject(
                FLEX_WHISPER_CONFLICTING_VAD,
                "ConflictingVad: legacy and whisper VAD are mutually exclusive",
            ));
        }
        if options.tap != FLEX_WHISPER_PRIMARY {
            return Err(reject(
                FLEX_WHISPER_UNSUPPORTED_TAP,
                "UnsupportedTap: only primary is supported",
            ));
        }
        if options.provisional > 1 {
            return Err(reject(
                code::FLEX_INVALID_ARG,
                "InvalidArgument: provisional must be 0 or 1",
            ));
        }
        params_from_c(&options.params)
            .map_err(|message| reject(code::FLEX_INVALID_ARG, message))?;
        // The facade currently has neither authoritative canonical indices nor valid-tail counts.
        // Inferring them from delivered chunks would silently corrupt origins after drops/padding.
        return Err(reject(FLEX_WHISPER_UNSUPPORTED_CONVERSION_CLOCK,
            "UnsupportedConversionClock: capture producer lacks canonical sample provenance and valid tail frames"));
    }
    Ok(base)
}

/// Open a versioned primary stream, returning a typed result code and last_error.
/// Attachment fails closed until the capture producer supplies exact canonical provenance.
/// # Safety
/// Config and its borrowed fields must be valid; out must be writable.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_open_v2(
    config: *const FlexStreamConfigV2,
    out: *mut *mut FlexStream,
) -> i32 {
    guard_i32(|| {
        clear_last_error();
        if out.is_null() || !out.is_aligned() {
            return reject(
                code::FLEX_INVALID_ARG,
                "InvalidArgument: invalid stream output",
            );
        }
        out.write(ptr::null_mut());
        let config = match validate(config) {
            Ok(c) => c,
            Err(status) => return status,
        };
        let stream = crate::flexaudio_open(config);
        if stream.is_null() {
            code::FLEX_FAILURE
        } else {
            out.write(stream);
            code::FLEX_OK
        }
    })
}

/// Poll a versioned chunk: 1 available, 0 absent, negative result on error.
/// # Safety
/// Stream must be exclusively owned; out must be writable, with any prior chunk already freed.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_poll_chunk_v2(s: *mut FlexStream, out: *mut FlexChunkV2) -> i32 {
    guard_i32(|| {
        clear_last_error();
        if out.is_null() || !out.is_aligned() {
            return reject(
                code::FLEX_INVALID_ARG,
                "InvalidArgument: invalid chunk output",
            );
        }
        out.write(mem::zeroed());
        (*out).size = u32::try_from(mem::size_of::<FlexChunkV2>()).expect("fixed ABI size");
        (*out).version = FLEX_STREAM_VERSION_2;
        crate::flexaudio_poll_chunk(s, &mut (*out).chunk)
    })
}
/// Free all allocations in a versioned chunk, then clear its fields. NULL is safe.
/// # Safety
/// Non-NULL must be a chunk returned by poll_chunk_v2 and must not have been freed already.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_chunk_free_v2(chunk: *mut FlexChunkV2) {
    guard_i32(|| {
        if !chunk.is_null() {
            crate::flexaudio_chunk_free(&mut (*chunk).chunk);
            if !(*chunk).whisper_vad_events.is_null() {
                drop(Box::from_raw(ptr::slice_from_raw_parts_mut(
                    (*chunk).whisper_vad_events,
                    (*chunk).whisper_vad_events_len,
                )));
            }
            chunk.write(mem::zeroed());
        }
        code::FLEX_OK
    });
}
/// Flush a whisper epoch. Disabled attachment is a no-op.
/// # Safety
/// s must be a valid, exclusively owned stream handle.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_flush_whisper_vad(s: *mut FlexStream) -> i32 {
    guard_i32(|| {
        clear_last_error();
        if s.is_null() || !s.is_aligned() {
            return reject(
                code::FLEX_INVALID_ARG,
                "InvalidArgument: invalid stream handle",
            );
        }
        code::FLEX_OK
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn attached_origin_has_separate_tag_and_exact_u64() {
        let e = attached_to_c(flexaudio_vad::AttachedWhisperVadEvent::EpochStart {
            epoch: 3,
            seq: 0,
            capture_sample: 9_007_199_254_740_993,
            pts_ns: 123,
        });
        assert_eq!((e.r#type, e.epoch, e.seq), (FLEX_WHISPER_EPOCH_START, 3, 0));
        unsafe {
            assert_eq!(e.data.epoch_start.capture_sample, 9_007_199_254_740_993);
            assert_eq!(e.data.epoch_start.pts_ns, 123);
        }
    }
    #[test]
    fn config_version_and_pointer_validation_without_device_access() {
        unsafe {
            let mut out = ptr::dangling_mut();
            assert_eq!(
                flexaudio_open_v2(ptr::null(), &mut out),
                code::FLEX_INVALID_ARG
            );
            assert!(out.is_null());
            let config = FlexStreamConfigV2 {
                size: 0,
                version: 2,
                config: ptr::null(),
                whisper_vad: ptr::null(),
            };
            assert_eq!(flexaudio_open_v2(&config, &mut out), code::FLEX_INVALID_ARG);
            assert!(out.is_null());
            flexaudio_chunk_free_v2(ptr::null_mut());
        }
    }
    #[test]
    fn attachment_conflicts_and_capabilities_fail_before_open() {
        unsafe {
            let mut base: FlexConfig = mem::zeroed();
            let mut options = FlexWhisperVadStreamOptions {
                params: crate::whisper_vad::flexaudio_whisper_vad_default_params(),
                provisional: 0,
                tap: FLEX_WHISPER_SECONDARY,
            };
            let config = FlexStreamConfigV2 {
                size: u32::try_from(mem::size_of::<FlexStreamConfigV2>()).unwrap(),
                version: 2,
                config: &base,
                whisper_vad: &options,
            };
            let mut out = ptr::null_mut();
            assert_eq!(
                flexaudio_open_v2(&config, &mut out),
                FLEX_WHISPER_UNSUPPORTED_TAP
            );
            options.tap = FLEX_WHISPER_PRIMARY;
            assert_eq!(
                flexaudio_open_v2(&config, &mut out),
                FLEX_WHISPER_UNSUPPORTED_CONVERSION_CLOCK
            );
            base.has_vad = true;
            assert!(base.has_vad);
            assert_eq!(
                flexaudio_open_v2(&config, &mut out),
                FLEX_WHISPER_CONFLICTING_VAD
            );
            assert!(out.is_null());
        }
    }
}
