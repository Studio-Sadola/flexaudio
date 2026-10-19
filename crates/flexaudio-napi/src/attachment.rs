//! Canonical capture intake, selected carriers, and ordered flush settlement.
use super::*;
use flexaudio_vad::{
    AttachedVadEvent, VadOptions, VadRuntimeOptions, VadTap as CaptureVad, VadTapError,
    VadTapFailure,
};

pub(super) struct VadBridge {
    owner: CaptureVad,
    pub tap: VadTap,
    events: Vec<AttachedVadEvent>,
    pub error: Option<VadTapError>,
    failures: Vec<shutdown::VadFailure>,
    notifications: VecDeque<(shutdown::VadFailure, u32)>,
    /// Latest epoch published on the selected tap, for the failure notification.
    pub last_epoch: u32,
    last_frame_index: u64,
    last_pts: i64,
    /// Test-only (`__openMockStream`): fail the VAD session on this 1-based capture chunk.
    #[cfg(feature = "test-faults")]
    fail_at_chunk: Option<u32>,
    /// Test-only (`__openMockStream`): make the next reinitializations fail.
    #[cfg(feature = "test-faults")]
    reset_failures: u32,
    #[cfg(feature = "test-faults")]
    capture_chunks: u32,
    #[cfg(all(flexaudio_test_addon, feature = "test-faults"))]
    inject_stop_failure: bool,
}

impl VadBridge {
    pub fn new(
        params: VadOptions,
        options: VadRuntimeOptions,
        tap: VadTap,
    ) -> Result<Self, VadTapError> {
        Ok(Self {
            owner: CaptureVad::new(params, options)?,
            tap,
            events: Vec::new(),
            error: None,
            failures: Vec::new(),
            notifications: VecDeque::new(),
            last_epoch: 0,
            last_frame_index: 0,
            last_pts: 0,
            #[cfg(feature = "test-faults")]
            fail_at_chunk: None,
            #[cfg(feature = "test-faults")]
            reset_failures: 0,
            #[cfg(feature = "test-faults")]
            capture_chunks: 0,
            #[cfg(all(flexaudio_test_addon, feature = "test-faults"))]
            inject_stop_failure: false,
        })
    }

    /// Test-only (`__openMockStream`): fail the VAD session on the given 1-based chunk and
    /// the given number of following reinitializations.
    #[cfg(feature = "test-faults")]
    pub(super) fn inject_faults(&mut self, fail_at_chunk: Option<u32>, reset_failures: u32) {
        self.fail_at_chunk = fail_at_chunk;
        self.reset_failures = reset_failures;
    }

    fn accept(
        &mut self,
        result: Result<Vec<AttachedVadEvent>, VadTapFailure>,
        stage: shutdown::VadFailureStage,
    ) {
        match result {
            Ok(events) => {
                if matches!(
                    stage,
                    shutdown::VadFailureStage::Process | shutdown::VadFailureStage::RuntimeFlush
                ) {
                    self.error = None;
                }
                for event in &events {
                    self.last_epoch = event.epoch();
                }
                self.events.extend(events);
            }
            Err(failure) => {
                for event in &failure.terminal_events {
                    self.last_epoch = event.epoch();
                }
                self.events.extend(failure.terminal_events);
                if self.error.is_none() {
                    let retained = shutdown::VadFailure {
                        error: failure.error.clone(),
                        stage,
                    };
                    self.notifications
                        .push_back((retained.clone(), self.last_epoch));
                    self.failures.push(retained);
                    self.error = Some(failure.error);
                }
            }
        }
    }

    pub(super) fn process(&mut self, chunk: AudioChunk) {
        let discontinuity = chunk.flags.contains(ChunkFlags::DISCONTINUITY);
        #[cfg(feature = "test-faults")]
        {
            self.capture_chunks = self.capture_chunks.saturating_add(1);
            let boundary = self.owner.is_discontinuity_boundary(
                chunk.frame_index,
                chunk.frames as u64,
                chunk.pts_ns,
                discontinuity,
            );
            if self.reset_failures > 0 && boundary {
                self.reset_failures -= 1;
                self.owner.inject_reset_failure();
            }
            if self.fail_at_chunk == Some(self.capture_chunks) {
                self.fail_at_chunk = None;
                self.owner.inject_failure();
            }
        }
        let result =
            self.owner
                .process(&chunk.data, chunk.frame_index, chunk.pts_ns, discontinuity);
        if result.is_ok() {
            self.last_frame_index = chunk.frame_index + chunk.frames as u64;
            self.last_pts = chunk.pts_ns + (chunk.frames as i64 * 1_000_000_000 / 48_000);
        }
        self.accept(result, shutdown::VadFailureStage::Process);
    }

