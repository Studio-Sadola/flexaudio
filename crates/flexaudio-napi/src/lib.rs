//! flexaudio-napi — Node.js (N-API) addon.
//!
//! Bindings for Node.js apps to use flexaudio in-process. Low-latency
//! streaming recordings are delivered to Node through callbacks.
//!
//! Design:
//! - Public functions use camelCase (`#[napi]` converts them to JS names).
//! - Chunks/events reach JS callbacks through `ThreadsafeFunction` (ErrorStrategy::Fatal).
//! - Constructing `FlexStream` spawns a bridge thread. After `stream.start()`, it
//!   polls `poll_chunk` / `poll_event` every 1ms and forwards them to TSFN in NonBlocking mode.
//! - Stopping uses `stop(): Promise<void>`. Joining happens off the JS thread. After the last PCM and
//!   the `frames:0` terminator are queued on the same TSFN, one end signal is queued. When JS
//!   processes that signal, the Promise resolves. Drop (GC) joins on a reaper
//!   thread without blocking JS.
//!
//! No network communication occurs at runtime (napi is only the N-API bridge).

mod boundary;
mod flac_encoder;
mod shutdown;
use boundary::{device_event_to_js, event_to_js, to_napi_err, JsAudioError, JsAudioLoss};
mod vad_integration;
mod whisper_integration;
mod whisper_vad;

use std::collections::VecDeque;
use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use napi::bindgen_prelude::{
    AsyncTask, BigInt, Either, Float32Array, FromNapiValue, Function, Int16Array, Undefined,
    Unknown,
};
use napi::threadsafe_function::{
    ErrorStrategy, ThreadSafeCallContext, ThreadsafeFunction, ThreadsafeFunctionCallMode,
};
use napi::{
    check_status, sys, Env, Error as NapiError, JsObject, NapiRaw, NapiValue, Status, Task,
};
use napi_derive::napi;

use flexaudio::{
    AudioChunk, ChunkFlags, DeviceEvent, DeviceInfo, Event, OutputFormat, ProcessInfo, ProcessMode,
    SecondaryChunk, SourceKind, StreamConfig,
};

// Core types for the three addons. Import aliases to avoid the `#[napi]` wrappers (Vad / Denoiser).
use flexaudio_denoise::{DenoiseError, Denoiser as CoreDenoiser};
use flexaudio_vad::{Vad as CoreVad, VadConfig, VadError, VadEvent};

/// pts window for pairing the secondary tap (60ms = 3 chunks). The secondary trails by 20–60ms,
/// so this window matches timestamps even with a delay of up to 3 chunks.
const PAIR_WINDOW_NS: i64 = 60_000_000;
/// Width of a 20ms chunk in ns.
const CHUNK_SPAN_NS: i64 = 20_000_000;

/// Default `max_speech_ms` for the integrated VAD path (`openStream`), used only
/// when the caller leaves `vad.maxSpeechMs` unset. silero's own default is 0
/// (unbounded), which lets a monologue with no real silence stall the segment —
/// and hence recognition latency — indefinitely. Real-time capture bounds it at
/// 30 s so an over-long utterance is force-split. The standalone `Vad` class
/// keeps silero's default (0); only the integrated path holds this opinion.
/// An explicit `maxSpeechMs` (including `0`) always wins.
const INTEGRATED_VAD_MAX_SPEECH_MS_DEFAULT: u32 = 30_000;

/// Which tap runs integrated VAD.
#[derive(Clone, Copy, PartialEq, Eq)]
enum VadTap {
    Primary,
    Secondary,
}

/// Secondary tap sample encoding (core always uses f32; encoding happens at the binding boundary).
#[derive(Clone, Copy, PartialEq, Eq)]
enum SecEncoding {
    F32,
    S16,
}

// Bridge thread polling interval. Small enough for 20ms chunks while avoiding busy-waiting.
const POLL_INTERVAL: Duration = Duration::from_millis(1);
// Device attachment/removal is infrequent. A 100ms response time is sufficient.
const DEVICE_POLL_INTERVAL: Duration = Duration::from_millis(100);

// TSFN aliases using ErrorStrategy::Fatal. `.call(value, mode)` accepts values directly
// (CalleeHandled uses `.call(Result<T>, mode)` and requires wrapping in Result).
type ChunkTsfn = ThreadsafeFunction<ChunkEmit, ErrorStrategy::Fatal>;
type SettleTsfn = ThreadsafeFunction<(), ErrorStrategy::Fatal>;
type EventTsfn = ThreadsafeFunction<JsStreamEvent, ErrorStrategy::Fatal>;
type DeviceTsfn = ThreadsafeFunction<JsDeviceEvent, ErrorStrategy::Fatal>;

/// Values queued on the onChunk TSFN. Chunks go to the user's `onChunk`; `StopFlushed` is
/// an end signal in the same queue (not exposed to JS onChunk).
enum ChunkEmit {
    Chunk(Box<JsAudioChunk>),
    WhisperFlushed(SendDeferred, Option<flexaudio_vad::WhisperVadTapError>),
    WhisperError(flexaudio_vad::WhisperVadTapError),
    StopFlushed,
    VadError(String),
}

/// `napi_deferred` is a raw pointer. Created on the JS thread and resolved through TSFN.
#[derive(Clone, Copy)]
struct SendDeferred(sys::napi_deferred);
unsafe impl Send for SendDeferred {}
unsafe impl Sync for SendDeferred {}

/// Completion coordination for `stop()`. Waiters are deferred values from `napi_create_promise`.
enum StopPhase {
    Running,
    Stopping { waiters: Vec<SendDeferred> },
    Stopped,
}

struct StreamInner {
    handle: Option<JoinHandle<()>>,
    cmd_tx: Option<mpsc::Sender<BridgeCmd>>,
}

/// Keep JS onChunk alive for the TSFN lifetime. `FunctionRef<JsAudioChunk, _>` may not
/// implement Send because of PhantomData, so use a raw `napi_ref`.
///
/// `napi_delete_reference` is restricted to the JS thread. Explicitly release it in StopFlushed /
/// the settlement callback (both TSFN callbacks on the JS thread); Drop during TSFN finalization
/// is then a no-op. This releases the onChunk closure after `stop()` even if the TSFN remains alive.
struct UserChunkCb {
    env: sys::napi_env,
    refer: AtomicPtr<c_void>,
}

unsafe impl Send for UserChunkCb {}
unsafe impl Sync for UserChunkCb {}

impl UserChunkCb {
    fn new(env: sys::napi_env, refer: sys::napi_ref) -> Arc<Self> {
        Arc::new(Self {
            env,
            refer: AtomicPtr::new(refer.cast()),
        })
    }

    fn refer(&self) -> sys::napi_ref {
        self.refer.load(Ordering::SeqCst).cast()
    }

    /// JS thread only. Subsequent calls are no-ops.
    fn release(&self) {
        let refer: sys::napi_ref = self.refer.swap(ptr::null_mut(), Ordering::SeqCst).cast();
        if !self.env.is_null() && !refer.is_null() {
            let _ = unsafe { sys::napi_delete_reference(self.env, refer) };
        }
    }
}

impl Drop for UserChunkCb {
    fn drop(&mut self) {
        self.release();
    }
}

fn lock_poisoned<T>(
    p: std::sync::PoisonError<std::sync::MutexGuard<T>>,
) -> std::sync::MutexGuard<T> {
    p.into_inner()
}

fn create_js_promise(env: &Env) -> napi::Result<(SendDeferred, JsObject)> {
    let mut deferred = ptr::null_mut();
    let mut promise = ptr::null_mut();
    check_status!(unsafe { sys::napi_create_promise(env.raw(), &mut deferred, &mut promise) })?;
    Ok((SendDeferred(deferred), unsafe {
        JsObject::from_raw_unchecked(env.raw(), promise)
    }))
}

fn resolve_undefined(env: sys::napi_env, deferred: SendDeferred) {
    let mut undefined = ptr::null_mut();
    let _ = unsafe { sys::napi_get_undefined(env, &mut undefined) };
    let _ = unsafe { sys::napi_resolve_deferred(env, deferred.0, undefined) };
}

/// Settle on the JS thread after delivery has drained. Terminal failures reject
/// every waiter, including subsequent stop calls.
fn settle_stop(env: sys::napi_env, deferred: SendDeferred, report: &FinalReport) {
    // A completed report is the sole authority for every stop waiter.
    let retained = report.lock().unwrap_or_else(lock_poisoned);
    // SAFETY: settlement is called only on the live JS thread.
    let js_env = unsafe { Env::from_raw(env) };
    let exception = match retained.as_ref() {
        Some(report) => report.exception(&js_env),
        None => boundary::js_error(
            &js_env,
            &flexaudio::Error::InvalidState("shutdown report missing".into()),
        )
        .map(Some),
    };
    match exception {
        Ok(Some(exception)) => {
            let _ = unsafe { sys::napi_reject_deferred(env, deferred.0, exception.raw()) };
        }
        Ok(None) => resolve_undefined(env, deferred),
        Err(error) => {
            if let Ok(exception) = js_env.create_error(error) {
                let _ = unsafe { sys::napi_reject_deferred(env, deferred.0, exception.raw()) };
            }
        }
    }
}

fn terminal_event(error: flexaudio::Error) -> JsStreamEvent {
    match error {
        flexaudio::Error::PermissionDenied { permission, detail } => {
            event_to_js(Event::PermissionDenied { permission, detail })
        }
        other => event_to_js(Event::TerminalError { error: other }),
    }
}

/// One notification path for normal polling and final stop, including hosts
/// without an event callback. The durable terminal snapshot precedes JS delivery.
fn forward_stream_events(
    stream: &mut flexaudio::Stream,
    callback: Option<&EventTsfn>,
    terminal: &Mutex<Option<StreamTerminal>>,
) {
    if let Some(error) = stream.terminal_error() {
        *terminal.lock().unwrap_or_else(lock_poisoned) = Some(StreamTerminal::Capture(error));
    }
    while let Some(event) = stream.poll_event() {
        if let Event::PermissionDenied { permission, detail } = &event {
            *terminal.lock().unwrap_or_else(lock_poisoned) = Some(StreamTerminal::Capture(
                flexaudio::Error::PermissionDenied {
                    permission: *permission,
                    detail: detail.clone(),
                },
            ));
        }
        if let Event::TerminalError { error } = &event {
            *terminal.lock().unwrap_or_else(lock_poisoned) =
                Some(StreamTerminal::Capture(error.clone()));
        }
        if let Some(callback) = callback {
            callback.call(event_to_js(event), ThreadsafeFunctionCallMode::NonBlocking);
        }
    }
}

fn take_stop_waiters(phase: &Mutex<StopPhase>) -> Vec<SendDeferred> {
    let mut g = phase.lock().unwrap_or_else(lock_poisoned);
    match std::mem::replace(&mut *g, StopPhase::Stopped) {
        StopPhase::Stopping { waiters } => waiters,
        StopPhase::Stopped => vec![],
        StopPhase::Running => vec![],
    }
}

/// Durable copy of the core's terminal failure, retained after the bridge exits.
#[derive(Clone)]
enum StreamTerminal {
    Capture(flexaudio::Error),
    Whisper(flexaudio_vad::WhisperVadTapError),
}
impl std::fmt::Display for StreamTerminal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Capture(error) => error.fmt(formatter),
            Self::Whisper(error) => error.fmt(formatter),
        }
    }
}
fn retain_whisper_failure(
    terminal: &Mutex<Option<StreamTerminal>>,
    error: flexaudio_vad::WhisperVadTapError,
) {
    let mut terminal = terminal.lock().unwrap_or_else(lock_poisoned);
    if !matches!(*terminal, Some(StreamTerminal::Capture(_))) {
        *terminal = Some(StreamTerminal::Whisper(error));
    }
}

type TerminalError = Arc<Mutex<Option<StreamTerminal>>>;
type FinalReport = Arc<Mutex<Option<shutdown::ShutdownReport>>>;

fn record_cleanup_failure(report: &FinalReport, error: flexaudio::Error) {
    let mut report = report.lock().unwrap_or_else(lock_poisoned);
    report
        .get_or_insert_with(shutdown::ShutdownReport::default)
        .binding_cleanup
        .push(error);
}

fn record_bridge_failure(report: &FinalReport, terminal: &Mutex<Option<StreamTerminal>>) {
    let terminal = terminal.lock().unwrap_or_else(lock_poisoned).clone();
    {
        let mut report = report.lock().unwrap_or_else(lock_poisoned);
        if report.is_none() {
            let mut retained = shutdown::ShutdownReport::default();
            match terminal {
                Some(StreamTerminal::Capture(error)) => {
                    retained.core = Some(flexaudio::core::ShutdownReport::new(
                        Some(error),
                        Vec::new(),
                    ));
                }
                Some(StreamTerminal::Whisper(error)) => {
                    retained.whisper = Some(shutdown::WhisperFailure {
                        error,
                        stage: shutdown::WhisperFailureStage::Process,
                    });
                }
                None => {}
            }
            *report = Some(retained);
        }
    }
    record_cleanup_failure(
        report,
        flexaudio::Error::Backend("bridge failure".into()).with_context(
            flexaudio::core::ErrorContext::new(flexaudio::core::Operation::Join),
        ),
    );
}

/// Every bridge entry point retains panics before signalling completion, even if no join
/// worker can be created. The panic payload is never used as a diagnostic.
fn spawn_bridge_thread<F>(
    chunk: Arc<ChunkTsfn>,
    settle: SettleTsfn,
    phase: Arc<Mutex<StopPhase>>,
    report: FinalReport,
    terminal: TerminalError,
    body: F,
) -> flexaudio::core::Result<JoinHandle<()>>
where
    F: FnOnce() + Send + 'static,
{
    thread::Builder::new()
        .name("flexaudio-napi-bridge".into())
        .spawn(move || {
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)).is_err() {
                record_bridge_failure(&report, &terminal);
            } else {
                let mut report = report.lock().unwrap_or_else(lock_poisoned);
                if report.is_none() {
                    *report = Some(shutdown::ShutdownReport::default());
                }
            }
            post_stop_flushed(&chunk, &settle, &phase);
        })
        .map_err(|_| flexaudio::Error::Backend("bridge thread creation failed".into()))
}

/// Weak reference to the chunk TSFN. `StopFlushed` / settlement use it to unref after resolving
/// (inserted later because the TSFN does not exist at creation time).
///
/// If the callback holds a Strong reference, napi-rs 2.16's `ThreadsafeFunction::clone` only
/// clones the Handle's Arc, creating a cycle: TSFN → callback → slot → TSFN.
/// Finalization (and release of onChunk's `napi_ref`) never occurs.
type ChunkTsfnWeakCell = Arc<OnceLock<Weak<ChunkTsfn>>>;

/// Release the delivery TSFN's event-loop reference. After `stop()` settles, Node can exit
/// even if a stream reference remains. Idempotent (subsequent calls are no-ops).
fn unref_chunk_tsfn(tsfn: &ChunkTsfn, env: &Env) {
    if tsfn.aborted() {
        return;
    }
    let mut tsfn = tsfn.clone();
    let _ = tsfn.unref(env);
}

fn unref_chunk_weak(cell: &OnceLock<Weak<ChunkTsfn>>, env: &Env) {
    let Some(weak) = cell.get() else {
        return;
    };
    let Some(tsfn) = weak.upgrade() else {
        return;
    };
    unref_chunk_tsfn(&tsfn, env);
}

fn make_user_chunk_cb(
    env: &Env,
    on_chunk: &Function<JsAudioChunk, Unknown>,
) -> napi::Result<Arc<UserChunkCb>> {
    let mut refer = ptr::null_mut();
    check_status!(unsafe { sys::napi_create_reference(env.raw(), on_chunk.raw(), 1, &mut refer) })?;
    Ok(UserChunkCb::new(env.raw(), refer))
}

/// TSFN that calls the user's `onChunk`. Chunks go to the user; `StopFlushed` resolves the
/// deferred in the same queue (without calling the user's onChunk). The pump function is a no-op.
/// After resolving, unref the chunk TSFN and release onChunk's `napi_ref`.
fn make_chunk_tsfn(
    env: &Env,
    stop_phase: Arc<Mutex<StopPhase>>,
    user: Arc<UserChunkCb>,
    chunk_weak: ChunkTsfnWeakCell,
    terminal: TerminalError,
    report: FinalReport,
) -> napi::Result<Arc<ChunkTsfn>> {
    let pump =
        env.create_function_from_closure("flexaudioChunkPump", |ctx| ctx.env.get_undefined())?;
    let tsfn =
        pump.create_threadsafe_function::<ChunkEmit, Unknown, _, ErrorStrategy::Fatal>(0, {
            let user = user;
            let chunk_weak = chunk_weak.clone();
            move |ctx: ThreadSafeCallContext<ChunkEmit>| match ctx.value {
                ChunkEmit::Chunk(chunk) => {
                    let denied = matches!(
                        *terminal.lock().unwrap_or_else(lock_poisoned),
                        Some(StreamTerminal::Capture(_))
                    );
                    if denied
                        && !(chunk.frames == 0
                            && (chunk.whisper_vad_events.is_some()
                                || chunk
                                    .secondary
                                    .as_ref()
                                    .is_some_and(|tap| tap.whisper_vad_events.is_some())))
                    {
                        return Ok(Vec::new());
                    }
                    let refer = user.refer();
                    if refer.is_null() {
                        return Ok(Vec::<Unknown>::new());
                    }
                    let mut value = ptr::null_mut();
                    check_status!(unsafe {
                        sys::napi_get_reference_value(ctx.env.raw(), refer, &mut value)
                    })?;
                    let func: Function<JsAudioChunk, Unknown> =
                        unsafe { Function::from_napi_value(ctx.env.raw(), value)? };
                    // Function::call captures and clears a thrown JS exception. Return it
                    // to the TSFN's Fatal handler so Node surfaces uncaughtException.
                    func.call(*chunk)?;
                    Ok(Vec::<Unknown>::new())
                }
                ChunkEmit::WhisperFlushed(deferred, error) => {
                    whisper_integration::settle_flush(&ctx.env, deferred, error)?;
                    Ok(Vec::<Unknown>::new())
                }
                ChunkEmit::WhisperError(error) => {
                    let error = whisper_integration::js_error(&ctx.env, &error)?;
                    ctx.env.throw(error)?;
                    Ok(Vec::<Unknown>::new())
                }
                ChunkEmit::VadError(message) => {
                    ctx.env.throw_error(&message, None)?;
                    Ok(Vec::<Unknown>::new())
                }
                ChunkEmit::StopFlushed => {
                    for deferred in take_stop_waiters(&stop_phase) {
                        settle_stop(ctx.env.raw(), deferred, &report);
                    }
                    // Release the event-loop reference after termination and resolution (ordering contract).
                    unref_chunk_weak(&chunk_weak, &ctx.env);
                    user.release();
                    Ok(Vec::<Unknown>::new())
                }
            }
        })?;
    let tsfn = Arc::new(tsfn);
    let _ = chunk_weak.set(Arc::downgrade(&tsfn));
    Ok(tsfn)
}

