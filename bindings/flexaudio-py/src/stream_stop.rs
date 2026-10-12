//! The single retained shutdown path for explicit stop, context exit, and drop.
use super::*;

impl Stream {
    pub(super) fn finish_shutdown(&mut self) -> fa::Result<()> {
        if let Some(report) = &self.shutdown {
            return report.result();
        }
        let _ = self.inner.stop_checked();
        let core_report = self.inner.shutdown_report();
        let primary = self.inner.terminal_error();
        let mut cleanup = core_report
            .map(|r| r.cleanup().to_vec())
            .unwrap_or_default();
        let before = cleanup.len();
        self.drain_capture();
        if primary.is_none() {
            if self.drain_output().is_err() {
                cleanup.push(flush_error("binding output drain failed"));
            }
            let samples = if self.output_end.is_some() {
                self.denoiser
                    .as_mut()
                    .map(CoreDenoiser::flush)
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            let mut tail = self.final_chunk(samples);
            if let Some(vad) = self.vad.as_mut() {
                let process =
                    vad.process_pcm(tail.samples(), self.output_rate, self.output_channels);
                match process {
                    Ok(events) => tail.set_vad_events(vad_event_pairs(events)),
                    Err(_) => cleanup.push(flush_error("binding VAD tail processing failed")),
                }
                match vad.flush() {
                    Ok(events) => tail.extend_vad_events(vad_event_pairs(events)),
                    Err(_) => cleanup.push(flush_error("binding VAD flush failed")),
                }
            }
            if tail.has_audio_or_vad() {
                self.ready_chunks.push_back(tail);
            }
        } else {
            self.ready_chunks.retain(|chunk| chunk.samples().is_empty());
        }
        if let Some(whisper) = self.whisper.as_mut() {
            let result = whisper.stop();
            self.accept_whisper(result, fa::Operation::Stop);
            self.whisper_carrier();
        }
        cleanup.extend(self.whisper_cleanup.iter().cloned());
        self.local_events.extend(
            cleanup[before..]
                .iter()
                .cloned()
                .map(|error| fa::Event::ShutdownError { error }),
        );
        let report = fa::ShutdownReport::new(primary, cleanup);
        let result = report.result();
        self.shutdown = Some(report);
        result
    }

    fn final_chunk(&self, samples: Vec<f32>) -> PyAudioChunk {
        let (frame_index, pts_ns, next_seq, dropped_before) =
            self.output_end.unwrap_or((0, 0, 0, 0));
        let mut chunk = chunk_to_py(fa::AudioChunk {
            seq: if samples.is_empty() {
                next_seq.saturating_sub(1)
            } else {
                next_seq
            },
            frames: samples.len() / usize::from(self.output_channels),
            data: samples,
            frame_index,
            pts_ns,
            dropped_before,
            flags: fa::ChunkFlags::empty(),
            peak: 0.0,
            rms: 0.0,
        });
        chunk.update_metrics();
        chunk
    }
}
fn flush_error(message: &str) -> fa::Error {
    fa::Error::Backend(message.into()).with_context(fa::ErrorContext::new(fa::Operation::Flush))
}
impl Drop for Stream {
    fn drop(&mut self) {
        let _ = self.finish_shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pyo3::ffi::c_str;

    struct CleanupFailure;
    impl fa::CaptureBackend for CleanupFailure {
        fn native_format(&self) -> (u32, u16) {
            (48_000, 2)
        }
        fn start(&mut self, _sink: fa::core::backend::RawSink) -> fa::Result<()> {
            Ok(())
        }
        fn stop(&mut self) {}
        fn stop_checked(&mut self) -> fa::Result<()> {
            Err(fa::Error::DeviceLost.with_context(fa::ErrorContext::new(fa::Operation::Stop)))
        }
    }
    fn fixture() -> Stream {
        let mut inner =
            fa::Stream::open(fa::StreamConfig::default(), Box::new(CleanupFailure)).unwrap();
        inner.start().unwrap();
        Stream {
            inner,
            shutdown: None,
            local_events: Default::default(),
            output_end: None,
            whisper: None,
            whisper_events: Vec::new(),
            whisper_origin: (0, 0),
            whisper_error: None,
            whisper_cleanup: Vec::new(),
            ready_chunks: Default::default(),
            denoiser: None,
            vad: None,
            output_rate: 48_000,
            output_channels: 2,
        }
    }
    struct StartupBurst;
    impl fa::CaptureBackend for StartupBurst {
        fn native_format(&self) -> (u32, u16) {
            (48_000, 2)
        }
        fn start(&mut self, mut sink: fa::core::backend::RawSink) -> fa::Result<()> {
            let mut samples = vec![2.0; 960];
            samples.extend([0.25; 3840]);
            sink.push(&samples, 0);
            Ok(())
        }
        fn stop(&mut self) {}
    }

    #[test]
    fn startup_timeout_raises_without_event_polling() {
        struct InvalidStartup;
        impl fa::CaptureBackend for InvalidStartup {
            fn native_format(&self) -> (u32, u16) {
                (48_000, 2)
            }
            fn start(&mut self, mut sink: fa::core::backend::RawSink) -> fa::Result<()> {
                sink.push(&[2.0; 960], 0);
                Ok(())
            }
            fn stop(&mut self) {}
        }
        Python::initialize();
        Python::attach(|py| {
            let mut stream = startup_fixture_with_backend(true, Box::new(InvalidStartup));
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(4);
            loop {
                match stream.poll_chunk() {
                    Ok(Some(_)) => panic!("out-of-range startup PCM must never escape"),
                    Ok(None) => {}
                    Err(error) => {
                        assert!(error.is_instance_of::<pyo3::exceptions::PyRuntimeError>(py));
                        assert!(error.to_string().contains("within 2 seconds"));
                        assert!(stream.terminal_error().is_some());
                        break;
                    }
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "startup timeout must surface without event polling"
                );
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            let mut carrier = stream.final_chunk(Vec::new());
            carrier.set_vad_events(vad_event_pairs(vec![flexaudio_vad::VadEvent::SpeechEnd {
                at_sample: 512,
            }]));
            stream.ready_chunks.push_back(carrier);
            let closure = stream
                .poll_chunk()
                .expect("retained closure before any event poll")
                .expect("terminal VAD closure remains independently deliverable");
            assert!(closure.samples().is_empty());
            assert!(
                stream.poll_chunk().is_err(),
                "the primary terminal error remains visible after the closure"
            );
            let _ = stream.stop();
        });
    }
    fn startup_fixture(whisper: bool) -> Stream {
        startup_fixture_with_backend(whisper, Box::new(StartupBurst))
    }

    #[test]
    fn pcm_and_attached_epoch_do_not_require_stream_event_polling() {
        Python::initialize();
        Python::attach(|py| {
            let mut stream = startup_fixture(true);
            stream
                .inner
                .stop_checked()
                .expect("quiescent synthetic producer");
            let mut samples = 0;
            let mut starts = 0;
            while let Some(chunk) = stream.poll_chunk().expect("PCM-only pull") {
                samples += chunk.samples().len();
                let object = Py::new(py, chunk).expect("chunk object");
                let events = object
                    .bind(py)
                    .getattr("whisper_vad_events")
                    .expect("attached events");
                if !events.is_none() {
                    for event in events.try_iter().expect("events iterable") {
                        if event
                            .expect("event")
                            .getattr("type")
                            .expect("type")
                            .extract::<String>()
                            .expect("type string")
                            == "epoch_start"
                        {
                            starts += 1;
                        }
                    }
                }
            }
            assert_eq!(
                samples, 3840,
                "all accepted PCM must be available without any event poll"
            );
            assert_eq!(
                starts, 1,
                "the attached epoch must start without an event poll"
            );
            stream.stop().expect("complete synthetic shutdown");
            assert!(stream.poll_chunk().expect("closing epoch").is_some());
            assert!(stream.poll_chunk().expect("empty after closure").is_none());
            assert!(
                stream.poll_event().is_some(),
                "PCM polling must preserve the startup notice"
            );
        });
    }

    #[test]
    fn startup_notice_is_queued_before_first_pcm_poll() {
        Python::initialize();
        Python::attach(|py| {
            let mut stream = startup_fixture(false);
            stream
                .inner
                .stop_checked()
                .expect("quiescent synthetic producer");
            let event = stream
                .poll_event()
                .expect("startup notice is already queued")
                .to_dict(py)
                .expect("event object");
            assert_eq!(
                event
                    .get_item("type")
                    .expect("type lookup")
                    .expect("type")
                    .extract::<String>()
                    .expect("type string"),
                "startupSamplesDropped"
            );
            assert!(stream.poll_chunk().expect("first PCM poll").is_some());
            stream.stop().expect("synthetic shutdown");
        });
    }
    fn startup_fixture_with_backend(whisper: bool, backend: Box<dyn fa::CaptureBackend>) -> Stream {
        let mut inner = fa::Stream::open(Default::default(), backend).expect("synthetic stream");
        let whisper_owner = if whisper {
            inner.enable_capture_tap().expect("canonical tap");
            Some(
                flexaudio_vad::WhisperVadTap::new(Default::default(), Default::default())
                    .expect("offline tap"),
            )
        } else {
            None
        };
        let vad = if whisper {
            None
        } else {
            Some(
                CoreVad::new(flexaudio_vad::VadConfig {
                    threshold: 0.0,
                    neg_threshold: Some(0.0),
                    min_speech_ms: 0,
                    min_silence_ms: 0,
                    speech_pad_ms: 0,
                    ..Default::default()
                })
                .expect("offline VAD"),
            )
        };
        inner.start().expect("synthetic capture");
        Stream {
            inner,
            shutdown: None,
            local_events: Default::default(),
            output_end: None,
            whisper: whisper_owner,
            whisper_events: Vec::new(),
            whisper_origin: (0, 0),
            whisper_error: None,
            whisper_cleanup: Vec::new(),
            ready_chunks: Default::default(),
            denoiser: None,
            vad,
            output_rate: 48_000,
            output_channels: 2,
        }
    }
    #[test]
    fn flush_before_startup_notice_finalizes_the_accepted_whisper_prefix() {
        Python::initialize();
        Python::attach(|py| {
            let mut stream = startup_fixture(true);
            stream
                .inner
                .stop_checked()
                .expect("quiescent synthetic producer");
            stream
                .flush_whisper_vad()
                .expect("materialize pending accepted input");
            let mut starts = 0;
            let mut ends = 0;
            while let Some(chunk) = stream.poll_chunk().expect("flushed output") {
                let object = Py::new(py, chunk).expect("chunk object");
                let events = object
                    .bind(py)
                    .getattr("whisper_vad_events")
                    .expect("attached events");
                if !events.is_none() {
                    for event in events.try_iter().expect("events iterable") {
                        match event
                            .expect("event")
                            .getattr("type")
                            .expect("type")
                            .extract::<String>()
                            .expect("type string")
                            .as_str()
                        {
                            "epoch_start" => starts += 1,
                            "epoch_end" => ends += 1,
                            _ => {}
                        }
                    }
                }
            }
            assert_eq!(starts, 1);
            assert_eq!(
                ends, 1,
                "flush must materialize the accepted prefix before resetting the epoch"
            );
            assert!(stream.whisper_error.is_none());
            assert!(
                stream.poll_event().is_some(),
                "the notice remains queued after PCM-only polling"
            );
            stream.stop().expect("complete shutdown");
            assert!(stream.poll_chunk().expect("final empty poll").is_none());
        });
    }

    #[test]
    fn stop_before_startup_notice_preserves_pcm_and_finalized_events() {
        Python::initialize();
        Python::attach(|py| {
            for whisper in [true, false] {
                let mut stream = startup_fixture(whisper);
                stream.stop().expect("complete synthetic shutdown");
                assert!(stream
                    .shutdown_report()
                    .expect("retained report")
                    .0
                    .cleanup()
                    .is_empty());
                let mut samples = 0;
                let mut event_types = Vec::new();
                while let Some(chunk) = stream.poll_chunk().expect("retained output") {
                    samples += chunk.samples().len();
                    let object = Py::new(py, chunk).expect("chunk object");
                    let events = object
                        .bind(py)
                        .getattr(if whisper {
                            "whisper_vad_events"
                        } else {
                            "vad_events"
                        })
                        .expect("attached events");
                    if !events.is_none() {
                        for event in events.try_iter().expect("events iterable") {
                            event_types.push(
                                event
                                    .expect("event")
                                    .getattr("type")
                                    .expect("event type")
                                    .extract::<String>()
                                    .expect("type string"),
                            );
                        }
                    }
                }
                assert_eq!(
                    samples, 3840,
                    "stop must process every accepted PCM frame before sealing the addons"
                );
                if whisper {
                    assert_eq!(
                        event_types
                            .iter()
                            .filter(|kind| *kind == "epoch_start")
                            .count(),
                        1
                    );
                    assert_eq!(
                        event_types
                            .iter()
                            .filter(|kind| *kind == "epoch_end")
                            .count(),
                        1,
                        "stop must retain the closing epoch while its startup report is pending"
                    );
                    assert!(
                        stream.whisper_error.is_none(),
                        "post-stop polling must not feed an already stopped tap"
                    );
                } else {
                    assert_eq!(
                        event_types
                            .iter()
                            .filter(|kind| *kind == "speech_end")
                            .count(),
                        1,
                        "legacy VAD must flush the accepted prefix at stop"
                    );
                }
                assert!(stream
                    .shutdown_report()
                    .expect("retained report")
                    .0
                    .cleanup()
                    .is_empty());
                let report = stream
                    .poll_event()
                    .expect("startup report remains queued after PCM-only polling")
                    .to_dict(py)
                    .expect("report object");
                assert_eq!(
                    report
                        .get_item("type")
                        .expect("type lookup")
                        .expect("type")
                        .extract::<String>()
                        .expect("event type"),
                    "startupSamplesDropped"
                );
                stream.stop().expect("idempotent stop");
                assert!(stream.poll_chunk().expect("empty poll").is_none());
            }
        });
    }

    #[test]
    fn cleanup_only_failure_is_retained_and_context_body_remains_primary() {
        Python::initialize();
        Python::attach(|py| {
            let mut stream = fixture();
            assert!(stream.shutdown_report().is_none());
            let first = stream.stop().unwrap_err();
            let second = stream.stop().unwrap_err();
            assert!(first.get_type(py).is(second.get_type(py)));
            let report = stream.shutdown_report().unwrap().0;
            assert!(report.primary().is_none());
            assert_eq!(report.cleanup().len(), 1);
            assert_eq!(report.cleanup()[0].kind(), fa::ErrorKind::DeviceLost);
            assert!(stream.terminal_error().is_none());
            assert_eq!(
                stream
                    .poll_event()
                    .unwrap()
                    .to_dict(py)
                    .unwrap()
                    .get_item("type")
                    .unwrap()
                    .unwrap()
                    .extract::<String>()
                    .unwrap(),
                "shutdownError"
            );
            assert!(stream.poll_event().is_none());
            let locals = PyDict::new(py);
            locals
                .set_item("stream", Py::new(py, fixture()).unwrap())
                .unwrap();
            py.run(
                c_str!(
                    r#"
try:
    with stream:
        raise KeyError('body')
except KeyError as body:
    assert str(body) == "'body'"
    assert body.__context__.audio_error.kind == 'deviceLost'
else:
    raise AssertionError('body error was lost')
"#
                ),
                None,
                Some(&locals),
            )
            .unwrap();
            locals
                .set_item("stream", Py::new(py, fixture()).unwrap())
                .unwrap();
            py.run(
                c_str!(
                    r#"
try:
    with stream:
        pass
except RuntimeError as cleanup:
    assert cleanup.audio_error.kind == 'deviceLost'
else:
    raise AssertionError('normal context exit hid cleanup failure')
"#
                ),
                None,
                Some(&locals),
            )
            .unwrap();
        });
    }
}
