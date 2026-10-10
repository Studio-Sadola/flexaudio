//! Versioned canonical capture attachment and ordered poll carriers.
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

fn tap_status(error: &flexaudio_vad::WhisperVadTapError) -> i32 {
    if matches!(
        error,
        flexaudio_vad::WhisperVadTapError::UnsupportedConversionClock
    ) {
        FLEX_WHISPER_UNSUPPORTED_CONVERSION_CLOCK
    } else {
        code::FLEX_FAILURE
    }
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
    }
    Ok(base)
}

/// Open a versioned primary stream, returning a typed result code and last_error.
/// Attachment uses the producer canonical branch before output conversion.
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
        let envelope = config;
        let config = match validate(config) {
            Ok(c) => c,
            Err(status) => return status,
        };
        let options = (*envelope).whisper_vad.as_ref();
        let whisper = match options
            .map(|options| {
                flexaudio_vad::WhisperVadTap::new(
                    params_from_c(&options.params).expect("validated parameters"),
                    flexaudio_vad::WhisperVadOptions {
                        provisional: options.provisional != 0,
                    },
                )
            })
            .transpose()
        {
            Ok(tap) => tap,
            Err(error) => return reject(tap_status(&error), &error.to_string()),
        };
        let stream = crate::flexaudio_open(config);
        if stream.is_null() {
            code::FLEX_FAILURE
        } else {
            if whisper.is_some() {
                if let Err(error) = (*stream).inner.enable_capture_tap() {
                    crate::flexaudio_free(stream);
                    return reject(code::FLEX_FAILURE, &error.to_string());
                }
                (*stream).inner.set_denoise(config.denoise);
                (*stream).denoiser = None;
            }
            (*stream).whisper = whisper;
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
        if s.is_null() || !s.is_aligned() {
            return reject(
                code::FLEX_INVALID_ARG,
                "InvalidArgument: invalid stream handle",
            );
        }
        let stream = &mut *s;
        if let Some(chunk) = stream.ready_chunks.pop_front() {
            out.write(chunk);
            return 1;
        }
        if !stream.whisper_error_reported && stream.whisper_events.is_empty() {
            if let Some(error) = stream.whisper_error.clone() {
                stream.whisper_error_reported = true;
                return reject(tap_status(&error), &error.to_string());
            }
        }
        match stream.next_whisper_chunk() {
            Ok(Some(chunk)) => {
                out.write(chunk);
                1
            }
            Ok(None) => {
                if !stream.whisper_error_reported {
                    if let Some(error) = stream.whisper_error.clone() {
                        stream.whisper_error_reported = true;
                        return reject(tap_status(&error), &error.to_string());
                    }
                }
                if let Some(error) = stream.inner.terminal_error() {
                    return reject(code::FLEX_FAILURE, &error.to_string());
                }
                0
            }
            Err(error) => reject(code::FLEX_FAILURE, &error.to_string()),
        }
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
        let stream = &mut *s;
        if stream.whisper.is_none() {
            return code::FLEX_OK;
        }
        if let Err(error) = stream.queue_whisper_input() {
            return reject(code::FLEX_FAILURE, &error.to_string());
        }
        if let Some(tap) = stream.whisper.as_mut() {
            let result = tap.flush();
            stream.whisper_error = None;
            stream.whisper_error_reported = false;
            stream.accept_whisper(result);
            stream.queue_whisper_carrier();
        }
        code::FLEX_OK
    })
}

fn empty_carrier(origin: (u64, i64)) -> flexaudio::AudioChunk {
    flexaudio::AudioChunk {
        data: Vec::new(),
        frames: 0,
        frame_index: origin.0,
        pts_ns: origin.1,
        seq: 0,
        flags: flexaudio::ChunkFlags::empty(),
        dropped_before: 0,
        peak: 0.0,
        rms: 0.0,
    }
}