/// Dedicated TSFN to return to the JS thread and settle stop()'s Promise.
/// Used when the chunk TSFN is Closing (no onChunk calls remain to deliver).
///
/// Unref from creation. The chunk TSFN holds the loop while alive, keeping settlement deliverable.
/// Even here (when chunks are no longer usable), unref the chunk TSFN after resolving and
/// release onChunk's `napi_ref`.
fn make_settle_tsfn(
    env: &Env,
    stop_phase: Arc<Mutex<StopPhase>>,
    chunk_weak: ChunkTsfnWeakCell,
    user: Arc<UserChunkCb>,
    report: FinalReport,
) -> napi::Result<SettleTsfn> {
    let pump =
        env.create_function_from_closure("flexaudioStopSettle", |ctx| ctx.env.get_undefined())?;
    let mut tsfn = pump.create_threadsafe_function::<(), Unknown, _, ErrorStrategy::Fatal>(0, {
        move |ctx: ThreadSafeCallContext<()>| {
            for deferred in take_stop_waiters(&stop_phase) {
                settle_stop(ctx.env.raw(), deferred, &report);
            }
            unref_chunk_weak(&chunk_weak, &ctx.env);
            user.release();
            Ok(Vec::<Unknown>::new())
        }
    })?;
    tsfn.unref(env)?;
    Ok(tsfn)
}

/// Queue StopFlushed on the chunk TSFN. On failure (Closing / QueueFull, etc.), use the settlement TSFN.
/// If settlement also fails, discard the deferred because the JS thread is unreachable.
///
/// The chunk TSFN uses `max_queue_size=0` (unbounded), so QueueFull is normally impossible. If it
/// occurs, settlement uses a separate queue and could resolve before onChunk. Retry on the same
/// queue in Blocking mode to preserve order; fall back to settlement only if that also fails.
fn post_stop_flushed(chunk: &ChunkTsfn, settle: &SettleTsfn, phase: &Mutex<StopPhase>) {
    let st = chunk.call(
        ChunkEmit::StopFlushed,
        ThreadsafeFunctionCallMode::NonBlocking,
    );
    if st == Status::Ok {
        return;
    }
    if st != Status::Closing {
        // QueueFull, etc.: queue in Blocking mode on the same TSFN, after the queued onChunk calls.
        let st_block = chunk.call(ChunkEmit::StopFlushed, ThreadsafeFunctionCallMode::Blocking);
        if st_block == Status::Ok {
            return;
        }
    }
    let st2 = settle.call((), ThreadsafeFunctionCallMode::NonBlocking);
    if st2 == Status::Ok {
        return;
    }
    if st2 != Status::Closing {
        let st2b = settle.call((), ThreadsafeFunctionCallMode::Blocking);
        if st2b == Status::Ok {
            return;
        }
    }
    // Even the settlement TSFN failed = the JS event loop is no longer running (Node is exiting).
    // Only then may the unresolved deferred be discarded. As long as JS is alive,
    // the settle TSFN calls resolve_undefined on the JS thread.
    let _ = take_stop_waiters(phase);
}

/// Run `flexaudio::processes()` on the libuv thread pool.
pub struct ProcessesTask;

impl Task for ProcessesTask {
    type Output = flexaudio::core::Result<Vec<ProcessInfo>>;
    type JsValue = Vec<JsProcessInfo>;

    fn compute(&mut self) -> napi::Result<Self::Output> {
        Ok(flexaudio::processes())
    }

    fn resolve(&mut self, env: Env, output: Self::Output) -> napi::Result<Self::JsValue> {
        Ok(output
            .map_err(|error| boundary::async_error(&env, error))?
            .into_iter()
            .map(process_info_to_js)
            .collect())
    }
}

/// VadError → napi::Error. Invalid configuration is a caller error: InvalidArg.
/// Model loading/inference failures are environmental: GenericFailure.
fn vad_err(err: VadError) -> NapiError {
    let status = match err {
        VadError::InvalidConfig(_) | VadError::InvalidFormat(_) => Status::InvalidArg,
        _ => Status::GenericFailure,
    };
    NapiError::new(status, err.to_string())
}

/// DenoiseError → napi::Error. Both variants indicate invalid arguments: InvalidArg.
fn denoise_err(err: DenoiseError) -> NapiError {
    NapiError::new(Status::InvalidArg, err.to_string())
}

/// Convert pure encoder diagnostics only at the Node boundary.
fn encode_err(err: flac_encoder::EncoderError) -> NapiError {
    let status = match err.kind {
        flac_encoder::ErrorKind::InvalidArgument => Status::InvalidArg,
        flac_encoder::ErrorKind::Failure => Status::GenericFailure,
    };
    NapiError::new(status, err.reason)
}

// ---------------------------------------------------------------------------
// JS data types (`#[napi(object)]` converts to/from plain JS objects)
// ---------------------------------------------------------------------------

/// JS DeviceInfo. `sourceKind` is a string ("mic"|"system"|"process").
#[napi(object)]
pub struct JsDeviceInfo {
    pub id: String,
    pub name: String,
    pub source_kind: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub is_loopback: bool,
    pub is_default: bool,
}

/// JS ProcessInfo (an element of `processes()`). A candidate for per-process capture.
///
/// Pass `pid` to `openStream({ kind: 'process', processId: pid })` to record that process.
/// `name` / `executable` / `bundleId` are for display (including self-reported app values);
/// the identity key is `pid`.
#[napi(object)]
pub struct JsProcessInfo {
    /// OS process ID (nonzero). Pass to `openStream` as `processId`.
    pub pid: u32,
    /// Display name (always nonempty). OS-reported name → executable name → bundle ID → `"pid <N>"`.
    pub name: String,
    /// Executable basename (e.g. `firefox` / `chrome.exe`). Present only when available.
    pub executable: Option<String>,
    /// macOS bundle ID (e.g. `com.apple.Music`). Present only when available on macOS.
    pub bundle_id: Option<String>,
    /// Whether audio is currently playing. Only when exposed by the OS (Linux=node Running /
    /// Windows=session Active / macOS=IsRunningOutput). `undefined` means unknown.
    pub is_output_active: Option<bool>,
}

/// JS AudioChunk. `data` is interleaved f32 (len = frames * channels).
/// `seq` (u64) uses BigInt to avoid precision loss. `flags` contains ChunkFlags bits (u32).
///
/// Delivery shape (0.3.0): `onChunk` in `openStream(options, onChunk)` receives **one argument**
/// (this primary chunk). Secondary tap (`secondaryOutput`) chunks arrive in the primary chunk's
/// `secondary` property, rather than a second argument. Finalized VAD events also arrive on
/// the selected `vadTap` chunk's `vadEvents`, rather than a separate callback
/// ('primary': `chunk.vadEvents`; 'secondary': `chunk.secondary?.vadEvents`).
///
/// `vadEvents` is populated only when `vad` is specified in `openStream`. When VAD is disabled,
/// it is unset (`undefined`). When enabled with no finalized events in this chunk, it is an empty array.
#[napi(object, object_from_js = false)]
pub struct JsAudioChunk {
    pub data: Float32Array,
    pub frame_index: BigInt,
    pub frames: u32,
    pub pts_ns: i64,
    pub seq: BigInt,
    pub flags: u32,
    pub dropped_before: u32,
    pub peak: f64,
    pub rms: f64,
    /// VAD events finalized in this chunk (only when `vadTap` is 'primary').
    pub vad_events: Option<Vec<JsVadEvent>>,
    /// Attached whisper-compatible events on the selected tap only.
    #[napi(ts_type = "AttachedWhisperVadEvent[]")]
    pub whisper_vad_events: Option<Vec<whisper_vad::JsWhisperVadEvent>>,
    /// Timestamp-matched secondary tap chunk (only with `secondaryOutput`). Delivered as a pair in
    /// the same callback (`primary.secondary` in `onChunk(primary)`, not a second argument). It is
    /// `undefined` when the secondary has not arrived. Match primary↔secondary by `ptsNs` (time);
    /// `seq` is independent for each tap.
    pub secondary: Option<JsSecondaryChunk>,
}

/// JS secondary tap chunk (only with `secondaryOutput`).
///
/// `data` is a typed array matching `encoding` (`Int16Array` for `'s16'`,
/// `Float32Array` for `'f32'`). Sample values use host native endianness. Serialization to s16le
/// wire format is the receiver's (consumer's) responsibility. `ptsNs` uses the same recording-zero
/// clock as the primary but is independent, trailing by 20–60ms due to secondary Stage2 resampler group delay.
#[napi(object, object_from_js = false)]
pub struct JsSecondaryChunk {
    pub data: Either<Int16Array, Float32Array>,
    /// 'f32' | 's16' (discriminator for narrowing the type of `data`).
    pub encoding: String,
    pub frame_index: BigInt,
    pub frames: u32,
    pub pts_ns: i64,
    pub seq: BigInt,
    pub flags: u32,
    pub dropped_before: u32,
    /// Computed from f32 before quantization (preserves meter precision even with s16).
    pub peak: f64,
    pub rms: f64,
    /// VAD events finalized in this chunk (only when `vadTap` is 'secondary').
    pub vad_events: Option<Vec<JsVadEvent>>,
    /// Attached whisper-compatible events on the selected tap only.
    #[napi(ts_type = "AttachedWhisperVadEvent[]")]
    pub whisper_vad_events: Option<Vec<whisper_vad::JsWhisperVadEvent>>,
}

/// JS VAD event (speech segment start/end).
///
/// `type` is limited to `'speechStart' | 'speechEnd'` (consistent with other events' `type`).
///
/// `atSample` is an absolute sample position **at VAD's internal rate (`sampleRate`=8000 or 16000,
/// default 16000)**, not the input chunk's sample rate (raw silero value for standalone/debug use).
/// Convert to seconds with `Number(atSample) / sampleRate`; approximate the input sample position with
/// `atSample * BigInt(inputSampleRate) / BigInt(sampleRate)`.
#[napi(object)]
#[derive(Debug)]
pub struct JsVadEvent {
    /// 'speechStart' | 'speechEnd' (speech segment start/end).
    #[napi(js_name = "type", ts_type = "'speechStart' | 'speechEnd'")]
    pub kind: String,
    pub at_sample: BigInt,
    /// Absolute nanoseconds from recording zero (`number`=f64). Populated only through integrated VAD
    /// (`vad` in `openStream`), computed from chunk `ptsNs` and the within-chunk offset at VAD's
    /// internal rate. Delivered in the same chunk and monotonic non-decreasing across chunks. Final
    /// `flushVad` events use the same `vadEvents` array. Standalone `Vad` (`process`/`flush`) has no pts
    /// context, so this is `undefined`. (Time is bounded by recording length: `number`; u64 counters use `bigint`.)
    pub at_ns: Option<i64>,
}

/// JS native format (returned by `FlexStream.nativeFormat`).
#[napi(object)]
pub struct JsNativeFormat {
    pub sample_rate: u32,
    pub channels: u16,
}

/// Integrated VAD settings (shared by `OpenOptions.vad` and the `Vad` constructor).
///
/// All fields are optional; omitted fields use silero defaults (`VadConfig::default`).
#[napi(object)]
pub struct VadOptions {
    /// Probability threshold for speech start (>=). Default 0.5.
    pub threshold: Option<f64>,
    /// Lower (silence-side) threshold for silence start (<). Defaults to `max(threshold - 0.15, 0.01)`.
    pub neg_threshold: Option<f64>,
    /// Minimum accepted speech duration (ms). Default 250.
    pub min_speech_ms: Option<u32>,
    /// Silence duration required to finalize speech end (ms). Default 100.
    pub min_silence_ms: Option<u32>,
    /// Padding extending segment boundaries on both sides (ms). Default 30.
    pub speech_pad_ms: Option<u32>,
    /// Maximum segment length (ms). 0 = unbounded. Exceeding this forces a split.
    ///
    /// Standalone `Vad` follows silero with default 0 (unbounded). **Integrated VAD (`vad` in `openStream`)
    /// defaults to 30000ms (caps long stretches of speech so realtime latency stays bounded)**.
    /// An explicit value (including `0`) always wins.
    pub max_speech_ms: Option<u32>,
    /// VAD internal sample rate. Only 8000 or 16000. Default 16000.
    pub sample_rate: Option<u32>,
}

/// Stream notification. permissionDenied is terminal, with permission and
/// cause/remedy in message. permissionPending is advisory: consent remains undecided,
/// capture continues and may stay silent until granted. silenceWhileSourceActive
/// is advisory: the system-audio diagnosis is inconclusive and capture continues.
/// Missing permission and genuine digital silence remain possible in that case.
#[napi(object)]
pub struct JsStreamEvent {
    /// Present on permissionDenied and permissionPending: microphone | systemAudio.
    #[napi(ts_type = "'microphone' | 'systemAudio'")]
    pub permission: Option<String>,
    #[napi(js_name = "type")]
    pub kind: String,
    pub count: Option<BigInt>,
    pub error: Option<JsAudioError>,
    pub loss: Option<JsAudioLoss>,
    pub message: Option<String>,
}

/// JS device event. `type` identifies the kind; device/id/sourceKind are optional.
#[napi(object)]
pub struct JsDeviceEvent {
    #[napi(js_name = "type")]
    pub kind: String,
    pub device: Option<JsDeviceInfo>,
    pub id: Option<String>,
    pub source_kind: Option<String>,
    pub dropped_events: Option<BigInt>,
    pub message: Option<String>,
}

/// Secondary output tap specification (`OpenOptions.secondaryOutput`).
///
/// Returns the same capture as the primary output (`outputRate`/`outputChannels`) in another format simultaneously.
/// Used for paired outputs such as 48k/stereo storage + 16k/mono/s16 recognition.
#[napi(object)]
pub struct SecondaryOutputOptions {
    /// Secondary output sample rate (Hz). Example: 16000.
    pub rate: u32,
    /// Secondary output channel count (1=mono / 2=stereo). Example: 1.
    pub channels: u16,
    /// Secondary chunk sample encoding. 'f32' (default) | 's16'. s16 is quantized after VAD
    /// and returned as `Int16Array` (native-endian values).
    pub encoding: Option<String>,
}

