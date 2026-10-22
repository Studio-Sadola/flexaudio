//! Serialized canonical capture intake and ordered polling carriers.
use super::*;

impl Stream {
    pub(super) fn flush_vad_tap_with(
        &mut self,
        flush: impl FnOnce(
            &mut flexaudio_vad::VadTap,
        )
            -> Result<Vec<flexaudio_vad::AttachedVadEvent>, flexaudio_vad::VadTapFailure>,
    ) -> PyResult<()> {
        if self.vad_tap.is_none() {
            return Ok(());
        }
        self.drain_capture();
        let drained = self.drain_output();
        let result = flush(self.vad_tap.as_mut().expect("enabled attachment"));
        let flushed = result
            .as_ref()
            .map(|_| ())
            .map_err(|failure| crate::vad::tap_error(failure.error.clone()));
        self.accept_vad_tap(result, fa::Operation::Flush);
        self.vad_carrier();
        drained.and(flushed)
    }

    pub(super) fn accept_vad_tap(
        &mut self,
        result: Result<Vec<flexaudio_vad::AttachedVadEvent>, flexaudio_vad::VadTapFailure>,
        operation: fa::Operation,
    ) {
        match result {
            Ok(events) => {
                // Empty output is not proof of recovery. The shared owner's lifecycle
                // is authoritative, including while normalization produces no event.
                if matches!(operation, fa::Operation::Normalize | fa::Operation::Flush)
                    && self
                        .vad_tap
                        .as_ref()
                        .is_some_and(|owner| !owner.is_failed())
                {
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

    pub(super) fn drain_capture(&mut self) {
        if self.vad_tap.is_none() {
            return;
        }
        while let Some(chunk) = self.inner.poll_capture() {
            let discontinuity = chunk.flags.contains(fa::ChunkFlags::DISCONTINUITY);
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
            self.accept_vad_tap(result, fa::Operation::Normalize);
        }
    }

    pub(super) fn vad_carrier(&mut self) {
        if self.vad_events.is_empty() {
            return;
        }
        if self.vad_secondary {
            self.secondary_vad_carrier();
            return;
        }
        let mut chunk = chunk_to_py({
            let mut chunk_value = fa::AudioChunk::new(Vec::new(), 0);
            chunk_value.frame_index = self.vad_origin.0;
            chunk_value.pts_ns = self.vad_origin.1;
            chunk_value.seq = 0;
            chunk_value.flags = fa::ChunkFlags::empty();
            chunk_value.dropped_before = 0;
            chunk_value.peak = 0.0;
            chunk_value.rms = 0.0;
            chunk_value
        });
        chunk.set_vad_events(std::mem::take(&mut self.vad_events));
        self.ready_chunks.push_back(chunk);
    }

    pub(super) fn drain_output(&mut self) -> PyResult<()> {
        // A source switch/explicit flush can run while capture still produces PCM.
        // Drain at most one queue capacity so producer refills cannot prolong the
        // control operation indefinitely. Stopped capture has no more than this.
        for _ in 0..self.inner.config().ring_capacity_chunks {
            let Some(chunk) = self.poll_processed_chunk()? else {
                break;
            };
            self.ready_chunks.push_back(chunk);
        }
        self.drain_secondary();
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

    /// PCM metadata for one delivered primary chunk. Event-only carriers are not audio.
    struct DeliveredPcm {
        frame_index: u64,
        frames: u64,
        pts_ns: i64,
        flags: u32,
    }

    fn delivered_pcm(py: Python<'_>, chunk: &Py<PyAudioChunk>) -> Option<DeliveredPcm> {
        let bound = chunk.bind(py);
        let frames: usize = bound
            .getattr("frames")
            .expect("chunk frames")
            .extract()
            .expect("integer frames");
        if frames == 0 {
            return None;
        }
        Some(DeliveredPcm {
            frame_index: bound
                .getattr("frame_index")
                .expect("chunk frame index")
                .extract()
                .expect("integer frame index"),
            frames: frames as u64,
            pts_ns: bound
                .getattr("pts_ns")
                .expect("chunk PTS")
                .extract()
                .expect("integer PTS"),
            flags: bound
                .getattr("flags")
                .expect("chunk flags")
                .extract()
                .expect("integer flags"),
        })
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

    fn has_error_epoch_end(events: &pyo3::Bound<'_, pyo3::PyAny>) -> bool {
        if events.is_none() {
            return false;
        }
        events
            .try_iter()
            .map(|iter| {
                iter.flatten().any(|event| {
                    let kind = event
                        .getattr("type")
                        .and_then(|kind| kind.extract::<String>())
                        .ok();
                    let reason = event
                        .getattr("reason")
                        .and_then(|reason| reason.extract::<String>())
                        .ok();
                    kind.as_deref() == Some("epoch_end") && reason.as_deref() == Some("error")
                })
            })
            .unwrap_or(false)
    }

    /// Capture publication can precede the matching primary chunk. Keep polling until
    /// both the requested event and that push's PCM have been observed; a zero-frame
    /// carrier is not evidence that the PCM was delivered.
    fn consume_marked_pcm(
        py: Python<'_>,
        stream: &mut Stream,
        marker: impl Fn(&pyo3::Bound<'_, pyo3::PyAny>) -> bool,
    ) -> DeliveredPcm {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut marked = false;
        let mut audio = None;
        loop {
            assert!(
                Instant::now() < deadline,
                "fake capture must deliver the marked PCM"
            );
            match stream.poll_chunk().unwrap() {
                Some(chunk) => {
                    let wrapped = Py::new(py, chunk).unwrap();
                    let bound = wrapped.bind(py);
                    marked |= marker(&bound.getattr("vad_events").unwrap());
                    if audio.is_none() {
                        audio = delivered_pcm(py, &wrapped);
                    }
                    if marked {
                        if let Some(delivered) = audio.take() {
                            return delivered;
                        }
                    }
                }
                None => std::thread::sleep(Duration::from_millis(2)),
            }
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
                    if has_epoch_start(&chunk.bind(py).getattr("vad_events").unwrap()) {
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
            AttachedVadEvent, EpochEndReason, VadEvent, VadEventKind, VadTapError, VadTapFailure,
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
                lifecycle_events: Default::default(),
                capture_events: Default::default(),
                settings: Default::default(),
                vad_tap: Some(
                    flexaudio_vad::VadTap::new(Default::default(), Default::default()).unwrap(),
                ),
                vad_events: Vec::new(),
                vad_origin: (0, 0),
                vad_error: None,
                vad_cleanup: Vec::new(),
                secondary: None,
                ready_secondary: Default::default(),
                vad_secondary: false,
                secondary_metadata: (0, 0, 0, 0),
                ready_chunks: Default::default(),
            };
            for (error, expected) in [
                (VadTapError::Conversion, "Conversion"),
                (
                    VadTapError::Vad(flexaudio_vad::VadError::Inference),
                    "Inference",
                ),
                (
                    VadTapError::UnsupportedConversionClock,
                    "UnsupportedConversionClock",
                ),
                (
                    VadTapError::Vad(flexaudio_vad::VadError::SessionFinished),
                    "SessionFinished",
                ),
            ] {
                let error = stream
                    .flush_vad_tap_with(|_| {
                        Err(VadTapFailure {
                            error,
                            terminal_events: vec![AttachedVadEvent::Vad(VadEvent {
                                epoch: 0,
                                seq: 1,
                                kind: VadEventKind::EpochEnd {
                                    reason: EpochEndReason::Error,
                                },
                            })],
                        })
                    })
                    .unwrap_err();
                assert!(error.is_instance_of::<crate::vad::VadRuntimeError>(py));
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
                let events = carrier.bind(py).getattr("vad_events").unwrap();
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
                stream.flush_vad_tap_with(|_| Ok(Vec::new())).unwrap();
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
            lifecycle_events: Default::default(),
            capture_events: Default::default(),
            settings: Default::default(),
            vad_tap: Some(
                flexaudio_vad::VadTap::new(Default::default(), Default::default()).unwrap(),
            ),
            vad_events: Vec::new(),
            vad_origin: (0, 0),
            vad_error: None,
            vad_cleanup: Vec::new(),
            secondary: None,
            ready_secondary: Default::default(),
            vad_secondary: false,
            secondary_metadata: (0, 0, 0, 0),
            ready_chunks: Default::default(),
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
        let first = consume_marked_pcm(py, stream, has_epoch_start);
        let first_delivery_ns = fa::core::clock::monotonic_now_ns();
        // 1920 interleaved stereo samples are one 20 ms chunk. Later pushes continue
        // this measured timeline; they must not be identified by a fixed sleep.
        assert_eq!(first.frame_index, 0);
        assert_eq!(first.frames, 960);
        stream.vad_tap.as_mut().unwrap().inject_failure();
        sink.lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .push(&[0.2; 1920], 20_000_000);
        let second = consume_marked_pcm(py, stream, has_error_epoch_end);
        assert_eq!(
            second.frame_index,
            first.frame_index + first.frames,
            "the failure push must continue the first chunk before another push is issued"
        );
        assert_eq!(second.frames, 960);
        assert_eq!(
            stream.vad_error.as_ref().unwrap().audio_error().vad_code(),
            Some(fa::VadErrorCode::Inference)
        );
        assert_eq!(
            stream.vad_cleanup.len(),
            1,
            "one real processor failure must retain exactly one cleanup cause"
        );
        // This empty poll used to raise the retained VAD error and end capture loops.
        // It is only empty after the failure push's PCM has been consumed; a single
        // poll beside an event carrier still races the primary chunk under load.
        assert!(stream.poll_chunk().unwrap().is_none());
        std::thread::sleep(idle);
        let third_push_ns = fa::core::clock::monotonic_now_ns();
        sink.lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .push(&[0.2; 1920], 40_000_000);
        let third = consume_marked_pcm(py, stream, |_| true);
        let third_delivery_ns = fa::core::clock::monotonic_now_ns();
        let end = second.frame_index + second.frames;
        assert_eq!(
            end, 1920,
            "two delivered 20 ms chunks precede the idle push"
        );
        assert_eq!(
            third.frame_index, end,
            "idle time must not skip or reset the frame timeline"
        );
        assert_eq!(third.frames, 960);
        let flags = third.flags;
        let pts_ns = third.pts_ns;
        assert_eq!(flags & (fa::ChunkFlags::DISCONTINUITY | fa::ChunkFlags::RECOVERED).bits(), 0,
            "the contiguous fixture must not recover while its producer is idle; flags={flags:#x}, pts_ns={pts_ns}, retained_failure={}", stream.vad_error.is_some());
        // Public PCM PTS follows capture arrival time after startup confirmation. The lease
        // keeps frame indices and VAD canonical epoch contiguous, but it does not
        // compact elapsed idle time. Bound the two intake arrivals by their real push/delivery
        // observations, retaining the 40 ms startup sample-position floor for rapid pushes.
        let earliest_pts = (third_push_ns - first_delivery_ns).max(40_000_000);
        let latest_pts = (third_delivery_ns - first_push_ns).max(40_000_000);
        assert!(
            (earliest_pts..=latest_pts).contains(&pts_ns),
            "third PCM PTS must preserve arrival time within observed publication bounds {earliest_pts}..={latest_pts} ns: pts_ns={pts_ns}, flags={flags:#x}"
        );
        assert!(stream.vad_error.is_some());
    }

    fn assert_vad_tap_shutdown(py: Python<'_>, stream: &mut Stream) {
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
                .get_item("vad_code")
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
            let owner = stream.vad_tap.as_mut().expect("enabled shared owner");
            owner.inject_failure();
            let failure = owner.process(&[0.2; 1920], 0, 0, false);
            assert!(
                failure.is_err(),
                "fault injection must latch the real owner"
            );
            stream.accept_vad_tap(failure, fa::Operation::Normalize);
            assert!(stream.vad_error.is_some());
            // An empty callback result is not proof that the authoritative session recovered.
            // Inject it at the existing result seam while the shared owner remains failed.
            stream.accept_vad_tap(Ok(Vec::new()), fa::Operation::Normalize);
            assert!(
                stream.vad_error.is_some(),
                "an empty success must not erase a latched shared-owner failure"
            );
            assert_eq!(
                stream.vad_cleanup.len(),
                1,
                "retain the original typed cause once"
            );
            assert_eq!(
                stream.vad_cleanup[0].vad_code(),
                Some(fa::VadErrorCode::Inference)
            );
            // Explicit successful recovery can clear the active failure while retaining cleanup.
            stream.flush_vad().expect("shared-owner recovery flush");
            assert!(stream.vad_error.is_none());
            assert_eq!(stream.vad_cleanup.len(), 1);
            stream
                .stop()
                .expect("isolated attachment cleanup does not reject capture stop");
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
            assert_vad_tap_shutdown(py, &mut stream);
        });
    }

    #[test]
    fn vad_tap_failure_preserves_python_polling_and_stop_with_typed_cleanup() {
        Python::initialize();
        Python::attach(|py| {
            let sink = Arc::new(Mutex::new(None));
            let fenced = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let mut stream = capture_stream(&sink, &fenced);
            fail_attachment(py, &mut stream, &sink, &fenced);
            assert_vad_tap_shutdown(py, &mut stream);
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
            stream.flush_vad()?;
            assert!(
                stream.vad_error.is_none(),
                "successful shared flush must clear the active failure"
            );
            sink.lock()
                .expect("fake capture sink mutex must remain usable")
                .as_mut()
                .expect("fake capture backend must retain its sink until stop")
                .push(&[0.2; 1920], 60_000_000);
            wait_for_epoch_start(py, &mut stream);
            assert_vad_tap_shutdown(py, &mut stream);
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
                    stream.vad_error.is_none(),
                    "the shared owner must recover only at a discontinuity"
                );
                assert_vad_tap_shutdown(py, &mut stream);
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
            let mut stream = Stream {
                inner,
                shutdown: None,
                local_events: Default::default(),
                lifecycle_events: Default::default(),
                capture_events: Default::default(),
                settings: Default::default(),
                vad_tap: Some(vad_tap),
                vad_events: Vec::new(),
                vad_origin: (0, 0),
                vad_error: None,
                vad_cleanup: Vec::new(),
                secondary: None,
                ready_secondary: Default::default(),
                vad_secondary: false,
                secondary_metadata: (0, 0, 0, 0),
                ready_chunks: Default::default(),
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
                    let events = chunk.bind(py).getattr("vad_events").unwrap();
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
            let events = chunk.getattr("vad_events").unwrap();
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
            stream.flush_vad().unwrap();
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
            let events = carrier.bind(py).getattr("vad_events").unwrap();
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
            stream.flush_vad().unwrap();
            assert!(stream.poll_chunk().unwrap().is_none());
            sink.lock().unwrap().as_mut().unwrap().push(&[0.0; 1920], 0);
            let _ = next_chunk(&mut stream);
            stream.stop().unwrap();
            let error = stream.flush_vad().unwrap_err();
            assert!(error.is_instance_of::<crate::vad::VadRuntimeError>(py));
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
            let events = terminal.bind(py).getattr("vad_events").unwrap();
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
