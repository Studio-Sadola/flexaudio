//! Thin standalone C adapters over the shared generic VAD session.

use crate::error::{clear_last_error, code, set_audio_error};
use crate::vad_types::*;
use crate::{guard_i32, guard_ptr};
use flexaudio_vad::{
    EpochEndReason, PreviewCloseReason, PreviewCutReason, Vad, VadError, VadEvent, VadEventKind,
    VadFailure, VadOptions, VadPostProcessor, VadRuntimeOptions,
};
use std::{mem, ptr, slice};

/// Opaque exclusively owned session. Construct off the capture callback.
pub struct FlexVad {
    inner: Vad,
}
/// Opaque probability-only segmentation owner.
pub struct FlexVadPostProcessor {
    inner: VadPostProcessor,
}

pub(crate) fn params_from_c(p: &FlexVadOptions) -> Result<VadOptions, &'static str> {
    let duration = |v| u32::try_from(v).map_err(|_| "InvalidParameter: negative duration");
    let params = VadOptions {
        threshold: p.threshold,
        min_speech_ms: duration(p.min_speech_ms)?,
        min_silence_ms: duration(p.min_silence_ms)?,
        max_speech_ms: p.max_speech_ms,
        speech_pad_ms: duration(p.speech_pad_ms)?,
    };
    params
        .validate()
        .map_err(|_| "InvalidParameter: outside the supported parameter domain")?;
    Ok(params)
}

pub(crate) fn error_code(e: &VadError) -> i32 {
    match e.root() {
        VadError::InvalidParameter { .. }
        | VadError::InvalidProbability { .. }
        | VadError::InvalidPcm { .. }
        | VadError::Overflow { .. } => code::FLEX_INVALID_ARG,
        VadError::SessionFinished | VadError::FailedSession => code::FLEX_INVALID_STATE,
        VadError::Inference | VadError::ModelLoad => code::FLEX_FAILURE,
        VadError::Cause { primary, .. } => error_code(primary),
    }
}

fn close_reason(r: PreviewCloseReason) -> u32 {
    match r {
        PreviewCloseReason::Hysteresis => FLEX_VAD_HYSTERESIS,
        PreviewCloseReason::Finish => FLEX_VAD_FINISH,
        PreviewCloseReason::Reset => FLEX_VAD_RESET,
        PreviewCloseReason::Error => FLEX_VAD_ERROR,
    }
}
fn cut_reason(r: PreviewCutReason) -> u32 {
    match r {
        PreviewCutReason::Limit => FLEX_VAD_LIMIT,
        PreviewCutReason::Hysteresis => FLEX_VAD_HYSTERESIS,
        PreviewCutReason::Finish => FLEX_VAD_FINISH,
        PreviewCutReason::Reset => FLEX_VAD_RESET,
        PreviewCutReason::Error => FLEX_VAD_ERROR,
    }
}
fn end_reason(r: EpochEndReason) -> u32 {
    match r {
        EpochEndReason::Finish => FLEX_VAD_FINISH,
        EpochEndReason::Reset => FLEX_VAD_RESET,
        EpochEndReason::Error => FLEX_VAD_ERROR,
    }
}

pub(crate) fn event_to_c(event: VadEvent) -> FlexVadEvent {
    // Zero inactive union storage before writing only the active fields.
    let mut out: FlexVadEvent = unsafe { mem::zeroed() };
    out.epoch = event.epoch;
    out.seq = event.seq;
    match event.kind {
        VadEventKind::Segment(s) => {
            out.r#type = FLEX_VAD_SEGMENT;
            out.data.segment.start_ms = s.start_ms;
            out.data.segment.end_ms = s.end_ms;
        }
        VadEventKind::ProvisionalSpeechStart { at_ms } => {
            out.r#type = FLEX_VAD_SPEECH_START;
            out.data.speech_start.at_ms = at_ms;
        }
        VadEventKind::ProvisionalSpeechEnd { at_ms, reason } => {
            out.r#type = FLEX_VAD_SPEECH_END;
            out.data.speech_end.at_ms = at_ms;
            out.data.speech_end.reason = close_reason(reason);
        }
        VadEventKind::ProvisionalCut {
            start_ms,
            end_ms,
            reason,
        } => {
            out.r#type = FLEX_VAD_CUT;
            out.data.cut.start_ms = start_ms;
            out.data.cut.end_ms = end_ms;
            out.data.cut.reason = cut_reason(reason);
        }
        VadEventKind::EpochEnd { reason } => {
            out.r#type = FLEX_VAD_EPOCH_END;
            out.data.epoch_end.reason = end_reason(reason);
        }
    }
    out
}