/// Options for openStream / __openMockStream.
#[napi(object)]
pub struct OpenOptions {
    /// "mic" | "system" | "process" | "mix"
    pub kind: String,
    pub device_id: Option<String>,
    /// Target process ID for `process` capture. Must be a finite positive integer
    /// in 1..=4294967295; invalid values fail with InvalidArg.
    pub process_id: Option<f64>,
    /// How to handle the target PID for process capture (process only). "include" (default) | "exclude".
    /// include=record only the target PID / exclude=all system audio except the target PID (process_id required).
    /// Ignored for mic / system. Supported on all three OSes: Linux / Windows / macOS.
    pub mode: Option<String>,
    /// Exclude the host process from `system` capture (also the system side of
    /// `mix`). Defaults to false; ignored by mic/process. Supported on Linux,
    /// Windows and macOS. Windows excludes the host's entire process tree;
    /// Electron's audio utility process is a direct child and is covered.
    pub exclude_self: Option<bool>,
    /// Process IDs whose playback is excluded from `system` capture (also the
    /// system side of `mix`), in addition to `excludeSelf`. Each must be a finite
    /// positive integer in 1..=4294967295; invalid values fail with InvalidArg.
    /// On macOS, each PID must also fit a positive signed 32-bit integer
    /// (1..=2147483647), or capture fails.
    /// Duplicates are allowed. Ignored by mic/process after validation.
    /// Linux and macOS exclude every listed PID. macOS resolves audio objects
    /// at capture start: a PID without one is not excluded; a failed lookup
    /// fails the open unless the process is gone.
    /// On Linux, while exclusion is active, a pipewire-pulse-relayed stream
    /// without a known application.process.id is not captured.
    /// Windows can exclude only one process tree. For Electron, use
    /// `excludeSelf: true` alone; any other PID fails with an error.
    pub exclude_pids: Option<Vec<f64>>,
    /// Default 48000
    pub output_rate: Option<u32>,
    /// Default 2
    pub output_channels: Option<u16>,
    /// Default 20
    pub chunk_ms: Option<f64>,
    /// Initial input gain (linear multiplier). Default 1.0. 1.0=unchanged, 2.0=about +6dB, 0.0=silence.
    /// Use `setGain` for runtime changes.
    pub gain: Option<f64>,
    /// Input device ID for the mic side of mix (mix only). Defaults to the default input.
    pub mic_device_id: Option<String>,
    /// Output endpoint ID for the system side of mix (mix only). Defaults to the default output.
    pub system_device_id: Option<String>,
    /// Pre-mix multiplier for the mic side (linear, mix only). Default 1.0. `gain` applies after mixing.
    pub mic_gain: Option<f64>,
    /// Pre-mix multiplier for the system side (linear, mix only). Default 1.0.
    pub system_gain: Option<f64>,
    /// Integrated VAD settings. When specified, the tap selected by `vadTap` passes through VAD and
    /// finalized events attach to that tap's chunk `vadEvents` (audio is unchanged). Omission disables VAD.
    pub vad: Option<VadOptions>,
    /// Whisper-compatible VAD options. Validated before any device is opened.
    /// Requires the shared canonical capture feed with exact producer provenance.
    #[napi(ts_type = "WhisperVadStreamOptions")]
    pub whisper_vad: Option<napi::JsUnknown>,
    /// Tap to run VAD on. 'primary' (default) | 'secondary'. 'secondary' requires
    /// `secondaryOutput`; a 16k/mono secondary avoids resampling for efficiency.
    pub vad_tap: Option<String>,
    /// true enables recording-time noise suppression. **Only available with 48000 Hz output**
    /// (RNNoise is fixed at 48kHz). Applied once to the internal 48kHz/stereo canonical form;
    /// both primary and secondary taps receive denoised audio (+10ms fixed latency). Setting true
    /// at any other rate makes `openStream` throw InvalidArg. Omission/false disables noise suppression.
    pub denoise: Option<bool>,
    /// Secondary output tap. When specified, returns paired chunks in another format simultaneously
    /// (`primary.secondary` in `onChunk`). Omission means no secondary tap, preserving previous behavior.
    pub secondary_output: Option<SecondaryOutputOptions>,
}

// ---------------------------------------------------------------------------
// Conversion helpers
// ---------------------------------------------------------------------------

fn source_kind_str(k: SourceKind) -> String {
    match k {
        SourceKind::Mic => "mic",
        SourceKind::SystemLoopback => "system",
        SourceKind::ProcessLoopback => "process",
        SourceKind::Mix => "mix",
    }
    .to_string()
}

fn parse_source_kind(s: &str) -> napi::Result<SourceKind> {
    match s {
        "mic" => Ok(SourceKind::Mic),
        "system" => Ok(SourceKind::SystemLoopback),
        "process" => Ok(SourceKind::ProcessLoopback),
        "mix" => Ok(SourceKind::Mix),
        other => Err(NapiError::new(
            Status::InvalidArg,
            format!("unknown kind: {other:?} (expected mic|system|process|mix)"),
        )),
    }
}

/// Convert "include" | "exclude" to [`ProcessMode`] (process only). `None`/omission defaults to Include.
fn parse_process_mode(s: Option<&str>) -> napi::Result<ProcessMode> {
    match s {
        None | Some("include") => Ok(ProcessMode::Include),
        Some("exclude") => Ok(ProcessMode::Exclude),
        Some(other) => Err(NapiError::new(
            Status::InvalidArg,
            format!("unknown mode: {other:?} (expected include|exclude)"),
        )),
    }
}

fn device_info_to_js(info: DeviceInfo) -> JsDeviceInfo {
    JsDeviceInfo {
        id: info.id,
        name: info.name,
        source_kind: source_kind_str(info.source_kind),
        sample_rate: info.sample_rate,
        channels: info.channels,
        is_loopback: info.is_loopback,
        is_default: info.is_default,
    }
}

fn process_info_to_js(info: ProcessInfo) -> JsProcessInfo {
    JsProcessInfo {
        pid: info.pid,
        name: info.name,
        executable: info.executable,
        bundle_id: info.bundle_id,
        is_output_active: info.is_output_active,
    }
}

fn chunk_to_js(chunk: AudioChunk) -> JsAudioChunk {
    let frames = chunk.frames as u32;
    JsAudioChunk {
        // Convert Vec<f32> to Float32Array (leave no ownership on the thread).
        frame_index: BigInt::from(chunk.frame_index),
        data: Float32Array::new(chunk.data),
        frames,
        pts_ns: chunk.pts_ns,
        seq: BigInt::from(chunk.seq),
        flags: chunk.flags.bits(),
        dropped_before: chunk.dropped_before,
        peak: chunk.peak as f64,
        rms: chunk.rms as f64,
        // Unset by default. The bridge overwrites it when integrated VAD is enabled on the primary tap.
        vad_events: None,
        whisper_vad_events: None,
        // The pairing bridge inserts a timestamp-matched secondary chunk (undefined if none).
        secondary: None,
    }
}

fn vad_event_to_js(ev: VadEvent) -> JsVadEvent {
    vad_event_to_js_abs(ev, None)
}

/// Map [`VadEvent`] to JS. `at_ns` is absolute time from recording zero (integrated VAD only;
/// `None` for standalone `Vad`). `at_sample` is the raw cumulative position at VAD's internal rate.
fn vad_event_to_js_abs(ev: VadEvent, at_ns: Option<i64>) -> JsVadEvent {
    let (kind, at_sample) = match ev {
        VadEvent::SpeechStart { at_sample } => ("speechStart", BigInt::from(at_sample)),
        VadEvent::SpeechEnd { at_sample } => ("speechEnd", BigInt::from(at_sample)),
    };
    JsVadEvent {
        kind: kind.to_string(),
        at_sample,
        at_ns,
    }
}

/// Convert 'primary' | 'secondary' to [`VadTap`]. `None`/omission defaults to Primary.
fn parse_vad_tap(s: Option<&str>) -> napi::Result<VadTap> {
    match s {
        None | Some("primary") => Ok(VadTap::Primary),
        Some("secondary") => Ok(VadTap::Secondary),
        Some(other) => Err(NapiError::new(
            Status::InvalidArg,
            format!("unknown vadTap: {other:?} (expected primary|secondary)"),
        )),
    }
}

/// Convert 'f32' | 's16' to [`SecEncoding`]. `None`/omission defaults to F32.
fn parse_sec_encoding(s: Option<&str>) -> napi::Result<SecEncoding> {
    match s {
        None | Some("f32") => Ok(SecEncoding::F32),
        Some("s16") => Ok(SecEncoding::S16),
        Some(other) => Err(NapiError::new(
            Status::InvalidArg,
            format!("unknown secondaryOutput.encoding: {other:?} (expected f32|s16)"),
        )),
    }
}

/// [`VadOptions`] → [`VadConfig`]. Omitted fields fall back to silero defaults.
/// Omitted `neg_threshold` stays `None` (the default formula in `VadConfig` applies).
fn build_vad_config(o: &VadOptions) -> VadConfig {
    let d = VadConfig::default();
    VadConfig {
        threshold: o.threshold.map(|v| v as f32).unwrap_or(d.threshold),
        neg_threshold: o.neg_threshold.map(|v| v as f32),
        min_speech_ms: o.min_speech_ms.unwrap_or(d.min_speech_ms),
        min_silence_ms: o.min_silence_ms.unwrap_or(d.min_silence_ms),
        speech_pad_ms: o.speech_pad_ms.unwrap_or(d.speech_pad_ms),
        max_speech_ms: o.max_speech_ms.unwrap_or(d.max_speech_ms),
        sample_rate: o.sample_rate.unwrap_or(d.sample_rate),
    }
}

/// [`VadOptions`] → [`VadConfig`] for the integrated `openStream` path.
///
/// Identical to [`build_vad_config`] except that an unset `maxSpeechMs` defaults
/// to [`INTEGRATED_VAD_MAX_SPEECH_MS_DEFAULT`] (30 s) instead of silero's 0
/// (unbounded), bounding real-time latency for a monologue with no silence. An
/// explicit value — including `0` to restore unbounded behavior — always wins.
/// The standalone `Vad` class stays silero-faithful and never applies this.
fn build_integrated_vad_config(o: &VadOptions) -> VadConfig {
    let mut cfg = build_vad_config(o);
    if o.max_speech_ms.is_none() {
        cfg.max_speech_ms = INTEGRATED_VAD_MAX_SPEECH_MS_DEFAULT;
    }
    cfg
}

/// Validate integrated denoise's 48kHz requirement (pure function, separated for testing).
///
/// RNNoise is fixed at 48kHz; when `enabled` and the output rate is not 48000,
/// return InvalidArg. `open_stream` rejects this before opening the stream.
fn check_denoise_rate(enabled: bool, output_rate: u32) -> napi::Result<()> {
    if enabled && output_rate != 48_000 {
        return Err(NapiError::new(
            Status::InvalidArg,
            format!(
                "denoise supports only 48000 Hz output (RNNoise is fixed at 48kHz). \
                 Cannot use outputRate={output_rate}"
            ),
        ));
    }
    Ok(())
}

/// Validate a JavaScript PID before converting it to the core's integer type.
fn parse_pid(field: &str, value: f64) -> napi::Result<u32> {
    if !value.is_finite() || value.fract() != 0.0 || value < 1.0 || value > f64::from(u32::MAX) {
        return Err(NapiError::new(
            Status::InvalidArg,
            format!("{field} must be a finite positive integer in 1..=4294967295, got {value}"),
        ));
    }
    // The checks above guarantee an exact conversion with no truncation.
    Ok(value as u32)
}

fn build_config(options: &OpenOptions) -> napi::Result<StreamConfig> {
    let kind = parse_source_kind(&options.kind)?;
    let mode = parse_process_mode(options.mode.as_deref())?;
    let target_pid = options
        .process_id
        .map(|value| parse_pid("processId", value))
        .transpose()?;
    let exclude_pids = options
        .exclude_pids
        .as_deref()
        .unwrap_or_default()
        .iter()
        .enumerate()
        .map(|(index, &value)| parse_pid(&format!("excludePids[{index}]"), value))
        .collect::<napi::Result<Vec<u32>>>()?;
    let output = OutputFormat {
        sample_rate: options.output_rate.unwrap_or(48_000),
        channels: options.output_channels.unwrap_or(2),
    };
    // Secondary tap (only when configured). Encoding is interpreted by binding-layer marshaling,
    // so core StreamConfig carries only rate/channels (core always uses f32).
    let secondary_output = options.secondary_output.as_ref().map(|s| OutputFormat {
        sample_rate: s.rate,
        channels: s.channels,
    });
    let mut config = StreamConfig {
        kind,
        output,
        secondary_output,
        device_id: options.device_id.clone(),
        target_pid,
        // The facade checks source-specific mode and exclusion semantics.
        mode,
        exclude_self: options.exclude_self.unwrap_or(false),
        exclude_pids,
        gain: options.gain.unwrap_or(1.0) as f32,
        // mix only (the facade ignores these for mic/system/process). Per-side gains default to 1.0.
        mix_mic_device_id: options.mic_device_id.clone(),
        mix_system_device_id: options.system_device_id.clone(),
        mix_mic_gain: options.mic_gain.unwrap_or(1.0) as f32,
        mix_system_gain: options.system_gain.unwrap_or(1.0) as f32,
        ..Default::default()
    };
    if let Some(ms) = options.chunk_ms {
        if ms != 20.0 {
            return Err(to_napi_err(flexaudio::Error::InvalidArg(
                "chunkMs must be 20".into(),
            )));
        }
        config.chunk_ms = 20;
    }
    Ok(config)
}

// ---------------------------------------------------------------------------
// FlexStream (class). Owns and stops the bridge thread.
// ---------------------------------------------------------------------------

/// Command requesting a source switch on the bridge thread.
///
/// The bridge thread owns Stream, so `switch_source` cannot be called directly. Send JS
/// switch requests to the bridge thread through this command and synchronously receive the
/// result through `result_tx` (JS expects a synchronous return).
struct SwitchCmd {
    config: StreamConfig,
    result_tx: mpsc::Sender<flexaudio::core::Result<()>>,
}

/// Command requesting current stream values from the bridge thread.
///
/// `is_paused` / `gain` / `native_format` / `dropped_chunks` are all Stream methods,
/// and the bridge thread owns Stream, preventing direct reads. Retrieve a combined
/// [`StreamSnapshot`] in one query; each getter extracts only its required field.
struct QueryCmd {
    result_tx: mpsc::Sender<StreamSnapshot>,
}

/// Snapshot of current stream values read by the bridge thread.
struct StreamSnapshot {
    is_paused: bool,
    gain: f32,
    native_sample_rate: u32,
    native_channels: u16,
    dropped_chunks: u64,
}

/// Commands sent to the bridge thread. Only that thread accesses Stream, so all JS
/// operations are requested through this channel.
enum BridgeCmd {
    /// Hot-swap the input source (return the result synchronously).
    Switch(SwitchCmd),
    /// Pause delivery.
    Pause,
    /// Resume delivery.
    Resume(mpsc::Sender<flexaudio::Result<()>>),
    /// Change input gain (linear multiplier). napi validates the value before sending.
    SetGain(f32),
    /// Return a current-value snapshot synchronously (for getters).
    Query(QueryCmd),
    /// Force-finalize integrated VAD's open utterance (`flushVad`). This runtime operation does not
    /// change config (`secondaryOutput` / encoding are fixed at open). Unlike audio
    /// stop-flush, it attaches the final speechEnd to the next tap chunk.
    FlushVad,
    FlushWhisperVad(SendDeferred),
}

/// Secondary tap marshaling configuration (rate/channels/encoding).
#[derive(Clone, Copy)]
struct SecondaryTapCfg {
    rate: u32,
    channels: u16,
    encoding: SecEncoding,
}

/// Bridge thread emission state. Converts primary/secondary chunks to JS, runs integrated VAD on
/// the selected tap, pairs within the pts window (60ms), and delivers to `onChunk`.
///
/// denoise has moved to core (internal canonical form) and is absent here. A single VAD instance
/// is bound to one tap (`vad_tap`), consuming pre-quantization f32 in Rust. Secondary s16 conversion follows VAD.
struct PairingBridge {
    on_chunk: ChunkTsfn,
    whisper: Option<whisper_integration::WhisperBridge>,
    stop_phase: Arc<Mutex<StopPhase>>,
    report: FinalReport,
    /// Integrated VAD (only when configured). One instance, one tap.
    vad: Option<vad_integration::IntegratedVad>,
    vad_tap: VadTap,
    /// Latest successful VAD anchor for trailing carriers.
    vad_anchor_pts: i64,
    /// VAD events finalized by runtime `flushVad`, awaiting a chunk to carry them.
    /// Prepended to `vadEvents` of the next VAD tap chunk queued in the FIFO.
    pending_flush_events: Vec<JsVadEvent>,
    /// Primary tap output format (passed to `process_pcm` when VAD uses primary).
    output_rate: u32,
    output_channels: u16,
    /// Secondary tap format/encoding (only when configured).
    secondary: Option<SecondaryTapCfg>,
    /// Pairing FIFOs. Drain both taps completely each iteration, then match within the pts window.
    primary_fifo: VecDeque<JsAudioChunk>,
    secondary_fifo: VecDeque<JsSecondaryChunk>,
    /// `ptsNs` of the last primary chunk delivered to `onChunk`. Clamp the final stop-flush carrier's
    /// pts to at least this value, preserving the non-decreasing primary pts contract.
    last_emitted_primary_pts: i64,
    last_primary_seq: u64,
    last_primary_frame_index: u64,
    last_primary_dropped: u32,
}

impl PairingBridge {
    fn report_vad_error(&self, message: String) {
        self.on_chunk.call(
            ChunkEmit::VadError(message),
            ThreadsafeFunctionCallMode::NonBlocking,
        );
    }

    /// Feed the bound tap's pre-quantization f32 to VAD and return finalized events with absolute time
    /// from recording zero. Reset and re-anchor on discontinuity flags / `dropped_before` increments.
    /// Returns `None` when VAD is not configured.
    fn run_vad(
        &mut self,
        samples: &[f32],
        in_rate: u32,
        in_channels: u16,
        pts_ns: i64,
        discontinuity: bool,
        dropped_before: u32,
    ) -> Option<Vec<JsVadEvent>> {
        let vad = self.vad.as_mut()?;
        match vad.process(
            samples,
            in_rate,
            in_channels,
            pts_ns,
            discontinuity,
            dropped_before,
        ) {
            Ok(events) => {
                self.vad_anchor_pts = vad.anchor_pts;
                Some(events)
            }
            Err(error) => {
                self.report_vad_error(error.to_string());
                None
            }
        }
    }

    /// Take pending flushVad events and prepend them to this chunk's VAD events (if any).
    /// Flush events finalize the previous utterance, so their timestamps precede new events
    /// from this chunk and they belong first.
    fn take_pending_prepended(&mut self, own: Option<Vec<JsVadEvent>>) -> Vec<JsVadEvent> {
        let mut merged = std::mem::take(&mut self.pending_flush_events);
        if let Some(ev) = own {
            merged.extend(ev);
        }
        merged
    }

