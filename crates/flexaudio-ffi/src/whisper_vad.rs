//! Thin standalone C adapters over the shared whisper-compatible session.

use crate::error::{clear_last_error, code, set_last_error};
use crate::whisper_types::*;
use crate::{guard_i32, guard_ptr};
use flexaudio_vad::{
    EpochEndReason, PreviewCloseReason, PreviewCutReason, WhisperVad, WhisperVadError,
    WhisperVadEvent, WhisperVadEventKind, WhisperVadFailure, WhisperVadOptions, WhisperVadParams,
    WhisperVadPostProcessor,
};
use std::{mem, ptr, slice};

/// Opaque exclusively owned session. Construct off the capture callback.
pub struct FlexWhisperVad {
    inner: WhisperVad,
}
/// Opaque probability-only segmentation owner.
pub struct FlexWhisperVadPostProcessor {
    inner: WhisperVadPostProcessor,
}

pub(crate) fn params_from_c(p: &FlexWhisperVadParams) -> Result<WhisperVadParams, &'static str> {
    let duration = |v| u32::try_from(v).map_err(|_| "InvalidParameter: negative duration");
    let params = WhisperVadParams {
        threshold: p.threshold,
        min_speech_duration_ms: duration(p.min_speech_duration_ms)?,
        min_silence_duration_ms: duration(p.min_silence_duration_ms)?,
        max_speech_duration_s: p.max_speech_duration_s,
        speech_pad_ms: duration(p.speech_pad_ms)?,
    };
    params
        .validate()
        .map_err(|_| "InvalidParameter: outside the pinned parameter domain")?;
    Ok(params)
}

pub(crate) fn error_code(e: &WhisperVadError) -> i32 {
    match e {
        WhisperVadError::InvalidParameter { .. }
        | WhisperVadError::InvalidProbability { .. }
        | WhisperVadError::InvalidPcm { .. }
        | WhisperVadError::Overflow { .. } => code::FLEX_INVALID_ARG,
        WhisperVadError::SessionFinished | WhisperVadError::FailedSession => {
            code::FLEX_INVALID_STATE
        }
        WhisperVadError::Inference | WhisperVadError::ModelLoad => code::FLEX_FAILURE,
    }
}

fn close_reason(r: PreviewCloseReason) -> u32 {
    match r {
        PreviewCloseReason::Hysteresis => FLEX_WHISPER_HYSTERESIS,
        PreviewCloseReason::Finish => FLEX_WHISPER_FINISH,
        PreviewCloseReason::Reset => FLEX_WHISPER_RESET,
        PreviewCloseReason::Error => FLEX_WHISPER_ERROR,
    }
}
fn cut_reason(r: PreviewCutReason) -> u32 {
    match r {
        PreviewCutReason::Limit => FLEX_WHISPER_LIMIT,
        PreviewCutReason::Hysteresis => FLEX_WHISPER_HYSTERESIS,
        PreviewCutReason::Finish => FLEX_WHISPER_FINISH,
        PreviewCutReason::Reset => FLEX_WHISPER_RESET,
        PreviewCutReason::Error => FLEX_WHISPER_ERROR,
    }
}
fn end_reason(r: EpochEndReason) -> u32 {
    match r {
        EpochEndReason::Finish => FLEX_WHISPER_FINISH,
        EpochEndReason::Reset => FLEX_WHISPER_RESET,
        EpochEndReason::Error => FLEX_WHISPER_ERROR,
    }
}

pub(crate) fn event_to_c(event: WhisperVadEvent) -> FlexWhisperVadEvent {
    // Zero inactive union storage before writing only the active fields.
    let mut out: FlexWhisperVadEvent = unsafe { mem::zeroed() };
    out.epoch = event.epoch;
    out.seq = event.seq;
    match event.kind {
        WhisperVadEventKind::Segment(s) => {
            out.r#type = FLEX_WHISPER_SEGMENT;
            out.data.segment.start_ms = s.start_ms;
            out.data.segment.end_ms = s.end_ms;
        }
        WhisperVadEventKind::ProvisionalSpeechStart { at_ms } => {
            out.r#type = FLEX_WHISPER_SPEECH_START;
            out.data.speech_start.at_ms = at_ms;
        }
        WhisperVadEventKind::ProvisionalSpeechEnd { at_ms, reason } => {
            out.r#type = FLEX_WHISPER_SPEECH_END;
            out.data.speech_end.at_ms = at_ms;
            out.data.speech_end.reason = close_reason(reason);
        }
        WhisperVadEventKind::ProvisionalCut {
            start_ms,
            end_ms,
            reason,
        } => {
            out.r#type = FLEX_WHISPER_CUT;
            out.data.cut.start_ms = start_ms;
            out.data.cut.end_ms = end_ms;
            out.data.cut.reason = cut_reason(reason);
        }
        WhisperVadEventKind::EpochEnd { reason } => {
            out.r#type = FLEX_WHISPER_EPOCH_END;
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
    set_last_error(message);
    code::FLEX_INVALID_ARG
}
unsafe fn read_params(p: *const FlexWhisperVadParams) -> Result<WhisperVadParams, &'static str> {
    if p.is_null() {
        return Ok(WhisperVadParams::default());
    }
    if !p.is_aligned() {
        return Err("InvalidArgument: misaligned params");
    }
    params_from_c(&*p)
}

/// Five pinned defaults. Supplied structs use literal fields, including zero.
#[no_mangle]
pub extern "C" fn flexaudio_whisper_vad_default_params() -> FlexWhisperVadParams {
    FlexWhisperVadParams {
        threshold: 0.5,
        min_speech_duration_ms: 250,
        min_silence_duration_ms: 100,
        max_speech_duration_s: f32::MAX,
        speech_pad_ms: 30,
    }
}

/// Create a session; NULL params/options use defaults. Returns NULL plus last_error on error.
/// # Safety
/// Optional pointers must refer to initialized aligned structs.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_whisper_vad_new(
    params: *const FlexWhisperVadParams,
    options: *const FlexWhisperVadOptions,
) -> *mut FlexWhisperVad {
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
        match WhisperVad::new(params, WhisperVadOptions { provisional }) {
            Ok(inner) => Box::into_raw(Box::new(FlexWhisperVad { inner })),
            Err(e) => {
                set_last_error(e.to_string());
                ptr::null_mut()
            }
        }
    })
}

