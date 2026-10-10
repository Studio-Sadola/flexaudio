//! Test-only fixtures invoke the real boundary and bridge paths.
use super::*;

/// Sends deterministic PCM without devices or a producer thread; failures travel
/// through the real facade stop/report and the production N-API bridge.
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
    napi::bindgen_prelude::register_module_export(None, "__reproP9Bridge\0", export_bridge);
    napi::bindgen_prelude::register_module_export(None, "__openMockStream\0", export_mock);
    napi::bindgen_prelude::register_module_export(None, "__reproP9VadEvents\0", export_vad_events);
    napi::bindgen_prelude::register_module_export(None, "__reproP9Error\0", export_error);
    napi::bindgen_prelude::register_module_export(None, "__reproP9Payloads\0", export_payloads);
    napi::bindgen_prelude::register_module_export(None, "__reproP9ChunkMs\0", export_chunk_ms);
    napi::bindgen_prelude::register_module_export(
        None,
        "__reproP9AsyncError\0",
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
    let function = env.create_function_from_closure("__reproP9AsyncError", |ctx| {
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
    let function = env.create_function_from_closure("__reproP9ChunkMs", |ctx| {
        let options = ctx.get::<napi::JsObject>(0)?;
        let options = unsafe { OpenOptions::from_napi_value(ctx.env.raw(), options.raw())? };
        let config = build_config(&options).map_err(|error| {
            if options.chunk_ms.is_some_and(|ms| ms != 20.0) {
                boundary::throw_error(
                    ctx.env,
                    flexaudio::Error::InvalidArg("chunkMs must be 20".into()),
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
    let report =
        env.create_function_from_closure("shutdownReport", move |_| Ok(stream.shutdown_report()))?;
    object.set_named_property("shutdownReport", report)?;
    Ok(object)
}

unsafe fn export_vad_events(raw_env: sys::napi_env) -> napi::Result<sys::napi_value> {
    let env = Env::from_raw(raw_env);
    let function = env.create_function_from_closure("__reproP9VadEvents", |_| {
        let mut events = Vec::new();
        for count in [9_007_199_254_740_993, 9_223_372_036_854_775_808, u64::MAX] {
            for event in [
                VadEvent::SpeechStart { at_sample: count },
                VadEvent::SpeechEnd { at_sample: count },
            ] {
                for pts in [None, Some(1_500_000_000)] {
                    events.push(vad_event_to_js_abs(event, pts));
                }
            }
        }
        Ok(events)
    })?;
    Ok(function.raw())
}

unsafe fn export_error(raw_env: sys::napi_env) -> napi::Result<sys::napi_value> {
    let env = Env::from_raw(raw_env);
    let function =
        env.create_function_from_closure("__reproP9Error", |ctx| -> napi::Result<()> {
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
    let function = env.create_function_from_closure("__reproP9Payloads", |ctx| {
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
        streams.push(event_to_js(Event::PermissionGranted));
        let mut object = ctx.env.create_object()?;
        object.set_named_property("streams", streams)?;
        object.set_named_property("devices", devices)?;
        Ok(object)
    })?;
    Ok(function.raw())
}

unsafe fn export_bridge(raw_env: sys::napi_env) -> napi::Result<sys::napi_value> {
    let env = Env::from_raw(raw_env);
    let function = env.create_function_from_closure("__reproP9Bridge", |ctx| {
        let scenario = ctx
            .get::<napi::JsString>(0)?
            .into_utf8()?
            .as_str()?
            .to_owned();
        let callback = ctx.get::<napi::JsFunction>(1)?;
        let callback = unsafe { Function::from_napi_value(ctx.env.raw(), callback.raw())? };
        let stream = repro_p9_bridge(*ctx.env, scenario, callback)?;
        js_stream(ctx.env, stream)
    })?;
    Ok(function.raw())
}

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
            *ctx.env, rate, channels, frequency, callback, None, None, None, None, None, None,
        )?;
        js_stream(ctx.env, stream)
    })?;
    Ok(function.raw())
}

#[napi(js_name = "__reproP9Bridge")]
pub fn repro_p9_bridge(
    env: Env,
    scenario: String,
    on_chunk: Function<JsAudioChunk, Unknown>,
) -> napi::Result<FlexStream> {
    let phase = Arc::new(Mutex::new(StopPhase::Running));
    let terminal: TerminalError = Arc::new(Mutex::new(None));
    let report: FinalReport = Arc::new(Mutex::new(None));
    let user = make_user_chunk_cb(&env, &on_chunk)?;
    let weak: ChunkTsfnWeakCell = Arc::new(OnceLock::new());
    let chunk = make_chunk_tsfn(
        &env,
        phase.clone(),
        user.clone(),
        weak.clone(),
        terminal.clone(),
        report.clone(),
    )?;
    let settle = make_settle_tsfn(&env, phase.clone(), weak, user, report.clone())?;
    let mut bridge = PairingBridge {
        whisper: None,
        on_chunk: chunk.as_ref().clone(),
        stop_phase: phase.clone(),
        report: report.clone(),
        vad: None,
        vad_tap: VadTap::Primary,

        vad_anchor_pts: 0,
        pending_flush_events: Vec::new(),
        output_rate: 48000,
        output_channels: 1,
        secondary: Some(SecondaryTapCfg {
            rate: 48000,
            channels: 1,
            encoding: SecEncoding::F32,
        }),
        primary_fifo: VecDeque::new(),
        secondary_fifo: VecDeque::new(),
        last_emitted_primary_pts: 0,
        last_primary_seq: 0,
        last_primary_frame_index: 0,
        last_primary_dropped: 0,
    };
    if matches!(
        scenario.as_str(),
        "whisper-process"
            | "whisper-flush"
            | "whisper-process-cleanup"
            | "whisper-flush-cleanup"
            | "whisper-flush-primary"
    ) {
        let mut whisper = whisper_integration::WhisperBridge::new(
            flexaudio_vad::WhisperVadParams::default(),
            flexaudio_vad::WhisperVadOptions::default(),
            VadTap::Primary,
        )
        .map_err(|error| whisper_integration::throw_error(&env, error))?;
        whisper.inject_shutdown_failure(scenario.starts_with("whisper-process"));
        bridge.whisper = Some(whisper);
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
        stream
            .start()
            .map_err(|error| boundary::throw_error(&env, error))?;
        return FlexStream::spawn(&env, stream, bridge, None, chunk, settle, terminal);
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
            stream
                .start()
                .map_err(|error| boundary::throw_error(&env, error))?;
            let created = FlexStream::spawn(&env, stream, bridge, None, chunk, settle, terminal)?;
            restrict_threads();
            return Ok(created);
        }
        restrict_threads();
        return FlexStream::spawn(&env, stream, bridge, None, chunk, settle, terminal);
    }
    let handle = spawn_bridge_thread(
        chunk.clone(),
        settle.clone(),
        phase.clone(),
        report.clone(),
        terminal.clone(),
        move || {
            if scenario == "panic" {
                panic!("repro: C F37 bridge failure");
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
            bridge.secondary_fifo.back_mut().unwrap().vad_events = Some(vec![JsVadEvent {
                kind: "speechEnd".into(),
                at_sample: BigInt::from(960u64),
                at_ns: Some(20_000_000),
            }]);
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
            bridge.flush_vad_final();
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
    })
}
