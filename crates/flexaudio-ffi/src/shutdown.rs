//! One checked binding teardown, shared by explicit stop and free.
use crate::types::FlexStream;
use flexaudio::{AudioChunk, ChunkFlags, Error, ErrorContext, Operation, ShutdownReport};

impl FlexStream {
    pub(crate) fn poll_binding_event(&mut self) -> Option<flexaudio::Event> {
        if let Some(event) = self.inner.poll_event() {
            return Some(event);
        }
        let error = self
            .shutdown
            .as_ref()?
            .cleanup()
            .get(self.shutdown_event_index)?
            .clone();
        self.shutdown_event_index += 1;
        Some(flexaudio::Event::ShutdownError { error })
    }

    pub(crate) fn stop_binding(&mut self) -> flexaudio::Result<()> {
        if let Some(report) = &self.shutdown {
            return report.result();
        }
        let _ = self.inner.stop_checked();
        let core = self
            .inner
            .shutdown_report()
            .expect("checked stop retains report");
        let mut cleanup = core.cleanup().to_vec();
        self.shutdown_event_index = cleanup.len();
        if core.primary().is_none() {
            if self.whisper.is_some() {
                self.stop_whisper();
            } else {
                // Collect first, then queue: polling also reads the retained delivery queue.
                let mut chunks = Vec::new();
                loop {
                    match self.poll_processed() {
                        Ok(Some(chunk)) => chunks.push(chunk),
                        Ok(None) => break,
                        Err(_) => {
                            cleanup.push(
                                Error::Backend("binding processing failed during shutdown".into())
                                    .with_context(ErrorContext::new(Operation::Flush)),
                            );
                            break;
                        }
                    }
                }
                let mut events = Vec::new();
                let output = self.inner.config().output;
                let tail = self
                    .denoiser
                    .as_mut()
                    .filter(|_| self.last_output.is_some())
                    .map(flexaudio_denoise::Denoiser::flush)
                    .unwrap_or_default();
                if let Some(vad) = self.vad.as_mut() {
                    if !tail.is_empty() {
                        match vad.process_pcm(&tail, output.sample_rate, output.channels) {
                            Ok(e) => events.extend(e),
                            Err(_) => cleanup.push(
                                Error::Backend("VAD tail processing failed".into())
                                    .with_context(ErrorContext::new(Operation::Flush)),
                            ),
                        }
                    }
                    match vad.flush() {
                        Ok(e) => events.extend(e),
                        Err(_) => cleanup.push(
                            Error::Backend("VAD flush failed".into())
                                .with_context(ErrorContext::new(Operation::Flush)),
                        ),
                    }
                }
                if !tail.is_empty() || !events.is_empty() {
                    let (frame_index, pts_ns, seq) = self.last_output.unwrap_or((0, 0, 0));
                    let (peak, rms) = crate::integration::peak_rms(&tail);
                    let chunk = AudioChunk {
                        frames: tail.len() / usize::from(output.channels),
                        data: tail,
                        frame_index,
                        pts_ns,
                        seq,
                        flags: ChunkFlags::empty(),
                        dropped_before: 0,
                        peak,
                        rms,
                    };
                    let mut chunk = crate::convert::chunk_to_c(chunk);
                    let (pointer, len) = crate::convert::vad_events_to_c(events);
                    chunk.vad_events = pointer;
                    chunk.vad_events_len = len;
                    chunks.push(chunk);
                }
                for chunk in chunks {
                    let chunk = self.versioned_chunk(chunk);
                    self.ready_chunks.push_back(chunk);
                }
            }
        } else {
            // Capture primary suppresses PCM while closing attached event epochs.
            if let Some(d) = self.denoiser.as_mut() {
                d.reset();
            }
            self.stop_whisper();
        }
        if self.whisper_error.is_some() {
            cleanup.push(
                Error::Backend("whisper VAD shutdown failed".into())
                    .with_context(ErrorContext::new(Operation::Flush)),
            );
        }
        let report = ShutdownReport::new(core.primary().cloned(), cleanup);
        let result = report.result();
        self.shutdown = Some(report);
        result
    }
}