pub(crate) unsafe fn input<'a, T>(p: *const T, len: usize) -> Result<&'a [T], &'static str> {
    if len > (isize::MAX as usize) / mem::size_of::<T>() {
        return Err("InvalidArgument: slice length exceeds isize::MAX");
    }
    if len == 0 {
        return Ok(&[]);
    }
    if p.is_null() || !p.is_aligned() {
        return Err("InvalidArgument: null or misaligned input");
    }
    Ok(slice::from_raw_parts(p, len))
}

pub(crate) unsafe fn prepare<T>(out: *mut *mut T, len: *mut usize) -> Result<(), &'static str> {
    // Clear each independently valid output even if its companion is invalid.
    if !out.is_null() && out.is_aligned() {
        out.write(ptr::null_mut());
    }
    if !len.is_null() && len.is_aligned() {
        len.write(0);
    }
    if out.is_null() || len.is_null() || !out.is_aligned() || !len.is_aligned() {
        return Err("InvalidArgument: null or misaligned output");
    }
    Ok(())
}
pub(crate) unsafe fn write_array<T>(items: Vec<T>, out: *mut *mut T, len: *mut usize) {
    if !items.is_empty() {
        let mut items = items.into_boxed_slice();
        len.write(items.len());
        out.write(items.as_mut_ptr());
        mem::forget(items);
    }
}
fn invalid(message: &str) -> i32 {
    crate::error::set_audio_error_with_message(
        flexaudio::Error::InvalidArg(message.into()),
        message.into(),
    );
    code::FLEX_INVALID_ARG
}
unsafe fn read_params(p: *const FlexVadOptions) -> Result<VadOptions, &'static str> {
    if p.is_null() {
        return Ok(VadOptions::default());
    }
    if !p.is_aligned() {
        return Err("InvalidArgument: misaligned params");
    }
    params_from_c(&*p)
}

/// Five pinned defaults. Supplied structs use literal fields, including zero.
#[no_mangle]
pub extern "C" fn flexaudio_vad_default_options() -> FlexVadOptions {
    crate::guard_value(unsafe { mem::zeroed() }, || FlexVadOptions {
        threshold: 0.5,
        min_speech_ms: 250,
        min_silence_ms: 100,
        max_speech_ms: 0,
        speech_pad_ms: 30,
    })
}

/// Create a session; NULL params/options use defaults. Returns NULL plus last_error on error.
/// # Safety
/// Optional pointers must refer to initialized aligned structs.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_vad_new(
    params: *const FlexVadOptions,
    options: *const FlexVadRuntimeOptions,
) -> *mut FlexVad {
    guard_ptr(|| {
        clear_last_error();
        let params = match read_params(params) {
            Ok(p) => p,
            Err(e) => {
                invalid(e);
                return ptr::null_mut();
            }
        };
        let provisional = if options.is_null() {
            false
        } else {
            if !options.is_aligned() || (*options).provisional > 1 {
                invalid("InvalidArgument: provisional must be 0 or 1");
                return ptr::null_mut();
            }
            (*options).provisional == 1
        };
        match Vad::new(params, VadRuntimeOptions { provisional }) {
            Ok(inner) => Box::into_raw(Box::new(FlexVad { inner })),
            Err(e) => {
                set_audio_error(e.audio_error());
                ptr::null_mut()
            }
        }
    })
}

unsafe fn stream_result(
    result: Result<Vec<VadEvent>, VadFailure>,
    out: *mut *mut FlexVadEvent,
    len: *mut usize,
) -> i32 {
    let (events, status) = match result {
        Ok(events) => (events, code::FLEX_OK),
        Err(failure) => {
            let status = error_code(&failure.error);
            set_audio_error(failure.error.audio_error());
            (failure.terminal_events, status)
        }
    };
    write_array(events.into_iter().map(event_to_c).collect(), out, len);
    status
}

/// Feed normalized interleaved PCM at the specified positive rate and channel count.
/// Input is converted to mono16k internally; event times use the input timeline,
/// compensated for converter group delay. After raw input validation, finite resampler
/// overshoot is clipped internally to [-1, 1]. Use 16000/1 for direct mono16k input.
/// On failure still drain/free the owned terminal event array.
/// # Safety
/// Handle is exclusively owned; input/output allocations must be valid for their lengths.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_vad_process(
    v: *mut FlexVad,
    samples: *const f32,
    len: usize,
    input_sample_rate: u32,
    input_channels: u16,
    out: *mut *mut FlexVadEvent,
    out_len: *mut usize,
) -> i32 {
    guard_i32(|| {
        clear_last_error();
        if let Err(e) = prepare(out, out_len) {
            return invalid(e);
        }
        if v.is_null() || !v.is_aligned() {
            return invalid("InvalidArgument: invalid VAD handle");
        }
        let samples = match input(samples, len) {
            Ok(s) => s,
            Err(e) => return invalid(e),
        };
        stream_result(
            (*v).inner
                .process_pcm(samples, input_sample_rate, input_channels),
            out,
            out_len,
        )
    })
}