    pub fn take_events(&mut self) -> Vec<vad::JsVadEvent> {
        vad::marshal(std::mem::take(&mut self.events))
    }

    #[cfg(any(test, flexaudio_test_addon))]
    /// Establish the real active prefix required by synthetic shutdown faults.
    /// The valid-first-frame fixture emits no startup notice. Accept its existing
    /// canonical capture before a stop can publish a primary that closes delivery.
    pub(super) fn prime_shutdown_fixture_capture(
        &mut self,
        stream: &mut flexaudio::Stream,
    ) -> flexaudio::Result<()> {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(error) = stream.terminal_error() {
                return Err(error);
            }
            if let Some(chunk) = stream.poll_capture_ordered() {
                self.process(chunk);
                return match self.error.as_ref() {
                    Some(error) => Err(error.audio_error()),
                    None => Ok(()),
                };
            }
            if std::time::Instant::now() >= deadline {
                return Err(flexaudio::Error::Backend(
                    "The shutdown fixture did not publish accepted canonical capture within 5 seconds; repair the synthetic producer before testing finalization.".into(),
                ));
            }
            thread::sleep(Duration::from_millis(2));
        }
    }

    #[cfg(all(flexaudio_test_addon, feature = "test-faults"))]
    pub(super) fn inject_shutdown_failure(&mut self, processing: bool) {
        self.fail_at_chunk = processing.then_some(2);
        self.inject_stop_failure = !processing;
    }
}

impl PairingBridge {
    pub(super) fn drain_capture(&mut self, stream: &mut flexaudio::Stream) {
        if let Some(attachment) = self.attachment.as_mut() {
            while let Some(chunk) = stream.poll_capture_ordered() {
                attachment.process(chunk);
            }
        }
    }

    pub(super) fn flush_attachment(&mut self, stopping: bool) {
        let Some(attachment) = self.attachment.as_mut() else {
            return;
        };
        #[cfg(all(flexaudio_test_addon, feature = "test-faults"))]
        if stopping && std::mem::take(&mut attachment.inject_stop_failure) {
            attachment.owner.inject_failure();
        }
        let result = if stopping {
            attachment.owner.stop()
        } else {
            attachment.owner.flush()
        };
        attachment.accept(
            result,
            if stopping {
                shutdown::VadFailureStage::StopFlush
            } else {
                shutdown::VadFailureStage::RuntimeFlush
            },
        );
        self.emit_attachment_carrier();
    }

    pub(super) fn retain_shutdown_outcome(&self, core: Option<flexaudio::core::ShutdownReport>) {
        let failures = self
            .attachment
            .as_ref()
            .map(|owner| owner.failures.clone())
            .unwrap_or_default();
        let mut retained = self.report.lock().unwrap_or_else(lock_poisoned);
        let mut report = shutdown::ShutdownReport::from_core(core);
        // Preserve cleanup causes recorded by bridge lifetime management.
        if let Some(previous) = retained.take() {
            report.binding_cleanup = previous.binding_cleanup;
        }
        report.attachment = failures;
        *retained = Some(report);
    }

    pub(super) fn report_attachment_failure(&mut self) {
        let Some(owner) = self.attachment.as_mut() else {
            return;
        };
        // A process failure waits for its terminal batch to ride a real PCM carrier.
        // Flush/stop explicitly emit their closing carrier before calling this method.
        if !owner.events.is_empty() {
            return;
        }
        while let Some((failure, epoch)) = owner.notifications.pop_front() {
            self.on_chunk.call(
                ChunkEmit::VadFailed {
                    error: failure.error,
                    stage: failure.stage,
                    epoch,
                },
                ThreadsafeFunctionCallMode::NonBlocking,
            );
        }
    }

