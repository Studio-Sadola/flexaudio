//! Stream-integrated noise suppression and VAD.
//!
//! Based on `denoise` / `has_vad` in `FlexConfig`, keep Denoiser / VAD inside `FlexStream` and
//! process chunks as **denoise → VAD** just before `poll_chunk` returns them. This thin layer handles
//! construction ([`build_addons`]) and processing ([`FlexStream::poll_processed`]); add-on logic
//! remains in each crate (avoiding a god class).

use flexaudio::ChunkFlags;
use flexaudio_denoise::Denoiser;
use flexaudio_vad::Vad;

use crate::convert::{self, resolve_output, vad_config_from_c, vad_events_to_c};
use crate::error::set_last_error;
use crate::types::{FlexChunk, FlexConfig, FlexStream};

/// Build denoise / VAD add-ons from `FlexConfig` (used by `flexaudio_open`).
///
/// - If `denoise` is enabled, output rate must be 48000 (RNNoise is fixed at 48 kHz); otherwise return `Err`.
///   Create Denoiser using the resolved output channel count (including the sentinel value).
/// - If `has_vad` is enabled, map `vad` to [`VadConfig`](flexaudio_vad::VadConfig) and create VAD.
///
/// On failure, both set last_error and return `Err(())` (the caller can return NULL directly).
/// Disabled add-ons are `None`.
pub(crate) fn build_addons(config: &FlexConfig) -> Result<(Option<Denoiser>, Option<Vad>), ()> {
    let output = resolve_output(config);

    let denoiser = if config.denoise != 0 {
        // RNNoise requires 48 kHz. Reject other output rates during open.
        if output.sample_rate != 48_000 {
            set_last_error(format!(
                "denoise requires output_rate 48000 (0=default), got {}",
                output.sample_rate
            ));
            return Err(());
        }
        match Denoiser::new(output.channels) {
            Ok(d) => Some(d),
            Err(e) => {
                crate::error::set_audio_error(flexaudio::Error::InvalidArg(e.to_string()));
                return Err(());
            }
        }
    } else {
        None
    };

    let vad = if config.has_vad != 0 {
        let vad_config = vad_config_from_c(&config.vad);
        match Vad::new(vad_config) {
            Ok(v) => Some(v),
            Err(e) => {
                let kind = match e {
                    flexaudio_vad::VadError::InvalidConfig(_)
                    | flexaudio_vad::VadError::InvalidFormat(_) => {
                        flexaudio::Error::InvalidArg("invalid VAD configuration".into())
                    }
                    _ => flexaudio::Error::Backend("VAD construction failed".into()),
                };
                crate::error::set_audio_error(kind);
                return Err(());
            }
        }
    } else {
        None
    };

    Ok((denoiser, vad))
}

impl FlexStream {
    /// Poll one chunk, run enabled add-ons in **denoise → VAD** order, then convert it to
    /// `FlexChunk`. Return `None` if there is no chunk.
    ///
    /// - A chunk flagged DISCONTINUITY first flushes and clears the add-on state (no audio continuity
    ///   across it): VAD is flushed so an open speech segment is finalized and its events are delivered
    ///   with this chunk, then the denoiser is reset.
    /// - denoise: process interleaved data in place (48 kHz is guaranteed by open).
    /// - VAD: pass data in the output format (after denoise) to `process_pcm` and append finalized
    ///   events to `FlexChunk::vad_events` (after any events flushed for the discontinuity).
    /// - Metrics (`peak` / `rms`) are recomputed from the delivered data, which denoise rewrote.
    pub(crate) fn poll_processed(&mut self) -> Result<Option<FlexChunk>, flexaudio_vad::VadError> {
        self.poll_processed_with_flush(Vad::flush)
    }

