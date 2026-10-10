//! flexaudio-os-linux — Linux backend: PipeWire (`pipewire` 0.10)
//!
//! Provides [`PwSystemBackend`] to capture system audio output (the default sink's monitor).
//! This is the Linux equivalent of WASAPI loopback and captures the audio sent to the speakers
//! through a `Stream/Input/Audio` stream with `stream.capture.sink=true`.
//!
//! # Handling `!Send`
//!
//! PipeWire's `MainLoop` / `Context` / `Core` / `Stream` are `!Send` (they hold raw pointers and
//! a thread-local loop), while [`CaptureBackend`] requires `Send`. Keep all PipeWire creation,
//! execution, and destruction on one dedicated thread. [`PwSystemBackend`] stores only `Send`
//! values (a [`pipewire::channel::Sender`] for stopping, a [`JoinHandle`], and
//! [`std::sync::mpsc`] for receiving the startup result). `MainLoop` and related values never
//! cross a thread boundary.
//!
//! # Format
//!
//! Request 48000 Hz / 2 channels / f32. PipeWire automatically inserts `audioconvert` if the
//! graph uses a different rate or channel count, so the core does not need to resample or remix.
//!
//! # Non-Linux
//!
//! `#![cfg(target_os = "linux")]` compiles this as an empty crate on non-Linux platforms, and
//! the `pipewire` dependency is only included in the Linux target section of `Cargo.toml`.

#![cfg(target_os = "linux")]
#![warn(missing_docs)]

use std::collections::VecDeque;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use flexaudio_core::backend::{CaptureBackend, RawSink};
use flexaudio_core::clock::monotonic_now_ns;
use flexaudio_core::types::{DeviceEvent, DeviceInfo, Error, ProcessMode, Result, SourceKind};
use flexaudio_core::{ErrorContext, Event, Operation};

mod discovery;
use discovery::EnumerationFailure;
mod owner;
use owner::{
    finish_worker, poll_backend_event, push_backend_event, rollback_worker, BackendEvents,
};
mod watcher_queue;
use watcher_queue::{lock_events, transition_default, WatchEventQueue, WatchEvents};

use pipewire as pw;
use pw::spa;
use pw::{properties::properties, stream::StreamFlags};
use spa::param::format::{MediaSubtype, MediaType};
use spa::param::format_utils;
use spa::pod::Pod;

/// Native sample rate (Hz). Request 48 kHz and let PipeWire convert as needed.
const NATIVE_RATE: u32 = 48_000;
/// Native channel count. Request stereo and let PipeWire convert as needed.
const NATIVE_CHANNELS: u16 = 2;

/// Maximum number of events in the watch queue. Prevents unbounded `VecDeque` growth if the
/// consumer does not call `poll_event` for a while or devices are repeatedly added and removed.
/// When full, the oldest delta is dropped and a sticky rescan notice precedes remaining deltas.
const MAX_WATCH_EVENTS: usize = 1024;

/// Deadline (milliseconds) for [`enumerate_pw`]'s synchronous wait loop. `done` usually arrives
/// quickly, but this prevents `while !done { run() }` from looping or hanging forever if it does
/// not. On timeout, return an error without publishing an incomplete inventory.
const ENUMERATE_DEADLINE_MS: u128 = 2_000;

/// Deadline (milliseconds) for [`run_pw_loop`]'s capture stream to negotiate its format.
///
/// Readiness is reported only once `param_changed` stores a negotiated format. A stream that
/// never negotiates (for example the sink disappeared mid-setup) must not block `start()`
/// forever, so a one-shot timer reports failure and quits the loop at this deadline.
const NEGOTIATE_DEADLINE_MS: u128 = 2_000;

/// Single-shot readiness report from the capture loop thread back to `start()`.
///
/// The loop thread reports success only after the stream has negotiated its format (see
/// [`add_capture_listener`]'s `param_changed`), or failure from the negotiation deadline timer.
/// `sent` makes the report single-shot, so a late deadline cannot overwrite a successful report.
struct Readiness {
    /// Channel back to `start()`.
    tx: mpsc::Sender<std::result::Result<(), String>>,
    /// Whether a report (success or failure) has already been sent.
    sent: std::cell::Cell<bool>,
}

impl Readiness {
    /// Report that the stream is set up and its format negotiated. No-op if already reported.
    fn report_ready(&self) {
        if !self.sent.replace(true) {
            let _ = self.tx.send(Ok(()));
        }
    }

    /// Report a setup failure. No-op if a report has already been sent.
    fn report_failure(&self, msg: String) {
        if !self.sent.replace(true) {
            let _ = self.tx.send(Err(msg));
        }
    }

    /// Whether a report has already been sent (success or failure).
    fn is_reported(&self) -> bool {
        self.sent.get()
    }
}

/// Call [`pipewire::init`] once per process.
///
/// `pw::init()` performs library-wide global initialization and may be called concurrently from
/// multiple backend threads (system / process / watch / enumerate). Use [`std::sync::Once`] to
/// prevent races from repeated calls.
fn pw_init_once() {
    use std::sync::Once;
    static PW_INIT: Once = Once::new();
    PW_INIT.call_once(|| {
        pw::init();
    });
}

// Process enumeration shares PID resolution with process capture.
mod processes;
pub use processes::list_processes;

/// [`CaptureBackend`] that captures system audio output (the sink monitor) through PipeWire.
///
/// Builds a PipeWire `MainLoop` and input `Stream` on a dedicated thread, then sends interleaved
/// f32 samples dequeued by the `process` callback to [`RawSink::push`] without blocking.
/// `stream.capture.sink=true` selects the sink (speaker) monitor, i.e. system audio output,
/// instead of a recording device.
///
/// If `device_id` is `None`, captures the default sink monitor; if it is `Some(node.name)`,
/// captures that sink's monitor (specified by `target.object`). If the requested sink does not
/// exist, [`start`](CaptureBackend::start) returns [`Error::DeviceNotFound`].
///
/// If PipeWire or a sink is unavailable (such as on a headless server),
/// [`start`](CaptureBackend::start) returns [`Error::Backend`] without panicking.
///
/// ```no_run
/// use flexaudio_os_linux::PwSystemBackend;
/// use flexaudio_core::backend::CaptureBackend;
///
/// let backend = PwSystemBackend::new(false, None);
/// assert_eq!(backend.native_format(), (48_000, 2));
/// // let mut backend = backend;
/// // backend.start(sink)?;   // Err(Backend) if PipeWire or an active sink is unavailable
/// // ...
/// // backend.stop();
/// ```
pub struct PwSystemBackend {
    /// Whether to exclude this process's playback audio (to prevent feedback). When `true`,
    /// [`start`](CaptureBackend::start) reuses the process Exclude mechanism with
    /// `std::process::id()` as the excluded PID and records all other apps' output
    /// (`Stream/Output/Audio`) through fan-in links. The sink monitor is already mixed, so this is
    /// the only way to exclude just this process. When `false`, records the sink monitor as-is.
    /// Since `exclude_pids` was added, `false` still selects the fan-in path when
    /// `exclude_pids` is non-empty.
    exclude_self: bool,
    /// Extra pids excluded from the system capture alongside `exclude_self`
    /// (see `StreamConfig::exclude_pids`). A non-empty exclusion set — from
    /// either source — selects the fan-in path.
    exclude_pids: Vec<u32>,
    /// Select the sink to capture by `node.name`. `None` captures the default sink monitor;
    /// `Some(id)` captures that sink's monitor by setting `target.object` (`DeviceInfo.id` from
    /// [`list_devices`] is this `node.name`). Ignored when `exclude_self == true`, since fan-in
    /// does not target a specific sink.
    /// Since `exclude_pids` was added, the fan-in path is taken whenever the
    /// effective exclusion set (`exclude_pids ∪ {self if exclude_self}`) is
    /// non-empty, and `device_id` is ignored on that path — not only when
    /// `exclude_self == true`.
    device_id: Option<String>,
    /// Running flag (guards against duplicate starts and is used by drop). `Send`.
    running: Arc<AtomicBool>,
    /// Sender for stopping the loop thread. Set to `Some` by `start`. Sending invokes the
    /// receiver callback attached to the loop, which calls `main_loop.quit()` on the loop thread
    /// and exits `run()`.
    stop_tx: Option<pw::channel::Sender<Terminate>>,
    /// Handle for the PipeWire loop thread. Set to `Some` by `start`.
    handle: Option<JoinHandle<()>>,
    shutdown: Option<Result<()>>,
    events: BackendEvents,
}

/// Zero-sized stop message sent to the loop thread.
struct Terminate;

impl PwSystemBackend {
    /// Construct the backend (does not connect to PipeWire yet).
    ///
    /// If `exclude_self` is `false` (default), captures the sink monitor as-is. If `true`, uses
    /// the process Exclude mechanism to record all other apps' output through fan-in (excluded
    /// PID = `std::process::id()`).
    ///
    /// Select the sink to capture by `node.name` using `device_id`. `None` selects the default
    /// sink. Ignored when `exclude_self == true` (fan-in does not target a specific sink).
    /// Since `exclude_pids` was added, the fan-in path is taken whenever the
    /// effective exclusion set (`exclude_pids ∪ {self if exclude_self}`,
    /// see [`with_exclude_pids`](Self::with_exclude_pids)) is non-empty, and
    /// `device_id` is ignored on that path — not only when `exclude_self == true`.
    /// The actual connection and stream creation happen on a dedicated thread inside
    /// [`start`](CaptureBackend::start).
    pub fn new(exclude_self: bool, device_id: Option<String>) -> Self {
        Self {
            exclude_self,
            exclude_pids: Vec::new(),
            device_id,
            running: Arc::new(AtomicBool::new(false)),
            stop_tx: None,
            handle: None,
            shutdown: None,
            events: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    /// The `exclude_self` flag.
    pub fn exclude_self(&self) -> bool {
        self.exclude_self
    }

    /// Exclude these pids' playback in addition to `exclude_self` (fan-in
    /// path). Empty = no change.
    pub fn with_exclude_pids(mut self, pids: Vec<u32>) -> Self {
        self.exclude_pids = pids;
        self
    }

    /// The extra excluded pids.
    pub fn exclude_pids(&self) -> &[u32] {
        &self.exclude_pids
    }
}

impl Default for PwSystemBackend {
    fn default() -> Self {
        Self::new(false, None)
    }
}

impl CaptureBackend for PwSystemBackend {
    fn native_format(&self) -> (u32, u16) {
        (NATIVE_RATE, NATIVE_CHANNELS)
    }

    fn start(&mut self, sink: RawSink) -> Result<()> {
        // Safe on duplicate start (does nothing if already running).
        if self.running.load(Ordering::SeqCst) {
            return Ok(());
        }

        // Resolve requested sinks only against a complete inventory. The fan-in exclusion
        // path is not device-scoped, so only the ordinary monitor path performs this lookup.
        let excluded =
            effective_exclusion(self.exclude_self, &self.exclude_pids, std::process::id());
        let fan_in = !excluded.is_empty();
        let device_id = self.device_id.clone();
        if !fan_in {
            if let Some(id) = device_id.as_deref() {
                let devs = list_devices()?;
                if !devs
                    .iter()
                    .any(|device| device.is_loopback && device.id == id)
                {
                    return Err(Error::DeviceNotFound);
                }
            }
        }

        // Stop channel for the loop thread (the receiver is attached to the loop).
        let (stop_tx, stop_rx) = pw::channel::channel::<Terminate>();
        // Channel to synchronously return setup status to start(): Ok(()) if init→mainloop→context→
        // connect→stream→connect succeeds, or Err(error string) on failure.
        let (ready_tx, ready_rx) = mpsc::channel::<std::result::Result<(), String>>();

        let running = self.running.clone();
        running.store(true, Ordering::SeqCst);

        // exclude_self reuses the process Exclude mechanism. With std::process::id() as the
        // excluded PID, it fan-in links all other apps' output (Stream/Output/Audio) to our
        // capture input, recording "system audio minus this process's playback." The sink
        // monitor is already mixed, and PipeWire has no OS primitive to subtract just this
        // process, so app-output fan-in is the only way to exclude it. When exclude_self is
        // false, capture the sink monitor as-is. The fan-in path ignores device_id.
        // `exclude_pids` joins the same mechanism: the excluded PID is now the
        // whole `excluded` set, and an empty set (neither flag nor pids) is what
        // keeps the plain sink-monitor path.
        let generation_events: BackendEvents = Arc::new(Mutex::new(VecDeque::new()));
        let events_for_thread = generation_events.clone();
        let handle = thread::Builder::new()
            .name(
                if fan_in {
                    "flexaudio-pw-system-excl"
                } else {
                    "flexaudio-pw-system"
                }
                .into(),
            )
            .spawn(move || {
                if fan_in {
                    // Delegate to the Exclude mechanism, which records everything outside the
                    // excluded PID set. Stop/ready channels and Terminate are shared with system.
                    run_pw_process_loop(
                        PidSelect::Exclude(excluded),
                        sink,
                        stop_rx,
                        &ready_tx,
                        events_for_thread,
                    );
                } else {
                    run_pw_loop(device_id, sink, stop_rx, &ready_tx, events_for_thread);
                }
            })
            .map_err(|e| {
                running.store(false, Ordering::SeqCst);
                Error::Backend(format!("spawn pipewire thread: {e}"))
                    .with_context(ErrorContext::new(Operation::Start))
            })?;

        // Wait for setup. Treat thread exit without a ready message (recv error) as failure.
        match ready_rx.recv() {
            Ok(Ok(())) => {
                // Setup succeeded. Keep the stop sender and thread handle.
                self.events = generation_events;
                self.shutdown = None;
                self.stop_tx = Some(stop_tx);
                self.handle = Some(handle);
                Ok(())
            }
            Ok(Err(msg)) => {
                // Setup failed (PipeWire unavailable, no sink, connection failure, etc.). The
                // thread has already returned, so join it for cleanup.
                //
                // All failures map to Error::Backend. PipeWire provides no typed API to
                // distinguish permission denial (portal/Flatpak/RTKit restrictions) from
                // absence (no sink/source/session) for connection, stream creation, or format
                // negotiation failures. It returns errno or a generic string, with no
                // HRESULT/OSStatus equivalent that separates PermissionDenied from NotFound, so
                // classification like macOS/Windows is not possible. A missing requested sink
                // is detected earlier via enumerate_pw and returned as DeviceNotFound.
                running.store(false, Ordering::SeqCst);
                Err(rollback_worker(
                    owner::startup_error(&generation_events, msg),
                    handle,
                ))
            }
            Err(_) => {
                // The thread exited without sending ready (e.g. an unexpected panic).
                running.store(false, Ordering::SeqCst);
                Err(rollback_worker(
                    Error::Backend(
                        "pipewire setup thread terminated before signaling readiness".into(),
                    )
                    .with_context(ErrorContext::new(Operation::Start)),
                    handle,
                ))
            }
        }
    }

    fn stop(&mut self) {
        let _ = self.stop_checked();
    }

    fn stop_checked(&mut self) -> Result<()> {
        self.running.store(false, Ordering::SeqCst);
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(Terminate);
        }
        finish_worker(&mut self.handle, &mut self.shutdown, &self.events)
    }

    fn poll_event(&mut self) -> Option<Event> {
        poll_backend_event(&self.events)
    }
}

impl Drop for PwSystemBackend {
    fn drop(&mut self) {
        self.stop();
    }
}

// ============================================================================
// Process output loopback (capture a specific PID's app audio via fan-out)
// ============================================================================

/// [`CaptureBackend`] that captures a specific process's (PID) audio output through PipeWire.
/// Linux equivalent of WASAPI process loopback (`AUDIOCLIENT_ACTIVATION_PARAMS`).
///
/// # Explicitly link output ports to our input ports with link-factory
///
/// Device testing showed that WirePlumber ignored node selection through `stream.connect`'s
/// target/`target.object`, connecting capture to the default source (the microphone). Instead,
/// explicitly link the capture stream's input ports to the target process's output node ports
/// with link-factory (the API equivalent of `pw-link out_FL→in_FL / out_FR→in_FR`). The app's
/// original link to the default sink remains (fan-out), so audio still plays through the speakers.
///
/// Resolve PID-to-node mapping in two steps. PipeWire stores the PID on a Client object, not a
/// node, and `pipewire.sec.pid` (`*pw::keys::SEC_PID`) is always present in the Client's registry
/// global props (the daemon sets it from socket credentials, so it cannot be spoofed; verified
/// on a stock device setup). A node only points to its owning Client via `client.id`. Therefore,
/// follow PID → global id of the Client whose `pipewire.sec.pid == target_pid` →
/// `Stream/Output/Audio` nodes whose `client.id` has that id (see `resolve_node_pid`).
///
/// Connect the capture stream with `stream.connect(Direction::Input, None, ...)`, but omit
/// `AUTOCONNECT` to prevent automatic linking to the microphone and allow only explicit links.
/// This creates input ports (`input_FL/FR`); no data arrives until they are linked. Once the
/// target output ports and our input ports are available, create channel-matched links with
/// `core.create_object::<Link>("link-factory", ...)`, specifying `LINK_OUTPUT_NODE/PORT` and
/// `LINK_INPUT_NODE/PORT`.
///
/// # Handling `!Send`
///
/// As with [`PwSystemBackend`], keep all such values on one dedicated thread. `MainLoop`/`Context`/
/// `Core`/`Registry`/`Stream` are `!Send`, so they live on the `flexaudio-pw-process` thread;
/// the backend stores only `Send` values (a [`pipewire::channel::Sender`] for stopping,
/// [`JoinHandle`], and [`AtomicBool`]).
///
/// # Starting or stopping later is expected
///
/// It is normal for the target PID's node to be absent initially or appear later. Once connected
/// to the PipeWire daemon and the registry is available, [`start`](CaptureBackend::start)
/// succeeds and waits. It creates a link with link-factory as soon as registry `global` events
/// provide both the target output ports and our input ports. If `global_remove` detects that the
/// target disappeared, drop its link and wait again (relinking is idempotent). Return
/// [`Error::Backend`] immediately, without panicking, only if the daemon is unavailable or the
/// registry cannot be retrieved.
///
/// # `mode`: Include / Exclude
///
/// - [`ProcessMode::Include`] (default): Capture every `Stream/Output/Audio` node owned by the
///   target PID (fan-out links; one process can own several output streams).
/// - [`ProcessMode::Exclude`]: Fan-in link all app outputs (`Stream/Output/Audio`) except the target
///   PID to our capture input (the Include predicate inverted across multiple nodes). Keep nodes
///   with unresolved PIDs pending until their Client arrives, so the wrong process is not excluded.
///
/// The system source's `exclude_self` setting is unrelated to this process backend.
///
/// ```no_run
/// use flexaudio_os_linux::PwProcessBackend;
/// use flexaudio_core::backend::CaptureBackend;
/// use flexaudio_core::types::ProcessMode;
///
/// let backend = PwProcessBackend::new(12345, ProcessMode::Include);
/// assert_eq!(backend.native_format(), (48_000, 2));
/// // let mut backend = backend;
/// // backend.start(sink)?;  // Err(Backend) if PipeWire or the registry is unavailable;
/// //                        // otherwise succeeds and waits (Include waits for the target PID,
/// //                        // Exclude links other PIDs through fan-in as they appear).
/// // ...
/// // backend.stop();
/// ```
pub struct PwProcessBackend {
    /// PID of the target process. Match it against `pipewire.sec.pid` (`*pw::keys::SEC_PID`) on
    /// registry Client objects, then target output nodes that reference the Client via `client.id`
    /// (two-step lookup; see [`resolve_node_pid`]).
    target_pid: u32,
    /// How to handle the target PID. [`ProcessMode::Include`] captures only the target PID.
    /// [`ProcessMode::Exclude`] fan-in captures all app output except the target PID.
    mode: ProcessMode,
    /// Running flag (guards against duplicate starts and is used by drop). `Send`.
    running: Arc<AtomicBool>,
    /// Sender for stopping the loop thread. Set to `Some` by `start`.
    /// Uses the same [`Terminate`] as [`PwSystemBackend`].
    stop_tx: Option<pw::channel::Sender<Terminate>>,
    /// Handle for the PipeWire loop thread. Set to `Some` by `start`.
    handle: Option<JoinHandle<()>>,
    shutdown: Option<Result<()>>,
    events: BackendEvents,
}

impl PwProcessBackend {
    /// Construct the backend from the target PID and `mode` (does not connect to PipeWire yet).
    /// The actual connection, stream creation, and link-factory links happen on a dedicated
    /// thread inside [`start`](CaptureBackend::start).
    ///
    /// [`ProcessMode::Include`] captures only the target PID. [`ProcessMode::Exclude`] fan-in
    /// captures all app output except the target PID.
    pub fn new(target_pid: u32, mode: ProcessMode) -> Self {
        Self {
            target_pid,
            mode,
            running: Arc::new(AtomicBool::new(false)),
            stop_tx: None,
            handle: None,
            shutdown: None,
            events: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    /// PID of the process to capture.
    pub fn target_pid(&self) -> u32 {
        self.target_pid
    }

    /// `mode` (Include/Exclude).
    pub fn mode(&self) -> ProcessMode {
        self.mode
    }
}

impl CaptureBackend for PwProcessBackend {
    fn native_format(&self) -> (u32, u16) {
        (NATIVE_RATE, NATIVE_CHANNELS)
    }

    fn start(&mut self, sink: RawSink) -> Result<()> {
        // Safe on duplicate start (does nothing if already running).
        if self.running.load(Ordering::SeqCst) {
            return Ok(());
        }

        // Convert mode to a node-selection predicate.
        // - Include: Link every Stream/Output/Audio node owned by the target PID (a process can
        //   own several output streams).
        // - Exclude: Link all Stream/Output/Audio nodes except the target PID (fan-in).
        let select = match self.mode {
            ProcessMode::Include => PidSelect::Include(self.target_pid),
            ProcessMode::Exclude => {
                PidSelect::Exclude(std::collections::HashSet::from([self.target_pid]))
            }
        };

        // Stop channel for the loop thread (the receiver is attached to the loop).
        let (stop_tx, stop_rx) = pw::channel::channel::<Terminate>();
        // Channel to synchronously return setup status to start(). Success means PipeWire
        // connection, registry retrieval, stream creation, and registry listener registration.
        // A fan-out link to the target PID is not required for success (the target may not have
        // appeared yet; the registry callback links it when it does).
        let (ready_tx, ready_rx) = mpsc::channel::<std::result::Result<(), String>>();

        let running = self.running.clone();
        running.store(true, Ordering::SeqCst);

        let generation_events: BackendEvents = Arc::new(Mutex::new(VecDeque::new()));
        let events_for_thread = generation_events.clone();
        let handle = thread::Builder::new()
            .name("flexaudio-pw-process".into())
            .spawn(move || {
                run_pw_process_loop(select, sink, stop_rx, &ready_tx, events_for_thread);
            })
            .map_err(|e| {
                running.store(false, Ordering::SeqCst);
                Error::Backend(format!("spawn pipewire process thread: {e}"))
                    .with_context(ErrorContext::new(Operation::Start))
            })?;

        // Wait for setup. Treat thread exit without sending ready as failure.
        match ready_rx.recv() {
            Ok(Ok(())) => {
                // Setup succeeded (connection through registry listener registration). The
                // thread now waits for the target PID and creates a link-factory link once its
                // output ports and our input ports are available.
                self.events = generation_events;
                self.shutdown = None;
                self.stop_tx = Some(stop_tx);
                self.handle = Some(handle);
                Ok(())
            }
            Ok(Err(msg)) => {
                // Setup failed (PipeWire unavailable, connection/registry failure, etc.). Map
                // to Error::Backend; see the corresponding code in PwSystemBackend::start because
                // PipeWire cannot type-distinguish permission denial from absence. A missing
                // target PID is an expected wait for a registry event, not an error, so do not
                // return DeviceNotFound here.
                running.store(false, Ordering::SeqCst);
                Err(rollback_worker(
                    owner::startup_error(&generation_events, msg),
                    handle,
                ))
            }
            Err(_) => {
                // The thread exited without sending ready (e.g. an unexpected panic).
                running.store(false, Ordering::SeqCst);
                Err(rollback_worker(
                    Error::Backend(
                        "pipewire process setup thread terminated before signaling readiness"
                            .into(),
                    )
                    .with_context(ErrorContext::new(Operation::Start)),
                    handle,
                ))
            }
        }
    }

    fn stop(&mut self) {
        let _ = self.stop_checked();
    }

    fn stop_checked(&mut self) -> Result<()> {
        self.running.store(false, Ordering::SeqCst);
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(Terminate);
        }
        finish_worker(&mut self.handle, &mut self.shutdown, &self.events)
    }

    fn poll_event(&mut self) -> Option<Event> {
        poll_backend_event(&self.events)
    }
}

impl Drop for PwProcessBackend {
    fn drop(&mut self) {
        self.stop();
    }
}

/// PipeWire loop thread body for process capture.
///
/// Creates, runs, and destroys `MainLoop`/`Context`/`Core`/`Registry`/`Stream` (all `!Send`)
/// inside this function. Reports setup success or failure to the caller through `ready_tx`, then
/// runs `main_loop.run()` until [`Terminate`] on success. Watches the registry for the target
/// PID's node and links it with link-factory once the target output ports and our input ports are
/// available. `select` chooses Include (capture only the target PID) or Exclude (capture all others).
fn run_pw_process_loop(
    select: PidSelect,
    sink: RawSink,
    stop_rx: pw::channel::Receiver<Terminate>,
    ready_tx: &mpsc::Sender<std::result::Result<(), String>>,
    events: BackendEvents,
) {
    // Setup (connection, stream creation, and registry listener registration) is in a separate function.
    // Keep its return values alive for the whole run (dropping them stops watching and linking).
    let (main_loop, _keep) = match setup_pw_process(select, sink, events.clone()) {
        Ok(t) => t,
        Err(msg) => {
            // Report setup failure and exit (without panicking).
            let _ = ready_tx.send(Err(msg));
            return;
        }
    };

    // Attach the stop channel receiver to the loop. On Terminate, call quit().
    // quit() runs inside a loop callback, i.e. on this thread.
    let main_loop_for_quit = main_loop.clone();
    let _attached = stop_rx.attach(main_loop.loop_(), move |_terminate| {
        main_loop_for_quit.quit();
    });

    // Report setup success. run() now blocks and waits for the target PID to appear.
    if ready_tx.send(Ok(())).is_err() {
        // The caller is gone (e.g. start was dropped). Do not run.
        return;
    }

    // Run until Terminate or process exit. Wait here while the target PID is absent; the registry
    // callback creates the link when it appears.
    main_loop.run();
    // On exit, drop _attached then _keep (listener→stream→registry→core→main_loop), destroying
    // the PipeWire resources on this thread.
}

// Bound Node proxies + their info listeners, keyed by registry global id.
// Binding is what delivers `application.process.id`: the registry `global`
// props omit it, a bound object's `info` carries it.
type BoundNode = (pw::node::Node, pw::node::NodeListener);

/// Resources held for the duration of process capture. Dropping them stops capture.
///
/// - `CoreRc`: Owns `core.create_object("link-factory", ...)`. Shared via `Rc` so registry
///   callbacks can create links; placed last so it drops last.
/// - `StreamRc`: Our capture stream, connected with `Direction::Input`. It has input ports that
///   receive data once linked to the target output ports.
/// - `StreamListener`: Registers param_changed/process callbacks; dropping it unregisters them.
/// - `RegistryRc`: Registry proxy.
/// - `Registry Listener`: global/global_remove listeners; dropping it unregisters them.
/// - `links`: Map of [`pw::link::Link`] proxies created by link-factory, grouped by the registry
///   global id of each linked output node. Keep them alive on the loop thread because dropping
///   them breaks the links. Registry callbacks insert/remove/clear entries, so share the map via
///   `Rc<RefCell<…>>`. Either mode may have many entries (dropping an entry disconnects its links).
/// - `_bound_nodes`: bound Node proxies + their info listeners, keyed by registry global id — dropping an entry unregisters that node's info listener.
#[allow(clippy::type_complexity)]
struct ProcessKeep {
    _stream: pw::stream::StreamRc,
    _listener: pw::stream::StreamListener<UserData>,
    _registry: pw::registry::RegistryRc,
    _registry_listener: pw::registry::Listener,
    _links: std::rc::Rc<std::cell::RefCell<std::collections::HashMap<u32, Vec<pw::link::Link>>>>,
    _core: pw::core::CoreRc,
    _bound_nodes: std::rc::Rc<std::cell::RefCell<std::collections::HashMap<u32, BoundNode>>>,
}

/// PID and protocol provenance collected from a Client global.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ClientEntry {
    pid: Option<u32>,
    pulse_proxied: bool,
}

impl ClientEntry {
    fn from_props(app_pid: Option<&str>, sec_pid: Option<&str>, api: Option<&str>) -> Self {
        Self {
            pid: pid_from_props(app_pid, sec_pid),
            pulse_proxied: is_pulse_proxied(api),
        }
    }
}

fn is_pulse_proxied(api: Option<&str>) -> bool {
    api == Some("pipewire-pulse")
}

/// An output node's PID, provenance and bound-info state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct NodeEntry {
    /// Owning Client global id, from `client.id`.
    owning_client_id: Option<u32>,
    /// `application.process.id`, initially from the global and updated by bound info.
    app_pid: Option<u32>,
    /// Whether the latest bound PROPS update carried a valid application PID.
    app_pid_from_info: bool,
    /// Pulse provenance seen on this node; the owning Client is checked as well.
    pulse_proxied: bool,
    /// Whether any bound info has arrived, including state-only updates.
    info_seen: bool,
    /// Whether a bound PROPS update carried a dictionary. Exclude waits for it.
    props_seen: bool,
    /// How many output ports the node itself declares (`NodeInfoRef::n_output_ports`),
    /// filled from the bound node's info; `None` until that info arrives (and in
    /// `processes.rs`, which does not read it). Port globals trickle in one at a
    /// time, so this declared count is what tells `link_plan_is_complete` that a
    /// stereo node's FR port is still missing rather than that the node is mono.
    n_output_ports: Option<u32>,
}

