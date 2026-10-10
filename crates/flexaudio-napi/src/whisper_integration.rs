//! Canonical capture intake, selected carriers, and ordered flush settlement.
use super::*;
use flexaudio_vad::{
    AttachedWhisperVadEvent, WhisperVadOptions, WhisperVadParams, WhisperVadTap,
    WhisperVadTapError, WhisperVadTapFailure,
};

pub(super) struct WhisperBridge {
    owner: WhisperVadTap,
    pub tap: VadTap,
    events: Vec<AttachedWhisperVadEvent>,
    pub error: Option<WhisperVadTapError>,
    reported_error: bool,
    last_frame_index: u64,
    last_pts: i64,
}

impl WhisperBridge {
    pub fn new(
        params: WhisperVadParams,
        options: WhisperVadOptions,
        tap: VadTap,
    ) -> Result<Self, WhisperVadTapError> {
        Ok(Self {
            owner: WhisperVadTap::new(params, options)?,
            tap,
            events: Vec::new(),
            error: None,
            reported_error: false,
            last_frame_index: 0,
            last_pts: 0,
        })
    }

    fn accept(&mut self, result: Result<Vec<AttachedWhisperVadEvent>, WhisperVadTapFailure>) {
        match result {
            Ok(events) => self.events.extend(events),
            Err(failure) => {
                self.events.extend(failure.terminal_events);
                if self.error.is_none() {
                    self.error = Some(failure.error);
                }
            }
        }
    }

    fn process(&mut self, chunk: AudioChunk) {
        if self.error.is_some() {
            return;
        }
        let result = self.owner.process(
            &chunk.data,
            chunk.frame_index,
            chunk.pts_ns,
            chunk.flags.contains(ChunkFlags::DISCONTINUITY),
        );
        if result.is_ok() {
            self.last_frame_index = chunk.frame_index + chunk.frames as u64;
            self.last_pts = chunk.pts_ns + (chunk.frames as i64 * 1_000_000_000 / 48_000);
        }
        self.accept(result);
    }

    pub fn take_events(&mut self) -> Vec<whisper_vad::JsWhisperVadEvent> {
        whisper_vad::marshal(std::mem::take(&mut self.events))
    }
}

impl PairingBridge {
    pub(super) fn drain_capture(&mut self, stream: &mut flexaudio::Stream) {
        if let Some(whisper) = self.whisper.as_mut() {
            while let Some(chunk) = stream.poll_capture() {
                whisper.process(chunk);
            }
        }
    }

    pub(super) fn flush_whisper(&mut self, stopping: bool) {
        let Some(whisper) = self.whisper.as_mut() else {
            return;
        };
        let result = if stopping {
            whisper.owner.stop()
        } else {
            whisper.owner.flush()
        };
        if !stopping {
            whisper.error = None;
            whisper.reported_error = false;
        }
        whisper.accept(result);
        self.emit_whisper_carrier();
    }

    pub(super) fn report_whisper_failure(&mut self) {
        let error = self
            .whisper
            .as_ref()
            .filter(|owner| !owner.reported_error)
            .and_then(|owner| owner.error.clone());
        if let Some(error) = error {
            self.emit_whisper_carrier();
            self.on_chunk.call(
                ChunkEmit::WhisperError(error),
                ThreadsafeFunctionCallMode::NonBlocking,
            );
            self.whisper
                .as_mut()
                .expect("enabled attachment")
                .reported_error = true;
        }
    }

    fn emit_whisper_carrier(&mut self) {
        let Some(whisper) = self.whisper.as_mut() else {
            return;
        };
        let events = whisper.take_events();
        let mut carrier = JsAudioChunk {
            data: Float32Array::new(Vec::new()),
            frames: 0,
            frame_index: BigInt::from(whisper.last_frame_index),
            pts_ns: whisper.last_pts.max(self.last_emitted_primary_pts),
            seq: BigInt::from(0u64),
            flags: 0,
            dropped_before: 0,
            peak: 0.0,
            rms: 0.0,
            vad_events: None,
            whisper_vad_events: None,
            secondary: None,
        };
        match whisper.tap {
            VadTap::Primary => carrier.whisper_vad_events = Some(events),
            VadTap::Secondary => {
                let (data, encoding) = match self.secondary.map(|config| config.encoding) {
                    Some(SecEncoding::S16) => (Either::A(Int16Array::new(Vec::new())), "s16"),
                    _ => (Either::B(Float32Array::new(Vec::new())), "f32"),
                };
                carrier.secondary = Some(JsSecondaryChunk {
                    data,
                    encoding: encoding.into(),
                    frames: 0,
                    frame_index: BigInt::from(whisper.last_frame_index),
                    pts_ns: carrier.pts_ns,
                    seq: BigInt::from(0u64),
                    flags: 0,
                    dropped_before: 0,
                    peak: 0.0,
                    rms: 0.0,
                    vad_events: None,
                    whisper_vad_events: Some(events),
                });
            }
        }
        self.on_chunk.call(
            ChunkEmit::Chunk(Box::new(carrier)),
            ThreadsafeFunctionCallMode::NonBlocking,
        );
    }
}