    /// Force-finalize integrated VAD's open utterance and return finalized JS events with `atNs`
    /// (absolute time based on the latest anchor). `flush()` resets VAD, so reset the cumulative
    /// counter to 0. Empty if no utterance is open.
    fn flush_vad_events(&mut self) -> Result<Vec<JsVadEvent>, VadError> {
        match self.vad.as_mut() {
            Some(vad) => vad.flush(),
            None => Ok(Vec::new()),
        }
    }

    /// Runtime finalization uses the next selected tap chunk as its carrier.
    fn flush_vad(&mut self) {
        match self.flush_vad_events() {
            Ok(events) => self.pending_flush_events.extend(events),
            Err(error) => self.report_vad_error(error.to_string()),
        }
    }

    /// Deliver all residual secondaries, then one final zero-frame terminator.
    fn flush_vad_final(&mut self) {
        while let Some(secondary) = self.secondary_fifo.pop_front() {
            self.emit_secondary_envelope(secondary);
        }
        let js = match self.flush_vad_events() {
            Ok(events) => events,
            Err(error) => {
                record_cleanup_failure(
                    &self.report,
                    flexaudio::Error::Backend("integrated VAD flush failed".into()).with_context(
                        flexaudio::core::ErrorContext::new(flexaudio::core::Operation::Flush),
                    ),
                );
                self.report_vad_error(error.to_string());
                Vec::new()
            }
        };
        // Prepend any runtime pending events (chronological order).
        let mut events = std::mem::take(&mut self.pending_flush_events);
        events.extend(js);
        // Preserve non-decreasing primary pts: carrier pts is the greater of the latest anchor and last delivered pts.
        let pts = self.vad_anchor_pts.max(self.last_emitted_primary_pts);
        let mut carrier = JsAudioChunk {
            frame_index: BigInt::from(self.last_primary_frame_index),
            data: Float32Array::new(Vec::new()),
            frames: 0,
            pts_ns: pts,
            seq: BigInt::from(self.last_primary_seq),
            flags: 0,
            dropped_before: self.last_primary_dropped,
            peak: 0.0,
            rms: 0.0,
            vad_events: None,
            whisper_vad_events: None,
            secondary: None,
        };
        match self.vad_tap {
            VadTap::Primary => carrier.vad_events = Some(events),
            VadTap::Secondary => {
                // Place secondary tap events on the secondary chunk (the consumer reads through `primary.secondary`).
                // Match the configured encoding (samples are empty).
                let (data, encoding) = match self.secondary.map(|c| c.encoding) {
                    Some(SecEncoding::S16) => (Either::A(Int16Array::new(Vec::new())), "s16"),
                    _ => (Either::B(Float32Array::new(Vec::new())), "f32"),
                };
                carrier.secondary = Some(JsSecondaryChunk {
                    frame_index: BigInt::from(0u64),
                    data,
                    encoding: encoding.to_string(),
                    frames: 0,
                    pts_ns: pts,
                    seq: BigInt::from(0u64),
                    flags: 0,
                    dropped_before: 0,
                    peak: 0.0,
                    rms: 0.0,
                    vad_events: Some(events),
                    whisper_vad_events: None,
                });
            }
        }
        self.on_chunk.call(
            ChunkEmit::Chunk(Box::new(carrier)),
            ThreadsafeFunctionCallMode::NonBlocking,
        );
    }

    /// Accept a primary chunk. Run VAD if primary is selected, convert to JS, and queue in the FIFO.
    fn on_primary(&mut self, chunk: AudioChunk) {
        self.last_primary_seq = chunk.seq;
        self.last_primary_frame_index = chunk.frame_index;
        self.last_primary_dropped = chunk.dropped_before;
        let (rate, ch) = (self.output_rate, self.output_channels);
        let mut vad_events = if self.vad_tap == VadTap::Primary {
            let disc = chunk.flags.contains(ChunkFlags::DISCONTINUITY);
            self.run_vad(
                &chunk.data,
                rate,
                ch,
                chunk.pts_ns,
                disc,
                chunk.dropped_before,
            )
        } else {
            None
        };
        // Prepend pending flushVad events (such as the previous utterance's final speechEnd).
        if self.vad_tap == VadTap::Primary && !self.pending_flush_events.is_empty() {
            vad_events = Some(self.take_pending_prepended(vad_events));
        }
        let mut js = chunk_to_js(chunk);
        js.vad_events = vad_events;
        if let Some(whisper) = self.whisper.as_mut() {
            if whisper.tap == VadTap::Primary {
                js.whisper_vad_events = Some(whisper.take_events());
            }
        }
        self.primary_fifo.push_back(js);
    }

    /// Accept a secondary chunk. Run VAD on pre-quantization f32 if secondary is selected, then marshal
    /// to `Int16Array`/`Float32Array` according to encoding and queue in the FIFO.
    fn on_secondary(&mut self, chunk: SecondaryChunk) {
        let Some(cfg) = self.secondary else {
            return; // Do nothing without secondary tap settings (defensive).
        };
        let mut vad_events = if self.vad_tap == VadTap::Secondary {
            let disc = chunk.flags.contains(ChunkFlags::DISCONTINUITY);
            self.run_vad(
                &chunk.samples,
                cfg.rate,
                cfg.channels,
                chunk.pts_ns,
                disc,
                chunk.dropped_before,
            )
        } else {
            None
        };
        // Prepend pending flushVad events (only when the VAD tap is secondary).
        if self.vad_tap == VadTap::Secondary && !self.pending_flush_events.is_empty() {
            vad_events = Some(self.take_pending_prepended(vad_events));
        }
        // Save metadata before consuming samples.
        let frames = chunk.frames as u32;
        let pts_ns = chunk.pts_ns;
        let seq = BigInt::from(chunk.seq);
        let flags = chunk.flags.bits();
        let dropped_before = chunk.dropped_before;
        let peak = chunk.peak as f64; // Already computed by core from pre-quantization f32.
        let rms = chunk.rms as f64;
        let (data, encoding) = match cfg.encoding {
            SecEncoding::S16 => {
                // Quantize to s16 after VAD (canonical quantize_i16 shared across layers; native endianness).
                let q: Vec<i16> = chunk
                    .samples
                    .iter()
                    .map(|&x| flexaudio::core::quantize_i16(x))
                    .collect();
                (Either::A(Int16Array::new(q)), "s16".to_string())
            }
            SecEncoding::F32 => (
                Either::B(Float32Array::new(chunk.samples)),
                "f32".to_string(),
            ),
        };
        let js = JsSecondaryChunk {
            frame_index: BigInt::from(chunk.frame_index),
            data,
            encoding,
            frames,
            pts_ns,
            seq,
            flags,
            dropped_before,
            peak,
            rms,
            vad_events,
            whisper_vad_events: self
                .whisper
                .as_mut()
                .filter(|owner| owner.tap == VadTap::Secondary)
                .map(|_| Vec::new()),
        };
        self.secondary_fifo.push_back(js);
    }

    fn emit_secondary_envelope(&mut self, mut secondary: JsSecondaryChunk) {
        if let Some(whisper) = self
            .whisper
            .as_mut()
            .filter(|owner| owner.tap == VadTap::Secondary)
        {
            secondary.whisper_vad_events = Some(whisper.take_events());
        }
        let carrier = JsAudioChunk {
            data: Float32Array::new(Vec::new()),
            frames: 0,
            frame_index: BigInt::from(self.last_primary_frame_index),
            pts_ns: secondary.pts_ns.max(self.last_emitted_primary_pts),
            seq: BigInt::from(self.last_primary_seq),
            flags: 0,
            dropped_before: self.last_primary_dropped,
            peak: 0.0,
            rms: 0.0,
            vad_events: None,
            whisper_vad_events: None,
            secondary: Some(secondary),
        };
        self.last_emitted_primary_pts = carrier.pts_ns;
        self.on_chunk.call(
            ChunkEmit::Chunk(Box::new(carrier)),
            ThreadsafeFunctionCallMode::NonBlocking,
        );
    }

    /// Match primary↔secondary within the pts window and call `onChunk(primary)` with `primary.secondary`.
    ///
    /// Rules: for primary `P`, inspect secondary FIFO head `S`. Discard overly old secondaries (orphans);
    /// pop and pair those within the window; if absent/newer than the window, deliver primary only with
    /// `secondary=undefined` (keep secondary for the next primary). The pts window avoids permanent drift (the 1:1 zip flaw).
    fn drain_pairs(&mut self) {
        while let Some(front) = self.primary_fifo.front() {
            let p_pts = front.pts_ns;
            let matched = loop {
                match self.secondary_fifo.front() {
                    None => break None,
                    Some(s) => {
                        if s.pts_ns < p_pts - PAIR_WINDOW_NS / 2 {
                            // Secondary too old (e.g. primary dropped): discard the orphan and inspect the next secondary.
                            let orphan = self.secondary_fifo.pop_front().unwrap();
                            self.emit_secondary_envelope(orphan);
                            continue;
                        } else if s.pts_ns < p_pts + CHUNK_SPAN_NS + PAIR_WINDOW_NS / 2 {
                            // Within the window: match.
                            break self.secondary_fifo.pop_front();
                        } else {
                            // Secondary has not arrived (newer than the window): deliver primary only, retain secondary.
                            break None;
                        }
                    }
                }
            };
            let mut p = self.primary_fifo.pop_front().unwrap();
            self.last_emitted_primary_pts = p.pts_ns.max(self.last_emitted_primary_pts);
            let mut matched = matched;
            if let Some(secondary) = matched.as_mut() {
                if let Some(whisper) = self
                    .whisper
                    .as_mut()
                    .filter(|owner| owner.tap == VadTap::Secondary)
                {
                    secondary.whisper_vad_events = Some(whisper.take_events());
                }
            }
            p.secondary = matched;
            self.on_chunk.call(
                ChunkEmit::Chunk(Box::new(p)),
                ThreadsafeFunctionCallMode::NonBlocking,
            );
        }
    }
}

/// Recording stream handle. Internally, the bridge thread owns and polls `flexaudio::Stream`
/// and sends chunks/events to JS through TSFN.
#[napi]
pub struct FlexStream {
    stop_flag: Arc<AtomicBool>,
    inner: Arc<Mutex<StreamInner>>,
    stop_phase: Arc<Mutex<StopPhase>>,
    chunk_tsfn: Arc<ChunkTsfn>,
    settle_tsfn: SettleTsfn,
    terminal: TerminalError,
    report: FinalReport,
}

impl FlexStream {
    /// Accept an already `start()`ed Stream and the emitting [`PairingBridge`], then spawn a bridge
    /// thread. Stream is Send, so move it to the thread (poll_* takes
    /// &mut self, requiring thread ownership). The bridge holds integrated VAD / secondary tap settings.
    fn spawn(
        env: &Env,
        stream: flexaudio::Stream,
        bridge: PairingBridge,
        on_event: Option<EventTsfn>,
        chunk_tsfn: Arc<ChunkTsfn>,
        settle_tsfn: SettleTsfn,
        terminal: TerminalError,
    ) -> napi::Result<Self> {
        let report = bridge.report.clone();
        let stop_flag = Arc::new(AtomicBool::new(false));
        let thread_stop = stop_flag.clone();
        let thread_terminal = terminal.clone();
        let stop_phase = bridge.stop_phase.clone();
        let (cmd_tx, cmd_rx) = mpsc::channel::<BridgeCmd>();

        let resources = Arc::new(Mutex::new(Some((stream, bridge, on_event))));
        let rollback_resources = resources.clone();
        let handle = spawn_bridge_thread(
            chunk_tsfn.clone(),
            settle_tsfn.clone(),
            stop_phase.clone(),
            report.clone(),
            terminal.clone(),
            move || {
                let (mut stream, mut bridge, on_event) = resources
                    .lock()
                    .unwrap_or_else(lock_poisoned)
                    .take()
                    .expect("bridge owns acquired resources");
                loop {
                    // Process commands together in the same iteration as polling.
                    while let Ok(cmd) = cmd_rx.try_recv() {
                        match cmd {
                            BridgeCmd::Switch(sw) => {
                                let r = stream.switch_source(sw.config);
                                // Ignore a dropped receiver (switch_source caller).
                                let _ = sw.result_tx.send(r);
                            }
                            BridgeCmd::Pause => stream.pause(),
                            BridgeCmd::Resume(result_tx) => {
                                let _ = result_tx.send(stream.resume());
                            }
                            BridgeCmd::SetGain(g) => {
                                // napi validates before sending, so Err is not expected.
                                // Even an unexpected Err does not become an event (the result is discarded).
                                let _ = stream.set_gain(g);
                            }
                            BridgeCmd::Query(q) => {
                                let (native_sample_rate, native_channels) = stream.native_format();
                                let snap = StreamSnapshot {
                                    is_paused: stream.is_paused(),
                                    gain: stream.gain(),
                                    native_sample_rate,
                                    native_channels,
                                    dropped_chunks: stream.dropped_chunks(),
                                };
                                // Ignore a dropped receiver.
                                let _ = q.result_tx.send(snap);
                            }
                            // Runtime flushVad: finalize the open utterance and attach final events to the next
                            // VAD tap chunk (pending).
                            BridgeCmd::FlushVad => bridge.flush_vad(),
                            BridgeCmd::FlushWhisperVad(deferred) => {
                                bridge.drain_capture(&mut stream);
                                while let Some(chunk) = stream.poll_chunk() {
                                    bridge.on_primary(chunk);
                                }
                                while let Some(chunk) = stream.poll_secondary() {
                                    bridge.on_secondary(chunk);
                                }
                                bridge.drain_pairs();
                                bridge.flush_whisper(false);
                                let error = bridge
                                    .whisper
                                    .as_ref()
                                    .and_then(|owner| owner.error.clone());
                                if matches!(
                                    *thread_terminal.lock().unwrap_or_else(lock_poisoned),
                                    Some(StreamTerminal::Whisper(_))
                                ) {
                                    *thread_terminal.lock().unwrap_or_else(lock_poisoned) =
                                        error.clone().map(StreamTerminal::Whisper);
                                }
                                bridge.on_chunk.call(
                                    ChunkEmit::WhisperFlushed(deferred, error),
                                    ThreadsafeFunctionCallMode::NonBlocking,
                                );
                            }
                        }
                    }
                    // A stop request may race an already accepted flush command.
                    // Drain accepted commands before exiting so their promises settle.
                    if thread_stop.load(Ordering::SeqCst) {
                        break;
                    }
                    if let Some(error) = stream.terminal_error() {
                        *thread_terminal.lock().unwrap_or_else(lock_poisoned) =
                            Some(StreamTerminal::Capture(error));
                        bridge.primary_fifo.clear();
                        bridge.secondary_fifo.clear();
                        bridge.pending_flush_events.clear();
                    }
                    // Drain all arriving primary/secondary chunks into the bridge. Run VAD/quantization, pair within
                    // the pts window, and deliver to onChunk.
                    bridge.drain_capture(&mut stream);
                    while let Some(chunk) = stream.poll_chunk() {
                        bridge.on_primary(chunk);
                    }
                    while let Some(chunk) = stream.poll_secondary() {
                        bridge.on_secondary(chunk);
                    }
                    bridge.drain_pairs();
                    bridge.report_whisper_failure();
                    if let Some(error) = bridge
                        .whisper
                        .as_ref()
                        .and_then(|owner| owner.error.clone())
                    {
                        retain_whisper_failure(&thread_terminal, error);
                    }
                    forward_stream_events(&mut stream, on_event.as_ref(), &thread_terminal);
                    thread::sleep(POLL_INTERVAL);
                }
                // Drain chunks remaining in the rings before stopping, then deliver pairs.
                bridge.drain_capture(&mut stream);
                while let Some(chunk) = stream.poll_chunk() {
                    bridge.on_primary(chunk);
                }
                while let Some(chunk) = stream.poll_secondary() {
                    bridge.on_secondary(chunk);
                }
                bridge.drain_pairs();
                // stop() ordering (addendum 2-2):
                //   1. Audio stop-flush: core flush queues the trailing tail (denoise delay line + resampler
                //      remainder) into the rings.
                //   2. Feed that tail through VAD into the FIFOs, then deliver audio through normal pairing.
                //   3. flushVad: force-finalize the open utterance and reliably deliver the final speechEnd with
                //      a dedicated trailing carrier (independent of pairing; no drops even without a primary tail).
                // This delivers both trailing recorded audio (2) and final speechEnd (3). Audio stop-flush and
                // flushVad are separate (audio samples versus VAD events).
                let _ = stream.stop_checked();
                // Stop can discover a pending backend denial. Preserve its event and
                // terminal state before queuing any tail audio or settlement.
                forward_stream_events(&mut stream, on_event.as_ref(), &thread_terminal);
                if let Some(error) = stream.terminal_error() {
                    *thread_terminal.lock().unwrap_or_else(lock_poisoned) =
                        Some(StreamTerminal::Capture(error));
                    bridge.primary_fifo.clear();
                    bridge.secondary_fifo.clear();
                    bridge.pending_flush_events.clear();
                }
                bridge.drain_capture(&mut stream);
                while let Some(chunk) = stream.poll_chunk() {
                    bridge.on_primary(chunk); // 2 (primary tap VAD also consumes the tail here)
                }
                while let Some(chunk) = stream.poll_secondary() {
                    bridge.on_secondary(chunk); // 2 (secondary tap VAD also consumes the tail here)
                }
                bridge.drain_pairs(); // 2 Deliver trailing audio
                if let Some(error) = stream.terminal_error() {
                    *thread_terminal.lock().unwrap_or_else(lock_poisoned) =
                        Some(StreamTerminal::Capture(error));
                    bridge.primary_fifo.clear();
                    bridge.secondary_fifo.clear();
                    bridge.pending_flush_events.clear();
                }
                if stream.terminal_error().is_none() {
                    while let Some(secondary) = bridge.secondary_fifo.pop_front() {
                        bridge.emit_secondary_envelope(secondary);
                    }
                }
                if bridge.whisper.is_some() {
                    bridge.flush_whisper(true);
                    if let Some(error) = bridge
                        .whisper
                        .as_ref()
                        .and_then(|owner| owner.processing_error())
                    {
                        retain_whisper_failure(&thread_terminal, error);
                    }
                } else if stream.terminal_error().is_none() {
                    bridge.flush_vad_final();
                }
                // Publish completion only after core and addon teardown are both known.
                bridge.retain_shutdown_outcome(stream.shutdown_report());
                // 4. End signal in the same TSFN queue. Processing it in JS resolves stop()'s Promise
                // (AsyncTask / another TSFN could resolve before onChunk).
                // If the chunk TSFN fails (Closing / QueueFull, etc.), use the settlement TSFN.
            },
        )
        .map_err(|error| {
            let rollback = rollback_resources
                .lock()
                .unwrap_or_else(lock_poisoned)
                .take();
            let error =
                match rollback {
                    Some((mut stream, _, _)) => match stream.stop_checked() {
                        Ok(()) => error,
                        Err(cleanup) => flexaudio::Error::Multiple(
                            flexaudio::core::ErrorGroup::new(error, cleanup, Vec::new()),
                        ),
                    },
                    None => error,
                };
            boundary::throw_error(env, error)
        })?;

        Ok(Self {
            stop_flag,
            inner: Arc::new(Mutex::new(StreamInner {
                handle: Some(handle),
                cmd_tx: Some(cmd_tx),
            })),
            stop_phase,
            chunk_tsfn,
            settle_tsfn,
            terminal,
            report,
        })
    }

