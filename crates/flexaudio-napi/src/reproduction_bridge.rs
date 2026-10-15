//! Test-only fixtures invoke the real boundary and bridge paths.
use super::*;

/// Sends deterministic PCM without devices or a producer thread; failures travel
/// through the real facade stop/report and the production N-API bridge.
struct StartupOrderFixtureBackend;
impl flexaudio::core::CaptureBackend for StartupOrderFixtureBackend {
    fn native_format(&self) -> (u32, u16) {
        (48_000, 2)
    }
    fn start(&mut self, mut sink: flexaudio::core::RawSink) -> flexaudio::Result<()> {
        let mut samples = vec![3.058; 960];
        samples.extend([0.25; 3840]);
        sink.push(&samples, 0);
        Ok(())
    }
    fn stop(&mut self) {}
}

struct ShutdownFixtureBackend {
    sink: Option<flexaudio::core::RawSink>,
    cleanup_failure: bool,
    capture_failure: bool,
    stopped: bool,
}

impl flexaudio::core::CaptureBackend for ShutdownFixtureBackend {
    fn native_format(&self) -> (u32, u16) {
        (48_000, 2)
    }
    fn start(&mut self, mut sink: flexaudio::core::RawSink) -> flexaudio::Result<()> {
        sink.push(&[0.0; 1920], 0);
        self.sink = Some(sink);
        Ok(())
    }
    fn stop(&mut self) {
        if let Some(mut sink) = self.sink.take() {
            sink.push(&[0.0; 1920], 20_000_000);
        }
        self.stopped = true;
    }
    fn stop_checked(&mut self) -> flexaudio::Result<()> {
        self.stop();
        if self.cleanup_failure {
            Err(
                flexaudio::Error::DeviceLost.with_context(flexaudio::core::ErrorContext::new(
                    flexaudio::core::Operation::Stop,
                )),
            )
        } else {
            Ok(())
        }
    }
    fn poll_event(&mut self) -> Option<Event> {
        if self.stopped && std::mem::take(&mut self.capture_failure) {
            Some(Event::TerminalError {
                error: flexaudio::Error::UnsupportedFormat("fixture".into()),
            })
        } else {
            None
        }
    }
}

// napi-derive disables automatic registration under cfg(test). Register only these test
// exports explicitly; expose the real FlexStream::stop through a plain JS object.
#[napi::bindgen_prelude::ctor]
fn register_test_exports() {
    #[cfg(test)]
    napi::bindgen_prelude::register_module_export(None, "__bindingContractBridge\0", export_bridge);
    #[cfg(test)]
    napi::bindgen_prelude::register_module_export(None, "__openMockStream\0", export_mock);
    napi::bindgen_prelude::register_module_export(
        None,
        "__bindingContractVadEvents\0",
        export_vad_events,
    );
    napi::bindgen_prelude::register_module_export(None, "__bindingContractError\0", export_error);
    napi::bindgen_prelude::register_module_export(
        None,
        "__bindingContractPayloads\0",
        export_payloads,
    );
    napi::bindgen_prelude::register_module_export(
        None,
        "__bindingContractChunkMs\0",
        export_chunk_ms,
    );
    napi::bindgen_prelude::register_module_export(
        None,
        "__bindingContractAsyncError\0",
        export_async_error,
    );
}

struct ErrorTask(flexaudio::Error);
impl Task for ErrorTask {
    type Output = flexaudio::Error;
    type JsValue = ();
    fn compute(&mut self) -> napi::Result<Self::Output> {
        Ok(self.0.clone())
    }
    fn resolve(&mut self, env: Env, output: Self::Output) -> napi::Result<()> {
        Err(boundary::async_error(&env, output))
    }
}

unsafe fn export_async_error(raw_env: sys::napi_env) -> napi::Result<sys::napi_value> {
    let env = Env::from_raw(raw_env);
    let function = env.create_function_from_closure("__bindingContractAsyncError", |ctx| {
        let kind = ctx
            .get::<napi::JsString>(0)?
            .into_utf8()?
            .as_str()?
            .to_owned();
        Ok(AsyncTask::new(ErrorTask(boundary::fixture_error(&kind))))
    })?;
    Ok(function.raw())
}

