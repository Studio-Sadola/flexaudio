//! Versioned canonical capture attachment and ordered poll carriers.
use crate::vad::params_from_c;
use crate::vad_types::*;
use crate::{
    error::{clear_last_error, code, set_audio_error},
    guard_const_ptr, guard_i32,
    types::{FlexChunk, FlexConfig, FlexStream},
};
use std::{mem, ptr};

pub const FLEX_STREAM_VERSION_2: u32 = 2;
pub const FLEX_VAD_PRIMARY: u32 = 1;
pub const FLEX_VAD_SECONDARY: u32 = 2;
pub const FLEX_VAD_UNSUPPORTED_TAP: i32 = -100;
pub const FLEX_VAD_UNSUPPORTED_CONVERSION_CLOCK: i32 = -101;
pub const FLEX_VAD_EPOCH_START: u32 = 6;

/// Fixed-width stream VAD attachment settings. v1/v2 support only the primary tap;
/// v3 also supports the secondary tap when secondary output is configured.
#[repr(C)]
pub struct FlexStreamVadOptions {
    pub params: FlexVadOptions,
    /// 0 or 1, never a default sentinel.
    pub provisional: u8,
    /// FLEX_VAD_PRIMARY or FLEX_VAD_SECONDARY.
    pub tap: u32,
}
/// Versioned envelope around the frozen v1 configuration. All pointers are borrowed during open.
#[repr(C)]
pub struct FlexStreamConfigV2 {
    pub size: u32,
    pub version: u32,
    pub config: *const FlexConfig,
    /// NULL disables vad_tap attachment.
    pub vad: *const FlexStreamVadOptions,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FlexVadEpochStart {
    /// 48 kHz frames on the dedicated capture timeline, independent of primary/secondary
    /// format and tap. capture_sample + t_ms*48 is exact. Equal to primary frame_index
    /// for the same sample only when primary is 48 kHz. Other rates share units but not
    /// the origin (resampler group delay); unreconciled startup-release padding can
    /// grow the offset by up to one primary chunk per boundary. Neither index is an offset into concatenated
    /// delivered PCM; pause and queue drops advance indices without delivering.
    pub capture_sample: u64,
    pub pts_ns: i64,
}
/// Attached union is distinct from the standalone event union.
#[repr(C)]
#[derive(Clone, Copy)]
pub union FlexAttachedVadPayload {
    pub vad: FlexVadPayload,
    pub epoch_start: FlexVadEpochStart,
}
/// Versioned attached event. Epoch start is sequence 0 before all VAD payloads.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FlexAttachedVadEvent {
    pub size: u32,
    pub version: u32,
    pub r#type: u32,
    pub epoch: u32,
    pub seq: u64,
    pub data: FlexAttachedVadPayload,
}
/// Owns v1 PCM/events and attached events until flexaudio_chunk_free_v2.
#[repr(C)]
pub struct FlexChunkV2 {
    pub size: u32,
    pub version: u32,
    pub chunk: FlexChunk,
    pub vad_events: *mut FlexAttachedVadEvent,
    pub vad_events_len: usize,
}

pub(crate) fn attached_to_c(e: flexaudio_vad::AttachedVadEvent) -> FlexAttachedVadEvent {
    let mut out: FlexAttachedVadEvent = unsafe { mem::zeroed() };
    out.size = u32::try_from(mem::size_of::<FlexAttachedVadEvent>()).expect("fixed ABI size");
    out.version = FLEX_STREAM_VERSION_2;
    out.epoch = e.epoch();
    out.seq = e.seq();
    match e {
        flexaudio_vad::AttachedVadEvent::EpochStart {
            capture_sample,
            pts_ns,
            ..
        } => {
            out.r#type = FLEX_VAD_EPOCH_START;
            out.data.epoch_start.capture_sample = capture_sample;
            out.data.epoch_start.pts_ns = pts_ns;
        }
        flexaudio_vad::AttachedVadEvent::Vad(e) => {
            let event = crate::vad::event_to_c(e);
            out.r#type = event.r#type;
            out.data.vad = event.data;
        }
    }
    out
}