/// Registration data for one port (read from a registry `ObjectType::Port` global).
///
/// Accumulate both target output node ports (`direction == "out"`) and our capture stream input
/// ports (`direction == "in"`) here, then link them by channel name (`audio.channel`).
#[derive(Clone, Debug, PartialEq, Eq)]
struct PortEntry {
    /// Registry global id of the node owning this port (`node.id` in the port props).
    node_id: u32,
    /// Direction (`"out"` = output port / `"in"` = input port).
    direction: String,
    /// Audio channel name (`"FL"` / `"FR"` / `"MONO"`, etc.), or empty if unavailable.
    channel: String,
}

/// Match output and input ports by channel and return the link pairs (independent of PipeWire and
/// arrival order). Converts output ports `(out_port_id, channel)` and input ports
/// `(in_port_id, channel)` into `(out_port_id, in_port_id)` pairs.
///
/// Matching rules:
/// 1. Prefer matching channel names (FL→FL / FR→FR / MONO→MONO, etc.).
/// 2. Duplicate mono output: if there is one output port (typically MONO) and multiple input
///    ports, link that output to every input (mono to both FL/FR).
/// 3. Order fallback: if channel names are unavailable or do not match, best-effort match the
///    remaining output and input ports in order.
///
/// Returns a list of unique link pairs, or an empty `Vec` if none can be created.
fn pair_ports(out_ports: &[(u32, String)], in_ports: &[(u32, String)]) -> Vec<(u32, u32)> {
    let mut pairs: Vec<(u32, u32)> = Vec::new();

    // Track matched input ports (do not link an input port twice).
    let mut used_in: Vec<bool> = vec![false; in_ports.len()];

    // Prefer matching channel names. For each output, find an unused input with the same nonempty channel name.
    for (out_id, out_ch) in out_ports {
        if out_ch.is_empty() {
            continue;
        }
        if let Some(idx) = in_ports
            .iter()
            .enumerate()
            .position(|(i, (_in_id, in_ch))| !used_in[i] && in_ch == out_ch)
        {
            used_in[idx] = true;
            pairs.push((*out_id, in_ports[idx].0));
        }
    }

    // Duplicate mono output. If there is only one output port and unmatched inputs remain, link
    // that output to all remaining inputs (e.g. mono to both FL/FR). Exclude inputs already
    // matched by channel name.
    if out_ports.len() == 1 {
        let (out_id, _out_ch) = &out_ports[0];
        for (i, _in_port) in in_ports.iter().enumerate() {
            if !used_in[i] {
                used_in[i] = true;
                pairs.push((*out_id, in_ports[i].0));
            }
        }
        return pairs;
    }

    // Order fallback. Match output ports not paired by channel name (including those with empty
    // channel names) to the remaining input ports in order.
    let mut paired_out: Vec<u32> = pairs.iter().map(|(o, _)| *o).collect();
    for (out_id, _out_ch) in out_ports {
        if paired_out.contains(out_id) {
            continue;
        }
        if let Some(idx) = used_in.iter().position(|used| !*used) {
            used_in[idx] = true;
            paired_out.push(*out_id);
            pairs.push((*out_id, in_ports[idx].0));
        }
    }

    pairs
}

/// Commit only a known mono/stereo layout after every declared port arrives.
/// Unknown and incomplete layouts stay unlinked until registry information completes.
fn link_plan_is_complete(
    expected_out: Option<u32>, // bound info's n_output_ports, if known
    out_ports_len: usize,
    in_ports_len: usize,
    pairs_len: usize,
    capture_channels: usize, // NATIVE_CHANNELS as usize
) -> bool {
    in_ports_len == capture_channels
        && expected_out.is_some_and(|n| n > 0 && n <= 2 && out_ports_len == n as usize)
        && pairs_len == capture_channels
}

/// Resolve a node's app PID, shared by capture and enumeration.
/// Pulse credentials name the proxy, so only a valid PID from bound node info
/// can resolve a Pulse node. Native clients retain the credential fallback.
fn resolve_node_pid(
    entry: &NodeEntry,
    client_pid: &std::collections::HashMap<u32, ClientEntry>,
) -> Option<u32> {
    let client = entry.owning_client_id.and_then(|id| client_pid.get(&id));
    if node_is_pulse_proxied(entry, client_pid) {
        return entry.app_pid.filter(|_| entry.app_pid_from_info);
    }
    entry
        .app_pid
        .or_else(|| client.and_then(|client| client.pid))
}

fn node_is_pulse_proxied(
    entry: &NodeEntry,
    client_pid: &std::collections::HashMap<u32, ClientEntry>,
) -> bool {
    entry.pulse_proxied
        || entry
            .owning_client_id
            .and_then(|id| client_pid.get(&id))
            .is_some_and(|client| client.pulse_proxied)
}

/// Apply bound info without treating state-only updates as PID removal.
/// For Pulse nodes, a PROPS update with a missing dictionary or unusable app PID
/// clears the old PID. Native nodes retain it. Pulse provenance is retained.
/// The latest PROPS update's PID validity is recorded even before provenance arrives.
fn update_node_info(
    entry: &mut NodeEntry,
    props_changed: bool,
    props: Option<(Option<&str>, Option<&str>)>,
    client_pid: &std::collections::HashMap<u32, ClientEntry>,
) -> bool {
    let previous = *entry;
    entry.info_seen = true;
    if props_changed {
        entry.props_seen |= props.is_some();
        let (app_pid, api) = props.unwrap_or_default();
        entry.pulse_proxied |= is_pulse_proxied(api);
        let app_pid = pid_from_props(app_pid, None);
        entry.app_pid_from_info = app_pid.is_some();
        if app_pid.is_some() || node_is_pulse_proxied(entry, client_pid) {
            entry.app_pid = app_pid;
        }
    }
    *entry != previous
}

/// Resolve a registry object's owning process from its properties.
///
/// `application.process.id` is preferred: for libpulse clients (Electron,
/// Chromium, most desktop apps) the daemon-assigned `pipewire.sec.pid` is
/// pipewire-pulse's own pid, and only `application.process.id` (set by
/// libpulse / libpipewire from the client's own props) names the app. It is
/// self-declared, which is acceptable for capture selection and self-exclusion.
/// `pipewire.sec.pid` is the fallback for clients that declare nothing.
pub(crate) fn pid_from_props(app_process_id: Option<&str>, sec_pid: Option<&str>) -> Option<u32> {
    let parse = |s: Option<&str>| s.and_then(|s| s.parse::<u32>().ok()).filter(|p| *p != 0);
    parse(app_process_id).or_else(|| parse(sec_pid))
}

/// Exclude needs bound props and a usable app PID, never a Pulse proxy PID.
fn exclude_decidable(
    entry: &NodeEntry,
    client_pid: &std::collections::HashMap<u32, ClientEntry>,
) -> bool {
    entry.props_seen && resolve_node_pid(entry, client_pid).is_some()
}

/// Node name for our capture stream. A unique name used to find our input ports in the registry;
/// includes the target PID to avoid collisions.
/// The suffix is now [`PidSelect::node_key`], not a bare pid, because Exclude
/// holds a whole set.
fn capture_node_name(key: &str) -> String {
    format!("flexaudio-capture-{key}")
}

/// The effective exclusion set for a system capture: the configured
/// `exclude_pids` plus `self_pid` when `exclude_self` is set (a set, so a
/// self pid already listed in `exclude_pids` does not appear twice).
///
/// An empty result means the plain sink-monitor path; a non-empty one means the
/// fan-in path. `self_pid` is a parameter rather than `std::process::id()` so
/// the decision is testable without depending on the running process.
fn effective_exclusion(
    exclude_self: bool,
    exclude_pids: &[u32],
    self_pid: u32,
) -> std::collections::HashSet<u32> {
    let mut excluded: std::collections::HashSet<u32> = exclude_pids.iter().copied().collect();
    if exclude_self {
        excluded.insert(self_pid);
    }
    excluded
}

/// Node-selection predicate for the fan-in capture loop.
///
/// `Include(pid)` links every output node owned by `pid` (a process can own
/// several streams); `Exclude(set)` links every resolved output node whose pid
/// is NOT in `set` (used by `ProcessMode::Exclude`, `exclude_self`, and
/// `exclude_pids`).
#[derive(Clone, PartialEq, Eq)]
enum PidSelect {
    /// Link every `Stream/Output/Audio` node whose resolved PID matches this PID (Include; a
    /// process can own several output streams).
    Include(u32),
    /// Link every `Stream/Output/Audio` node whose resolved pid is not in this
    /// set (Exclude / `exclude_self` / `exclude_pids`). The set holds the pids
    /// to keep OUT of the recording.
    Exclude(std::collections::HashSet<u32>),
}