unsafe fn export_chunk_ms(raw_env: sys::napi_env) -> napi::Result<sys::napi_value> {
    let env = Env::from_raw(raw_env);
    let function = env.create_function_from_closure("__bindingContractChunkMs", |ctx| {
        let options = ctx.get::<napi::JsObject>(0)?;
        let options = unsafe { OpenOptions::from_napi_value(ctx.env.raw(), options.raw())? };
        let config = build_config(&options).map_err(|error| {
            if options.chunk_ms.is_some_and(|ms| ms != 20.0) {
                boundary::throw_error(
                    ctx.env,
                    flexaudio::core::chunk_ms_error(flexaudio::core::ChunkMsOption::JavaScript),
                )
            } else {
                error
            }
        })?;
        let chunk_ms = config.chunk_ms;
        // Facade validation is also exercised, with no start or device acquisition.
        flexaudio::Stream::open(
            config,
            Box::new(flexaudio::MockBackend::new(48_000, 2, 0.0)),
        )
        .map_err(|error| boundary::throw_error(ctx.env, error))?;
        Ok(chunk_ms)
    })?;
    Ok(function.raw())
}

#[cfg(test)]
fn js_stream(env: &Env, stream: FlexStream) -> napi::Result<JsObject> {
    let stream = Arc::new(stream);
    let mut object = env.create_object()?;
    let stop_stream = stream.clone();
    let stop = env.create_function_from_closure("stop", move |ctx| stop_stream.stop(*ctx.env))?;
    object.set_named_property("stop", stop)?;
    let terminal_stream = stream.clone();
    let terminal = env.create_function_from_closure("terminalError", move |_| {
        Ok(terminal_stream.terminal_error())
    })?;
    object.set_named_property("terminalError", terminal)?;
    let pause_stream = stream.clone();
    object.set_named_property(
        "pause",
        env.create_function_from_closure("pause", move |_| pause_stream.pause())?,
    )?;
    let resume_stream = stream.clone();
    object.set_named_property(
        "resume",
        env.create_function_from_closure("resume", move |ctx| resume_stream.resume(*ctx.env))?,
    )?;
    let paused_stream = stream.clone();
    object.set_named_property(
        "isPaused",
        env.create_function_from_closure("isPaused", move |_| paused_stream.is_paused())?,
    )?;
    let flush_stream = stream.clone();
    object.set_named_property(
        "flushVad",
        env.create_function_from_closure("flushVad", move |ctx| flush_stream.flush_vad(*ctx.env))?,
    )?;
    let switch_stream = stream.clone();
    object.set_named_property(
        "switchSource",
        env.create_function_from_closure("switchSource", move |ctx| {
            switch_stream.switch_source(*ctx.env, ctx.get::<OpenOptions>(0)?)
        })?,
    )?;
    let report =
        env.create_function_from_closure("shutdownReport", move |_| Ok(stream.shutdown_report()))?;
    object.set_named_property("shutdownReport", report)?;
    Ok(object)
}

unsafe fn export_vad_events(raw_env: sys::napi_env) -> napi::Result<sys::napi_value> {
    let env = Env::from_raw(raw_env);
    let function = env.create_function_from_closure("__bindingContractVadEvents", |_| {
        let events = [9_007_199_254_740_993, 9_223_372_036_854_775_808, u64::MAX]
            .into_iter()
            .map(|capture_sample| {
                vad::JsVadEvent(flexaudio_vad::AttachedVadEvent::EpochStart {
                    epoch: 0,
                    seq: 0,
                    capture_sample,
                    pts_ns: 1_500_000_000,
                })
            })
            .collect::<Vec<_>>();
        Ok(events)
    })?;
    Ok(function.raw())
}

unsafe fn export_error(raw_env: sys::napi_env) -> napi::Result<sys::napi_value> {
    let env = Env::from_raw(raw_env);
    let function =
        env.create_function_from_closure("__bindingContractError", |ctx| -> napi::Result<()> {
            let kind = ctx
                .get::<napi::JsString>(0)?
                .into_utf8()?
                .as_str()?
                .to_owned();
            Err(boundary::throw_error(
                ctx.env,
                boundary::fixture_error(&kind),
            ))
        })?;
    Ok(function.raw())
}

