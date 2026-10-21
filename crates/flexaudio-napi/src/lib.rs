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
mod bridge_drain;
mod callback;
mod capabilities;
mod devices;
use bridge_drain::start_bridge_stream;
use callback::UserCallback;
pub use capabilities::capabilities;
pub use devices::devices_with_namespaces;
mod flac_encoder;
mod lifecycle;
mod settings;
mod settings_owner;
mod settings_validation;
mod shutdown;
use boundary::{device_event_to_js, event_to_js, to_napi_err, JsAudioError, JsAudioLoss};
mod attachment;
mod vad;

use std::collections::VecDeque;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
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

/// pts window for pairing the secondary tap (60ms = 3 chunks). The secondary trails by 20–60ms,
/// so this window matches timestamps even with a delay of up to 3 chunks.
const PAIR_WINDOW_NS: i64 = 60_000_000;
/// Width of a 20ms chunk in ns.
const CHUNK_SPAN_NS: i64 = 20_000_000;

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
type EventCallbacks = (Option<EventTsfn>, Option<Arc<UserCallback>>);
type DeviceTsfn = ThreadsafeFunction<JsDeviceEvent, ErrorStrategy::Fatal>;

/// Values queued on the onChunk TSFN. Chunks go to the user's `onChunk`; `StopFlushed` is
/// an end signal in the same queue (not exposed to JS onChunk).
enum ChunkEmit {
    StartupDropped(Box<JsStreamEvent>),
    Chunk(Box<JsAudioChunk>),
    VadFlushed(SendDeferred, Option<flexaudio_vad::VadTapError>),
    /// A VAD-only failure: the closing carrier is already queued on this same TSFN,
    /// and the notification is handed to onEvent only after it (never an uncaught throw).
    VadFailed {
        error: flexaudio_vad::VadTapError,
        stage: shutdown::VadFailureStage,
        epoch: u32,
    },
    StopFlushed,
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
            reject_stop_fallback(env, deferred, error);
        }
    }
}

/// Shared settlement policy, also tested with injected native-allocation failures.
fn reject_with_fallback<T, E>(
    rich: Result<T, E>,
    fallback: impl FnOnce(E) -> T,
    reject: impl FnOnce(T),
) {
    reject(match rich {
        Ok(value) => value,
        Err(error) => fallback(error),
    });
}

