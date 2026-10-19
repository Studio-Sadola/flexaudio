//! Owned secondary PCM and independent delivery; encoding never changes float meters.
use crate::{
    boundary::{invalid, output},
    convert::valid_pointer,
    error::{self, code},
    guard_const_ptr, guard_i32,
    types::FlexStream,
    v3_types::*,
    vad_integration::{attached_to_c, FlexAttachedVadEvent},
};
use flexaudio::{OutputFormat, SecondaryChunk};
use std::{collections::VecDeque, mem, ptr};

enum Samples {
    F32(Box<[f32]>),
    S16(Box<[i16]>),
}

/// Immutable secondary chunk owner. PCM, metadata and events live until chunk_free_v3.
/// Borrowed arrays must never be freed separately. Release each owner exactly once.
pub struct FlexSecondaryChunkV3 {
    pub(crate) metadata: FlexSecondaryMetadataV3,
    samples: Samples,
    events: Box<[FlexAttachedVadEvent]>,
}

pub(crate) struct SecondaryState {
    format: OutputFormat,
    encoding: SecondaryEncoding,
    pub(crate) vad_secondary: bool,
    ready: VecDeque<FlexSecondaryChunkV3>,
}
impl SecondaryState {
    pub(crate) fn new(
        format: OutputFormat,
        encoding: SecondaryEncoding,
        vad_secondary: bool,
    ) -> Self {
        Self {
            format,
            encoding,
            vad_secondary,
            ready: VecDeque::new(),
        }
    }

    fn chunk(
        &self,
        chunk: SecondaryChunk,
        events: Vec<flexaudio_vad::AttachedVadEvent>,
    ) -> FlexSecondaryChunkV3 {
        let metadata = FlexSecondaryMetadataV3 {
            size: u32::try_from(mem::size_of::<FlexSecondaryMetadataV3>()).expect("fixed ABI size"),
            version: FLEX_STREAM_VERSION_3,
            rate: self.format.sample_rate,
            channels: self.format.channels,
            encoding: self.encoding.code(),
            frames: u32::try_from(chunk.frames).expect("bounded chunk frames"),
            frame_index: chunk.frame_index,
            pts_ns: chunk.pts_ns,
            seq: chunk.seq,
            flags: chunk.flags.bits(),
            dropped_before: chunk.dropped_before,
            peak: chunk.peak,
            rms: chunk.rms,
        };
        let samples = match self.encoding {
            SecondaryEncoding::F32 => Samples::F32(chunk.samples.into_boxed_slice()),
            SecondaryEncoding::S16 => Samples::S16(
                chunk
                    .samples
                    .into_iter()
                    .map(flexaudio::core::quantize_i16)
                    .collect(),
            ),
        };
        FlexSecondaryChunkV3 {
            metadata,
            samples,
            events: events.into_iter().map(attached_to_c).collect(),
        }
    }
}

impl FlexStream {
    fn next_secondary(&mut self) -> Option<FlexSecondaryChunkV3> {
        self.secondary.as_ref()?;
        self.drain_vad_capture();
        let chunk = self.inner.poll_secondary()?;
        let events = self.take_secondary_events();
        Some(
            self.secondary
                .as_ref()
                .expect("secondary enabled")
                .chunk(chunk, events),
        )
    }

    fn take_secondary_events(&mut self) -> Vec<flexaudio_vad::AttachedVadEvent> {
        if self
            .secondary
            .as_ref()
            .is_some_and(|state| state.vad_secondary)
        {
            mem::take(&mut self.vad_events)
        } else {
            Vec::new()
        }
    }

    pub(crate) fn queue_secondary_input(&mut self) {
        while let Some(chunk) = self.next_secondary() {
            self.secondary
                .as_mut()
                .expect("secondary enabled")
                .ready
                .push_back(chunk);
        }
    }

    pub(crate) fn queue_secondary_carrier(&mut self) {
        if self.vad_events.is_empty()
            || !self
                .secondary
                .as_ref()
                .is_some_and(|state| state.vad_secondary)
        {
            return;
        }
        let chunk = SecondaryChunk {
            samples: Vec::new(),
            frames: 0,
            frame_index: self.vad_origin.0,
            pts_ns: self.vad_origin.1,
            seq: 0,
            flags: flexaudio::ChunkFlags::empty(),
            dropped_before: 0,
            peak: 0.0,
            rms: 0.0,
        };
        let events = self.take_secondary_events();
        let state = self.secondary.as_mut().expect("secondary enabled");
        let chunk = state.chunk(chunk, events);
        state.ready.push_back(chunk);
    }
}