impl PidSelect {
    /// Use the same decision when creating links and when revoking stale links.
    fn selects_node(
        &self,
        entry: &NodeEntry,
        client_pid: &std::collections::HashMap<u32, ClientEntry>,
    ) -> bool {
        if matches!(self, Self::Exclude(_)) && !exclude_decidable(entry, client_pid) {
            return false;
        }
        self.selects(resolve_node_pid(entry, client_pid))
    }

    /// Is `pid` one of the pids this predicate is *about* (the included pid, or
    /// a member of the exclusion set)? Used to track those Clients for
    /// `global_remove`.
    fn is_subject_pid(&self, pid: u32) -> bool {
        match self {
            PidSelect::Include(p) => *p == pid,
            PidSelect::Exclude(set) => set.contains(&pid),
        }
    }

    /// Whether a resolved PID is selected (PipeWire-independent). Unresolved (`None`) is not
    /// selected, preventing links to unverified nodes. Also used after info updates to ensure
    /// that a node is not accidentally left linked.
    fn selects(&self, resolved: Option<u32>) -> bool {
        match (self, resolved) {
            (PidSelect::Include(p), Some(r)) => *p == r,
            (PidSelect::Exclude(set), Some(r)) => !set.contains(&r),
            (_, None) => false,
        }
    }

    /// Suffix for this capture stream's `node.name` (registry-visible, unique
    /// enough to avoid colliding with another concurrent capture).
    fn node_key(&self) -> String {
        match self {
            PidSelect::Include(p) => p.to_string(),
            PidSelect::Exclude(set) => format!("excl-{}", set.iter().min().copied().unwrap_or(0)),
        }
    }
}

/// Process capture setup. Returns `Err(String)` on failure (does not panic).
///
/// Differences from [`setup_pw`] (system monitor):
/// - Do not set `STREAM_CAPTURE_SINK` or `AUTOCONNECT` (prevents automatic microphone links and
///   allows explicit links only). Give `node.name` a unique value ([`capture_node_name`]) so our
///   input ports can be found in the registry.
/// - Call `stream.connect(Direction::Input, None, ...)` once here. This creates input ports
///   (input_FL/FR), but data does not arrive until links are established (linking negotiates the
///   format, then data flows).
/// - Subscribe to registry `global` events continuously and track Clients / Nodes / Ports. The
///   Client's `pipewire.sec.pid` (`*pw::keys::SEC_PID`) is always present (set by the daemon from
///   socket credentials, so it cannot be spoofed; verified on a stock device setup). A node only
///   points to its Client via `client.id`, so PID lookup takes two steps (node → client.id →
///   Client PID; see [`resolve_node_pid`]). Reevaluate on each global event, regardless of
///   whether Client or Node arrives first.
///   application.process.id takes precedence — see pid_from_props.
/// - The [`PidSelect`] predicate chooses nodes to link. Include selects every Stream/Output/Audio
///   node owned by the target PID; Exclude selects every such node with a resolved PID outside
///   the excluded set (unresolved PIDs wait for their Client). Once a target's output ports and
///   our input ports are available, the loop-thread registry callback creates channel-matched
///   links with `core.create_object::<pw::link::Link>("link-factory", ...)` (see [`pair_ports`]:
///   FL→FL/FR→FR, with mono duplication). Keep links in `linked` (node_id → Links), grouped by node.
/// - On `global_remove`, drop only the affected entry if a linked node or its output port
///   disappears (preserve other Exclude links). If our node, an input port, or the target Client
///   disappears, drop all entries and wait again. All paths support idempotent relinking.
///
/// Keys used (verified in crate `keys.rs` to be outside feature gates):
/// `*pw::keys::SEC_PID`(="pipewire.sec.pid"), `*pw::keys::CLIENT_ID`(="client.id"),
/// `*pw::keys::NODE_ID`(="node.id"), `*pw::keys::PORT_DIRECTION`(="port.direction"),
/// `*pw::keys::AUDIO_CHANNEL`(="audio.channel"), `*pw::keys::LINK_OUTPUT_NODE`/
/// `LINK_OUTPUT_PORT`/`LINK_INPUT_NODE`/`LINK_INPUT_PORT`.
#[allow(clippy::type_complexity)]
fn setup_pw_process(
    select: PidSelect,
    sink: RawSink,
    events: BackendEvents,
) -> std::result::Result<(pw::main_loop::MainLoopRc, ProcessKeep), String> {
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;
    use std::rc::Rc;

    pw_init_once();

    let main_loop = pw::main_loop::MainLoopRc::new(None)
        .map_err(|e| format!("create pipewire main loop failed: {e}"))?;
    let context = pw::context::ContextRc::new(&main_loop, None)
        .map_err(|e| format!("create pipewire context failed: {e}"))?;
    let core = context
        .connect_rc(None)
        .map_err(|e| format!("connect to pipewire daemon failed (is PipeWire running?): {e}"))?;
    let registry = core
        .get_registry_rc()
        .map_err(|e| format!("get pipewire registry failed: {e}"))?;

    // Input (capture) stream properties.
    // - media.type=Audio / media.category=Capture: audio capture stream
    // - media.class=Stream/Input/Audio: graph role (input / recording side)
    // - media.role=Music: hint
    // - node.name=flexaudio-capture-<pid>: unique name used to find our input ports in the registry
    // Do not set STREAM_CAPTURE_SINK or AUTOCONNECT (prevents automatic microphone links; use
    // explicit link-factory links only). Include the selected PID in node.name to avoid collisions.
    // Include embeds the pid; Exclude embeds `excl-<smallest excluded pid>`.
    let node_name = capture_node_name(&select.node_key());
    let props = properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_CLASS => "Stream/Input/Audio",
        *pw::keys::MEDIA_ROLE => "Music",
        *pw::keys::NODE_NAME => node_name.as_str(),
    };

    let stream = pw::stream::StreamRc::new(core.clone(), "flexaudio-process-capture", props)
        .map_err(|e| format!("create pipewire capture stream failed: {e}"))?;

    let user_data = UserData {
        format: spa::param::audio::AudioInfoRaw::new(),
        sink,
        scratch: Default::default(),
        events: events.clone(),
        // The fan-out path reports readiness at connect time (see run_pw_process_loop) and does
        // not wait for negotiation on this listener.
        readiness: None,
    };
    // Register callbacks (shared helper; same param_changed/process behavior as system capture).
    let listener = add_capture_listener(&stream, user_data, &main_loop)?;

    // Connect our stream once (Direction::Input, target=None, no AUTOCONNECT). This creates input
    // ports (input_FL/FR); data does not arrive until linked (link establishment negotiates the
    // format, then data flows). Format POD is F32LE/48000/2ch.
    {
        let values = build_format_pod_bytes()?;
        let pod = Pod::from_bytes(&values)
            .ok_or_else(|| "build audio format pod from bytes failed".to_string())?;
        let mut params = [pod];
        stream
            .connect(
                spa::utils::Direction::Input,
                None,
                StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
                &mut params,
            )
            .map_err(|e| format!("connect pipewire capture stream failed: {e}"))?;
    }

    // Registry global id of our node (used to find our input ports by `node.id`). It may be
    // unset (0) just after connect, but should be set by the time input ports appear in the
    // registry. Reread stream.node_id() for each Port event.
    let self_node_id: Rc<Cell<Option<u32>>> = Rc::new(Cell::new(None));

    // State tables. Registry callbacks run only on the loop thread, so Cell/RefCell provide
    // interior mutability; no Mutex is needed.

    // Watched nodes: registry node global id → registration data (owning client.id / direct PID).
    let nodes: Rc<RefCell<HashMap<u32, NodeEntry>>> = Rc::new(RefCell::new(HashMap::new()));
    // Client global id -> PID and protocol provenance, even when PID is unknown.
    let client_pid: Rc<RefCell<HashMap<u32, ClientEntry>>> = Rc::new(RefCell::new(HashMap::new()));
    // Registry global ids of the Clients owned by this predicate's subject pids
    // (the included pid, or any member of the exclusion set). `global_remove`
    // uses it to notice such a Client disappearing. A set, because Exclude can
    // be about several pids at once.
    let target_client_ids: Rc<RefCell<std::collections::HashSet<u32>>> =
        Rc::new(RefCell::new(std::collections::HashSet::new()));
    // Ports: registry port global id → registration data (owning node.id / direction / channel).
    let ports: Rc<RefCell<HashMap<u32, PortEntry>>> = Rc::new(RefCell::new(HashMap::new()));
    // Currently linked output nodes: registry global id → Link proxies created for that node.
    // Keep them for the entire run because dropping them disconnects the links. Either mode may
    // have many entries (a process can own several output streams). Remove an entry to disconnect
    // one node or clear the map to disconnect all.
    let linked: Rc<RefCell<HashMap<u32, Vec<pw::link::Link>>>> =
        Rc::new(RefCell::new(HashMap::new()));
    // Bound Node proxies + their info listeners, keyed by registry global id.
    // Binding is what delivers `application.process.id`: the registry `global`
    // props omit it, a bound object's `info` carries it.
    let bound_nodes: Rc<RefCell<HashMap<u32, BoundNode>>> = Rc::new(RefCell::new(HashMap::new()));

    // Reconcile selection before adding links, including when a Client arrives
    // after its Node and reveals Pulse provenance. Both Include and Exclude link
    // every matching node: one process can own several output streams, and each
    // must be recorded.
    #[allow(clippy::too_many_arguments)]
    fn try_link(
        core: &pw::core::CoreRc,
        stream: &pw::stream::StreamRc,
        select: &PidSelect,
        self_node_id: &Cell<Option<u32>>,
        nodes: &RefCell<HashMap<u32, NodeEntry>>,
        client_pid: &RefCell<HashMap<u32, ClientEntry>>,
        ports: &RefCell<HashMap<u32, PortEntry>>,
        linked: &RefCell<HashMap<u32, Vec<pw::link::Link>>>,
        events: &BackendEvents,
        main_loop: &pw::main_loop::MainLoopRc,
    ) -> Result<()> {
        let result = (|| -> Result<()> {
            {
                let nodes = nodes.borrow();
                let client_pid = client_pid.borrow();
                linked.borrow_mut().retain(|id, _| {
                    nodes
                        .get(id)
                        .is_some_and(|entry| select.selects_node(entry, &client_pid))
                });
            }
            // Reread our node id from the stream (it may be unset just after connect).
            // When unset, this returns SPA_ID_INVALID (=ID_ANY=u32::MAX) or 0.
            let sid = stream.node_id();
            if sid != 0 && sid != pw::constants::ID_ANY {
                self_node_id.set(Some(sid));
            }
            let Some(self_nid) = self_node_id.get() else {
                return Ok(());
            };

            // Use the predicate to select output node ids to link.
            // - Include: every node whose resolved PID equals pid (one process can
            //   own several output streams).
            // - Exclude: every node with a resolved PID outside the excluded set (unresolved PIDs excluded).
            let targets: Vec<u32> = {
                let nodes = nodes.borrow();
                let client_pid = client_pid.borrow();
                let linked = linked.borrow();
                nodes
                    .iter()
                    .filter(|(id, entry)| {
                        if linked.contains_key(id) {
                            return false;
                        }
                        select.selects_node(entry, &client_pid)
                    })
                    .map(|(&id, _)| id)
                    .collect()
            };

            if targets.is_empty() {
                return Ok(());
            }

            // Get our input ports from the ports table (shared by all target nodes).
            let in_ports: Vec<(u32, String)> = {
                let ports = ports.borrow();
                ports
                    .iter()
                    .filter(|(_pid, p)| p.node_id == self_nid && p.direction == "in")
                    .map(|(&pid, p)| (pid, p.channel.clone()))
                    .collect()
            };
            // Cannot link until our input ports appear (reevaluate on the next global event).
            if in_ports.is_empty() {
                return Ok(());
            }

            for target_node_id in targets {
                // Get the target node's output ports from the ports table.
                let out_ports: Vec<(u32, String)> = {
                    let ports = ports.borrow();
                    ports
                        .iter()
                        .filter(|(_pid, p)| p.node_id == target_node_id && p.direction == "out")
                        .map(|(&pid, p)| (pid, p.channel.clone()))
                        .collect()
                };
                // Cannot link this node until its output ports appear (reevaluate next time).
                if out_ports.is_empty() {
                    continue;
                }

                // Pair ports by channel (FL→FL/FR→FR; duplicate mono; fall back to order if unavailable).
                let pairs = pair_ports(&out_ports, &in_ports);
                // The node's own declared output-port count, if its bound info has
                // arrived. Borrow of `nodes` ends with this block — nothing below is
                // allowed to hold it across `create_object`.
                let expected_out: Option<u32> = nodes
                    .borrow()
                    .get(&target_node_id)
                    .and_then(|entry| entry.n_output_ports);
                if expected_out.is_some_and(|count| count > 2) || out_ports.len() > 2 {
                    return Err(Error::UnsupportedFormat(
                        "pipewire input supports at most two channels".into(),
                    )
                    .with_context(ErrorContext::new(Operation::Link)));
                }
                // Commit only a complete plan. Port globals arrive one at a time on
                // BOTH sides, and a partial pairing inserted into `linked` below is
                // fossilised, because a linked node is never re-paired: half-arrived
                // capture inputs link FL alone, and a half-arrived target (one output
                // port of a declared stereo node) makes `pair_ports`' mono rule
                // duplicate FL onto both inputs. Leaving the node OUT of `linked`
                // here is deliberate: the next port global re-evaluates it, and by
                // then the missing port exists. (Subsumes the old is-empty check: a
                // complete plan has at least one pair.)
                if !link_plan_is_complete(
                    expected_out,
                    out_ports.len(),
                    in_ports.len(),
                    pairs.len(),
                    NATIVE_CHANNELS as usize,
                ) {
                    continue;
                }
                // Stereo channel ordering must be known before committing a route. Pairing
                // unknown names by HashMap iteration order can silently swap the channels.
                let stereo_ports = |ports: &[(u32, String)]| {
                    ports.len() == 2
                        && ports.iter().filter(|(_, channel)| channel == "FL").count() == 1
                        && ports.iter().filter(|(_, channel)| channel == "FR").count() == 1
                };
                if !stereo_ports(&in_ports) || (out_ports.len() == 2 && !stereo_ports(&out_ports)) {
                    return Err(Error::UnsupportedFormat(
                        "pipewire channel routing is unknown or unsupported".into(),
                    )
                    .with_context(ErrorContext::new(Operation::Link)));
                }
                let want = pairs.len();

                // Link each pair with link-factory.
                let mut created: Vec<pw::link::Link> = Vec::with_capacity(want);
                for (out_port_id, in_port_id) in pairs {
                    let link_props = properties! {
                        *pw::keys::LINK_OUTPUT_NODE => target_node_id.to_string(),
                        *pw::keys::LINK_OUTPUT_PORT => out_port_id.to_string(),
                        *pw::keys::LINK_INPUT_NODE => self_nid.to_string(),
                        *pw::keys::LINK_INPUT_PORT => in_port_id.to_string(),
                    };
                    let link = core
                        .create_object::<pw::link::Link>("link-factory", &link_props)
                        .map_err(|error| {
                            Error::Backend(format!("pipewire link creation failed: {error}"))
                                .with_context(ErrorContext::new(Operation::Link))
                        })?;
                    created.push(link);
                }

                // All pairs are linked. Keep the Link proxies grouped by node.
                linked.borrow_mut().insert(target_node_id, created);
            }
            Ok(())
        })();
        if let Err(error) = &result {
            linked.borrow_mut().clear();
            push_backend_event(
                events,
                Event::TerminalError {
                    error: error.clone(),
                },
            );
            main_loop.quit();
        }
        result
    }

    // registry global / global_remove listeners.
    // global registers Client→client_pid, Stream/Output/Audio node→nodes, and Port→ports entries,
    // then reevaluates links with try_link each time.
    // `PidSelect` is no longer `Copy` (Exclude owns a HashSet), so every closure
    // that used to capture it by copy gets its own clone.
    let select_for_global = select.clone();
    let select_for_remove = select.clone();
    let events_for_global = events.clone();
    let loop_for_global = main_loop.clone();
    let core_for_global = core.clone();
    let stream_for_global = stream.clone();
    let self_node_for_global = self_node_id.clone();
    let nodes_for_global = nodes.clone();
    let client_pid_for_global = client_pid.clone();
    let target_client_for_global = target_client_ids.clone();
    let ports_for_global = ports.clone();
    let linked_for_global = linked.clone();
    let registry_for_global = registry.clone();
    let bound_for_global = bound_nodes.clone();

    let events_for_remove = events.clone();
    let loop_for_remove = main_loop.clone();
    let core_for_remove = core.clone();
    let stream_for_remove = stream.clone();
    let self_node_for_remove = self_node_id.clone();
    let nodes_for_remove = nodes.clone();
    let client_pid_for_remove = client_pid.clone();
    let target_client_for_remove = target_client_ids.clone();
    let ports_for_remove = ports.clone();
    let linked_for_remove = linked.clone();
    let bound_for_remove = bound_nodes.clone();

    let _registry_listener = registry
        .add_listener_local()
        .global(move |global| {
            // A panic crossing FFI is UB, so wrap the callback body in catch_unwind.
            let _ = catch_unwind(AssertUnwindSafe(|| {
                let Some(props) = global.props else {
                    return;
                };
                match global.type_ {
                    pw::types::ObjectType::Client => {
                        let client = ClientEntry::from_props(
                            props.get(*pw::keys::APP_PROCESS_ID),
                            props.get(*pw::keys::SEC_PID),
                            props.get(*pw::keys::CLIENT_API),
                        );
                        client_pid_for_global.borrow_mut().insert(global.id, client);
                        // Track subject Clients for disappearance detection.
                        if client
                            .pid
                            .is_some_and(|pid| select_for_global.is_subject_pid(pid))
                        {
                            target_client_for_global.borrow_mut().insert(global.id);
                        }
                    }
                    pw::types::ObjectType::Node => {
                        // Only select app output nodes (playback streams).
                        let media_class = props.get(*pw::keys::MEDIA_CLASS).unwrap_or("");
                        if media_class != "Stream/Output/Audio" {
                            return;
                        }
                        // client.id points to the owning Client.
                        let owning_client_id = props
                            .get(*pw::keys::CLIENT_ID)
                            .and_then(|s| s.parse::<u32>().ok());
                        // The global usually omits the app PID; bound info supplies it.
                        let app_pid = pid_from_props(props.get(*pw::keys::APP_PROCESS_ID), None);
                        nodes_for_global.borrow_mut().insert(
                            global.id,
                            NodeEntry {
                                owning_client_id,
                                app_pid,
                                app_pid_from_info: false,
                                pulse_proxied: is_pulse_proxied(props.get(*pw::keys::CLIENT_API)),
                                info_seen: false,
                                props_seen: false,
                                // Only the bound info carries the declared port count.
                                n_output_ports: None,
                            },
                        );

                        // Bind to learn the app PID. If binding fails, Exclude
                        // leaves this node undecidable and never links it.
                        let bound: std::result::Result<pw::node::Node, _> =
                            registry_for_global.bind(global);
                        if let Ok(node) = bound {
                            let node_id = global.id;
                            let events_for_info = events_for_global.clone();
                            let loop_for_info = loop_for_global.clone();
                            let nodes_for_info = nodes_for_global.clone();
                            let client_pid_for_info = client_pid_for_global.clone();
                            let ports_for_info = ports_for_global.clone();
                            let linked_for_info = linked_for_global.clone();
                            let self_node_for_info = self_node_for_global.clone();
                            let core_for_info = core_for_global.clone();
                            let stream_for_info = stream_for_global.clone();
                            let select_for_info = select.clone(); // `select` is no longer Copy
                            let listener = node
                                .add_listener_local()
                                .info(move |info| {
                                    let _ = catch_unwind(AssertUnwindSafe(|| {
                                        let props_changed = info
                                            .change_mask()
                                            .contains(pw::node::NodeChangeMask::PROPS);
                                        let props = info.props().map(|props| {
                                            (
                                                props.get(*pw::keys::APP_PROCESS_ID),
                                                props.get(*pw::keys::CLIENT_API),
                                            )
                                        });

                                        // The node's own declared output-port count. Port
                                        // globals arrive one at a time, so this is what
                                        // distinguishes "stereo, FR not here yet" from
                                        // "mono" in link_plan_is_complete.
                                        let n_out = info.n_output_ports();

                                        // PID/provenance and declared-port changes require
                                        // reevaluation; unrelated state updates do not.
                                        let update = {
                                            let mut nodes = nodes_for_info.borrow_mut();
                                            let Some(entry) = nodes.get_mut(&node_id) else {
                                                return;
                                            };
                                            let changed = update_node_info(
                                                entry,
                                                props_changed,
                                                props,
                                                &client_pid_for_info.borrow(),
                                            );
                                            let n_out_changed = entry.n_output_ports != Some(n_out);
                                            if n_out_changed {
                                                entry.n_output_ports = Some(n_out);
                                            }
                                            (changed || n_out_changed).then_some(n_out_changed)
                                        };
                                        let Some(n_out_changed) = update else {
                                            return;
                                        };

                                        // A declared output-port count that changed after we
                                        // already committed a plan means the plan we latched
                                        // was built against the old count and may be
                                        // incomplete (e.g. the node declared 1 port when we
                                        // linked and now declares 2). Drop the links and rebuild
                                        // the plan against the new count; a plan that became
                                        // incomplete must be revisited.
                                        if n_out_changed {
                                            linked_for_info.borrow_mut().remove(&node_id);
                                        }

                                        // All state borrows have ended. Reconciliation also
                                        // unlinks a Pulse node whose app PID was invalidated.
                                        let _ = try_link(
                                            &core_for_info,
                                            &stream_for_info,
                                            &select_for_info,
                                            &self_node_for_info,
                                            &nodes_for_info,
                                            &client_pid_for_info,
                                            &ports_for_info,
                                            &linked_for_info,
                                            &events_for_info,
                                            &loop_for_info,
                                        );
                                    }));
                                })
                                .register();
                            bound_for_global
                                .borrow_mut()
                                .insert(node_id, (node, listener));
                        }
                    }
                    pw::types::ObjectType::Port => {
                        // Accumulate ports (both target output ports and our input ports are read from here).
                        let Some(node_id) = props
                            .get(*pw::keys::NODE_ID)
                            .and_then(|s| s.parse::<u32>().ok())
                        else {
                            return;
                        };
                        let direction = props
                            .get(*pw::keys::PORT_DIRECTION)
                            .unwrap_or("")
                            .to_string();
                        if direction != "out" && direction != "in" {
                            return;
                        }
                        let channel = props
                            .get(*pw::keys::AUDIO_CHANNEL)
                            .unwrap_or("")
                            .to_string();
                        ports_for_global.borrow_mut().insert(
                            global.id,
                            PortEntry {
                                node_id,
                                direction,
                                channel,
                            },
                        );
                    }
                    _ => return,
                }

                // State changed regardless of whether this was a Client, Node, or Port; reevaluate.
                // We are on the loop thread, so it is safe to access the `!Send` core/stream.
                let _ = try_link(
                    &core_for_global,
                    &stream_for_global,
                    &select_for_global,
                    &self_node_for_global,
                    &nodes_for_global,
                    &client_pid_for_global,
                    &ports_for_global,
                    &linked_for_global,
                    &events_for_global,
                    &loop_for_global,
                );
            }));
        })
        .global_remove(move |id| {
            // A panic crossing FFI is UB, so wrap the callback body in catch_unwind.
            let _ = catch_unwind(AssertUnwindSafe(|| {
                // Remove the id from the appropriate table and update links. To avoid borrow
                // conflicts, use scoped borrows to determine actions as bool/owner values first,
                // then update linked and call try_link.
                let mut relink_needed = false;

                // Whether the removed id is a linked node, target/excluded Client, or our node
                // (target/excluded Clients are checked against the set).
                let was_linked_node = linked_for_remove.borrow().contains_key(&id);
                let was_target_client = target_client_for_remove.borrow().contains(&id);
                // Whether our own capture stream node disappeared.
                let was_self_node = self_node_for_remove.get() == Some(id);

                // If the removed id is an output port owned by a linked node, find its owner node
                // id. Also check whether one of our input ports disappeared. Missing our input
                // port must be detected; otherwise a disconnected input can remain marked linked
                // and never recover from silence. Compute owner/bool values within this scope so
                // `ports.borrow()` is not held across try_link.
                let (linked_out_owner, was_self_in_port): (Option<u32>, bool) = {
                    let ports = ports_for_remove.borrow();
                    let owner = ports.get(&id).and_then(|p| {
                        if p.direction == "out"
                            && linked_for_remove.borrow().contains_key(&p.node_id)
                        {
                            Some(p.node_id)
                        } else {
                            None
                        }
                    });
                    let self_in = if let Some(self_nid) = self_node_for_remove.get() {
                        ports
                            .get(&id)
                            .map(|p| p.node_id == self_nid && p.direction == "in")
                            .unwrap_or(false)
                    } else {
                        false
                    };
                    (owner, self_in)
                };

                // If our node/input port or a target Client disappears, clear all links and
                // reevaluate.
                // - Our node/input port: the input side disappeared, invalidating all links.
                // - Target/excluded Client: Include loses all nodes for that PID (capture target gone).
                //   (Japanese above: the Exclude case used to be cleared here too, on the
                //   grounds that clear-all then relink also ends up correct.) That applies
                //   to Include only. In Exclude mode a tracked client id is an EXCLUDED
                //   client, whose nodes are never in `linked` — so clearing every link on
                //   its departure drops audio we were legitimately recording and costs an
                //   audible gap while the links are rebuilt, which an Electron host pays
                //   every time one of its libpulse helper clients closes. Nothing needs
                //   clearing: the excluded client's own nodes get their own `global_remove`,
                //   which handles any staleness.
                if was_self_node
                    || was_self_in_port
                    || (was_target_client && matches!(select_for_remove, PidSelect::Include(_)))
                {
                    // Drop all held Links (disconnect them) and return to the unlinked state.
                    linked_for_remove.borrow_mut().clear();
                    relink_needed = true;
                } else {
                    // Remove only the disappeared node (preserve other Exclude links).
                    if was_linked_node {
                        linked_for_remove.borrow_mut().remove(&id);
                        relink_needed = true;
                    }
                    if let Some(owner) = linked_out_owner {
                        linked_for_remove.borrow_mut().remove(&owner);
                        relink_needed = true;
                    }
                }

                if was_target_client {
                    target_client_for_remove.borrow_mut().remove(&id);
                }
                if was_self_node {
                    // Clear the cached id if our node disappeared. try_link rereads it from the
                    // stream and can pick up the new id if the node is recreated.
                    self_node_for_remove.set(None);
                }

                // Remove the disappeared id from every table to prevent stale PID/port lookups.
                nodes_for_remove.borrow_mut().remove(&id);
                client_pid_for_remove.borrow_mut().remove(&id);
                ports_for_remove.borrow_mut().remove(&id);
                bound_for_remove.borrow_mut().remove(&id);

                // Once waiting again, immediately retry links if another target is already ready.
                if relink_needed {
                    let _ = try_link(
                        &core_for_remove,
                        &stream_for_remove,
                        &select_for_remove,
                        &self_node_for_remove,
                        &nodes_for_remove,
                        &client_pid_for_remove,
                        &ports_for_remove,
                        &linked_for_remove,
                        &events_for_remove,
                        &loop_for_remove,
                    );
                }
            }));
        })
        .register();

    Ok((
        main_loop,
        ProcessKeep {
            _stream: stream,
            _listener: listener,
            _registry: registry,
            _registry_listener,
            _links: linked,
            _core: core,
            _bound_nodes: bound_nodes,
        },
    ))
}

