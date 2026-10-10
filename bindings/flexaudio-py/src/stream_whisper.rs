//! Serialized canonical capture intake and ordered polling carriers.
use super::*;

impl Stream {
    pub(super) fn accept_whisper(
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

    pub(super) fn drain_capture(&mut self) {
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
                chunk.flags.contains(fa::ChunkFlags::DISCONTINUITY),
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

    pub(super) fn whisper_carrier(&mut self) {
        if self.whisper_events.is_empty() {
            return;
        }
        let mut chunk = chunk_to_py(fa::AudioChunk {
            data: Vec::new(),
            frames: 0,
            frame_index: self.whisper_origin.0,
            pts_ns: self.whisper_origin.1,
            seq: 0,
            flags: fa::ChunkFlags::empty(),
            dropped_before: 0,
            peak: 0.0,
            rms: 0.0,
        });
        chunk.set_whisper_events(std::mem::take(&mut self.whisper_events));
        self.ready_chunks.push_back(chunk);
    }

    pub(super) fn drain_output(&mut self) -> PyResult<()> {
        while let Some(chunk) = self.poll_chunk_with_flush(CoreVad::flush)? {
            self.ready_chunks.push_back(chunk);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    struct PushBackend(Arc<Mutex<Option<fa::core::backend::RawSink>>>);
    impl fa::CaptureBackend for PushBackend {
        fn native_format(&self) -> (u32, u16) {
            (48_000, 2)
        }
        fn start(&mut self, sink: fa::core::backend::RawSink) -> fa::Result<()> {
            *self.0.lock().unwrap() = Some(sink);
            Ok(())
        }
        fn stop(&mut self) {
            self.0.lock().unwrap().take();
        }
    }

    fn next_chunk(stream: &mut Stream) -> PyAudioChunk {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(chunk) = stream.poll_chunk().unwrap() {
                return chunk;
            }
            assert!(Instant::now() < deadline, "fake capture timed out");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn attached_python_chunks_switch_flush_and_stop_are_ordered() {
        Python::initialize();
        Python::attach(|py| {
            let sink = Arc::new(Mutex::new(None));
            let mut inner = fa::Stream::open(
                fa::StreamConfig::default(),
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
            let mut stream = Stream {
                inner,
                whisper: Some(whisper),
                whisper_events: Vec::new(),
                whisper_origin: (0, 0),
                whisper_error: None,
                whisper_error_reported: false,
                ready_chunks: Default::default(),
                vad: None,
                denoiser: None,
                output_rate: 48_000,
                output_channels: 2,
            };
            for expected in [0, 960] {
                sink.lock().unwrap().as_mut().unwrap().push(&[0.0; 1920], 0);
                let chunk = Py::new(py, next_chunk(&mut stream)).unwrap();
                assert_eq!(
                    chunk
                        .bind(py)
                        .getattr("frame_index")
                        .unwrap()
                        .extract::<u64>()
                        .unwrap(),
                    expected
                );
                if expected == 0 {
                    let events = chunk.bind(py).getattr("whisper_vad_events").unwrap();
                    let start = events.get_item(0).unwrap();
                    assert_eq!(
                        start
                            .getattr("capture_sample")
                            .unwrap()
                            .extract::<u64>()
                            .unwrap(),
                        expected
                    );
                    assert_eq!(start.getattr("seq").unwrap().extract::<u64>().unwrap(), 0);
                }
            }
            stream
                .inner
                .switch_backend(Box::new(PushBackend(sink.clone())))
                .unwrap();
            sink.lock().unwrap().as_mut().unwrap().push(&[0.0; 1920], 0);
            let chunk = Py::new(py, next_chunk(&mut stream)).unwrap();
            let chunk = chunk.bind(py);
            let index = chunk
                .getattr("frame_index")
                .unwrap()
                .extract::<u64>()
                .unwrap();
            assert_eq!(index, 1920);
            let events = chunk.getattr("whisper_vad_events").unwrap();
            let mut ended = false;
            let mut restarted = false;
            for event in events.try_iter().unwrap() {
                let event = event.unwrap();
                let kind = event.getattr("type").unwrap().extract::<String>().unwrap();
                if kind == "epoch_end" {
                    ended = true;
                }
                if kind == "epoch_start" {
                    assert!(ended);
                    assert_eq!(
                        event
                            .getattr("capture_sample")
                            .unwrap()
                            .extract::<u64>()
                            .unwrap(),
                        index
                    );
                    restarted = true;
                }
            }
            assert!(restarted);
            stream.flush_whisper_vad().unwrap();
            let carrier = Py::new(py, next_chunk(&mut stream)).unwrap();
            assert_eq!(
                carrier
                    .bind(py)
                    .getattr("frames")
                    .unwrap()
                    .extract::<usize>()
                    .unwrap(),
                0
            );
            let events = carrier.bind(py).getattr("whisper_vad_events").unwrap();
            assert_eq!(
                events
                    .get_item(-1)
                    .unwrap()
                    .getattr("type")
                    .unwrap()
                    .extract::<String>()
                    .unwrap(),
                "epoch_end"
            );
            stream.flush_whisper_vad().unwrap();
            assert!(stream.poll_chunk().unwrap().is_none());
            sink.lock().unwrap().as_mut().unwrap().push(&[0.0; 1920], 0);
            let _ = next_chunk(&mut stream);
            stream.stop().unwrap();
            let terminal = Py::new(py, next_chunk(&mut stream)).unwrap();
            let events = terminal.bind(py).getattr("whisper_vad_events").unwrap();
            assert_eq!(
                events
                    .get_item(-1)
                    .unwrap()
                    .getattr("type")
                    .unwrap()
                    .extract::<String>()
                    .unwrap(),
                "epoch_end"
            );
            stream.stop().unwrap();
            assert!(stream.poll_chunk().unwrap().is_none());
        });
    }
}