impl FlexStream {
    pub(crate) fn accept_whisper(
        &mut self,
        result: Result<
            Vec<flexaudio_vad::AttachedWhisperVadEvent>,
            flexaudio_vad::WhisperVadTapFailure,
        >,
    ) {
        match result {
            Ok(events) => self.whisper_events.extend(events),
            Err(failure) => {
                self.whisper_events.extend(failure.terminal_events);
                if self.whisper_error.is_none() {
                    self.whisper_error = Some(failure.error);
                }
            }
        }
    }
    pub(crate) fn drain_whisper_capture(&mut self) {
        if self.whisper.is_none() {
            return;
        }
        while let Some(chunk) = self.inner.poll_capture() {
            if self.whisper_error.is_some() {
                continue;
            }
            let result = self.whisper.as_mut().expect("enabled attachment").process(
                &chunk.data,
                chunk.frame_index,
                chunk.pts_ns,
                chunk.flags.contains(flexaudio::ChunkFlags::DISCONTINUITY),
            );
            if result.is_ok() {
                self.whisper_origin = (
                    chunk.frame_index + chunk.frames as u64,
                    chunk.pts_ns + (chunk.frames as i64 * 1_000_000_000 / 48_000),
                );
            }
            self.accept_whisper(result);
        }
    }
    fn versioned_chunk(&mut self, chunk: FlexChunk) -> FlexChunkV2 {
        let events: Box<[_]> = mem::take(&mut self.whisper_events)
            .into_iter()
            .map(attached_to_c)
            .collect();
        let len = events.len();
        let data = if len == 0 {
            ptr::null_mut()
        } else {
            Box::into_raw(events) as *mut FlexAttachedWhisperVadEvent
        };
        FlexChunkV2 {
            size: u32::try_from(mem::size_of::<FlexChunkV2>()).expect("fixed ABI size"),
            version: FLEX_STREAM_VERSION_2,
            chunk,
            whisper_vad_events: data,
            whisper_vad_events_len: len,
        }
    }

    fn next_whisper_chunk(&mut self) -> Result<Option<FlexChunkV2>, flexaudio_vad::VadError> {
        self.drain_whisper_capture();
        let chunk = self.poll_processed()?;
        match chunk {
            Some(chunk) => Ok(Some(self.versioned_chunk(chunk))),
            None if !self.whisper_events.is_empty() => {
                let chunk = crate::convert::chunk_to_c(empty_carrier(self.whisper_origin));
                Ok(Some(self.versioned_chunk(chunk)))
            }
            None => Ok(None),
        }
    }

    fn queue_whisper_input(&mut self) -> Result<(), flexaudio_vad::VadError> {
        while let Some(chunk) = self.next_whisper_chunk()? {
            self.ready_chunks.push_back(chunk);
        }
        Ok(())
    }

    fn queue_whisper_carrier(&mut self) {
        if self.whisper_events.is_empty() {
            return;
        }
        let chunk = crate::convert::chunk_to_c(empty_carrier(self.whisper_origin));
        let chunk = self.versioned_chunk(chunk);
        self.ready_chunks.push_back(chunk);
    }

    pub(crate) fn stop_whisper(&mut self) {
        if self.whisper.is_none() {
            return;
        }
        self.queue_whisper_input()
            .expect("whisper attachment excludes legacy VAD");
        if let Some(tap) = self.whisper.as_mut() {
            let result = tap.stop();
            self.accept_whisper(result);
            self.queue_whisper_carrier();
        }
    }
}