fn tap_status(error: &flexaudio_vad::VadTapError) -> i32 {
    use flexaudio_vad::VadTapError;
    match error.root() {
        VadTapError::Cause { primary, .. } => tap_status(primary),
        VadTapError::Vad(error) => crate::vad::error_code(error),
        VadTapError::InvalidStereoLength
        | VadTapError::InvalidPcm { .. }
        | VadTapError::CaptureSampleOverflow
        | VadTapError::PtsOutOfRange => code::FLEX_INVALID_ARG,
        VadTapError::Stopped | VadTapError::FailedSession => code::FLEX_INVALID_STATE,
        VadTapError::UnsupportedConversionClock => FLEX_VAD_UNSUPPORTED_CONVERSION_CLOCK,
        VadTapError::Conversion => code::FLEX_FAILURE,
    }
}

fn reject(status: i32, message: &str) -> i32 {
    let error = match status {
        code::FLEX_INVALID_ARG => flexaudio::Error::InvalidArg(message.into()),
        code::FLEX_INVALID_STATE => flexaudio::Error::InvalidState(message.into()),
        FLEX_VAD_UNSUPPORTED_TAP => flexaudio::Error::Unsupported,
        _ => flexaudio::Error::Backend(message.into()),
    };
    crate::error::set_audio_error_with_message(error, message.into());
    status
}

pub(crate) fn reject_tap(error: flexaudio_vad::VadTapError) -> i32 {
    reject_audio(tap_status(&error), error.audio_error())
}

fn reject_audio(status: i32, error: flexaudio::Error) -> i32 {
    set_audio_error(error);
    status
}

struct ValidatedConfig<'a> {
    base: &'a FlexConfig,
    vad: Option<(flexaudio_vad::VadOptions, flexaudio_vad::VadRuntimeOptions)>,
}

unsafe fn validate<'a>(config: *const FlexStreamConfigV2) -> Result<ValidatedConfig<'a>, i32> {
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
            "InvalidArgument: invalid base config pointer",
        ));
    }
    let base = &*config.config;
    crate::convert::validate_config(base).map_err(|()| code::FLEX_INVALID_ARG)?;
    let vad = validate_vad(config.vad, false)?.map(|(params, runtime, _)| (params, runtime));
    Ok(ValidatedConfig { base, vad })
}

pub(crate) type ValidatedVad = (
    flexaudio_vad::VadOptions,
    flexaudio_vad::VadRuntimeOptions,
    bool,
);

pub(crate) unsafe fn validate_vad(
    options: *const FlexStreamVadOptions,
    secondary_exists: bool,
) -> Result<Option<ValidatedVad>, i32> {
    let vad = if !options.is_null() {
        if !options.is_aligned() {
            return Err(reject(
                code::FLEX_INVALID_ARG,
                "InvalidArgument: misaligned VAD options",
            ));
        }
        let options = &*options;
        match options.tap {
            FLEX_VAD_PRIMARY => {}
            FLEX_VAD_SECONDARY if secondary_exists => {}
            FLEX_VAD_SECONDARY => {
                return Err(reject(
                    FLEX_VAD_UNSUPPORTED_TAP,
                    "UnsupportedTap: only primary is supported",
                ))
            }
            _ => {
                return Err(reject(
                    code::FLEX_INVALID_ARG,
                    "InvalidArgument: unknown VAD tap",
                ))
            }
        }
        if options.provisional > 1 {
            return Err(reject(
                code::FLEX_INVALID_ARG,
                "InvalidArgument: provisional must be 0 or 1",
            ));
        }
        let params = params_from_c(&options.params)
            .map_err(|message| reject(code::FLEX_INVALID_ARG, message))?;
        Some((
            params,
            flexaudio_vad::VadRuntimeOptions {
                provisional: options.provisional != 0,
            },
            options.tap == FLEX_VAD_SECONDARY,
        ))
    } else {
        None
    };
    Ok(vad)
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
        let validated = match validate(config) {
            Ok(c) => c,
            Err(status) => return status,
        };
        let vad_tap = match validated
            .vad
            .map(|(params, runtime)| flexaudio_vad::VadTap::new(params, runtime))
            .transpose()
        {
            Ok(tap) => tap,
            Err(error) => return reject_tap(error),
        };
        let config = validated.base;
        let stream = crate::open_with_exclude_pids_using(config, ptr::null(), 0, |decoded| {
            let options = flexaudio::OpenOptions::default().settings_attachments(
                crate::settings_request::attachments(&*config, 0).vad(vad_tap.is_some()),
            );
            flexaudio::open_with_options(decoded, options)
        });
        if stream.is_null() {
            crate::error::last_audio_error()
                .as_ref()
                .map_or(code::FLEX_FAILURE, crate::error::root_code)
        } else {
            if vad_tap.is_some() {
                if let Err(error) = (*stream).inner.enable_capture_tap() {
                    crate::flexaudio_free(stream);
                    crate::error::set_audio_error(error);
                    return code::FLEX_FAILURE;
                }
            }
            (*stream).vad_tap = vad_tap;
            out.write(stream);
            code::FLEX_OK
        }
    })
}