    /// Keep the flush boundary injectable so recovery can be tested without changing the VAD API.
    fn poll_processed_with_flush(
        &mut self,
        flush: impl FnOnce(&mut Vad) -> Result<Vec<flexaudio_vad::VadEvent>, flexaudio_vad::VadError>,
    ) -> Result<Option<FlexChunk>, flexaudio_vad::VadError> {
        if self.whisper.is_none() {
            if let Some(chunk) = self.ready_chunks.pop_front() {
                return Ok(Some(chunk.chunk));
            }
        }
        let Some(mut chunk) = self.inner.poll_chunk() else {
            return Ok(None);
        };

        self.whisper_origin.1 = self.whisper_origin.1.max(chunk.pts_ns);

        // A DISCONTINUITY chunk (resume after pause, source switch, or dropped audio) is not
        // contiguous with what came before, so clear the add-ons' history: otherwise the denoise
        // delay line replays pre-gap audio into the first samples and VAD keeps counting across a
        // timeline that restarted. The core marks the first chunk after such a gap with this flag,
        // which is the existing signal for "reset your state" (the N-API binding resets its VAD the
        // same way).
        //
        // Flush VAD before clearing it. A bare reset discards an unreported open segment: both
        // SpeechStart and SpeechEnd are emitted only when that segment is finalized. Flush returns
        // that pair on the old sample clock and resets before processing post-gap audio.
        let mut vad_events = Vec::new();
        if chunk.flags.contains(ChunkFlags::DISCONTINUITY) {
            let mut flush_error = None;
            if let Some(vad) = self.vad.as_mut() {
                match flush(vad) {
                    Ok(events) => vad_events = events,
                    Err(error) => {
                        // A latched failure makes flush return before resetting. Attempt recovery
                        // anyway; if reset also fails, VAD retains that failure for later polls.
                        let _ = vad.reset();
                        flush_error = Some(error);
                    }
                }
            }
            if let Some(dn) = self.denoiser.as_mut() {
                dn.reset();
            }
            // Report the original flush error once, after both reset attempts. This poll consumes
            // the discontinuity chunk; the next poll starts with fresh state if reset succeeded.
            if let Some(error) = flush_error {
                return Err(error);
            }
        }

        // 1) denoise (in place). Length is frames×channels, hence divisible by channel count,
        //    so this should not fail; if it does, pass through the original data.
        if let Some(dn) = self.denoiser.as_mut() {
            dn.process(&mut chunk.data).map_err(|_| {
                flexaudio_vad::VadError::Inference("denoise processing failed".into())
            })?;
        }
        // Denoise rewrote the samples, so the core's peak / RMS describe pre-denoise audio while the
        // C API documents them as metrics of the delivered PCM. Recompute them from the final data
        // (same one-pass formula the core uses; VAD below only reads the samples).
        let (peak, rms) = peak_rms(&chunk.data);
        chunk.peak = peak;
        chunk.rms = rms;

        // 2) VAD. Pass the unchanged output format (guaranteed after open) to process_pcm.
        //    output is Copy, so save it before borrowing mutably. New events follow any events the
        //    discontinuity flush above already produced (pre-gap segment first, then this chunk).
        let output = self.inner.config().output;
        if let Some(vad) = self.vad.as_mut() {
            vad_events.extend(vad.process_pcm(&chunk.data, output.sample_rate, output.channels)?);
        }

        self.last_output = Some((
            chunk
                .frame_index
                .saturating_add(chunk.frames as u64 * 48_000 / u64::from(output.sample_rate)),
            chunk.pts_ns.saturating_add(
                chunk.frames as i64 * 1_000_000_000 / i64::from(output.sample_rate),
            ),
            chunk.seq.saturating_add(1),
        ));
        let mut fc = convert::chunk_to_c(chunk);
        let (ev_ptr, ev_len) = vad_events_to_c(vad_events);
        fc.vad_events = ev_ptr;
        fc.vad_events_len = ev_len;
        Ok(Some(fc))
    }
}