    fn emit_attachment_carrier(&mut self) {
        let Some(attachment) = self.attachment.as_mut() else {
            return;
        };
        let events = attachment.take_events();
        let mut carrier = JsAudioChunk {
            data: Float32Array::new(Vec::new()),
            frames: 0,
            frame_index: BigInt::from(attachment.last_frame_index),
            pts_ns: attachment.last_pts.max(self.last_emitted_primary_pts),
            arrival_ns: attachment.last_pts.max(self.last_emitted_primary_pts),
            seq: BigInt::from(self.last_primary_seq),
            flags: 0,
            dropped_before: self.last_primary_dropped,
            peak: 0.0,
            rms: 0.0,
            vad_events: None,
            secondary: None,
        };
        match attachment.tap {
            VadTap::Primary => carrier.vad_events = Some(events),
            VadTap::Secondary => {
                let mut secondary = secondary_event_carrier(
                    self.secondary.map(|config| config.encoding),
                    carrier.pts_ns,
                    self.last_secondary,
                );
                secondary.vad_events = Some(events);
                carrier.secondary = Some(secondary);
            }
        }
        self.on_chunk.call(
            ChunkEmit::Chunk(Box::new(carrier)),
            ThreadsafeFunctionCallMode::NonBlocking,
        );
    }
}

#[cfg(feature = "test-faults")]
/// Validate the mock-only injection contract before acquiring any resources.
pub(super) fn validate_fault_options(
    attached: bool,
    fail_at_chunk: Option<f64>,
    reset_failures: Option<f64>,
) -> Result<(Option<u32>, u32), vad::BoundaryError> {
    if fail_at_chunk.is_none() && reset_failures.is_none() {
        return Ok((None, 0));
    }
    #[cfg(feature = "test-faults")]
    {
        if !attached {
            return Err(vad::BoundaryError::InvalidParameter);
        }
        fn count(value: f64, minimum: f64) -> Result<u32, vad::BoundaryError> {
            if !value.is_finite()
                || value.fract() != 0.0
                || !(minimum..=f64::from(u32::MAX)).contains(&value)
            {
                return Err(vad::BoundaryError::InvalidParameter);
            }
            // The validated integer is representable without truncation or wrap.
            Ok(value as u32)
        }
        Ok((
            fail_at_chunk.map(|value| count(value, 1.0)).transpose()?,
            reset_failures
                .map(|value| count(value, 0.0))
                .transpose()?
                .unwrap_or(0),
        ))
    }
}

/// One `vadFailed` notification for a failure of the attached VAD session only.
///
/// The typed cause is projected exactly like the retained shutdown cause (same message,
/// operation context and stable attachment code), so the event and `shutdownReport()` agree.
/// It contains no PCM, device name or transcript.
pub(super) fn failure_event(
    error: &VadTapError,
    stage: shutdown::VadFailureStage,
    epoch: u32,
) -> super::JsStreamEvent {
    super::JsStreamEvent {
        frames: None,
        reason: None,
        permission: None,
        kind: "vadFailed".to_string(),
        count: None,
        error: Some(
            shutdown::VadFailure {
                error: error.clone(),
                stage,
            }
            .audio_error(),
        ),
        loss: None,
        message: None,
        epoch: Some(epoch),
    }
}

pub(super) fn throw_error(env: &Env, error: VadTapError) -> NapiError {
    match js_error(env, &error).and_then(|exception| env.throw(exception)) {
        Ok(()) => NapiError::new(Status::PendingException, error.to_string()),
        Err(error) => error,
    }
}

pub(super) fn js_error(env: &Env, error: &VadTapError) -> napi::Result<JsObject> {
    let mut object = env.create_error(NapiError::new(Status::GenericFailure, error.to_string()))?;
    object.set_named_property("code", error_code(error))?;
    object.set_named_property(
        "audioError",
        super::boundary::audio_error(&error.audio_error()),
    )?;
    object.set_named_property("terminalEvents", env.create_array(0)?)?;
    Ok(object)
}

pub(super) fn error_code(error: &VadTapError) -> &'static str {
    error.code().as_str()
}

