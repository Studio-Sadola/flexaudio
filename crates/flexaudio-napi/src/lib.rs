//! flexaudio-napi — Node.js (N-API) addon.
//!
//! Bindings that let Node.js apps use flexaudio in-process. Low-latency streaming capture is
//! delivered to Node through callbacks.
//!
//! Design:
//! - Public functions are camelCase (`#[napi]` converts them to JS names).
//! - Chunks/events are sent to JS callbacks via `ThreadsafeFunction` (ErrorStrategy::Fatal).
//! - Constructing a `FlexStream` spawns a bridge thread; after `stream.start()` it polls
//!   `poll_chunk` / `poll_event` every 1ms and hands the results to the TSFN as NonBlocking.
//! - Stop is `stop(): Promise<void>`. The join is never done on the JS thread: after the last
//!   PCM and the `frames:0` terminator are queued on the same TSFN, one "end signal" is
//!   queued, and the Promise resolves when JS processes that signal. Drop (GC) does not block
//!   JS; the join happens on a reaper thread.
//!
//! No network access at runtime (napi is only the N-API bridge).

use std::collections::VecDeque;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use napi::bindgen_prelude::{
    AsyncTask, BigInt, Either, Float32Array, FromNapiValue, Function, Int16Array, Unknown,
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

// Core types of the three add-ons. They share names with the `#[napi]` wrappers (Vad /
// Denoiser), so they are imported under aliases.
use flexaudio_denoise::{DenoiseError, Denoiser as CoreDenoiser};
use flexaudio_encode::{EncodeError, FlacWriter};
use flexaudio_vad::{Vad as CoreVad, VadConfig, VadError, VadEvent};

/// pts window for pairing secondary-tap chunks (60ms = 3 chunks). The secondary lags the
/// primary by 20-60ms, so this window keeps the timing correspondence even when it is up to
/// 3 chunks behind.
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

/// Which tap the integrated VAD runs on.
#[derive(Clone, Copy, PartialEq, Eq)]
enum VadTap {
    Primary,
    Secondary,
}

/// Sample encoding of the secondary tap (core is always f32; encoding happens at the binding
/// boundary).
#[derive(Clone, Copy, PartialEq, Eq)]
enum SecEncoding {
    F32,
    S16,
}

// Polling interval of the bridge thread. Small enough relative to 20ms chunks while avoiding
// busy-spinning.
const POLL_INTERVAL: Duration = Duration::from_millis(1);
// Device hotplug is infrequent. 100ms responsiveness is enough.
const DEVICE_POLL_INTERVAL: Duration = Duration::from_millis(100);

// TSFN aliases with ErrorStrategy::Fatal. `.call(value, mode)` takes the value directly
// (with CalleeHandled it becomes `.call(Result<T>, mode)` and needs a Result wrapper).
type ChunkTsfn = ThreadsafeFunction<ChunkEmit, ErrorStrategy::Fatal>;
type SettleTsfn = ThreadsafeFunction<(), ErrorStrategy::Fatal>;
type EventTsfn = ThreadsafeFunction<JsStreamEvent, ErrorStrategy::Fatal>;
type DeviceTsfn = ThreadsafeFunction<JsDeviceEvent, ErrorStrategy::Fatal>;

/// Value queued on the onChunk TSFN. Chunks are passed to the user's `onChunk`; `StopFlushed`
/// is the "end signal" on the same queue (not surfaced to the JS onChunk).
enum ChunkEmit {
    Chunk(Box<JsAudioChunk>),
    StopFlushed,
}

/// `napi_deferred` is a raw pointer. It is created on the JS thread and resolved via a TSFN.
#[derive(Clone, Copy)]
struct SendDeferred(sys::napi_deferred);
unsafe impl Send for SendDeferred {}
unsafe impl Sync for SendDeferred {}

/// Rendezvous for `stop()` completion. Waiters are deferreds from `napi_create_promise`.
enum StopPhase {
    Running,
    Stopping { waiters: Vec<SendDeferred> },
    Stopped,
}

struct StreamInner {
    handle: Option<JoinHandle<()>>,
    cmd_tx: Option<mpsc::Sender<BridgeCmd>>,
}

/// Holds the JS onChunk for the lifetime of the TSFN. `FunctionRef<JsAudioChunk, _>` can fail
/// to be Send because of its PhantomData, so a raw `napi_ref` is used.
///
/// `napi_delete_reference` is JS-thread only. It is released explicitly in the StopFlushed /
/// settle callbacks (both TSFN = JS thread), and the Drop at TSFN finalize is then a second,
/// no-op call. This way the onChunk closure is dropped after `stop()` even if the TSFN itself
/// remains.
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

    /// JS-thread only. A second call is a no-op.
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

/// flexaudio::Error → napi::Error. Stringifies the message into a GenericFailure.
fn to_napi_err(err: flexaudio::Error) -> NapiError {
    NapiError::new(Status::GenericFailure, err.to_string())
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

fn take_stop_waiters(phase: &Mutex<StopPhase>) -> Vec<SendDeferred> {
    let mut g = phase.lock().unwrap_or_else(lock_poisoned);
    match std::mem::replace(&mut *g, StopPhase::Stopped) {
        StopPhase::Stopping { waiters } => waiters,
        StopPhase::Stopped => vec![],
        StopPhase::Running => vec![],
    }
}

/// Weak reference to the chunk TSFN. Used by `StopFlushed` / the settle callback to unref it
/// after resolving (it is filled in later, because the TSFN does not exist yet at creation).
///
/// If the callback held a Strong reference, napi-rs 2.16's `ThreadsafeFunction::clone` only
/// clones the Arc of the Handle, forming a "TSFN → callback → slot → TSFN" cycle, and finalize
/// (and the release of onChunk's `napi_ref`) never comes.
type ChunkTsfnWeakCell = Arc<OnceLock<Weak<ChunkTsfn>>>;

/// Drops the delivery TSFN's hold on the event loop. After `stop()` settles, Node can exit on
/// its own even if a stream reference remains. Idempotent (a second call is a no-op).
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

/// TSFN that calls the user's `onChunk`. Chunks are passed to the user; `StopFlushed` resolves
/// the deferreds on the same queue (without calling the user's onChunk). The pump function is
/// a no-op. After resolving, it unrefs the chunk TSFN and drops onChunk's `napi_ref`.
fn make_chunk_tsfn(
    env: &Env,
    stop_phase: Arc<Mutex<StopPhase>>,
    user: Arc<UserChunkCb>,
    chunk_weak: ChunkTsfnWeakCell,
) -> napi::Result<Arc<ChunkTsfn>> {
    let pump =
        env.create_function_from_closure("flexaudioChunkPump", |ctx| ctx.env.get_undefined())?;
    let tsfn =
        pump.create_threadsafe_function::<ChunkEmit, Unknown, _, ErrorStrategy::Fatal>(0, {
            let user = user;
            let chunk_weak = chunk_weak.clone();
            move |ctx: ThreadSafeCallContext<ChunkEmit>| match ctx.value {
                ChunkEmit::Chunk(chunk) => {
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
                    let _ = func.call(*chunk);
                    Ok(Vec::<Unknown>::new())
                }
                ChunkEmit::StopFlushed => {
                    for deferred in take_stop_waiters(&stop_phase) {
                        resolve_undefined(ctx.env.raw(), deferred);
                    }
                    // Drop the loop hold only after the terminator and resolve are done
                    // (ordering contract).
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

/// Dedicated TSFN that returns to the JS thread to settle the stop() Promise.
/// Used when the chunk TSFN is Closing (there is no onChunk left to deliver to).
///
/// Unref'd from creation. While alive, the chunk TSFN holds the loop, so the settle callback
/// is not lost. When we get here (the chunk TSFN is no longer usable), it also unrefs the
/// chunk side after resolving and drops onChunk's `napi_ref`.
fn make_settle_tsfn(
    env: &Env,
    stop_phase: Arc<Mutex<StopPhase>>,
    chunk_weak: ChunkTsfnWeakCell,
    user: Arc<UserChunkCb>,
) -> napi::Result<SettleTsfn> {
    let pump =
        env.create_function_from_closure("flexaudioStopSettle", |ctx| ctx.env.get_undefined())?;
    let mut tsfn = pump.create_threadsafe_function::<(), Unknown, _, ErrorStrategy::Fatal>(0, {
        move |ctx: ThreadSafeCallContext<()>| {
            for deferred in take_stop_waiters(&stop_phase) {
                resolve_undefined(ctx.env.raw(), deferred);
            }
            unref_chunk_weak(&chunk_weak, &ctx.env);
            user.release();
            Ok(Vec::<Unknown>::new())
        }
    })?;
    tsfn.unref(env)?;
    Ok(tsfn)
}

/// Queues StopFlushed on the chunk TSFN. On failure (Closing / QueueFull etc.) it falls back
/// to the settle TSFN. If that fails too, we cannot get back to the JS thread, so the deferreds
/// are discarded.
///
/// The chunk TSFN has `max_queue_size=0` (unbounded), so QueueFull normally never happens. If
/// it ever does, the settle TSFN is a separate queue and could resolve before onChunk. So we
/// first retry on the same queue with Blocking to preserve ordering, and fall back to the
/// settle TSFN only if that still fails.
fn post_stop_flushed(chunk: &ChunkTsfn, settle: &SettleTsfn, phase: &Mutex<StopPhase>) {
    let st = chunk.call(
        ChunkEmit::StopFlushed,
        ThreadsafeFunctionCallMode::NonBlocking,
    );
    if st == Status::Ok {
        return;
    }
    if st != Status::Closing {
        // QueueFull etc.: put it on the same TSFN queue with Blocking, behind the queued
        // onChunk calls.
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
    // Even the settle TSFN failed = the JS event loop no longer runs (Node is exiting).
    // Only in this case may unresolved deferreds be discarded. As long as JS is alive, the
    // settle TSFN calls resolve_undefined on the JS thread.
    let _ = take_stop_waiters(phase);
}

/// Runs `flexaudio::processes()` on the libuv thread pool.
pub struct ProcessesTask;

impl Task for ProcessesTask {
    type Output = Vec<ProcessInfo>;
    type JsValue = Vec<JsProcessInfo>;

    fn compute(&mut self) -> napi::Result<Self::Output> {
        flexaudio::processes().map_err(to_napi_err)
    }

    fn resolve(&mut self, _env: Env, output: Self::Output) -> napi::Result<Self::JsValue> {
        Ok(output.into_iter().map(process_info_to_js).collect())
    }
}

/// VadError → napi::Error. An invalid config is a caller mistake, so InvalidArg; model
/// load/inference failures are environmental, so GenericFailure.
fn vad_err(err: VadError) -> NapiError {
    let status = match err {
        VadError::InvalidConfig(_) => Status::InvalidArg,
        _ => Status::GenericFailure,
    };
    NapiError::new(status, err.to_string())
}

/// DenoiseError → napi::Error. Both variants are invalid arguments, so InvalidArg.
fn denoise_err(err: DenoiseError) -> NapiError {
    NapiError::new(Status::InvalidArg, err.to_string())
}

/// EncodeError → napi::Error. Unsupported parameters are InvalidArg; IO/encoder internals are
/// GenericFailure. It is `#[non_exhaustive]`, so `_` also catches future variants.
fn encode_err(err: EncodeError) -> NapiError {
    let status = match err {
        EncodeError::Unsupported(_) => Status::InvalidArg,
        _ => Status::GenericFailure,
    };
    NapiError::new(status, err.to_string())
}

// ---------------------------------------------------------------------------
// JS-facing data types (converted to/from JS as plain objects via `#[napi(object)]`)
// ---------------------------------------------------------------------------

/// JS-side DeviceInfo. `sourceKind` is a string ("mic"|"system"|"process").
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

/// JS-side ProcessInfo (element of `processes()`). A candidate target for per-process capture.
///
/// Passing `pid` to `openStream({ kind: 'process', processId: pid })` captures that process.
/// `name` / `executable` / `bundleId` are for display (they include values the app claims for
/// itself); the identity key is `pid`.
#[napi(object)]
pub struct JsProcessInfo {
    /// OS process ID (non-zero). Pass it as `processId` to `openStream`.
    pub pid: u32,
    /// Display name (never empty). OS-reported name → executable name → bundle ID →
    /// `"pid <N>"`.
    pub name: String,
    /// Base name of the executable (e.g. `firefox` / `chrome.exe`). Only when available.
    pub executable: Option<String>,
    /// macOS bundle ID (e.g. `com.apple.Music`). Only when available on macOS.
    pub bundle_id: Option<String>,
    /// Whether it is outputting audio right now. Only when the OS exposes it (Linux = node is
    /// Running / Windows = session is Active / macOS = IsRunningOutput). `undefined` means
    /// unknown.
    pub is_output_active: Option<bool>,
}

/// JS-side AudioChunk. `data` is interleaved f32 (len = frames * channels).
/// `seq` (u64) is a BigInt to avoid precision loss. `flags` is the ChunkFlags bits (u32).
///
/// Delivery shape (0.3.0): the `onChunk` of `openStream(options, onChunk)` is called with
/// **one argument** (this primary chunk). The secondary-tap (`secondaryOutput`) chunk is not a
/// second argument; it arrives in this primary chunk's `secondary` property. Finalized VAD
/// events likewise have no separate callback; they ride on the `vadEvents` of the chunk of the
/// tap selected by `vadTap` (`chunk.vadEvents` for 'primary', `chunk.secondary?.vadEvents` for
/// 'secondary').
///
/// `vadEvents` is filled only when `vad` is passed to `openStream`. With VAD disabled it is
/// unset (`undefined`). With VAD enabled it is an empty array when the chunk finalized no
/// events.
#[napi(object)]
pub struct JsAudioChunk {
    pub data: Float32Array,
    pub frames: u32,
    pub pts_ns: i64,
    pub seq: BigInt,
    pub flags: u32,
    pub dropped_before: u32,
    pub peak: f64,
    pub rms: f64,
    /// VAD events finalized in this chunk (only when `vadTap` is 'primary').
    pub vad_events: Option<Vec<JsVadEvent>>,
    /// The time-matched secondary-tap chunk (only when `secondaryOutput` is set). Delivered as
    /// a pair in the same callback (`primary.secondary` of `onChunk(primary)`, not a second
    /// argument). `undefined` on rounds where the secondary has not arrived yet. Match
    /// primary↔secondary by `ptsNs` (time) (`seq` is independent per tap).
    pub secondary: Option<JsSecondaryChunk>,
}

/// JS-side secondary-tap chunk (only when `secondaryOutput` is set).
///
/// `data` is the typed array matching `encoding` (`Int16Array` for `'s16'`, `Float32Array` for
/// `'f32'`). Sample values are in the host's native endianness. Serializing to the s16le wire
/// format is the receiver's (consumer's) responsibility. `ptsNs` is on the same
/// recording-start-zero clock as the primary, but its values are independent of the primary
/// and lag it by 20-60ms due to the group delay of the secondary Stage2 resampler.
#[napi(object)]
pub struct JsSecondaryChunk {
    pub data: Either<Int16Array, Float32Array>,
    /// 'f32' | 's16' (discriminant for narrowing the type of `data`).
    pub encoding: String,
    pub frames: u32,
    pub pts_ns: i64,
    pub seq: BigInt,
    pub flags: u32,
    pub dropped_before: u32,
    /// Computed on the pre-quantization f32 (no loss of meter precision even for s16).
    pub peak: f64,
    pub rms: f64,
    /// VAD events finalized in this chunk (only when `vadTap` is 'secondary').
    pub vad_events: Option<Vec<JsVadEvent>>,
}

/// JS-side VAD event (start/end of a speech segment).
///
/// `type` takes only the two values `'speechStart' | 'speechEnd'` (consistent with the `type`
/// of other events).
///
/// `atSample` is an absolute sample position **in the VAD's internal rate (`sampleRate` = 8000
/// or 16000, default 16000)**, not in input-chunk samples (raw silero value; for standalone /
/// debug use). To convert to seconds use `atSample / sampleRate`; the input sample position can
/// be approximated as `atSample * inputSampleRate / sampleRate`.
#[napi(object)]
pub struct JsVadEvent {
    /// 'speechStart' | 'speechEnd' (start/end of a speech segment).
    #[napi(js_name = "type", ts_type = "'speechStart' | 'speechEnd'")]
    pub kind: String,
    pub at_sample: i64,
    /// Absolute nanoseconds from recording start (`number` = f64). Filled only via the
    /// integrated VAD (`vad` of `openStream`) (computed from the chunk's `ptsNs` and the
    /// in-chunk offset at the VAD internal rate). Delivered in the same chunk and monotonically
    /// non-decreasing across chunks. The final events of `flushVad` ride on the same
    /// `vadEvents` array. `undefined` for the standalone `Vad` class (`process`/`flush`), which
    /// has no pts context. (Time is bounded by the recording length, hence `number`; only
    /// `seq`, a raw u64 counter, is `bigint`.)
    pub at_ns: Option<i64>,
}

/// JS-side native format (return value of `FlexStream.nativeFormat`).
#[napi(object)]
pub struct JsNativeFormat {
    pub sample_rate: u32,
    pub channels: u16,
}

/// Integrated VAD settings (shared by `OpenOptions.vad` and the `Vad` constructor).
///
/// Every field is optional; omitted fields take the silero-compliant defaults
/// (`VadConfig::default`).
#[napi(object)]
pub struct VadOptions {
    /// Probability threshold for treating it as speech start (>=). Default 0.5.
    pub threshold: Option<f64>,
    /// Negative threshold for treating it as silence start (<). Defaults to
    /// `max(threshold - 0.15, 0.01)`.
    pub neg_threshold: Option<f64>,
    /// Minimum length of an accepted speech segment (ms). Default 250.
    pub min_speech_ms: Option<u32>,
    /// Silence length required to finalize speech end (ms). Default 100.
    pub min_silence_ms: Option<u32>,
    /// Padding that widens segment boundaries on both sides (ms). Default 30.
    pub speech_pad_ms: Option<u32>,
    /// Maximum length of one segment (ms). 0 = unbounded. Force-split when exceeded.
    ///
    /// The standalone `Vad` class is silero-faithful with default 0 (unbounded). **The
    /// integrated VAD (`vad` of `openStream`) defaults to 30000ms when omitted (bounding
    /// monologues to keep real-time latency down)**. An explicit value (including `0`) wins.
    pub max_speech_ms: Option<u32>,
    /// Internal sample rate of the VAD. Only 8000 or 16000. Default 16000.
    pub sample_rate: Option<u32>,
}

/// JS-side stream event. `type` is the kind; `count`/`message` are optional.
#[napi(object)]
pub struct JsStreamEvent {
    #[napi(js_name = "type")]
    pub kind: String,
    pub count: Option<i64>,
    pub message: Option<String>,
}

/// JS-side device event. `type` is the kind; device/id/sourceKind are optional.
#[napi(object)]
pub struct JsDeviceEvent {
    #[napi(js_name = "type")]
    pub kind: String,
    pub device: Option<JsDeviceInfo>,
    pub id: Option<String>,
    pub source_kind: Option<String>,
}

/// Secondary output tap specification (`OpenOptions.secondaryOutput`).
///
/// Returns the same capture as the primary output (`outputRate`/`outputChannels`) in a
/// different format at the same time. Used for paired capture such as 48k/stereo for storage
/// + 16k/mono/s16 for recognition.
#[napi(object)]
pub struct SecondaryOutputOptions {
    /// Secondary output sample rate (Hz). E.g. 16000.
    pub rate: u32,
    /// Secondary output channel count (1=mono / 2=stereo). E.g. 1.
    pub channels: u16,
    /// Sample encoding of secondary chunks. 'f32' (default) | 's16'. s16 is quantized after
    /// the VAD and returned as an `Int16Array` (values in native endianness).
    pub encoding: Option<String>,
}

/// Options for openStream / __openMockStream.
#[napi(object)]
pub struct OpenOptions {
    /// "mic" | "system" | "process" | "mix"
    pub kind: String,
    pub device_id: Option<String>,
    pub process_id: Option<u32>,
    /// How the target PID of process is treated (process only). "include" (default) |
    /// "exclude". include = capture only the target PID / exclude = all system audio except
    /// the target PID (process_id required). Ignored for mic / system. Supported on all three
    /// OSes: Linux / Windows / macOS.
    pub mode: Option<String>,
    /// Whether to exclude the host's own (own process) audio from system audio (system only;
    /// for mix it applies to the system side). Default false. Ignored for mic / process.
    /// Supported on all three OSes: Linux / Windows / macOS.
    pub exclude_self: Option<bool>,
    /// Default 48000
    pub output_rate: Option<u32>,
    /// Default 2
    pub output_channels: Option<u16>,
    /// Default 20
    pub chunk_ms: Option<u32>,
    /// Initial input gain (linear factor). Default 1.0. 1.0 = unchanged, 2.0 = about +6dB,
    /// 0.0 = silence. Change it at runtime with `setGain`.
    pub gain: Option<f64>,
    /// Input device ID selected for the mic side of mix (mix only). Default input if unset.
    pub mic_device_id: Option<String>,
    /// Output endpoint ID selected for the system side of mix (mix only). Default output if
    /// unset.
    pub system_device_id: Option<String>,
    /// Pre-mix factor for the mic side of mix (linear, mix only). Default 1.0. `gain` is applied
    /// after mixing.
    pub mic_gain: Option<f64>,
    /// Pre-mix factor for the system side of mix (linear, mix only). Default 1.0.
    pub system_gain: Option<f64>,
    /// Integrated VAD settings. When set, the tap selected by `vadTap` is fed to the VAD and
    /// finalized events are attached to that tap's chunk `vadEvents` (the audio itself is not
    /// modified). VAD is disabled when omitted.
    pub vad: Option<VadOptions>,
    /// Tap the VAD runs on. 'primary' (default) | 'secondary'. 'secondary' is valid only when
    /// `secondaryOutput` is set, and is efficient when the secondary is 16k/mono since
    /// resampling is skipped.
    pub vad_tap: Option<String>,
    /// true enables capture-time noise suppression. **Usable only when the output is 48000 Hz**
    /// (RNNoise is fixed at 48kHz). When enabled it is applied once to the 48kHz/stereo
    /// internal canonical form, and both the primary and secondary taps receive denoised audio
    /// (+10ms fixed latency). Setting it to true with a rate other than 48kHz makes
    /// `openStream` throw InvalidArg. Omitted/false means no noise suppression.
    pub denoise: Option<bool>,
    /// Secondary output tap. When set, chunks in a different format are returned paired with
    /// the primary at the same time (`primary.secondary` of `onChunk`). When omitted there is
    /// no secondary tap = same as before.
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

/// "include" | "exclude" to [`ProcessMode`] (process only). `None`/unset is the default
/// Include.
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
        // Turn the Vec<f32> into a Float32Array (no ownership left on the thread side).
        data: Float32Array::new(chunk.data),
        frames,
        pts_ns: chunk.pts_ns,
        seq: BigInt::from(chunk.seq),
        flags: chunk.flags.bits(),
        dropped_before: chunk.dropped_before,
        peak: chunk.peak as f64,
        rms: chunk.rms as f64,
        // Unset by default. With the integrated VAD enabled (primary tap), the bridge
        // overwrites it.
        vad_events: None,
        // The bridge inserts the time-matched secondary chunk during pairing (undefined if
        // none).
        secondary: None,
    }
}

fn vad_event_to_js(ev: VadEvent) -> JsVadEvent {
    vad_event_to_js_abs(ev, None)
}

/// Maps a [`VadEvent`] to JS. `at_ns` is the absolute time from recording start (only via
/// the integrated VAD; `None` for the standalone `Vad` class). `at_sample` is the raw
/// cumulative position at the VAD internal rate.
fn vad_event_to_js_abs(ev: VadEvent, at_ns: Option<i64>) -> JsVadEvent {
    let (kind, at_sample) = match ev {
        VadEvent::SpeechStart { at_sample } => ("speechStart", at_sample as i64),
        VadEvent::SpeechEnd { at_sample } => ("speechEnd", at_sample as i64),
    };
    JsVadEvent {
        kind: kind.to_string(),
        at_sample,
        at_ns,
    }
}

/// 'primary' | 'secondary' to [`VadTap`]. `None`/unset is the default Primary.
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

/// 'f32' | 's16' to [`SecEncoding`]. `None`/unset is the default F32.
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

/// [`VadOptions`] → [`VadConfig`]. Omitted fields fall back to the silero-compliant defaults.
/// An omitted `neg_threshold` stays `None` (the default formula on the `VadConfig` side
/// applies).
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

/// Validates the 48kHz precondition of the integrated denoise (pure function, split out for
/// tests).
///
/// RNNoise is fixed at 48kHz, so if `enabled` and the output rate is not 48000 it returns
/// InvalidArg. `open_stream` rejects with this before opening the stream.
fn check_denoise_rate(enabled: bool, output_rate: u32) -> napi::Result<()> {
    if enabled && output_rate != 48_000 {
        return Err(NapiError::new(
            Status::InvalidArg,
            format!(
                "denoise supports only 48000 Hz output (RNNoise is fixed at 48kHz); \
                 it cannot be used with outputRate={output_rate}"
            ),
        ));
    }
    Ok(())
}

/// Builds the path for the `index`-th (1-based) FLAC rotation file (pure function).
///
/// Same convention as the CLI's `split_file_path`: for `rec.flac` it inserts a 3-digit
/// zero-padded sequence number before the extension, giving `rec-001.flac, rec-002.flac, …`.
/// From 1000 on the digits simply grow. For a path without an extension the number is
/// appended at the end. The parent directory is preserved.
fn split_flac_path(base: &Path, index: u64) -> PathBuf {
    let stem = base
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = match base.extension() {
        Some(ext) => format!("{stem}-{index:03}.{}", ext.to_string_lossy()),
        None => format!("{stem}-{index:03}"),
    };
    base.with_file_name(name)
}

fn event_to_js(ev: Event) -> JsStreamEvent {
    match ev {
        Event::ChunkDropped { count } => JsStreamEvent {
            kind: "chunkDropped".to_string(),
            count: Some(count as i64),
            message: None,
        },
        Event::StreamStalled => JsStreamEvent {
            kind: "stalled".to_string(),
            count: None,
            message: None,
        },
        Event::StreamRecovered => JsStreamEvent {
            kind: "recovered".to_string(),
            count: None,
            message: None,
        },
        Event::PermissionDenied => JsStreamEvent {
            kind: "permissionDenied".to_string(),
            count: None,
            message: None,
        },
        Event::DeviceLost => JsStreamEvent {
            kind: "deviceLost".to_string(),
            count: None,
            message: None,
        },
        Event::Error(msg) => JsStreamEvent {
            kind: "error".to_string(),
            count: None,
            message: Some(msg),
        },
        // Event is #[non_exhaustive]. To prepare for future variants, unknown kinds are
        // reported to JS as "error" + their debug representation (not swallowed).
        other => JsStreamEvent {
            kind: "error".to_string(),
            count: None,
            message: Some(format!("unknown event: {other:?}")),
        },
    }
}

fn device_event_to_js(ev: DeviceEvent) -> JsDeviceEvent {
    match ev {
        DeviceEvent::Added(info) => JsDeviceEvent {
            kind: "added".to_string(),
            device: Some(device_info_to_js(info)),
            id: None,
            source_kind: None,
        },
        DeviceEvent::Removed { id } => JsDeviceEvent {
            kind: "removed".to_string(),
            device: None,
            id: Some(id),
            source_kind: None,
        },
        DeviceEvent::DefaultChanged { kind, id } => JsDeviceEvent {
            kind: "defaultChanged".to_string(),
            device: None,
            id: Some(id),
            source_kind: Some(source_kind_str(kind)),
        },
        // DeviceEvent is #[non_exhaustive]. To prepare for future variants, unknown kinds are
        // passed to JS as "unknown" (not swallowed).
        _ => JsDeviceEvent {
            kind: "unknown".to_string(),
            device: None,
            id: None,
            source_kind: None,
        },
    }
}

fn build_config(options: &OpenOptions) -> napi::Result<StreamConfig> {
    let kind = parse_source_kind(&options.kind)?;
    let mode = parse_process_mode(options.mode.as_deref())?;
    let output = OutputFormat {
        sample_rate: options.output_rate.unwrap_or(48_000),
        channels: options.output_channels.unwrap_or(2),
    };
    // Secondary tap (only when set). The encoding is interpreted by the binding layer's
    // marshalling, so only rate/channels go into core's StreamConfig (core is always f32).
    let secondary_output = options.secondary_output.as_ref().map(|s| OutputFormat {
        sample_rate: s.rate,
        channels: s.channels,
    });
    let mut config = StreamConfig {
        kind,
        output,
        secondary_output,
        device_id: options.device_id.clone(),
        target_pid: options.process_id,
        // mode is process-only / exclude_self is system-only. The facade enforces not mixing
        // them.
        mode,
        exclude_self: options.exclude_self.unwrap_or(false),
        gain: options.gain.unwrap_or(1.0) as f32,
        // mix only (the facade ignores these for mic/system/process). Per-side gains default
        // to 1.0 when unset.
        mix_mic_device_id: options.mic_device_id.clone(),
        mix_system_device_id: options.system_device_id.clone(),
        mix_mic_gain: options.mic_gain.unwrap_or(1.0) as f32,
        mix_system_gain: options.system_gain.unwrap_or(1.0) as f32,
        ..Default::default()
    };
    if let Some(ms) = options.chunk_ms {
        config.chunk_ms = ms;
    }
    Ok(config)
}

// ---------------------------------------------------------------------------
// FlexStream (class). Owns and stops the bridge thread.
// ---------------------------------------------------------------------------

/// Command asking the bridge thread to switch sources.
///
/// The bridge thread owns the Stream, so `switch_source` cannot be called directly. A switch
/// request from JS is sent to the bridge thread with this command and the result is received
/// synchronously via `result_tx` (the JS side expects a synchronous return).
struct SwitchCmd {
    config: StreamConfig,
    result_tx: mpsc::Sender<std::result::Result<(), String>>,
}

/// Command asking the bridge thread to read the stream's current values.
///
/// `is_paused` / `gain` / `native_format` / `dropped_chunks` are all methods on the Stream,
/// and the bridge thread owns the Stream, so they cannot be read directly. A single query
/// returns a [`StreamSnapshot`] with all of them, and each getter picks the field it needs.
struct QueryCmd {
    result_tx: mpsc::Sender<StreamSnapshot>,
}

/// Snapshot of the stream's current values as read by the bridge thread.
struct StreamSnapshot {
    is_paused: bool,
    gain: f32,
    native_sample_rate: u32,
    native_channels: u16,
    dropped_chunks: u64,
}

/// Commands sent to the bridge thread. Only the bridge thread touches the Stream, so every
/// operation from JS is requested through this channel.
enum BridgeCmd {
    /// Hot-swap of the input source (returns the result synchronously).
    Switch(SwitchCmd),
    /// Pauses delivery.
    Pause,
    /// Resumes delivery.
    Resume,
    /// Changes the input gain (linear factor). The value is validated on the napi side before
    /// sending.
    SetGain(f32),
    /// Returns a snapshot of the current values synchronously (for getters).
    Query(QueryCmd),
    /// Force-finalizes the integrated VAD's open speech segment (`flushVad`). A runtime
    /// operation that does not change the config (`secondaryOutput` / encoding are fixed at
    /// open). Distinct from the audio stop-flush: the final speechEnd rides on the next chunk
    /// that arrives on the tap.
    FlushVad,
}

/// Marshalling settings of the secondary tap (rate/channels/encoding).
#[derive(Clone, Copy)]
struct SecondaryTapCfg {
    rate: u32,
    channels: u16,
    encoding: SecEncoding,
}

/// Emit state of the bridge thread. Converts primary/secondary chunks to JS, applies the
/// integrated VAD to the selected tap, pairs them within the pts window (60ms), and delivers
/// them to `onChunk`.
///
/// denoise has moved into core (internal canonical form) and is not here. The VAD binds a
/// single instance to a single tap (`vad_tap`) and consumes the pre-quantization f32 inside
/// Rust. Secondary s16 conversion happens after the VAD.
struct PairingBridge {
    on_chunk: ChunkTsfn,
    stop_phase: Arc<Mutex<StopPhase>>,
    /// Integrated VAD (only when set). Single instance, single tap.
    vad: Option<CoreVad>,
    vad_tap: VadTap,
    /// VAD internal rate (denominator of the absolute-time formula, 8000/16000).
    vad_rate: i64,
    /// Cumulative samples fed at the VAD internal rate (reset to 0 on `reset`). Reference
    /// point for absolute time.
    vad_samples_fed: i64,
    /// `dropped_before` of the previous chunk on the VAD tap (for detecting drop deltas).
    vad_last_dropped: u32,
    /// Reference point of the chunk most recently fed to the VAD (`(vad_sample_base,
    /// pts_base)`). Kept so the absolute time of the final events generated by `flushVad` can
    /// be computed even when there is no new chunk to process in that round.
    vad_anchor_sample: i64,
    vad_anchor_pts: i64,
    /// VAD events finalized by a runtime `flushVad` for which no carrying chunk has arrived
    /// yet. They are inserted at the front of the `vadEvents` of the next VAD-tap chunk pushed
    /// onto the FIFO.
    pending_flush_events: Vec<JsVadEvent>,
    /// Output format of the primary tap (passed to `process_pcm` when the VAD is on primary).
    output_rate: u32,
    output_channels: u16,
    /// Format and encoding of the secondary tap (only when set).
    secondary: Option<SecondaryTapCfg>,
    /// FIFOs for pairing. Both primary and secondary are fully drained every round and then
    /// matched within the pts window.
    primary_fifo: VecDeque<JsAudioChunk>,
    secondary_fifo: VecDeque<JsSecondaryChunk>,
    /// `ptsNs` of the primary chunk last delivered to `onChunk`. The pts of the final flush
    /// carrier at stop is clamped to at least this, preserving the primary pts non-decreasing
    /// contract.
    last_emitted_primary_pts: i64,
}

impl PairingBridge {
    /// Feeds the bound tap's pre-quantization f32 to the VAD and returns finalized events with
    /// absolute time from recording start. Resets and re-anchors on the discontinuity flag or
    /// a `dropped_before` increase.
    /// `None` if no VAD is configured.
    fn run_vad(
        &mut self,
        samples: &[f32],
        in_rate: u32,
        in_channels: u16,
        pts_ns: i64,
        discontinuity: bool,
        dropped_before: u32,
    ) -> Option<Vec<JsVadEvent>> {
        self.vad.as_ref()?;
        // On discontinuity or a ChunkRing drop increase, reset the internal state and re-anchor
        // the cumulative position at 0.
        let dropped_jump = dropped_before > self.vad_last_dropped;
        self.vad_last_dropped = dropped_before;
        let vad_rate = self.vad_rate;
        if discontinuity || dropped_jump {
            self.vad.as_mut().unwrap().reset();
            self.vad_samples_fed = 0;
        }
        // Record the (vad_sample_base, pts_base) for the start of this chunk. Also keep it in
        // self so that a later flushVad can compute the absolute time of the final events
        // without a new chunk.
        let vad_sample_base = self.vad_samples_fed;
        let pts_base = pts_ns;
        self.vad_anchor_sample = vad_sample_base;
        self.vad_anchor_pts = pts_base;
        let events = self
            .vad
            .as_mut()
            .unwrap()
            .process_pcm(samples, in_rate, in_channels);
        // Add the (approximate) number of VAD-internal-rate samples fed in this chunk to the
        // running total.
        let frames = samples.len() / (in_channels.max(1) as usize);
        self.vad_samples_fed += (frames as i64 * vad_rate) / (in_rate.max(1) as i64);

        let js = events
            .into_iter()
            .map(|ev| {
                let at_sample = match ev {
                    VadEvent::SpeechStart { at_sample } => at_sample,
                    VadEvent::SpeechEnd { at_sample } => at_sample,
                } as i64;
                // Absolute time = pts_base + (at_sample - VAD position at chunk start) /
                // vad_rate.
                let abs_ns = pts_base + (at_sample - vad_sample_base) * 1_000_000_000 / vad_rate;
                vad_event_to_js_abs(ev, Some(abs_ns))
            })
            .collect();
        Some(js)
    }

    /// Takes the deferred flushVad events and returns them concatenated in front of this
    /// chunk's VAD events (if any). Flush events finalize the previous speech segment = they
    /// are earlier in time than the new chunk's new events, so they go first.
    fn take_pending_prepended(&mut self, own: Option<Vec<JsVadEvent>>) -> Vec<JsVadEvent> {
        let mut merged = std::mem::take(&mut self.pending_flush_events);
        if let Some(ev) = own {
            merged.extend(ev);
        }
        merged
    }

    /// Force-finalizes the integrated VAD's open speech segment and returns the finalized
    /// events for JS (with `atNs` set to the absolute time relative to the latest anchor).
    /// `flush()` resets the VAD, so the cumulative counter is re-anchored at 0. Empty if no
    /// speech segment is open.
    fn flush_vad_events(&mut self) -> Vec<JsVadEvent> {
        let Some(vad) = self.vad.as_mut() else {
            return Vec::new();
        };
        let events = vad.flush();
        // flush() reset the VAD = the cumulative position returns to a zero origin.
        self.vad_samples_fed = 0;
        let vad_rate = self.vad_rate;
        let anchor_sample = self.vad_anchor_sample;
        let anchor_pts = self.vad_anchor_pts;
        events
            .into_iter()
            .map(|ev| {
                let at_sample = match ev {
                    VadEvent::SpeechStart { at_sample } => at_sample,
                    VadEvent::SpeechEnd { at_sample } => at_sample,
                } as i64;
                // Absolute time = anchor_pts + (at_sample - anchor_sample) / vad_rate.
                let abs_ns = anchor_pts + (at_sample - anchor_sample) * 1_000_000_000 / vad_rate;
                vad_event_to_js_abs(ev, Some(abs_ns))
            })
            .collect()
    }

    /// Runtime `flushVad` (`FlexStream.flushVad`). Finalizes the open speech segment and
    /// inserts the final events at the front of the `vadEvents` of the next chunk arriving on
    /// the VAD tap (pending). On an always-flowing tap a chunk arrives every 20ms, so the delay
    /// is ≤ 1 chunk.
    fn flush_vad(&mut self) {
        let js = self.flush_vad_events();
        if !js.is_empty() {
            self.pending_flush_events.extend(js);
        }
    }

    /// Final flush at stop. Finalizes the open speech segment and **always** delivers it in a
    /// dedicated trailing carrier chunk (`frames:0`). The `frames:0` terminator is emitted even
    /// without VAD events (contract: it reaches onChunk before `stop()` resolves).
    fn flush_vad_final(&mut self) {
        let js = self.flush_vad_events();
        // Pending events accumulated at runtime, if any, go first (time order).
        let mut events = std::mem::take(&mut self.pending_flush_events);
        events.extend(js);
        // To keep the primary pts non-decreasing contract, the carrier pts is the larger of
        // the latest anchor and the last delivered pts.
        let pts = self.vad_anchor_pts.max(self.last_emitted_primary_pts);
        let mut carrier = JsAudioChunk {
            data: Float32Array::new(Vec::new()),
            frames: 0,
            pts_ns: pts,
            seq: BigInt::from(0u64),
            flags: 0,
            dropped_before: 0,
            peak: 0.0,
            rms: 0.0,
            vad_events: None,
            secondary: None,
        };
        match self.vad_tap {
            VadTap::Primary => carrier.vad_events = Some(events),
            VadTap::Secondary => {
                // Secondary-tap events ride on the secondary chunk (the consumer reads them via
                // `primary.secondary`). The encoding follows the setting (samples are empty).
                let (data, encoding) = match self.secondary.map(|c| c.encoding) {
                    Some(SecEncoding::S16) => (Either::A(Int16Array::new(Vec::new())), "s16"),
                    _ => (Either::B(Float32Array::new(Vec::new())), "f32"),
                };
                carrier.secondary = Some(JsSecondaryChunk {
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
                });
            }
        }
        self.on_chunk.call(
            ChunkEmit::Chunk(Box::new(carrier)),
            ThreadsafeFunctionCallMode::NonBlocking,
        );
    }

    /// Ingests a primary chunk. Runs it through the VAD if the VAD is on primary, converts it
    /// to JS, and pushes it onto the FIFO.
    fn on_primary(&mut self, chunk: AudioChunk) {
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
        // Insert the deferred flushVad events (e.g. the previous segment's final speechEnd)
        // at the front.
        if self.vad_tap == VadTap::Primary && !self.pending_flush_events.is_empty() {
            vad_events = Some(self.take_pending_prepended(vad_events));
        }
        let mut js = chunk_to_js(chunk);
        js.vad_events = vad_events;
        self.primary_fifo.push_back(js);
    }

    /// Ingests a secondary chunk. If the VAD is on secondary, feeds it the pre-quantization
    /// f32, then marshals to `Int16Array`/`Float32Array` according to the encoding and pushes
    /// it onto the FIFO.
    fn on_secondary(&mut self, chunk: SecondaryChunk) {
        let Some(cfg) = self.secondary else {
            return; // Do nothing without a secondary-tap setting (defensive).
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
        // Insert the deferred flushVad events at the front (only when the VAD tap is
        // secondary).
        if self.vad_tap == VadTap::Secondary && !self.pending_flush_events.is_empty() {
            vad_events = Some(self.take_pending_prepended(vad_events));
        }
        // Record the metadata before consuming samples.
        let frames = chunk.frames as u32;
        let pts_ns = chunk.pts_ns;
        let seq = BigInt::from(chunk.seq);
        let flags = chunk.flags.bits();
        let dropped_before = chunk.dropped_before;
        let peak = chunk.peak as f64; // Already computed by core on the pre-quantization f32.
        let rms = chunk.rms as f64;
        let (data, encoding) = match cfg.encoding {
            SecEncoding::S16 => {
                // s16 quantization after the VAD (the canonical quantize_i16 shared by all
                // layers, native endianness).
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
        };
        self.secondary_fifo.push_back(js);
    }

    /// Matches primary↔secondary within the pts window and calls `onChunk(primary)` (with
    /// `primary.secondary`).
    ///
    /// Rule: for primary `P`, look at the head `S` of the secondary FIFO — a secondary that is
    /// too old is discarded (orphan); one within the window is popped and paired; if it has not
    /// arrived yet / is newer than the window, only the primary is delivered with
    /// `secondary=undefined` (the secondary stays for the next primary). Because it uses a pts
    /// window, it never drifts permanently (avoiding the flaw of a 1:1 zip).
    fn drain_pairs(&mut self) {
        while let Some(front) = self.primary_fifo.front() {
            let p_pts = front.pts_ns;
            let matched = loop {
                match self.secondary_fifo.front() {
                    None => break None,
                    Some(s) => {
                        if s.pts_ns < p_pts - PAIR_WINDOW_NS / 2 {
                            // Secondary too old (e.g. the primary was dropped) → discard
                            // the orphan and move to the next secondary.
                            self.secondary_fifo.pop_front();
                            continue;
                        } else if s.pts_ns < p_pts + CHUNK_SPAN_NS + PAIR_WINDOW_NS / 2 {
                            // Within the window → pair.
                            break self.secondary_fifo.pop_front();
                        } else {
                            // Secondary not here yet (newer than the window) → deliver only
                            // the primary; keep the secondary.
                            break None;
                        }
                    }
                }
            };
            let mut p = self.primary_fifo.pop_front().unwrap();
            self.last_emitted_primary_pts = p.pts_ns.max(self.last_emitted_primary_pts);
            p.secondary = matched;
            self.on_chunk.call(
                ChunkEmit::Chunk(Box::new(p)),
                ThreadsafeFunctionCallMode::NonBlocking,
            );
        }
    }
}

/// Handle to a capture stream. Internally a bridge thread owns and polls the
/// `flexaudio::Stream` and sends chunks/events to JS via TSFNs.
#[napi]
pub struct FlexStream {
    stop_flag: Arc<AtomicBool>,
    inner: Arc<Mutex<StreamInner>>,
    stop_phase: Arc<Mutex<StopPhase>>,
    chunk_tsfn: Arc<ChunkTsfn>,
    settle_tsfn: SettleTsfn,
}

impl FlexStream {
    /// Takes an already `start()`ed Stream and the [`PairingBridge`] responsible for emitting,
    /// and spawns the bridge thread. The Stream is Send, so it is moved into the thread
    /// (poll_* take &mut self, so ownership lives on the thread side). The integrated VAD /
    /// secondary-tap settings are held by the bridge.
    fn spawn(
        mut stream: flexaudio::Stream,
        mut bridge: PairingBridge,
        on_event: Option<EventTsfn>,
        chunk_tsfn: Arc<ChunkTsfn>,
        settle_tsfn: SettleTsfn,
    ) -> Self {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let thread_stop = stop_flag.clone();
        let stop_phase = bridge.stop_phase.clone();
        let settle_for_bridge = settle_tsfn.clone();
        let (cmd_tx, cmd_rx) = mpsc::channel::<BridgeCmd>();

        let handle = thread::spawn(move || {
            loop {
                if thread_stop.load(Ordering::SeqCst) {
                    break;
                }
                // Process commands in a batch in the same round as the poll.
                while let Ok(cmd) = cmd_rx.try_recv() {
                    match cmd {
                        BridgeCmd::Switch(sw) => {
                            let r = stream.switch_source(sw.config).map_err(|e| e.to_string());
                            // Ignore if the receiver (the switch_source caller) has
                            // dropped.
                            let _ = sw.result_tx.send(r);
                        }
                        BridgeCmd::Pause => stream.pause(),
                        BridgeCmd::Resume => stream.resume(),
                        BridgeCmd::SetGain(g) => {
                            // Validated on the napi side before sending, so Err is assumed
                            // not to happen. Even if it does, it is not turned into an event
                            // (the result is discarded).
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
                            // Ignore if the receiver has dropped.
                            let _ = q.result_tx.send(snap);
                        }
                        // Runtime flushVad: finalize the open speech segment and put the
                        // final events on the next chunk arriving on the VAD tap (pending).
                        BridgeCmd::FlushVad => bridge.flush_vad(),
                    }
                }
                // Drain both primary and secondary into the bridge as soon as they arrive. They
                // go through VAD/quantization, get paired within the pts window, and are
                // delivered to onChunk.
                while let Some(chunk) = stream.poll_chunk() {
                    bridge.on_primary(chunk);
                }
                while let Some(chunk) = stream.poll_secondary() {
                    bridge.on_secondary(chunk);
                }
                bridge.drain_pairs();
                // Consume events too.
                while let Some(ev) = stream.poll_event() {
                    if let Some(cb) = &on_event {
                        cb.call(event_to_js(ev), ThreadsafeFunctionCallMode::NonBlocking);
                    }
                }
                thread::sleep(POLL_INTERVAL);
            }
            // Before stopping, take every chunk left in the ring and deliver the pairs.
            while let Some(chunk) = stream.poll_chunk() {
                bridge.on_primary(chunk);
            }
            while let Some(chunk) = stream.poll_secondary() {
                bridge.on_secondary(chunk);
            }
            bridge.drain_pairs();
            // Order of stop() (addendum 2-2):
            //   ① Audio stop-flush: the core-side flush pushes the trailing tail (denoise delay
            //      line + resampler residue) into the ring.
            //   ② That tail is fed through the VAD while being pushed onto the FIFO, and the
            //      audio is delivered by the normal pair delivery.
            //   ③ flushVad: force-finalize the open speech segment and reliably deliver the
            //      final speechEnd in a dedicated trailing carrier (independent of pairing =
            //      not lost even if there is no primary tail).
            // This way both the audio at the end of the recording (②) and the final speechEnd
            // (③) arrive. The audio stop-flush and flushVad are different things (the former
            // is audio samples, the latter VAD events).
            stream.stop(); // ①
            while let Some(chunk) = stream.poll_chunk() {
                bridge.on_primary(chunk); // ② (the primary-tap VAD also eats the tail here)
            }
            while let Some(chunk) = stream.poll_secondary() {
                bridge.on_secondary(chunk); // ② (the secondary-tap VAD also eats the tail here)
            }
            bridge.drain_pairs(); // ② deliver the audio of the trailing tail
            bridge.flush_vad_final(); // ③ final events + the frames:0 terminator
                                      // ④ End signal on the same TSFN queue. The stop() Promise
                                      // resolves when JS processes it (with an AsyncTask / separate
                                      // TSFN it could resolve before onChunk). If the chunk TSFN
                                      // fails (Closing / QueueFull etc.), fall back to the settle
                                      // TSFN.
            post_stop_flushed(&bridge.on_chunk, &settle_for_bridge, &bridge.stop_phase);
        });

        Self {
            stop_flag,
            inner: Arc::new(Mutex::new(StreamInner {
                handle: Some(handle),
                cmd_tx: Some(cmd_tx),
            })),
            stop_phase,
            chunk_tsfn,
            settle_tsfn,
        }
    }

    fn take_join_handle(&self) -> Option<JoinHandle<()>> {
        let mut g = self.inner.lock().unwrap_or_else(lock_poisoned);
        g.cmd_tx = None;
        g.handle.take()
    }

    fn spawn_stop_worker(&self, h: JoinHandle<()>) {
        let tsfn = self.chunk_tsfn.clone();
        let settle = self.settle_tsfn.clone();
        let phase = self.stop_phase.clone();
        let _ = thread::Builder::new()
            .name("flexaudio-napi-stop".into())
            .spawn(move || {
                let _ = h.join();
                // Fallback only when the bridge could not queue the signal (e.g. it panicked).
                let needs_flush = {
                    let g = phase.lock().unwrap_or_else(lock_poisoned);
                    matches!(*g, StopPhase::Stopping { .. })
                };
                if needs_flush {
                    post_stop_flushed(tsfn.as_ref(), &settle, &phase);
                }
            });
    }

    /// Sends a Query to the bridge thread and synchronously receives a snapshot of the stream's
    /// current values. The implementation behind each getter (`is_paused`/`gain`/
    /// `native_format`/`dropped_chunks`). Throws if already `stop()`ped.
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
    /// Stops the capture. By the time the Promise resolves, every `onChunk` queued on the TSFN
    /// before the stop (the last PCM and the `frames:0` terminator) has been handed to JS.
    /// A second call waits for the same completion / resolves immediately if already done.
    /// Calling it from inside `onChunk` does not hang, because there is no join on the JS
    /// thread.
    #[napi(ts_return_type = "Promise<void>")]
    pub fn stop(&self, env: Env) -> napi::Result<JsObject> {
        let (deferred, promise) = create_js_promise(&env)?;
        // This is the JS thread. If the TSFN is already closed, there is no join handle, or the
        // phase is Stopped, napi_resolve_deferred may be called right here.
        let chunk_closed = self.chunk_tsfn.aborted();
        let mut phase = self.stop_phase.lock().unwrap_or_else(lock_poisoned);
        match &mut *phase {
            StopPhase::Stopped => {
                drop(phase);
                resolve_undefined(env.raw(), deferred);
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
                // No join handle (e.g. the Drop reaper took it). The callbacks to deliver are
                // being cleaned up on another path / are already gone. Resolve immediately on
                // the JS thread.
                for d in take_stop_waiters(&self.stop_phase) {
                    resolve_undefined(env.raw(), d);
                }
                unref_chunk_tsfn(self.chunk_tsfn.as_ref(), &env);
            }
            Some(h) if chunk_closed => {
                // In napi-rs 2.16, chunk_tsfn.aborted() is a Rust-side flag that becomes true
                // only in these cases:
                //   - `abort()` (`napi_tsfn_abort` = discard the queue's pending items and
                //     destroy immediately)
                //   - TSFN finalize (after release; in release mode, finalize runs only after
                //     the pending items have been processed)
                // It is not the "Closing but onChunk still left in the queue" state. By the
                // time we get here there is no onChunk left to deliver (discarded or already
                // handed over), so the ordering contract holds vacuously. It is fine to resolve
                // immediately on this thread, where JS is alive.
                // The join is handed to a reaper so JS is not blocked.
                let _ = thread::Builder::new()
                    .name("flexaudio-napi-reaper".into())
                    .spawn(move || {
                        let _ = h.join();
                    });
                for d in take_stop_waiters(&self.stop_phase) {
                    resolve_undefined(env.raw(), d);
                }
                unref_chunk_tsfn(self.chunk_tsfn.as_ref(), &env);
            }
            Some(h) => self.spawn_stop_worker(h),
        }
        Ok(promise)
    }

    /// Hot-swaps the input source (mic/system/process) without stopping the capture.
    ///
    /// Asks the bridge thread to switch to the `StreamConfig` built from `options` and returns
    /// the result synchronously (`Ok` on success, an exception on failure). The output format
    /// (`outputRate`/`outputChannels`) cannot be changed by a switch (it would change the
    /// frames of the continuous stream). Requesting a change makes `switch_source` return
    /// InvalidArg, which becomes an exception here. The chunk `seq` is continuous across the
    /// switch, and the first chunk after the switch has the DISCONTINUITY flag set.
    /// `options.gain` is ignored (gain is stream state; change it with `setGain`).
    ///
    /// Throws if already `stop()`ped (after the bridge thread has stopped).
    #[napi]
    pub fn switch_source(&self, options: OpenOptions) -> napi::Result<()> {
        // options → StreamConfig via build_config, same as openStream.
        let config = build_config(&options)?;

        // Send the command to the bridge thread and receive the result synchronously.
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
        // Wait for the bridge thread to run switch_source and return the result
        // (synchronous).
        match result_rx.recv() {
            Ok(Ok(())) => Ok(()),
            Ok(Err(msg)) => Err(NapiError::new(Status::GenericFailure, msg)),
            Err(_) => Err(NapiError::new(
                Status::GenericFailure,
                "bridge thread dropped before responding".to_string(),
            )),
        }
    }

    /// Pauses the capture. The device keeps running; only delivery stops. Resume with
    /// `resume`; the first chunk after resuming has DISCONTINUITY set. Throws if already
    /// `stop()`ped.
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

    /// Clears the pause and resumes delivery. Throws if already `stop()`ped.
    #[napi]
    pub fn resume(&self) -> napi::Result<()> {
        let cmd_tx = {
            let g = self.inner.lock().unwrap_or_else(lock_poisoned);
            g.cmd_tx.clone().ok_or_else(|| {
                NapiError::new(Status::GenericFailure, "stream already stopped".to_string())
            })?
        };
        cmd_tx.send(BridgeCmd::Resume).map_err(|_| {
            NapiError::new(
                Status::GenericFailure,
                "bridge thread is not running".to_string(),
            )
        })?;
        Ok(())
    }

    /// Force-finalizes the integrated VAD's "currently open speech segment" (runtime
    /// operation).
    ///
    /// silero emits no `speechEnd` until silence arrives, so call this when pausing
    /// recognition or when you want to finalize the last speech segment at the end of a
    /// recording. If a speech segment is open, the final `speechEnd` (and its paired
    /// `speechStart`) ride at the front of the `vadEvents` (with `atNs` from recording start)
    /// of the next chunk arriving on the tap (a chunk flows every 20ms, so the delay is ≤ 1
    /// chunk). After the call the VAD is reset, and the next speech is picked up in a fresh
    /// context.
    ///
    /// This **does not change the config** (unrelated to `secondaryOutput`/encoding being
    /// fixed at open = not changeable by `switchSource`). It is also distinct from the audio
    /// stop-flush and does not modify audio samples. Does nothing if no VAD is configured.
    /// `stop()` runs this automatically after the audio stop-flush. Throws if already
    /// `stop()`ped.
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

    /// Changes the input gain (linear factor). 1.0 = unchanged, 2.0 = about +6dB, 0.0 =
    /// silence. Can be called at any time during capture and takes effect from the next chunk
    /// (20ms granularity). Samples after multiplication are clamped to ±1.0. Throws unless it
    /// is finite and >= 0. Throws if already `stop()`ped.
    #[napi]
    pub fn set_gain(&self, gain: f64) -> napi::Result<()> {
        // Validate the value after the f64→f32 conversion (this also rejects huge values that
        // f32 cannot represent and that become infinity).
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

    /// Whether it is currently paused. Throws if already `stop()`ped.
    #[napi]
    pub fn is_paused(&self) -> napi::Result<bool> {
        Ok(self.query_snapshot()?.is_paused)
    }

    /// Current input gain (linear factor). Throws if already `stop()`ped.
    #[napi]
    pub fn gain(&self) -> napi::Result<f64> {
        Ok(self.query_snapshot()?.gain as f64)
    }

    /// Native format `{ sampleRate, channels }` of the current backend. For display and
    /// diagnostics (the chunks actually delivered use the output format
    /// `outputRate`/`outputChannels`). Changing the source with `switchSource` updates it to
    /// the new backend's values. Throws if already `stop()`ped.
    #[napi]
    pub fn native_format(&self) -> napi::Result<JsNativeFormat> {
        let s = self.query_snapshot()?;
        Ok(JsNativeFormat {
            sample_rate: s.native_sample_rate,
            channels: s.native_channels,
        })
    }

    /// Cumulative number of chunks the chunk ring discarded with DROP_OLDEST (BigInt). Throws
    /// if already `stop()`ped.
    #[napi]
    pub fn dropped_chunks(&self) -> napi::Result<BigInt> {
        Ok(BigInt::from(self.query_snapshot()?.dropped_chunks))
    }
}

impl Drop for FlexStream {
    fn drop(&mut self) {
        // GC path. Do not block the JS thread (GC) with a join. Set stop_flag and join the
        // handle on a reaper thread. Resources (bridge, TSFNs, capture) are released when the
        // bridge ends. There are no Promise waiters (no explicit stop was made).
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(h) = self.take_join_handle() {
            let _ = thread::Builder::new()
                .name("flexaudio-napi-reaper".into())
                .spawn(move || {
                    let _ = h.join();
                });
        }
    }
}

// ---------------------------------------------------------------------------
// DeviceWatcherHandle (class)
// ---------------------------------------------------------------------------

/// Handle to the device hotplug watch. A bridge thread polls the `DeviceWatcher`.
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
    /// Stops the watch and joins the bridge thread. Safe to call twice.
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

/// Enumerates the available devices. In a headless environment it does not throw, even if
/// the array is empty.
#[napi]
pub fn devices() -> napi::Result<Vec<JsDeviceInfo>> {
    let list = flexaudio::devices().map_err(to_napi_err)?;
    Ok(list.into_iter().map(device_info_to_js).collect())
}

/// Enumerates the processes that have an audio output session (stream) and can therefore be
/// targeted by per-process capture (`openStream({ kind: 'process', processId })`). The
/// calling process itself is not included. Stopped / Idle ones are listed too. Check
/// `isOutputActive` to see whether one is playing right now.
///
/// Ordering is "outputting (`isOutputActive: true`) first → display name → pid", and entries
/// with the same pid are merged into one. Read-only; it shows no permission prompt. It
/// returns within at most 3 seconds even if the OS does not respond. It runs on the libuv
/// thread pool, so it does not block the JS event loop.
///
/// - Linux (PipeWire): clients that have a `Stream/Output/Audio` node. `executable` is from
///   `/proc/<pid>/exe`, or `/proc/<pid>/comm` if that cannot be read.
/// - Windows: processes that have an audio session on an active output device. Both
///   enumeration and capture require Windows build 20348 or later (Windows 11 / Windows
///   Server 2022).
/// - macOS 14.4+: process objects known to Core Audio (with `bundleId`; input-only processes
///   are included too).
///
/// How to read the result: an empty array = per-process capture is available but no such
/// process exists right now (not "nothing is playing"). A rejection = per-process capture is
/// unavailable in this environment (PipeWire unreachable on Linux; `unsupported OS version`
/// below macOS 14.4 / Windows build 20348; `unsupported` on other OSes; permission denied),
/// the OS did not respond in time, or the previous query has not finished yet (same `Error`
/// type and wording as in the synchronous era).
#[napi(ts_return_type = "Promise<Array<JsProcessInfo>>")]
pub fn processes() -> AsyncTask<ProcessesTask> {
    AsyncTask::new(ProcessesTask)
}

/// Opens and starts a stream and returns a `FlexStream` that sends chunks/events to the
/// callbacks.
///
/// Setting `options.denoise` enables noise suppression in core (internal canonical form), and
/// both the primary and secondary taps receive denoised audio. Setting `options.vad` feeds the
/// tap selected by `vadTap` to the VAD and attaches finalized events to that tap's chunk
/// `vadEvents` (with `atNs`, the absolute time from recording start). Setting
/// `options.secondaryOutput` enables the secondary tap, which returns chunks in a different
/// format paired with the primary (`primary.secondary` of `onChunk`). The denoise 48kHz
/// precondition and invalid VAD settings are validated and rejected here, before the stream
/// is opened.
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
    let config = build_config(&options)?;
    let output_rate = config.output.sample_rate;
    let output_channels = config.output.channels;

    // Integrated denoise: validate the public contract's 48kHz precondition first (kept as
    // is). If enabled, delegate to core (the facade's set_denoise; denoise itself is applied
    // to the internal canonical form 48k/stereo).
    let denoise_enabled = options.denoise.unwrap_or(false);
    check_denoise_rate(denoise_enabled, output_rate)?;

    // Interpret the secondary-tap encoding / VAD tap.
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

    // Integrated VAD: built when specified (model load and invalid settings become exceptions
    // here). The VAD internal rate is the denominator of the absolute-time formula. The
    // integrated path applies the 30s default when maxSpeechMs is unset (the standalone Vad
    // stays silero-faithful at 0 = unbounded).
    let (vad, vad_rate) = match &options.vad {
        Some(o) => {
            let cfg = build_integrated_vad_config(o);
            let rate = cfg.sample_rate;
            (Some(CoreVad::new(cfg).map_err(vad_err)?), rate)
        }
        None => (None, 16_000),
    };

    let mut stream = flexaudio::open(config).map_err(to_napi_err)?;
    if denoise_enabled {
        stream.set_denoise(true);
    }
    stream.start().map_err(to_napi_err)?;

    let stop_phase = Arc::new(Mutex::new(StopPhase::Running));
    let user = make_user_chunk_cb(&env, &on_chunk)?;
    let chunk_weak: ChunkTsfnWeakCell = Arc::new(OnceLock::new());
    let on_chunk = make_chunk_tsfn(&env, stop_phase.clone(), user.clone(), chunk_weak.clone())?;
    let settle_tsfn = make_settle_tsfn(&env, stop_phase.clone(), chunk_weak, user)?;
    let bridge = PairingBridge {
        on_chunk: on_chunk.as_ref().clone(),
        stop_phase,
        vad,
        vad_tap,
        vad_rate: vad_rate as i64,
        vad_samples_fed: 0,
        vad_last_dropped: 0,
        vad_anchor_sample: 0,
        vad_anchor_pts: 0,
        pending_flush_events: Vec::new(),
        output_rate,
        output_channels,
        secondary,
        primary_fifo: VecDeque::new(),
        secondary_fifo: VecDeque::new(),
        last_emitted_primary_pts: 0,
    };
    Ok(FlexStream::spawn(
        stream,
        bridge,
        on_event,
        on_chunk,
        settle_tsfn,
    ))
}

/// Watches device hotplug and returns a `DeviceWatcherHandle` that sends events to the
/// callback.
#[napi]
pub fn watch_devices(
    #[napi(ts_arg_type = "(event: JsDeviceEvent) => void")] on_event: DeviceTsfn,
) -> napi::Result<DeviceWatcherHandle> {
    let mut watcher = flexaudio::watch_devices().map_err(to_napi_err)?;
    let stop_flag = Arc::new(AtomicBool::new(false));
    let thread_stop = stop_flag.clone();

    let handle = thread::spawn(move || {
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
    });

    Ok(DeviceWatcherHandle {
        stop_flag,
        handle: Some(handle),
    })
}

/// Test-only; not part of the public API.
///
/// Creates a stream by passing a `MockBackend` to the low-level `Stream::open` and runs it
/// through the same bridge / TSFN path as `open_stream`. Verifies the whole marshaling path
/// (Float32Array, BigInt, peak/rms, frames) end-to-end without real audio. Do not use it from
/// production code.
///
/// Passing `secondaryRate` enables the secondary tap (`secondaryChannels` = default 1,
/// `secondaryEncoding` = 'f32'|'s16', default 'f32'), so pairing, s16 quantization, and
/// `Int16Array` marshalling can be verified without real audio (no real capture needed).
///
/// Passing `vadThreshold` enables the integrated VAD (`vadTap` = 'primary'|'secondary',
/// default 'primary'), so `flushVad`, the `atNs` of `vadEvents`, and the automatic flush of
/// `stop()` can be verified without real audio. For testing it is built with
/// `minSpeechMs=0`, so with threshold 0 `flushVad` reliably finalizes the open speech segment
/// (real-time end-of-recording finalization can be verified even with a synthetic wave that
/// never goes silent).
///
/// The JS name is `__openMockStream`. The leading `__` marks it as outside the public API.
/// napi's default conversion would drop the leading underscores and turn it into
/// `openMockStream`, so it is pinned with `js_name`.
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
) -> napi::Result<FlexStream> {
    // Secondary tap (only when set). Validate the encoding and build the marshalling
    // settings.
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
    // Integrated VAD (for tests, only when `vadThreshold` is given). Built with
    // min_speech=0, so flushVad reliably finalizes even a short open speech segment. The VAD
    // internal rate is fixed at 16000.
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
    let mut stream = flexaudio::Stream::open(config, backend).map_err(to_napi_err)?;
    stream.start().map_err(to_napi_err)?;
    // The mock path does not go through the integrated denoise. The VAD is applied only when
    // `vadThreshold` is given (the purpose is to verify flushVad, vadEvents, and the pairing
    // path).
    let stop_phase = Arc::new(Mutex::new(StopPhase::Running));
    let user = make_user_chunk_cb(&env, &on_chunk)?;
    let chunk_weak: ChunkTsfnWeakCell = Arc::new(OnceLock::new());
    let on_chunk = make_chunk_tsfn(&env, stop_phase.clone(), user.clone(), chunk_weak.clone())?;
    let settle_tsfn = make_settle_tsfn(&env, stop_phase.clone(), chunk_weak, user)?;
    let bridge = PairingBridge {
        on_chunk: on_chunk.as_ref().clone(),
        stop_phase,
        vad,
        vad_tap,
        vad_rate: 16_000,
        vad_samples_fed: 0,
        vad_last_dropped: 0,
        vad_anchor_sample: 0,
        vad_anchor_pts: 0,
        pending_flush_events: Vec::new(),
        output_rate: sample_rate,
        output_channels: channels,
        secondary: secondary_cfg,
        primary_fifo: VecDeque::new(),
        secondary_fifo: VecDeque::new(),
        last_emitted_primary_pts: 0,
    };
    Ok(FlexStream::spawn(
        stream,
        bridge,
        None,
        on_chunk,
        settle_tsfn,
    ))
}

// ---------------------------------------------------------------------------
// Standalone add-on 1: Vad (a small wrapper that runs silero-VAD in streaming mode)
// ---------------------------------------------------------------------------

/// Handle to an offline VAD (silero-VAD on ONNX, model embedded).
///
/// One instance holds one ONNX session. Feeding any format (interleaved f32 at
/// `inputSampleRate` / `inputChannels`) to [`Vad::process`] converts it internally to mono at
/// the VAD rate, detects speech segments, and returns the finalized [`JsVadEvent`]s. Use it
/// when you want to classify an arbitrary sample sequence yourself instead of using the
/// integrated VAD of `openStream`.
#[napi]
pub struct Vad {
    inner: CoreVad,
}

#[napi]
impl Vad {
    /// Builds a VAD from an options object (loads the embedded model). InvalidArg if the
    /// settings are invalid (sampleRate other than 8000/16000, threshold outside `[0,1]`,
    /// etc.); GenericFailure if the model fails to load.
    #[napi(constructor)]
    pub fn new(options: VadOptions) -> napi::Result<Self> {
        let inner = CoreVad::new(build_vad_config(&options)).map_err(vad_err)?;
        Ok(Vad { inner })
    }

    /// Processes interleaved f32 in any format and returns the finalized [`JsVadEvent`]s.
    ///
    /// Partial frames are carried over internally, so the input may be split at any position.
    /// `atSample` is in the VAD internal rate (see [`JsVadEvent`]).
    #[napi]
    pub fn process(
        &mut self,
        samples: Float32Array,
        input_sample_rate: u32,
        input_channels: u16,
    ) -> Vec<JsVadEvent> {
        self.inner
            .process_pcm(&samples[..], input_sample_rate, input_channels)
            .into_iter()
            .map(vad_event_to_js)
            .collect()
    }

    /// Force-finalizes the currently open speech segment and returns the finalized
    /// [`JsVadEvent`]s (same behavior as reaching the end of input). After the call the
    /// internal state is reset, and the next `process` starts from a fresh context. No model
    /// inference runs, so it is lightweight and deterministic.
    ///
    /// The standalone `Vad` has no pts context, so `atNs` is `undefined` (`atSample` is the raw
    /// cumulative position at the VAD internal rate). Returns an empty array when no speech is
    /// in progress.
    #[napi]
    pub fn flush(&mut self) -> Vec<JsVadEvent> {
        self.inner
            .flush()
            .into_iter()
            .map(vad_event_to_js)
            .collect()
    }

    /// Initializes the internal state (state / context / state machine / sample position /
    /// resampler).
    #[napi]
    pub fn reset(&mut self) {
        self.inner.reset();
    }
}

// ---------------------------------------------------------------------------
// Standalone add-on 2: FlacEncoder (incremental FLAC writing + rotation by seconds)
// ---------------------------------------------------------------------------

/// Writer that incrementally saves capture chunks losslessly to FLAC files.
///
/// With `splitSeconds` of 1 or more, each time the number of written frames reaches
/// `splitSeconds × sampleRate` the current file is closed and it rotates to the next file with
/// a 3-digit sequence number, `name-001.flac, name-002.flac, …` (same convention as the CLI's
/// WAV splitting). The boundary is "move on once reached or exceeded" at chunk granularity, so
/// each file can be up to one chunk longer than the specified seconds, but chunks are never
/// split and nothing is dropped. With `splitSeconds` omitted/0 it is a single file.
#[napi]
pub struct FlacEncoder {
    /// Output base path (the base for sequence numbers when splitting; used as is for a
    /// single file).
    base: PathBuf,
    sample_rate: u32,
    channels: u16,
    /// Frame-count threshold per file (splitSeconds × sampleRate). 0 = single file.
    frames_per_file: u64,
    /// Writer currently being written to. None right after a rotation (lazily created on the
    /// next chunk).
    writer: Option<FlacWriter>,
    /// Frames written to the current file (reset to 0 on rotation).
    frames_in_current: u64,
    /// Sequence number of the next file to open (1-based; meaningful only when splitting).
    file_index: u64,
}

#[napi]
impl FlacEncoder {
    /// Creates a FLAC writer. `splitSeconds` omitted/0 gives a single file; 1 or more rotates
    /// by seconds.
    ///
    /// `channels` is 1..=2, `sampleRate` is 1..=96000 Hz (InvalidArg when out of range). When
    /// splitting, the first file (`name-001.flac`) is created immediately.
    #[napi(factory)]
    pub fn create(
        path: String,
        sample_rate: u32,
        channels: u16,
        split_seconds: Option<u32>,
    ) -> napi::Result<FlacEncoder> {
        let base = PathBuf::from(path);
        let frames_per_file = u64::from(split_seconds.unwrap_or(0)) * u64::from(sample_rate);
        let file_index = 1;
        // Open base for a single file, or name-001.ext as the first file when splitting.
        let first_path = if frames_per_file > 0 {
            split_flac_path(&base, file_index)
        } else {
            base.clone()
        };
        let writer = FlacWriter::create(&first_path, sample_rate, channels).map_err(encode_err)?;
        Ok(FlacEncoder {
            base,
            sample_rate,
            channels,
            frames_per_file,
            writer: Some(writer),
            frames_in_current: 0,
            file_index,
        })
    }

    /// Path of the next file to open when splitting.
    fn next_path(&self) -> PathBuf {
        if self.frames_per_file > 0 {
            split_flac_path(&self.base, self.file_index)
        } else {
            self.base.clone()
        }
    }

    /// Appends interleaved f32 (length a multiple of `channels`). InvalidArg if it is not a
    /// multiple.
    ///
    /// After writing, if the current file's frame count is at or above the threshold, it is
    /// finalized immediately and rotation moves to the next file (the next `writeChunk` becomes
    /// the start of the new file).
    #[napi]
    pub fn write_chunk(&mut self, samples: Float32Array) -> napi::Result<()> {
        // Right after a rotation writer=None. Open the next file here (lazy creation).
        if self.writer.is_none() {
            let path = self.next_path();
            self.writer = Some(
                FlacWriter::create(&path, self.sample_rate, self.channels).map_err(encode_err)?,
            );
        }
        let writer = self.writer.as_mut().expect("opened just above");
        writer.write_chunk(&samples[..]).map_err(encode_err)?;

        // Frames = samples / channels. write_chunk has verified it is a multiple, so it
        // divides evenly.
        let frames = samples.len() as u64 / u64::from(self.channels);
        self.frames_in_current += frames;

        if self.frames_per_file > 0 && self.frames_in_current >= self.frames_per_file {
            // Threshold reached. Finalize the current file; the next chunk goes to the next
            // file.
            let done = self.writer.take().expect("written just above");
            done.finalize().map_err(encode_err)?;
            self.file_index += 1;
            self.frames_in_current = 0;
        }
        Ok(())
    }

    /// Writes out the remainder, finalizes the header, and closes the open file. Safe to call
    /// twice (the second and later calls are no-ops). Discarding it without calling this still
    /// closes the file best-effort via `FlacWriter`'s Drop, but call this if you want to detect
    /// write errors.
    #[napi]
    pub fn finalize(&mut self) -> napi::Result<()> {
        if let Some(writer) = self.writer.take() {
            writer.finalize().map_err(encode_err)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Standalone add-on 3: Denoiser (offline noise suppression with RNNoise)
// ---------------------------------------------------------------------------

/// Offline noise suppressor (RNNoise via nnnoiseless, weights embedded). **Assumes 48kHz**
/// and is intended to reduce stationary noise in microphone recordings (fans, air
/// conditioning, typing, etc.).
///
/// It has a fixed latency of [`FRAME_SIZE`](flexaudio_denoise::FRAME_SIZE) (10ms at 48kHz);
/// the output is the input delayed by one frame. The first frame of the stream is silent
/// padding, and the one frame left at the end is retrieved with [`Denoiser::flush`].
#[napi]
pub struct Denoiser {
    inner: CoreDenoiser,
}

#[napi]
impl Denoiser {
    /// Builds it for the given channel count (1 = mono, 2 = stereo interleaved). InvalidArg
    /// when out of range.
    #[napi(constructor)]
    pub fn new(channels: u16) -> napi::Result<Self> {
        let inner = CoreDenoiser::new(channels).map_err(denoise_err)?;
        Ok(Denoiser { inner })
    }

    /// Noise-suppresses interleaved f32 of any length (normalized to ±1.0, 48kHz, length a
    /// multiple of channels) and returns it as a **new array** (a copy, since in-place is
    /// awkward in napi). InvalidArg if the length is not a multiple of channels.
    #[napi]
    pub fn process(&mut self, samples: Float32Array) -> napi::Result<Float32Array> {
        let mut buf = samples.to_vec();
        self.inner.process(&mut buf).map_err(denoise_err)?;
        Ok(Float32Array::new(buf))
    }

    /// Processes the carried-over remainder, returns the trailing delayed portion (1
    /// frame/ch), and closes the stream. After the call it is back in the freshly-created
    /// state and can go on to process another stream.
    #[napi]
    pub fn flush(&mut self) -> Float32Array {
        Float32Array::new(self.inner.flush())
    }

    /// Initializes the RNN state, the carry-over, and the delay line.
    #[napi]
    pub fn reset(&mut self) {
        self.inner.reset();
    }
}

#[cfg(test)]
mod tests {
    //! Verifies the pure parts of the marshalling without a JS runtime.
    //!
    //! Only the pure "Rust value → JS-facing intermediate representation" conversions are
    //! checked here:
    //! - `parse_source_kind` / `source_kind_str` (round trip)
    //! - `parse_process_mode` (default/explicit/unknown)
    //! - `build_config` (OpenOptions → StreamConfig defaults and propagation)
    //! - `to_napi_err` (flexaudio::Error → napi string, Status)
    //! - `event_to_js` / `device_event_to_js` (kind strings, payload)
    //! - `chunk_to_js` (seq u64 → BigInt, data, frames, peak/rms)
    //!
    //! `Float32Array::new(Vec)` and `BigInt::from(u64)` store values in plain Rust fields,
    //! which can be read back without a JS runtime via `Deref<[f32]>` / `get_u64()`
    //! (napi 2.16).

    use super::*;

    // --- source kind round trip ---

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
        // None / "include" is the default Include.
        assert_eq!(parse_process_mode(None).unwrap(), ProcessMode::Include);
        assert_eq!(
            parse_process_mode(Some("include")).unwrap(),
            ProcessMode::Include
        );
        // "exclude" is Exclude.
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

    /// Helper that builds OpenOptions with every field unset (kind only).
    fn options_with_kind(kind: &str) -> OpenOptions {
        OpenOptions {
            kind: kind.to_string(),
            device_id: None,
            process_id: None,
            mode: None,
            exclude_self: None,
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
        // chunk_ms unset gives the StreamConfig default (20).
        assert_eq!(cfg.chunk_ms, 20);
        // gain unset gives the default 1.0.
        assert_eq!(cfg.gain, 1.0);
        // Defaults of the mix-only fields (no device specified, per-side gain 1.0).
        assert_eq!(cfg.mix_mic_device_id, None);
        assert_eq!(cfg.mix_system_device_id, None);
        assert_eq!(cfg.mix_mic_gain, 1.0);
        assert_eq!(cfg.mix_system_gain, 1.0);
    }

    #[test]
    fn build_config_reflects_all_fields() {
        let opts = OpenOptions {
            kind: "process".to_string(),
            device_id: Some("dev-x".to_string()),
            process_id: Some(9999),
            mode: Some("exclude".to_string()),
            exclude_self: Some(true),
            output_rate: Some(16_000),
            output_channels: Some(1),
            chunk_ms: Some(20),
            gain: Some(2.5),
            mic_device_id: None,
            system_device_id: None,
            mic_gain: None,
            system_gain: None,
            vad: None,
            vad_tap: None,
            denoise: None,
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
    fn to_napi_err_carries_message_and_status() {
        let err = to_napi_err(flexaudio::Error::DeviceNotFound);
        assert_eq!(err.status, Status::GenericFailure);
        // The Display string goes into reason.
        assert_eq!(err.reason, flexaudio::Error::DeviceNotFound.to_string());
        assert!(err.reason.contains("device not found"));
    }

    // --- event_to_js ---

    #[test]
    fn event_to_js_maps_each_variant() {
        let dropped = event_to_js(Event::ChunkDropped { count: 7 });
        assert_eq!(dropped.kind, "chunkDropped");
        assert_eq!(dropped.count, Some(7));
        assert_eq!(dropped.message, None);

        assert_eq!(event_to_js(Event::StreamStalled).kind, "stalled");
        assert_eq!(event_to_js(Event::StreamRecovered).kind, "recovered");
        assert_eq!(
            event_to_js(Event::PermissionDenied).kind,
            "permissionDenied"
        );
        assert_eq!(event_to_js(Event::DeviceLost).kind, "deviceLost");

        let errev = event_to_js(Event::Error("boom".to_string()));
        assert_eq!(errev.kind, "error");
        assert_eq!(errev.message, Some("boom".to_string()));
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
            kind: SourceKind::SystemLoopback,
            id: "sink-2".to_string(),
        });
        assert_eq!(changed.kind, "defaultChanged");
        assert_eq!(changed.id.as_deref(), Some("sink-2"));
        assert_eq!(changed.source_kind.as_deref(), Some("system"));
    }

    // seq u64 → BigInt conversion (the pure part of the marshalling).
    //
    // `chunk_to_js` as a whole creates a `Float32Array`, so it cannot be tested here. In napi
    // 2.16, the `Drop` of `Float32Array` unconditionally references
    // `napi_call_threadsafe_function`, so the cdylib unit test binary (no Node host) cannot
    // link and `cargo test -p flexaudio-napi` breaks. So only the seq→BigInt conversion, which
    // does not depend on a JS runtime, is checked with the same logic (`BigInt::from(u64)` +
    // `get_u64`). The data/Float32Array path is covered by the Node-side E2E
    // (`__openMockStream`).

    #[test]
    fn seq_u64_to_bigint_is_lossless() {
        // chunk_to_js turns seq into a BigInt with `BigInt::from(chunk.seq)`.
        // Check that even 2^53+1 (a magnitude f64 cannot represent) round-trips losslessly.
        let seq: u64 = 9_007_199_254_740_993; // 2^53 + 1.
        let big = BigInt::from(seq);
        let (sign, value, lossless) = big.get_u64();
        assert!(!sign, "seq is non-negative");
        assert_eq!(
            value, seq,
            "seq value is kept losslessly (digits that f64 would lose)"
        );
        assert!(lossless, "a single u64 word, so lossless");

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
        // vad/denoise are interpreted by open_stream, not in StreamConfig, so check that
        // build_config passes unaffected by them (= orthogonal to the capture settings
        // themselves).
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

    /// VadOptions with every field unset.
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
        // An unset negThreshold stays None (the default formula on the VadConfig side
        // applies).
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

    // --- build_integrated_vad_config (integrated-path maxSpeechMs default 30s, addendum 2-3) ---

    #[test]
    fn integrated_vad_config_defaults_max_speech_to_30s() {
        // The integrated path sets 30_000 when maxSpeechMs is unset (bounding monologues).
        let cfg = build_integrated_vad_config(&empty_vad_options());
        assert_eq!(cfg.max_speech_ms, INTEGRATED_VAD_MAX_SPEECH_MS_DEFAULT);
        assert_eq!(cfg.max_speech_ms, 30_000);
        // The other fields match build_vad_config (silero defaults).
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
        // An explicit value is respected.
        let mut o = empty_vad_options();
        o.max_speech_ms = Some(5_000);
        assert_eq!(build_integrated_vad_config(&o).max_speech_ms, 5_000);
        // Explicitly passing 0 (unbounded) restores the silero default (explicit wins over the
        // default override).
        o.max_speech_ms = Some(0);
        assert_eq!(build_integrated_vad_config(&o).max_speech_ms, 0);
    }

    #[test]
    fn standalone_vad_config_keeps_silero_max_speech() {
        // The standalone Vad path (build_vad_config) stays silero-faithful = default 0
        // (unbounded).
        assert_eq!(build_vad_config(&empty_vad_options()).max_speech_ms, 0);
    }

    // --- check_denoise_rate (denoise 48kHz precondition) ---

    #[test]
    fn denoise_requires_48k() {
        // Enabled + 48000 is OK.
        assert!(check_denoise_rate(true, 48_000).is_ok());
        // Enabled + anything other than 48000 is InvalidArg.
        let err = check_denoise_rate(true, 16_000).unwrap_err();
        assert_eq!(err.status, Status::InvalidArg);
        // Disabled is OK regardless of rate (not validated).
        assert!(check_denoise_rate(false, 16_000).is_ok());
        assert!(check_denoise_rate(false, 48_000).is_ok());
    }

    // --- split_flac_path (sequential naming, same convention as the CLI) ---

    #[test]
    fn split_flac_path_numbering() {
        // With an extension: 3-digit zero-padded sequence number before the extension.
        assert_eq!(
            split_flac_path(Path::new("rec.flac"), 1),
            PathBuf::from("rec-001.flac")
        );
        assert_eq!(
            split_flac_path(Path::new("rec.flac"), 12),
            PathBuf::from("rec-012.flac")
        );
        // From 1000 on the digits simply grow.
        assert_eq!(
            split_flac_path(Path::new("rec.flac"), 1000),
            PathBuf::from("rec-1000.flac")
        );
        // Without an extension: sequence number at the end.
        assert_eq!(
            split_flac_path(Path::new("rec"), 3),
            PathBuf::from("rec-003")
        );
        // The parent directory is preserved.
        assert_eq!(
            split_flac_path(Path::new("/tmp/out/meeting.flac"), 2),
            PathBuf::from("/tmp/out/meeting-002.flac")
        );
    }

    // --- Error mapping of each add-on ---

    #[test]
    fn error_mappers_carry_status() {
        // denoise: invalid channels is InvalidArg.
        let e = denoise_err(DenoiseError::InvalidChannels(3));
        assert_eq!(e.status, Status::InvalidArg);
        // encode: unsupported parameters are InvalidArg.
        let e = encode_err(EncodeError::Unsupported("bad".to_string()));
        assert_eq!(e.status, Status::InvalidArg);
        // encode: encoder internals are GenericFailure.
        let e = encode_err(EncodeError::Encoder("boom".to_string()));
        assert_eq!(e.status, Status::GenericFailure);
        // vad: invalid config is InvalidArg.
        let e = vad_err(VadError::InvalidConfig("nope".to_string()));
        assert_eq!(e.status, Status::InvalidArg);
        // vad: model load failure is GenericFailure.
        let e = vad_err(VadError::ModelLoad("x".to_string()));
        assert_eq!(e.status, Status::GenericFailure);
    }

    // --- vad_event_to_js (kind string, atSample) ---

    #[test]
    fn vad_event_to_js_maps_variants() {
        let start = vad_event_to_js(VadEvent::SpeechStart { at_sample: 512 });
        assert_eq!(start.kind, "speechStart");
        assert_eq!(start.at_sample, 512);
        // The standalone path (no absolute-time context) has atNs = undefined.
        assert_eq!(start.at_ns, None);
        let end = vad_event_to_js(VadEvent::SpeechEnd { at_sample: 4096 });
        assert_eq!(end.kind, "speechEnd");
        assert_eq!(end.at_sample, 4096);
        assert_eq!(end.at_ns, None);
    }

    #[test]
    fn vad_event_to_js_abs_carries_recording_time() {
        // The integrated path carries atNs, the absolute time from recording start (atSample
        // stays the raw internal-rate position).
        let ev = vad_event_to_js_abs(VadEvent::SpeechEnd { at_sample: 8000 }, Some(1_500_000_000));
        assert_eq!(ev.kind, "speechEnd");
        assert_eq!(ev.at_sample, 8000);
        assert_eq!(ev.at_ns, Some(1_500_000_000));
    }
}