/// State shared between the `process` and `param_changed` callbacks.
///
/// Holds the negotiated format (channels) for access from `process`.
struct UserData {
    /// Capture format negotiated by PipeWire. Updated by `param_changed`.
    format: spa::param::audio::AudioInfoRaw,
    /// Destination for raw frames. `process` pushes through `&mut`.
    sink: RawSink,
    scratch: std::rc::Rc<std::cell::RefCell<Vec<f32>>>,
    events: BackendEvents,
    /// Readiness report for the system-monitor path: `param_changed` reports success once a
    /// format is negotiated. `None` for the process fan-out path, which reports readiness at
    /// connect time and does not wait for negotiation on this listener.
    readiness: Option<std::rc::Rc<Readiness>>,
}

/// Register `param_changed` / `process` callbacks on a capture stream.
///
/// Both [`PwSystemBackend`] (system monitor) and [`PwProcessBackend`] (process fan-out) use the
/// same callback behavior, so this is a shared helper. It stores the negotiated format in
/// `param_changed`, then sends interleaved f32 data dequeued by `process` to [`RawSink::push`]
/// without blocking.
///
/// Returns the registered [`StreamListener`](pw::stream::StreamListener). The caller must keep
/// it alive for the entire run because dropping it unregisters the callbacks.
fn add_capture_listener(
    stream: &pw::stream::StreamRc,
    user_data: UserData,
    main_loop: &pw::main_loop::MainLoopRc,
) -> std::result::Result<pw::stream::StreamListener<UserData>, String> {
    // Listener-owned storage is prepared on the setup thread and travels with the listener
    // to PipeWire's RT thread. The process callback never grows this allocation.
    let scratch = user_data.scratch.clone();
    scratch.borrow_mut().reserve(PROC_SCRATCH_CAP);

    let diagnostics = user_data.sink.diagnostics();
    let loop_for_format = main_loop.clone();
    stream
        .add_local_listener_with_user_data(user_data)
        .param_changed(move |_stream, user_data, id, param| {
            // A panic crossing FFI is UB, so wrap the callback body in catch_unwind.
            let _ = catch_unwind(AssertUnwindSafe(|| {
                if id != pw::spa::param::ParamType::Format.as_raw() {
                    return;
                }
                // NULL clears the format; it is not successful negotiation.
                let Some(param) = param else {
                    user_data.format = spa::param::audio::AudioInfoRaw::new();
                    return;
                };
                // Parsing may mutate its destination even on failure. Validate a fresh value
                // before replacing the format used by the process callback.
                let mut format = spa::param::audio::AudioInfoRaw::new();
                if format.parse(param).is_ok() && format.channels() > 2 {
                    push_backend_event(
                        &user_data.events,
                        Event::TerminalError {
                            error: Error::UnsupportedFormat(
                                "pipewire input supports at most two channels".into(),
                            )
                            .with_context(ErrorContext::new(Operation::Start)),
                        },
                    );
                    if let Some(readiness) = &user_data.readiness {
                        readiness.report_failure("unsupported pipewire channel count".into());
                    }
                    loop_for_format.quit();
                    return;
                }
                let mut format = spa::param::audio::AudioInfoRaw::new();
                let accepted = matches!(
                    format_utils::parse_format(param),
                    Ok((MediaType::Audio, MediaSubtype::Raw))
                ) && format.parse(param).is_ok()
                    && format.format() == spa::param::audio::AudioFormat::F32LE
                    && format.channels() != 0
                    && format.channels() == u32::from(user_data.sink.native_channels())
                    && format.rate() != 0;
                if !accepted {
                    if let Some(readiness) = &user_data.readiness {
                        if !readiness.is_reported() {
                            readiness.report_failure("unacceptable pipewire capture format".into());
                            loop_for_format.quit();
                        }
                    }
                    return;
                }
                user_data.format = format;
                if let Some(readiness) = &user_data.readiness {
                    readiness.report_ready();
                }
            }));
        })
        .process(move |stream, user_data| {
            // Runs on the real-time thread. Avoid blocking and allocation.
            // A panic crossing FFI is UB, so wrap the callback body in catch_unwind.
            let result = catch_unwind(AssertUnwindSafe(|| {
                // Do nothing if there is no buffer (do not panic).
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                let datas = buffer.datas_mut();
                if datas.is_empty() {
                    return;
                }
                let data = &mut datas[0];
                // Save the chunk fields before borrowing data(). `flags` and `stride` come from
                // the SPA chunk and describe the validity and layout of the bytes.
                let chunk = data.chunk();
                let size = chunk.size() as usize;
                let offset = chunk.offset() as usize;
                let stride = chunk.stride();
                if chunk
                    .flags()
                    .contains(pw::spa::buffer::ChunkFlags::CORRUPTED)
                {
                    diagnostics.record_corrupt_buffer(None);
                    return;
                }
                if size == 0 {
                    return;
                }
                let Some(bytes) = data.data() else {
                    return;
                };
                // Data::data() spans maxsize. SPA offsets are modulo maxsize and chunk sizes
                // are clamped to maxsize; still reject a region that exceeds mapped memory.
                if bytes.is_empty() {
                    return;
                }
                let offset = offset % bytes.len();
                let size = size.min(bytes.len());
                let Some(end) = offset.checked_add(size).filter(|end| *end <= bytes.len()) else {
                    return;
                };
                let valid = &bytes[offset..end];
                // Interleaved f32: `channels` samples per frame. The negotiated channel count
                // (see param_changed) decides the audio bytes actually carried by each frame.
                let channels = user_data.format.channels() as usize;
                if channels == 0 {
                    return;
                }
                let frame_bytes = channels * std::mem::size_of::<f32>();
                // spa_chunk.stride is the byte distance between consecutive frames. It can be
                // larger than the audio frame when the producer pads each frame.
                if stride > 0 && (stride as usize) < frame_bytes {
                    diagnostics.record_malformed_buffer(None);
                    return;
                }
                let step = if stride > 0 {
                    stride as usize
                } else {
                    frame_bytes
                };
                // The last complete frame need not include trailing padding.
                let n_frames = if valid.len() >= frame_bytes {
                    1 + (valid.len() - frame_bytes) / step
                } else {
                    0
                };
                let n_floats = n_frames * channels;
                if n_frames == 0 {
                    return;
                }
                // Read the bytes as interleaved f32. `data` alignment is not guaranteed, so use
                // from_le_bytes instead of align_to. Fill the preallocated reusable buffer and
                // push once (RawSink::push is nonblocking and drops data when full).
                {
                    let Ok(mut scratch) = scratch.try_borrow_mut() else {
                        diagnostics.record_callback_rejected(None);
                        return;
                    };
                    if n_floats > scratch.capacity() {
                        diagnostics.record_malformed_buffer(None);
                        return;
                    }
                    scratch.clear();
                    for frame in 0..n_frames {
                        // Audio bytes of this frame; any trailing padding up to `step` is skipped.
                        let base = frame * step;
                        for i in 0..channels {
                            let b = base + i * 4;
                            let v = f32::from_le_bytes([
                                valid[b],
                                valid[b + 1],
                                valid[b + 2],
                                valid[b + 3],
                            ]);
                            scratch.push(v);
                        }
                    }
                    // PTS: currently use the monotonic arrival time (`monotonic_now_ns`) as a
                    // substitute. This monotonic approximation works because the downstream
                    // ClockNormalizer establishes the initial origin. It can later be replaced
                    // with the device clock from `pw_buffer.time`.
                    user_data.sink.push(&scratch, monotonic_now_ns());
                }
            }));
            if result.is_err() {
                diagnostics.record_callback_rejected(None);
            }
        })
        .register()
        .map_err(|e| format!("register pipewire stream listener failed: {e}"))
}

/// Build the byte representation of the requested format POD (f32 / 48000 / 2 channels).
///
/// Because rate/channels are explicit, PipeWire automatically inserts `audioconvert` if the
/// graph differs and converts to 48 kHz / stereo / f32. Build a POD from the returned bytes with
/// [`Pod::from_bytes`] (keep the byte array alive through the connect call, since the POD points to it).
fn build_format_pod_bytes() -> std::result::Result<Vec<u8>, String> {
    let mut audio_info = spa::param::audio::AudioInfoRaw::new();
    audio_info.set_format(spa::param::audio::AudioFormat::F32LE);
    audio_info.set_rate(NATIVE_RATE);
    audio_info.set_channels(NATIVE_CHANNELS as u32);

    let obj = pw::spa::pod::Object {
        type_: pw::spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: pw::spa::param::ParamType::EnumFormat.as_raw(),
        properties: audio_info.into(),
    };
    let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(obj),
    )
    .map_err(|e| format!("serialize audio format pod failed: {e}"))?
    .0
    .into_inner();
    Ok(values)
}

/// PipeWire loop thread body.
///
/// Creates, runs, and destroys `MainLoop`/`Context`/`Core`/`Stream` (all `!Send`) only inside this
/// function, without crossing thread boundaries. Reports readiness to the caller through
/// `ready_tx` once the stream has negotiated its format, then runs `main_loop.run()` until a stop
/// request on success.
fn run_pw_loop(
    device_id: Option<String>,
    sink: RawSink,
    stop_rx: pw::channel::Receiver<Terminate>,
    ready_tx: &mpsc::Sender<std::result::Result<(), String>>,
    events: BackendEvents,
) {
    // Single-shot readiness report. It is filled by `param_changed` (success) or by the
    // negotiation deadline timer below (failure).
    let readiness = std::rc::Rc::new(Readiness {
        tx: ready_tx.clone(),
        sent: std::cell::Cell::new(false),
    });

    // Setup is in a separate function. Keep its return values alive for the whole run (dropping them stops it).
    let (main_loop, _stream, _listener) = match setup_pw(device_id, sink, readiness.clone(), events)
    {
        Ok(t) => t,
        Err(msg) => {
            // Report setup failure and exit (without panicking).
            let _ = ready_tx.send(Err(msg));
            return;
        }
    };

    // Attach the stop channel receiver to the loop. On Terminate, call quit(). attach only
    // borrows this local `main_loop`, so AttachedReceiver is scoped to this stack frame (no
    // self-referential struct or unsafe lifetime extension is needed). quit() runs in a loop
    // callback, i.e. on this thread.
    let main_loop_for_quit = main_loop.clone();
    let _attached = stop_rx.attach(main_loop.loop_(), move |_terminate| {
        main_loop_for_quit.quit();
    });

    // Readiness is reported only once the format is negotiated (see `param_changed`), never at
    // connect time: reporting at connect time told the caller capture was running even if the
    // stream never negotiated. A one-shot timer bounds the wait, so a stream that never
    // negotiates cannot block `start()` forever; if a report was already sent, it is a no-op.
    let readiness_for_timer = readiness.clone();
    let main_loop_for_timeout = main_loop.clone();
    let _timer = main_loop.loop_().add_timer(move |_expirations| {
        if !readiness_for_timer.is_reported() {
            readiness_for_timer.report_failure("pipewire format negotiation timed out".into());
            main_loop_for_timeout.quit();
        }
    });
    if let Err(e) = _timer
        .update_timer(
            Some(std::time::Duration::from_millis(
                NEGOTIATE_DEADLINE_MS as u64,
            )),
            None,
        )
        .into_result()
    {
        readiness.report_failure(format!("arm pipewire negotiation deadline failed: {e}"));
        main_loop.quit();
        return;
    }

    // Run until Terminate, the negotiation deadline, or process exit.
    main_loop.run();
    // On exit, drop _timer → _attached → _listener → _stream → main_loop in reverse declaration
    // order, destroying the PipeWire resources on this thread.
}