pub(super) fn throw_error(env: &Env, error: WhisperVadTapError) -> NapiError {
    match js_error(env, &error).and_then(|exception| env.throw(exception)) {
        Ok(()) => NapiError::new(Status::PendingException, error.to_string()),
        Err(error) => error,
    }
}

pub(super) fn js_error(env: &Env, error: &WhisperVadTapError) -> napi::Result<JsObject> {
    let mut object = env.create_error(NapiError::new(Status::GenericFailure, error.to_string()))?;
    let code = match error {
        WhisperVadTapError::Vad(error) => whisper_vad::core_code(error).0,
        WhisperVadTapError::InvalidStereoLength => "InvalidStereoLength",
        WhisperVadTapError::InvalidPcm { .. } => "InvalidPcm",
        WhisperVadTapError::CaptureSampleOverflow => "CaptureSampleOverflow",
        WhisperVadTapError::PtsOutOfRange => "PtsOutOfRange",
        WhisperVadTapError::UnsupportedConversionClock => "UnsupportedConversionClock",
        WhisperVadTapError::Conversion => "Conversion",
        WhisperVadTapError::Stopped => "Stopped",
        WhisperVadTapError::FailedSession => "FailedSession",
    };
    object.set_named_property("code", code)?;
    object.set_named_property("terminalEvents", env.create_array(0)?)?;
    Ok(object)
}

pub(super) fn settle_flush(
    env: &Env,
    deferred: SendDeferred,
    error: Option<WhisperVadTapError>,
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
    fn attached_primary_and_secondary_use_canonical_origins_and_close_once() {
        for selected in [VadTap::Primary, VadTap::Secondary] {
            let params = WhisperVadParams {
                threshold: 0.0,
                min_speech_duration_ms: 0,
                speech_pad_ms: 0,
                ..Default::default()
            };
            let mut bridge =
                WhisperBridge::new(params, WhisperVadOptions { provisional: true }, selected)
                    .unwrap();
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
                if let Some(chunk) = stream.poll_capture() {
                    frames += chunk.frames;
                    bridge.process(chunk);
                } else {
                    thread::sleep(Duration::from_millis(2));
                }
            }
            let events = bridge.take_events();
            assert!(matches!(
                events[0].0,
                AttachedWhisperVadEvent::EpochStart {
                    capture_sample: 0,
                    seq: 0,
                    ..
                }
            ));
            stream.stop();
            while let Some(chunk) = stream.poll_capture() {
                bridge.process(chunk);
            }
            let result = bridge.owner.stop();
            bridge.accept(result);
            let terminal = bridge.take_events();
            assert!(matches!(
                terminal.last().unwrap().0,
                AttachedWhisperVadEvent::Vad(flexaudio_vad::WhisperVadEvent {
                    kind: flexaudio_vad::WhisperVadEventKind::EpochEnd { .. },
                    ..
                })
            ));
            assert!(bridge.owner.stop().unwrap().is_empty());
            assert!(bridge.error.is_none());
        }
    }

    #[test]
    fn discontinuity_origin_preserves_bigint_capture_index() {
        let params = WhisperVadParams {
            threshold: 0.0,
            min_speech_duration_ms: 0,
            speech_pad_ms: 0,
            ..Default::default()
        };
        let mut bridge = WhisperBridge::new(
            params,
            WhisperVadOptions { provisional: true },
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
                AttachedWhisperVadEvent::EpochStart { capture_sample, .. } => Some(capture_sample),
                _ => None,
            })
            .collect();
        assert_eq!(starts, [base, base + 1920]);
        let end = events
            .iter()
            .position(|event| {
                matches!(
                    event.0,
                    AttachedWhisperVadEvent::Vad(flexaudio_vad::WhisperVadEvent {
                        kind: flexaudio_vad::WhisperVadEventKind::EpochEnd { .. },
                        ..
                    })
                )
            })
            .unwrap();
        let restart = events
            .iter()
            .position(|event| {
                matches!(
                    event.0,
                    AttachedWhisperVadEvent::EpochStart { epoch: 1, .. }
                )
            })
            .unwrap();
        assert!(end < restart);
    }
}