/// Native allocation failure cannot safely be forced in a live Node environment;
/// the shared settlement policy is fault-injected independently of N-API.
fn reject_stop_fallback(env: sys::napi_env, deferred: SendDeferred, error: NapiError) {
    let js_env = unsafe { Env::from_raw(env) };
    reject_with_fallback(
        js_env
            .create_error(error)
            .map(|error| unsafe { error.raw() }),
        |_| {
            let mut pending = false;
            let _ = unsafe { sys::napi_is_exception_pending(env, &mut pending) };
            if pending {
                let mut exception = ptr::null_mut();
                let _ = unsafe { sys::napi_get_and_clear_last_exception(env, &mut exception) };
            }
            let message = b"flexaudio stop failed while creating its error";
            let mut text = ptr::null_mut();
            let status = unsafe {
                sys::napi_create_string_utf8(env, message.as_ptr().cast(), message.len(), &mut text)
            };
            let mut plain = ptr::null_mut();
            if status == sys::Status::napi_ok
                && unsafe { sys::napi_create_error(env, ptr::null_mut(), text, &mut plain) }
                    == sys::Status::napi_ok
            {
                return plain;
            }
            if text.is_null() {
                let _ = unsafe { sys::napi_get_undefined(env, &mut text) };
            }
            text
        },
        |value| {
            let _ = unsafe { sys::napi_reject_deferred(env, deferred.0, value) };
        },
    );
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
    chunk_queue: &ChunkTsfn,
    terminal: &Mutex<Option<StreamTerminal>>,
    lifecycle: Option<&lifecycle::LifecycleDelivery>,
) -> bool {
    let mut released_startup = false;
    if let Some(error) = stream.terminal_error() {
        *terminal.lock().unwrap_or_else(lock_poisoned) = Some(StreamTerminal(error));
    }
    while let Some(event) = stream.poll_event() {
        if lifecycle::is_lifecycle(&event) {
            if let Some(lifecycle) = lifecycle {
                lifecycle.emit(event);
            }
            continue;
        }
        if let Event::PermissionDenied { permission, detail } = &event {
            *terminal.lock().unwrap_or_else(lock_poisoned) =
                Some(StreamTerminal(flexaudio::Error::PermissionDenied {
                    permission: *permission,
                    detail: detail.clone(),
                }));
        }
        if let Event::TerminalError { error } = &event {
            *terminal.lock().unwrap_or_else(lock_poisoned) = Some(StreamTerminal(error.clone()));
        }
        if matches!(event, Event::StartupSamplesDropped { .. }) {
            released_startup = true;
            // poll_event claims the notice before this nonblocking enqueue. The
            // unbounded queue is owned by this bridge thread, so its notice is
            // queued before PCM on the same TSFN. If the queue is Closing,
            // subsequent PCM enqueues fail too; no callback can overtake it.
            chunk_queue.call(
                ChunkEmit::StartupDropped(Box::new(event_to_js(event))),
                ThreadsafeFunctionCallMode::NonBlocking,
            );
        } else if let Some(callback) = callback {
            callback.call(event_to_js(event), ThreadsafeFunctionCallMode::NonBlocking);
        }
    }
    released_startup
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
///
/// A streaming VAD failure is not a capture failure: it is reported once through
/// `vadFailed` and retained in the shutdown report's cleanup causes.
#[derive(Clone)]
struct StreamTerminal(flexaudio::Error);
impl std::fmt::Display for StreamTerminal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
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
            if let Some(StreamTerminal(error)) = terminal {
                retained.core = Some(flexaudio::core::ShutdownReport::new(
                    Some(error),
                    Vec::new(),
                ));
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
        .map_err(|error| flexaudio::core::spawn_error("bridge", error))
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
) -> napi::Result<Arc<UserCallback>> {
    UserCallback::retain(env, on_chunk)
}

/// Retain the original event callback for ordered VAD delivery. Core notifications
/// use an unreferenced TSFN; the chunk queue owns the event-loop lifetime through stop.
fn make_stream_event_callbacks(
    env: &Env,
    callback: Option<Function<JsStreamEvent, Unknown>>,
) -> napi::Result<EventCallbacks> {
    let Some(callback) = callback else {
        return Ok((None, None));
    };
    let retained = UserCallback::retain(env, &callback)?;
    let function = unsafe { napi::JsFunction::from_napi_value(env.raw(), callback.raw())? };
    let mut tsfn = function
        .create_threadsafe_function::<JsStreamEvent, JsStreamEvent, _, ErrorStrategy::Fatal>(
            0,
            |ctx| Ok(vec![ctx.value]),
        )?;
    tsfn.unref(env)?;
    Ok((Some(tsfn), Some(retained)))
}

/// Invoke the retained callback on the chunk queue for events with PCM ordering guarantees.
fn call_ordered_event(
    env: &Env,
    callback: Option<&Arc<UserCallback>>,
    event: JsStreamEvent,
) -> napi::Result<()> {
    let Some(callback) = callback else {
        return Ok(());
    };
    let refer = callback.refer();
    if refer.is_null() {
        return Ok(());
    }
    let mut value = ptr::null_mut();
    check_status!(unsafe { sys::napi_get_reference_value(env.raw(), refer, &mut value) })?;
    let function: Function<JsStreamEvent, Unknown> =
        unsafe { Function::from_napi_value(env.raw(), value)? };
    function.call(event)?;
    Ok(())
}

/// TSFN that calls the user's `onChunk`. Chunks go to the user; `StopFlushed` resolves the
/// deferred in the same queue (without calling the user's onChunk). The pump function is a no-op.
/// After resolving, unref the chunk TSFN and release onChunk's `napi_ref`.
#[allow(clippy::too_many_arguments)]
fn make_chunk_tsfn(
    env: &Env,
    stop_phase: Arc<Mutex<StopPhase>>,
    user: Arc<UserCallback>,
    chunk_weak: ChunkTsfnWeakCell,
    terminal: TerminalError,
    report: FinalReport,
    on_event: Option<Arc<UserCallback>>,
    lifecycle: Option<Arc<lifecycle::LifecycleDelivery>>,
) -> napi::Result<Arc<ChunkTsfn>> {
    let pump =
        env.create_function_from_closure("flexaudioChunkPump", |ctx| ctx.env.get_undefined())?;
    let tsfn =
        pump.create_threadsafe_function::<ChunkEmit, Unknown, _, ErrorStrategy::Fatal>(0, {
            let user = user;
            let chunk_weak = chunk_weak.clone();
            move |ctx: ThreadSafeCallContext<ChunkEmit>| match ctx.value {
                ChunkEmit::StartupDropped(event) => {
                    call_ordered_event(&ctx.env, on_event.as_ref(), *event)?;
                    Ok(Vec::<Unknown>::new())
                }
                ChunkEmit::Chunk(chunk) => {
                    let denied = matches!(
                        *terminal.lock().unwrap_or_else(lock_poisoned),
                        Some(StreamTerminal(_))
                    );
                    if denied
                        && !(chunk.frames == 0
                            && (chunk.vad_events.is_some()
                                || chunk
                                    .secondary
                                    .as_ref()
                                    .is_some_and(|tap| tap.vad_events.is_some())))
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
                ChunkEmit::VadFlushed(deferred, error) => {
                    attachment::settle_flush(&ctx.env, deferred, error)?;
                    Ok(Vec::<Unknown>::new())
                }
                ChunkEmit::VadFailed {
                    error,
                    stage,
                    epoch,
                } => {
                    // The failure's terminal carrier was queued on this same TSFN before
                    // this value, so its onChunk call ran first. A VAD-only failure is
                    // reported to onEvent; it must never become an uncaught exception.
                    call_ordered_event(
                        &ctx.env,
                        on_event.as_ref(),
                        attachment::failure_event(&error, stage, epoch),
                    )?;
                    Ok(Vec::<Unknown>::new())
                }

                ChunkEmit::StopFlushed => {
                    if let Some(lifecycle) = &lifecycle {
                        if lifecycle.defer_stop() {
                            return Ok(Vec::<Unknown>::new());
                        }
                        lifecycle.finish();
                    }
                    for deferred in take_stop_waiters(&stop_phase) {
                        settle_stop(ctx.env.raw(), deferred, &report);
                    }
                    // Release the event-loop reference after termination and resolution (ordering contract).
                    unref_chunk_weak(&chunk_weak, &ctx.env);
                    user.release();
                    if let Some(callback) = &on_event {
                        callback.release();
                    }
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
    user: Arc<UserCallback>,
    report: FinalReport,
    on_event: Option<Arc<UserCallback>>,
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
            if let Some(callback) = &on_event {
                callback.release();
            }
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

/// JS device information (`JsDeviceInfo`). `sourceKind` is a string ("mic"|"system"|"process").
#[napi(object)]
pub struct JsDeviceInfo {
    pub id: String,
    pub name: String,
    pub source_kind: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub format_available: bool,
    pub is_loopback: bool,
    pub is_default: bool,
}

/// JS process information (`JsProcessInfo`, an element of `processes()`). A candidate for per-process capture.
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

/// JS audio chunk (`JsAudioChunk`). `data` is interleaved f32 (len = frames * channels).
/// `seq` (u64) uses BigInt to avoid precision loss. `flags` is an unsigned 32-bit bitmask.
///
/// Delivery shape (0.3.0): `onChunk` in `openStream(options, onChunk)` receives **one argument**
/// (this primary chunk). Secondary tap (`secondaryOutput`) chunks arrive in the primary chunk's
/// `secondary` property, rather than a second argument. Finalized VAD events also arrive on
/// the selected `vad.tap` chunk's `vadEvents`, rather than a separate callback
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
    pub arrival_ns: i64,
    pub seq: BigInt,
    pub flags: u32,
    pub dropped_before: u32,
    pub peak: f64,
    pub rms: f64,
    /// Attached streaming events on the selected tap only.
    #[napi(ts_type = "AttachedVadEvent[]")]
    pub vad_events: Option<Vec<vad::JsVadEvent>>,
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
    pub arrival_ns: i64,
    pub seq: BigInt,
    pub flags: u32,
    pub dropped_before: u32,
    /// Computed from f32 before quantization (preserves meter precision even with s16).
    pub peak: f64,
    pub rms: f64,
    /// Attached streaming events on the selected tap only.
    #[napi(ts_type = "AttachedVadEvent[]")]
    pub vad_events: Option<Vec<vad::JsVadEvent>>,
}

/// JS native format (returned by `FlexStream.nativeFormat`).
#[napi(object)]
pub struct JsNativeFormat {
    pub sample_rate: u32,
    pub channels: u16,
}

/// Stream notification. permissionDenied is terminal, with permission and
/// cause/remedy in message. permissionPending remains for custom
/// Rust backends; built-in sources on every OS do not emit this advisory.
/// A custom permissionPending is advisory: consent remains undecided and capture continues.
/// silenceWhileSourceActive
/// is advisory: the system-audio diagnosis is inconclusive and capture continues.
/// Missing permission and genuine digital silence remain possible in that case.
/// vadFailed is advisory for capture: the attached VAD session failed,
/// carrying its typed code, and capture, PCM delivery, pause/resume, switchSource and
/// stop keep working. It is delivered after that failure's `epochEnd(error)` carrier.
#[napi(object)]
pub struct JsStreamEvent {
    /// Present on startupSamplesDropped: suppressed native microphone frames.
    pub frames: Option<BigInt>,
    /// Present on startupSamplesDropped: outOfRange (includes non-finite input).
    #[napi(ts_type = "'outOfRange'")]
    pub reason: Option<String>,
    /// Present on permissionDenied and permissionPending: microphone | systemAudio.
    #[napi(ts_type = "'microphone' | 'systemAudio'")]
    pub permission: Option<String>,
    #[napi(js_name = "type")]
    pub kind: String,
    pub count: Option<BigInt>,
    pub error: Option<JsAudioError>,
    pub loss: Option<JsAudioLoss>,
    pub message: Option<String>,
    /// Present on vadFailed: the epoch whose VAD session failed.
    pub epoch: Option<u32>,
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
    pub error: Option<JsAudioError>,
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

/// Options for openStream.
#[napi(object)]
pub struct OpenOptions {
    /// Strict by default on open; false allows ignored settings. Omission retains on switch.
    pub strict: Option<bool>,
    pub ring_capacity_chunks: Option<f64>,
    #[napi(ts_type = "(event: LifecycleEvent) => void")]
    pub on_lifecycle_event: Option<napi::JsFunction>,
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
    /// VAD-compatible VAD options. Validated before any device is opened.
    /// Requires the shared canonical capture feed with exact producer provenance.
    #[napi(ts_type = "StreamVadOptions")]
    pub vad: Option<napi::JsUnknown>,
    /// true enables recording-time noise suppression. **Only available with 48000 Hz output**
    /// (RNNoise is fixed at 48kHz). Applied once to the internal 48kHz/stereo canonical form;
    /// both primary and secondary taps receive denoised audio (+10ms fixed latency). Setting true
    /// at any other rate makes `openStream` throw InvalidArg. Omission/false disables noise suppression.
    pub denoise: Option<bool>,
    /// Enable the macOS silence diagnostic probe. Default true.
    pub silence_probe: Option<bool>,
    /// Keep Windows classic system loopback active by rendering silence. Omitted on
    /// open defaults to true; omitted on switchSource retains the current value.
    /// Ignored on other platforms and process/exclusion capture paths.
    pub windows_classic_loopback_keepalive: Option<bool>,
    /// Secondary output tap. When specified, returns paired chunks in another format simultaneously
    /// (`primary.secondary` in `onChunk`). Omission means no secondary tap, preserving previous behavior.
    pub secondary_output: Option<SecondaryOutputOptions>,
}

// ---------------------------------------------------------------------------
// Conversion helpers
// ---------------------------------------------------------------------------

/// Resolve open defaults once at the binding boundary.
fn capture_open_options(options: &OpenOptions) -> flexaudio::OpenOptions {
    switch_open_options(
        &flexaudio::OpenOptions::default(),
        options.windows_classic_loopback_keepalive,
    )
    .strict(
        options
            .strict
            .unwrap_or(flexaudio::OpenOptions::default().strict),
    )
}

/// An omitted switch option retains the current facade-owned value.
fn switch_open_options(
    current: &flexaudio::OpenOptions,
    keepalive: Option<bool>,
) -> flexaudio::OpenOptions {
    match keepalive {
        Some(enabled) => current.clone().windows_classic_loopback_keepalive(enabled),
        None => current.clone(),
    }
}

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
        format_available: info.format_available,
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
        arrival_ns: chunk.pts_ns,
        seq: BigInt::from(chunk.seq),
        flags: chunk.flags.bits(),
        dropped_before: chunk.dropped_before,
        peak: chunk.peak as f64,
        rms: chunk.rms as f64,
        // Unset by default. The bridge overwrites it when integrated VAD is enabled on the primary tap.
        vad_events: None,
        // The pairing bridge inserts a timestamp-matched secondary chunk (undefined if none).
        secondary: None,
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
    let mut config = StreamConfig::new(kind);
    config.output = output;
    config.secondary_output = secondary_output;
    config.device_id = options.device_id.clone();
    config.target_pid = target_pid;
    // The facade checks source-specific mode and exclusion semantics.
    config.mode = mode;
    config.exclude_self = options.exclude_self.unwrap_or(false);
    config.exclude_pids = exclude_pids;
    config.gain = options.gain.unwrap_or(1.0) as f32;
    config.silence_probe = options.silence_probe.unwrap_or(true);
    // mix only (the facade ignores these for mic/system/process). Per-side gains default to 1.0.
    config.mix_mic_device_id = options.mic_device_id.clone();
    config.mix_system_device_id = options.system_device_id.clone();
    config.mix_mic_gain = options.mic_gain.unwrap_or(1.0) as f32;
    config.mix_system_gain = options.system_gain.unwrap_or(1.0) as f32;
    if let Some(ms) = options.chunk_ms {
        if ms != 20.0 {
            return Err(to_napi_err(flexaudio::core::chunk_ms_error(
                flexaudio::core::ChunkMsOption::JavaScript,
            )));
        }
        config.chunk_ms = 20;
    }
    if let Some(capacity) = options.ring_capacity_chunks {
        if !capacity.is_finite()
            || capacity.fract() != 0.0
            || !(1.0..=9_007_199_254_740_991.0).contains(&capacity)
            || capacity > usize::MAX as f64
        {
            return Err(to_napi_err(flexaudio::Error::InvalidArg(
                "ringCapacityChunks must be a positive safe integer fitting native capacity".into(),
            )));
        }
        config.ring_capacity_chunks = capacity as usize;
    }
    Ok(config)
}

fn config_at_boundary(env: &Env, options: &OpenOptions) -> napi::Result<StreamConfig> {
    build_config(options).map_err(|error| {
        if options.chunk_ms.is_some_and(|ms| ms != 20.0) {
            boundary::throw_error(
                env,
                flexaudio::core::chunk_ms_error(flexaudio::core::ChunkMsOption::JavaScript),
            )
        } else {
            error
        }
    })
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
    mock_source: bool,
    result_tx: mpsc::Sender<flexaudio::core::Result<()>>,
    request: settings::Request,
    lifecycle_handler: Option<Arc<UserCallback>>,
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
    /// Read the facade-owned snapshot; the binding stores it only after shutdown.
    Settings(mpsc::Sender<Option<flexaudio::SettingsReport>>),
    /// Hot-swap the input source (return the result synchronously).
    Switch(Box<SwitchCmd>),
    /// Pause delivery.
    Pause,
    /// Resume delivery.
    Resume(mpsc::Sender<flexaudio::Result<()>>),
    /// Change input gain (linear multiplier). napi validates the value before sending.
    SetGain(f32),
    /// Return a current-value snapshot synchronously (for getters).
    Query(QueryCmd),
    FlushVad(SendDeferred),
}

/// Secondary tap encoding; core owns its rate and channel configuration.
#[derive(Clone, Copy)]
struct SecondaryTapCfg {
    encoding: SecEncoding,
}

/// Pair PCM taps within the timestamp window and deliver to `onChunk`.
/// One capture owner supplies VAD events; tap selection changes only their carrier.
/// Denoising runs in core before canonical capture and output conversion.
struct PairingBridge {
    // Only custom test backends need a binding snapshot; native streams own theirs.
    settings: Option<flexaudio::SettingsReport>,
    lifecycle: Option<Arc<lifecycle::LifecycleDelivery>>,
    strict: bool,
    on_chunk: ChunkTsfn,
    attachment: Option<attachment::VadBridge>,
    stop_phase: Arc<Mutex<StopPhase>>,
    report: FinalReport,
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
    last_secondary: SecondaryCarrierMetadata,
}

/// Last observed secondary timeline; reused by event-only stop carriers.
#[derive(Default, Clone, Copy)]
struct SecondaryCarrierMetadata {
    frame_index: u64,
    seq: u64,
    dropped_before: u32,
}

fn secondary_carrier_counters(metadata: SecondaryCarrierMetadata) -> (BigInt, BigInt, u32) {
    (
        BigInt::from(metadata.frame_index),
        BigInt::from(metadata.seq),
        metadata.dropped_before,
    )
}

fn secondary_event_carrier(
    encoding: Option<SecEncoding>,
    pts: i64,
    metadata: SecondaryCarrierMetadata,
) -> JsSecondaryChunk {
    let (frame_index, seq, dropped_before) = secondary_carrier_counters(metadata);
    let (data, encoding) = match encoding {
        Some(SecEncoding::S16) => (Either::A(Int16Array::new(Vec::new())), "s16"),
        _ => (Either::B(Float32Array::new(Vec::new())), "f32"),
    };
    JsSecondaryChunk {
        frame_index,
        data,
        encoding: encoding.into(),
        frames: 0,
        pts_ns: pts,
        arrival_ns: pts,
        seq,
        flags: 0,
        dropped_before,
        peak: 0.0,
        rms: 0.0,
        vad_events: None,
    }
}

impl PairingBridge {
    /// Deliver all residual secondaries, then one final zero-frame terminator.
    fn emit_terminator(&mut self) {
        while let Some(secondary) = self.secondary_fifo.pop_front() {
            self.emit_secondary_envelope(secondary);
        }
        let carrier = JsAudioChunk {
            frame_index: BigInt::from(self.last_primary_frame_index),
            data: Float32Array::new(Vec::new()),
            frames: 0,
            pts_ns: self.last_emitted_primary_pts,
            arrival_ns: self.last_emitted_primary_pts,
            seq: BigInt::from(self.last_primary_seq),
            flags: 0,
            dropped_before: self.last_primary_dropped,
            peak: 0.0,
            rms: 0.0,
            vad_events: None,
            secondary: None,
        };
        self.on_chunk.call(
            ChunkEmit::Chunk(Box::new(carrier)),
            ThreadsafeFunctionCallMode::NonBlocking,
        );
    }

    /// Marshal primary PCM, attach pending events for its selected carrier, and queue it.
    fn on_primary(&mut self, chunk: AudioChunk) {
        self.last_primary_seq = chunk.seq;
        self.last_primary_frame_index = chunk.frame_index;
        self.last_primary_dropped = chunk.dropped_before;
        let mut js = chunk_to_js(chunk);
        if let Some(attachment) = self.attachment.as_mut() {
            if attachment.tap == VadTap::Primary {
                js.vad_events = Some(attachment.take_events());
            }
        }
        self.primary_fifo.push_back(js);
    }

    /// Marshal secondary PCM to its encoding and queue it; attach events at delivery.
    fn on_secondary(&mut self, chunk: SecondaryChunk) {
        let Some(cfg) = self.secondary else {
            return; // Do nothing without secondary tap settings (defensive).
        };
        self.last_secondary = SecondaryCarrierMetadata {
            frame_index: chunk.frame_index,
            seq: chunk.seq,
            dropped_before: chunk.dropped_before,
        };
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
            arrival_ns: pts_ns,
            seq,
            flags,
            dropped_before,
            peak,
            rms,
            vad_events: self
                .attachment
                .as_mut()
                .filter(|owner| owner.tap == VadTap::Secondary)
                .map(|_| Vec::new()),
        };
        self.secondary_fifo.push_back(js);
    }

    fn emit_secondary_envelope(&mut self, mut secondary: JsSecondaryChunk) {
        if let Some(attachment) = self
            .attachment
            .as_mut()
            .filter(|owner| owner.tap == VadTap::Secondary)
        {
            secondary.vad_events = Some(attachment.take_events());
        }
        let carrier = JsAudioChunk {
            data: Float32Array::new(Vec::new()),
            frames: 0,
            frame_index: BigInt::from(self.last_primary_frame_index),
            pts_ns: secondary.pts_ns.max(self.last_emitted_primary_pts),
            arrival_ns: secondary.pts_ns.max(self.last_emitted_primary_pts),
            seq: BigInt::from(self.last_primary_seq),
            flags: 0,
            dropped_before: self.last_primary_dropped,
            peak: 0.0,
            rms: 0.0,
            vad_events: None,
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
                            if self.attachment.is_some() {
                                // Preserve this secondary PCM on a real primary carrier.
                                // Empty midstream envelopes look like stop to consumers;
                                // the secondary retains its own original clock metadata.
                                break self.secondary_fifo.pop_front();
                            }
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
                if let Some(attachment) = self
                    .attachment
                    .as_mut()
                    .filter(|owner| owner.tap == VadTap::Secondary)
                {
                    secondary.vad_events = Some(attachment.take_events());
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

/// Recording stream handle (`FlexStream`). Internally, the bridge thread polls captured audio
/// and sends chunks/events to JS through TSFN.
#[napi]
pub struct FlexStream {
    // Native reports are queried from Stream while running and retained here at shutdown.
    settings: Arc<Mutex<Option<flexaudio::SettingsReport>>>,
    lifecycle: Option<Arc<lifecycle::LifecycleDelivery>>,
    stop_flag: Arc<AtomicBool>,
    inner: Arc<Mutex<StreamInner>>,
    stop_phase: Arc<Mutex<StopPhase>>,
    chunk_tsfn: Arc<ChunkTsfn>,
    settle_tsfn: SettleTsfn,
    terminal: TerminalError,
    report: FinalReport,
    mock_source: bool,
}

impl FlexStream {
    /// Accept an already `start()`ed Stream and the emitting [`PairingBridge`], then spawn a bridge
    /// thread. Stream is Send, so move it to the thread (poll_* takes
    /// &mut self, requiring thread ownership). The bridge holds integrated VAD / secondary tap settings.
    #[allow(clippy::too_many_arguments)]
    fn spawn(
        env: &Env,
        stream: flexaudio::Stream,
        mut bridge: PairingBridge,
        on_event: Option<EventTsfn>,
        chunk_tsfn: Arc<ChunkTsfn>,
        settle_tsfn: SettleTsfn,
        terminal: TerminalError,
        mock_source: bool,
    ) -> napi::Result<Self> {
        let report = bridge.report.clone();
        let settings = Arc::new(Mutex::new(bridge.settings.take()));
        let thread_settings = settings.clone();
        let lifecycle = bridge.lifecycle.clone();
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
                let (stream, mut bridge, on_event) = resources
                    .lock()
                    .unwrap_or_else(lock_poisoned)
                    .take()
                    .expect("bridge owns acquired resources");
                let mut stream =
                    settings_owner::SettingsOwner::new(stream, thread_settings.clone());
                loop {
                    // Process commands together in the same iteration as polling.
                    while let Ok(cmd) = cmd_rx.try_recv() {
                        match cmd {
                            BridgeCmd::Settings(result_tx) => {
                                let report = stream.effective_settings().cloned().or_else(|| {
                                    thread_settings.lock().unwrap_or_else(lock_poisoned).clone()
                                });
                                let _ = result_tx.send(report);
                            }
                            BridgeCmd::Switch(sw) => {
                                let sw = *sw;
                                let strict = sw.request.strict.unwrap_or(bridge.strict);
                                let mut previous =
                                    stream.effective_settings().cloned().or_else(|| {
                                        thread_settings.lock().unwrap_or_else(lock_poisoned).clone()
                                    });
                                if let Some(entry) = previous.as_mut().and_then(|r| {
                                    r.entries
                                        .iter_mut()
                                        .find(|entry| entry.setting == flexaudio::SettingKey::Gain)
                                }) {
                                    entry.effective = flexaudio::SettingValue::Float(stream.gain());
                                }
                                let candidate = sw.request.resolve(previous.as_ref(), strict);
                                let r = candidate.and_then(|candidate| {
                                    // Pending notifications belong to the currently registered
                                    // handler; replacements take effect only at commit.
                                    forward_stream_events(
                                        &mut stream,
                                        on_event.as_ref(),
                                        &bridge.on_chunk,
                                        &thread_terminal,
                                        bridge.lifecycle.as_deref(),
                                    );
                                    let r = if sw.mock_source {
                                        stream.switch_backend(lifecycle::mock_backend())
                                    } else {
                                        let options = sw
                                            .request
                                            .open_options(switch_open_options(
                                                stream.open_options(),
                                                sw.request.keepalive,
                                            ))
                                            .strict(strict);
                                        stream
                                            .switch_source_with_options(sw.request.config, options)
                                    };
                                    if r.is_ok() {
                                        bridge.strict = strict;
                                        if sw.mock_source {
                                            *thread_settings.lock().unwrap_or_else(lock_poisoned) =
                                                Some(candidate);
                                        }
                                    }
                                    r
                                });
                                if let Some(handler) = sw.lifecycle_handler {
                                    if let Some(delivery) = &bridge.lifecycle {
                                        if r.is_ok() {
                                            delivery.replace(handler);
                                        } else {
                                            delivery.release(handler);
                                        }
                                    }
                                }
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
                            BridgeCmd::FlushVad(deferred) => {
                                bridge.drain_stream(
                                    &mut stream,
                                    on_event.as_ref(),
                                    &thread_terminal,
                                );
                                bridge.flush_attachment(false);
                                // flushVad reinitializes on success, so a previous
                                // VAD-only failure no longer applies to this stream.
                                bridge.report_attachment_failure();
                                let error = bridge
                                    .attachment
                                    .as_ref()
                                    .and_then(|owner| owner.error.clone());
                                bridge.on_chunk.call(
                                    ChunkEmit::VadFlushed(deferred, error),
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
                    // Drain all arriving primary/secondary chunks into the bridge. Run VAD/quantization, pair within
                    // the pts window, and deliver to onChunk.
                    bridge.drain_stream(&mut stream, on_event.as_ref(), &thread_terminal);
                    // A VAD-only failure never becomes the capture terminal: it is
                    // reported once through vadFailed and retained as cleanup.
                    bridge.report_attachment_failure();
                    thread::sleep(POLL_INTERVAL);
                }
                // Drain chunks remaining in the rings before stopping, then deliver pairs.
                bridge.drain_stream(&mut stream, on_event.as_ref(), &thread_terminal);
                // Stop ordering:
                //   1. Audio stop-flush: core flush queues the trailing tail (denoise delay line + resampler
                //      remainder) into the rings.
                //   2. Feed that tail through VAD into the FIFOs, then deliver audio through normal pairing.
                //   3. flushVad: force-finalize the open utterance and reliably deliver the final VAD events with
                //      a dedicated trailing carrier (independent of pairing; no drops even without a primary tail).
                // This delivers both trailing recorded audio (2) and final VAD events (3). Audio stop-flush and
                // flushVad are separate (audio samples versus VAD events).
                let _ = stream.stop_checked();
                // Stop can discover a pending backend denial. The shared drain
                // forwards it and suppresses failed PCM before accepting the tail.
                bridge.drain_stream(&mut stream, on_event.as_ref(), &thread_terminal);
                if stream.terminal_error().is_none() {
                    while let Some(secondary) = bridge.secondary_fifo.pop_front() {
                        bridge.emit_secondary_envelope(secondary);
                    }
                }
                if bridge.attachment.is_some() {
                    bridge.flush_attachment(true);
                    // Deliver the one vadFailed notification (if not already sent)
                    // before stop() settles; the retained report carries it afterwards.
                    bridge.report_attachment_failure();
                } else if stream.terminal_error().is_none() {
                    bridge.emit_terminator();
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
            mock_source,
            settings,
            lifecycle,
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
    /// Snapshot of the last successfully committed capture configuration.
    #[napi(ts_return_type = "SettingsReport")]
    pub fn effective_settings(&self, env: Env) -> napi::Result<JsObject> {
        let sender = self
            .inner
            .lock()
            .unwrap_or_else(lock_poisoned)
            .cmd_tx
            .clone();
        if let Some(sender) = sender {
            let (result_tx, result_rx) = mpsc::channel();
            if sender.send(BridgeCmd::Settings(result_tx)).is_ok() {
                if let Ok(Some(report)) = result_rx.recv() {
                    return settings::to_js(&env, &report);
                }
            }
        }
        let report = self.settings.lock().unwrap_or_else(lock_poisoned);
        settings::to_js(
            &env,
            report.as_ref().ok_or_else(|| {
                NapiError::new(
                    Status::GenericFailure,
                    "settings are unavailable for this custom backend",
                )
            })?,
        )
    }
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

    /// Stored **capture** terminal failure, or undefined when capture has not terminally
    /// failed. An attached streaming VAD failure is not a capture failure: it is
    /// reported once as a `vadFailed` event and retained in `shutdownReport()`'s
    /// cleanup causes, so this remains undefined after a VAD-only failure.
    /// Remains available after stop and does not consume onEvent notifications.
    #[napi]
    pub fn terminal_error(&self) -> Either<JsStreamEvent, Undefined> {
        self.terminal
            .lock()
            .unwrap_or_else(lock_poisoned)
            .clone()
            .map(|StreamTerminal(error)| Either::A(terminal_event(error)))
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
    /// Request a switch using `OpenOptions` on the bridge thread and return
    /// synchronously (`undefined` on success, exception on failure). The output format (`outputRate`/
    /// `outputChannels`) cannot change during a switch (it would change frames in a continuous stream).
    /// Requesting a format change makes `switchSource` throw InvalidArg. Chunk
    /// `seq` stays continuous across switches; the first chunk afterward has the DISCONTINUITY flag.
    /// An explicit `options.windowsClassicLoopbackKeepalive` replaces the current value;
    /// omission retains it. `options.denoise` and `options.vad` are ignored;
    /// the existing denoise and VAD settings remain in effect.
    /// `options.gain` is ignored (gain is stream state; use `setGain` to change it).
    ///
    /// A streaming VAD failure does not block a source switch: the switch closes
    /// the old epoch and the next capture frame reinitializes the VAD session.
    ///
    /// Throws once `stop()` has been requested, including while shutdown is pending.
    #[napi]
    pub fn switch_source(&self, env: Env, options: OpenOptions) -> napi::Result<()> {
        // Reject before retaining a handler or queueing work. stop() can leave the
        // bridge in teardown while its command receiver is still alive.
        if self.stop_flag.load(Ordering::SeqCst) {
            return Err(NapiError::new(
                Status::GenericFailure,
                "stream already stopped",
            ));
        }
        if let Some(StreamTerminal(error)) =
            self.terminal.lock().unwrap_or_else(lock_poisoned).clone()
        {
            return Err(boundary::throw_error(&env, error));
        }
        // As in openStream, build_config converts options → StreamConfig.
        let mock_source = options.kind == "mock";
        if mock_source && !self.mock_source {
            return Err(boundary::throw_error(&env, flexaudio::Error::InvalidArg(
                "mock source switches require a mock stream because real streams must select a capture backend".into(),
            )));
        }
        let mut options = options;
        if mock_source {
            options.kind = "mic".into();
        }
        let config = config_at_boundary(&env, &options)?;

        // Send a command to the bridge thread and synchronously receive the result.
        let cmd_tx = {
            let g = self.inner.lock().unwrap_or_else(lock_poisoned);
            g.cmd_tx.clone().ok_or_else(|| {
                NapiError::new(Status::GenericFailure, "stream already stopped".to_string())
            })?
        };
        let (result_tx, result_rx) = mpsc::channel();
        let request = settings::Request::new(&options, config)?;
        let lifecycle_handler = if self.lifecycle.is_some() {
            options
                .on_lifecycle_event
                .as_ref()
                .map(|handler| lifecycle::retain(&env, handler))
                .transpose()?
        } else {
            None
        };
        cmd_tx
            .send(BridgeCmd::Switch(Box::new(SwitchCmd {
                mock_source,
                result_tx,
                request,
                lifecycle_handler,
            })))
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

    /// Unpause and resume delivery. The first chunk afterward is a discontinuity, which also
    /// reinitializes a failed streaming VAD session; it never throws for that failure.
    /// Throws if `stop()` has already completed.
    #[napi]
    pub fn resume(&self, env: Env) -> napi::Result<()> {
        if let Some(StreamTerminal(error)) =
            self.terminal.lock().unwrap_or_else(lock_poisoned).clone()
        {
            return Err(boundary::throw_error(&env, error));
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

    /// Drain an attached epoch and deliver its closing carrier before promise settlement.
    #[napi(ts_return_type = "Promise<void>")]
    pub fn flush_vad(&self, env: Env) -> napi::Result<JsObject> {
        let sender = self
            .inner
            .lock()
            .unwrap_or_else(lock_poisoned)
            .cmd_tx
            .clone()
            .ok_or_else(|| NapiError::new(Status::GenericFailure, "stream already stopped"))?;
        let (deferred, promise) = create_js_promise(&env)?;
        sender
            .send(BridgeCmd::FlushVad(deferred))
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
            let retained = Arc::new(Mutex::new(Some(h)));
            let worker_handle = retained.clone();
            let worker_report = report.clone();
            let worker_terminal = terminal.clone();
            if let Err(error) = thread::Builder::new()
                .name("flexaudio-napi-reaper".into())
                .spawn(move || {
                    if let Some(handle) = worker_handle.lock().unwrap_or_else(lock_poisoned).take()
                    {
                        if handle.join().is_err() {
                            record_bridge_failure(&worker_report, &worker_terminal);
                        }
                    }
                })
            {
                record_cleanup_failure(&report, flexaudio::core::spawn_error("GC reaper", error));
                if let Some(handle) = retained.lock().unwrap_or_else(lock_poisoned).take() {
                    if handle.join().is_err() {
                        record_bridge_failure(&report, &terminal);
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// DeviceWatcherHandle (class)
// ---------------------------------------------------------------------------

/// Device attachment/removal watcher handle (`DeviceWatcherHandle`).
#[napi]
pub struct DeviceWatcherHandle {
    stop_flag: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    on_event: Option<DeviceTsfn>,
}

impl DeviceWatcherHandle {
    fn shutdown(&mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            if h.join().is_err() {
                if let Some(callback) = &self.on_event {
                    callback.call(
                        device_event_to_js(DeviceEvent::TerminalError {
                            error: flexaudio::Error::Backend(
                                "device bridge worker panicked; restart device watching".into(),
                            )
                            .with_context(
                                flexaudio::core::ErrorContext::new(
                                    flexaudio::core::Operation::Join,
                                ),
                            ),
                        }),
                        ThreadsafeFunctionCallMode::NonBlocking,
                    );
                }
            }
        }
        self.on_event.take();
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
/// On macOS, enumeration never prompts. With undecided microphone consent it throws
/// a typed Backend error before any HAL/device/format query. Grant the responsible
/// host access or use the bounded capture-open consent flow, then retry enumeration.
/// Denied/restricted consent throws PermissionDenied. Unreadable mandatory DeviceInfo formats
/// use format_available=false and the 0/0 sentinel; other entries remain usable.
/// Windows enumeration does not preflight microphone permission; native enumeration
/// proceeds. Microphone capture retains its separate existing permission preflight.
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
/// Supply onEvent for startupSamplesDropped, runtime denial and silenceWhileSourceActive
/// advisories. Without onEvent, terminalError() and stop() expose terminal failures
/// only; they do not expose advisories. macOS microphone consent is checked
/// before any HAL query. Bundled prompts wait at most 30 s; an unanswered prompt
/// throws a typed Backend error without starting capture. Answer the prompt and retry.
/// Undecided consent without a nonempty NSMicrophoneUsageDescription fails immediately;
/// configure the bundle or grant the responsible host access before retrying.
/// Opening is synchronous; use a worker thread for GUI hosts.
/// The 30 s bound covers undecided consent, not authorized native queries.
/// Built-in sources on every OS do not emit permissionPending; that advisory
/// remain for custom Rust backends and compatibility. Each microphone start requires
/// decided consent; authorization changes and revocation are not continuously monitored.
/// With onEvent, startupSamplesDropped precedes the affected generation's first
/// onChunk and attached VAD epochStart. Recovered and other advisories use
/// independent callbacks and may arrive before or after the startup notice.
/// Windows checks Microphone privacy, including
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
/// `denoise`, `secondaryOutput` and `vad` are independent optional processing features.
/// Denoise requires 48 kHz output and affects both taps. The attached VAD consumes
/// the canonical capture feed; `vad.tap` selects the event carrier only. Both standalone
/// and attached VAD default to an unbounded maximum speech duration. Call `flushVad()`
/// to finish/reset its current epoch and await delivery of closing events.
///
/// `onChunk` receives one primary chunk; secondary PCM appears as `chunk.secondary`.
/// VAD events appear on the selected tap as `vadEvents`. The 20 ms chunk size is fixed
/// because capture conversion and event ordering share that cadence. Avoid synchronous
/// heavy work in callbacks; it delays delivery and stop settlement.
#[napi]
pub fn open_stream(
    env: Env,
    mut options: OpenOptions,
    #[napi(ts_arg_type = "(chunk: JsAudioChunk) => void")] on_chunk: Function<
        JsAudioChunk,
        Unknown,
    >,
    #[napi(ts_arg_type = "((event: JsStreamEvent) => void) | undefined | null")] on_event: Option<
        Function<JsStreamEvent, Unknown>,
    >,
) -> napi::Result<FlexStream> {
    let mock_source = cfg!(feature = "test-faults") && options.kind == "mock";
    if mock_source {
        options.kind = "mic".into();
    }
    let config = config_at_boundary(&env, &options)?;
    let request = settings::Request::new(&options, config.clone())?;
    let settings = request
        .resolve(
            None,
            options
                .strict
                .unwrap_or(flexaudio::OpenOptions::default().strict),
        )
        .map_err(|error| boundary::throw_error(&env, error))?;
    let output_rate = config.output.sample_rate;
    let denoise_enabled = options.denoise.unwrap_or(false);
    check_denoise_rate(denoise_enabled, output_rate)?;
    let attachment_options = options
        .vad
        .as_ref()
        .map(|value| {
            // SAFETY: borrow the live JS value during synchronous parsing only.
            let value = unsafe { napi::JsUnknown::from_raw_unchecked(env.raw(), value.raw()) };
            vad::stream_options(value, options.secondary_output.is_some())
        })
        .transpose()
        .map_err(|error| vad::throw_boundary(&env, error))?;
    let attachment = attachment_options
        .map(|(params, options, tap)| attachment::VadBridge::new(params, options, tap))
        .transpose()
        .map_err(|error| attachment::throw_error(&env, error))?;
    // Parse secondary tap encoding / VAD tap.
    let secondary = match &options.secondary_output {
        Some(s) => Some(SecondaryTapCfg {
            encoding: parse_sec_encoding(s.encoding.as_deref())?,
        }),
        None => None,
    };
    let mut stream = if mock_source {
        flexaudio::Stream::open(config, lifecycle::mock_backend())
    } else {
        flexaudio::open_with_options(config, request.open_options(capture_open_options(&options)))
    }
    .map_err(|error| boundary::throw_error(&env, error))?;
    if denoise_enabled {
        stream.set_denoise(true);
    }
    if attachment.is_some() {
        stream
            .enable_capture_tap()
            .map_err(|error| boundary::throw_error(&env, error))?;
    }
    start_bridge_stream(&env, &mut stream)?;

    let stop_phase = Arc::new(Mutex::new(StopPhase::Running));
    let terminal: TerminalError = Arc::new(Mutex::new(None));
    let report: FinalReport = Arc::new(Mutex::new(None));
    let user = make_user_chunk_cb(&env, &on_chunk)?;
    let (on_event, event_callback) = make_stream_event_callbacks(&env, on_event)?;
    let chunk_weak: ChunkTsfnWeakCell = Arc::new(OnceLock::new());
    let lifecycle = lifecycle::LifecycleDelivery::new(
        &env,
        options.on_lifecycle_event.as_ref(),
        chunk_weak.clone(),
    )?;
    let on_chunk = make_chunk_tsfn(
        &env,
        stop_phase.clone(),
        user.clone(),
        chunk_weak.clone(),
        terminal.clone(),
        report.clone(),
        event_callback.clone(),
        Some(lifecycle.clone()),
    )?;
    let settle_tsfn = make_settle_tsfn(
        &env,
        stop_phase.clone(),
        chunk_weak,
        user,
        report.clone(),
        event_callback,
    )?;
    let bridge = PairingBridge {
        settings: mock_source.then_some(settings),
        lifecycle: Some(lifecycle),
        strict: options
            .strict
            .unwrap_or(flexaudio::OpenOptions::default().strict),
        attachment,
        on_chunk: on_chunk.as_ref().clone(),
        stop_phase,
        report: report.clone(),
        secondary,
        primary_fifo: VecDeque::new(),
        secondary_fifo: VecDeque::new(),
        last_emitted_primary_pts: 0,
        last_primary_seq: 0,
        last_primary_frame_index: 0,
        last_primary_dropped: 0,
        last_secondary: SecondaryCarrierMetadata::default(),
    };
    FlexStream::spawn(
        &env,
        stream,
        bridge,
        on_event,
        on_chunk,
        settle_tsfn,
        terminal,
        mock_source,
    )
}

/// One event drain for normal watching and final owner teardown.
fn forward_device_events(watcher: &mut flexaudio::DeviceWatcher, callback: &DeviceTsfn) -> bool {
    let mut failed = false;
    while let Some(event) = watcher.poll_event() {
        failed |= matches!(event, DeviceEvent::TerminalError { .. });
        callback.call(
            device_event_to_js(event),
            ThreadsafeFunctionCallMode::NonBlocking,
        );
    }
    failed
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
    let retained_callback = on_event.clone();

    let handle = thread::Builder::new()
        .name("flexaudio-napi-devices".into())
        .spawn(move || {
            loop {
                if thread_stop.load(Ordering::SeqCst) {
                    break;
                }
                if forward_device_events(&mut watcher, &on_event) {
                    break;
                }
                thread::sleep(DEVICE_POLL_INTERVAL);
            }
            watcher.stop();
            forward_device_events(&mut watcher, &on_event);
        })
        .map_err(|error| {
            boundary::throw_error(&env, flexaudio::core::spawn_error("device bridge", error))
        })?;

    Ok(DeviceWatcherHandle {
        stop_flag,
        handle: Some(handle),
        on_event: Some(retained_callback),
    })
}

/// Test-only capability marker; absent from builds without fault injection.
#[cfg(feature = "test-faults")]
#[napi]
#[allow(non_upper_case_globals)]
pub const __vadTestFaults: bool = true;

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
/// `vad` attaches the canonical VAD owner; `onEvent` receives stream notifications
/// (including `vadFailed`). `vadFailAtChunk` fails the VAD session on that 1-based
/// capture chunk and `vadResetFailures` fails the next reinitializations, so a
/// VAD-only failure and its recovery path can be exercised without a model fault or device.
///
/// JS name is `__openMockStream`. Leading `__` marks it outside the public API. napi's default
/// conversion drops leading underscores, producing `openMockStream`, so `js_name` fixes the name.
#[cfg(feature = "test-faults")]
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
    vad: Option<napi::JsUnknown>,
    #[napi(ts_arg_type = "((event: JsStreamEvent) => void) | undefined | null")] on_event: Option<
        Function<JsStreamEvent, Unknown>,
    >,
    vad_fail_at_chunk: Option<f64>,
    vad_reset_failures: Option<f64>,
) -> napi::Result<FlexStream> {
    let (fault_chunk, reset_faults) =
        attachment::validate_fault_options(vad.is_some(), vad_fail_at_chunk, vad_reset_failures)
            .map_err(|error| vad::throw_boundary(&env, error))?;
    #[cfg(not(feature = "test-faults"))]
    let _ = (fault_chunk, reset_faults);
    let attachment_options = vad
        .map(|value| vad::stream_options(value, secondary_rate.is_some()))
        .transpose()
        .map_err(|error| vad::throw_boundary(&env, error))?;
    let attachment = attachment_options
        .map(|(params, options, tap)| attachment::VadBridge::new(params, options, tap))
        .transpose()
        .map_err(|error| attachment::throw_error(&env, error))?;
    #[cfg(feature = "test-faults")]
    let mut attachment = attachment;
    #[cfg(feature = "test-faults")]
    if let Some(bridge) = attachment.as_mut() {
        bridge.inject_faults(fault_chunk, reset_faults);
    }
    // Secondary tap (only when configured). Validate encoding and build marshaling settings.
    let secondary_cfg = match secondary_rate {
        Some(_) => Some(SecondaryTapCfg {
            encoding: parse_sec_encoding(secondary_encoding.as_deref())?,
        }),
        None => None,
    };
    let mut config = StreamConfig::new(SourceKind::Mic);
    config.output = OutputFormat {
        sample_rate,
        channels,
    };
    config.secondary_output = secondary_rate.map(|rate| OutputFormat {
        sample_rate: rate,
        channels: secondary_channels.unwrap_or(1),
    });
    let backend = Box::new(flexaudio::MockBackend::new(
        sample_rate,
        channels,
        freq_hz as f32,
    ));
    let mut stream = flexaudio::Stream::open(config, backend)
        .map_err(|error| boundary::throw_error(&env, error))?;
    if attachment.is_some() {
        stream
            .enable_capture_tap()
            .map_err(|error| boundary::throw_error(&env, error))?;
    }
    start_bridge_stream(&env, &mut stream)?;
    // Mock capture exercises the same attachment and delivery paths without audio hardware.
    let stop_phase = Arc::new(Mutex::new(StopPhase::Running));
    let terminal: TerminalError = Arc::new(Mutex::new(None));
    let report: FinalReport = Arc::new(Mutex::new(None));
    let user = make_user_chunk_cb(&env, &on_chunk)?;
    let (on_event, event_callback) = make_stream_event_callbacks(&env, on_event)?;
    let chunk_weak: ChunkTsfnWeakCell = Arc::new(OnceLock::new());
    let on_chunk = make_chunk_tsfn(
        &env,
        stop_phase.clone(),
        user.clone(),
        chunk_weak.clone(),
        terminal.clone(),
        report.clone(),
        event_callback.clone(),
        None,
    )?;
    let settle_tsfn = make_settle_tsfn(
        &env,
        stop_phase.clone(),
        chunk_weak,
        user,
        report.clone(),
        event_callback,
    )?;
    let bridge = PairingBridge {
        settings: None,
        lifecycle: None,
        strict: false,
        attachment,
        on_chunk: on_chunk.as_ref().clone(),
        stop_phase,
        report: report.clone(),
        secondary: secondary_cfg,
        primary_fifo: VecDeque::new(),
        secondary_fifo: VecDeque::new(),
        last_emitted_primary_pts: 0,
        last_primary_seq: 0,
        last_primary_frame_index: 0,
        last_primary_dropped: 0,
        last_secondary: SecondaryCarrierMetadata::default(),
    };
    FlexStream::spawn(
        &env,
        stream,
        bridge,
        on_event,
        on_chunk,
        settle_tsfn,
        terminal,
        true,
    )
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
/// Fixed latency of 480 samples per channel (10ms at 48kHz). Output is
/// the input delayed by one frame. The first frame is silence padding;
/// retrieve the remaining trailing frame with `Denoiser.flush()`.
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
            strict: None,
            ring_capacity_chunks: None,
            on_lifecycle_event: None,
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
            silence_probe: None,
            windows_classic_loopback_keepalive: None,
            denoise: None,
            secondary_output: None,
        }
    }

    #[test]
    fn capture_keepalive_open_and_switch_defaults() {
        let mut options = options_with_kind("system");
        assert!(capture_open_options(&options).windows_classic_loopback_keepalive);
        for current in [true, false] {
            options.windows_classic_loopback_keepalive = Some(current);
            assert_eq!(
                capture_open_options(&options).windows_classic_loopback_keepalive,
                current
            );
            let retained =
                flexaudio::OpenOptions::default().windows_classic_loopback_keepalive(current);
            assert_eq!(switch_open_options(&retained, None), retained);
            for replacement in [true, false] {
                assert_eq!(
                    switch_open_options(&retained, Some(replacement))
                        .windows_classic_loopback_keepalive,
                    replacement
                );
            }
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
            strict: None,
            ring_capacity_chunks: None,
            on_lifecycle_event: None,
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
            denoise: None,
            vad: None,
            silence_probe: None,
            windows_classic_loopback_keepalive: None,
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
        let info = {
            let mut device_value =
                DeviceInfo::new("node-1".to_string(), "Mic A".to_string(), SourceKind::Mic);
            device_value.format_available = true;
            device_value.sample_rate = 48_000;
            device_value.channels = 2;
            device_value.is_loopback = false;
            device_value.is_default = true;
            device_value
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
        let mut info = DeviceInfo::new(
            "id-x".to_string(),
            "Name X".to_string(),
            SourceKind::SystemLoopback,
        );
        info.format_available = true;
        info.sample_rate = 44_100;
        info.channels = 1;
        info.is_loopback = true;
        info.is_default = false;
        let js = device_info_to_js(info);
        assert_eq!(js.id, "id-x");
        assert_eq!(js.name, "Name X");
        assert_eq!(js.source_kind, "system");
        assert_eq!(js.sample_rate, 44_100);
        assert_eq!(js.channels, 1);
        assert!(js.is_loopback);
        assert!(!js.is_default);
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
    }
}

#[cfg(test)]
mod reproduction_tests {
    use super::*;

    #[test]
    fn binding_contract_error_kind() {
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
    fn binding_contract_invalid_arg_kind() {
        let mapped = to_napi_err(flexaudio::Error::InvalidArg("invalid rate".into()));
        assert_eq!(mapped.status, Status::InvalidArg);
    }

    #[test]
    fn binding_contract_error_control() {
        let mapped = to_napi_err(flexaudio::Error::Backend("control cause".into()));
        assert_eq!(mapped.status, Status::GenericFailure);
        assert!(mapped.reason.contains("control cause"));
    }

    #[test]
    fn binding_contract_counter_cast() {
        let event = event_to_js(Event::ChunkDropped { count: u64::MAX });
        assert!(
            event.count.as_ref().unwrap().get_u64() == (false, u64::MAX, true),
            "positive u64 drop count becomes negative i64"
        );
    }
    #[test]
    fn binding_contract_counter_control() {
        assert_eq!(
            event_to_js(Event::ChunkDropped { count: 7 })
                .count
                .unwrap()
                .get_u64(),
            (false, 7, true)
        );
    }

    #[test]
    fn binding_contract_event_union() {
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
            "silenceWhileSourceActive",
            "deviceLost",
            "error",
            "terminalError",
            "recoverableError",
            "shutdownError",
            "audioLoss",
            "clipped",
            "unknown",
            "vadFailed",
        ] {
            assert!(stream.contains(&format!("type: '{tag}'")), "missing {tag}");
        }
        assert!(stream.contains("type: 'chunkDropped'; count: bigint"));
        for tag in ["terminalError", "recoverableError", "shutdownError"] {
            assert!(stream.contains(&format!("type: '{tag}'; error: AudioError")));
        }
        assert!(stream.contains("type: 'audioLoss'; loss: AudioLoss"));
        assert!(
            stream.contains("type: 'vadFailed'; error: AudioError; epoch: number"),
            "an attached VAD failure must carry its typed error and failed epoch"
        );
    }

    #[test]
    fn binding_contract_device_event_union() {
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
}

// Build the test addon with cargo rustc --example capture_test_addon --features test-faults -- --cfg flexaudio_test_addon.
#[cfg(all(flexaudio_test_addon, feature = "test-faults"))]
mod reproduction_bridge;

#[cfg(test)]
mod silent_failure_binding_tests {
    use super::*;
    #[test]
    fn silent_failure_failed_error_creation_still_rejects_once() {
        let mut rejected = Vec::new();
        reject_with_fallback(
            Err::<&str, _>(()),
            |_| "plain fallback",
            |value| rejected.push(value),
        );
        assert_eq!(rejected, ["plain fallback"]);
    }
    #[test]
    fn silent_failure_secondary_tail_keeps_last_timeline_counters() {
        let metadata = SecondaryCarrierMetadata {
            frame_index: 9600,
            seq: u64::MAX,
            dropped_before: 17,
        };
        // Typed-array destruction requires a Node host; this shared production
        // counter projection can be checked without N-API or real audio hardware.
        let (frame_index, seq, dropped_before) = secondary_carrier_counters(metadata);
        assert_eq!(frame_index.get_u64(), (false, 9600, true));
        assert_eq!(seq.get_u64(), (false, u64::MAX, true));
        assert_eq!(dropped_before, 17);
    }
}