/// Poll a versioned chunk: 1 available, 0 absent, negative result on error.
/// PCM polling does not wait for event polling, including after stop.
/// Startup notices are enqueued before publishing their generation's first chunk;
/// event-first callers can drain the ordinary FIFO event queue before PCM.
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
        if let Some(error) = stream.inner.terminal_error() {
            // Keep closing event carriers, while freeing every queued PCM allocation.
            let queued = mem::take(&mut stream.ready_chunks);
            for mut chunk in queued {
                if chunk.chunk.frames == 0 {
                    stream.ready_chunks.push_back(chunk);
                } else {
                    flexaudio_chunk_free_v2(&mut chunk);
                }
            }
            if let Some(carrier) = stream.ready_chunks.pop_front() {
                out.write(carrier);
                return 1;
            }
            crate::error::set_audio_error(error);
            return code::FLEX_FAILURE;
        }
        if let Some(chunk) = stream.ready_chunks.pop_front() {
            out.write(chunk);
            return 1;
        }
        match stream.next_vad_tap_chunk() {
            Ok(Some(chunk)) => {
                out.write(chunk);
                1
            }
            Ok(None) => {
                if let Some(error) = stream.inner.terminal_error() {
                    crate::error::set_audio_error(error);
                    return code::FLEX_FAILURE;
                }
                0
            }
            Err(error) => {
                crate::error::set_audio_error(error.audio_error());
                code::FLEX_FAILURE
            }
        }
    })
}
/// Free all allocations in a versioned chunk, then clear its fields. NULL is safe.
/// # Safety
/// Non-NULL must be a chunk returned by poll_chunk_v2 and must not have been freed already.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_chunk_free_v2(chunk: *mut FlexChunkV2) {
    guard_i32(|| {
        if chunk.is_null() {
            return code::FLEX_OK;
        }
        if !crate::v2::valid(chunk) {
            return crate::v2::invalid();
        }
        crate::flexaudio_chunk_free(&mut (*chunk).chunk);
        if !(*chunk).vad_events.is_null() {
            drop(Box::from_raw(ptr::slice_from_raw_parts_mut(
                (*chunk).vad_events,
                (*chunk).vad_events_len,
            )));
        }
        chunk.write(mem::zeroed());
        code::FLEX_OK
    });
}
/// Flush a vad_tap epoch. Disabled attachment is a no-op.
/// Failures return their root code immediately; closing events remain available via poll_chunk_v2.
/// # Safety
/// s must be a valid, exclusively owned stream handle.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_flush_vad(s: *mut FlexStream) -> i32 {
    guard_i32(|| {
        clear_last_error();
        if s.is_null() || !s.is_aligned() {
            return reject(
                code::FLEX_INVALID_ARG,
                "InvalidArgument: invalid stream handle",
            );
        }
        (&mut *s).flush_vad_tap_with(flexaudio_vad::VadTap::flush)
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
    fn flush_vad_tap_with(
        &mut self,
        flush: impl FnOnce(
            &mut flexaudio_vad::VadTap,
        )
            -> Result<Vec<flexaudio_vad::AttachedVadEvent>, flexaudio_vad::VadTapFailure>,
    ) -> i32 {
        if self.vad_tap.is_none() {
            return code::FLEX_OK;
        }
        if let Err(error) = self.queue_vad_tap_input() {
            return {
                crate::error::set_audio_error(error.audio_error());
                code::FLEX_FAILURE
            };
        }
        let result = flush(self.vad_tap.as_mut().expect("enabled attachment"));
        let flush_error = result.as_ref().err().map(|failure| failure.error.clone());
        self.accept_vad_tap(result, flexaudio::Operation::Flush);
        self.queue_vad_carrier();
        if let Some(error) = flush_error {
            return reject_tap(error);
        }
        code::FLEX_OK
    }

    pub(crate) fn accept_vad_tap(
        &mut self,
        result: Result<Vec<flexaudio_vad::AttachedVadEvent>, flexaudio_vad::VadTapFailure>,
        operation: flexaudio::Operation,
    ) {
        match result {
            Ok(events) => {
                if matches!(
                    operation,
                    flexaudio::Operation::Normalize | flexaudio::Operation::Flush
                ) {
                    self.vad_error = None;
                }
                self.vad_events.extend(events);
            }
            Err(failure) => {
                self.vad_events.extend(failure.terminal_events);
                if self.vad_error.is_none() {
                    self.vad_cleanup.push(failure.error.audio_error());
                    self.vad_error = Some(failure.error);
                }
            }
        }
    }
    pub(crate) fn drain_vad_capture(&mut self) {
        if self.vad_tap.is_none() {
            return;
        }
        while let Some(chunk) = self.inner.poll_capture() {
            let discontinuity = chunk.flags.contains(flexaudio::ChunkFlags::DISCONTINUITY);
            let result = self.vad_tap.as_mut().expect("enabled attachment").process(
                &chunk.data,
                chunk.frame_index,
                chunk.pts_ns,
                discontinuity,
            );
            if result.is_ok() {
                self.vad_origin = (
                    chunk.frame_index + chunk.frames as u64,
                    chunk.pts_ns + (chunk.frames as i64 * 1_000_000_000 / 48_000),
                );
            }
            self.accept_vad_tap(result, flexaudio::Operation::Normalize);
        }
    }
    pub(crate) fn versioned_chunk(&mut self, chunk: FlexChunk) -> FlexChunkV2 {
        let events = if self
            .secondary
            .as_ref()
            .is_some_and(|state| state.vad_secondary)
        {
            Vec::new()
        } else {
            mem::take(&mut self.vad_events)
        };
        let events: Box<[_]> = events.into_iter().map(attached_to_c).collect();
        let len = events.len();
        let data = if len == 0 {
            ptr::null_mut()
        } else {
            Box::into_raw(events) as *mut FlexAttachedVadEvent
        };
        FlexChunkV2 {
            size: u32::try_from(mem::size_of::<FlexChunkV2>()).expect("fixed ABI size"),
            version: FLEX_STREAM_VERSION_2,
            chunk,
            vad_events: data,
            vad_events_len: len,
        }
    }

    fn next_vad_tap_chunk(&mut self) -> Result<Option<FlexChunkV2>, flexaudio_vad::InferenceError> {
        self.drain_vad_capture();
        let chunk = self.poll_processed()?;
        match chunk {
            Some(chunk) => Ok(Some(self.versioned_chunk(chunk))),
            None if !self.vad_events.is_empty()
                && !self
                    .secondary
                    .as_ref()
                    .is_some_and(|state| state.vad_secondary) =>
            {
                let chunk = crate::convert::chunk_to_c(empty_carrier(self.vad_origin));
                Ok(Some(self.versioned_chunk(chunk)))
            }
            None => Ok(None),
        }
    }

    fn queue_vad_tap_input(&mut self) -> Result<(), flexaudio_vad::InferenceError> {
        while let Some(chunk) = self.next_vad_tap_chunk()? {
            self.ready_chunks.push_back(chunk);
        }
        self.queue_secondary_input();
        Ok(())
    }

    fn queue_vad_carrier(&mut self) {
        if self.vad_events.is_empty() {
            return;
        }
        if self
            .secondary
            .as_ref()
            .is_some_and(|state| state.vad_secondary)
        {
            self.queue_secondary_carrier();
            return;
        }
        let chunk = crate::convert::chunk_to_c(empty_carrier(self.vad_origin));
        let chunk = self.versioned_chunk(chunk);
        self.ready_chunks.push_back(chunk);
    }

    pub(crate) fn stop_vad_tap(&mut self) {
        if self.vad_tap.is_none() {
            return;
        }
        self.queue_vad_tap_input()
            .expect("canonical attachment preserves PCM processing");
        if let Some(tap) = self.vad_tap.as_mut() {
            let result = tap.stop();
            self.accept_vad_tap(result, flexaudio::Operation::Stop);
            self.queue_vad_carrier();
        }
    }
}

/// Borrow attached events until chunk_free_v2; NULL when none are present.
/// # Safety
/// chunk must point to a live versioned chunk; len must be writable when non-NULL.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_chunk_vad_events(
    chunk: *const FlexChunkV2,
    len: *mut usize,
) -> *const FlexAttachedVadEvent {
    guard_const_ptr(|| {
        clear_last_error();
        if !len.is_null() && !crate::v2::valid(len) {
            crate::v2::invalid();
            return ptr::null();
        }
        if !len.is_null() {
            len.write(0);
        }
        if !crate::v2::valid(chunk) {
            crate::v2::invalid();
            return ptr::null();
        }
        if !len.is_null() {
            len.write((*chunk).vad_events_len);
        }
        (*chunk).vad_events
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    struct PushBackend(Arc<Mutex<Option<flexaudio::core::backend::RawSink>>>);

    #[test]
    fn queued_pcm_is_suppressed_after_permission_denial() {
        struct Denied;
        impl flexaudio::CaptureBackend for Denied {
            fn native_format(&self) -> (u32, u16) {
                (48_000, 2)
            }
            fn start(&mut self, _: flexaudio::core::backend::RawSink) -> flexaudio::Result<()> {
                Err(flexaudio::Error::PermissionDenied {
                    permission: flexaudio::core::types::Permission::Microphone,
                    detail: "denied by user".into(),
                })
            }
            fn stop(&mut self) {}
        }
        let mut inner = flexaudio::Stream::open(Default::default(), Box::new(Denied)).unwrap();
        assert!(inner.start().is_err());
        let mut stream = FlexStream {
            events: Default::default(),
            secondary: None,
            shutdown: None,
            shutdown_event_index: 0,
            inner,
            vad_tap: None,
            vad_events: Vec::new(),
            vad_origin: (0, 0),
            vad_error: None,
            vad_cleanup: Vec::new(),
            ready_chunks: Default::default(),
        };
        let mut chunk = empty_carrier((0, 0));
        chunk.frames = 960;
        chunk.data = vec![0.25; 1920];
        let chunk = stream.versioned_chunk(crate::convert::chunk_to_c(chunk));
        stream.ready_chunks.push_back(chunk);
        unsafe {
            let mut out = mem::zeroed();
            let status = flexaudio_poll_chunk_v2(&mut stream, &mut out);
            flexaudio_chunk_free_v2(&mut out);
            assert_eq!(
                status,
                code::FLEX_FAILURE,
                "queued PCM was delivered after denial"
            );
        }
    }
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
            let params = flexaudio_vad::VadOptions {
                threshold: 0.0,
                min_speech_ms: 0,
                speech_pad_ms: 0,
                ..Default::default()
            };
            let vad_tap = flexaudio_vad::VadTap::new(
                params,
                flexaudio_vad::VadRuntimeOptions { provisional: true },
            )
            .unwrap();
            inner.start().unwrap();
            let mut stream = FlexStream {
                events: Default::default(),
                secondary: None,
                shutdown: None,
                shutdown_event_index: 0,
                inner,
                vad_tap: Some(vad_tap),
                vad_events: Vec::new(),
                vad_origin: (0, 0),
                vad_error: None,
                vad_cleanup: Vec::new(),
                ready_chunks: Default::default(),
            };
            for expected in [0, 960] {
                sink.lock().unwrap().as_mut().unwrap().push(&[0.0; 1920], 0);
                let mut chunk = next_chunk(&mut stream);
                assert_eq!(crate::flexaudio_chunk_frame_index(&chunk.chunk), expected);
                if expected == 0 {
                    let first = &*chunk.vad_events;
                    assert_eq!(first.r#type, FLEX_VAD_EPOCH_START);
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
            let pointer = flexaudio_chunk_vad_events(&chunk, &mut len);
            let events = std::slice::from_raw_parts(pointer, len);
            let end = events
                .iter()
                .position(|e| e.r#type == FLEX_VAD_EPOCH_END)
                .unwrap();
            let start = events
                .iter()
                .position(|e| e.r#type == FLEX_VAD_EPOCH_START)
                .unwrap();
            assert!(end < start);
            assert_eq!(events[start].data.epoch_start.capture_sample, index);
            flexaudio_chunk_free_v2(&mut chunk);
            assert_eq!(flexaudio_flush_vad(&mut stream), code::FLEX_OK);
            let mut carrier = next_chunk(&mut stream);
            assert_eq!(carrier.chunk.frames, 0);
            assert_eq!(
                (*carrier.vad_events.add(carrier.vad_events_len - 1)).r#type,
                FLEX_VAD_EPOCH_END
            );
            flexaudio_chunk_free_v2(&mut carrier);
            assert_eq!(flexaudio_flush_vad(&mut stream), code::FLEX_OK);
            let mut absent = mem::zeroed();
            assert_eq!(flexaudio_poll_chunk_v2(&mut stream, &mut absent), 0);
            sink.lock().unwrap().as_mut().unwrap().push(&[0.0; 1920], 0);
            let mut chunk = next_chunk(&mut stream);
            flexaudio_chunk_free_v2(&mut chunk);
            assert_eq!(crate::flexaudio_stop(&mut stream), code::FLEX_OK);
            assert_eq!(flexaudio_flush_vad(&mut stream), code::FLEX_INVALID_STATE);
            assert!(!crate::flexaudio_last_error().is_null());
            let handle = Box::into_raw(Box::new(stream));
            let mut terminal = next_chunk(&mut *handle);
            assert_eq!(terminal.chunk.frames, 0);
            assert_eq!(
                (*terminal.vad_events.add(terminal.vad_events_len - 1)).r#type,
                FLEX_VAD_EPOCH_END
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
        let e = attached_to_c(flexaudio_vad::AttachedVadEvent::EpochStart {
            epoch: 3,
            seq: 0,
            capture_sample: 9_007_199_254_740_993,
            pts_ns: 123,
        });
        assert_eq!((e.r#type, e.epoch, e.seq), (FLEX_VAD_EPOCH_START, 3, 0));
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
                vad: ptr::null(),
            };
            assert_eq!(flexaudio_open_v2(&config, &mut out), code::FLEX_INVALID_ARG);
            assert!(out.is_null());
            flexaudio_chunk_free_v2(ptr::null_mut());
        }
    }
    #[test]
    fn attachment_conflicts_and_capabilities_fail_before_open() {
        unsafe {
            let base: FlexConfig = mem::zeroed();
            let mut options = FlexStreamVadOptions {
                params: crate::vad::flexaudio_vad_default_options(),
                provisional: 0,
                tap: FLEX_VAD_SECONDARY,
            };
            let config = FlexStreamConfigV2 {
                size: u32::try_from(mem::size_of::<FlexStreamConfigV2>()).unwrap(),
                version: 2,
                config: &base,
                vad: &options,
            };
            let mut out = ptr::null_mut();
            assert_eq!(
                flexaudio_open_v2(&config, &mut out),
                FLEX_VAD_UNSUPPORTED_TAP
            );
            options.tap = FLEX_VAD_PRIMARY;
            assert_eq!(options.tap, FLEX_VAD_PRIMARY);
            assert!(validate(&config).is_ok());
            assert!(out.is_null());
        }
    }
}

#[cfg(test)]
#[path = "../tests/ffi/vad_attachment.rs"]
mod attachment_tests;

#[cfg(test)]
mod error_projection_tests {
    use super::*;
    #[test]
    fn typed_vad_tap_rejection_keeps_typed_converter_cause() {
        let error = flexaudio_vad::VadTapError::Cause {
            primary: Box::new(flexaudio_vad::VadTapError::Conversion),
            source: flexaudio_vad::InferenceError::Resample("injected converter cause".into()),
        };
        let expected = error.audio_error();
        assert_eq!(reject_tap(error), code::FLEX_FAILURE);
        assert_eq!(crate::error::last_audio_error(), Some(expected));
    }
}