unsafe fn export_payloads(raw_env: sys::napi_env) -> napi::Result<sys::napi_value> {
    use flexaudio::core::{AudioLoss, MixLane, OutputTap};
    let env = Env::from_raw(raw_env);
    let function = env.create_function_from_closure("__bindingContractPayloads", |ctx| {
        let mut streams = vec![
            event_to_js(Event::RecoverableError {
                error: flexaudio::Error::DeviceLost,
            }),
            event_to_js(Event::ShutdownError {
                error: flexaudio::Error::DeviceLost,
            }),
            event_to_js(Event::TerminalError {
                error: flexaudio::Error::DeviceLost,
            }),
        ];
        let mut devices = vec![
            device_event_to_js(DeviceEvent::DefaultCleared {
                kind: flexaudio::DefaultDeviceKind::Microphone,
            }),
            device_event_to_js(DeviceEvent::DefaultCleared {
                kind: flexaudio::DefaultDeviceKind::SystemAudio,
            }),
        ];
        for count in [9_007_199_254_740_993, 9_223_372_036_854_775_808, u64::MAX] {
            streams.push(event_to_js(Event::ChunkDropped { count }));
            devices.push(device_event_to_js(DeviceEvent::RescanRequired {
                dropped_events: count,
            }));
        }
        let count = std::num::NonZeroU64::new(u64::MAX);
        for loss in [
            AudioLoss::raw_overflow(None, None, 48_000, 2).unwrap(),
            AudioLoss::raw_overflow(Some(MixLane::Microphone), count, 48_000, 2).unwrap(),
            AudioLoss::mix_fifo_overflow(MixLane::SystemAudio, count),
            AudioLoss::output_overflow(OutputTap::Secondary, count, 16_000, 1).unwrap(),
        ] {
            streams.push(event_to_js(Event::AudioLoss { loss }));
        }
        streams.push(event_to_js(Event::Clipped));
        let mut object = ctx.env.create_object()?;
        object.set_named_property("streams", streams)?;
        object.set_named_property("devices", devices)?;
        Ok(object)
    })?;
    Ok(function.raw())
}

#[cfg(test)]
unsafe fn export_bridge(raw_env: sys::napi_env) -> napi::Result<sys::napi_value> {
    let env = Env::from_raw(raw_env);
    let function = env.create_function_from_closure("__bindingContractBridge", |ctx| {
        let scenario = ctx
            .get::<napi::JsString>(0)?
            .into_utf8()?
            .as_str()?
            .to_owned();
        let callback = ctx.get::<napi::JsFunction>(1)?;
        let callback = unsafe { Function::from_napi_value(ctx.env.raw(), callback.raw())? };
        let stream = binding_contract_bridge(
            *ctx.env,
            scenario,
            callback,
            if ctx.length <= 2 {
                None
            } else {
                ctx.get::<Option<Function<JsStreamEvent, Unknown>>>(2)?
            },
        )?;
        js_stream(ctx.env, stream)
    })?;
    Ok(function.raw())
}

#[cfg(test)]
unsafe fn export_mock(raw_env: sys::napi_env) -> napi::Result<sys::napi_value> {
    let env = Env::from_raw(raw_env);
    let function = env.create_function_from_closure("__openMockStream", |ctx| {
        let rate = ctx.get::<napi::JsNumber>(0)?.get_uint32()?;
        let channels = u16::try_from(ctx.get::<napi::JsNumber>(1)?.get_uint32()?)
            .map_err(|_| NapiError::new(Status::InvalidArg, "channels exceed u16"))?;
        let frequency = ctx.get::<napi::JsNumber>(2)?.get_double()?;
        let callback = ctx.get::<napi::JsFunction>(3)?;
        let callback = unsafe { Function::from_napi_value(ctx.env.raw(), callback.raw())? };
        let stream = open_mock_stream(
            *ctx.env,
            rate,
            channels,
            frequency,
            callback,
            if ctx.length <= 4 {
                None
            } else {
                ctx.get::<Option<u32>>(4)?
            },
            if ctx.length <= 5 {
                None
            } else {
                ctx.get::<Option<u16>>(5)?
            },
            if ctx.length <= 6 {
                None
            } else {
                ctx.get::<Option<String>>(6)?
            },
            if ctx.length <= 7 {
                None
            } else {
                ctx.get::<Option<napi::JsUnknown>>(7)?
            },
            if ctx.length <= 8 {
                None
            } else {
                ctx.get::<Option<Function<JsStreamEvent, Unknown>>>(8)?
            },
            if ctx.length <= 9 {
                None
            } else {
                ctx.get::<Option<f64>>(9)?
            },
            if ctx.length <= 10 {
                None
            } else {
                ctx.get::<Option<f64>>(10)?
            },
        )?;
        js_stream(ctx.env, stream)
    })?;
    Ok(function.raw())
}

