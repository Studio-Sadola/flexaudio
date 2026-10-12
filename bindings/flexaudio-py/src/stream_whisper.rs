//! Serialized canonical capture intake and ordered polling carriers.
use super::*;

impl Stream {
    pub(super) fn flush_whisper_with(
        &mut self,
        flush: impl FnOnce(
            &mut flexaudio_vad::WhisperVadTap,
        ) -> Result<
            Vec<flexaudio_vad::AttachedWhisperVadEvent>,
            flexaudio_vad::WhisperVadTapFailure,
        >,
    ) -> PyResult<()> {
        if self.whisper.is_none() {
            return Ok(());
        }
        self.drain_capture();
        let drained = self.drain_output();
        let result = flush(self.whisper.as_mut().expect("enabled attachment"));
        let flushed = result
            .as_ref()
            .map(|_| ())
            .map_err(|failure| crate::whisper_vad::tap_error(failure.error.clone()));
        self.accept_whisper(result, fa::Operation::Flush);
        self.whisper_carrier();
        drained.and(flushed)
    }

    pub(super) fn accept_whisper(
        &mut self,
        result: Result<
            Vec<flexaudio_vad::AttachedWhisperVadEvent>,
            flexaudio_vad::WhisperVadTapFailure,
        >,
        operation: fa::Operation,
    ) {
        match result {
            Ok(events) => {
                // Empty output is not proof of recovery. The shared owner's lifecycle
                // is authoritative, including while normalization produces no event.
                if matches!(operation, fa::Operation::Normalize | fa::Operation::Flush)
                    && self
                        .whisper
                        .as_ref()
                        .is_some_and(|owner| !owner.is_failed())
                {
                    self.whisper_error = None;
                }
                self.whisper_events.extend(events);
            }
            Err(failure) => {
                self.whisper_events.extend(failure.terminal_events);
                if self.whisper_error.is_none() {
                    self.whisper_cleanup.push(failure.error.audio_error());
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
            let discontinuity = chunk.flags.contains(fa::ChunkFlags::DISCONTINUITY);
            let result = self.whisper.as_mut().expect("enabled attachment").process(
                &chunk.data,
                chunk.frame_index,
                chunk.pts_ns,
                discontinuity,
            );
            if result.is_ok() {
                self.whisper_origin = (
                    chunk.frame_index + chunk.frames as u64,
                    chunk.pts_ns + (chunk.frames as i64 * 1_000_000_000 / 48_000),
                );
            }
            self.accept_whisper(result, fa::Operation::Normalize);
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
        while let Some(chunk) = self.poll_chunk_with_steps(CoreVad::flush, CoreVad::process_pcm)? {
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

    /// Keep the manual producer's generation stable without holding a lock that
    /// capture delivery also needs while the watchdog reconciles a backend stop.
    struct ContiguousBackend {
        sink: Arc<Mutex<Option<fa::core::backend::RawSink>>>,
        fenced: Arc<std::sync::atomic::AtomicBool>,
    }
    impl fa::CaptureBackend for ContiguousBackend {
        fn native_format(&self) -> (u32, u16) {
            (48_000, 2)
        }
        fn start(&mut self, sink: fa::core::backend::RawSink) -> fa::Result<()> {
            let mut current = self.sink.lock().expect("fake producer ownership lock");
            if self.fenced.load(std::sync::atomic::Ordering::SeqCst) && current.is_some() {
                return Err(fa::Error::Backend(
                    "The contiguous test generation is leased; release the fixture lease before reopening.".into(),
                ));
            }
            *current = Some(sink);
            Ok(())
        }
        fn stop(&mut self) {
            let mut current = self.sink.lock().expect("fake producer ownership lock");
            if !self.fenced.load(std::sync::atomic::Ordering::SeqCst) {
                current.take();
            }
        }
    }
    struct GenerationLease(Arc<std::sync::atomic::AtomicBool>);
    impl Drop for GenerationLease {
        fn drop(&mut self) {
            self.0.store(false, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// Whether one attached carrier publishes an epoch origin.
    fn has_epoch_start(events: &pyo3::Bound<'_, pyo3::PyAny>) -> bool {
        if events.is_none() {
            return false;
        }
        events
            .try_iter()
            .map(|iter| {
                iter.flatten().any(|event| {
                    event
                        .getattr("type")
                        .and_then(|kind| kind.extract::<String>())
                        .is_ok_and(|kind| kind == "epoch_start")
                })
            })
            .unwrap_or(false)
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

    /// Poll until a real PCM chunk is delivered (attached carriers carry `frames` 0).
    fn next_audio_chunk(py: Python<'_>, stream: &mut Stream) -> Py<PyAudioChunk> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(chunk) = stream.poll_chunk().unwrap() {
                let wrapped = Py::new(py, chunk).unwrap();
                let frames = wrapped
                    .bind(py)
                    .getattr("frames")
                    .unwrap()
                    .extract::<usize>()
                    .unwrap();
                if frames > 0 {
                    return wrapped;
                }
            }
            assert!(Instant::now() < deadline, "fake capture timed out");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Poll until one attached carrier publishes an epoch origin.
    fn wait_for_epoch_start(py: Python<'_>, stream: &mut Stream) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            assert!(
                Instant::now() < deadline,
                "the discontinuity boundary must start a new epoch"
            );
            match stream.poll_chunk().unwrap() {
                Some(chunk) => {
                    let chunk = Py::new(py, chunk).unwrap();
                    if has_epoch_start(&chunk.bind(py).getattr("whisper_vad_events").unwrap()) {
                        return;
                    }
                }
                None => std::thread::sleep(Duration::from_millis(2)),
            }
        }
    }

    #[test]
    fn flush_failures_raise_typed_errors_and_keep_terminal_carriers() {
        use flexaudio_vad::{
            AttachedWhisperVadEvent, EpochEndReason, WhisperVadEvent, WhisperVadEventKind,
            WhisperVadTapError, WhisperVadTapFailure,
        };
        Python::initialize();
        Python::attach(|py| {
            let mut stream = Stream {
                inner: fa::Stream::open(
                    Default::default(),
                    Box::new(PushBackend(Arc::new(Mutex::new(None)))),
                )
                .unwrap(),
                shutdown: None,
                local_events: Default::default(),
                output_end: None,
                whisper: Some(
                    flexaudio_vad::WhisperVadTap::new(Default::default(), Default::default())
                        .unwrap(),
                ),
                whisper_events: Vec::new(),
                whisper_origin: (0, 0),
                whisper_error: None,
                whisper_cleanup: Vec::new(),
                ready_chunks: Default::default(),
                vad: None,
                denoiser: None,
                output_rate: 48_000,
                output_channels: 2,
            };
            for (error, expected) in [
                (WhisperVadTapError::Conversion, "Conversion"),
                (
                    WhisperVadTapError::Vad(flexaudio_vad::WhisperVadError::Inference),
                    "Inference",
                ),
                (
                    WhisperVadTapError::UnsupportedConversionClock,
                    "UnsupportedConversionClock",
                ),
                (
                    WhisperVadTapError::Vad(flexaudio_vad::WhisperVadError::SessionFinished),
                    "SessionFinished",
                ),
            ] {
                let error = stream
                    .flush_whisper_with(|_| {
                        Err(WhisperVadTapFailure {
                            error,
                            terminal_events: vec![AttachedWhisperVadEvent::Vad(WhisperVadEvent {
                                epoch: 0,
                                seq: 1,
                                kind: WhisperVadEventKind::EpochEnd {
                                    reason: EpochEndReason::Error,
                                },
                            })],
                        })
                    })
                    .unwrap_err();
                assert!(error.is_instance_of::<crate::whisper_vad::WhisperVadRuntimeError>(py));
                assert_eq!(
                    error
                        .value(py)
                        .getattr("code")
                        .unwrap()
                        .extract::<String>()
                        .unwrap(),
                    expected
                );
                let carrier = Py::new(py, stream.poll_chunk().unwrap().unwrap()).unwrap();
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
                assert_eq!(events.len().unwrap(), 1);
                assert_eq!(
                    events
                        .get_item(0)
                        .unwrap()
                        .getattr("type")
                        .unwrap()
                        .extract::<String>()
                        .unwrap(),
                    "epoch_end"
                );
                assert!(stream.poll_chunk().unwrap().is_none());
                stream.flush_whisper_with(|_| Ok(Vec::new())).unwrap();
            }
            stream.stop().unwrap();
            while stream.poll_chunk().unwrap().is_some() {}
        });
    }

    fn capture_stream(
        sink: &Arc<Mutex<Option<fa::core::backend::RawSink>>>,
        fenced: &Arc<std::sync::atomic::AtomicBool>,
    ) -> Stream {
        let mut inner = fa::Stream::open(
            fa::StreamConfig::default(),
            Box::new(ContiguousBackend {
                sink: sink.clone(),
                fenced: fenced.clone(),
            }),
        )
        .unwrap();
        inner.enable_capture_tap().unwrap();
        inner.start().unwrap();
        Stream {
            inner,
            shutdown: None,
            local_events: Default::default(),
            output_end: None,
            whisper: Some(
                flexaudio_vad::WhisperVadTap::new(Default::default(), Default::default()).unwrap(),
            ),
            whisper_events: Vec::new(),
            whisper_origin: (0, 0),
            whisper_error: None,
            whisper_cleanup: Vec::new(),
            ready_chunks: Default::default(),
            vad: None,
            denoiser: None,
            output_rate: 48_000,
            output_channels: 2,
        }
    }

    fn fail_attachment(
        py: Python<'_>,
        stream: &mut Stream,
        sink: &Arc<Mutex<Option<fa::core::backend::RawSink>>>,
        fenced: &Arc<std::sync::atomic::AtomicBool>,
    ) {
        fail_attachment_with_delay(py, stream, sink, fenced, Duration::ZERO);
    }

    fn fail_attachment_with_delay(
        py: Python<'_>,
        stream: &mut Stream,
        sink: &Arc<Mutex<Option<fa::core::backend::RawSink>>>,
        fenced: &Arc<std::sync::atomic::AtomicBool>,
        idle: Duration,
    ) {
        // Model one contiguous generation regardless of processor startup or gate load.
        // Drop the lease before the caller intentionally pauses, switches, or stops.
        let deadline = Instant::now() + Duration::from_secs(5);
        let _lease = loop {
            let current = sink.lock().expect("fake producer ownership lock");
            if current.is_some() {
                // Lease acquisition and backend stop check share the ownership lock.
                // Release it before polling so delivery can reconcile watchdog attempts.
                fenced.store(true, std::sync::atomic::Ordering::SeqCst);
                break GenerationLease(fenced.clone());
            }
            drop(current);
            assert!(
                Instant::now() < deadline,
                "fake producer must become available for the contiguous generation lease"
            );
            std::thread::sleep(Duration::from_millis(2));
        };
        let first_push_ns = fa::core::clock::monotonic_now_ns();
        sink.lock().unwrap().as_mut().unwrap().push(&[0.2; 1920], 0);
        wait_for_epoch_start(py, stream);
        let first_delivery_ns = fa::core::clock::monotonic_now_ns();
        stream.whisper.as_mut().unwrap().inject_failure();
        sink.lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .push(&[0.2; 1920], 20_000_000);
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut ended = false;
        while !ended {
            assert!(
                Instant::now() < deadline,
                "injected failure must deliver its terminal batch"
            );
            if let Some(chunk) = stream.poll_chunk().unwrap() {
                let chunk = Py::new(py, chunk).unwrap();
                let events = chunk.bind(py).getattr("whisper_vad_events").unwrap();
                if !events.is_none() {
                    for event in events.try_iter().unwrap() {
                        let event = event.unwrap();
                        if event.getattr("type").unwrap().extract::<String>().unwrap()
                            == "epoch_end"
                        {
                            assert_eq!(
                                event
                                    .getattr("reason")
                                    .unwrap()
                                    .extract::<String>()
                                    .unwrap(),
                                "error"
                            );
                            ended = true;
                        }
                    }
                }
            } else {
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        assert_eq!(
            stream
                .whisper_error
                .as_ref()
                .unwrap()
                .audio_error()
                .whisper_code(),
            Some(fa::WhisperCode::Inference)
        );
        assert_eq!(
            stream.whisper_cleanup.len(),
            1,
            "one real processor failure must retain exactly one cleanup cause"
        );
        // This empty poll used to raise the retained Whisper error and end capture loops.
        assert!(stream.poll_chunk().unwrap().is_none());
        std::thread::sleep(idle);
        let third_push_ns = fa::core::clock::monotonic_now_ns();
        sink.lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .push(&[0.2; 1920], 40_000_000);
        let chunk = next_audio_chunk(py, stream);
        let third_delivery_ns = fa::core::clock::monotonic_now_ns();
        assert_eq!(
            chunk
                .bind(py)
                .getattr("frame_index")
                .unwrap()
                .extract::<u64>()
                .unwrap(),
            1920
        );
        let flags: u32 = chunk
            .bind(py)
            .getattr("flags")
            .expect("capture flags")
            .extract()
            .expect("integer flags");
        let pts_ns: i64 = chunk
            .bind(py)
            .getattr("pts_ns")
            .expect("capture PTS")
            .extract()
            .expect("integer PTS");
        assert_eq!(flags & (fa::ChunkFlags::DISCONTINUITY | fa::ChunkFlags::RECOVERED).bits(), 0,
            "the contiguous fixture must not recover while its producer is idle; flags={flags:#x}, pts_ns={pts_ns}, retained_failure={}", stream.whisper_error.is_some());
        // Public PCM PTS follows capture arrival time after startup confirmation. The lease
        // keeps frame indices and Whisper's canonical epoch contiguous, but it does not
        // compact elapsed idle time. Bound the two intake arrivals by their real push/delivery
        // observations, retaining the 40 ms startup sample-position floor for rapid pushes.
        let earliest_pts = (third_push_ns - first_delivery_ns).max(40_000_000);
        let latest_pts = (third_delivery_ns - first_push_ns).max(40_000_000);
        assert!(
            (earliest_pts..=latest_pts).contains(&pts_ns),
            "third PCM PTS must preserve arrival time within observed publication bounds {earliest_pts}..={latest_pts} ns: pts_ns={pts_ns}, flags={flags:#x}"
        );
        assert!(stream.whisper_error.is_some());
    }

    fn assert_whisper_shutdown(py: Python<'_>, stream: &mut Stream) {
        stream.stop().unwrap();
        stream.stop().unwrap();
        assert!(stream.terminal_error().is_none());
        let report = Py::new(py, stream.shutdown_report().unwrap()).unwrap();
        let report = report.bind(py).call_method0("to_dict").unwrap();
        assert!(report.get_item("primary").unwrap().is_none());
        let cleanup = report.get_item("cleanup_errors").unwrap();
        assert_eq!(
            cleanup.len().unwrap(),
            1,
            "a processor failure must survive recovery exactly once"
        );
        assert_eq!(
            cleanup
                .get_item(0)
                .unwrap()
                .get_item("whisper_code")
                .unwrap()
                .extract::<String>()
                .unwrap(),
            "Inference"
        );
        // Consume every closing batch instead of dropping unread terminal events.
        while stream.poll_chunk().unwrap().is_some() {}
    }

    #[test]
    fn empty_success_does_not_clear_a_latched_shared_owner_failure() {
        Python::initialize();
        Python::attach(|_| {
            let sink = Arc::new(Mutex::new(None));
            let fenced = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let mut stream = capture_stream(&sink, &fenced);
            let owner = stream.whisper.as_mut().expect("enabled shared owner");
            owner.inject_failure();
            let failure = owner.process(&[0.2; 1920], 0, 0, false);
            assert!(
                failure.is_err(),
                "fault injection must latch the real owner"
            );
            stream.accept_whisper(failure, fa::Operation::Normalize);
            assert!(stream.whisper_error.is_some());
            // An empty callback result is not proof that the authoritative session recovered.
            // Inject it at the existing result seam while the shared owner remains failed.
            stream.accept_whisper(Ok(Vec::new()), fa::Operation::Normalize);
            assert!(
                stream.whisper_error.is_some(),
                "an empty success must not erase a latched shared-owner failure"
            );
            assert_eq!(
                stream.whisper_cleanup.len(),
                1,
                "retain the original typed cause once"
            );
            assert_eq!(
                stream.whisper_cleanup[0].whisper_code(),
                Some(fa::WhisperCode::Inference)
            );
            // Explicit successful recovery can clear the active failure while retaining cleanup.
            stream
                .flush_whisper_vad()
                .expect("shared-owner recovery flush");
            assert!(stream.whisper_error.is_none());
            assert_eq!(stream.whisper_cleanup.len(), 1);
            let _ = stream.stop();
        });
    }

    #[test]
    fn contiguous_failed_attachment_survives_fixture_idle_above_recovery_timeout() {
        Python::initialize();
        Python::attach(|py| {
            let sink = Arc::new(Mutex::new(None));
            let fenced = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let mut stream = capture_stream(&sink, &fenced);
            fail_attachment_with_delay(
                py,
                &mut stream,
                &sink,
                &fenced,
                Duration::from_millis(2_750),
            );
            assert_whisper_shutdown(py, &mut stream);
        });
    }

    #[test]
    fn whisper_failure_preserves_python_polling_and_stop_with_typed_cleanup() {
        Python::initialize();
        Python::attach(|py| {
            let sink = Arc::new(Mutex::new(None));
            let fenced = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let mut stream = capture_stream(&sink, &fenced);
            fail_attachment(py, &mut stream, &sink, &fenced);
            assert_whisper_shutdown(py, &mut stream);
        });
    }

    #[test]
    fn successful_flush_reinitializes_shared_owner_after_process_failure() -> PyResult<()> {
        Python::initialize();
        Python::attach(|py| {
            let sink = Arc::new(Mutex::new(None));
            let fenced = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let mut stream = capture_stream(&sink, &fenced);
            fail_attachment(py, &mut stream, &sink, &fenced);
            stream.flush_whisper_vad()?;
            assert!(
                stream.whisper_error.is_none(),
                "successful shared flush must clear the active failure"
            );
            sink.lock()
                .expect("fake capture sink mutex must remain usable")
                .as_mut()
                .expect("fake capture backend must retain its sink until stop")
                .push(&[0.2; 1920], 60_000_000);
            wait_for_epoch_start(py, &mut stream);
            assert_whisper_shutdown(py, &mut stream);
            Ok(())
        })
    }

    #[test]
    fn a_failed_attachment_reinitializes_at_the_next_discontinuity() {
        Python::initialize();
        Python::attach(|py| {
            for switch in [false, true] {
                let sink = Arc::new(Mutex::new(None));
                let fenced = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let mut stream = capture_stream(&sink, &fenced);
                fail_attachment(py, &mut stream, &sink, &fenced);
                if switch {
                    stream
                        .inner
                        .switch_backend(Box::new(PushBackend(sink.clone())))
                        .unwrap();
                } else {
                    stream.pause();
                    stream.resume().unwrap();
                }
                sink.lock()
                    .unwrap()
                    .as_mut()
                    .unwrap()
                    .push(&[0.2; 1920], 60_000_000);
                wait_for_epoch_start(py, &mut stream);
                assert!(
                    stream.whisper_error.is_none(),
                    "the shared owner must recover only at a discontinuity"
                );
                assert_whisper_shutdown(py, &mut stream);
            }
        });
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
                shutdown: None,
                local_events: Default::default(),
                output_end: None,
                whisper: Some(whisper),
                whisper_events: Vec::new(),
                whisper_origin: (0, 0),
                whisper_error: None,
                whisper_cleanup: Vec::new(),
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
            let error = stream.flush_whisper_vad().unwrap_err();
            assert!(error.is_instance_of::<crate::whisper_vad::WhisperVadRuntimeError>(py));
            assert_eq!(
                error
                    .value(py)
                    .getattr("code")
                    .unwrap()
                    .extract::<String>()
                    .unwrap(),
                "Stopped"
            );
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