/// Peak (maximum absolute sample) and RMS (root mean square, linear) of interleaved `data`.
///
/// Mirrors the facade's private `peak_rms` so the C metrics are bit-identical to the core's for an
/// unchanged chunk. Empty data returns `(0.0, 0.0)`.
pub(crate) fn peak_rms(data: &[f32]) -> (f32, f32) {
    if data.is_empty() {
        return (0.0, 0.0);
    }
    let mut peak = 0.0f32;
    let mut sum_sq = 0.0f64;
    for &x in data {
        let a = x.abs();
        if a > peak {
            peak = a;
        }
        sum_sq += (x as f64) * (x as f64);
    }
    let rms = (sum_sq / data.len() as f64).sqrt() as f32;
    (peak, rms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::FlexVadConfig;
    use std::ptr;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    /// A backend whose `RawSink` is driven directly by the test (no audio hardware).
    struct PushBackend(Arc<Mutex<Option<flexaudio::core::backend::RawSink>>>);

    impl flexaudio::CaptureBackend for PushBackend {
        fn native_format(&self) -> (u32, u16) {
            (48_000, 1)
        }
        fn start(&mut self, sink: flexaudio::core::backend::RawSink) -> flexaudio::Result<()> {
            *self.0.lock().unwrap() = Some(sink);
            Ok(())
        }
        fn stop(&mut self) {
            self.0.lock().unwrap().take();
        }
    }

    /// Build a 48 kHz mono stream with integrated VAD using `threshold` for both the speech and
    /// silence thresholds, plus a sink the test can push into.
    fn vad_stream(
        threshold: f32,
    ) -> (
        FlexStream,
        Arc<Mutex<Option<flexaudio::core::backend::RawSink>>>,
    ) {
        vad_stream_with_max(threshold, 0)
    }

    fn vad_stream_with_max(
        threshold: f32,
        max_speech_ms: u32,
    ) -> (
        FlexStream,
        Arc<Mutex<Option<flexaudio::core::backend::RawSink>>>,
    ) {
        let sink = Arc::new(Mutex::new(None));
        let mut config = flexaudio::StreamConfig::default();
        config.output.channels = 1;
        let mut inner =
            flexaudio::Stream::open(config, Box::new(PushBackend(sink.clone()))).expect("open");
        let vad = Vad::new(flexaudio_vad::VadConfig {
            threshold,
            neg_threshold: Some(threshold),
            min_speech_ms: 0,
            min_silence_ms: 0,
            speech_pad_ms: 0,
            max_speech_ms,
            sample_rate: 16_000,
        })
        .expect("build vad");
        // Model construction can exceed the watchdog's idle limit under parallel tests. Start
        // capture only when the fixture is ready to supply PCM, avoiding unrelated recovery flags.
        inner.start().expect("start");
        (
            FlexStream {
                shutdown: None,
                shutdown_event_index: 0,
                last_output: None,
                whisper: None,
                whisper_events: Vec::new(),
                whisper_origin: (0, 0),
                whisper_error: None,
                whisper_error_reported: false,
                ready_chunks: std::collections::VecDeque::new(),
                inner,
                denoiser: None,
                vad: Some(vad),
            },
            sink,
        )
    }

    /// Push one 960-frame (20 ms) chunk and return the processed chunk.
    fn push_and_poll(
        stream: &mut FlexStream,
        sink: &Arc<Mutex<Option<flexaudio::core::backend::RawSink>>>,
    ) -> FlexChunk {
        assert_eq!(
            sink.lock().unwrap().as_mut().unwrap().push(&[0.0; 960], 0),
            960
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(chunk) = stream.poll_processed().expect("poll") {
                return chunk;
            }
            assert!(Instant::now() < deadline, "mock chunk timed out");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Release a `FlexChunk` returned by `poll_processed`.
    fn free(mut chunk: FlexChunk) {
        // SAFETY: this chunk is live and released exactly once.
        unsafe { crate::flexaudio_chunk_free(&mut chunk) };
    }

    /// A DISCONTINUITY chunk must flush an open VAD speech segment: the caller receives exactly one
    /// SpeechEnd (with its matching SpeechStart) attached to that chunk, and no later event refers to
    /// the old segment (it is finalized before the new timeline starts).
    #[test]
    fn discontinuity_flushes_open_vad_segment() {
        // threshold 0 makes every frame "speech"; min_speech 0 keeps the flushed segment.
        let (mut stream, sink) = vad_stream(0.0);
        // Two chunks (~639 converted 16k frames) exceed one 512-sample inference frame, so the
        // segmenter is triggered before the discontinuity.
        free(push_and_poll(&mut stream, &sink));
        free(push_and_poll(&mut stream, &sink));
        // pause + resume marks the next chunk DISCONTINUITY.
        stream.inner.pause();
        stream.inner.resume().expect("resume");
        let mut chunk = push_and_poll(&mut stream, &sink);
        assert_ne!(chunk.flags & ChunkFlags::DISCONTINUITY.bits(), 0);
        let events: Vec<(i32, i64)> = unsafe {
            std::slice::from_raw_parts(chunk.vad_events, chunk.vad_events_len)
                .iter()
                .map(|ev| (ev.kind, ev.at_sample))
                .collect()
        };
        unsafe { crate::flexaudio_chunk_free(&mut chunk) };
        stream.inner.stop();
        // Exactly one SpeechEnd (kind 1) and it is the last event, so nothing after it refers to the
        // flushed segment.
        assert_eq!(
            events.iter().filter(|(kind, _)| *kind == 1).count(),
            1,
            "flushed events: {events:?}"
        );
        assert_eq!(
            events.last().map(|(kind, _)| *kind),
            Some(1),
            "events: {events:?}"
        );
        // The matching SpeechStart precedes its SpeechEnd.
        let end_index = events
            .iter()
            .position(|(kind, _)| *kind == 1)
            .expect("one SpeechEnd");
        assert_eq!(events[end_index - 1].0, 0, "events: {events:?}");
    }

    /// Control: with no open segment, a DISCONTINUITY chunk emits no VAD event (the flush is a no-op).
    #[test]
    fn discontinuity_without_open_segment_emits_nothing() {
        // threshold 1 is unreachable for a sigmoid probability, so the segmenter never triggers.
        let (mut stream, sink) = vad_stream(1.0);
        free(push_and_poll(&mut stream, &sink));
        free(push_and_poll(&mut stream, &sink));
        stream.inner.pause();
        stream.inner.resume().expect("resume");
        let mut chunk = push_and_poll(&mut stream, &sink);
        assert_ne!(chunk.flags & ChunkFlags::DISCONTINUITY.bits(), 0);
        let len = chunk.vad_events_len;
        let null = chunk.vad_events.is_null();
        unsafe { crate::flexaudio_chunk_free(&mut chunk) };
        stream.inner.stop();
        assert_eq!(len, 0, "no segment was open, so no event should be emitted");
        assert!(null);
    }

    fn events(chunk: &FlexChunk) -> Vec<(i32, i64)> {
        if chunk.vad_events_len == 0 {
            return Vec::new();
        }
        // SAFETY: poll_processed owns this live event array until free is called.
        unsafe { std::slice::from_raw_parts(chunk.vad_events, chunk.vad_events_len) }
            .iter()
            .map(|event| (event.kind, event.at_sample))
            .collect()
    }

    #[test]
    fn discontinuity_chunk_separates_old_and_new_vad_timelines() {
        let (mut stream, sink) = vad_stream_with_max(0.0, 32);
        for _ in 0..2 {
            let chunk = push_and_poll(&mut stream, &sink);
            assert!(
                events(&chunk).is_empty(),
                "open segment is not yet reported"
            );
            free(chunk);
        }
        stream.inner.pause();
        stream.inner.resume().unwrap();
        let chunk = push_and_poll(&mut stream, &sink);
        assert_ne!(chunk.flags & ChunkFlags::DISCONTINUITY.bits(), 0);
        assert_eq!(chunk.frames, 960, "the core delivers a 20 ms chunk");
        assert_eq!(events(&chunk), [(0, 0), (1, 512)], "old timeline only");
        assert_eq!(
            stream.vad.as_ref().unwrap().converted_sample_position(),
            319
        );
        free(chunk);

        // Post-gap speech starts inside the same chunk, but 319 converted samples cannot complete
        // a fresh 512-sample frame. Its boundaries are finalized and delivered on a later chunk.
        for index in 0..3 {
            let chunk = push_and_poll(&mut stream, &sink);
            assert_eq!(chunk.flags & ChunkFlags::DISCONTINUITY.bits(), 0);
            if index == 2 {
                assert_eq!(events(&chunk), [(0, 0), (1, 1024)], "new timeline only");
            } else {
                assert!(events(&chunk).is_empty());
            }
            free(chunk);
        }
        stream.inner.stop();
    }

    #[test]
    fn discontinuity_flush_error_resets_both_addons_and_is_reported_once() {
        use std::cell::Cell;

        // Inject at the flush boundary: a latched core failure returns this error without resetting.
        // The no-error case uses the real flush as a control for the same reset/processing path.
        for fail_flush in [false, true] {
            let (mut stream, sink) = vad_stream(0.0);
            free(push_and_poll(&mut stream, &sink));
            free(push_and_poll(&mut stream, &sink));
            let old_position = stream.vad.as_ref().unwrap().converted_sample_position();
            let mut denoiser = Denoiser::new(1).unwrap();
            denoiser.process(&mut [0.8; 137]).unwrap();
            stream.denoiser = Some(denoiser);
            stream.inner.pause();
            stream.inner.resume().unwrap();
            assert_eq!(
                sink.lock().unwrap().as_mut().unwrap().push(&[0.0; 960], 0),
                960
            );
            let error = flexaudio_vad::VadError::Inference("injected latched VAD failure".into());
            let flush_calls = Cell::new(0);
            let deadline = Instant::now() + Duration::from_secs(2);
            let mut reference = Denoiser::new(1).unwrap();
            let mut error_count = 0;
            loop {
                let result = stream.poll_processed_with_flush(|vad| {
                    flush_calls.set(flush_calls.get() + 1);
                    assert_eq!(vad.converted_sample_position(), old_position);
                    if fail_flush {
                        Err(error.clone())
                    } else {
                        vad.flush()
                    }
                });
                match result {
                    Err(actual) => {
                        assert!(fail_flush);
                        assert_eq!(actual, error, "preserve the original flush error");
                        error_count += 1;
                        assert_eq!(stream.vad.as_ref().unwrap().converted_sample_position(), 0);
                        break;
                    }
                    Ok(Some(chunk)) => {
                        assert!(!fail_flush);
                        assert_eq!(events(&chunk), [(0, 0), (1, 512)]);
                        let mut expected = [0.0; 960];
                        reference.process(&mut expected).unwrap();
                        // SAFETY: the chunk's PCM allocation is live until free below.
                        assert_eq!(
                            unsafe { std::slice::from_raw_parts(chunk.data, chunk.len) },
                            expected
                        );
                        free(chunk);
                        break;
                    }
                    Ok(None) => {
                        assert!(Instant::now() < deadline, "mock chunk timed out");
                        std::thread::sleep(Duration::from_millis(2));
                    }
                }
            }
            assert_eq!(flush_calls.get(), 1);
            for _ in 0..3 {
                let chunk = push_and_poll(&mut stream, &sink);
                let mut expected = [0.0; 960];
                reference.process(&mut expected).unwrap();
                // SAFETY: the chunk's PCM allocation is live until free below.
                assert_eq!(
                    unsafe { std::slice::from_raw_parts(chunk.data, chunk.len) },
                    expected
                );
                free(chunk);
            }
            assert_eq!(error_count, usize::from(fail_flush));
            assert_eq!(
                stream.vad.as_ref().unwrap().converted_sample_position(),
                if fail_flush { 959 } else { 1279 }
            );
            assert!(stream.poll_processed().unwrap().is_none());
            stream.inner.stop();
        }
    }

    fn zero_vad() -> FlexVadConfig {
        FlexVadConfig {
            threshold: 0.0,
            neg_threshold: 0.0,
            min_speech_ms: 0,
            min_silence_ms: 0,
            speech_pad_ms: 0,
            max_speech_ms: 0,
            sample_rate: 0,
        }
    }

    fn base_config() -> FlexConfig {
        FlexConfig {
            kind: crate::types::FlexSourceKind::Mic as i32,
            device_id: ptr::null(),
            process_id: 0,
            mode: crate::types::FlexProcessMode::Include as i32,
            exclude_self: 0,
            output_rate: 0,
            output_channels: 0,
            chunk_ms: 0,
            gain: 0.0,
            mix_mic_device_id: ptr::null(),
            mix_system_device_id: ptr::null(),
            mix_mic_gain: 0.0,
            mix_system_gain: 0.0,
            denoise: 0,
            has_vad: 0,
            vad: zero_vad(),
        }
    }

    #[test]
    fn no_addons_yields_none() {
        let c = base_config();
        let (dn, vad) = build_addons(&c).expect("Disabled add-ons always succeed");
        assert!(dn.is_none());
        assert!(vad.is_none());
    }

    #[test]
    fn denoise_requires_48k_output() {
        // Denoise enabled + non-48k → Err.
        let mut c = base_config();
        c.denoise = 1;
        c.output_rate = 16_000;
        assert!(build_addons(&c).is_err());

        // Denoise enabled + explicit 48k → Ok and creates Denoiser.
        let mut c48 = base_config();
        c48.denoise = 1;
        c48.output_rate = 48_000;
        let (dn, _) = build_addons(&c48).expect("48k should succeed");
        assert!(dn.is_some());

        // Denoise enabled + default (output_rate=0 → 48000) → Ok.
        let mut cdef = base_config();
        cdef.denoise = 1;
        let (dn2, _) = build_addons(&cdef).expect("Default 48k should succeed");
        assert!(dn2.is_some());
    }
}