/// PipeWire setup. Returns `Err(String)` on failure (does not panic).
///
/// If `device_id` is `Some(node.name)`, target that sink through `target.object`; `None` selects
/// the default sink. The caller (`start`) has already checked that the sink exists.
///
/// Returns handles that must stay alive for the entire run:
/// - `MainLoopRc`: drives `run()`/`quit()`
/// - `StreamRc`: capture stream
/// - `StreamListener`: callback registration; dropping it unregisters callbacks
///
/// The caller ([`run_pw_loop`]) attaches the stop channel receiver to the loop. This avoids making
/// `AttachedReceiver` a self-referential struct borrowing the return tuple (which contains `MainLoopRc`).
///
/// `readiness` is stored in the stream's [`UserData`] so `param_changed` can report success once
/// the format is negotiated (readiness must not be reported at connect time, see [`run_pw_loop`]).
#[allow(clippy::type_complexity)]
fn setup_pw(
    device_id: Option<String>,
    sink: RawSink,
    readiness: std::rc::Rc<Readiness>,
    events: BackendEvents,
) -> std::result::Result<
    (
        pw::main_loop::MainLoopRc,
        pw::stream::StreamRc,
        pw::stream::StreamListener<UserData>,
    ),
    String,
> {
    // Call pw::init once per process (Once prevents thread races).
    pw_init_once();

    let main_loop = pw::main_loop::MainLoopRc::new(None)
        .map_err(|e| format!("create pipewire main loop failed: {e}"))?;
    let context = pw::context::ContextRc::new(&main_loop, None)
        .map_err(|e| format!("create pipewire context failed: {e}"))?;
    // Connect to the default PipeWire daemon. Return Err here if it is unavailable.
    let core = context
        .connect_rc(None)
        .map_err(|e| format!("connect to pipewire daemon failed (is PipeWire running?): {e}"))?;

    // Input (capture) stream properties.
    // - media.type=Audio / media.category=Capture: audio capture stream
    // - media.class=Stream/Input/Audio: graph role (input / recording side)
    // - stream.capture.sink=true: capture the sink monitor (system audio output), not a recording device
    // - media.role: hint for autoconnect to the default sink
    let mut props = properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_CLASS => "Stream/Input/Audio",
        *pw::keys::MEDIA_ROLE => "Music",
    };
    // Request capture from the monitor (sink output = system audio).
    props.insert(*pw::keys::STREAM_CAPTURE_SINK, "true");
    // If device_id is set, target that sink through target.object (node.name). Keep autoconnect;
    // when target.object is set, WirePlumber connects to that sink's monitor instead of the
    // default. Do not use stream.connect's target argument (None below), which WirePlumber once
    // ignored; use these props instead. start has already rejected a missing sink, so no check is
    // needed here. pw::keys::TARGET_OBJECT is behind the crate's v0_3_44 feature, so specify the
    // key as a string (other feature-gated keys are handled the same way).
    if let Some(id) = device_id {
        props.insert("target.object", id);
    }

    let stream = pw::stream::StreamRc::new(core, "flexaudio-system-capture", props)
        .map_err(|e| format!("create pipewire capture stream failed: {e}"))?;

    let user_data = UserData {
        format: spa::param::audio::AudioInfoRaw::new(),
        sink,
        scratch: Default::default(),
        events: events.clone(),
        // `param_changed` reports readiness through this handle once the format is negotiated.
        readiness: Some(readiness),
    };

    // Register callbacks. Store the negotiated format in `param_changed` and send buffers
    // dequeued by `process` to RawSink (shared helper).
    let listener = add_capture_listener(&stream, user_data, &main_loop)?;

    // Requested format params: f32 / 48000 / 2 channels. Since rate/channels are explicit,
    // PipeWire automatically inserts audioconvert to convert mismatched graphs to 48 kHz/stereo/f32.
    let values = build_format_pod_bytes()?;
    let pod = Pod::from_bytes(&values)
        .ok_or_else(|| "build audio format pod from bytes failed".to_string())?;
    let mut params = [pod];

    // Connect as input. AUTOCONNECT connects to the sink monitor (the selected sink if
    // target.object is set, otherwise the default sink). MAP_BUFFERS allows direct buffer reads,
    // and RT_PROCESS runs process in real time.
    stream
        .connect(
            spa::utils::Direction::Input,
            None,
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
            &mut params,
        )
        .map_err(|e| format!("connect pipewire capture stream failed: {e}"))?;

    Ok((main_loop, stream, listener))
}

/// Capacity (number of f32 values) to preallocate for the `process` f32 conversion scratch.
/// Native format is 48000 Hz / 2 channels, so one second is 96000 values. Real device process
/// blocks are hundreds to thousands of frames (far less than one second), so this prevents
/// reserve calls in the real-time path.
const PROC_SCRATCH_CAP: usize = (NATIVE_RATE as usize) * (NATIVE_CHANNELS as usize);

// ============================================================================
// Device enumeration (Linux/PipeWire implementation of `devices()`)
// ============================================================================

/// Raw information for one node collected from PipeWire registry global events during enumeration.
///
/// Callbacks write to local `!Send` state, so store owned `String`s here and build [`DeviceInfo`]
/// after the enumeration loop ends.
struct NodeRecord {
    /// `node.name` used as a stable, persistent ID.
    node_name: String,
    /// Display name: prefer `node.description`, otherwise use `node.name`.
    description: String,
    /// `media.class` (`"Audio/Sink"` / `"Audio/Source"`, etc.).
    media_class: String,
    /// Rate (Hz) if `audio.rate` could be read.
    rate: Option<u32>,
    /// Channel count if `audio.channels` could be read.
    channels: Option<u16>,
}

/// Collector shared across the enumeration loop (`!Send`, confined to the loop thread).
#[derive(Default)]
struct EnumState {
    /// Collected Audio/Sink and Audio/Source nodes.
    nodes: Vec<NodeRecord>,
    /// Default sink `node.name` (from `default.audio.sink` metadata).
    default_sink: Option<String>,
    /// Default source `node.name` (from `default.audio.source` metadata).
    default_source: Option<String>,
}

/// Enumerate audio devices (microphones and system output sinks) through PipeWire.
///
/// Wait for one round of registry global events:
/// - `media.class == "Audio/Sink"` → system audio output (target for recording the default sink
///   monitor); `is_loopback = true` / `source_kind = SystemLoopback`.
/// - `media.class == "Audio/Source"` → recording devices such as microphones;
///   `is_loopback = false` / `source_kind = Mic`.
///   These are PipeWire graph identities, not cpal device-name IDs. The flexaudio
///   facade omits these entries and obtains its openable microphone IDs from cpal.
///
/// Map these to [`DeviceInfo`]. `id` is the persistent `node.name`; `name` is `node.description`
/// (or `node.name` if absent). `sample_rate` / `channels` use `audio.rate` / `audio.channels` if
/// available, otherwise default to `48000 / 2`. Set `is_default = true` for the device whose
/// `node.name` matches the `default` metadata (`default.audio.sink` / `default.audio.source`).
///
/// Run a short-lived `MainLoop` and call `quit()` when `core.sync()` reports `done` to signal
/// enumeration completion. Treat a missing PipeWire daemon, connection failure, or registry
/// retrieval failure as an error. Only a completed empty inventory returns `Ok([])`.
pub fn list_devices() -> Result<Vec<DeviceInfo>> {
    enumerate_pw().map_err(|message| {
        Error::Backend(message).with_context(ErrorContext::new(Operation::Enumerate))
    })
}

/// PipeWire registry enumeration implementation. Returns `Err(String)` on failure (does not panic).
///
/// Creates, runs, and destroys `MainLoop`/`Context`/`Core`/`Registry` (all `!Send`) only inside
/// this function. Enumeration uses a short-lived loop, so `list_devices` runs synchronously on
/// the caller's thread without creating a dedicated thread.
fn enumerate_pw() -> std::result::Result<Vec<DeviceInfo>, String> {
    use std::cell::RefCell;
    use std::rc::Rc;

    pw_init_once();

    let main_loop = pw::main_loop::MainLoopRc::new(None)
        .map_err(|e| format!("create pipewire main loop failed: {e}"))?;
    let context = pw::context::ContextRc::new(&main_loop, None)
        .map_err(|e| format!("create pipewire context failed: {e}"))?;
    let core = context
        .connect_rc(None)
        .map_err(|e| format!("connect to pipewire daemon failed (is PipeWire running?): {e}"))?;
    // RegistryRc is cloneable and can be moved into the global callback for binding.
    let registry = core
        .get_registry_rc()
        .map_err(|e| format!("get pipewire registry failed: {e}"))?;

    let failure = Rc::new(std::cell::Cell::new(None::<EnumerationFailure>));
    let state = Rc::new(RefCell::new(EnumState::default()));
    // Keeps default metadata property listeners alive. Push Metadata proxies and listeners bound
    // in the global callback here.
    type MetaKeep = (Box<dyn pw::proxy::ProxyT>, Box<dyn pw::proxy::Listener>);
    let meta_keep: Rc<RefCell<Vec<MetaKeep>>> = Rc::new(RefCell::new(Vec::new()));

    // Registry global listener: collect Audio nodes and default metadata.
    let failure_for_global = failure.clone();
    let state_for_global = state.clone();
    let registry_for_global = registry.clone();
    let meta_keep_for_global = meta_keep.clone();
    let _reg_listener = registry
        .add_listener_local()
        .global(move |global| {
            // A panic crossing FFI is UB, so wrap the callback body in catch_unwind.
            let result = catch_unwind(AssertUnwindSafe(|| {
                let Some(props) = global.props else {
                    failure_for_global.set(Some(EnumerationFailure::Identity));
                    return;
                };
                match global.type_ {
                    pw::types::ObjectType::Node => {
                        // Collect only nodes whose media.class is Audio/Sink or Audio/Source.
                        let media_class = props.get(*pw::keys::MEDIA_CLASS).unwrap_or("");
                        if media_class != "Audio/Sink" && media_class != "Audio/Source" {
                            return;
                        }
                        let node_name = props.get(*pw::keys::NODE_NAME).unwrap_or("");
                        if node_name.is_empty() {
                            failure_for_global.set(Some(EnumerationFailure::Identity));
                            return;
                        }
                        let description = props
                            .get(*pw::keys::NODE_DESCRIPTION)
                            .filter(|s| !s.is_empty())
                            .unwrap_or(node_name);
                        // The pipewire crate feature-gates the audio.rate key constant, so specify
                        // it as a string. It is often absent from registry node props; downstream
                        // code then falls back to the default (48000/2).
                        let rate = props.get("audio.rate").and_then(|s| s.parse::<u32>().ok());
                        let channels = props
                            .get(*pw::keys::AUDIO_CHANNELS)
                            .and_then(|s| s.parse::<u16>().ok());
                        if props.get("audio.rate").is_some() && rate.is_none_or(|rate| rate == 0)
                            || props.get(*pw::keys::AUDIO_CHANNELS).is_some()
                                && channels.is_none_or(|channels| channels == 0)
                        {
                            failure_for_global.set(Some(EnumerationFailure::Format));
                            return;
                        }
                        state_for_global.borrow_mut().nodes.push(NodeRecord {
                            node_name: node_name.to_string(),
                            description: description.to_string(),
                            media_class: media_class.to_string(),
                            rate,
                            channels,
                        });
                    }
                    pw::types::ObjectType::Metadata => {
                        // Bind only "default" metadata, which stores the default sink/source
                        // (the pipewire crate has no "metadata.name" key constant, so use a string).
                        let meta_name = props.get("metadata.name").unwrap_or("");
                        if meta_name != "default" {
                            return;
                        }
                        let metadata: pw::metadata::Metadata =
                            match registry_for_global.bind(global) {
                                Ok(m) => m,
                                Err(_) => {
                                    failure_for_global.set(Some(EnumerationFailure::Metadata));
                                    return;
                                }
                            };
                        let failure_for_meta = failure_for_global.clone();
                        let state_for_meta = state_for_global.clone();
                        let listener = metadata
                            .add_listener_local()
                            .property(move |_subject, key, _type, value| {
                                // Property callbacks also cross FFI, so wrap them in catch_unwind.
                                if catch_unwind(AssertUnwindSafe(|| {
                                    // value is JSON (e.g. {"name":"alsa_output...."}). Extract name.
                                    if let (Some(key), Some(value)) = (key, value) {
                                        if key == "default.audio.sink" {
                                            state_for_meta.borrow_mut().default_sink =
                                                extract_json_name(value);
                                        } else if key == "default.audio.source" {
                                            state_for_meta.borrow_mut().default_source =
                                                extract_json_name(value);
                                        }
                                    }
                                }))
                                .is_err()
                                {
                                    failure_for_meta.set(Some(EnumerationFailure::Callback));
                                }
                                0
                            })
                            .register();
                        meta_keep_for_global
                            .borrow_mut()
                            .push((Box::new(metadata), Box::new(listener)));
                    }
                    _ => {}
                }
            }));
            if result.is_err() {
                failure_for_global.set(Some(EnumerationFailure::Callback));
            }
        })
        .register();

    // Wait for enumeration using a two-stage sync→done barrier.
    //
    // The first done guarantees that the initial registry globals have arrived, but the initial
    // property dump from default metadata bound within those globals (default sink/source values)
    // may still be pending because proxy events arrive separately. After the first done, sync
    // again and quit on the second done. This waits for both global enumeration and default
    // metadata properties. done is guaranteed, so the loop cannot run forever.
    let done = Rc::new(std::cell::Cell::new(false));
    let aborted = Rc::new(std::cell::Cell::new(false));
    let stage = Rc::new(std::cell::Cell::new(0u8));
    let pending1 = core
        .sync(0)
        .map_err(|e| format!("pipewire sync failed: {e}"))?;
    let pending1 = Rc::new(std::cell::Cell::new(pending1.seq()));

    let failure_for_cb = failure.clone();
    let done_for_cb = done.clone();
    let aborted_for_cb = aborted.clone();
    let stage_for_cb = stage.clone();
    let pending1_for_cb = pending1.clone();
    let loop_for_cb = main_loop.clone();
    let failure_for_core = failure.clone();
    let aborted_for_core = aborted.clone();
    let loop_for_core = main_loop.clone();
    let core_weak = core.downgrade();
    let _core_listener = core
        .add_listener_local()
        .done(move |id, seq| {
            if id != pw::core::PW_ID_CORE {
                return;
            }
            let seq = seq.seq();
            match stage_for_cb.get() {
                0 if seq == pending1_for_cb.get() => {
                    // Stage 1 complete → issue a second sync to wait for metadata properties.
                    stage_for_cb.set(1);
                    let Some(core) = core_weak.upgrade() else {
                        // Without the core the second sync cannot be issued, so
                        // enumeration is incomplete. Do not report completion;
                        // quit so the wait loop stops instead of waiting forever.
                        failure_for_cb.set(Some(EnumerationFailure::CoreGone));
                        aborted_for_cb.set(true);
                        loop_for_cb.quit();
                        return;
                    };
                    match core.sync(0) {
                        Ok(p) => pending1_for_cb.set(p.seq()),
                        Err(_) => {
                            // The second sync was refused: enumeration did not
                            // complete, so `done` stays unset. Quit the wait loop.
                            failure_for_cb.set(Some(EnumerationFailure::SecondSync));
                            aborted_for_cb.set(true);
                            loop_for_cb.quit();
                        }
                    }
                }
                1 if seq == pending1_for_cb.get() => {
                    // Stage 2 complete → enumeration finished.
                    done_for_cb.set(true);
                    loop_for_cb.quit();
                }
                _ => {}
            }
        })
        .register();

    let _error_listener = core
        .add_listener_local()
        .error(move |_id, _seq, _code, _message| {
            failure_for_core.set(Some(EnumerationFailure::CoreError));
            aborted_for_core.set(true);
            loop_for_core.quit();
        })
        .register();

    // Wait for done (both sync round trips complete). `run()` blocks until `quit()` and would
    // hang forever when no event ever arrives, so iterate with a finite timeout instead: each
    // iteration returns within its timeout, and the loop stops once the deadline passes even if
    // `done` never arrives. `iterate` also dispatches the core `done` callback, which sets
    // `done` and calls `quit()` on the normal path. It is best-effort and must not panic.
    let deadline = std::time::Instant::now();
    while !done.get() && !aborted.get() {
        let elapsed_ms = deadline.elapsed().as_millis();
        if elapsed_ms >= ENUMERATE_DEADLINE_MS {
            // Deadline exceeded; stop waiting for a completion that may never arrive.
            break;
        }
        let remaining =
            std::time::Duration::from_millis((ENUMERATE_DEADLINE_MS - elapsed_ms) as u64);
        if main_loop
            .loop_()
            .iterate(pw::loop_::Timeout::Finite(remaining))
            < 0
        {
            failure.set(Some(EnumerationFailure::Iterate));
            aborted.set(true);
        }
    }

    if let Some(failure) = failure.get() {
        return Err(failure.message().into());
    }
    if !done.get() || aborted.get() {
        return Err(EnumerationFailure::Deadline.message().into());
    }

    // Build DeviceInfo values from the collected raw nodes.
    let state = state.borrow();
    let mut out = Vec::with_capacity(state.nodes.len());
    for n in &state.nodes {
        let is_loopback = n.media_class == "Audio/Sink";
        let source_kind = if is_loopback {
            SourceKind::SystemLoopback
        } else {
            SourceKind::Mic
        };
        let is_default = if is_loopback {
            state.default_sink.as_deref() == Some(n.node_name.as_str())
        } else {
            state.default_source.as_deref() == Some(n.node_name.as_str())
        };
        out.push(DeviceInfo {
            id: n.node_name.clone(),
            name: n.description.clone(),
            source_kind,
            // If unavailable, default to the requested native format (48000/2).
            sample_rate: n.rate.unwrap_or(NATIVE_RATE),
            channels: n.channels.unwrap_or(NATIVE_CHANNELS),
            is_loopback,
            is_default,
        });
    }
    Ok(out)
}