/// Return owned events, including terminal closure on failure.
/// # Safety
/// Handle and mandatory outputs must be valid and exclusively owned during mutation.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_vad_finish(
    v: *mut FlexVad,
    out: *mut *mut FlexVadEvent,
    out_len: *mut usize,
) -> i32 {
    guard_i32(|| {
        clear_last_error();
        if let Err(e) = prepare(out, out_len) {
            return invalid(e);
        }
        if v.is_null() || !v.is_aligned() {
            return invalid("InvalidArgument: invalid VAD handle");
        }
        stream_result((*v).inner.finish(), out, out_len)
    })
}
/// Return owned events, including terminal closure on failure.
/// # Safety
/// Handle and mandatory outputs must be valid and exclusively owned during mutation.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_vad_reset(
    v: *mut FlexVad,
    out: *mut *mut FlexVadEvent,
    out_len: *mut usize,
) -> i32 {
    guard_i32(|| {
        clear_last_error();
        if let Err(e) = prepare(out, out_len) {
            return invalid(e);
        }
        if v.is_null() || !v.is_aligned() {
            return invalid("InvalidArgument: invalid VAD handle");
        }
        stream_result((*v).inner.reset(), out, out_len)
    })
}

/// Borrow latest probabilities until the next session mutation/free. Empty arrays return NULL.
/// # Safety
/// Handle and all mandatory output pointers must be valid.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_vad_probabilities(
    v: *const FlexVad,
    first_frame: *mut u64,
    out: *mut *const f32,
    out_len: *mut usize,
) -> i32 {
    guard_i32(|| {
        clear_last_error();
        if !first_frame.is_null() && first_frame.is_aligned() {
            first_frame.write(0);
        }
        if !out.is_null() && out.is_aligned() {
            out.write(ptr::null());
        }
        if !out_len.is_null() && out_len.is_aligned() {
            out_len.write(0);
        }
        if v.is_null()
            || !v.is_aligned()
            || first_frame.is_null()
            || !first_frame.is_aligned()
            || out.is_null()
            || !out.is_aligned()
            || out_len.is_null()
            || !out_len.is_aligned()
        {
            return invalid("InvalidArgument: invalid probabilities handle/output");
        }
        let batch = (*v).inner.last_frame_probabilities();
        first_frame.write(batch.first_frame_index);
        out_len.write(batch.values.len());
        if !batch.values.is_empty() {
            out.write(batch.values.as_ptr());
        }
        code::FLEX_OK
    })
}

/// Create the inference-independent 16k/512 probability processor. NULL uses defaults.
/// # Safety
/// Params must be NULL or a valid aligned struct.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_vad_postprocessor_new(
    p: *const FlexVadOptions,
) -> *mut FlexVadPostProcessor {
    guard_ptr(|| {
        clear_last_error();
        let p = match read_params(p) {
            Ok(p) => p,
            Err(e) => {
                invalid(e);
                return ptr::null_mut();
            }
        };
        match VadPostProcessor::new(p) {
            Ok(inner) => Box::into_raw(Box::new(FlexVadPostProcessor { inner })),
            Err(e) => {
                set_audio_error(e.audio_error());
                ptr::null_mut()
            }
        }
    })
}
unsafe fn segment_result(
    result: Result<Vec<flexaudio_vad::SpeechSegment>, VadError>,
    out: *mut *mut FlexSpeechSegment,
    len: *mut usize,
) -> i32 {
    match result {
        Ok(items) => {
            write_array(
                items
                    .into_iter()
                    .map(|s| FlexSpeechSegment {
                        start_ms: s.start_ms,
                        end_ms: s.end_ms,
                    })
                    .collect(),
                out,
                len,
            );
            code::FLEX_OK
        }
        Err(e) => {
            set_audio_error(e.audio_error());
            error_code(&e)
        }
    }
}