/// Independently poll one secondary chunk: 1 available, 0 absent, negative error.
/// Output initializes to NULL. Stop drains both taps; final chunks remain pollable.
/// Free any previously returned owner before reusing out.
/// # Safety
/// Stream must be live and exclusively borrowed; out must be writable and aligned.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_poll_secondary_chunk_v3(
    stream: *mut FlexStream,
    out: *mut *mut FlexSecondaryChunkV3,
) -> i32 {
    guard_i32(|| {
        error::clear_last_error();
        if let Err(status) = output(out) {
            return status;
        }
        if !valid_pointer(stream) {
            return invalid("invalid stream handle");
        }
        let stream = &mut *stream;
        if let Some(error) = stream.inner.terminal_error() {
            if let Some(state) = stream.secondary.as_mut() {
                state.ready.retain(|chunk| chunk.metadata.frames == 0);
                if let Some(chunk) = state.ready.pop_front() {
                    out.write(Box::into_raw(Box::new(chunk)));
                    return 1;
                }
            }
            return error::set_audio_error(error);
        }
        let ready = stream
            .secondary
            .as_mut()
            .and_then(|state| state.ready.pop_front());
        let chunk = ready.or_else(|| stream.next_secondary());
        let chunk = chunk.or_else(|| {
            stream.queue_secondary_carrier();
            stream
                .secondary
                .as_mut()
                .and_then(|state| state.ready.pop_front())
        });
        match chunk {
            Some(chunk) => {
                out.write(Box::into_raw(Box::new(chunk)));
                1
            }
            None => stream
                .inner
                .terminal_error()
                .map_or(0, error::set_audio_error),
        }
    })
}

/// Copy immutable metadata. Returns 1, or a negative typed error.
/// # Safety
/// Chunk must be a live owner; out must be writable and aligned.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_secondary_metadata_v3(
    chunk: *const FlexSecondaryChunkV3,
    out: *mut FlexSecondaryMetadataV3,
) -> i32 {
    guard_i32(|| {
        error::clear_last_error();
        if !valid_pointer(chunk) || !valid_pointer(out) {
            return invalid("invalid secondary metadata pointer");
        }
        out.write((*chunk).metadata);
        1
    })
}

/// Borrow native float PCM until chunk_free_v3. Wrong encoding returns NULL and
/// sets a typed InvalidArg. A valid len initializes to zero on errors.
/// # Safety
/// Chunk must be live; len must be writable and aligned.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_secondary_f32_v3(
    chunk: *const FlexSecondaryChunkV3,
    len: *mut usize,
) -> *const f32 {
    guard_const_ptr(|| {
        error::clear_last_error();
        if !valid_pointer(len) {
            invalid("invalid secondary length output");
            return ptr::null();
        }
        len.write(0);
        if !valid_pointer(chunk) {
            invalid("invalid secondary chunk");
            return ptr::null();
        }
        match &(*chunk).samples {
            Samples::F32(samples) => {
                len.write(samples.len());
                samples.as_ptr()
            }
            _ => {
                invalid("secondary chunk encoding is not f32");
                ptr::null()
            }
        }
    })
}

/// Borrow native int16_t PCM until chunk_free_v3. Wrong encoding returns NULL and
/// sets a typed InvalidArg. A valid len initializes to zero on errors.
/// # Safety
/// Chunk must be live; len must be writable and aligned.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_secondary_s16_v3(
    chunk: *const FlexSecondaryChunkV3,
    len: *mut usize,
) -> *const i16 {
    guard_const_ptr(|| {
        error::clear_last_error();
        if !valid_pointer(len) {
            invalid("invalid secondary length output");
            return ptr::null();
        }
        len.write(0);
        if !valid_pointer(chunk) {
            invalid("invalid secondary chunk");
            return ptr::null();
        }
        match &(*chunk).samples {
            Samples::S16(samples) => {
                len.write(samples.len());
                samples.as_ptr()
            }
            _ => {
                invalid("secondary chunk encoding is not s16");
                ptr::null()
            }
        }
    })
}

/// Borrow canonical attached VAD events carried by the secondary tap, until chunk_free_v3.
/// NULL/0 means no events. Events retain their dedicated capture timeline.
/// # Safety
/// Chunk must be live; len must be writable and aligned.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_secondary_vad_events_v3(
    chunk: *const FlexSecondaryChunkV3,
    len: *mut usize,
) -> *const FlexAttachedVadEvent {
    guard_const_ptr(|| {
        error::clear_last_error();
        if !valid_pointer(len) {
            invalid("invalid secondary event length output");
            return ptr::null();
        }
        len.write(0);
        if !valid_pointer(chunk) {
            invalid("invalid secondary chunk");
            return ptr::null();
        }
        let events = &(*chunk).events;
        len.write(events.len());
        if events.is_empty() {
            ptr::null()
        } else {
            events.as_ptr()
        }
    })
}

/// Release one owned chunk and all its borrowed views. NULL is safe.
/// # Safety
/// Chunk must be a live library owner or NULL. Release each owner exactly once.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_secondary_chunk_free_v3(chunk: *mut FlexSecondaryChunkV3) {
    guard_i32(|| {
        if chunk.is_null() {
            return code::FLEX_OK;
        }
        if !valid_pointer(chunk) {
            return invalid("invalid secondary chunk");
        }
        drop(Box::from_raw(chunk));
        code::FLEX_OK
    });
}