/// Extract `name` from PipeWire `default.audio.{sink,source}` metadata (JSON `{"name":"..."}`).
/// Uses a lightweight parser to avoid adding a JSON dependency. Returns `None` for unexpected values.
fn extract_json_name(value: &str) -> Option<String> {
    // Get the first string literal after the `"name"` key, skipping whitespace and the colon.
    let after_key = value.split("\"name\"").nth(1)?;
    let after_colon = after_key.split(':').nth(1)?;
    // Extract text between the first and next `"`.
    let start = after_colon.find('"')? + 1;
    let rest = &after_colon[start..];
    let end = rest.find('"')?;
    let name = &rest[..end];
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

// ============================================================================
// Device hot-plug watcher (Linux/PipeWire implementation of `watch_devices()`)
// ============================================================================

/// Watch the PipeWire registry continuously and publish device additions/removals (hot-plug) as
/// [`DeviceEvent`] values.
///
/// # Difference from [`PwSystemBackend`] / `enumerate_pw`
///
/// Like [`PwSystemBackend`], this owns one dedicated thread, but behaves differently:
/// - Persistent instead of short-lived: `enumerate_pw` calls `quit()` on `core.sync` done and
///   exits, while this watcher keeps running and receives registry `global` / `global_remove`
///   events until [`stop`](Self::stop).
/// - No RawSink: it does not record audio, only watches registry global/global_remove events.
///
/// `MainLoop` / `Context` / `Core` / `Registry` are `!Send`, so confine them to the dedicated
/// `flexaudio-pw-watch` thread. The backend stores only `Send` values (event queue
/// [`Arc<Mutex<VecDeque>>`], stop flag, stop [`pipewire::channel::Sender`], and [`JoinHandle`]).
///
/// # Published events
/// - [`DeviceEvent::Added`]: Audio/Sink|Source nodes that appear after the initial scan. Nodes
///   already present during the scan are registered but not published.
/// - [`DeviceEvent::Removed`]: Nodes removed while watching (id = `node.name`).
/// - [`DeviceEvent::DefaultChanged`]: Default sink/source changes (from default metadata).
///
/// # PipeWire unavailable
/// If the PipeWire daemon is unavailable or connection fails, [`start`](Self::start) returns
/// [`Error::Backend`] without panicking. The facade degrades this to a no-op watcher (there is
/// nothing to publish if there are no device changes). An available but empty PipeWire session
/// works normally.
///
/// ```no_run
/// use flexaudio_os_linux::PwDeviceWatcher;
///
/// // Returns Err if PipeWire is unavailable.
/// if let Ok(mut watcher) = PwDeviceWatcher::start() {
///     while let Some(ev) = watcher.poll_event() {
///         println!("device event: {ev:?}");
///     }
///     watcher.stop();
/// }
/// ```
pub struct PwDeviceWatcher {
    /// Bounded event queue with a sticky invalidation outside the delta queue. `Send`.
    /// Watcher callbacks push here; [`poll_event`](Self::poll_event) pops events.
    events: WatchEventQueue,
    /// Watching flag (guards against duplicate starts and is used by drop). `Send`.
    running: Arc<AtomicBool>,
    /// Sender for stopping the watcher thread. Set to `Some` by [`start`](Self::start).
    /// Uses the same [`Terminate`] as [`PwSystemBackend`].
    stop_tx: Option<pw::channel::Sender<Terminate>>,
    /// Handle for the watcher thread. Set to `Some` by [`start`](Self::start).
    handle: Option<JoinHandle<()>>,
}

impl PwDeviceWatcher {
    /// Start watching. On a dedicated thread, create `MainLoop` + `Context` + `Core` + `Registry`,
    /// register registry `global` / `global_remove` listeners, and complete the initial scan.
    /// Return setup status synchronously. On success, the thread keeps running and pushes hot-plug
    /// events into the event queue.
    ///
    /// Return [`Error::Backend`] if PipeWire is unavailable or connection fails (without panicking).
    pub fn start() -> Result<Self> {
        // Create the event queue before start and clone it into setup.
        let events: WatchEventQueue = Arc::new(Mutex::new(WatchEvents::default()));

        // Stop channel for the watcher thread (receiver is attached to the loop).
        let (stop_tx, stop_rx) = pw::channel::channel::<Terminate>();
        // Channel to synchronously return setup status to start() (Ok once registry listeners are
        // registered and the initial scan completes).
        let (ready_tx, ready_rx) = mpsc::channel::<std::result::Result<(), String>>();

        let running = Arc::new(AtomicBool::new(true));

        let events_for_thread = events.clone();
        let handle = thread::Builder::new()
            .name("flexaudio-pw-watch".into())
            .spawn(move || {
                run_watch_loop(events_for_thread, stop_rx, &ready_tx);
            })
            .map_err(|e| Error::Backend(format!("spawn pipewire watch thread: {e}")))?;

        // Wait for setup. Treat thread exit without sending ready as failure.
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                events,
                running,
                stop_tx: Some(stop_tx),
                handle: Some(handle),
            }),
            Ok(Err(msg)) => {
                // Setup failed (PipeWire unavailable, connection/registry failure, etc.). The
                // thread has already returned, so join it for cleanup.
                running.store(false, Ordering::SeqCst);
                Err(rollback_worker(
                    Error::Backend(msg).with_context(ErrorContext::new(Operation::Start)),
                    handle,
                ))
            }
            Err(_) => {
                // The thread exited without sending ready (e.g. an unexpected panic).
                running.store(false, Ordering::SeqCst);
                Err(rollback_worker(
                    Error::Backend(
                        "pipewire watch thread terminated before signaling readiness".into(),
                    )
                    .with_context(ErrorContext::new(Operation::Start)),
                    handle,
                ))
            }
        }
    }

    /// Pop the next hot-plug event from the queue, or return `None` if empty.
    /// Poisoned queues are salvaged and emit rescan before any retained deltas.
    pub fn poll_event(&mut self) -> Option<DeviceEvent> {
        lock_events(&self.events).poll()
    }

    /// Stop watching (safe on duplicate stop or stop before start).
    ///
    /// As with [`PwSystemBackend::stop`], sending `Terminate` invokes the receiver callback
    /// attached to the loop, which calls `main_loop.quit()` on the watcher thread and exits
    /// `run()`. Wait for cleanup to finish with `join()`, retaining pending events for polling.
    pub fn stop(&mut self) {
        // Safe on duplicate stop or stop before start.
        if !self.running.swap(false, Ordering::SeqCst) {
            // Already stopped or not started. Join any leftover thread just in case.
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
            self.stop_tx = None;
            return;
        }

        // Notify the watcher thread to stop (the receiver callback calls loop.quit()).
        // Ignore failure (the receiver is gone because the thread has exited).
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(Terminate);
        }

        // Wait for run() to exit and the thread to finish. On exit, Registry→Core→Context→MainLoop
        // are dropped in order, all on the watcher thread.
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for PwDeviceWatcher {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Local state shared throughout the watcher loop (`!Send`, confined to the thread).
#[derive(Default)]
struct WatchState {
    /// Reverse map of registry global id → [`DeviceInfo`] for events.
    /// `global_remove` provides only the numeric id, so use this to recover `node.name`.
    by_global_id: std::collections::HashMap<u32, DeviceInfo>,
    /// Whether the initial scan (the first two-stage sync→done barrier) has completed.
    /// While `false`, register incoming globals but do not publish `Added`.
    initial_scan_done: bool,
    /// Default sink `node.name` (from `default.audio.sink` metadata).
    /// Publish changes after the initial scan as [`DeviceEvent::DefaultChanged`].
    default_sink: Option<String>,
    /// Default source `node.name` (from `default.audio.source` metadata).
    default_source: Option<String>,
}

/// PipeWire watcher loop thread body.
///
/// Creates, runs, and destroys `MainLoop`/`Context`/`Core`/`Registry` (all `!Send`) only inside
/// this function. Reports setup success or failure to the caller through `ready_tx`, then runs
/// `main_loop.run()` until [`Terminate`] on success.
fn run_watch_loop(
    events: WatchEventQueue,
    stop_rx: pw::channel::Receiver<Terminate>,
    ready_tx: &mpsc::Sender<std::result::Result<(), String>>,
) {
    // Setup (connection, registry listener registration, and initial scan) is in a separate
    // function. Keep its return values alive for the whole run (dropping them stops watching).
    let (main_loop, _core, _registry, _listeners) = match setup_watch(events) {
        Ok(t) => t,
        Err(msg) => {
            // Report setup failure and exit (without panicking).
            let _ = ready_tx.send(Err(msg));
            return;
        }
    };

    // Attach the stop channel receiver to the loop. On Terminate, call quit().
    // quit() runs inside a loop callback, i.e. on this thread.
    let main_loop_for_quit = main_loop.clone();
    let _attached = stop_rx.attach(main_loop.loop_(), move |_terminate| {
        main_loop_for_quit.quit();
    });

    // Report setup success. run() now blocks and continues publishing hot-plug events.
    if ready_tx.send(Ok(())).is_err() {
        // The caller is gone (e.g. start was dropped). Do not run.
        return;
    }

    // Run until Terminate or process exit. Unlike enumerate_pw, done does not call quit, so this
    // loop continues indefinitely.
    main_loop.run();
    // On exit, drop _attached → _listeners → _registry → _core → main_loop in reverse
    // declaration order, destroying the PipeWire resources on this thread.
}

/// Resources held by the watcher for the entire run. Dropping them stops watching, so keep them
/// on the `run_watch_loop` stack.
///
/// - `MainLoopRc`: drives `run()`/`quit()`.
/// - `CoreRc`: parent for registry / sync (downgraded for use in the done callback).
/// - `RegistryRc`: registry proxy.
/// - Listeners: registry listener, core(done) listener, and bound default metadata proxies and
///   listeners. Keep them type-erased in a Box because dropping them unregisters callbacks.
#[allow(clippy::type_complexity)]
type WatchKeep = (
    pw::main_loop::MainLoopRc,
    pw::core::CoreRc,
    pw::registry::RegistryRc,
    WatchListeners,
);

/// One bound default metadata proxy/listener pair (dropping it unregisters the callback).
/// Same type as `enumerate_pw`'s local `MetaKeep`.
type MetaKeepEntry = (Box<dyn pw::proxy::ProxyT>, Box<dyn pw::proxy::Listener>);

/// Store for `MetaKeepEntry` values (shared by `Rc` on the watcher thread; `!Send`).
type MetaKeepStore = std::rc::Rc<std::cell::RefCell<Vec<MetaKeepEntry>>>;

/// Listeners kept alive during watching (dropping them unregisters callbacks).
struct WatchListeners {
    /// Registry global/global_remove listeners.
    _registry_listener: pw::registry::Listener,
    /// Core done listener (detects completion of the initial scan's two-stage barrier).
    _core_listener: pw::core::Listener,
    /// Store for default metadata proxies/listeners bound in the global callback (same type as
    /// [`enumerate_pw`], shared by `Rc` and confined to the watcher thread).
    _meta_keep: MetaKeepStore,
}

/// PipeWire watcher setup. Returns `Err(String)` on failure (does not panic).
///
/// Reuses [`enumerate_pw`]'s registry global extraction and two-stage sync→done barrier, but
/// `done` only sets the initial-scan-complete flag instead of calling `quit()`. It then continues
/// receiving global/global_remove events indefinitely.
#[allow(clippy::type_complexity)]
fn setup_watch(events: WatchEventQueue) -> std::result::Result<WatchKeep, String> {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    pw_init_once();

    let main_loop = pw::main_loop::MainLoopRc::new(None)
        .map_err(|e| format!("create pipewire main loop failed: {e}"))?;
    let context = pw::context::ContextRc::new(&main_loop, None)
        .map_err(|e| format!("create pipewire context failed: {e}"))?;
    let core = context
        .connect_rc(None)
        .map_err(|e| format!("connect to pipewire daemon failed (is PipeWire running?): {e}"))?;
    let registry = core
        .get_registry_rc()
        .map_err(|e| format!("get pipewire registry failed: {e}"))?;

    // Local watcher-thread state (!Send), shared with each closure through Rc.
    let query_failure = Rc::new(Cell::new(None::<EnumerationFailure>));
    let state = Rc::new(RefCell::new(WatchState::default()));
    // Clone and move the event queue (events: Arc<Mutex<VecDeque>>) into each closure.

    // Store that keeps default metadata property listeners alive (same type as enumerate_pw:
    // MetaKeepStore = Rc<RefCell<Vec<MetaKeepEntry>>>).
    let meta_keep: MetaKeepStore = Rc::new(RefCell::new(Vec::new()));

    // Registry global / global_remove listeners.
    let failure_for_global = query_failure.clone();
    let state_for_global = state.clone();
    let events_for_global = events.clone();
    let registry_for_global = registry.clone();
    let meta_keep_for_global = meta_keep.clone();
    let state_for_remove = state.clone();
    let events_for_remove = events.clone();
    let _registry_listener = registry
        .add_listener_local()
        .global(move |global| {
            // A panic crossing FFI is UB, so wrap the callback body in catch_unwind.
            let _ = catch_unwind(AssertUnwindSafe(|| {
                let Some(props) = global.props else {
                    return;
                };
                match global.type_ {
                    pw::types::ObjectType::Node => {
                        // Same extraction logic as enumerate_pw.
                        // Collect only nodes whose media.class is Audio/Sink or Audio/Source.
                        let media_class = props.get(*pw::keys::MEDIA_CLASS).unwrap_or("");
                        if media_class != "Audio/Sink" && media_class != "Audio/Source" {
                            return;
                        }
                        let node_name = props.get(*pw::keys::NODE_NAME).unwrap_or("");
                        if node_name.is_empty() {
                            failure_for_global.set(Some(EnumerationFailure::Identity));
                            lock_events(&events_for_global).invalidate();
                            return;
                        }
                        let description = props
                            .get(*pw::keys::NODE_DESCRIPTION)
                            .filter(|s| !s.is_empty())
                            .unwrap_or(node_name);
                        let rate = props.get("audio.rate").and_then(|s| s.parse::<u32>().ok());
                        let channels = props
                            .get(*pw::keys::AUDIO_CHANNELS)
                            .and_then(|s| s.parse::<u16>().ok());

                        let is_loopback = media_class == "Audio/Sink";
                        let source_kind = if is_loopback {
                            SourceKind::SystemLoopback
                        } else {
                            SourceKind::Mic
                        };
                        // Compare against known default metadata values to set is_default. Metadata
                        // may not have arrived during the initial scan, in which case it is false
                        // and a later DefaultChanged event corrects it.
                        let mut st = state_for_global.borrow_mut();
                        let is_default = if is_loopback {
                            st.default_sink.as_deref() == Some(node_name)
                        } else {
                            st.default_source.as_deref() == Some(node_name)
                        };

                        let info = DeviceInfo {
                            id: node_name.to_string(),
                            name: description.to_string(),
                            source_kind,
                            // If unavailable, default to the requested native format (48000/2), as in enumerate_pw.
                            sample_rate: rate.unwrap_or(NATIVE_RATE),
                            channels: channels.unwrap_or(NATIVE_CHANNELS),
                            is_loopback,
                            is_default,
                        };
                        st.by_global_id.insert(global.id, info.clone());
                        let initial_scan_done = st.initial_scan_done;
                        drop(st);

                        // During the initial scan, only register nodes. Publish Added only for later arrivals.
                        if initial_scan_done {
                            enqueue_event(&events_for_global, DeviceEvent::Added(info));
                        }
                    }
                    pw::types::ObjectType::Metadata => {
                        // Bind only "default" metadata, which stores the default sink/source (as in enumerate_pw).
                        let meta_name = props.get("metadata.name").unwrap_or("");
                        if meta_name != "default" {
                            return;
                        }
                        let metadata: pw::metadata::Metadata =
                            match registry_for_global.bind(global) {
                                Ok(m) => m,
                                Err(_) => {
                                    failure_for_global.set(Some(EnumerationFailure::Metadata));
                                    lock_events(&events_for_global).invalidate();
                                    return;
                                }
                            };
                        let state_for_meta = state_for_global.clone();
                        let events_for_meta = events_for_global.clone();
                        let listener = metadata
                            .add_listener_local()
                            .property(move |_subject, key, _type, value| {
                                // Property callbacks also cross FFI, so wrap them in catch_unwind.
                                catch_unwind(AssertUnwindSafe(|| {
                                    // A null key clears every default property for this subject.
                                    for (property, kind) in [
                                        (
                                            "default.audio.sink",
                                            flexaudio_core::DefaultDeviceKind::SystemAudio,
                                        ),
                                        (
                                            "default.audio.source",
                                            flexaudio_core::DefaultDeviceKind::Microphone,
                                        ),
                                    ] {
                                        if key.is_some_and(|key| key != property) {
                                            continue;
                                        }
                                        let next = key.and(value).and_then(extract_json_name);
                                        let mut state = state_for_meta.borrow_mut();
                                        let publish = state.initial_scan_done;
                                        let previous = if property == "default.audio.sink" {
                                            &mut state.default_sink
                                        } else {
                                            &mut state.default_source
                                        };
                                        let event = transition_default(previous, next, kind);
                                        drop(state);
                                        if publish {
                                            if let Some(event) = event {
                                                enqueue_event(&events_for_meta, event);
                                            }
                                        }
                                    }
                                }))
                                .ok();
                                0
                            })
                            .register();
                        meta_keep_for_global
                            .borrow_mut()
                            .push((Box::new(metadata), Box::new(listener)));
                    }
                    _ => {}
                }
            }));
        })
        .global_remove(move |id| {
            // A panic crossing FFI is UB, so wrap the callback body in catch_unwind.
            let _ = catch_unwind(AssertUnwindSafe(|| {
                // Publish Removed only for nodes in the reverse map. Ignore ids absent from the
                // map (non-node globals such as Metadata may also be removed).
                let removed = state_for_remove.borrow_mut().by_global_id.remove(&id);
                if let Some(info) = removed {
                    enqueue_event(&events_for_remove, DeviceEvent::Removed { id: info.id });
                }
            }));
        })
        .register();

    // Detect initial scan completion with the same two-stage sync→done barrier as enumerate_pw.
    // Unlike enumerate_pw, done only sets initial_scan_done and does not call quit(). After the
    // second done, initial globals and default metadata's initial property dump are available, so
    // later global/global_remove/property changes can be published as user-driven device or
    // default changes.
    let aborted = Rc::new(Cell::new(false));
    let stage = Rc::new(Cell::new(0u8));
    let pending = core
        .sync(0)
        .map_err(|e| format!("pipewire sync failed: {e}"))?;
    let pending = Rc::new(Cell::new(pending.seq()));

    let aborted_for_cb = aborted.clone();
    let stage_for_cb = stage.clone();
    let pending_for_cb = pending.clone();
    let state_for_done = state.clone();
    let loop_for_done = main_loop.clone();
    let core_weak = core.downgrade();
    let _core_listener = core
        .add_listener_local()
        .done(move |id, seq| {
            if id != pw::core::PW_ID_CORE {
                return;
            }
            let seq = seq.seq();
            match stage_for_cb.get() {
                0 if seq == pending_for_cb.get() => {
                    // Stage 1 complete → issue a second sync to wait for metadata properties.
                    stage_for_cb.set(1);
                    if let Some(core) = core_weak.upgrade() {
                        match core.sync(0) {
                            Ok(p) => pending_for_cb.set(p.seq()),
                            Err(_) => {
                                aborted_for_cb.set(true);
                                loop_for_done.quit();
                            }
                        }
                    } else {
                        aborted_for_cb.set(true);
                        loop_for_done.quit();
                    }
                }
                1 if seq == pending_for_cb.get() => {
                    // Stage 2 complete → initial scan finished. quit() is called here only to
                    // exit the initial-scan run() (the while loop below). run_watch_loop handles
                    // persistent watching. stage is now 2, so later done events match no arm and
                    // never call quit() again.
                    stage_for_cb.set(2);
                    state_for_done.borrow_mut().initial_scan_done = true;
                    loop_for_done.quit();
                }
                _ => {}
            }
        })
        .register();

    // Run until the initial scan completes (both sync round trips). done sets initial_scan_done
    // and calls quit(), so this exits as in enumerate_pw. Return only after the initial globals
    // and default metadata property dump are available. run_watch_loop handles persistent
    // watching. Once stage reaches 2, done no longer calls quit(), so that run() continues.
    let deadline = std::time::Instant::now();
    while !state.borrow().initial_scan_done && !aborted.get() {
        let elapsed = deadline.elapsed().as_millis();
        if elapsed >= ENUMERATE_DEADLINE_MS {
            return Err("pipewire watch initial scan timed out".into());
        }
        let remaining = std::time::Duration::from_millis((ENUMERATE_DEADLINE_MS - elapsed) as u64);
        if main_loop
            .loop_()
            .iterate(pw::loop_::Timeout::Finite(remaining))
            < 0
        {
            aborted.set(true);
        }
    }
    if aborted.get() {
        return Err("pipewire watch initial scan aborted".into());
    }
    if let Some(failure) = query_failure.get() {
        return Err(failure.message().into());
    }

    Ok((
        main_loop,
        core,
        registry,
        WatchListeners {
            _registry_listener,
            _core_listener,
            _meta_keep: meta_keep,
        },
    ))
}

/// Push one delta to the bounded queue, salvaging poison and preserving sticky invalidation.
fn enqueue_event(events: &WatchEventQueue, ev: DeviceEvent) {
    lock_events(events).push(ev, MAX_WATCH_EVENTS);
}

#[cfg(test)]
mod tests {
    use super::*;
    use flexaudio_core::raw_ring::raw_ring;

    /// Verify `PwSystemBackend: Send` as required by the [`CaptureBackend`] contract (proves
    /// PipeWire's `!Send` values are confined to the dedicated thread). Passing compilation is enough.
    #[test]
    fn backend_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<PwSystemBackend>();
    }

    /// Verify that the native format is fixed at (48000, 2) immediately after construction.
    #[test]
    fn native_format_is_48k_stereo() {
        let be = PwSystemBackend::new(false, None);
        assert_eq!(be.native_format(), (NATIVE_RATE, NATIVE_CHANNELS));
        assert_eq!(be.native_format(), (48_000, 2));
        assert!(!be.exclude_self());
    }

    /// Stopping before start and stopping twice are safe (do not panic).
    #[test]
    fn stop_without_start_is_safe() {
        let mut be = PwSystemBackend::new(false, None);
        be.stop();
        be.stop();
    }

    /// `exclude_self=true` for system capture reuses the process Exclude mechanism.
    /// `start` does not return `Unsupported`: it may return [`Error::Backend`] in a headless
    /// environment without PipeWire, or `Ok(())` (waits successfully) when a PipeWire session is
    /// available. Verify no panic in either case and, on Ok, complete a start-to-stop cycle.
    #[test]
    fn system_exclude_self_is_graceful() {
        let (prod, _cons) = raw_ring(1 << 16);
        let sink = RawSink::new(prod, NATIVE_RATE, NATIVE_CHANNELS);
        let mut be = PwSystemBackend::new(true, None);
        assert!(be.exclude_self());
        match be.start(sink) {
            Ok(()) => {
                // PipeWire session available. Delegate to Exclude fan-in for all other processes;
                // waiting succeeds even if targets have not appeared. Complete the stop cycle.
                be.stop();
            }
            Err(error) if error.kind() == flexaudio_core::ErrorKind::Backend => {
                // PipeWire unavailable/registry failure is expected. The key is no panic.
            }
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }

    /// Verify that `extract_json_name` extracts name from PipeWire metadata (JSON).
    #[test]
    fn extract_json_name_parses_default_metadata_value() {
        assert_eq!(
            extract_json_name(r#"{"name":"alsa_output.pci-0000_00_1f.3.analog-stereo"}"#)
                .as_deref(),
            Some("alsa_output.pci-0000_00_1f.3.analog-stereo")
        );
        // Extract name even when whitespace is present.
        assert_eq!(
            extract_json_name(r#"{ "name" : "foo.bar" }"#).as_deref(),
            Some("foo.bar")
        );
        // Return None if the name key is missing, empty, or invalid.
        assert_eq!(extract_json_name(r#"{"other":"x"}"#), None);
        assert_eq!(extract_json_name(r#"{"name":""}"#), None);
        assert_eq!(extract_json_name("not json"), None);
    }

    /// Discovery returns a complete inventory or Backend failure. If devices are
    /// returned, verify Sink→SystemLoopback / Source→Mic consistency and nonempty ids (=node.name).
    #[test]
    fn list_devices_is_complete_or_backend_error() {
        let devices = match list_devices() {
            Ok(devices) => devices,
            Err(error) => {
                assert_eq!(error.kind(), flexaudio_core::ErrorKind::Backend);
                assert!(
                    matches!(error, Error::Context { context, .. } if context.operation() == Operation::Enumerate)
                );
                return;
            }
        };
        for d in &devices {
            assert!(!d.id.is_empty(), "id (=node.name) is nonempty");
            match d.source_kind {
                SourceKind::SystemLoopback => assert!(d.is_loopback, "Sink is loopback"),
                SourceKind::Mic => assert!(!d.is_loopback, "Source is not loopback"),
                other => panic!("unexpected source_kind: {other:?}"),
            }
            assert!(d.sample_rate > 0);
            assert!(d.channels > 0);
        }
        // There is at most one default sink and one default source.
        let default_loopback = devices
            .iter()
            .filter(|d| d.is_default && d.is_loopback)
            .count();
        let default_mic = devices
            .iter()
            .filter(|d| d.is_default && !d.is_loopback)
            .count();
        assert!(default_loopback <= 1);
        assert!(default_mic <= 1);
    }

    /// Smoke test: `start` may return `Err(Error::Backend)` in a headless environment without
    /// PipeWire or a sink, but must not panic. Both Ok (PipeWire and active sink available) and
    /// Err(Backend) are allowed.
    ///
    /// On a desktop/laptop running PipeWire, this returns Ok and can complete through `stop()`.
    /// See the `#[ignore]` test below for actual audio end-to-end verification.
    #[test]
    fn start_is_graceful_without_pipewire() {
        let (prod, _cons) = raw_ring(1 << 16);
        let sink = RawSink::new(prod, NATIVE_RATE, NATIVE_CHANNELS);
        let mut be = PwSystemBackend::new(false, None);
        match be.start(sink) {
            Ok(()) => {
                // PipeWire and an active sink are available. Complete the stop cycle.
                be.stop();
            }
            Err(error) if error.kind() == flexaudio_core::ErrorKind::Backend => {
                // PipeWire unavailable/no sink is expected. The key is no panic.
            }
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }

    /// Start with a `device_id` for a nonexistent sink. If PipeWire is running, the sink is not
    /// listed and [`Error::DeviceNotFound`] is returned. Without PipeWire, enumerate_pw treats
    /// the failure as an error with Enumerate context. Verify
    /// no panic and no Ok in either case.
    #[test]
    fn start_with_unknown_device_id_is_not_found_or_backend() {
        let (prod, _cons) = raw_ring(1 << 16);
        let sink = RawSink::new(prod, NATIVE_RATE, NATIVE_CHANNELS);
        let mut be = PwSystemBackend::new(false, Some("flexaudio-no-such-sink-zzz".to_string()));
        match be.start(sink) {
            Err(Error::DeviceNotFound) => {}
            Err(error) if error.kind() == flexaudio_core::ErrorKind::Backend => {}
            Ok(()) => {
                be.stop();
                panic!("start should not succeed for an unknown device_id");
            }
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }

    /// Real capture end-to-end (only on a desktop/laptop running PipeWire).
    ///
    /// How to run (on a laptop or similar with PipeWire and audio playing):
    /// ```text
    /// cargo test -p flexaudio-os-linux -- --ignored capture_smoke
    /// ```
    /// Capture the default sink monitor for a while and expect samples to arrive (observed via
    /// overflow or pop). Ignored in headless environments/CI because they have neither audio
    /// sources nor PipeWire.
    #[test]
    #[ignore = "requires a running PipeWire session with audio playing (desktop/laptop)"]
    fn capture_smoke() {
        use std::time::Duration;
        let (prod, mut cons) = raw_ring(1 << 18);
        let sink = RawSink::new(prod, NATIVE_RATE, NATIVE_CHANNELS);
        let mut be = PwSystemBackend::new(false, None);
        be.start(sink)
            .expect("start should succeed on a PipeWire desktop");
        // Wait briefly for capture to run.
        thread::sleep(Duration::from_millis(500));
        be.stop();
        // Some samples arrived (even a silent sink produces 0.0 samples).
        let mut out = vec![0.0f32; 1920];
        let got = cons.pop_slice(&mut out);
        assert!(
            got > 0,
            "expected captured samples from the default sink monitor"
        );
    }

    // ------------------------------------------------------------------------
    // PwProcessBackend (process output loopback)
    // ------------------------------------------------------------------------

    /// Verify `PwProcessBackend: Send` as required by the [`CaptureBackend`] contract (proves
    /// PipeWire's `!Send` values are confined to the dedicated thread). Passing compilation is enough.
    #[test]
    fn process_backend_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<PwProcessBackend>();
    }

    /// Verify that the native format is fixed at (48000, 2) immediately after construction.
    /// Also verify that PID and mode are retained.
    #[test]
    fn process_native_format_is_48k_stereo() {
        let be = PwProcessBackend::new(4242, ProcessMode::Exclude);
        assert_eq!(be.native_format(), (NATIVE_RATE, NATIVE_CHANNELS));
        assert_eq!(be.native_format(), (48_000, 2));
        // Constructor arguments are retained.
        assert_eq!(be.target_pid(), 4242);
        assert_eq!(be.mode(), ProcessMode::Exclude);
        let be2 = PwProcessBackend::new(1, ProcessMode::Include);
        assert_eq!(be2.mode(), ProcessMode::Include);
    }

    /// Stopping before start and stopping twice are safe (do not panic).
    #[test]
    fn process_stop_without_start_is_safe() {
        let mut be = PwProcessBackend::new(1234, ProcessMode::Include);
        be.stop();
        be.stop();
    }

    /// Process [`ProcessMode::Exclude`] captures all PIDs except the target through fan-in.
    /// `start` does not return `Unsupported`: it may return [`Error::Backend`] on a headless
    /// machine without PipeWire, or `Ok(())` (waits successfully) when a PipeWire session exists.
    /// Verify no panic in either case and, on Ok, complete duplicate start (no-op), stop, and
    /// duplicate stop.
    #[test]
    fn process_exclude_mode_is_graceful() {
        let (prod, _cons) = raw_ring(1 << 16);
        let sink = RawSink::new(prod, NATIVE_RATE, NATIVE_CHANNELS);
        let mut be = PwProcessBackend::new(u32::MAX, ProcessMode::Exclude);
        match be.start(sink) {
            Ok(()) => {
                // PipeWire session available. Delegate to Exclude fan-in for all PIDs except the
                // target; waiting succeeds. Duplicate start is safe (no-op returning Ok).
                let (prod2, _cons2) = raw_ring(1 << 16);
                let sink2 = RawSink::new(prod2, NATIVE_RATE, NATIVE_CHANNELS);
                assert!(be.start(sink2).is_ok());
                // Complete the stop cycle (safe to destroy even before linking).
                be.stop();
                // Duplicate stop is also safe.
                be.stop();
            }
            Err(error) if error.kind() == flexaudio_core::ErrorKind::Backend => {
                // PipeWire unavailable/registry failure is expected. The key is no panic.
            }
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }

    /// Native PID resolution remains independent of registry arrival order.
    #[test]
    fn resolve_node_pid_via_client_table() {
        use std::collections::HashMap;

        let node = NodeEntry {
            owning_client_id: Some(60),
            ..NodeEntry::default()
        };
        let mut clients = HashMap::new();
        assert_eq!(resolve_node_pid(&node, &clients), None);
        clients.insert(60, ClientEntry::from_props(None, Some("13394"), None));
        assert_eq!(resolve_node_pid(&node, &clients), Some(13394));

        let orphan = NodeEntry::default();
        assert_eq!(resolve_node_pid(&orphan, &clients), None);
        let direct = NodeEntry {
            app_pid: Some(424242),
            ..NodeEntry::default()
        };
        assert_eq!(resolve_node_pid(&direct, &HashMap::new()), Some(424242));

        let other = NodeEntry {
            owning_client_id: Some(61),
            ..NodeEntry::default()
        };
        assert_eq!(resolve_node_pid(&other, &clients), None);
        clients.insert(61, ClientEntry::from_props(None, Some("555"), None));
        assert_eq!(resolve_node_pid(&other, &clients), Some(555));
        assert_eq!(resolve_node_pid(&node, &clients), Some(13394));
    }

    /// libpulse clients reach PipeWire through pipewire-pulse, so their Client's
    /// `pipewire.sec.pid` is pipewire-pulse's pid; the app's own pid is only in
    /// `application.process.id`. The registry `global` event never carries that
    /// key for Client or Node (confirmed live, 2026-09-22); it arrives only via
    /// a bound object's `info` props. Symptom before the bind/info fix landed:
    /// Moss/Chrome/Zoom all resolved to pid 3020 (pipewire-pulse) instead of
    /// their own.
    #[test]
    fn pid_from_props_prefers_application_process_id() {
        // libpulse client: app pid wins over the daemon's socket-peer pid.
        assert_eq!(pid_from_props(Some("28551"), Some("3020")), Some(28551));
        // native client without application.process.id: sec.pid is the answer.
        assert_eq!(pid_from_props(None, Some("13394")), Some(13394));
        // node props carry no sec.pid at all.
        assert_eq!(pid_from_props(Some("42"), None), Some(42));
        // garbage / zero app pid falls back to sec.pid; nothing usable → None.
        assert_eq!(pid_from_props(Some("nope"), Some("7")), Some(7));
        assert_eq!(pid_from_props(Some("0"), Some("7")), Some(7));
        assert_eq!(pid_from_props(None, None), None);
    }

    /// Pulse provenance can come from either the node or its owning Client.
    #[test]
    fn node_pid_decision_table() {
        use std::collections::{HashMap, HashSet};

        // The measured Chromium/Electron fixture: app PID 1028793, proxy PID 1584.
        // Columns: node API, client API, info seen, props seen, bound PID, app PID,
        // resolved PID, Exclude decidable.
        let cases = [
            (
                None,
                Some("pipewire-pulse"),
                false,
                false,
                false,
                None,
                None,
                false,
            ),
            (
                None,
                Some("pipewire-pulse"),
                true,
                false,
                false,
                None,
                None,
                false,
            ),
            (
                None,
                Some("pipewire-pulse"),
                true,
                true,
                false,
                Some(1028793),
                None,
                false,
            ),
            (
                None,
                Some("pipewire-pulse"),
                true,
                true,
                true,
                Some(1028793),
                Some(1028793),
                true,
            ),
            (
                Some("pipewire-pulse"),
                None,
                true,
                true,
                true,
                Some(1028793),
                Some(1028793),
                true,
            ),
            (
                Some("pipewire-pulse"),
                None,
                true,
                true,
                false,
                None,
                None,
                false,
            ),
            (
                None,
                Some("pipewire"),
                false,
                false,
                false,
                None,
                Some(1584),
                false,
            ),
            (
                None,
                Some("pipewire"),
                true,
                false,
                false,
                None,
                Some(1584),
                false,
            ),
            (
                None,
                Some("pipewire"),
                true,
                true,
                false,
                None,
                Some(1584),
                true,
            ),
            (
                None,
                None,
                true,
                false,
                false,
                Some(1028793),
                Some(1028793),
                false,
            ),
            (
                None,
                None,
                true,
                true,
                true,
                Some(1028793),
                Some(1028793),
                true,
            ),
        ];
        for (
            node_api,
            client_api,
            info_seen,
            props_seen,
            app_pid_from_info,
            app_pid,
            pid,
            decidable,
        ) in cases
        {
            let clients =
                HashMap::from([(40, ClientEntry::from_props(None, Some("1584"), client_api))]);
            let entry = NodeEntry {
                owning_client_id: Some(40),
                app_pid,
                app_pid_from_info,
                pulse_proxied: is_pulse_proxied(node_api),
                info_seen,
                props_seen,
                ..NodeEntry::default()
            };
            assert_eq!(
                resolve_node_pid(&entry, &clients),
                pid,
                "{entry:?}, {clients:?}"
            );
            assert_eq!(
                exclude_decidable(&entry, &clients),
                decidable,
                "{entry:?}, {clients:?}"
            );
            let exclude = PidSelect::Exclude(HashSet::from([1028793]));
            assert_eq!(
                exclude.selects_node(&entry, &clients),
                decidable && pid != Some(1028793)
            );
            assert_eq!(
                PidSelect::Include(1028793).selects_node(&entry, &clients),
                pid == Some(1028793)
            );
        }
    }

    /// Invalid or removed Pulse PIDs revoke selection; state-only info retains it.
    #[test]
    fn bound_info_pid_update_table() {
        use std::collections::{HashMap, HashSet};

        // Columns: PROPS mask, dictionary, expected app PID, still selected.
        let cases = [
            (false, None, Some(1028793), true),
            (false, Some((Some("0"), None)), Some(1028793), true),
            (true, None, None, false),
            (true, Some((Some("1028793"), None)), Some(1028793), true),
            (true, Some((Some("42"), None)), Some(42), true),
            (true, Some((Some("1584"), None)), Some(1584), false),
            (true, Some((None, None)), None, false),
            (true, Some((Some("0"), None)), None, false),
            (true, Some((Some("-1"), None)), None, false),
            (true, Some((Some("nope"), None)), None, false),
            (true, Some((Some(""), None)), None, false),
            (true, Some((Some("4294967296"), None)), None, false),
        ];
        let exclude = PidSelect::Exclude(HashSet::from([1584]));
        for (node_api, client_api) in [
            (None, Some("pipewire-pulse")),
            (Some("pipewire-pulse"), None),
        ] {
            let clients =
                HashMap::from([(40, ClientEntry::from_props(None, Some("1584"), client_api))]);
            for (props_changed, props, expected_pid, selected) in cases {
                let mut entry = NodeEntry {
                    owning_client_id: Some(40),
                    app_pid: Some(1028793),
                    app_pid_from_info: true,
                    pulse_proxied: is_pulse_proxied(node_api),
                    info_seen: true,
                    props_seen: true,
                    ..NodeEntry::default()
                };
                assert!(exclude.selects_node(&entry, &clients));
                let previous = entry;
                let changed = update_node_info(&mut entry, props_changed, props, &clients);
                assert_eq!(changed, entry != previous);
                assert_eq!(
                    entry.app_pid, expected_pid,
                    "node API={node_api:?}, client API={client_api:?}, mask={props_changed}, props={props:?}"
                );
                assert_eq!(resolve_node_pid(&entry, &clients), expected_pid);
                assert_eq!(exclude.selects_node(&entry, &clients), selected);
                assert_eq!(exclude_decidable(&entry, &clients), expected_pid.is_some());
            }
        }
    }

    /// Native nodes preserve their previous app PID when a PROPS update omits it.
    #[test]
    fn native_bound_info_pid_update_table() {
        use std::collections::{HashMap, HashSet};

        let clients = HashMap::from([(
            40,
            ClientEntry::from_props(None, Some("7"), Some("pipewire")),
        )]);
        for app_pid_from_info in [false, true] {
            for props in [
                None,
                Some((None, None)),
                Some((Some("0"), None)),
                Some((Some("-1"), None)),
                Some((Some("nope"), None)),
                Some((Some(""), None)),
                Some((Some("4294967296"), None)),
            ] {
                let mut entry = NodeEntry {
                    owning_client_id: Some(40),
                    app_pid: Some(42),
                    app_pid_from_info,
                    info_seen: true,
                    props_seen: true,
                    ..NodeEntry::default()
                };
                assert_eq!(
                    update_node_info(&mut entry, true, props, &clients),
                    app_pid_from_info,
                );
                assert_eq!(entry.app_pid, Some(42));
                assert!(!entry.app_pid_from_info);
                assert_eq!(resolve_node_pid(&entry, &clients), Some(42));
                assert!(PidSelect::Include(42).selects_node(&entry, &clients));
                assert!(!PidSelect::Include(7).selects_node(&entry, &clients));
                assert!(PidSelect::Exclude(HashSet::from([7])).selects_node(&entry, &clients));
                assert!(!PidSelect::Exclude(HashSet::from([42])).selects_node(&entry, &clients));
            }
        }
    }

    /// Late Pulse provenance must not accept a PID omitted by the latest PROPS update.
    #[test]
    fn late_client_provenance_rejects_stale_bound_pid() {
        use std::collections::{HashMap, HashSet};

        for props in [Some((None, None)), None] {
            let mut entry = NodeEntry {
                owning_client_id: Some(40),
                ..NodeEntry::default()
            };
            let mut clients = HashMap::new();
            let exclude = PidSelect::Exclude(HashSet::from([7]));

            assert!(update_node_info(
                &mut entry,
                true,
                Some((Some("42"), None)),
                &clients,
            ));
            assert!(entry.app_pid_from_info);
            assert_eq!(resolve_node_pid(&entry, &clients), Some(42));
            assert!(exclude.selects_node(&entry, &clients));

            assert!(update_node_info(&mut entry, true, props, &clients));
            assert_eq!(entry.app_pid, Some(42));
            assert!(!entry.app_pid_from_info);
            assert_eq!(resolve_node_pid(&entry, &clients), Some(42));
            assert!(exclude.selects_node(&entry, &clients));

            clients.insert(
                40,
                ClientEntry::from_props(None, Some("7"), Some("pipewire-pulse")),
            );
            assert_eq!(resolve_node_pid(&entry, &clients), None);
            assert!(!exclude_decidable(&entry, &clients));
            assert!(!exclude.selects_node(&entry, &clients));
            assert!(!PidSelect::Include(42).selects_node(&entry, &clients));
        }
    }

    /// Only a dictionary carried by a PROPS update unlocks Exclude decisions.
    #[test]
    fn bound_props_seen_table() {
        use std::collections::{HashMap, HashSet};

        let clients = HashMap::from([(40, ClientEntry::from_props(None, Some("7"), None))]);
        for (props_changed, props, props_seen) in [
            (false, None, false),
            (false, Some((None, None)), false),
            (true, None, false),
            (true, Some((None, None)), true),
        ] {
            let mut entry = NodeEntry {
                owning_client_id: Some(40),
                ..NodeEntry::default()
            };
            assert!(update_node_info(&mut entry, props_changed, props, &clients));
            assert!(entry.info_seen);
            assert_eq!(entry.props_seen, props_seen);
            assert_eq!(exclude_decidable(&entry, &clients), props_seen);
            assert_eq!(
                PidSelect::Exclude(HashSet::from([42])).selects_node(&entry, &clients),
                props_seen,
            );
        }
    }

    /// Fresh Pulse info without a usable PID never allows credential fallback.
    #[test]
    fn pulse_missing_or_invalid_bound_pid_table() {
        use std::collections::HashMap;

        for app_pid in [None, Some("0"), Some("-2"), Some("NaN"), Some("")] {
            for (node_api, client_api) in [
                (None, Some("pipewire-pulse")),
                (Some("pipewire-pulse"), None),
            ] {
                let clients =
                    HashMap::from([(40, ClientEntry::from_props(None, Some("1584"), client_api))]);
                let mut entry = NodeEntry {
                    owning_client_id: Some(40),
                    ..NodeEntry::default()
                };
                assert!(update_node_info(
                    &mut entry,
                    true,
                    Some((app_pid, node_api)),
                    &clients,
                ));
                assert_eq!(resolve_node_pid(&entry, &clients), None);
                assert!(!exclude_decidable(&entry, &clients));
            }
        }
    }

    #[test]
    fn late_client_provenance_revokes_provisional_global_pid() {
        use std::collections::{HashMap, HashSet};

        let mut entry = NodeEntry {
            owning_client_id: Some(40),
            app_pid: Some(1584),
            ..NodeEntry::default()
        };
        let mut clients = HashMap::new();
        update_node_info(&mut entry, false, None, &clients);
        let exclude = PidSelect::Exclude(HashSet::from([1028793]));
        assert!(entry.info_seen);
        assert!(!entry.props_seen);
        assert!(!exclude.selects_node(&entry, &clients));
        clients.insert(40, ClientEntry::from_props(None, Some("1584"), None));
        assert!(!exclude.selects_node(&entry, &clients));
        clients.insert(
            40,
            ClientEntry::from_props(None, Some("1584"), Some("pipewire-pulse")),
        );
        assert!(!exclude.selects_node(&entry, &clients));
        update_node_info(&mut entry, true, Some((Some("1028793"), None)), &clients);
        assert_eq!(resolve_node_pid(&entry, &clients), Some(1028793));
        assert!(!exclude.selects_node(&entry, &clients));
    }

    /// `PidSelect::Exclude` holds a SET of pids (`exclude_self` ∪ `exclude_pids`),
    /// and the predicate trio (`is_subject_pid` / `selects` / `node_key`) is
    /// PipeWire-independent.
    #[test]
    fn pid_select_exclude_takes_a_set() {
        use std::collections::HashSet;
        let sel = PidSelect::Exclude(HashSet::from([10, 20]));
        assert!(sel.is_subject_pid(10) && sel.is_subject_pid(20) && !sel.is_subject_pid(30));
        // Exclude links every RESOLVED pid outside the set; unresolved waits.
        assert!(sel.selects(Some(30)));
        assert!(!sel.selects(Some(20)));
        assert!(!sel.selects(None));
        let inc = PidSelect::Include(7);
        assert!(inc.selects(Some(7)) && !inc.selects(Some(8)) && !inc.selects(None));
        assert_eq!(inc.node_key(), "7");
        assert_eq!(sel.node_key(), "excl-10");
    }

    /// `effective_exclusion` is table-driven and independent of the running
    /// process: `exclude_pids` ∪ `{self_pid}` when `exclude_self`, deduped.
    #[test]
    fn effective_exclusion_unions_and_dedups() {
        use std::collections::HashSet;
        let self_pid = 4242u32;
        let cases: &[(bool, &[u32], HashSet<u32>, &str)] = &[
            (
                false,
                &[],
                HashSet::new(),
                "neither flag nor pids → sink-monitor path",
            ),
            (
                true,
                &[],
                HashSet::from([self_pid]),
                "exclude_self alone → just self",
            ),
            (
                false,
                &[5, 6],
                HashSet::from([5, 6]),
                "pids alone → fan-in without self",
            ),
            (
                true,
                &[5, 4242],
                HashSet::from([5, self_pid]),
                "self pid already listed → union, no duplicate",
            ),
        ];
        for (excl_self, pids, want, msg) in cases {
            assert_eq!(
                effective_exclusion(*excl_self, pids, self_pid),
                *want,
                "{msg}"
            );
        }
    }

    /// `with_exclude_pids` records the extra pids without disturbing `exclude_self`.
    #[test]
    fn system_backend_exclude_pids_builder() {
        let be = PwSystemBackend::new(false, None).with_exclude_pids(vec![5, 6]);
        assert_eq!(be.exclude_pids(), &[5, 6]);
        assert!(!be.exclude_self());
    }

    /// Verify `pair_ports` channel matching (independent of PipeWire).
    #[test]
    fn pair_ports_maps_channels() {
        // Stereo to stereo: FL→FL / FR→FR (matching channel names).
        // Output ports: id 10=FL, 11=FR. Input ports: id 20=FL, 21=FR.
        let out = vec![(10u32, "FL".to_string()), (11u32, "FR".to_string())];
        let inp = vec![(20u32, "FL".to_string()), (21u32, "FR".to_string())];
        let mut pairs = pair_ports(&out, &inp);
        pairs.sort();
        assert_eq!(pairs, vec![(10, 20), (11, 21)], "FL→FL / FR→FR");

        // Channel names produce the correct pairs even if input order is reversed.
        let inp_rev = vec![(21u32, "FR".to_string()), (20u32, "FL".to_string())];
        let mut pairs = pair_ports(&out, &inp_rev);
        pairs.sort();
        assert_eq!(
            pairs,
            vec![(10, 20), (11, 21)],
            "FL→FL / FR→FR even with reversed order"
        );

        // Mono output to stereo input: duplicate the single output to both FL/FR.
        let mono_out = vec![(30u32, "MONO".to_string())];
        let stereo_in = vec![(40u32, "FL".to_string()), (41u32, "FR".to_string())];
        let mut pairs = pair_ports(&mono_out, &stereo_in);
        pairs.sort();
        assert_eq!(
            pairs,
            vec![(30, 40), (30, 41)],
            "mono is duplicated to FL/FR"
        );

        // Missing (empty) channel names → order fallback.
        let out_noch = vec![(50u32, String::new()), (51u32, String::new())];
        let in_noch = vec![(60u32, String::new()), (61u32, String::new())];
        let pairs = pair_ports(&out_noch, &in_noch);
        // Two ports pair one-to-one (each input is used at most once).
        assert_eq!(pairs.len(), 2);
        let ins: std::collections::HashSet<u32> = pairs.iter().map(|(_, i)| *i).collect();
        assert_eq!(ins.len(), 2, "each input port is used at most once");

        // Empty sets produce no links (do not link if either side is absent).
        assert!(pair_ports(&[], &inp).is_empty());
        assert!(pair_ports(&out, &[]).is_empty());

        // If only one side has a matching channel, use order fallback instead of mono duplication.
        // Output has FL only, input has FR only (name mismatch) → one pair by order fallback.
        let out_fl = vec![(70u32, "FL".to_string())];
        let in_fr = vec![(80u32, "FR".to_string())];
        // With one output port, mono duplication applies to the remaining inputs.
        let pairs = pair_ports(&out_fl, &in_fr);
        assert_eq!(
            pairs,
            vec![(70, 80)],
            "single output is duplicated to remaining inputs"
        );
    }

    /// Both sides of the fan-in race, as a table:
    /// (expected_out, out_ports_len, in_ports_len, pairs_len, capture_channels).
    #[test]
    fn link_plan_is_complete_requires_every_channel_on_both_sides() {
        struct Case {
            expected_out: Option<u32>,
            out_len: usize,
            in_len: usize,
            pairs_len: usize,
            chans: usize,
            want: bool,
            why: &'static str,
        }
        let case = |expected_out, out_len, in_len, pairs_len, chans, want, why| Case {
            expected_out,
            out_len,
            in_len,
            pairs_len,
            chans,
            want,
            why,
        };
        let cases = [
            case(
                Some(2),
                2,
                1,
                1,
                2,
                false,
                "capture input FR has not arrived yet",
            ),
            case(Some(2), 2, 2, 2, 2, true, "stereo source fully paired"),
            case(
                Some(2),
                1,
                2,
                2,
                2,
                false,
                "target output FR missing — pair_ports' mono rule duplicated FL onto both \
                 inputs, which must not latch",
            ),
            case(
                Some(1),
                1,
                2,
                2,
                2,
                true,
                "genuine mono source duplicated onto FL+FR",
            ),
            case(
                Some(6),
                6,
                2,
                2,
                2,
                false,
                "multichannel input is unsupported",
            ),
            case(
                None,
                2,
                2,
                2,
                2,
                false,
                "unknown layout cannot establish complete routing",
            ),
            case(None, 0, 2, 0, 2, false, "nothing to link"),
            case(
                Some(0),
                1,
                2,
                2,
                2,
                false,
                "a declared count of 0 alongside a visible port means the node has not \
                 finished describing itself — not known yet, so incomplete",
            ),
        ];
        for c in cases {
            assert_eq!(
                link_plan_is_complete(c.expected_out, c.out_len, c.in_len, c.pairs_len, c.chans),
                c.want,
                "{} ({:?}, {}, {}, {}, {})",
                c.why,
                c.expected_out,
                c.out_len,
                c.in_len,
                c.pairs_len,
                c.chans
            );
        }
    }

    /// Smoke test: process capture `start` may return `Err(Error::Backend)` without panicking on
    /// a headless machine where PipeWire is unavailable or registry retrieval fails. With a
    /// PipeWire session, it succeeds and waits even if the target PID has not appeared yet (the
    /// registry is available and the process will be linked if it appears). On Ok, verify the
    /// start-to-stop cycle works even if the target PID is silent (safe destruction).
    #[test]
    fn process_start_is_graceful_without_pipewire() {
        let (prod, _cons) = raw_ring(1 << 16);
        let sink = RawSink::new(prod, NATIVE_RATE, NATIVE_CHANNELS);
        // A likely nonexistent PID. Include start may wait successfully even if it never appears.
        let mut be = PwProcessBackend::new(u32::MAX, ProcessMode::Include);
        match be.start(sink) {
            Ok(()) => {
                // PipeWire session available. Waiting succeeds even if the target PID is absent.
                // Duplicate start is safe (no-op returning Ok).
                let (prod2, _cons2) = raw_ring(1 << 16);
                let sink2 = RawSink::new(prod2, NATIVE_RATE, NATIVE_CHANNELS);
                assert!(be.start(sink2).is_ok());
                // Complete the stop cycle (safe to destroy before linking).
                be.stop();
                // Duplicate stop is also safe.
                be.stop();
            }
            Err(error) if error.kind() == flexaudio_core::ErrorKind::Backend => {
                // PipeWire unavailable/registry failure is expected. The key is no panic.
            }
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }

    /// Real capture end-to-end (only on a desktop/laptop running PipeWire).
    ///
    /// How to run (on a laptop or similar with PipeWire and audio playing from the target PID):
    /// ```text
    /// # Example: play speaker-test and get its PID
    /// speaker-test -t sine -f 1000 -c 2 &  # → note the PID
    /// FLEXAUDIO_TEST_PID=<PID> \
    ///   cargo test -p flexaudio-os-linux -- --ignored process_capture_smoke
    /// ```
    /// Link to the target PID's app output ports with link-factory and expect samples to arrive.
    /// Skip if `FLEXAUDIO_TEST_PID` is unset (the PID is unknown). Ignored in headless environments/CI
    /// because they have neither PipeWire nor an audio source.
    #[test]
    #[ignore = "requires a running PipeWire session with the target PID playing audio (set FLEXAUDIO_TEST_PID)"]
    fn process_capture_smoke() {
        use std::time::Duration;
        let Ok(pid_str) = std::env::var("FLEXAUDIO_TEST_PID") else {
            eprintln!("skipping because FLEXAUDIO_TEST_PID is not set");
            return;
        };
        let pid: u32 = pid_str.parse().expect("FLEXAUDIO_TEST_PID must be a u32");
        let (prod, mut cons) = raw_ring(1 << 18);
        let sink = RawSink::new(prod, NATIVE_RATE, NATIVE_CHANNELS);
        let mut be = PwProcessBackend::new(pid, ProcessMode::Include);
        be.start(sink)
            .expect("start should succeed on a PipeWire desktop");
        // Wait briefly for linking and capture to start.
        thread::sleep(Duration::from_millis(800));
        be.stop();
        let mut out = vec![0.0f32; 1920];
        let got = cons.pop_slice(&mut out);
        assert!(
            got > 0,
            "expected captured samples link-factory-linked from PID {pid}"
        );
    }

    // ------------------------------------------------------------------------
    // PwDeviceWatcher (hot-plug notifications)
    // ------------------------------------------------------------------------

    /// Verify [`PwDeviceWatcher`] is `Send` (proves PipeWire `!Send` values are confined to the
    /// dedicated thread). Passing compilation is enough. Follows the corresponding
    /// `PwSystemBackend` test.
    #[test]
    fn watcher_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<PwDeviceWatcher>();
    }

    /// Verify `start()` does not panic in a headless environment without PipeWire. It may return
    /// `Ok` when a PipeWire session exists or `Err(Backend)` otherwise; the key is no panic (Linux
    /// facade propagates startup errors). On Ok, also verify the stop cycle completes safely.
    #[test]
    fn watcher_graceful_without_pipewire() {
        match PwDeviceWatcher::start() {
            Ok(mut w) => {
                // PipeWire session available. poll_event is nonblocking; since initial scan
                // events are suppressed, it may immediately return None (an event is also fine).
                let _ = w.poll_event();
                w.stop();
            }
            Err(error) if error.kind() == flexaudio_core::ErrorKind::Backend => {
                // PipeWire unavailable is expected. The key is no panic.
            }
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }

    /// After successful `start()`, calling `stop()` twice is safe (no panic; the second call is
    /// a no-op). Skip if `start()` returns Err because PipeWire is unavailable.
    #[test]
    fn watcher_double_stop_is_safe() {
        if let Ok(mut w) = PwDeviceWatcher::start() {
            w.stop();
            w.stop();
        }
        // If start failed (PipeWire unavailable), there is nothing to verify; no panic is enough.
    }

    /// Verify `enqueue_event` / `poll`-style queue operations work in FIFO order (independent of
    /// PipeWire; tests only the event queue logic).
    #[test]
    fn enqueue_and_drain_is_fifo() {
        let events: WatchEventQueue = Arc::new(Mutex::new(WatchEvents::default()));
        let mic = DeviceInfo {
            id: "mic.a".into(),
            name: "Mic A".into(),
            source_kind: SourceKind::Mic,
            sample_rate: NATIVE_RATE,
            channels: NATIVE_CHANNELS,
            is_loopback: false,
            is_default: false,
        };
        enqueue_event(&events, DeviceEvent::Added(mic.clone()));
        enqueue_event(&events, DeviceEvent::Removed { id: "mic.a".into() });
        enqueue_event(
            &events,
            DeviceEvent::DefaultChanged {
                kind: flexaudio_core::DefaultDeviceKind::SystemAudio,
                id: "sink.x".into(),
            },
        );
        // Equivalent to poll_event (pop in FIFO order).
        let mut drained = Vec::new();
        while let Some(ev) = lock_events(&events).poll() {
            drained.push(ev);
        }
        assert_eq!(
            drained,
            vec![
                DeviceEvent::Added(mic),
                DeviceEvent::Removed { id: "mic.a".into() },
                DeviceEvent::DefaultChanged {
                    kind: flexaudio_core::DefaultDeviceKind::SystemAudio,
                    id: "sink.x".into(),
                },
            ]
        );
    }

    /// `enqueue_event` caps the event queue at [`MAX_WATCH_EVENTS`], dropping the oldest event
    /// before pushing new ones when full. Add more than the limit and verify the queue stays
    /// within the limit and retains the newest entries.
    #[test]
    fn enqueue_event_caps_queue_and_drops_oldest() {
        let events: WatchEventQueue = Arc::new(Mutex::new(WatchEvents::default()));
        // Push the limit + 10 events. Use node numbers in ids to track which remain.
        let total = MAX_WATCH_EVENTS + 10;
        for i in 0..total {
            enqueue_event(
                &events,
                DeviceEvent::Removed {
                    id: format!("n{i}"),
                },
            );
        }
        assert_eq!(
            lock_events(&events).poll(),
            Some(DeviceEvent::RescanRequired { dropped_events: 10 })
        );
        let queue = events.lock().unwrap();
        let q = &queue.deltas;
        // Length never exceeds the limit.
        assert_eq!(
            q.len(),
            MAX_WATCH_EVENTS,
            "queue length is capped at the limit"
        );
        // The oldest 10 events (n0..n9) are dropped, so the first is n10.
        match q.front().unwrap() {
            DeviceEvent::Removed { id } => assert_eq!(id, "n10", "oldest events are dropped first"),
            other => panic!("unexpected event: {other:?}"),
        }
        // The newest event (n{total-1}) remains.
        match q.back().unwrap() {
            DeviceEvent::Removed { id } => assert_eq!(id, &format!("n{}", total - 1)),
            other => panic!("unexpected event: {other:?}"),
        }
    }
}