    fn take_join_handle(&self) -> Option<JoinHandle<()>> {
        let mut g = self.inner.lock().unwrap_or_else(lock_poisoned);
        g.cmd_tx = None;
        g.handle.take()
    }

    fn join_finished_bridge(&self) {
        let handle = {
            let mut inner = self.inner.lock().unwrap_or_else(lock_poisoned);
            if inner.handle.as_ref().is_some_and(JoinHandle::is_finished) {
                inner.handle.take()
            } else {
                None
            }
        };
        if let Some(handle) = handle {
            if handle.join().is_err() {
                record_bridge_failure(&self.report, &self.terminal);
            }
        }
    }

    fn spawn_stop_worker(&self, handle: JoinHandle<()>) {
        let tsfn = self.chunk_tsfn.clone();
        let settle = self.settle_tsfn.clone();
        let phase = self.stop_phase.clone();
        let report = self.report.clone();
        let terminal = self.terminal.clone();
        // Retain ownership if the join worker cannot be created. The bridge entry point
        // independently signals completion after cleanup, so exhaustion cannot strand stop().
        let pending = Arc::new(Mutex::new(Some(handle)));
        let worker_pending = pending.clone();
        let worker = thread::Builder::new()
            .name("flexaudio-napi-stop".into())
            .spawn(move || {
                if let Some(handle) = worker_pending.lock().unwrap_or_else(lock_poisoned).take() {
                    if handle.join().is_err() {
                        record_bridge_failure(&report, &terminal);
                    }
                }
                let needs_flush = matches!(
                    *phase.lock().unwrap_or_else(lock_poisoned),
                    StopPhase::Stopping { .. }
                );
                if needs_flush {
                    post_stop_flushed(tsfn.as_ref(), &settle, &phase);
                }
            });
        if worker.is_err() {
            self.inner.lock().unwrap_or_else(lock_poisoned).handle =
                pending.lock().unwrap_or_else(lock_poisoned).take();
        }
    }

    /// Send Query to the bridge thread and synchronously receive a snapshot of current stream values.
    /// Implementation shared by getters (`is_paused`/`gain`/`native_format`/`dropped_chunks`). Throws
    /// if `stop()` has already completed.
    fn query_snapshot(&self) -> napi::Result<StreamSnapshot> {
        let cmd_tx = {
            let g = self.inner.lock().unwrap_or_else(lock_poisoned);
            g.cmd_tx.clone().ok_or_else(|| {
                NapiError::new(Status::GenericFailure, "stream already stopped".to_string())
            })?
        };
        let (result_tx, result_rx) = mpsc::channel();
        cmd_tx
            .send(BridgeCmd::Query(QueryCmd { result_tx }))
            .map_err(|_| {
                NapiError::new(
                    Status::GenericFailure,
                    "bridge thread is not running".to_string(),
                )
            })?;
        result_rx.recv().map_err(|_| {
            NapiError::new(
                Status::GenericFailure,
                "bridge thread dropped before responding".to_string(),
            )
        })
    }
}

#[napi]
impl FlexStream {
    /// Stop recording. When the Promise resolves, all `onChunk` calls queued on the TSFN before
    /// stop (the last PCM and `frames:0` terminator) have been delivered to JS.
    /// Repeated calls await the same completion, or resolve immediately if already complete. Calling
    /// inside `onChunk` does not freeze JS because joining happens off the JS thread.
    /// Rejects with the stored permission error on terminal failure; queued audio
    /// and the terminator are suppressed after confirmed denial.
    #[napi(ts_return_type = "Promise<void>")]
    pub fn stop(&self, env: Env) -> napi::Result<JsObject> {
        let (deferred, promise) = create_js_promise(&env)?;
        // This is the JS thread. If the TSFN is already closed, no join handle remains, or phase is
        // Stopped, napi_resolve_deferred can be called here.
        let mut phase = self.stop_phase.lock().unwrap_or_else(lock_poisoned);
        match &mut *phase {
            StopPhase::Stopped => {
                drop(phase);
                self.join_finished_bridge();
                settle_stop(env.raw(), deferred, &self.report);
                unref_chunk_tsfn(self.chunk_tsfn.as_ref(), &env);
                return Ok(promise);
            }
            StopPhase::Stopping { waiters } => {
                waiters.push(deferred);
                return Ok(promise);
            }
            StopPhase::Running => {
                *phase = StopPhase::Stopping {
                    waiters: vec![deferred],
                };
            }
        }
        drop(phase);

        self.stop_flag.store(true, Ordering::SeqCst);
        match self.take_join_handle() {
            None => {
                // No join handle (e.g. taken by Drop's reaper). Callbacks to deliver are being
                // cleaned up through another path, or are gone. Resolve immediately on the JS thread.
                for d in take_stop_waiters(&self.stop_phase) {
                    settle_stop(env.raw(), d, &self.report);
                }
                unref_chunk_tsfn(self.chunk_tsfn.as_ref(), &env);
            }
            Some(h) => self.spawn_stop_worker(h),
        }
        Ok(promise)
    }

    /// Stored terminal failure, or undefined when capture has not terminally failed.
    /// Remains available after stop and does not consume onEvent notifications.
    #[napi]
    pub fn terminal_error(&self) -> Either<JsStreamEvent, Undefined> {
        self.terminal
            .lock()
            .unwrap_or_else(lock_poisoned)
            .clone()
            .map(|error| {
                Either::A(match error {
                    StreamTerminal::Capture(error) => terminal_event(error),
                    StreamTerminal::Whisper(error) => event_to_js(Event::Error(error.to_string())),
                })
            })
            .unwrap_or(Either::B(()))
    }

    /// Retained checked shutdown, including binding bridge failures.
    #[napi]
    pub fn shutdown_report(&self) -> Either<boundary::JsShutdownReport, Undefined> {
        self.report
            .lock()
            .unwrap_or_else(lock_poisoned)
            .as_ref()
            .map(|report| Either::A(report.to_js()))
            .unwrap_or(Either::B(()))
    }

    /// Hot-swap the input source (mic/system/process) without stopping recording.
    ///
    /// Request a switch to the `StreamConfig` built from `options` on the bridge thread and return
    /// the result synchronously (`Ok` on success, exception on failure). The output format (`outputRate`/
    /// `outputChannels`) cannot change during a switch (it would change frames in a continuous stream).
    /// Requesting a change makes `switch_source` return InvalidArg, thrown here as an exception. Chunk
    /// `seq` stays continuous across switches; the first chunk afterward has the DISCONTINUITY flag.
    /// `options.gain` is ignored (gain is stream state; use `setGain` to change it).
    ///
    /// Throws if `stop()` has already completed (the bridge thread has stopped).
    #[napi]
    pub fn switch_source(&self, env: Env, options: OpenOptions) -> napi::Result<()> {
        if let Some(error) = self.terminal.lock().unwrap_or_else(lock_poisoned).clone() {
            return Err(match error {
                StreamTerminal::Capture(error) => boundary::throw_error(&env, error),
                StreamTerminal::Whisper(error) => whisper_integration::throw_error(&env, error),
            });
        }
        // As in openStream, build_config converts options → StreamConfig.
        let config = build_config(&options).map_err(|error| {
            if options.chunk_ms.is_some_and(|ms| ms != 20.0) {
                boundary::throw_error(
                    &env,
                    flexaudio::Error::InvalidArg("chunkMs must be 20".into()),
                )
            } else {
                error
            }
        })?;

        // Send a command to the bridge thread and synchronously receive the result.
        let cmd_tx = {
            let g = self.inner.lock().unwrap_or_else(lock_poisoned);
            g.cmd_tx.clone().ok_or_else(|| {
                NapiError::new(Status::GenericFailure, "stream already stopped".to_string())
            })?
        };
        let (result_tx, result_rx) = mpsc::channel();
        cmd_tx
            .send(BridgeCmd::Switch(SwitchCmd { config, result_tx }))
            .map_err(|_| {
                NapiError::new(
                    Status::GenericFailure,
                    "bridge thread is not running".to_string(),
                )
            })?;
        // Wait synchronously for the bridge thread to execute switch_source and return its result.
        match result_rx.recv() {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(boundary::throw_error(&env, error)),
            Err(_) => Err(NapiError::new(
                Status::GenericFailure,
                "bridge thread dropped before responding".to_string(),
            )),
        }
    }

    /// Pause recording. Keep the device running but stop delivery. `resume` restarts delivery;
    /// the first chunk afterward has DISCONTINUITY. Throws if `stop()` has already completed.
    #[napi]
    pub fn pause(&self) -> napi::Result<()> {
        let cmd_tx = {
            let g = self.inner.lock().unwrap_or_else(lock_poisoned);
            g.cmd_tx.clone().ok_or_else(|| {
                NapiError::new(Status::GenericFailure, "stream already stopped".to_string())
            })?
        };
        cmd_tx.send(BridgeCmd::Pause).map_err(|_| {
            NapiError::new(
                Status::GenericFailure,
                "bridge thread is not running".to_string(),
            )
        })?;
        Ok(())
    }

    /// Unpause and resume delivery. Throws if `stop()` has already completed.
    #[napi]
    pub fn resume(&self, env: Env) -> napi::Result<()> {
        if let Some(error) = self.terminal.lock().unwrap_or_else(lock_poisoned).clone() {
            return Err(match error {
                StreamTerminal::Capture(error) => boundary::throw_error(&env, error),
                StreamTerminal::Whisper(error) => whisper_integration::throw_error(&env, error),
            });
        }
        let cmd_tx = {
            let g = self.inner.lock().unwrap_or_else(lock_poisoned);
            g.cmd_tx.clone().ok_or_else(|| {
                NapiError::new(Status::GenericFailure, "stream already stopped".to_string())
            })?
        };
        let (result_tx, result_rx) = mpsc::channel();
        cmd_tx.send(BridgeCmd::Resume(result_tx)).map_err(|_| {
            NapiError::new(
                Status::GenericFailure,
                "bridge thread is not running".to_string(),
            )
        })?;
        result_rx
            .recv()
            .map_err(|_| {
                NapiError::new(
                    Status::GenericFailure,
                    "bridge thread dropped before responding",
                )
            })?
            .map_err(|error| boundary::throw_error(&env, error))
    }

    /// Force-finalize integrated VAD's currently open utterance (runtime operation).
    ///
    /// silero emits no `speechEnd` until silence arrives. Call this when pausing recognition or
    /// finalizing the last utterance at recording end. If an utterance is open, its final `speechEnd`
    /// (and paired `speechStart`) are prepended to `vadEvents` of the next tap chunk (with `atNs`
    /// from recording zero). Chunks arrive every 20ms, so latency is ≤ 1 chunk. VAD then
    /// resets and detects the next utterance with fresh context.
    ///
    /// This **does not change config** (independent of `secondaryOutput`/encoding being fixed at open
    /// and unchangeable through `switchSource`). It is also separate from audio stop-flush and does not
    /// modify audio samples. No-op if VAD is not configured. `stop()` calls this automatically after audio
    /// stop-flush. Throws if `stop()` has already completed.
    #[napi]
    pub fn flush_vad(&self) -> napi::Result<()> {
        let cmd_tx = {
            let g = self.inner.lock().unwrap_or_else(lock_poisoned);
            g.cmd_tx.clone().ok_or_else(|| {
                NapiError::new(Status::GenericFailure, "stream already stopped".to_string())
            })?
        };
        cmd_tx.send(BridgeCmd::FlushVad).map_err(|_| {
            NapiError::new(
                Status::GenericFailure,
                "bridge thread is not running".to_string(),
            )
        })?;
        Ok(())
    }

    /// Drain an attached epoch and deliver its closing carrier before promise settlement.
    #[napi(ts_return_type = "Promise<void>")]
    pub fn flush_whisper_vad(&self, env: Env) -> napi::Result<JsObject> {
        let sender = self
            .inner
            .lock()
            .unwrap_or_else(lock_poisoned)
            .cmd_tx
            .clone()
            .ok_or_else(|| NapiError::new(Status::GenericFailure, "stream already stopped"))?;
        let (deferred, promise) = create_js_promise(&env)?;
        sender
            .send(BridgeCmd::FlushWhisperVad(deferred))
            .map_err(|_| NapiError::new(Status::GenericFailure, "bridge thread is not running"))?;
        Ok(promise)
    }

    /// Change input gain (linear multiplier). 1.0=unchanged, 2.0=about +6dB, 0.0=silence. Callable
    /// any time during recording; takes effect on the next chunk (20ms granularity). Multiplied samples
    /// are clamped to ±1.0. Throws unless finite and nonnegative, or if `stop()` has already completed.
    #[napi]
    pub fn set_gain(&self, gain: f64) -> napi::Result<()> {
        // Validate after f64→f32 conversion (also rejects huge values that become infinity in f32).
        let gain = gain as f32;
        if !gain.is_finite() || gain < 0.0 {
            return Err(NapiError::new(
                Status::InvalidArg,
                format!("gain must be finite and >= 0.0, got {gain}"),
            ));
        }
        let cmd_tx = {
            let g = self.inner.lock().unwrap_or_else(lock_poisoned);
            g.cmd_tx.clone().ok_or_else(|| {
                NapiError::new(Status::GenericFailure, "stream already stopped".to_string())
            })?
        };
        cmd_tx.send(BridgeCmd::SetGain(gain)).map_err(|_| {
            NapiError::new(
                Status::GenericFailure,
                "bridge thread is not running".to_string(),
            )
        })?;
        Ok(())
    }

    /// Whether currently paused. Throws if `stop()` has already completed.
    #[napi]
    pub fn is_paused(&self) -> napi::Result<bool> {
        Ok(self.query_snapshot()?.is_paused)
    }

    /// Current input gain (linear multiplier). Throws if `stop()` has already completed.
    #[napi]
    pub fn gain(&self) -> napi::Result<f64> {
        Ok(self.query_snapshot()?.gain as f64)
    }

    /// Current backend's native format `{ sampleRate, channels }`, for display/diagnostics
    /// (delivered chunks use output format `outputRate`/`outputChannels`).
    /// Changing sources with `switchSource` updates this to the new backend's values. Throws if `stop()`
    /// has already completed.
    #[napi]
    pub fn native_format(&self) -> napi::Result<JsNativeFormat> {
        let s = self.query_snapshot()?;
        Ok(JsNativeFormat {
            sample_rate: s.native_sample_rate,
            channels: s.native_channels,
        })
    }

    /// Cumulative chunks discarded by the chunk ring's DROP_OLDEST policy (BigInt). Throws if `stop()`
    /// has already completed.
    #[napi]
    pub fn dropped_chunks(&self) -> napi::Result<BigInt> {
        Ok(BigInt::from(self.query_snapshot()?.dropped_chunks))
    }
}

impl Drop for FlexStream {
    fn drop(&mut self) {
        // GC path. Do not block the JS thread (GC) with join. Set stop_flag and join the handle
        // on a reaper thread. Resources (bridge, TSFN, capture) are destroyed when the bridge exits.
        // No Promise waiters exist (no explicit stop).
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(h) = self.take_join_handle() {
            if h.is_finished() {
                if h.join().is_err() {
                    record_bridge_failure(&self.report, &self.terminal);
                }
                return;
            }
            let report = self.report.clone();
            let terminal = self.terminal.clone();
            let _ = thread::Builder::new()
                .name("flexaudio-napi-reaper".into())
                .spawn(move || {
                    if h.join().is_err() {
                        record_bridge_failure(&report, &terminal);
                    }
                });
        }
    }
}

// ---------------------------------------------------------------------------
// DeviceWatcherHandle (class)
// ---------------------------------------------------------------------------

