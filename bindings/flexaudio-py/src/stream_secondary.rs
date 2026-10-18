//! Secondary polling and event carriers, independent from primary delivery.
use super::*;
use crate::secondary::PySecondaryChunk;

impl Stream {
    pub(super) fn poll_processed_secondary(&mut self) -> Option<PySecondaryChunk> {
        let options = self.secondary?;
        self.drain_capture();
        let chunk = self.inner.poll_secondary()?;
        self.secondary_metadata = (
            chunk.frame_index,
            chunk.pts_ns,
            chunk.seq,
            chunk.dropped_before,
        );
        let mut result = PySecondaryChunk::new(chunk, options.encoding);
        if self.vad_secondary {
            result.vad_events = Some(std::mem::take(&mut self.vad_events));
        }
        Some(result)
    }

    pub(super) fn secondary_vad_carrier(&mut self) {
        let Some(options) = self.secondary else {
            return;
        };
        let (frame_index, pts_ns, seq, dropped_before) = self.secondary_metadata;
        let mut carrier = PySecondaryChunk::new(
            fa::SecondaryChunk {
                samples: Vec::new(),
                frames: 0,
                frame_index,
                pts_ns: pts_ns.max(self.vad_origin.1),
                seq,
                flags: fa::ChunkFlags::empty(),
                dropped_before,
                peak: 0.0,
                rms: 0.0,
            },
            options.encoding,
        );
        carrier.vad_events = Some(std::mem::take(&mut self.vad_events));
        self.ready_secondary.push_back(carrier);
    }

    pub(super) fn drain_secondary(&mut self) {
        for _ in 0..self.inner.config().ring_capacity_chunks {
            let Some(chunk) = self.poll_processed_secondary() else {
                break;
            };
            self.ready_secondary.push_back(chunk);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    struct Input(Arc<Mutex<Option<fa::core::backend::RawSink>>>);
    impl fa::CaptureBackend for Input {
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

    fn fixture(
        vad: bool,
        encoding: crate::secondary::Encoding,
    ) -> (Stream, Arc<Mutex<Option<fa::core::backend::RawSink>>>) {
        let sink = Arc::new(Mutex::new(None));
        let format = fa::OutputFormat::default();
        let config = fa::StreamConfig {
            secondary_output: Some(format),
            ..Default::default()
        };
        let mut inner = fa::Stream::open(config, Box::new(Input(sink.clone()))).unwrap();
        if vad {
            inner.enable_capture_tap().unwrap();
        }
        inner.start().unwrap();
        (
            Stream {
                inner,
                secondary: Some(crate::secondary::SecondaryOutputOptions { format, encoding }),
                ready_secondary: Default::default(),
                vad_secondary: vad,
                secondary_metadata: (0, 0, 0, 0),
                shutdown: None,
                local_events: Default::default(),
                vad_tap: vad.then(|| {
                    flexaudio_vad::VadTap::new(Default::default(), Default::default()).unwrap()
                }),
                vad_events: Vec::new(),
                vad_origin: (0, 0),
                vad_error: None,
                vad_cleanup: Vec::new(),
                ready_chunks: Default::default(),
            },
            sink,
        )
    }

    #[test]
    fn secondary_polling_is_independent_and_stop_drains_both_taps() {
        for encoding in [
            crate::secondary::Encoding::F32,
            crate::secondary::Encoding::S16,
        ] {
            let (mut stream, sink) = fixture(false, encoding);
            let samples: Vec<_> = (0..3840)
                .map(|i| if i % 2 == 0 { 0.123456 } else { -0.333333 })
                .collect();
            sink.lock().unwrap().as_mut().unwrap().push(&samples, 0);
            let deadline = Instant::now() + Duration::from_secs(5);
            let secondary = loop {
                if let Some(chunk) = stream.poll_secondary().unwrap() {
                    break chunk;
                }
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(2));
            };
            assert_eq!(secondary.chunk.samples, samples[..1920]);
            assert_eq!(secondary.chunk.seq, 0);
            let primary = stream
                .poll_chunk()
                .unwrap()
                .expect("secondary polling must preserve primary");
            assert_eq!(primary.samples(), secondary.chunk.samples);
            stream.stop().unwrap();
            let mut primary_frames = 960;
            let mut secondary_frames = secondary.chunk.frames;
            while let Some(chunk) = stream.poll_chunk().unwrap() {
                primary_frames += chunk.samples().len() / 2;
            }
            while let Some(chunk) = stream.poll_secondary().unwrap() {
                secondary_frames += chunk.chunk.frames;
            }
            assert_eq!(primary_frames, 1920);
            assert_eq!(secondary_frames, 1920);
            assert!(stream.poll_secondary().unwrap().is_none());
        }
    }

    #[test]
    fn secondary_vad_uses_canonical_capture_and_keeps_closing_events_on_secondary() {
        Python::initialize();
        Python::attach(|_| {
            let (mut stream, sink) = fixture(true, crate::secondary::Encoding::S16);
            sink.lock()
                .unwrap()
                .as_mut()
                .unwrap()
                .push(&[0.25; 1920], 0);
            let deadline = Instant::now() + Duration::from_secs(5);
            let secondary = loop {
                if let Some(chunk) = stream.poll_secondary().unwrap() {
                    break chunk;
                }
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(2));
            };
            assert!(matches!(
                secondary.vad_events.as_ref().unwrap().first(),
                Some(flexaudio_vad::AttachedVadEvent::EpochStart {
                    capture_sample: 0,
                    ..
                })
            ));
            assert!(stream.poll_chunk().unwrap().is_some());
            stream.flush_vad().unwrap();
            let closing = stream
                .poll_secondary()
                .unwrap()
                .expect("secondary flush carrier");
            assert!(closing.chunk.samples.is_empty());
            assert!(!closing.vad_events.unwrap().is_empty());
            assert!(stream.poll_chunk().unwrap().is_none());
            stream.stop().unwrap();
            while stream.poll_secondary().unwrap().is_some() {}
        });
    }
}