unsafe fn stream_result(
    result: Result<Vec<WhisperVadEvent>, WhisperVadFailure>,
    out: *mut *mut FlexWhisperVadEvent,
    len: *mut usize,
) -> i32 {
    let (events, status) = match result {
        Ok(events) => (events, code::FLEX_OK),
        Err(failure) => {
            let status = error_code(&failure.error);
            set_last_error(failure.error.to_string());
            (failure.terminal_events, status)
        }
    };
    write_array(events.into_iter().map(event_to_c).collect(), out, len);
    status
}

/// Feed normalized mono16k PCM. On failure still drain/free the owned terminal event array.
/// # Safety
/// Handle is exclusively owned; input/output allocations must be valid for their lengths.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_whisper_vad_process(
    v: *mut FlexWhisperVad,
    samples: *const f32,
    len: usize,
    out: *mut *mut FlexWhisperVadEvent,
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
        stream_result((*v).inner.process(samples), out, out_len)
    })
}

/// Return owned events, including terminal closure on failure.
/// # Safety
/// Handle and mandatory outputs must be valid and exclusively owned during mutation.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_whisper_vad_finish(
    v: *mut FlexWhisperVad,
    out: *mut *mut FlexWhisperVadEvent,
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
pub unsafe extern "C" fn flexaudio_whisper_vad_reset(
    v: *mut FlexWhisperVad,
    out: *mut *mut FlexWhisperVadEvent,
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
pub unsafe extern "C" fn flexaudio_whisper_vad_probabilities(
    v: *const FlexWhisperVad,
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
pub unsafe extern "C" fn flexaudio_whisper_postprocessor_new(
    p: *const FlexWhisperVadParams,
) -> *mut FlexWhisperVadPostProcessor {
    guard_ptr(|| {
        clear_last_error();
        let p = match read_params(p) {
            Ok(p) => p,
            Err(e) => {
                invalid(e);
                return ptr::null_mut();
            }
        };
        match WhisperVadPostProcessor::new(p) {
            Ok(inner) => Box::into_raw(Box::new(FlexWhisperVadPostProcessor { inner })),
            Err(e) => {
                set_last_error(e.to_string());
                ptr::null_mut()
            }
        }
    })
}
unsafe fn segment_result(
    result: Result<Vec<flexaudio_vad::WhisperSpeechSegment>, WhisperVadError>,
    out: *mut *mut FlexWhisperSpeechSegment,
    len: *mut usize,
) -> i32 {
    match result {
        Ok(items) => {
            write_array(
                items
                    .into_iter()
                    .map(|s| FlexWhisperSpeechSegment {
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
            set_last_error(e.to_string());
            error_code(&e)
        }
    }
}

/// Feed finite [0,1] probabilities. Validation preserves processor state.
/// # Safety
/// Handle, input and output allocations must be valid for their lengths.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_whisper_postprocessor_process(
    v: *mut FlexWhisperVadPostProcessor,
    samples: *const f32,
    len: usize,
    out: *mut *mut FlexWhisperSpeechSegment,
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
pub unsafe extern "C" fn flexaudio_whisper_postprocessor_finish(
    v: *mut FlexWhisperVadPostProcessor,
    out: *mut *mut FlexWhisperSpeechSegment,
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
pub unsafe extern "C" fn flexaudio_whisper_postprocessor_reset(
    v: *mut FlexWhisperVadPostProcessor,
) -> i32 {
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
pub unsafe extern "C" fn flexaudio_whisper_vad_free(v: *mut FlexWhisperVad) {
    guard_i32(|| {
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
pub unsafe extern "C" fn flexaudio_whisper_postprocessor_free(v: *mut FlexWhisperVadPostProcessor) {
    guard_i32(|| {
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
pub unsafe extern "C" fn flexaudio_whisper_events_free(v: *mut FlexWhisperVadEvent, len: usize) {
    guard_i32(|| {
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
pub unsafe extern "C" fn flexaudio_whisper_segments_free(
    v: *mut FlexWhisperSpeechSegment,
    len: usize,
) {
    guard_i32(|| {
        if !v.is_null() {
            drop(Box::from_raw(ptr::slice_from_raw_parts_mut(v, len)));
        }
        code::FLEX_OK
    });
}

#[cfg(test)]
#[path = "../tests/ffi/whisper_vad.rs"]
mod tests;