#[napi(js_name = "__bindingContractBridge")]
pub fn binding_contract_bridge(
    env: Env,
    scenario: String,
    on_chunk: Function<JsAudioChunk, Unknown>,
    on_event: Option<Function<JsStreamEvent, Unknown>>,
) -> napi::Result<FlexStream> {
    let phase = Arc::new(Mutex::new(StopPhase::Running));
    let terminal: TerminalError = Arc::new(Mutex::new(None));
    let report: FinalReport = Arc::new(Mutex::new(None));
    let user = make_user_chunk_cb(&env, &on_chunk)?;
    let (mut event_tsfn, event_callback) = make_stream_event_callbacks(&env, on_event)?;
    let weak: ChunkTsfnWeakCell = Arc::new(OnceLock::new());
    let chunk = make_chunk_tsfn(
        &env,
        phase.clone(),
        user.clone(),
        weak.clone(),
        terminal.clone(),
        report.clone(),
        event_callback.clone(),
    )?;
    let settle = make_settle_tsfn(
        &env,
        phase.clone(),
        weak,
        user,
        report.clone(),
        event_callback.clone(),
    )?;
    // Reverse queue creation order in this fixture: independent TSFNs cannot provide FIFO.
    let startup_drain_fixture =
        matches!(scenario.as_str(), "startup-flush" | "startup-stop-prologue");
    let mut startup_stream = None;
    if scenario == "startup-order" || startup_drain_fixture {
        drop(event_tsfn.take());
        if let Some(callback) = &event_callback {
            let mut value = ptr::null_mut();
            check_status!(unsafe {
                sys::napi_get_reference_value(env.raw(), callback.refer(), &mut value)
            })?;
            let function =
                unsafe { Function::<JsStreamEvent, Unknown>::from_napi_value(env.raw(), value)? };
            event_tsfn = make_stream_event_callbacks(&env, Some(function))?.0;
        }
    }
    let mut bridge = PairingBridge {
        attachment: None,
        on_chunk: chunk.as_ref().clone(),
        stop_phase: phase.clone(),
        report: report.clone(),

        secondary: Some(SecondaryTapCfg {
            encoding: SecEncoding::F32,
        }),
        primary_fifo: VecDeque::new(),
        secondary_fifo: VecDeque::new(),
        last_emitted_primary_pts: 0,
        last_primary_seq: 0,
        last_primary_frame_index: 0,
        last_primary_dropped: 0,
        last_secondary: SecondaryCarrierMetadata::default(),
    };
    if scenario == "startup-order" || startup_drain_fixture {
        bridge.secondary = None;
        bridge.attachment = Some(
            attachment::VadBridge::new(
                flexaudio_vad::VadOptions::default(),
                flexaudio_vad::VadRuntimeOptions::default(),
                VadTap::Primary,
            )
            .map_err(|error| attachment::throw_error(&env, error))?,
        );
        let mut stream = flexaudio::Stream::open(
            StreamConfig::default(),
            Box::new(StartupOrderFixtureBackend),
        )
        .map_err(|error| boundary::throw_error(&env, error))?;
        stream
            .enable_capture_tap()
            .map_err(|error| boundary::throw_error(&env, error))?;
        start_bridge_stream(&env, &mut stream)?;
        if startup_drain_fixture {
            // Join intake without claiming events, making the accepted prefix deterministic.
            // The fixture then invokes the same drain as the production flush/stop branch.
            stream
                .stop_checked()
                .map_err(|error| boundary::throw_error(&env, error))?;
            startup_stream = Some(stream);
        } else {
            // Queue both notice and PCM before the bridge starts, exposing cross-TSFN ordering.
            std::thread::sleep(Duration::from_millis(30));
            return FlexStream::spawn(
                &env, stream, bridge, event_tsfn, chunk, settle, terminal, true,
            );
        }
    }
    if matches!(
        scenario.as_str(),
        "attachment-secondary-orphan" | "attachment-pending-flush" | "attachment-pending-stop"
    ) {
        let mut attachment = attachment::VadBridge::new(
            flexaudio_vad::VadOptions::default(),
            flexaudio_vad::VadRuntimeOptions::default(),
            VadTap::Secondary,
        )
        .map_err(|error| attachment::throw_error(&env, error))?;
        attachment.inject_faults(Some(2), 0);
        bridge.attachment = Some(attachment);
    }
    if matches!(
        scenario.as_str(),
        "attachment-process"
            | "attachment-flush"
            | "attachment-process-cleanup"
            | "attachment-flush-cleanup"
            | "attachment-flush-primary"
    ) {
        let mut attachment = attachment::VadBridge::new(
            flexaudio_vad::VadOptions::default(),
            flexaudio_vad::VadRuntimeOptions::default(),
            VadTap::Primary,
        )
        .map_err(|error| attachment::throw_error(&env, error))?;
        attachment.inject_shutdown_failure(scenario.starts_with("attachment-process"));
        bridge.attachment = Some(attachment);
        let mut stream = flexaudio::Stream::open(
            StreamConfig::default(),
            Box::new(ShutdownFixtureBackend {
                sink: None,
                cleanup_failure: scenario.ends_with("cleanup") || scenario.ends_with("primary"),
                capture_failure: scenario.ends_with("primary"),
                stopped: false,
            }),
        )
        .map_err(|error| boundary::throw_error(&env, error))?;
        stream
            .enable_capture_tap()
            .map_err(|error| boundary::throw_error(&env, error))?;
        start_bridge_stream(&env, &mut stream)?;
        if let Some(attachment) = bridge.attachment.as_mut() {
            attachment
                .prime_shutdown_fixture_capture(&mut stream)
                .map_err(|error| boundary::throw_error(&env, error))?;
        }
        return FlexStream::spawn(&env, stream, bridge, None, chunk, settle, terminal, true);
    }
    #[cfg(target_os = "linux")]
    if scenario == "exhaust" || scenario == "reaper" {
        // Reduce only this probe child's thread allowance after Node/TSFN initialization.
        #[repr(C)]
        struct Limit {
            soft: u64,
            hard: u64,
        }
        unsafe extern "C" {
            fn getrlimit(resource: i32, limit: *mut Limit) -> i32;
            fn setrlimit(resource: i32, limit: *const Limit) -> i32;
        }
        let mut stream = flexaudio::Stream::open(
            StreamConfig::default(),
            Box::new(flexaudio::MockBackend::new(48000, 2, 440.0)),
        )
        .map_err(|error| boundary::throw_error(&env, error))?;
        let restrict_threads = || {
            let mut limit = Limit { soft: 0, hard: 0 };
            // SAFETY: Linux RLIMIT_NPROC=6 and a valid Linux x64 rlimit.
            unsafe {
                assert_eq!(getrlimit(6, &mut limit), 0);
                limit.soft = 0;
                assert_eq!(setrlimit(6, &limit), 0);
            }
        };
        if scenario == "reaper" {
            start_bridge_stream(&env, &mut stream)?;
            let created =
                FlexStream::spawn(&env, stream, bridge, None, chunk, settle, terminal, true)?;
            restrict_threads();
            return Ok(created);
        }
        restrict_threads();
        return FlexStream::spawn(&env, stream, bridge, None, chunk, settle, terminal, true);
    }
    let fixture_terminal = terminal.clone();
    let handle = spawn_bridge_thread(
        chunk.clone(),
        settle.clone(),
        phase.clone(),
        report.clone(),
        terminal.clone(),
        move || {
            if let Some(mut stream) = startup_stream.take() {
                bridge.drain_stream(&mut stream, event_tsfn.as_ref(), &fixture_terminal);
                if scenario == "startup-stop-prologue" && bridge.last_primary_frame_index == 0 {
                    record_cleanup_failure(&bridge.report,
                        flexaudio::Error::Backend("The synthetic stop prologue skipped accepted startup input; forward queued stream events before polling PCM.".into()));
                }
                bridge.flush_attachment(scenario == "startup-stop-prologue");
                bridge.report_attachment_failure();
                bridge.retain_shutdown_outcome(stream.shutdown_report());
                return;
            }
            if scenario == "panic" {
                panic!("synthetic bridge failure");
            }
            if matches!(scenario.as_str(), "attachment-secondary-orphan" | "attachment-pending-flush" | "attachment-pending-stop") {
                let mut attachment = bridge
                    .attachment
                    .take()
                    .expect("the scenario owns its initialized VAD attachment");
                for index in 0..2 {
                    attachment.process(AudioChunk {
                        data: vec![0.0; 1920],
                        frames: 960,
                        frame_index: index * 960,
                        pts_ns: index as i64 * 20_000_000,
                        seq: index,
                        flags: ChunkFlags::empty(),
                        dropped_before: 0,
                        peak: 0.0,
                        rms: 0.0,
                    });
                }
                bridge.attachment = Some(attachment);
                bridge.on_secondary(SecondaryChunk {
                    samples: vec![0.0; 960],
                    frames: 960,
                    frame_index: 0,
                    pts_ns: 0,
                    seq: 17,
                    flags: ChunkFlags::empty(),
                    dropped_before: 5,
                    peak: 0.0,
                    rms: 0.0,
                });
                bridge.report_attachment_failure(); // No carrier yet: retain the notification.
                if scenario == "attachment-secondary-orphan" {
                    bridge.on_primary(AudioChunk {
                        data: vec![0.0; 960],
                        frames: 960,
                        frame_index: 4800,
                        pts_ns: 100_000_000,
                        seq: 0,
                        flags: ChunkFlags::empty(),
                        dropped_before: 0,
                        peak: 0.0,
                        rms: 0.0,
                    });
                    bridge.drain_pairs();
                } else {
                    bridge.flush_attachment(scenario == "attachment-pending-stop"); // Recovery/stop must not erase pending failure.
                }
                bridge.report_attachment_failure();
                bridge.retain_shutdown_outcome(None);
                return;
            }
            let primary_pts = if scenario == "orphan" { 100_000_000 } else { 0 };
            let secondary_pts = if scenario == "residual" {
                100_000_000
            } else {
                0
            };
            bridge.on_secondary(SecondaryChunk {
                frame_index: 0,
                samples: vec![0.25; 960],
                frames: 960,
                pts_ns: secondary_pts,
                seq: 7,
                flags: ChunkFlags::empty(),
                dropped_before: 3,
                peak: 0.25,
                rms: 0.25,
            });
            bridge.secondary_fifo.back_mut().unwrap().vad_events = Some(vec![vad::JsVadEvent(flexaudio_vad::AttachedVadEvent::Vad(flexaudio_vad::VadEvent {
                epoch: 0, seq: 0, kind: flexaudio_vad::VadEventKind::Segment(flexaudio_vad::SpeechSegment { start_ms: 0, end_ms: 20 }),
            }))]);
            bridge.on_primary(AudioChunk {
                frame_index: 0,
                data: vec![0.25; 960],
                frames: 960,
                pts_ns: primary_pts,
                seq: 7,
                flags: ChunkFlags::empty(),
                dropped_before: 3,
                peak: 0.25,
                rms: 0.25,
            });
            bridge.drain_pairs();
            bridge.emit_terminator();
        },
    )
    .map_err(|error| boundary::throw_error(&env, error))?;
    Ok(FlexStream {
        stop_flag: Arc::new(AtomicBool::new(false)),
        inner: Arc::new(Mutex::new(StreamInner {
            handle: Some(handle),
            cmd_tx: None,
        })),
        stop_phase: phase,
        chunk_tsfn: chunk,
        settle_tsfn: settle,
        terminal,
        report,
        mock_source: true,
    })
}