/// Borrow attached events until chunk_free_v2; NULL when none are present.
/// # Safety
/// chunk must point to a live versioned chunk; len must be writable when non-NULL.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_chunk_whisper_vad_events(
    chunk: *const FlexChunkV2,
    len: *mut usize,
) -> *const FlexAttachedWhisperVadEvent {
    let Some(chunk) = chunk.as_ref() else {
        if let Some(len) = len.as_mut() {
            *len = 0;
        }
        return ptr::null();
    };
    if let Some(len) = len.as_mut() {
        *len = chunk.whisper_vad_events_len;
    }
    chunk.whisper_vad_events
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    struct PushBackend(Arc<Mutex<Option<flexaudio::core::backend::RawSink>>>);
    impl flexaudio::CaptureBackend for PushBackend {
        fn native_format(&self) -> (u32, u16) {
            (48_000, 2)
        }
        fn start(&mut self, sink: flexaudio::core::backend::RawSink) -> flexaudio::Result<()> {
            *self.0.lock().unwrap() = Some(sink);
            Ok(())
        }
        fn stop(&mut self) {
            self.0.lock().unwrap().take();
        }
    }

    unsafe fn next_chunk(stream: &mut FlexStream) -> FlexChunkV2 {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let mut chunk = mem::zeroed();
            let status = flexaudio_poll_chunk_v2(stream, &mut chunk);
            assert!(status >= 0, "attached poll failed: {status}");
            if status == 1 {
                return chunk;
            }
            assert!(Instant::now() < deadline, "fake capture timed out");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn attached_c_chunks_flush_switch_and_stop_preserve_v1_layout() {
        unsafe {
            let sink = Arc::new(Mutex::new(None));
            let mut inner = flexaudio::Stream::open(
                flexaudio::StreamConfig::default(),
                Box::new(PushBackend(sink.clone())),
            )
            .unwrap();
            inner.enable_capture_tap().unwrap();
            let params = flexaudio_vad::WhisperVadParams {
                threshold: 0.0,
                min_speech_duration_ms: 0,
                speech_pad_ms: 0,
                ..Default::default()
            };
            let whisper = flexaudio_vad::WhisperVadTap::new(
                params,
                flexaudio_vad::WhisperVadOptions { provisional: true },
            )
            .unwrap();
            inner.start().unwrap();
            let mut stream = FlexStream {
                inner,
                whisper: Some(whisper),
                whisper_events: Vec::new(),
                whisper_origin: (0, 0),
                whisper_error: None,
                whisper_error_reported: false,
                ready_chunks: Default::default(),
                vad: None,
                denoiser: None,
            };
            for expected in [0, 960] {
                sink.lock().unwrap().as_mut().unwrap().push(&[0.0; 1920], 0);
                let mut chunk = next_chunk(&mut stream);
                assert_eq!(crate::flexaudio_chunk_frame_index(&chunk.chunk), expected);
                if expected == 0 {
                    let first = &*chunk.whisper_vad_events;
                    assert_eq!(first.r#type, FLEX_WHISPER_EPOCH_START);
                    assert_eq!(first.data.epoch_start.capture_sample, expected);
                    assert_eq!(first.seq, 0);
                }
                flexaudio_chunk_free_v2(&mut chunk);
            }
            stream
                .inner
                .switch_backend(Box::new(PushBackend(sink.clone())))
                .unwrap();
            sink.lock().unwrap().as_mut().unwrap().push(&[0.0; 1920], 0);
            let mut chunk = next_chunk(&mut stream);
            let index = crate::flexaudio_chunk_frame_index(&chunk.chunk);
            assert_eq!(index, 1920);
            let mut len = 0;
            let pointer = flexaudio_chunk_whisper_vad_events(&chunk, &mut len);
            let events = std::slice::from_raw_parts(pointer, len);
            let end = events
                .iter()
                .position(|e| e.r#type == FLEX_WHISPER_EPOCH_END)
                .unwrap();
            let start = events
                .iter()
                .position(|e| e.r#type == FLEX_WHISPER_EPOCH_START)
                .unwrap();
            assert!(end < start);
            assert_eq!(events[start].data.epoch_start.capture_sample, index);
            flexaudio_chunk_free_v2(&mut chunk);
            assert_eq!(flexaudio_flush_whisper_vad(&mut stream), code::FLEX_OK);
            let mut carrier = next_chunk(&mut stream);
            assert_eq!(carrier.chunk.frames, 0);
            assert_eq!(
                (*carrier
                    .whisper_vad_events
                    .add(carrier.whisper_vad_events_len - 1))
                .r#type,
                FLEX_WHISPER_EPOCH_END
            );
            flexaudio_chunk_free_v2(&mut carrier);
            assert_eq!(flexaudio_flush_whisper_vad(&mut stream), code::FLEX_OK);
            let mut absent = mem::zeroed();
            assert_eq!(flexaudio_poll_chunk_v2(&mut stream, &mut absent), 0);
            sink.lock().unwrap().as_mut().unwrap().push(&[0.0; 1920], 0);
            let mut chunk = next_chunk(&mut stream);
            flexaudio_chunk_free_v2(&mut chunk);
            assert_eq!(crate::flexaudio_stop(&mut stream), code::FLEX_OK);
            let handle = Box::into_raw(Box::new(stream));
            crate::flexaudio_free(handle);
            assert!(std::ffi::CStr::from_ptr(crate::flexaudio_last_error())
                .to_str()
                .unwrap()
                .contains("PendingWhisperVadEvents"));
            let mut terminal = next_chunk(&mut *handle);
            assert_eq!(terminal.chunk.frames, 0);
            assert_eq!(
                (*terminal
                    .whisper_vad_events
                    .add(terminal.whisper_vad_events_len - 1))
                .r#type,
                FLEX_WHISPER_EPOCH_END
            );
            flexaudio_chunk_free_v2(&mut terminal);
            assert_eq!(crate::flexaudio_stop(handle), code::FLEX_OK);
            assert_eq!(flexaudio_poll_chunk_v2(handle, &mut absent), 0);
            crate::flexaudio_free(handle);
            assert!(crate::flexaudio_last_error().is_null());
        }
    }
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
            assert_eq!(options.tap, FLEX_WHISPER_PRIMARY);
            assert!(validate(&config).is_ok());
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