/// Feed finite [0,1] probabilities. Validation preserves processor state.
/// # Safety
/// Handle, input and output allocations must be valid for their lengths.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_vad_postprocessor_process(
    v: *mut FlexVadPostProcessor,
    samples: *const f32,
    len: usize,
    out: *mut *mut FlexSpeechSegment,
    out_len: *mut usize,
) -> i32 {
    guard_i32(|| {
        clear_last_error();
        if let Err(e) = prepare(out, out_len) {
            return invalid(e);
        }
        if v.is_null() || !v.is_aligned() {
            return invalid("InvalidArgument: invalid processor handle");
        }
        let samples = match input(samples, len) {
            Ok(s) => s,
            Err(e) => return invalid(e),
        };
        segment_result((*v).inner.process(samples), out, out_len)
    })
}
/// Finish probability segmentation without inference.
/// # Safety
/// Handle and outputs must be valid and exclusively owned during mutation.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_vad_postprocessor_finish(
    v: *mut FlexVadPostProcessor,
    out: *mut *mut FlexSpeechSegment,
    out_len: *mut usize,
) -> i32 {
    guard_i32(|| {
        clear_last_error();
        if let Err(e) = prepare(out, out_len) {
            return invalid(e);
        }
        if v.is_null() || !v.is_aligned() {
            return invalid("InvalidArgument: invalid processor handle");
        }
        segment_result((*v).inner.finish(), out, out_len)
    })
}
/// Discard the probability timeline.
/// # Safety
/// Handle must be valid and exclusively owned.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_vad_postprocessor_reset(v: *mut FlexVadPostProcessor) -> i32 {
    guard_i32(|| {
        clear_last_error();
        if v.is_null() || !v.is_aligned() {
            return invalid("InvalidArgument: invalid processor handle");
        }
        (*v).inner.reset();
        code::FLEX_OK
    })
}

/// Release an exclusively owned handle. NULL is safe.
/// # Safety
/// Non-NULL must be a live handle of this exact type returned by its constructor.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_vad_free(v: *mut FlexVad) {
    guard_i32(|| {
        if !v.is_null() && !v.is_aligned() {
            return invalid("InvalidArgument: misaligned VAD handle");
        }
        if !v.is_null() {
            drop(Box::from_raw(v));
        }
        code::FLEX_OK
    });
}
/// Release an exclusively owned handle. NULL is safe.
/// # Safety
/// Non-NULL must be a live handle of this exact type returned by its constructor.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_vad_postprocessor_free(v: *mut FlexVadPostProcessor) {
    guard_i32(|| {
        if !v.is_null() && !v.is_aligned() {
            return invalid("InvalidArgument: misaligned VAD handle");
        }
        if !v.is_null() {
            drop(Box::from_raw(v));
        }
        code::FLEX_OK
    });
}
/// Release an owned result array with its original length. NULL/0 is safe.
/// # Safety
/// Non-NULL must be an array of this exact type and original length returned by this API.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_vad_events_free(v: *mut FlexVadEvent, len: usize) {
    guard_i32(|| {
        if !(v.is_null() && len == 0)
            && (!crate::convert::valid_array(v, len) || v.is_null() || len == 0)
        {
            return invalid("InvalidArgument: invalid VAD result array");
        }
        if !v.is_null() {
            drop(Box::from_raw(ptr::slice_from_raw_parts_mut(v, len)));
        }
        code::FLEX_OK
    });
}
/// Release an owned result array with its original length. NULL/0 is safe.
/// # Safety
/// Non-NULL must be an array of this exact type and original length returned by this API.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_vad_segments_free(v: *mut FlexSpeechSegment, len: usize) {
    guard_i32(|| {
        if !(v.is_null() && len == 0)
            && (!crate::convert::valid_array(v, len) || v.is_null() || len == 0)
        {
            return invalid("InvalidArgument: invalid VAD result array");
        }
        if !v.is_null() {
            drop(Box::from_raw(ptr::slice_from_raw_parts_mut(v, len)));
        }
        code::FLEX_OK
    });
}

#[cfg(test)]
#[path = "../tests/ffi/vad.rs"]
mod tests;

#[cfg(test)]
mod status_tests {
    #[test]
    fn all_status_codes_are_unique() {
        use crate::error::code::*;
        let codes = [
            FLEX_OK,
            FLEX_INVALID_ARG,
            FLEX_FAILURE,
            FLEX_PANIC,
            FLEX_INVALID_STATE,
            FLEX_DEVICE_NOT_FOUND,
            FLEX_DEVICE_LOST,
            FLEX_PERMISSION_DENIED,
            FLEX_UNSUPPORTED_OS_VERSION,
            FLEX_UNSUPPORTED,
            FLEX_UNSUPPORTED_FORMAT,
            FLEX_NATIVE_FORMAT_CHANGED,
            FLEX_AMBIGUOUS_DEVICE_NAME,
            crate::vad_integration::FLEX_VAD_UNSUPPORTED_TAP,
            crate::vad_integration::FLEX_VAD_UNSUPPORTED_CONVERSION_CLOCK,
        ];
        for (index, code) in codes.iter().enumerate() {
            assert!(!codes[..index].contains(code), "duplicate status {code}");
        }
    }
}