pub(super) fn settle_flush(
    env: &Env,
    deferred: SendDeferred,
    error: Option<VadTapError>,
) -> napi::Result<()> {
    match error {
        None => resolve_undefined(env.raw(), deferred),
        Some(error) => {
            let error = js_error(env, &error)?;
            check_status!(unsafe {
                sys::napi_reject_deferred(env.raw(), deferred.0, error.raw())
            })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shutdown_fixture_accepts_capture_before_terminal_stop() -> flexaudio::Result<()> {
        struct TerminalOnStop {
            stopped: bool,
            reported: bool,
        }
        impl flexaudio::CaptureBackend for TerminalOnStop {
            fn native_format(&self) -> (u32, u16) {
                (48_000, 2)
            }
            fn start(&mut self, mut sink: flexaudio::core::RawSink) -> flexaudio::Result<()> {
                sink.push(&[0.0; 1920], 0);
                Ok(())
            }
            fn stop(&mut self) {
                self.stopped = true;
            }
            fn stop_checked(&mut self) -> flexaudio::Result<()> {
                self.stop();
                Err(flexaudio::Error::DeviceLost)
            }
            fn poll_event(&mut self) -> Option<Event> {
                if self.stopped && !std::mem::replace(&mut self.reported, true) {
                    Some(Event::TerminalError {
                        error: flexaudio::Error::UnsupportedFormat(
                            "The synthetic capture ended; reopen the fixture to continue.".into(),
                        ),
                    })
                } else {
                    None
                }
            }
        }
        let mut attachment =
            VadBridge::new(Default::default(), Default::default(), VadTap::Primary)
                .expect("offline shared capture owner");
        let mut stream = flexaudio::Stream::open(
            Default::default(),
            Box::new(TerminalOnStop {
                stopped: false,
                reported: false,
            }),
        )?;
        stream.enable_ordered_delivery()?;
        stream.enable_capture_tap()?;
        stream.start()?;
        attachment.prime_shutdown_fixture_capture(&mut stream)?;
        assert_eq!(attachment.last_frame_index, 960,
            "the shutdown fixture must accept its actual canonical prefix before terminal capture closes delivery");
        stream
            .stop_checked()
            .expect_err("capture primary and cleanup failure");
        // The real shared owner's fault applies to finishing accepted PCM, never
        // fabricated output after the capture primary has closed delivery.
        #[cfg(feature = "test-faults")]
        attachment.owner.inject_failure();
        let finalization = attachment.owner.stop();
        attachment.accept(finalization, shutdown::VadFailureStage::StopFlush);
        let report = shutdown::ShutdownReport {
            core: stream.shutdown_report(),
            attachment: attachment.failures.clone(),
            binding_cleanup: Vec::new(),
        }
        .to_js();
        let Either::A(primary) = &report.primary else {
            panic!("the actual capture failure must remain the primary")
        };
        assert_eq!(primary.kind, "unsupportedFormat");
        #[cfg(feature = "test-faults")]
        assert_eq!(
            report
                .cleanup_errors
                .iter()
                .map(|error| error.kind.as_str())
                .collect::<Vec<_>>(),
            ["deviceLost", "backend"],
            "an active fixture must retain the real final VAD failure after core cleanup"
        );
        #[cfg(feature = "test-faults")]
        assert_eq!(
            report.cleanup_errors[1].vad_code.as_deref(),
            Some("Inference")
        );
        #[cfg(not(feature = "test-faults"))]
        assert_eq!(
            report
                .cleanup_errors
                .iter()
                .map(|error| error.kind.as_str())
                .collect::<Vec<_>>(),
            ["deviceLost"],
            "successful attachment closure preserves the core cleanup cause"
        );
        Ok(())
    }

    #[test]
    fn a_attachment_failure_event_names_its_cause_and_epoch() {
        let event = failure_event(
            &VadTapError::Vad(flexaudio_vad::VadError::Inference),
            shutdown::VadFailureStage::Process,
            4,
        );
        assert_eq!(event.kind, "vadFailed");
        assert_eq!(event.epoch, Some(4));
        assert!(event.permission.is_none() && event.count.is_none() && event.loss.is_none());
        let error = event.error.expect("typed cause");
        assert_eq!(error.kind, "backend");
        assert_eq!(error.vad_code.as_deref(), Some("Inference"));
        assert_eq!(error.contexts[0].operation, "normalize");
        assert!(
            error.message.contains("Inference"),
            "message: {}",
            error.message
        );
    }

    #[test]
    fn attached_primary_and_secondary_use_canonical_origins_and_close_once() {
        for selected in [VadTap::Primary, VadTap::Secondary] {
            let params = VadOptions {
                threshold: 0.0,
                min_speech_ms: 0,
                speech_pad_ms: 0,
                ..Default::default()
            };
            let mut bridge =
                VadBridge::new(params, VadRuntimeOptions { provisional: true }, selected).unwrap();
            let config = StreamConfig {
                secondary_output: Some(OutputFormat {
                    sample_rate: 16_000,
                    channels: 1,
                }),
                ..Default::default()
            };
            let mut stream = flexaudio::Stream::open(
                config,
                Box::new(flexaudio::MockBackend::new(48_000, 2, 0.0)),
            )
            .unwrap();
            stream.enable_capture_tap().unwrap();
            stream.start().unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            let mut frames = 0;
            while frames < 1920 {
                assert!(std::time::Instant::now() < deadline);
                if let Some(chunk) = stream.poll_capture_ordered() {
                    frames += chunk.frames;
                    bridge.process(chunk);
                } else {
                    thread::sleep(Duration::from_millis(2));
                }
            }
            let events = bridge.take_events();
            assert!(matches!(
                events[0].0,
                AttachedVadEvent::EpochStart {
                    capture_sample: 0,
                    seq: 0,
                    ..
                }
            ));
            stream.stop();
            while let Some(chunk) = stream.poll_capture_ordered() {
                bridge.process(chunk);
            }
            let result = bridge.owner.stop();
            bridge.accept(result, shutdown::VadFailureStage::StopFlush);
            let terminal = bridge.take_events();
            assert!(matches!(
                terminal.last().unwrap().0,
                AttachedVadEvent::Vad(flexaudio_vad::VadEvent {
                    kind: flexaudio_vad::VadEventKind::EpochEnd { .. },
                    ..
                })
            ));
            assert!(bridge.owner.stop().unwrap().is_empty());
            assert!(bridge.error.is_none());
        }
    }

    #[test]
    fn discontinuity_origin_preserves_bigint_capture_index() {
        let params = VadOptions {
            threshold: 0.0,
            min_speech_ms: 0,
            speech_pad_ms: 0,
            ..Default::default()
        };
        let mut bridge = VadBridge::new(
            params,
            VadRuntimeOptions { provisional: true },
            VadTap::Primary,
        )
        .unwrap();
        let base = 9_007_199_254_740_993;
        for (index, pts, flags) in [
            (base, 0, ChunkFlags::empty()),
            (base + 960, 20_000_000, ChunkFlags::empty()),
            (base + 1920, 1_000_000_000, ChunkFlags::DISCONTINUITY),
        ] {
            bridge.process(AudioChunk {
                data: vec![0.0; 1920],
                frames: 960,
                frame_index: index,
                pts_ns: pts,
                seq: 0,
                flags,
                dropped_before: 0,
                peak: 0.0,
                rms: 0.0,
            });
        }
        let events = bridge.take_events();
        let starts: Vec<_> = events
            .iter()
            .filter_map(|event| match event.0 {
                AttachedVadEvent::EpochStart { capture_sample, .. } => Some(capture_sample),
                _ => None,
            })
            .collect();
        assert_eq!(starts, [base, base + 1920]);
        let end = events
            .iter()
            .position(|event| {
                matches!(
                    event.0,
                    AttachedVadEvent::Vad(flexaudio_vad::VadEvent {
                        kind: flexaudio_vad::VadEventKind::EpochEnd { .. },
                        ..
                    })
                )
            })
            .unwrap();
        let restart = events
            .iter()
            .position(|event| matches!(event.0, AttachedVadEvent::EpochStart { epoch: 1, .. }))
            .unwrap();
        assert!(end < restart);
    }
}