/// Device attachment/removal watcher handle. The bridge thread polls `DeviceWatcher`.
#[napi]
pub struct DeviceWatcherHandle {
    stop_flag: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl DeviceWatcherHandle {
    fn shutdown(&mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

#[napi]
impl DeviceWatcherHandle {
    /// Stop watching and join the bridge thread. Safe to call repeatedly.
    #[napi]
    pub fn stop(&mut self) {
        self.shutdown();
    }
}

impl Drop for DeviceWatcherHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ---------------------------------------------------------------------------
// Public functions
// ---------------------------------------------------------------------------

/// Enumerate the complete inventory. Discovery failure throws a typed audio exception.
#[napi]
pub fn devices(env: Env) -> napi::Result<Vec<JsDeviceInfo>> {
    let list = flexaudio::devices().map_err(|error| boundary::throw_error(&env, error))?;
    Ok(list.into_iter().map(device_info_to_js).collect())
}

/// Enumerate processes with audio output sessions (streams) that can be targeted by per-process
/// capture (`openStream({ kind: 'process', processId })`). Excludes the calling process itself.
/// Stopped/Idle sessions are included. Use `isOutputActive` to check whether audio is playing.
///
/// Sorted by active output (`isOutputActive: true`) first → display name → pid, with each pid
/// deduplicated. Read-only, with no permission prompts. Returns within 3 seconds even if the OS
/// does not respond. Runs on the libuv thread pool without blocking the JS event loop.
///
/// - Linux (PipeWire): clients with `Stream/Output/Audio` nodes. `executable` comes from
///   `/proc/<pid>/exe`, falling back to `/proc/<pid>/comm` if unreadable.
/// - Windows: processes with audio sessions on enabled output devices. Both enumeration and capture
///   require Windows build 20348 or later (Windows 11 / Windows Server 2022).
/// - macOS 14.4+: process objects known to Core Audio (with `bundleId`,
///   including input-only processes).
///
/// Interpreting results: an empty array means per-process capture is available but no qualifying
/// processes currently exist (not "nothing is playing"). Rejection means per-process capture is
/// unavailable (PipeWire unreachable on Linux; macOS below 14.4 / Windows below build 20348 return
/// `unsupported OS version`; other OSes return `unsupported`; permission denied), the OS did not
/// respond within the time limit, or the previous query is still pending (same `Error`
/// type and wording as the synchronous version).
#[napi(ts_return_type = "Promise<Array<JsProcessInfo>>")]
pub fn processes() -> AsyncTask<ProcessesTask> {
    AsyncTask::new(ProcessesTask)
}

/// Open and start a stream, returning a `FlexStream` that sends chunks/events to callbacks.
/// Confirmed microphone/system-audio denial throws actionable permission guidance.
/// Supply onEvent for runtime denial, permissionPending and silenceWhileSourceActive
/// advisories. Without onEvent, terminalError() and stop() expose terminal failures
/// only; they do not expose advisories. macOS bundled microphone prompts wait up to
/// 30 s; an unanswered prompt proceeds with capture. Undecided consent emits
/// permissionPending after 5 s of capture. Authorization is checked every 500 ms
/// for the first 60 s, then every 2 s until resolved or stopped. Bare-host opens rely
/// on the responsible-app prompt. Windows checks Microphone privacy, including
/// Let desktop apps access your microphone.
///
/// macOS system/process capture runs a once-per-generation self-probe after 5 s of
/// exact-zero samples with eligible external output active. It renders a roughly
/// 300 ms diagnostic signal on the default output and captures only our own process
/// in a separate private tap. A confirmed failure emits terminal permissionDenied
/// with permission systemAudio; an inconclusive result emits silenceWhileSourceActive
/// and continues. The signal may enter capture when our own process is included
/// (e.g. excludeSelf: false). Stopping cancels the probe without a late event.
///
/// `options.denoise` enables noise suppression in core (internal canonical form), so both primary
/// and secondary taps receive denoised audio. `options.vad` runs VAD on the tap selected by `vadTap`
/// and attaches finalized events to that tap's chunk `vadEvents` (with absolute `atNs` from
/// recording zero). `options.secondaryOutput` enables the secondary tap, returning paired chunks
/// in another format (`primary.secondary` in `onChunk`). The denoise 48kHz requirement and
/// invalid VAD settings are checked and rejected here before opening the stream.
///
/// Standard operation enables the secondary tap and VAD for the entire
/// recording. Toggle transcription by keeping or discarding the delivered
/// results, not by re-opening the stream; the secondary format is fixed at open
/// (see `switchSource`), so the recognition resample + VAD run on every
/// recording — budget for them as a constant cost, not an opt-in. The integrated
/// VAD defaults `maxSpeechMs` to 30 s (see `VadOptions`) so a monologue with no
/// silence stays bounded; call `flushVad` to force-close the open utterance
/// (e.g. when pausing recognition). Per-chunk `vadEvents[].atNs` is a recording
/// zero-based absolute time that is monotonic non-decreasing across chunks.
///
/// `onChunk` is called with **one** argument, the primary `JsAudioChunk`. When
/// `secondaryOutput` is set, the paired secondary chunk arrives as
/// `chunk.secondary` (it is not a second callback argument). VAD results ride
/// on the chunk of the tap selected by `vadTap`: `chunk.vadEvents` for
/// 'primary', `chunk.secondary?.vadEvents` for 'secondary'. Do not block
/// synchronously inside `onChunk` (defer heavy work to a later task); blocking
/// stalls terminator delivery and delays `stop()` resolving.
#[napi]
pub fn open_stream(
    env: Env,
    options: OpenOptions,
    #[napi(ts_arg_type = "(chunk: JsAudioChunk) => void")] on_chunk: Function<
        JsAudioChunk,
        Unknown,
    >,
    #[napi(ts_arg_type = "((event: JsStreamEvent) => void) | undefined | null")] on_event: Option<
        EventTsfn,
    >,
) -> napi::Result<FlexStream> {
    let whisper_options = options
        .whisper_vad
        .as_ref()
        .map(|value| {
            // SAFETY: borrow the live JS value during synchronous parsing only.
            let value = unsafe { napi::JsUnknown::from_raw_unchecked(env.raw(), value.raw()) };
            whisper_vad::stream_options(
                value,
                options.vad.is_some(),
                options.vad_tap.is_some(),
                options.secondary_output.is_some(),
            )
        })
        .transpose()
        .map_err(|error| whisper_vad::throw_boundary(&env, error))?;
    let whisper = whisper_options
        .map(|(params, options, tap)| whisper_integration::WhisperBridge::new(params, options, tap))
        .transpose()
        .map_err(|error| whisper_integration::throw_error(&env, error))?;
    let config = build_config(&options).map_err(|error| {
        if options.chunk_ms.is_some_and(|ms| ms != 20.0) {
            boundary::throw_error(
                &env,
                flexaudio::Error::InvalidArg("chunkMs must be 20".into()),
            )
        } else {
            error
        }
    })?;
    let output_rate = config.output.sample_rate;
    let output_channels = config.output.channels;

    // Integrated denoise: first validate the public contract's 48kHz requirement (unchanged). Delegate to core
    // if enabled (facade set_denoise; denoise applies to the internal 48k/stereo canonical form).
    let denoise_enabled = options.denoise.unwrap_or(false);
    check_denoise_rate(denoise_enabled, output_rate)?;

    // Parse secondary tap encoding / VAD tap.
    let secondary = match &options.secondary_output {
        Some(s) => Some(SecondaryTapCfg {
            rate: s.rate,
            channels: s.channels,
            encoding: parse_sec_encoding(s.encoding.as_deref())?,
        }),
        None => None,
    };
    let vad_tap = parse_vad_tap(options.vad_tap.as_deref())?;
    if vad_tap == VadTap::Secondary && secondary.is_none() {
        return Err(NapiError::new(
            Status::InvalidArg,
            "vadTap 'secondary' requires secondaryOutput".to_string(),
        ));
    }

    // Integrated VAD: construct when specified (model loading/invalid settings throw here). The internal VAD
    // rate is the denominator for absolute time calculation. The integrated path defaults maxSpeechMs
    // to 30s if omitted (standalone Vad keeps silero's faithful 0=unbounded default).
    let (vad, vad_rate) = match &options.vad {
        Some(o) => {
            let cfg = build_integrated_vad_config(o);
            let rate = cfg.sample_rate;
            (Some(CoreVad::new(cfg).map_err(vad_err)?), rate)
        }
        None => (None, 16_000),
    };

    let mut stream = flexaudio::open(config).map_err(|error| boundary::throw_error(&env, error))?;
    if denoise_enabled {
        stream.set_denoise(true);
    }
    if whisper.is_some() {
        stream
            .enable_capture_tap()
            .map_err(|error| boundary::throw_error(&env, error))?;
    }
    stream
        .start()
        .map_err(|error| boundary::throw_error(&env, error))?;

    let stop_phase = Arc::new(Mutex::new(StopPhase::Running));
    let terminal: TerminalError = Arc::new(Mutex::new(None));
    let report: FinalReport = Arc::new(Mutex::new(None));
    let user = make_user_chunk_cb(&env, &on_chunk)?;
    let chunk_weak: ChunkTsfnWeakCell = Arc::new(OnceLock::new());
    let on_chunk = make_chunk_tsfn(
        &env,
        stop_phase.clone(),
        user.clone(),
        chunk_weak.clone(),
        terminal.clone(),
        report.clone(),
    )?;
    let settle_tsfn = make_settle_tsfn(&env, stop_phase.clone(), chunk_weak, user, report.clone())?;
    let bridge = PairingBridge {
        whisper,
        on_chunk: on_chunk.as_ref().clone(),
        stop_phase,
        report: report.clone(),
        vad: vad.map(|owner| vad_integration::IntegratedVad::new(owner, vad_rate)),
        vad_tap,
        vad_anchor_pts: 0,
        pending_flush_events: Vec::new(),
        output_rate,
        output_channels,
        secondary,
        primary_fifo: VecDeque::new(),
        secondary_fifo: VecDeque::new(),
        last_emitted_primary_pts: 0,
        last_primary_seq: 0,
        last_primary_frame_index: 0,
        last_primary_dropped: 0,
    };
    FlexStream::spawn(
        &env,
        stream,
        bridge,
        on_event,
        on_chunk,
        settle_tsfn,
        terminal,
    )
}

/// Watch device attachment/removal and return a `DeviceWatcherHandle` that sends events to the callback.
#[napi]
pub fn watch_devices(
    env: Env,
    #[napi(ts_arg_type = "(event: JsDeviceEvent) => void")] on_event: DeviceTsfn,
) -> napi::Result<DeviceWatcherHandle> {
    let mut watcher =
        flexaudio::watch_devices().map_err(|error| boundary::throw_error(&env, error))?;
    let stop_flag = Arc::new(AtomicBool::new(false));
    let thread_stop = stop_flag.clone();

    let handle = thread::Builder::new()
        .name("flexaudio-napi-devices".into())
        .spawn(move || {
            loop {
                if thread_stop.load(Ordering::SeqCst) {
                    break;
                }
                while let Some(ev) = watcher.poll_event() {
                    on_event.call(
                        device_event_to_js(ev),
                        ThreadsafeFunctionCallMode::NonBlocking,
                    );
                }
                thread::sleep(DEVICE_POLL_INTERVAL);
            }
            watcher.stop();
        })
        .map_err(|_| {
            boundary::throw_error(
                &env,
                flexaudio::Error::Backend("device bridge thread creation failed".into()),
            )
        })?;

    Ok(DeviceWatcherHandle {
        stop_flag,
        handle: Some(handle),
    })
}

/// Test-only; outside the public API.
///
/// Create a stream by passing `MockBackend` to low-level `Stream::open`, then run it through the
/// same bridge / TSFN path as `open_stream`. End-to-end verification of all marshaling paths
/// (Float32Array, BigInt, peak/rms, frames) without real audio. Do not use in production code.
///
/// Passing `secondaryRate` enables the secondary tap (`secondaryChannels` defaults to 1;
/// `secondaryEncoding` is 'f32'|'s16', default 'f32'), allowing pairing, s16 quantization, and
/// `Int16Array` marshaling to be verified without real audio (no actual capture needed).
///
/// Passing `vadThreshold` enables integrated VAD (`vadTap` is 'primary'|'secondary', default
/// 'primary'), allowing `flushVad`, `vadEvents` `atNs`, and automatic `stop()` flush to be verified
/// without real audio. Tests construct it with `minSpeechMs=0`, so threshold 0 lets `flushVad`
/// reliably finalize open utterances (verifies RT finalization even with synthetic waves lacking silence).
///
/// JS name is `__openMockStream`. Leading `__` marks it outside the public API. napi's default
/// conversion drops leading underscores, producing `openMockStream`, so `js_name` fixes the name.
#[napi(js_name = "__openMockStream")]
#[allow(clippy::too_many_arguments)]
pub fn open_mock_stream(
    env: Env,
    sample_rate: u32,
    channels: u16,
    freq_hz: f64,
    #[napi(ts_arg_type = "(chunk: JsAudioChunk) => void")] on_chunk: Function<
        JsAudioChunk,
        Unknown,
    >,
    secondary_rate: Option<u32>,
    secondary_channels: Option<u16>,
    secondary_encoding: Option<String>,
    vad_threshold: Option<f64>,
    vad_tap: Option<String>,
    whisper_vad: Option<napi::JsUnknown>,
) -> napi::Result<FlexStream> {
    let whisper_options = whisper_vad
        .map(|value| {
            whisper_vad::stream_options(
                value,
                vad_threshold.is_some(),
                vad_tap.is_some(),
                secondary_rate.is_some(),
            )
        })
        .transpose()
        .map_err(|error| whisper_vad::throw_boundary(&env, error))?;
    let whisper = whisper_options
        .map(|(params, options, tap)| whisper_integration::WhisperBridge::new(params, options, tap))
        .transpose()
        .map_err(|error| whisper_integration::throw_error(&env, error))?;
    // Secondary tap (only when configured). Validate encoding and build marshaling settings.
    let secondary_cfg = match secondary_rate {
        Some(rate) => {
            let ch = secondary_channels.unwrap_or(1);
            Some(SecondaryTapCfg {
                rate,
                channels: ch,
                encoding: parse_sec_encoding(secondary_encoding.as_deref())?,
            })
        }
        None => None,
    };
    let config = StreamConfig {
        kind: SourceKind::Mic,
        output: OutputFormat {
            sample_rate,
            channels,
        },
        secondary_output: secondary_cfg.map(|c| OutputFormat {
            sample_rate: c.rate,
            channels: c.channels,
        }),
        ..Default::default()
    };
    // Integrated VAD (test-only, when `vadThreshold` is specified). Construct with min_speech=0 so
    // flushVad reliably finalizes even short open utterances. Internal VAD rate is fixed at 16000.
    let vad_tap = parse_vad_tap(vad_tap.as_deref())?;
    if vad_tap == VadTap::Secondary && secondary_cfg.is_none() {
        return Err(NapiError::new(
            Status::InvalidArg,
            "vadTap 'secondary' requires a secondary tap".to_string(),
        ));
    }
    let vad = match vad_threshold {
        Some(thr) => {
            let cfg = VadConfig {
                threshold: thr as f32,
                neg_threshold: Some(thr as f32),
                min_speech_ms: 0,
                min_silence_ms: 0,
                speech_pad_ms: 0,
                max_speech_ms: 0,
                sample_rate: 16_000,
            };
            Some(CoreVad::new(cfg).map_err(vad_err)?)
        }
        None => None,
    };

    let backend = Box::new(flexaudio::MockBackend::new(
        sample_rate,
        channels,
        freq_hz as f32,
    ));
    let mut stream = flexaudio::Stream::open(config, backend)
        .map_err(|error| boundary::throw_error(&env, error))?;
    if whisper.is_some() {
        stream
            .enable_capture_tap()
            .map_err(|error| boundary::throw_error(&env, error))?;
    }
    stream
        .start()
        .map_err(|error| boundary::throw_error(&env, error))?;
    // The mock path bypasses integrated denoise. VAD runs only when `vadThreshold` is specified (for
    // verification of flushVad, vadEvents, and pairing paths).
    let stop_phase = Arc::new(Mutex::new(StopPhase::Running));
    let terminal: TerminalError = Arc::new(Mutex::new(None));
    let report: FinalReport = Arc::new(Mutex::new(None));
    let user = make_user_chunk_cb(&env, &on_chunk)?;
    let chunk_weak: ChunkTsfnWeakCell = Arc::new(OnceLock::new());
    let on_chunk = make_chunk_tsfn(
        &env,
        stop_phase.clone(),
        user.clone(),
        chunk_weak.clone(),
        terminal.clone(),
        report.clone(),
    )?;
    let settle_tsfn = make_settle_tsfn(&env, stop_phase.clone(), chunk_weak, user, report.clone())?;
    let bridge = PairingBridge {
        whisper,
        on_chunk: on_chunk.as_ref().clone(),
        stop_phase,
        report: report.clone(),
        vad: vad.map(|owner| vad_integration::IntegratedVad::new(owner, 16_000)),
        vad_tap,
        vad_anchor_pts: 0,
        pending_flush_events: Vec::new(),
        output_rate: sample_rate,
        output_channels: channels,
        secondary: secondary_cfg,
        primary_fifo: VecDeque::new(),
        secondary_fifo: VecDeque::new(),
        last_emitted_primary_pts: 0,
        last_primary_seq: 0,
        last_primary_frame_index: 0,
        last_primary_dropped: 0,
    };
    FlexStream::spawn(&env, stream, bridge, None, on_chunk, settle_tsfn, terminal)
}

// ---------------------------------------------------------------------------
// Standalone addon 1: Vad (small wrapper for streaming silero-VAD)
// ---------------------------------------------------------------------------

/// Offline VAD handle (silero-VAD on ONNX, embedded model).
///
/// Each instance owns one ONNX session. Feed arbitrary-format interleaved f32 (`inputSampleRate` /
/// `inputChannels`) to [`Vad::process`]; it converts to mono at the VAD rate internally,
/// detects speech segments, and returns finalized [`JsVadEvent`] values. Use this to classify arbitrary
/// sample sequences yourself instead of using integrated VAD in `openStream`.
#[napi]
pub struct Vad {
    inner: CoreVad,
}

#[napi]
impl Vad {
    /// Construct VAD from a settings object (load the embedded model). Invalid settings
    /// (sampleRate other than 8000/16000, threshold outside `[0,1]`, etc.) yield InvalidArg;
    /// model loading failure yields GenericFailure.
    #[napi(constructor)]
    pub fn new(options: VadOptions) -> napi::Result<Self> {
        let inner = CoreVad::new(build_vad_config(&options)).map_err(vad_err)?;
        Ok(Vad { inner })
    }

    /// Process arbitrary-format interleaved f32 and return finalized [`JsVadEvent`] values.
    ///
    /// Partial frames carry over internally, so input can be split anywhere. `atSample` uses
    /// VAD's internal rate (see [`JsVadEvent`]).
    #[napi]
    pub fn process(
        &mut self,
        samples: Float32Array,
        input_sample_rate: u32,
        input_channels: u16,
    ) -> napi::Result<Vec<JsVadEvent>> {
        Ok(self
            .inner
            .process_pcm(&samples[..], input_sample_rate, input_channels)
            .map_err(vad_err)?
            .into_iter()
            .map(vad_event_to_js)
            .collect())
    }

    /// Force-finalize the currently open utterance and return finalized [`JsVadEvent`] values (same behavior
    /// as reaching input end). Internal state then resets; the next `process` starts with fresh
    /// context. No model inference runs, making this lightweight and deterministic.
    ///
    /// Standalone `Vad` has no pts context, so `atNs` is `undefined` (`atSample` remains the raw cumulative
    /// position at VAD's internal rate). Returns an empty array when no utterance is open.
    #[napi]
    pub fn flush(&mut self) -> napi::Result<Vec<JsVadEvent>> {
        Ok(self
            .inner
            .flush()
            .map_err(vad_err)?
            .into_iter()
            .map(vad_event_to_js)
            .collect())
    }

    /// Initialize internal state (state / context / state machine / sample position / resampler).
    #[napi]
    pub fn reset(&mut self) -> napi::Result<()> {
        self.inner.reset().map_err(vad_err)
    }
}

// ---------------------------------------------------------------------------
// Standalone addon 2: FlacEncoder (incremental FLAC output + timed rotation)
// ---------------------------------------------------------------------------

/// Writer that incrementally saves recording chunks to FLAC files with lossless compression.
///
/// When `splitSeconds` is at least 1, reaching `splitSeconds × sampleRate` written frames closes
/// the current file and rotates to the next, using three-digit sequence numbers such as
/// `name-001.flac, name-002.flac, …` (same convention as CLI WAV splitting). Boundaries advance
/// at chunk granularity when the threshold is reached, so each file may exceed the specified duration
/// by up to one chunk; chunks are never split and no data is dropped. Omitted/0 `splitSeconds` uses one file.
#[napi]
pub struct FlacEncoder {
    inner: flac_encoder::RotatingFlacEncoder,
}

#[napi]
impl FlacEncoder {
    /// Create a FLAC writer. Omitted/0 `splitSeconds` uses one file; at least 1 enables timed rotation.
    /// `channels` must be 1..=2, `sampleRate` 1..=96000 Hz (otherwise InvalidArg).
    #[napi(factory)]
    pub fn create(
        path: String,
        sample_rate: u32,
        channels: u16,
        split_seconds: Option<u32>,
    ) -> napi::Result<FlacEncoder> {
        let inner = flac_encoder::RotatingFlacEncoder::create(
            path.into(),
            sample_rate,
            channels,
            split_seconds,
        )
        .map_err(encode_err)?;
        Ok(Self { inner })
    }

    /// Append interleaved f32 (length must be a multiple of `channels`, otherwise InvalidArg).
    /// Reaching the split threshold finalizes the current file; the next chunk opens the next file.
    #[napi]
    pub fn write_chunk(&mut self, samples: Float32Array) -> napi::Result<()> {
        self.inner.write_chunk(&samples).map_err(encode_err)
    }

    /// Finalize and close the file. Repeated successful calls are no-ops; failures remain errors.
    /// Dropping still makes a best-effort close, but finalize reports write errors.
    #[napi]
    pub fn finalize(&mut self) -> napi::Result<()> {
        self.inner.finalize().map_err(encode_err)
    }
}

// ---------------------------------------------------------------------------
// Standalone addon 3: Denoiser (offline RNNoise noise suppression)
// ---------------------------------------------------------------------------

/// Offline noise suppressor (RNNoise via nnnoiseless, embedded weights). **Requires 48kHz**;
/// intended to reduce steady noise in microphone recordings (fans, air conditioning, typing, etc.).
///
/// Fixed latency of [`FRAME_SIZE`](flexaudio_denoise::FRAME_SIZE) (10ms at 48kHz). Output is
/// the input delayed by one frame. The first frame is silence padding;
/// retrieve the remaining trailing frame with [`Denoiser::flush`].
#[napi]
pub struct Denoiser {
    inner: CoreDenoiser,
}

#[napi]
impl Denoiser {
    /// Construct with a channel count (1 = mono, 2 = stereo interleaved). Out-of-range values yield
    /// InvalidArg.
    #[napi(constructor)]
    pub fn new(channels: u16) -> napi::Result<Self> {
        let inner = CoreDenoiser::new(channels).map_err(denoise_err)?;
        Ok(Denoiser { inner })
    }

    /// Suppress noise in arbitrary-length interleaved f32 (±1.0 normalized, 48kHz, length a multiple of
    /// channels), returning a **new array** (copied because in-place processing is awkward in napi).
    /// A length not divisible by channels yields InvalidArg.
    #[napi]
    pub fn process(&mut self, samples: Float32Array) -> napi::Result<Float32Array> {
        let mut buf = samples.to_vec();
        self.inner.process(&mut buf).map_err(denoise_err)?;
        Ok(Float32Array::new(buf))
    }

    /// Process the carried remainder, return the trailing delay (1 frame/ch), and close the stream.
    /// Then return to the freshly constructed state, ready to process another stream.
    #[napi]
    pub fn flush(&mut self) -> Float32Array {
        Float32Array::new(self.inner.flush())
    }

    /// Initialize all RNN state, carryover, and delay lines.
    #[napi]
    pub fn reset(&mut self) {
        self.inner.reset();
    }
}

#[cfg(test)]
mod tests {
    //! Verify pure marshaling logic without a JS runtime.
    //!
    //! Only pure conversion from Rust values to JS intermediate representations is checked here:
    //! - `parse_source_kind` / `source_kind_str` (round trip)
    //! - `parse_process_mode` (default/explicit/unknown)
    //! - `build_config` (OpenOptions → StreamConfig defaults and mapping)
    //! - `to_napi_err` (flexaudio::Error → napi string and Status)
    //! - `event_to_js` / `device_event_to_js` (kind strings and payload)
    //! - `chunk_to_js` (seq u64 → BigInt, data, frames, peak/rms)
    //!
    //! `Float32Array::new(Vec)` and `BigInt::from(u64)` populate pure Rust fields; values can be
    //! read back through `Deref<[f32]>` / `get_u64()` without a JS runtime (napi 2.16).

    use super::*;
    use flexaudio_encode::EncodeError;

    // --- source kind round trip ---

    #[test]
    fn terminal_backend_events_keep_error_kind_and_cause() {
        let error = flexaudio::Error::Backend("authorization query failed".into());
        let event = event_to_js(Event::TerminalError {
            error: error.clone(),
        });
        assert_eq!(event.kind, "terminalError");
        let mapped = event.error.unwrap();
        assert_eq!(mapped.kind, "backend");
        assert_eq!(mapped.message, error.to_string());
        assert_eq!(event.permission, None);
    }

    #[test]
    fn source_kind_roundtrips() {
        for (s, k) in [
            ("mic", SourceKind::Mic),
            ("system", SourceKind::SystemLoopback),
            ("process", SourceKind::ProcessLoopback),
            ("mix", SourceKind::Mix),
        ] {
            assert_eq!(parse_source_kind(s).unwrap(), k);
            assert_eq!(source_kind_str(k), s);
        }
    }

    #[test]
    fn parse_source_kind_rejects_unknown() {
        let err = parse_source_kind("bogus").unwrap_err();
        assert_eq!(err.status, Status::InvalidArg);
    }

    // --- process mode ---

    #[test]
    fn parse_process_mode_defaults_and_explicit() {
        // None / "include" defaults to Include.
        assert_eq!(parse_process_mode(None).unwrap(), ProcessMode::Include);
        assert_eq!(
            parse_process_mode(Some("include")).unwrap(),
            ProcessMode::Include
        );
        // "exclude" maps to Exclude.
        assert_eq!(
            parse_process_mode(Some("exclude")).unwrap(),
            ProcessMode::Exclude
        );
    }

    #[test]
    fn parse_process_mode_rejects_unknown() {
        let err = parse_process_mode(Some("nope")).unwrap_err();
        assert_eq!(err.status, Status::InvalidArg);
    }

    // --- build_config ---

    /// Helper creating OpenOptions with all fields omitted (except kind).
    fn options_with_kind(kind: &str) -> OpenOptions {
        OpenOptions {
            kind: kind.to_string(),
            device_id: None,
            process_id: None,
            mode: None,
            exclude_self: None,
            exclude_pids: None,
            output_rate: None,
            output_channels: None,
            chunk_ms: None,
            gain: None,
            mic_device_id: None,
            system_device_id: None,
            mic_gain: None,
            system_gain: None,
            vad: None,
            vad_tap: None,
            whisper_vad: None,
            denoise: None,
            secondary_output: None,
        }
    }

    #[test]
    fn build_config_defaults() {
        let opts = options_with_kind("mic");
        let cfg = build_config(&opts).unwrap();
        assert_eq!(cfg.kind, SourceKind::Mic);
        // Default output {48000, 2}.
        assert_eq!(cfg.output.sample_rate, 48_000);
        assert_eq!(cfg.output.channels, 2);
        assert_eq!(cfg.mode, ProcessMode::Include);
        assert!(!cfg.exclude_self);
        assert_eq!(cfg.target_pid, None);
        assert_eq!(cfg.device_id, None);
        // Omitted chunk_ms uses the StreamConfig default (20).
        assert_eq!(cfg.chunk_ms, 20);
        // Omitted gain defaults to 1.0.
        assert_eq!(cfg.gain, 1.0);
        // Defaults for mix-only fields (devices unspecified, per-side gain 1.0).
        assert_eq!(cfg.mix_mic_device_id, None);
        assert_eq!(cfg.mix_system_device_id, None);
        assert_eq!(cfg.mix_mic_gain, 1.0);
        assert_eq!(cfg.mix_system_gain, 1.0);
    }

    #[test]
    fn chunk_ms_defaults_to_20_and_rejects_every_other_value() {
        let mut options = options_with_kind("mic");
        assert_eq!(build_config(&options).unwrap().chunk_ms, 20);
        options.chunk_ms = Some(20.0);
        assert_eq!(build_config(&options).unwrap().chunk_ms, 20);
        for ms in [
            0.0,
            10.0,
            40.0,
            20.5,
            -20.0,
            f64::NAN,
            f64::INFINITY,
            4_294_967_316.0,
        ] {
            options.chunk_ms = Some(ms);
            assert_eq!(
                build_config(&options).unwrap_err().status,
                Status::InvalidArg
            );
        }
    }

    #[test]
    fn build_config_exclude_pids() {
        let mut opts = options_with_kind("system");
        opts.exclude_pids = Some(vec![1.0, 100.0, 200.0, 100.0, f64::from(u32::MAX)]);
        let cfg = build_config(&opts).unwrap();
        assert_eq!(cfg.exclude_pids, vec![1, 100, 200, 100, u32::MAX]);
        assert!(!cfg.exclude_self);
        let cfg = build_config(&options_with_kind("system")).unwrap();
        assert!(cfg.exclude_pids.is_empty());
    }

    #[test]
    fn build_config_process_id() {
        for (value, expected) in [(1.0, 1), (9999.0, 9999), (f64::from(u32::MAX), u32::MAX)] {
            let mut opts = options_with_kind("process");
            opts.process_id = Some(value);
            let cfg = build_config(&opts).unwrap();
            assert_eq!(cfg.target_pid, Some(expected));
        }
    }

    #[test]
    fn build_config_rejects_invalid_pids() {
        for value in [
            -1.0,
            1.5,
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            0.0,
            -0.0,
            4294967296.0,
        ] {
            let mut opts = options_with_kind("process");
            opts.process_id = Some(value);
            let err = build_config(&opts).unwrap_err();
            assert_eq!(err.status, Status::InvalidArg);
            assert!(err.reason.contains("processId"));
            assert!(err.reason.contains(&format!("got {value}")));

            let mut opts = options_with_kind("system");
            opts.exclude_pids = Some(vec![100.0, value]);
            let err = build_config(&opts).unwrap_err();
            assert_eq!(err.status, Status::InvalidArg);
            assert!(err.reason.contains("excludePids[1]"));
            assert!(err.reason.contains(&format!("got {value}")));
        }
    }

    #[test]
    fn build_config_reflects_all_fields() {
        let opts = OpenOptions {
            kind: "process".to_string(),
            device_id: Some("dev-x".to_string()),
            process_id: Some(9999.0),
            mode: Some("exclude".to_string()),
            exclude_self: Some(true),
            exclude_pids: None,
            output_rate: Some(16_000),
            output_channels: Some(1),
            chunk_ms: Some(20.0),
            gain: Some(2.5),
            mic_device_id: None,
            system_device_id: None,
            mic_gain: None,
            system_gain: None,
            vad: None,
            vad_tap: None,
            denoise: None,
            whisper_vad: None,
            secondary_output: None,
        };
        let cfg = build_config(&opts).unwrap();
        assert_eq!(cfg.kind, SourceKind::ProcessLoopback);
        assert_eq!(cfg.device_id.as_deref(), Some("dev-x"));
        assert_eq!(cfg.target_pid, Some(9999));
        assert_eq!(cfg.mode, ProcessMode::Exclude);
        assert!(cfg.exclude_self);
        assert_eq!(cfg.output.sample_rate, 16_000);
        assert_eq!(cfg.output.channels, 1);
        assert_eq!(cfg.chunk_ms, 20);
        assert_eq!(cfg.gain, 2.5);
    }

    #[test]
    fn build_config_reflects_mix_fields() {
        let mut opts = options_with_kind("mix");
        opts.mic_device_id = Some("mic-a".to_string());
        opts.system_device_id = Some("sink-b".to_string());
        opts.mic_gain = Some(0.5);
        opts.system_gain = Some(2.0);
        let cfg = build_config(&opts).unwrap();
        assert_eq!(cfg.kind, SourceKind::Mix);
        assert_eq!(cfg.mix_mic_device_id.as_deref(), Some("mic-a"));
        assert_eq!(cfg.mix_system_device_id.as_deref(), Some("sink-b"));
        assert_eq!(cfg.mix_mic_gain, 0.5);
        assert_eq!(cfg.mix_system_gain, 2.0);
    }

    #[test]
    fn build_config_rejects_unknown_kind() {
        let opts = options_with_kind("speaker");
        let err = build_config(&opts).unwrap_err();
        assert_eq!(err.status, Status::InvalidArg);
    }

    // --- to_napi_err ---

    #[test]
    fn terminal_permission_errors_use_existing_napi_status_and_event_shape() {
        for permission in [
            flexaudio::Permission::Microphone,
            flexaudio::Permission::SystemAudio,
        ] {
            let error = flexaudio::Error::PermissionDenied {
                permission,
                detail: "denied by user".into(),
            };
            let expected = error.to_string();
            let mapped = to_napi_err(error.clone());
            assert_eq!(mapped.status, Status::GenericFailure);
            assert_eq!(mapped.reason, expected);
            let event = terminal_event(error);
            assert_eq!(event.kind, "permissionDenied");
            assert_eq!(event.permission.as_deref(), Some(permission.as_str()));
            assert_eq!(event.message.as_deref(), Some(expected.as_str()));
        }
    }

    #[test]
    fn to_napi_err_carries_message_and_status() {
        let err = to_napi_err(flexaudio::Error::DeviceNotFound);
        assert_eq!(err.status, Status::GenericFailure);
        // The Display string goes into reason.
        assert_eq!(err.reason, flexaudio::Error::DeviceNotFound.to_string());
        assert!(err.reason.contains("device not found"));
    }

    // --- event_to_js ---

    #[test]
    fn permission_pending_preserves_permission_and_advisory_message() {
        for permission in [
            flexaudio::Permission::Microphone,
            flexaudio::Permission::SystemAudio,
        ] {
            let mapped = event_to_js(Event::PermissionPending {
                permission,
                detail: "Permission is pending; capture may remain silent until granted".into(),
            });
            assert_eq!(mapped.kind, "permissionPending");
            assert_eq!(mapped.permission.as_deref(), Some(permission.as_str()));
            assert!(mapped.count.is_none());
            assert_eq!(
                mapped.message.as_deref(),
                Some("Permission is pending; capture may remain silent until granted")
            );
        }
    }

    #[test]
    fn permission_events_preserve_cause_and_advisory_kind() {
        for permission in [
            flexaudio::Permission::Microphone,
            flexaudio::Permission::SystemAudio,
        ] {
            let expected = flexaudio::Error::PermissionDenied {
                permission,
                detail: "denied by user".into(),
            }
            .to_string();
            let mapped = event_to_js(Event::PermissionDenied {
                permission,
                detail: "denied by user".into(),
            });
            assert_eq!(mapped.kind, "permissionDenied");
            assert_eq!(mapped.permission.as_deref(), Some(permission.as_str()));
            assert_eq!(mapped.message.as_deref(), Some(expected.as_str()));
        }
        let advisory = event_to_js(Event::SilenceWhileSourceActive {
            detail: "check recording privacy settings".into(),
        });
        assert_eq!(advisory.kind, "silenceWhileSourceActive");
        assert_eq!(advisory.permission, None);
        assert_eq!(
            advisory.message.as_deref(),
            Some("check recording privacy settings")
        );
    }

    #[test]
    fn event_to_js_maps_each_variant() {
        let dropped = event_to_js(Event::ChunkDropped { count: 7 });
        assert_eq!(dropped.kind, "chunkDropped");
        assert_eq!(dropped.count.unwrap().get_u64(), (false, 7, true));
        assert_eq!(dropped.message, None);

        assert_eq!(event_to_js(Event::StreamStalled).kind, "stalled");
        assert_eq!(event_to_js(Event::StreamRecovered).kind, "recovered");
        assert_eq!(
            event_to_js(Event::PermissionDenied {
                permission: flexaudio::Permission::Microphone,
                detail: "denied by user".into()
            })
            .kind,
            "permissionDenied"
        );
        assert_eq!(event_to_js(Event::DeviceLost).kind, "deviceLost");

        let errev = event_to_js(Event::Error("boom".to_string()));
        assert_eq!(errev.kind, "error");
        assert_eq!(errev.message, Some("legacy capture failure".to_string()));
    }

    // --- device_event_to_js ---

    #[test]
    fn device_event_to_js_maps_variants() {
        let info = DeviceInfo {
            id: "node-1".to_string(),
            name: "Mic A".to_string(),
            source_kind: SourceKind::Mic,
            sample_rate: 48_000,
            channels: 2,
            is_loopback: false,
            is_default: true,
        };
        let added = device_event_to_js(DeviceEvent::Added(info));
        assert_eq!(added.kind, "added");
        let dev = added.device.expect("device present");
        assert_eq!(dev.id, "node-1");
        assert_eq!(dev.source_kind, "mic");
        assert!(dev.is_default);

        let removed = device_event_to_js(DeviceEvent::Removed {
            id: "gone".to_string(),
        });
        assert_eq!(removed.kind, "removed");
        assert_eq!(removed.id.as_deref(), Some("gone"));

        let changed = device_event_to_js(DeviceEvent::DefaultChanged {
            kind: flexaudio::DefaultDeviceKind::SystemAudio,
            id: "sink-2".to_string(),
        });
        assert_eq!(changed.kind, "defaultChanged");
        assert_eq!(changed.id.as_deref(), Some("sink-2"));
        assert_eq!(changed.source_kind.as_deref(), Some("system"));
    }

    // seq u64 → BigInt conversion (pure marshaling logic).
    //
    // Full `chunk_to_js` cannot be tested here because it creates `Float32Array`. In napi
    // 2.16, `Float32Array`'s `Drop` unconditionally references `napi_call_threadsafe_function`,
    // preventing linkage in the cdylib unit test binary (no Node host) and breaking
    // `cargo test -p flexaudio-napi`. Therefore test only JS-runtime-independent
    // seq→BigInt conversion with the same logic (`BigInt::from(u64)` + `get_u64`).
    // The data/Float32Array path is covered by Node E2E (`__openMockStream`).

    #[test]
    fn seq_u64_to_bigint_is_lossless() {
        // chunk_to_js converts seq to BigInt with `BigInt::from(chunk.seq)`.
        // Verify lossless round trips even for 2^53+1 (not representable in f64).
        let seq: u64 = 9_007_199_254_740_993; // 2^53 + 1.
        let big = BigInt::from(seq);
        let (sign, value, lossless) = big.get_u64();
        assert!(!sign, "seq is nonnegative");
        assert_eq!(
            value, seq,
            "seq is preserved losslessly (precision that f64 would lose)"
        );
        assert!(lossless, "lossless because u64 fits in one word");

        // Lossless even at the u64::MAX boundary.
        let (_, max_val, max_lossless) = BigInt::from(u64::MAX).get_u64();
        assert_eq!(max_val, u64::MAX);
        assert!(max_lossless);
    }

    #[test]
    fn device_info_to_js_maps_all_fields() {
        let info = DeviceInfo {
            id: "id-x".to_string(),
            name: "Name X".to_string(),
            source_kind: SourceKind::SystemLoopback,
            sample_rate: 44_100,
            channels: 1,
            is_loopback: true,
            is_default: false,
        };
        let js = device_info_to_js(info);
        assert_eq!(js.id, "id-x");
        assert_eq!(js.name, "Name X");
        assert_eq!(js.source_kind, "system");
        assert_eq!(js.sample_rate, 44_100);
        assert_eq!(js.channels, 1);
        assert!(js.is_loopback);
        assert!(!js.is_default);
    }

    // --- Integrated options: OpenOptions carries vad/denoise ---

    #[test]
    fn open_options_carries_vad_and_denoise() {
        // vad/denoise are interpreted by open_stream, not StreamConfig. Verify build_config passes
        // unaffected by these options (orthogonal to recording configuration).
        let mut opts = options_with_kind("mic");
        opts.denoise = Some(true);
        opts.vad = Some(VadOptions {
            threshold: Some(0.4),
            neg_threshold: None,
            min_speech_ms: None,
            min_silence_ms: None,
            speech_pad_ms: None,
            max_speech_ms: None,
            sample_rate: None,
        });
        let cfg = build_config(&opts).unwrap();
        assert_eq!(cfg.kind, SourceKind::Mic);
        assert_eq!(cfg.output.sample_rate, 48_000);
    }

    // --- build_vad_config (VadOptions → VadConfig) ---

    /// VadOptions with every field omitted.
    fn empty_vad_options() -> VadOptions {
        VadOptions {
            threshold: None,
            neg_threshold: None,
            min_speech_ms: None,
            min_silence_ms: None,
            speech_pad_ms: None,
            max_speech_ms: None,
            sample_rate: None,
        }
    }

    #[test]
    fn build_vad_config_defaults_match_silero() {
        let cfg = build_vad_config(&empty_vad_options());
        let d = VadConfig::default();
        assert_eq!(cfg.threshold, d.threshold);
        // Omitted negThreshold stays None (the default formula in VadConfig applies).
        assert_eq!(cfg.neg_threshold, None);
        assert_eq!(cfg.min_speech_ms, d.min_speech_ms);
        assert_eq!(cfg.min_silence_ms, d.min_silence_ms);
        assert_eq!(cfg.speech_pad_ms, d.speech_pad_ms);
        assert_eq!(cfg.max_speech_ms, d.max_speech_ms);
        assert_eq!(cfg.sample_rate, d.sample_rate);
    }

    #[test]
    fn build_vad_config_reflects_all_fields() {
        let opts = VadOptions {
            threshold: Some(0.7),
            neg_threshold: Some(0.2),
            min_speech_ms: Some(120),
            min_silence_ms: Some(200),
            speech_pad_ms: Some(40),
            max_speech_ms: Some(5000),
            sample_rate: Some(8000),
        };
        let cfg = build_vad_config(&opts);
        assert_eq!(cfg.threshold, 0.7);
        assert_eq!(cfg.neg_threshold, Some(0.2));
        assert_eq!(cfg.min_speech_ms, 120);
        assert_eq!(cfg.min_silence_ms, 200);
        assert_eq!(cfg.speech_pad_ms, 40);
        assert_eq!(cfg.max_speech_ms, 5000);
        assert_eq!(cfg.sample_rate, 8000);
    }

    // --- build_integrated_vad_config (integrated maxSpeechMs default 30s; addendum 2-3) ---

    #[test]
    fn integrated_vad_config_defaults_max_speech_to_30s() {
        // The integrated path supplies 30_000 when maxSpeechMs is omitted (bounds monologues).
        let cfg = build_integrated_vad_config(&empty_vad_options());
        assert_eq!(cfg.max_speech_ms, INTEGRATED_VAD_MAX_SPEECH_MS_DEFAULT);
        assert_eq!(cfg.max_speech_ms, 30_000);
        // Other fields match build_vad_config (silero defaults).
        let d = VadConfig::default();
        assert_eq!(cfg.threshold, d.threshold);
        assert_eq!(cfg.neg_threshold, None);
        assert_eq!(cfg.min_speech_ms, d.min_speech_ms);
        assert_eq!(cfg.min_silence_ms, d.min_silence_ms);
        assert_eq!(cfg.speech_pad_ms, d.speech_pad_ms);
        assert_eq!(cfg.sample_rate, d.sample_rate);
    }

    #[test]
    fn integrated_vad_config_respects_explicit_max_speech() {
        // Explicit values are respected.
        let mut o = empty_vad_options();
        o.max_speech_ms = Some(5_000);
        assert_eq!(build_integrated_vad_config(&o).max_speech_ms, 5_000);
        // Explicit 0 (unbounded) restores silero's default (explicit values override default replacement).
        o.max_speech_ms = Some(0);
        assert_eq!(build_integrated_vad_config(&o).max_speech_ms, 0);
    }

    #[test]
    fn standalone_vad_config_keeps_silero_max_speech() {
        // Standalone Vad (build_vad_config) stays faithful to silero: default 0 (unbounded).
        assert_eq!(build_vad_config(&empty_vad_options()).max_speech_ms, 0);
    }

    // --- check_denoise_rate (denoise's 48kHz requirement) ---

    #[test]
    fn denoise_requires_48k() {
        // Enabled + 48000 is OK.
        assert!(check_denoise_rate(true, 48_000).is_ok());
        // Enabled + any rate other than 48000 yields InvalidArg.
        let err = check_denoise_rate(true, 16_000).unwrap_err();
        assert_eq!(err.status, Status::InvalidArg);
        // Disabled is OK regardless of rate (no validation).
        assert!(check_denoise_rate(false, 16_000).is_ok());
        assert!(check_denoise_rate(false, 48_000).is_ok());
    }

    // --- Error mapping for each addon ---

    #[test]
    fn error_mappers_carry_status() {
        // denoise: invalid channels yield InvalidArg.
        let e = denoise_err(DenoiseError::InvalidChannels(3));
        assert_eq!(e.status, Status::InvalidArg);
        // encode: unsupported parameters yield InvalidArg.
        let e = encode_err(EncodeError::Unsupported("bad".to_string()).into());
        assert_eq!(e.status, Status::InvalidArg);
        // encode: encoder internals yield GenericFailure.
        let e = encode_err(EncodeError::Encoder("boom".to_string()).into());
        assert_eq!(e.status, Status::GenericFailure);
        // vad: invalid settings yield InvalidArg.
        let e = vad_err(VadError::InvalidConfig("nope".to_string()));
        assert_eq!(e.status, Status::InvalidArg);
        // vad: model loading failure yields GenericFailure.
        let e = vad_err(VadError::ModelLoad("x".to_string()));
        assert_eq!(e.status, Status::GenericFailure);
    }

    // --- vad_event_to_js (kind strings, atSample) ---

    #[test]
    fn vad_event_to_js_maps_variants() {
        let start = vad_event_to_js(VadEvent::SpeechStart { at_sample: 512 });
        assert_eq!(start.kind, "speechStart");
        assert_eq!(start.at_sample.get_u64(), (false, 512, true));
        // Standalone path (no absolute time context): atNs = undefined.
        assert_eq!(start.at_ns, None);
        let end = vad_event_to_js(VadEvent::SpeechEnd { at_sample: 4096 });
        assert_eq!(end.kind, "speechEnd");
        assert_eq!(end.at_sample.get_u64(), (false, 4096, true));
        assert_eq!(end.at_ns, None);
    }

    #[test]
    fn vad_event_to_js_abs_carries_recording_time() {
        // Integrated path carries absolute atNs from recording zero (atSample remains the raw internal-rate position).
        let ev = vad_event_to_js_abs(VadEvent::SpeechEnd { at_sample: 8000 }, Some(1_500_000_000));
        assert_eq!(ev.kind, "speechEnd");
        assert_eq!(ev.at_sample.get_u64(), (false, 8000, true));
        assert_eq!(ev.at_ns, Some(1_500_000_000));
    }
}

#[cfg(test)]
mod reproduction_tests {
    use super::*;

    #[test]
    fn vad_at_sample_bigint_preserves_u64() {
        for count in [9_007_199_254_740_993, 9_223_372_036_854_775_808, u64::MAX] {
            for event in [
                VadEvent::SpeechStart { at_sample: count },
                VadEvent::SpeechEnd { at_sample: count },
            ] {
                for pts in [None, Some(1_500_000_000)] {
                    let event = vad_event_to_js_abs(event, pts);
                    assert_eq!(event.at_sample.get_u64(), (false, count, true));
                    assert_eq!(event.at_ns, pts);
                }
            }
        }
    }

    #[test]
    fn repro_p9_error_kind() {
        let missing = to_napi_err(flexaudio::Error::DeviceNotFound);
        let lost = to_napi_err(flexaudio::Error::DeviceLost);
        assert_eq!(missing.status, Status::GenericFailure);
        assert_eq!(lost.status, Status::GenericFailure);
        assert_ne!(
            boundary::error_code(flexaudio::Error::DeviceNotFound.kind()),
            boundary::error_code(flexaudio::Error::DeviceLost.kind()),
            "distinct core kinds must have distinct domain codes"
        );
    }

    #[test]
    fn repro_p9_invalid_arg_kind() {
        let mapped = to_napi_err(flexaudio::Error::InvalidArg("invalid rate".into()));
        assert_eq!(mapped.status, Status::InvalidArg);
    }

    #[test]
    fn repro_p9_error_control() {
        let mapped = to_napi_err(flexaudio::Error::Backend("control cause".into()));
        assert_eq!(mapped.status, Status::GenericFailure);
        assert!(mapped.reason.contains("control cause"));
    }

    #[test]
    fn repro_p9_counter_cast() {
        let event = event_to_js(Event::ChunkDropped { count: u64::MAX });
        assert!(
            event.count.as_ref().unwrap().get_u64() == (false, u64::MAX, true),
            "positive u64 drop count becomes negative i64"
        );
    }
    #[test]
    fn repro_p9_counter_control() {
        assert_eq!(
            event_to_js(Event::ChunkDropped { count: 7 })
                .count
                .unwrap()
                .get_u64(),
            (false, 7, true)
        );
    }

    #[test]
    fn repro_p9_event_union() {
        // A widening to arbitrary strings prevents exhaustive checking of the public event contract.
        let declarations = include_str!("../index.d.ts");
        let stream = declarations
            .split("export type JsStreamEvent =")
            .nth(1)
            .unwrap()
            .split("export ")
            .next()
            .unwrap();
        assert!(
            !stream.contains("type: string"),
            "JsStreamEvent.type accepts arbitrary strings"
        );
        for tag in [
            "chunkDropped",
            "stalled",
            "recovered",
            "permissionDenied",
            "permissionPending",
            "permissionGranted",
            "silenceWhileSourceActive",
            "deviceLost",
            "error",
            "terminalError",
            "recoverableError",
            "shutdownError",
            "audioLoss",
            "clipped",
            "unknown",
        ] {
            assert!(stream.contains(&format!("type: '{tag}'")), "missing {tag}");
        }
        assert!(stream.contains("type: 'chunkDropped'; count: bigint"));
        for tag in ["terminalError", "recoverableError", "shutdownError"] {
            assert!(stream.contains(&format!("type: '{tag}'; error: AudioError")));
        }
        assert!(stream.contains("type: 'audioLoss'; loss: AudioLoss"));
    }

    #[test]
    fn repro_p9_device_event_union() {
        let declarations = include_str!("../index.d.ts");
        let device = declarations
            .split("export type JsDeviceEvent =")
            .nth(1)
            .unwrap()
            .split("export ")
            .next()
            .unwrap();
        assert!(
            !device.contains("type: string"),
            "JsDeviceEvent.type accepts arbitrary strings"
        );
        for tag in [
            "added",
            "removed",
            "defaultChanged",
            "defaultCleared",
            "rescanRequired",
            "unknown",
        ] {
            assert!(device.contains(&format!("type: '{tag}'")), "missing {tag}");
        }
        assert!(device.contains("type: 'rescanRequired'; droppedEvents: bigint"));
        assert!(device.contains("type: 'defaultCleared'; sourceKind: 'mic' | 'system'"));
    }

    #[test]
    fn repro_p9_event_union_control() {
        let declarations = include_str!("../index.d.ts");
        let vad = declarations
            .split("export interface JsVadEvent {")
            .nth(1)
            .unwrap()
            .split('}')
            .next()
            .unwrap();
        assert!(!vad.contains("type: string"));
        assert!(vad.contains("speechStart"));
    }
}

// Build the test addon with cargo rustc --lib -- --cfg test --cfg flexaudio_repro_addon.
#[cfg(all(test, flexaudio_repro_addon))]
mod reproduction_bridge;
