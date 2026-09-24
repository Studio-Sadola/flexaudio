//! flexaudio-os-linux — Linux backend: PipeWire (`pipewire` 0.10)
//!
//! Provides [`PwSystemBackend`], which captures "system audio output (the default sink's
//! monitor)". It is the Linux equivalent of WASAPI loopback, and records the very sound flowing
//! to the speakers through a `Stream/Input/Audio` stream with `stream.capture.sink=true`.
//!
//! # Handling `!Send`
//!
//! PipeWire's `MainLoop` / `Context` / `Core` / `Stream` are `!Send` (they hold raw pointers and
//! a thread-local loop). [`CaptureBackend`], on the other hand, requires `Send`.
//! So PipeWire creation, execution, and destruction are confined to a single dedicated thread,
//! and [`PwSystemBackend`] holds only `Send` things (the stop
//! [`pipewire::channel::Sender`], the [`JoinHandle`], and a [`std::sync::mpsc`] for receiving the
//! startup result). `MainLoop` and friends never cross the thread boundary.
//!
//! # Format
//!
//! Requests 48000 Hz / 2ch / f32. Even if the graph's rate/channels differ, PipeWire
//! automatically inserts `audioconvert` to convert, so the core does not need to resample/remix.
//!
//! # Non-Linux
//!
//! With `#![cfg(target_os = "linux")]` this compiles to nothing on non-Linux, and the `pipewire`
//! dependency is pulled in only by the `target.'cfg(...linux)'` section of `Cargo.toml`.

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

use pipewire as pw;
use pw::spa;
use pw::{properties::properties, stream::StreamFlags};
use spa::param::format::{MediaSubtype, MediaType};
use spa::param::format_utils;
use spa::pod::Pod;

/// Native sample rate (Hz). Requests 48 kHz and lets PipeWire convert.
const NATIVE_RATE: u32 = 48_000;
/// Native channel count. Requests stereo and lets PipeWire convert.
const NATIVE_CHANNELS: u16 = 2;

/// Upper bound of the watch queue (number of events). Prevents the `VecDeque` from growing
/// without limit when the consumer does not call `poll_event` for a long time, or when devices
/// are hotplugged repeatedly. On overflow, the oldest event is dropped.
const MAX_WATCH_EVENTS: usize = 1024;

/// Deadline (milliseconds) of the sync wait loop in [`enumerate_pw`]. `done` usually arrives
/// quickly, but this prevents `while !done { run() }` from looping forever/hanging when it does
/// not. On overrun, it gives up and returns what has been collected so far.
const ENUMERATE_DEADLINE_MS: u128 = 2_000;

/// Calls [`pipewire::init`] exactly once for the whole process.
///
/// `pw::init()` is a library-internal global initialization and may be called concurrently from
/// several backend threads (system / process / watch / enumerate). Calling it multiple times
/// raises thread-race concerns, so it is collapsed into one call with [`std::sync::Once`].
fn pw_init_once() {
    use std::sync::Once;
    static PW_INIT: Once = Once::new();
    PW_INIT.call_once(|| {
        pw::init();
    });
}

// Enumeration of capturable processes (`list_processes`). PID resolution shares the same
// `resolve_node_pid` as per-process capture.
mod processes;
pub use processes::list_processes;

/// [`CaptureBackend`] that captures system audio output (the sink's monitor) via
/// PipeWire.
///
/// Builds a PipeWire `MainLoop` + input `Stream` on a dedicated thread, and streams the
/// interleaved f32 samples dequeued in the `process` callback to [`RawSink::push`] without
/// blocking. With `stream.capture.sink=true`, the target is not a recording device but the
/// monitor of a sink (speakers), i.e. system audio output.
///
/// If `device_id` is `None`, records the default sink's monitor; if `Some(node.name)`, records
/// that sink's monitor (selected via `target.object`). If the specified sink does not exist,
/// [`start`](CaptureBackend::start) returns [`Error::DeviceNotFound`].
///
/// In environments without PipeWire/a sink (headless servers, etc.) it does not panic;
/// [`start`](CaptureBackend::start) returns [`Error::Backend`].
///
/// ```no_run
/// use flexaudio_os_linux::PwSystemBackend;
/// use flexaudio_core::backend::CaptureBackend;
///
/// let backend = PwSystemBackend::new(false, None);
/// assert_eq!(backend.native_format(), (48_000, 2));
/// // let mut backend = backend;
/// // backend.start(sink)?;   // Err(Backend) if PipeWire is absent / no sink is running
/// // ...
/// // backend.stop();
/// ```
pub struct PwSystemBackend {
    /// Whether to exclude the host process's own playback (feedback prevention). When `true`,
    /// [`start`](CaptureBackend::start) reuses the process Exclude mechanism with the excluded
    /// PID = `std::process::id()`, and fan-in links and records every app output
    /// (`Stream/Output/Audio`) other than its own. The sink monitor is already mixed and its own
    /// part cannot be subtracted, so this is the only way to exclude itself. If `false`, records
    /// the sink's monitor as-is.
    exclude_self: bool,
    /// Selects the sink to record by `node.name`. `None` means the default sink's monitor.
    /// `Some(id)` records that sink's monitor, selected via target.object (the
    /// `DeviceInfo.id` returned by [`list_devices`] is this `node.name`). It has no effect on
    /// the `exclude_self == true` fan-in path (that path does not target a specific sink, so it
    /// is ignored).
    device_id: Option<String>,
    /// Running flag (double-start guard / drop check). `Send`.
    running: Arc<AtomicBool>,
    /// Sender that tells the loop thread to stop. `Some` after `start`. Sending on it makes the
    /// receiver callback attached to the loop call `main_loop.quit()` from the loop thread
    /// itself, exiting `run()`.
    stop_tx: Option<pw::channel::Sender<Terminate>>,
    /// Handle of the PipeWire loop thread. `Some` after `start`.
    handle: Option<JoinHandle<()>>,
}

/// Stop message sent to the loop thread (zero-sized).
struct Terminate;

impl PwSystemBackend {
    /// Constructs the backend (does not connect to PipeWire at this point).
    ///
    /// If `exclude_self` is `false` (default), records the sink's monitor as-is. If `true`,
    /// fan-ins and records every app output other than its own (reusing the process Exclude
    /// mechanism, with excluded PID = `std::process::id()`).
    ///
    /// `device_id` selects the sink to record by `node.name`. `None` means the default sink.
    /// Ignored when `exclude_self == true` (fan-in does not target a specific sink).
    /// The actual connection and stream creation happen inside
    /// [`start`](CaptureBackend::start), on a dedicated thread.
    pub fn new(exclude_self: bool, device_id: Option<String>) -> Self {
        Self {
            exclude_self,
            device_id,
            running: Arc::new(AtomicBool::new(false)),
            stop_tx: None,
            handle: None,
        }
    }

    /// The `exclude_self` flag.
    pub fn exclude_self(&self) -> bool {
        self.exclude_self
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
        // Safe against double start (does nothing if already running).
        if self.running.load(Ordering::SeqCst) {
            return Ok(());
        }

        // If device_id is specified (and this is the normal monitor path), first check that the
        // sink exists. If not, DeviceNotFound. When enumerate_pw returns Err (no daemon, etc.),
        // do not swallow it; proceed to the normal setup and let the connection failure be
        // returned as Backend (so "absent" is not confused with "no such sink"). The
        // exclude_self fan-in path does not target a specific sink, so it is not checked.
        let device_id = self.device_id.clone();
        if !self.exclude_self {
            if let Some(id) = device_id.as_deref() {
                if let Ok(devs) = enumerate_pw() {
                    let found = devs.iter().any(|d| d.is_loopback && d.id == id);
                    if !found {
                        return Err(Error::DeviceNotFound);
                    }
                }
            }
        }

        // Stop channel to the loop thread (the receiver is attached to the loop).
        let (stop_tx, stop_rx) = pw::channel::channel::<Terminate>();
        // Channel that reports setup success/failure back to start() synchronously. Ok(()) if
        // everything from init→mainloop→context→connect→stream→connect succeeds, otherwise
        // Err(error string).
        let (ready_tx, ready_rx) = mpsc::channel::<std::result::Result<(), String>>();

        let running = self.running.clone();
        running.store(true, Ordering::SeqCst);

        // exclude_self reuses the process Exclude mechanism. With the excluded PID =
        // std::process::id(), it fan-in links every app output (Stream/Output/Audio) other than
        // its own to its capture input, and records "system audio − the host process's
        // playback". The sink monitor is already mixed, and PipeWire has no OS primitive to
        // subtract only the host process's part from it, so this app-output fan-in is the only
        // way to exclude itself. exclude_self == false stays on the sink monitor.
        // With exclude_self it is a fan-in, so device_id is not used.
        let exclude_self = self.exclude_self;
        let handle = thread::Builder::new()
            .name(
                if exclude_self {
                    "flexaudio-pw-system-excl"
                } else {
                    "flexaudio-pw-system"
                }
                .into(),
            )
            .spawn(move || {
                if exclude_self {
                    // Delegate to the Exclude mechanism that records everything except itself
                    // (std::process::id()). The stop/ready channels and Terminate are shared
                    // with the system path.
                    run_pw_process_loop(
                        PidSelect::Exclude(std::process::id()),
                        sink,
                        stop_rx,
                        &ready_tx,
                    );
                } else {
                    run_pw_loop(device_id, sink, stop_rx, &ready_tx);
                }
            })
            .map_err(|e| Error::Backend(format!("spawn pipewire thread: {e}")))?;

        // Wait for the setup result. If the thread exits without sending ready (recv error),
        // that is also treated as a failure.
        match ready_rx.recv() {
            Ok(Ok(())) => {
                // Setup succeeded. Keep the stop sender and the handle.
                self.stop_tx = Some(stop_tx);
                self.handle = Some(handle);
                Ok(())
            }
            Ok(Err(msg)) => {
                // Setup failed (pipewire absent, no sink, connect failed, etc.).
                // The thread has already returned, so join it to clean up.
                //
                // All failures become Error::Backend. For none of connect failure, stream
                // creation failure, or format negotiation failure does PipeWire have an API that
                // distinguishes permission denial (portal/Flatpak/RTKit refusal) from absence (no
                // sink/source/session) by type (it returns errno/generic strings, with no
                // HRESULT/OSStatus equivalent separating PermissionDenied from NotFound). So a
                // typed classification like on macOS/Windows is not possible. Absence of the
                // specified sink is caught at the top of start by checking enumerate_pw, which
                // returns DeviceNotFound first, so it never gets here.
                running.store(false, Ordering::SeqCst);
                let _ = handle.join();
                Err(Error::Backend(msg))
            }
            Err(_) => {
                // The thread vanished without ever sending ready (unexpected panic, etc.).
                running.store(false, Ordering::SeqCst);
                let _ = handle.join();
                Err(Error::Backend(
                    "pipewire setup thread terminated before signaling readiness".into(),
                ))
            }
        }
    }

    fn stop(&mut self) {
        // Safe against double stop / stop before start.
        if !self.running.swap(false, Ordering::SeqCst) {
            // running is false → never started or already stopped. Join leftovers just in case.
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
            self.stop_tx = None;
            return;
        }

        // Notify the loop thread to stop (the receiver callback calls loop.quit()).
        // Send before dropping the sender. Failure (receiver gone) is ignored (already finished).
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(Terminate);
        }

        // Wait for the thread to exit run() and terminate. When the thread ends,
        // Stream→Core→Context→MainLoop are destroyed in drop order (all on the loop thread).
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for PwSystemBackend {
    fn drop(&mut self) {
        self.stop();
    }
}

// ============================================================================
// Process output loopback (captures a specific PID's app audio via a fan-out duplicate)
// ============================================================================

/// [`CaptureBackend`] that captures the audio output of a specific process (PID) via PipeWire.
/// The Linux equivalent of WASAPI process-loopback (`AUDIOCLIENT_ACTIVATION_PARAMS`).
///
/// # Explicitly linking output ports → own input ports with link-factory
///
/// Specifying the node via `stream.connect`'s target/`target.object` was ignored by
/// WirePlumber in real-device testing, and the capture got connected to the default source,
/// i.e. the microphone. So the input ports of our own capture stream are explicitly linked to
/// the ports of the target process's output node with link-factory (the API version of
/// `pw-link out_FL→in_FL / out_FR→in_FR`). The app's original link to the default sink stays
/// as-is (fan-out), so the user's speakers keep playing.
///
/// The PID-to-node mapping is resolved in two stages. In PipeWire the PID lives not on the node
/// but on the Client object: `pipewire.sec.pid` (`*pw::keys::SEC_PID`) is always present in the
/// registry's Client global props (the daemon sets it from the socket credentials, so it cannot
/// be spoofed; confirmed on a stock real-device setup). A node only points to its owning Client
/// via `client.id`. So the chain is "PID → global id of the Client with
/// `pipewire.sec.pid == target_pid` → the `Stream/Output/Audio` node whose `client.id` is that
/// id" (see `resolve_node_pid`).
///
/// Our own stream connects with `stream.connect(Direction::Input, None, ...)` but without
/// `AUTOCONNECT` (preventing auto-linking to the microphone so only explicit links exist). This
/// creates the input ports (input_FL/FR), and no data arrives until they are linked. As the
/// target output ports and our own input ports appear,
/// `core.create_object::<Link>("link-factory", ...)` with `LINK_OUTPUT_NODE/PORT` and
/// `LINK_INPUT_NODE/PORT` creates the channel-matched links, re-planning them on each later
/// port arrival so the plan is never fixed while ports are still arriving.
///
/// # Handling `!Send`
///
/// Like [`PwSystemBackend`], it is confined to a single dedicated thread. `MainLoop`/`Context`/
/// `Core`/`Registry`/`Stream` are `!Send`, so they live on a dedicated thread
/// (`flexaudio-pw-process`), and the backend itself holds only `Send` things (the stop
/// [`pipewire::channel::Sender`], the [`JoinHandle`], and an [`AtomicBool`]).
///
/// # Starting to play later / disappearing is the normal case
///
/// The target PID's node not existing yet / appearing later is the normal case. Once connected
/// to the PipeWire daemon and the registry has been obtained, [`start`](CaptureBackend::start)
/// is treated as successful and waits; as the target output ports and our own input ports
/// arrive via the registry's `global`, it links them with link-factory. When
/// `global_remove` detects that the target disappeared, it drops the links and waits again (it
/// can relink idempotently). Only when the PipeWire daemon is absent or getting the registry
/// fails does it return [`Error::Backend`] immediately (no panic).
///
/// # `mode`: Include / Exclude
///
/// - [`ProcessMode::Include`] (default): records only the target PID's node (fan-out link; one
///   representative node).
/// - [`ProcessMode::Exclude`]: fan-in links and records every app output
///   (`Stream/Output/Audio`) other than the target PID to our own capture input (Include's
///   predicate inverted and made multi-node). Nodes whose PID is not yet resolved are deferred
///   until their Client arrives, so the excluded process is never mistaken.
///
/// The system source's `exclude_self` is unrelated to this process backend.
///
/// ```no_run
/// use flexaudio_os_linux::PwProcessBackend;
/// use flexaudio_core::backend::CaptureBackend;
/// use flexaudio_core::types::ProcessMode;
///
/// let backend = PwProcessBackend::new(12345, ProcessMode::Include);
/// assert_eq!(backend.native_format(), (48_000, 2));
/// // let mut backend = backend;
/// // backend.start(sink)?;  // Err(Backend) if PipeWire is absent / registry fails;
/// //                        // otherwise succeeds and waits (Include waits for the target PID
/// //                        // to appear; Exclude fan-in links everything but the target PID
/// //                        // as it appears).
/// // ...
/// // backend.stop();
/// ```
pub struct PwProcessBackend {
    /// PID of the process to capture. It is matched against `pipewire.sec.pid`
    /// (`*pw::keys::SEC_PID`) of the registry's Client objects, and the output nodes pointing to
    /// that Client via `client.id` become the target (two-stage match; see [`resolve_node_pid`]).
    target_pid: u32,
    /// How the target PID is treated. [`ProcessMode::Include`] records only the target PID.
    /// [`ProcessMode::Exclude`] fan-ins and records every app output other than the target PID.
    mode: ProcessMode,
    /// Running flag (double-start guard / drop check). `Send`.
    running: Arc<AtomicBool>,
    /// Sender that tells the loop thread to stop. `Some` after `start`.
    /// Uses the same [`Terminate`] as [`PwSystemBackend`].
    stop_tx: Option<pw::channel::Sender<Terminate>>,
    /// Handle of the PipeWire loop thread. `Some` after `start`.
    handle: Option<JoinHandle<()>>,
}

impl PwProcessBackend {
    /// Constructs the backend from the target PID and `mode` (does not connect to PipeWire at
    /// this point). The actual connection, stream creation, and link-factory linking happen
    /// inside [`start`](CaptureBackend::start), on a dedicated thread.
    ///
    /// [`ProcessMode::Include`] records only the target PID. [`ProcessMode::Exclude`] fan-ins
    /// and records every app output other than the target PID.
    pub fn new(target_pid: u32, mode: ProcessMode) -> Self {
        Self {
            target_pid,
            mode,
            running: Arc::new(AtomicBool::new(false)),
            stop_tx: None,
            handle: None,
        }
    }

    /// PID of the capture target.
    pub fn target_pid(&self) -> u32 {
        self.target_pid
    }

    /// The `mode` (Include/Exclude).
    pub fn mode(&self) -> ProcessMode {
        self.mode
    }
}

impl CaptureBackend for PwProcessBackend {
    fn native_format(&self) -> (u32, u16) {
        (NATIVE_RATE, NATIVE_CHANNELS)
    }

    fn start(&mut self, sink: RawSink) -> Result<()> {
        // Safe against double start (does nothing if already running).
        if self.running.load(Ordering::SeqCst) {
            return Ok(());
        }

        // Map mode to a node-selection predicate.
        // - Include: link only the target PID's node (one representative node).
        // - Exclude: link every Stream/Output/Audio node other than the target PID (fan-in).
        let select = match self.mode {
            ProcessMode::Include => PidSelect::Include(self.target_pid),
            ProcessMode::Exclude => PidSelect::Exclude(self.target_pid),
        };

        // Stop channel to the loop thread (the receiver is attached to the loop).
        let (stop_tx, stop_rx) = pw::channel::channel::<Terminate>();
        // Channel that reports setup success/failure back to start() synchronously. Success here
        // covers "PipeWire connection + registry acquisition + stream creation + registry
        // listener registration". The fan-out link to the target PID is not part of the success
        // condition (not having appeared yet is the normal case; the link is made from the
        // registry callback when it appears).
        let (ready_tx, ready_rx) = mpsc::channel::<std::result::Result<(), String>>();

        let running = self.running.clone();
        running.store(true, Ordering::SeqCst);

        let handle = thread::Builder::new()
            .name("flexaudio-pw-process".into())
            .spawn(move || {
                run_pw_process_loop(select, sink, stop_rx, &ready_tx);
            })
            .map_err(|e| Error::Backend(format!("spawn pipewire process thread: {e}")))?;

        // Wait for the setup result. A thread that exits without sending ready is also a failure.
        match ready_rx.recv() {
            Ok(Ok(())) => {
                // Setup succeeded (from connection through registry listener registration).
                // From here the thread waits until the target PID appears, and creates the
                // link-factory links as the output ports / own input ports arrive.
                self.stop_tx = Some(stop_tx);
                self.handle = Some(handle);
                Ok(())
            }
            Ok(Err(msg)) => {
                // Setup failed (pipewire absent, connect/registry failure, etc.). Always
                // Error::Backend (for the reason, see the same spot in PwSystemBackend::start:
                // PipeWire cannot distinguish permission denial/absence by type). Absence of the
                // target PID is a normal-case wait, not an error (waiting for it to appear in the
                // registry), so it is not turned into DeviceNotFound here.
                running.store(false, Ordering::SeqCst);
                let _ = handle.join();
                Err(Error::Backend(msg))
            }
            Err(_) => {
                // The thread vanished without ever sending ready (unexpected panic, etc.).
                running.store(false, Ordering::SeqCst);
                let _ = handle.join();
                Err(Error::Backend(
                    "pipewire process setup thread terminated before signaling readiness".into(),
                ))
            }
        }
    }

    fn stop(&mut self) {
        // Safe against double stop / stop before start (same shape as PwSystemBackend::stop).
        if !self.running.swap(false, Ordering::SeqCst) {
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
            self.stop_tx = None;
            return;
        }

        // Notify the loop thread to stop (the receiver callback calls loop.quit()).
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(Terminate);
        }

        // Wait for the thread to exit run() and terminate. On exit,
        // Stream→Registry→Core→Context→MainLoop are destroyed in drop order (all on the loop
        // thread).
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for PwProcessBackend {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Body of the PipeWire loop thread for process capture.
///
/// Creates, runs, and destroys `MainLoop`/`Context`/`Core`/`Registry`/`Stream` (all `!Send`)
/// only inside this function. Reports setup completion/failure to the caller via `ready_tx`,
/// and on success spins in `main_loop.run()` until the stop instruction ([`Terminate`]). It
/// waits for the target PID's node in the registry, and creates the link-factory links as the
/// target output ports and our own input ports arrive.
/// `select` switches between Include (record only the target PID) and Exclude (record everything
/// but the target PID).
fn run_pw_process_loop(
    select: PidSelect,
    sink: RawSink,
    stop_rx: pw::channel::Receiver<Terminate>,
    ready_tx: &mpsc::Sender<std::result::Result<(), String>>,
) {
    // Setup (connection, stream creation, registry listener registration) is a separate
    // function. The return value is kept alive for the whole run (dropping it stops the
    // watch/links).
    let (main_loop, _keep) = match setup_pw_process(select, sink) {
        Ok(t) => t,
        Err(msg) => {
            // Report the setup failure and exit (no panic).
            let _ = ready_tx.send(Err(msg));
            return;
        }
    };

    // Attach the stop channel's receiver to the loop. quit() on receiving Terminate.
    // quit() is called inside a loop-driven callback, i.e. from this thread.
    let main_loop_for_quit = main_loop.clone();
    let _attached = stop_rx.attach(main_loop.loop_(), move |_terminate| {
        main_loop_for_quit.quit();
    });

    // Report setup success. From here run() blocks and waits for the target PID to appear.
    if ready_tx.send(Ok(())).is_err() {
        // The caller is gone (start already dropped, etc.). Do not start.
        return;
    }

    // Spins until Terminate is received or the process exits. It waits here while the target PID
    // has not appeared, and the registry callback does the linking.
    main_loop.run();
    // On exit, drops happen in the order _attached → _keep
    // (listener→stream→registry→core→main_loop), and PipeWire resources are destroyed on this
    // thread.
}

/// Things owned for the whole run of a process capture. Dropping them stops the capture.
///
/// - `CoreRc`: the subject of `core.create_object("link-factory", ...)`. Shared via `Rc` so the
///   registry callback can create links, and placed last in drop order.
/// - `StreamRc`: our own capture stream (connected with `Direction::Input`; it has input ports,
///   and data flows in once links to the target output ports are established).
/// - `StreamListener`: param_changed/process callback registration. Removed on drop.
/// - `RegistryRc`: the registry proxy itself.
/// - `Registry Listener`: global/global_remove listener (removed on drop).
/// - `links`: map grouping the [`pw::link::Link`] proxies created by link-factory by the
///   registry global id of the linked output node ([`NodeLinks`], keyed by port pair). Dropping
///   them cuts the links, so they are kept alive on the loop thread. The registry callback inserts / removes / clears here, so it
///   is shared as `Rc<RefCell<…>>`. Include has at most 1 entry, Exclude has many (dropping the
///   whole map cuts every link).
#[allow(clippy::type_complexity)]
struct ProcessKeep {
    _stream: pw::stream::StreamRc,
    _listener: pw::stream::StreamListener<UserData>,
    _registry: pw::registry::RegistryRc,
    _registry_listener: pw::registry::Listener,
    _links: std::rc::Rc<std::cell::RefCell<std::collections::HashMap<u32, NodeLinks>>>,
    _core: pw::core::CoreRc,
}

/// Registration info of one watched Stream/Output/Audio node (picked up from registry globals).
///
/// In PipeWire the PID lives not on the node but in the Client object's `pipewire.sec.pid`.
/// A node normally has no PID and only points to its owning Client via `client.id`. So PID
/// resolution has two stages (node → client.id → the Client's PID). If the node itself carries a
/// PID, it is kept in `app_pid` (in preparation for future setups).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct NodeEntry {
    /// Registry global id of the Client that owns this node (`client.id` in the node props).
    /// May be absent (then neither `app_pid` nor client_pid resolution matches).
    owning_client_id: Option<u32>,
    /// The PID, if the node's own props carried one (normally `None`; in preparation for a
    /// future setup where PipeWire puts the PID on the node).
    app_pid: Option<u32>,
}

/// Registration info of one port (picked up from the registry's `ObjectType::Port` globals).
///
/// Both the target output node's output ports (`direction == "out"`) and our own capture
/// stream's input ports (`direction == "in"`) are accumulated here and linked by matching the
/// channel name (`audio.channel`).
#[derive(Clone, Debug, PartialEq, Eq)]
struct PortEntry {
    /// Registry global id of the node that owns this port (`node.id` in the port props).
    node_id: u32,
    /// Direction (`"out"` = output port / `"in"` = input port).
    direction: String,
    /// Audio channel name (`"FL"` / `"FR"` / `"MONO"`, etc.). Empty if absent.
    channel: String,
}

/// Matches output ports to input ports by channel and returns the pairs of links to create
/// (PipeWire-independent, arrival-order-independent). Builds `(out_port_id, in_port_id)` from a
/// set of `(out_port_id, channel)` output ports and a set of `(in_port_id, channel)` input ports.
///
/// Matching rules:
/// 1. Prefer matching channel names (FL→FL / FR→FR / MONO→MONO, etc.).
/// 2. Mono output duplication: if there is 1 output port (typically MONO) and several inputs,
///    that single output is duplicated to every input port (mono → both FL/FR).
/// 3. Order fallback: when channel names are unavailable/do not match, the remaining output
///    ports and input ports are matched best-effort by their order.
///
/// Returns a list of link pairs without duplicates. An empty `Vec` if none can be made.
fn pair_ports(out_ports: &[(u32, String)], in_ports: &[(u32, String)]) -> Vec<(u32, u32)> {
    let mut pairs: Vec<(u32, u32)> = Vec::new();

    // Record the input ports already matched (never double-link to the same input port).
    let mut used_in: Vec<bool> = vec![false; in_ports.len()];

    // Prefer channel-name matches. For each output port, find an unused input port with the same
    // non-empty channel name.
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

    // Mono output duplication. If there is only 1 output port and unmatched input ports remain,
    // duplicate that single output to all the remaining inputs (mono → both FL/FR, etc.).
    // Inputs already matched by channel are excluded.
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

    // Order fallback. Match the output ports not matched by channel name (including empty
    // channels) to the remaining input ports in order.
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

/// Links one linked output node currently holds, keyed by the `(out_port_id, in_port_id)` pair
/// each one connects. Dropping a [`pw::link::Link`] cuts that link.
type NodeLinks = std::collections::HashMap<(u32, u32), pw::link::Link>;

/// The difference between the links a node has and the links its current port plan wants
/// ([`plan_link_changes`]).
#[derive(Debug, PartialEq, Eq)]
struct LinkChanges {
    /// Pairs that are linked but no longer in the plan (cut them).
    remove: Vec<(u32, u32)>,
    /// Pairs that are in the plan but not linked yet (create them), in plan order.
    add: Vec<(u32, u32)>,
}

/// Reconciles the pairs a node is linked with (`current`) against the pairs [`pair_ports`]
/// wants now (`wanted`) (PipeWire-independent, arrival-order-independent).
///
/// Ports reach the registry one global at a time, so the first plan for a node can be partial:
/// our own input may have only FL yet (FL→FL alone, FR never linked), or the target may have
/// only FL yet (mono duplication FL→FL/FR). Re-planning on every arrival and applying only this
/// difference converges on the full plan while leaving the links that are already right in
/// place (no gap in the audio).
fn plan_link_changes(current: &[(u32, u32)], wanted: &[(u32, u32)]) -> LinkChanges {
    let mut remove: Vec<(u32, u32)> = current
        .iter()
        .filter(|pair| !wanted.contains(pair))
        .copied()
        .collect();
    remove.sort_unstable();
    let add = wanted
        .iter()
        .filter(|pair| !current.contains(pair))
        .copied()
        .collect();
    LinkChanges { remove, add }
}

/// Resolves a node's PID (PipeWire-independent, arrival-order-independent).
///
/// If the node itself has a PID, uses it; otherwise looks up the owning Client via `client.id`
/// and resolves it from the `client_pid` table (Client global id → that Client's
/// `pipewire.sec.pid`). Client and Node may arrive in either order; re-evaluating with this
/// function on each global arrival yields `Some(pid)` once both are present.
fn resolve_node_pid(
    entry: &NodeEntry,
    client_pid: &std::collections::HashMap<u32, u32>,
) -> Option<u32> {
    if let Some(pid) = entry.app_pid {
        // Future setup where the PID is put directly on the node. Settled without the Client.
        return Some(pid);
    }
    // Normal path: client.id → the Client's PID.
    let client_id = entry.owning_client_id?;
    client_pid.get(&client_id).copied()
}

/// Node name of our own capture stream. A unique name used to look up our own input ports in
/// the registry; the target PID is embedded to avoid collisions.
fn capture_node_name(target_pid: u32) -> String {
    format!("flexaudio-capture-{target_pid}")
}

/// Node-selection predicate of the process capture loop.
///
/// Handles the three paths Include / Exclude / exclude_self with one fan-in link mechanism. The
/// contained `u32` is in every case the PID to compare against; Include links on a match, and
/// Exclude links on a mismatch (leaving that PID out).
#[derive(Clone, Copy, PartialEq, Eq)]
enum PidSelect {
    /// Link only nodes whose resolved PID == this PID (Include; one representative node).
    Include(u32),
    /// Link every `Stream/Output/Audio` node whose resolved PID != this PID
    /// (Exclude / exclude_self). The contained PID is the PID of the process excluded from
    /// recording.
    Exclude(u32),
}

impl PidSelect {
    /// The PID to compare against (the recorded side for Include, the excluded side for
    /// Exclude). Used by `global_remove` to detect that the target/excluded Client disappeared.
    fn pid(self) -> u32 {
        match self {
            PidSelect::Include(p) | PidSelect::Exclude(p) => p,
        }
    }
}

/// The full setup for process capture. Failures are `Err(String)` (no panic).
///
/// Differences from [`setup_pw`] (system monitor):
/// - Neither `STREAM_CAPTURE_SINK` nor `AUTOCONNECT` is set (preventing auto-linking to the
///   microphone so only explicit links exist). `node.name` gets a unique name
///   ([`capture_node_name`]) so our own input ports can be looked up in the registry.
/// - `stream.connect(Direction::Input, None, ...)` is called exactly once here. This creates
///   the input ports (input_FL/FR), but no data arrives until they are linked (once a link is
///   established: format negotiation → data flows in).
/// - The registry's `global` stays subscribed, tracking Client / Node / Port. The PID is always
///   present in the Client's `pipewire.sec.pid` (`*pw::keys::SEC_PID`) (the daemon sets it from
///   the socket credentials, so it cannot be spoofed; confirmed on a stock real-device setup).
///   A node only points to its Client via `client.id`, so PID matching has two stages (node →
///   client.id → the Client's PID; [`resolve_node_pid`]). Whether the Client or the Node comes
///   first, it is re-evaluated on each global arrival.
/// - The `select` ([`PidSelect`]) predicate decides which nodes to link. Include: the one
///   Stream/Output/Audio node belonging to the target PID; Exclude: every Stream/Output/Audio
///   node with a resolved PID other than the excluded PID (nodes with an unresolved PID are
///   deferred until their Client arrives). Once a target node has an output port and our own
///   node has an input port, the registry callback (running on the loop thread) creates
///   channel-matched links ([`pair_ports`]: FL→FL/FR→FR, mono duplicated) with
///   `core.create_object::<pw::link::Link>("link-factory", ...)`. Ports arrive one global at a
///   time, so every later arrival re-plans the linked nodes too and applies only the difference
///   ([`plan_link_changes`]); a first plan made from part of the ports is never latched. Links
///   are kept per node in the `linked` (node_id → [`NodeLinks`]) map.
/// - When `global_remove` detects that an individually linked node / its output port
///   disappeared, only that node's entry is dropped (under Exclude, the other nodes' links are
///   kept); when our own node / own input port / the target Client disappears, all entries are
///   dropped and it waits again (either way it can relink idempotently).
///
/// Key constants used (all confirmed to be outside feature gates in the crate's `keys.rs`):
/// `*pw::keys::SEC_PID`(="pipewire.sec.pid"), `*pw::keys::CLIENT_ID`(="client.id"),
/// `*pw::keys::NODE_ID`(="node.id"), `*pw::keys::PORT_DIRECTION`(="port.direction"),
/// `*pw::keys::AUDIO_CHANNEL`(="audio.channel"), `*pw::keys::LINK_OUTPUT_NODE`/
/// `LINK_OUTPUT_PORT`/`LINK_INPUT_NODE`/`LINK_INPUT_PORT`.
#[allow(clippy::type_complexity)]
fn setup_pw_process(
    select: PidSelect,
    sink: RawSink,
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

    // Properties of the input (capture) stream.
    // - media.type=Audio / media.category=Capture: audio capture stream
    // - media.class=Stream/Input/Audio: role in the graph (input = the recording side)
    // - media.role=Music: hint
    // - node.name=flexaudio-capture-<pid>: unique name used to look up our own input ports in
    //   the registry
    // Neither STREAM_CAPTURE_SINK nor AUTOCONNECT is set (preventing auto-linking to the
    // microphone so only explicit link-factory links exist). node.name embeds select's compare
    // PID to avoid collisions.
    let node_name = capture_node_name(select.pid());
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
    };
    // Callback registration (shared helper; same param_changed/process behavior as the system
    // path).
    let listener = add_capture_listener(&stream, user_data)?;

    // Connect our own stream exactly once (Direction::Input, target=None, no AUTOCONNECT).
    // This creates the input ports (input_FL/FR). No data arrives until they are linked
    // (once a link is established: format negotiation → data flows in). The format POD is
    // F32LE/48000/2ch.
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

    // Registry global id of our own node (used to look up our own input ports by `node.id`).
    // Right after connect it may still be unset (0), but it is settled by the time the input
    // ports appear in the registry. Re-read with stream.node_id() on every Port arrival.
    let self_node_id: Rc<Cell<Option<u32>>> = Rc::new(Cell::new(None));

    // State tables. Registry callbacks are only ever called from the one loop thread, so
    // Cell/RefCell is enough for interior mutability (no Mutex needed).

    // Watched-node table: registry node global id → registration info (owning client.id / direct
    // PID).
    let nodes: Rc<RefCell<HashMap<u32, NodeEntry>>> = Rc::new(RefCell::new(HashMap::new()));
    // Client table: the Client's registry global id → that Client's pipewire.sec.pid.
    let client_pid: Rc<RefCell<HashMap<u32, u32>>> = Rc::new(RefCell::new(HashMap::new()));
    // Registry global id of the Client of the compare PID (the recorded PID for Include / the
    // excluded PID for Exclude) (Some once known). Used by global_remove to detect that the
    // target/excluded Client disappeared.
    let target_client_id: Rc<Cell<Option<u32>>> = Rc::new(Cell::new(None));
    // Port table: registry port global id → registration info (owning node.id / direction /
    // channel).
    let ports: Rc<RefCell<HashMap<u32, PortEntry>>> = Rc::new(RefCell::new(HashMap::new()));
    // Table of currently linked output nodes: the output node's registry global id → the Link
    // proxies created for that node. Dropping them cuts the links, so they are kept for the
    // whole run. Include has at most 1 entry, Exclude has many. Links can be cut individually by
    // removing an entry, or all at once by clearing the map.
    let linked: Rc<RefCell<HashMap<u32, NodeLinks>>> = Rc::new(RefCell::new(HashMap::new()));

    // Each time the state is updated, re-plan the output nodes that should be linked and bring
    // each one's links in line with the plan: channel-matched links from the output ports and
    // our own input ports present right now, created with link-factory.
    // The `select` predicate decides the set of target nodes:
    // - Include(pid): nodes whose resolved PID == pid (only one representative node; once one
    //   is linked, only that node is re-planned, i.e. it stays a single node).
    // - Exclude(pid): every `Stream/Output/Audio` node whose resolved PID != pid. Nodes whose PID
    //   is unresolved (None) are not linked yet (wait until the Client arrives and resolves the
    //   PID, so the excluded process is never mistaken).
    // A node that is already linked is re-planned rather than linked again: only the difference
    // from its current links is applied ([`plan_link_changes`]), so a plan made while ports were
    // still arriving grows into the full one and a pair is never linked twice.
    // Called on the loop thread (the `!Send` core/stream may be touched).
    #[allow(clippy::too_many_arguments)]
    fn try_link(
        core: &pw::core::CoreRc,
        stream: &pw::stream::StreamRc,
        select: PidSelect,
        self_node_id: &Cell<Option<u32>>,
        nodes: &RefCell<HashMap<u32, NodeEntry>>,
        client_pid: &RefCell<HashMap<u32, u32>>,
        ports: &RefCell<HashMap<u32, PortEntry>>,
        linked: &RefCell<HashMap<u32, NodeLinks>>,
    ) {
        // Re-read our own node id from the stream (it may be unset right after connect).
        // When unset, SPA_ID_INVALID(=ID_ANY=u32::MAX) or 0 is returned.
        let sid = stream.node_id();
        if sid != 0 && sid != pw::constants::ID_ANY {
            self_node_id.set(Some(sid));
        }
        let Some(self_nid) = self_node_id.get() else {
            return;
        };

        // Decide the set of output node ids to plan with the predicate.
        // - Include: exactly one node — the linked one if there is one, otherwise one whose
        //   resolved PID == pid.
        // - Exclude: every node with a resolved PID (!= pid) (unresolved PIDs excluded), linked
        //   or not.
        let targets: Vec<u32> = {
            let nodes = nodes.borrow();
            let client_pid = client_pid.borrow();
            let linked = linked.borrow();
            match select {
                PidSelect::Include(_) if !linked.is_empty() => linked.keys().copied().collect(),
                PidSelect::Include(pid) => nodes
                    .iter()
                    .find(|(_id, entry)| resolve_node_pid(entry, &client_pid) == Some(pid))
                    .map(|(&node_id, _)| node_id)
                    .into_iter()
                    .collect(),
                PidSelect::Exclude(pid) => nodes
                    .iter()
                    .filter(|(_id, entry)| {
                        // Target only when resolved and not the excluded PID. Unresolved (None)
                        // is deferred until the Client arrives (never mistake the excluded
                        // process).
                        matches!(resolve_node_pid(entry, &client_pid), Some(other) if other != pid)
                    })
                    .map(|(&node_id, _)| node_id)
                    .collect(),
            }
        };

        if targets.is_empty() {
            return;
        }

        // Look up our own input ports in the ports table (shared by all target nodes).
        let in_ports: Vec<(u32, String)> = {
            let ports = ports.borrow();
            ports
                .iter()
                .filter(|(_pid, p)| p.node_id == self_nid && p.direction == "in")
                .map(|(&pid, p)| (pid, p.channel.clone()))
                .collect()
        };
        // Cannot link while our own input ports are missing (re-evaluated on the next global).
        if in_ports.is_empty() {
            return;
        }

        for target_node_id in targets {
            // Look up the target node's output ports in the ports table.
            let out_ports: Vec<(u32, String)> = {
                let ports = ports.borrow();
                ports
                    .iter()
                    .filter(|(_pid, p)| p.node_id == target_node_id && p.direction == "out")
                    .map(|(&pid, p)| (pid, p.channel.clone()))
                    .collect()
            };
            // If the output ports have not appeared, this node cannot be linked yet (re-evaluated
            // next time).
            if out_ports.is_empty() {
                continue;
            }

            // Build pairs by channel (FL→FL/FR→FR, mono duplicated, order if unavailable).
            let pairs = pair_ports(&out_ports, &in_ports);
            if pairs.is_empty() {
                continue;
            }

            // Take this node's current links out of the map (empty if not linked yet), so no
            // borrow of `linked` is held while creating links.
            let mut node_links = linked
                .borrow_mut()
                .remove(&target_node_id)
                .unwrap_or_default();
            let current: Vec<(u32, u32)> = node_links.keys().copied().collect();
            let changes = plan_link_changes(&current, &pairs);

            // Cut the links the plan no longer wants (e.g. a mono duplication FL→FR made before
            // the target's FR port appeared).
            for pair in &changes.remove {
                node_links.remove(pair);
            }

            // Link each missing pair with link-factory.
            let mut all_created = true;
            for &(out_port_id, in_port_id) in &changes.add {
                let link_props = properties! {
                    *pw::keys::LINK_OUTPUT_NODE => target_node_id.to_string(),
                    *pw::keys::LINK_OUTPUT_PORT => out_port_id.to_string(),
                    *pw::keys::LINK_INPUT_NODE => self_nid.to_string(),
                    *pw::keys::LINK_INPUT_PORT => in_port_id.to_string(),
                };
                match core.create_object::<pw::link::Link>("link-factory", &link_props) {
                    Ok(link) => {
                        node_links.insert((out_port_id, in_port_id), link);
                    }
                    Err(_e) => {
                        // Creating this pair's link failed. Skip the rest to avoid a partial link.
                        all_created = false;
                        break;
                    }
                }
            }

            // Keep the node linked only when its links match the whole plan. Keeping a partial
            // link where only one channel succeeded (e.g. FL connected, FR dropped) would
            // effectively lock the target to mono. If a pair failed, drop every Link of this
            // node (node_links goes out of scope), leave it unlinked, and re-evaluate on the next
            // global arrival (this retries when a link dropped temporarily). Processing of the
            // other target nodes continues.
            if all_created {
                linked.borrow_mut().insert(target_node_id, node_links);
            }
        }
    }

    // Registry global / global_remove listeners.
    // global: registers Client→client_pid table / Stream/Output/Audio node→nodes table /
    // Port→ports table, and re-evaluates the links with try_link every time.
    let core_for_global = core.clone();
    let stream_for_global = stream.clone();
    let self_node_for_global = self_node_id.clone();
    let nodes_for_global = nodes.clone();
    let client_pid_for_global = client_pid.clone();
    let target_client_for_global = target_client_id.clone();
    let ports_for_global = ports.clone();
    let linked_for_global = linked.clone();

    let core_for_remove = core.clone();
    let stream_for_remove = stream.clone();
    let self_node_for_remove = self_node_id.clone();
    let nodes_for_remove = nodes.clone();
    let client_pid_for_remove = client_pid.clone();
    let target_client_for_remove = target_client_id.clone();
    let ports_for_remove = ports.clone();
    let linked_for_remove = linked.clone();

    let _registry_listener = registry
        .add_listener_local()
        .global(move |global| {
            // A panic across FFI is UB, so wrap the body in catch_unwind.
            let _ = catch_unwind(AssertUnwindSafe(|| {
                let Some(props) = global.props else {
                    return;
                };
                match global.type_ {
                    pw::types::ObjectType::Client => {
                        // The PID is always present in the Client's pipewire.sec.pid (set by
                        // the daemon, so it cannot be spoofed).
                        let Some(pid_str) = props.get(*pw::keys::SEC_PID) else {
                            return;
                        };
                        let Ok(pid) = pid_str.parse::<u32>() else {
                            return;
                        };
                        client_pid_for_global.borrow_mut().insert(global.id, pid);
                        // Remember the compare PID's Client (used by global_remove to detect its
                        // removal).
                        if pid == select.pid() {
                            target_client_for_global.set(Some(global.id));
                        }
                    }
                    pw::types::ObjectType::Node => {
                        // Target only app output nodes (playback streams).
                        let media_class = props.get(*pw::keys::MEDIA_CLASS).unwrap_or("");
                        if media_class != "Stream/Output/Audio" {
                            return;
                        }
                        // client.id pointing to the owning Client.
                        let owning_client_id = props
                            .get(*pw::keys::CLIENT_ID)
                            .and_then(|s| s.parse::<u32>().ok());
                        // If the node itself carries a PID, it can be matched directly (future
                        // setups).
                        let app_pid = props
                            .get(*pw::keys::SEC_PID)
                            .and_then(|s| s.parse::<u32>().ok());
                        nodes_for_global.borrow_mut().insert(
                            global.id,
                            NodeEntry {
                                owning_client_id,
                                app_pid,
                            },
                        );
                    }
                    pw::types::ObjectType::Port => {
                        // Accumulate ports (both target output ports and own input ports are looked
                        // up here).
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

                // Whichever of Client / Node / Port arrived, the state changed, so re-evaluate.
                // This is on the loop thread, so the `!Send` core/stream may be touched.
                try_link(
                    &core_for_global,
                    &stream_for_global,
                    select,
                    &self_node_for_global,
                    &nodes_for_global,
                    &client_pid_for_global,
                    &ports_for_global,
                    &linked_for_global,
                );
            }));
        })
        .global_remove(move |id| {
            // A panic across FFI is UB, so wrap the body in catch_unwind.
            let _ = catch_unwind(AssertUnwindSafe(|| {
                // Remove the vanished id from the tables according to its kind, and review the
                // link state. To avoid borrow conflicts, first settle what to do as bool / owner
                // in a scoped borrow, then modify linked and call try_link.
                let mut relink_needed = false;

                // Whether the vanished id is a linked node / the target or excluded Client / our
                // own node.
                let was_linked_node = linked_for_remove.borrow().contains_key(&id);
                let was_target_client = target_client_for_remove.get() == Some(id);
                // Whether our own node (the node of our capture stream) itself vanished.
                let was_self_node = self_node_for_remove.get() == Some(id);

                // If the vanished id is an output port belonging to one of the linked nodes, find
                // its owning node id. Also determine whether an input port belonging to our own
                // node vanished. Missing the removal of our own input port would leave it stuck
                // as linked while the input is gone, never recovering from silence. So that
                // ports.borrow() is not held across the try_link call, compute owner / bool
                // inside this scope before leaving it.
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

                // Removal of our own node / own input port / the target Client releases all
                // links at once and leaves it to re-evaluation.
                // - Own node / own input port: the input side is gone, so every link is invalid.
                // - Target/excluded Client: under Include, all of that PID's nodes vanish (the
                //   recording target is gone). Under Exclude, releasing all → relinking also
                //   gives the correct result (the excluded Client's nodes are removed from the
                //   nodes table afterwards, so they are not relinked; only the kept side is
                //   relinked).
                if was_self_node || was_self_in_port || was_target_client {
                    // Drop every held Link (= unlink) and return to unlinked.
                    linked_for_remove.borrow_mut().clear();
                    relink_needed = true;
                } else {
                    // Release only the individual node that vanished (Exclude keeps the other
                    // nodes' links).
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
                    target_client_for_remove.set(None);
                }
                if was_self_node {
                    // If our own node vanished, clear the id cache. try_link re-reads it from
                    // the stream and can pick up the new id on re-creation.
                    self_node_for_remove.set(None);
                }

                // Remove the vanished id from each table (so pid/port resolution never sees stale
                // values).
                nodes_for_remove.borrow_mut().remove(&id);
                client_pid_for_remove.borrow_mut().remove(&id);
                ports_for_remove.borrow_mut().remove(&id);

                // Once back to waiting, try relinking immediately if another target is already
                // complete.
                if relink_needed {
                    try_link(
                        &core_for_remove,
                        &stream_for_remove,
                        select,
                        &self_node_for_remove,
                        &nodes_for_remove,
                        &client_pid_for_remove,
                        &ports_for_remove,
                        &linked_for_remove,
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
        },
    ))
}

/// State shared between the `process` callback and `param_changed`.
///
/// Holds the settled format (channels) so that `process` can refer to it.
struct UserData {
    /// The capture format settled by PipeWire. Updated in `param_changed`.
    format: spa::param::audio::AudioInfoRaw,
    /// Where raw frames are streamed. `process` pushes to it via `&mut`.
    sink: RawSink,
}

/// Registers the `param_changed` / `process` callbacks on a capture stream.
///
/// [`PwSystemBackend`] (system monitor) and [`PwProcessBackend`] (process fan-out) use the same
/// callback behavior, so this is a shared helper. `param_changed` records the settled format, and
/// `process` streams the dequeued interleaved f32 to [`RawSink::push`] without blocking.
///
/// Returns the registered [`StreamListener`](pw::stream::StreamListener) (dropping it removes
/// the callbacks, so the caller keeps it for the whole run).
fn add_capture_listener(
    stream: &pw::stream::StreamRc,
    user_data: UserData,
) -> std::result::Result<pw::stream::StreamListener<UserData>, String> {
    // Pre-allocate, at stream setup time (on this loop thread), the thread-local scratch that the
    // RT process callback uses to repack f32, up to the maximum expected block length. This
    // avoids reserve inside process (an RT allocation = xrun risk) in steady state. setup_pw /
    // setup_pw_process call this function after registration, so reserve happens once, in the
    // non-RT setup phase.
    PROC_SCRATCH.with(|cell| {
        let mut s = cell.borrow_mut();
        let cap = s.capacity();
        if cap < PROC_SCRATCH_CAP {
            s.reserve(PROC_SCRATCH_CAP - cap);
        }
    });

    stream
        .add_local_listener_with_user_data(user_data)
        .param_changed(|_stream, user_data, id, param| {
            // A panic across FFI is UB, so wrap the body in catch_unwind.
            let _ = catch_unwind(AssertUnwindSafe(|| {
                // NULL means the format is cleared.
                let Some(param) = param else {
                    return;
                };
                if id != pw::spa::param::ParamType::Format.as_raw() {
                    return;
                }
                let (media_type, media_subtype) = match format_utils::parse_format(param) {
                    Ok(v) => v,
                    Err(_) => return,
                };
                // Accept raw audio only.
                if media_type != MediaType::Audio || media_subtype != MediaSubtype::Raw {
                    return;
                }
                // Record the settled format (process uses it as the channel count).
                if user_data.format.parse(param).is_err() {
                    // On parse failure, do not update (keep the previous value).
                }
            }));
        })
        .process(|stream, user_data| {
            // Called on the RT thread. Avoid blocking and allocation.
            // A panic across FFI is UB, so wrap the body in catch_unwind.
            let _ = catch_unwind(AssertUnwindSafe(|| {
                // Do nothing if there is no buffer (no panic).
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                let datas = buffer.datas_mut();
                if datas.is_empty() {
                    return;
                }
                let data = &mut datas[0];
                // Record the valid byte count and offset (position in the ring) before borrowing
                // data().
                let chunk = data.chunk();
                let size = chunk.size() as usize;
                let offset = chunk.offset() as usize;
                if size == 0 {
                    return;
                }
                let Some(bytes) = data.data() else {
                    return;
                };
                // [offset, offset+size) is the valid region. Reject out-of-range (defensive).
                let end = offset.saturating_add(size);
                if end > bytes.len() {
                    return;
                }
                let valid = &bytes[offset..end];
                // Take only a multiple of f32 (ignore leftover bytes).
                let n_floats = valid.len() / std::mem::size_of::<f32>();
                if n_floats == 0 {
                    return;
                }
                // Read the bytes as interleaved f32. The alignment of `data` is not guaranteed,
                // so read with from_le_bytes rather than align_to. Pack into the pre-allocated
                // reused buffer, then push once (RawSink::push is non-blocking and DROPs when
                // full).
                PROC_SCRATCH.with(|cell| {
                    let mut scratch = cell.borrow_mut();
                    // If pre-allocated (PROC_SCRATCH_CAP), reserve is a no-op in steady state and
                    // no RT allocation happens. It grows once only for a block larger than
                    // expected (and keeps that capacity afterwards).
                    let cap = scratch.capacity();
                    if n_floats > cap {
                        scratch.reserve(n_floats - cap);
                    }
                    scratch.clear();
                    for i in 0..n_floats {
                        let b = i * 4;
                        let v = f32::from_le_bytes([
                            valid[b],
                            valid[b + 1],
                            valid[b + 2],
                            valid[b + 3],
                        ]);
                        scratch.push(v);
                    }
                    // PTS: currently substituted by the monotonic clock at arrival
                    // (`monotonic_now_ns`). The downstream ClockNormalizer takes the first value
                    // as origin, so a monotonic approximation does not break anything.
                    // It can later be replaced by the device clock of `pw_buffer.time`.
                    user_data.sink.push(&scratch, monotonic_now_ns());
                });
            }));
        })
        .register()
        .map_err(|e| format!("register pipewire stream listener failed: {e}"))
}

/// Builds the bytes of the requested format POD (f32 / 48000 / 2ch).
///
/// rate/channels are explicit, so if the graph differs PipeWire automatically inserts
/// `audioconvert` to convert to 48k/stereo/f32. The POD is made from the returned bytes with
/// [`Pod::from_bytes`] (the bytes are what the POD points to, so keep them alive until the
/// connect call).
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

/// Body of the PipeWire loop thread.
///
/// Creates, runs, and destroys `MainLoop`/`Context`/`Core`/`Stream` (all `!Send`) only inside
/// this function, never letting them cross the thread boundary. Reports setup completion/failure
/// to the caller via `ready_tx`, and on success spins in `main_loop.run()` until told to stop.
fn run_pw_loop(
    device_id: Option<String>,
    sink: RawSink,
    stop_rx: pw::channel::Receiver<Terminate>,
    ready_tx: &mpsc::Sender<std::result::Result<(), String>>,
) {
    // Setup is a separate function. The return value is owned for the whole run (dropping it
    // stops).
    let (main_loop, _stream, _listener) = match setup_pw(device_id, sink) {
        Ok(t) => t,
        Err(msg) => {
            // Report the setup failure and exit (no panic).
            let _ = ready_tx.send(Err(msg));
            return;
        }
    };

    // Attach the stop channel's receiver to the loop. quit() on receiving Terminate. attach only
    // borrows this local `main_loop`, so the returned AttachedReceiver stays within this stack
    // frame (no self-referential struct and no unsafe lifetime extension needed). quit() is
    // called inside a loop-driven callback, i.e. from this thread.
    let main_loop_for_quit = main_loop.clone();
    let _attached = stop_rx.attach(main_loop.loop_(), move |_terminate| {
        main_loop_for_quit.quit();
    });

    // Report setup success. From here run() blocks.
    if ready_tx.send(Ok(())).is_err() {
        // The caller is gone (start already dropped, etc.). Do not start.
        return;
    }

    // Spins until Terminate is received or the process exits.
    main_loop.run();
    // On exit, drops happen in the order _attached → _listener → _stream → main_loop (reverse
    // declaration order), and PipeWire resources are destroyed on this thread.
}

/// The full PipeWire setup. Failures are `Err(String)` (no panic).
///
/// If `device_id` is `Some(node.name)`, targets that sink via `target.object` (`None` is the
/// default sink). The sink's existence has already been checked before the call (in `start`).
///
/// Returns handles kept alive for the whole run:
/// - `MainLoopRc`: the subject of `run()`/`quit()`
/// - `StreamRc`: the capture stream itself
/// - `StreamListener`: callback registration. Removed on drop
///
/// Attaching the stop channel to the loop is done by the caller ([`run_pw_loop`]). That way
/// `AttachedReceiver` does not become a self-referential struct borrowing the returned tuple
/// (which contains the `MainLoopRc`).
#[allow(clippy::type_complexity)]
fn setup_pw(
    device_id: Option<String>,
    sink: RawSink,
) -> std::result::Result<
    (
        pw::main_loop::MainLoopRc,
        pw::stream::StreamRc,
        pw::stream::StreamListener<UserData>,
    ),
    String,
> {
    // pw::init only once for the whole process (Once prevents thread races).
    pw_init_once();

    let main_loop = pw::main_loop::MainLoopRc::new(None)
        .map_err(|e| format!("create pipewire main loop failed: {e}"))?;
    let context = pw::context::ContextRc::new(&main_loop, None)
        .map_err(|e| format!("create pipewire context failed: {e}"))?;
    // Connect to the default PipeWire daemon. Err here if the daemon is absent.
    let core = context
        .connect_rc(None)
        .map_err(|e| format!("connect to pipewire daemon failed (is PipeWire running?): {e}"))?;

    // Properties of the input (capture) stream.
    // - media.type=Audio / media.category=Capture: audio capture stream
    // - media.class=Stream/Input/Audio: role in the graph (input = the recording side)
    // - stream.capture.sink=true: record the sink's monitor (system audio output), not a
    //   recording device
    // - media.role: hint for autoconnect to the default sink
    let mut props = properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_CLASS => "Stream/Input/Audio",
        *pw::keys::MEDIA_ROLE => "Music",
    };
    // Request recording the monitor (the sink's output = system audio).
    props.insert(*pw::keys::STREAM_CAPTURE_SINK, "true");
    // If device_id is specified, target that sink via target.object (node.name). autoconnect is
    // kept, but with target.object WirePlumber connects to this sink's monitor instead of the
    // default one. The target argument of stream.connect (the None below) was once ignored by
    // WirePlumber, so it is not used; the props setting here is used instead. A missing sink is
    // already rejected in start, so it is not checked here.
    // pw::keys::TARGET_OBJECT is under the crate's v0_3_44 feature, so the key string is written
    // directly (other feature-gated keys are also specified as strings).
    if let Some(id) = device_id {
        props.insert("target.object", id);
    }

    let stream = pw::stream::StreamRc::new(core, "flexaudio-system-capture", props)
        .map_err(|e| format!("create pipewire capture stream failed: {e}"))?;

    let user_data = UserData {
        format: spa::param::audio::AudioInfoRaw::new(),
        sink,
    };

    // Callback registration. `param_changed` records the settled format, and `process` streams
    // the dequeued buffers to the RawSink (shared helper).
    let listener = add_capture_listener(&stream, user_data)?;

    // Requested format param: f32 / 48000 / 2ch. rate/channels are explicit, so if the graph
    // differs PipeWire automatically inserts audioconvert to convert to 48k/stereo/f32.
    let values = build_format_pod_bytes()?;
    let pod = Pod::from_bytes(&values)
        .ok_or_else(|| "build audio format pod from bytes failed".to_string())?;
    let mut params = [pod];

    // Connect in the input direction. AUTOCONNECT connects to the sink's monitor (the sink given
    // by target.object if specified, otherwise the default sink). MAP_BUFFERS allows reading
    // buffers directly, and RT_PROCESS runs process in RT.
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

/// Capacity (number of f32) pre-allocated for the f32 repacking scratch of `process`. The native
/// request is 48000 Hz / 2ch, so 1 second = 96000. Real-device process blocks are hundreds to
/// thousands of frames (far smaller than 1 second), so with this much allocated, reserve never
/// happens inside RT.
const PROC_SCRATCH_CAP: usize = (NATIVE_RATE as usize) * (NATIVE_CHANNELS as usize);

thread_local! {
    /// Scratch for f32 repacking in the `process` callback. [`add_capture_listener`]
    /// pre-allocates it up to [`PROC_SCRATCH_CAP`] at stream setup time, so no reallocation
    /// happens inside the RT process.
    static PROC_SCRATCH: std::cell::RefCell<Vec<f32>> = const { std::cell::RefCell::new(Vec::new()) };
}

// ============================================================================
// Device enumeration (the Linux/PipeWire part of `devices()`)
// ============================================================================

/// Raw info of one node collected from PipeWire registry global events during enumeration.
///
/// The callbacks write to `!Send` local state, so the values are kept here as owned `String`s
/// and assembled into [`DeviceInfo`] after the enumeration loop ends.
struct NodeRecord {
    /// `node.name`, used as the stable ID (persistent).
    node_name: String,
    /// Display name. `node.description` preferred, otherwise `node.name`.
    description: String,
    /// `media.class` (`"Audio/Sink"` / `"Audio/Source"`, etc.).
    media_class: String,
    /// The rate (Hz), if `audio.rate` could be read.
    rate: Option<u32>,
    /// The channel count, if `audio.channels` could be read.
    channels: Option<u16>,
}

/// Collection target shared across the whole enumeration loop (`!Send`; confined to the loop
/// thread).
#[derive(Default)]
struct EnumState {
    /// Collected Audio/Sink and Audio/Source nodes.
    nodes: Vec<NodeRecord>,
    /// `node.name` of the default sink (from the `default.audio.sink` metadata).
    default_sink: Option<String>,
    /// `node.name` of the default source (from the `default.audio.source` metadata).
    default_source: Option<String>,
}

/// Enumerates audio devices (microphones + system output sinks) via PipeWire.
///
/// Receives one round trip of registry global events and maps
/// - `media.class == "Audio/Sink"` → system audio output (the target whose default-sink monitor
///   is recorded). `is_loopback = true` / `source_kind = SystemLoopback`.
/// - `media.class == "Audio/Source"` → recording devices such as microphones.
///   `is_loopback = false` / `source_kind = Mic`.
///
/// to [`DeviceInfo`]. `id` is the persistent `node.name`, `name` is `node.description`
/// (`node.name` if absent). `sample_rate` / `channels` are the values of `audio.rate` /
/// `audio.channels` if available, otherwise the default `48000 / 2`. For default devices,
/// `is_default = true` is set on the one matching the `node.name` pointed to by the `default`
/// metadata (`default.audio.sink` / `default.audio.source`).
///
/// Runs one short-lived `MainLoop`, detects enumeration completion with the `done` of
/// `core.sync()`, and calls `quit()`. PipeWire daemon absence, connection failure, and registry
/// acquisition failure are swallowed as `Ok(empty Vec)` (no panic; for enumeration this is
/// equivalent to "none").
pub fn list_devices() -> Result<Vec<DeviceInfo>> {
    match enumerate_pw() {
        Ok(v) => Ok(v),
        // Treat daemon absence etc. the same as "nothing to enumerate" (do not break the caller).
        Err(_msg) => Ok(Vec::new()),
    }
}

/// The core of PipeWire registry enumeration. Failures are `Err(String)` (no panic).
///
/// Creates, runs, and destroys `MainLoop`/`Context`/`Core`/`Registry` (all `!Send`) only inside
/// this function. It enumerates with a short-lived loop and finishes immediately, so
/// `list_devices` runs it synchronously on the calling thread without spawning a dedicated
/// thread.
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
    // RegistryRc is cloneable, so it can be moved into the global callback and used for bind.
    let registry = core
        .get_registry_rc()
        .map_err(|e| format!("get pipewire registry failed: {e}"))?;

    let state = Rc::new(RefCell::new(EnumState::default()));
    // Storage that keeps the default metadata's property listeners alive. The Metadata proxy +
    // listener bound inside the global callback are pushed here.
    type MetaKeep = (Box<dyn pw::proxy::ProxyT>, Box<dyn pw::proxy::Listener>);
    let meta_keep: Rc<RefCell<Vec<MetaKeep>>> = Rc::new(RefCell::new(Vec::new()));

    // Registry global listener: collects Audio nodes and the default metadata.
    let state_for_global = state.clone();
    let registry_for_global = registry.clone();
    let meta_keep_for_global = meta_keep.clone();
    let _reg_listener = registry
        .add_listener_local()
        .global(move |global| {
            // A panic across FFI is UB, so wrap the body in catch_unwind.
            let _ = catch_unwind(AssertUnwindSafe(|| {
                let Some(props) = global.props else {
                    return;
                };
                match global.type_ {
                    pw::types::ObjectType::Node => {
                        // Pick up only nodes whose media.class is Audio/Sink|Source.
                        let media_class = props.get(*pw::keys::MEDIA_CLASS).unwrap_or("");
                        if media_class != "Audio/Sink" && media_class != "Audio/Source" {
                            return;
                        }
                        let node_name = props.get(*pw::keys::NODE_NAME).unwrap_or("");
                        if node_name.is_empty() {
                            // A node without a stable key cannot be enumerated (skip).
                            return;
                        }
                        let description = props
                            .get(*pw::keys::NODE_DESCRIPTION)
                            .filter(|s| !s.is_empty())
                            .unwrap_or(node_name);
                        // The key constant for audio.rate is behind a feature gate in the
                        // pipewire crate, so it is given as a string. It is often missing from
                        // the registry's node props; in that case it falls back downstream to
                        // the default (48000/2).
                        let rate = props.get("audio.rate").and_then(|s| s.parse::<u32>().ok());
                        let channels = props
                            .get(*pw::keys::AUDIO_CHANNELS)
                            .and_then(|s| s.parse::<u16>().ok());
                        state_for_global.borrow_mut().nodes.push(NodeRecord {
                            node_name: node_name.to_string(),
                            description: description.to_string(),
                            media_class: media_class.to_string(),
                            rate,
                            channels,
                        });
                    }
                    pw::types::ObjectType::Metadata => {
                        // Bind only the "default" metadata, which holds the default sink/source
                        // (the pipewire crate has no key constant for "metadata.name", so it is
                        // given as a string).
                        let meta_name = props.get("metadata.name").unwrap_or("");
                        if meta_name != "default" {
                            return;
                        }
                        let metadata: pw::metadata::Metadata =
                            match registry_for_global.bind(global) {
                                Ok(m) => m,
                                Err(_) => return,
                            };
                        let state_for_meta = state_for_global.clone();
                        let listener = metadata
                            .add_listener_local()
                            .property(move |_subject, key, _type, value| {
                                // The property callback also crosses FFI, so wrap it in
                                // catch_unwind.
                                catch_unwind(AssertUnwindSafe(|| {
                                    // value is JSON (e.g. {"name":"alsa_output...."}). Extract
                                    // name.
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
        .register();

    // Wait for enumeration to complete with a two-stage sync→done barrier.
    //
    // The first-stage done guarantees that the registry's initial globals have all arrived, but
    // the initial property dump (the default sink/source values) of the default metadata bound
    // during those globals may not have arrived yet (events via a proxy arrive separately). So on
    // receiving the first-stage done, sync once more, and quit on the second-stage done. This
    // exits only after both the global enumeration and the default metadata's properties are in.
    // done always arrives, so this never becomes infinite.
    let done = Rc::new(std::cell::Cell::new(false));
    let stage = Rc::new(std::cell::Cell::new(0u8));
    let pending1 = core
        .sync(0)
        .map_err(|e| format!("pipewire sync failed: {e}"))?;
    let pending1 = Rc::new(std::cell::Cell::new(pending1.seq()));

    let done_for_cb = done.clone();
    let stage_for_cb = stage.clone();
    let pending1_for_cb = pending1.clone();
    let loop_for_cb = main_loop.clone();
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
                    // Stage 1 done → issue the stage-2 sync to wait for the metadata properties.
                    stage_for_cb.set(1);
                    if let Some(core) = core_weak.upgrade() {
                        match core.sync(0) {
                            Ok(p) => pending1_for_cb.set(p.seq()),
                            Err(_) => {
                                // If stage 2 cannot be issued, stop here.
                                done_for_cb.set(true);
                                loop_for_cb.quit();
                            }
                        }
                    } else {
                        done_for_cb.set(true);
                        loop_for_cb.quit();
                    }
                }
                1 if seq == pending1_for_cb.get() => {
                    // Stage 2 done → enumeration finished.
                    done_for_cb.set(true);
                    loop_for_cb.quit();
                }
                _ => {}
            }
        })
        .register();

    // Spin until done is set (= both round trips complete). If run() keeps returning immediately
    // without done (spurious quit, etc.), it would become a tight loop/hang, so give up at the
    // deadline and return what has been collected. Enumeration is best-effort; even if
    // incomplete, it never panics/hangs.
    let deadline = std::time::Instant::now();
    while !done.get() {
        main_loop.run();
        if deadline.elapsed().as_millis() >= ENUMERATE_DEADLINE_MS {
            // Over the limit without done being set. Give up and return what was collected.
            break;
        }
    }

    // Assemble DeviceInfo from the collected raw nodes.
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
            // If unavailable, default to the requested native values (48000/2).
            sample_rate: n.rate.unwrap_or(NATIVE_RATE),
            channels: n.channels.unwrap_or(NATIVE_CHANNELS),
            is_loopback,
            is_default,
        });
    }
    Ok(out)
}

/// Extracts `name` from a PipeWire `default.audio.{sink,source}` metadata value (JSON
/// `{"name":"..."}`). A simple extraction, to avoid adding an external JSON crate. `None` if the
/// value is unexpected.
fn extract_json_name(value: &str) -> Option<String> {
    // Take the first string literal after the `"name"` key. Skip whitespace and the colon.
    let after_key = value.split("\"name\"").nth(1)?;
    let after_colon = after_key.split(':').nth(1)?;
    // Extract from the first `"` to the next `"`.
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
// Device hotplug watch (hotplug notifications / the Linux/PipeWire part of `watch_devices()`)
// ============================================================================

/// Watcher that permanently watches the PipeWire registry and delivers device hotplug events as
/// [`DeviceEvent`]s.
///
/// # Differences from [`PwSystemBackend`] / `enumerate_pw`
///
/// Like [`PwSystemBackend`] it owns a single dedicated thread, but its nature differs:
/// - Permanent, not short-lived: `enumerate_pw` calls `quit()` on the `done` of `core.sync` and
///   exits immediately, whereas this one does not `quit()` even on `done`, keeps spinning, and
///   keeps receiving the registry's `global` / `global_remove` until [`stop`](Self::stop).
/// - No RawSink: it records no audio and looks only at the registry's global/global_remove.
///
/// `MainLoop` / `Context` / `Core` / `Registry` are `!Send`, so they are confined to a dedicated
/// thread (`flexaudio-pw-watch`), and the watcher itself holds only `Send` things (the delivery
/// queue [`Arc<Mutex<VecDeque>>`], the stop flag, the stop [`pipewire::channel::Sender`], and
/// the [`JoinHandle`]).
///
/// # Delivered events
/// - [`DeviceEvent::Added`]: Audio/Sink|Source nodes that appeared after the initial scan
///   completed. Nodes that already existed during the initial scan are only registered, not
///   delivered.
/// - [`DeviceEvent::Removed`]: nodes that vanished while watched (id = `node.name`).
/// - [`DeviceEvent::DefaultChanged`]: default sink / source switches (default metadata watch).
///
/// # PipeWire absent
/// When the PipeWire daemon is absent or the connection fails, [`start`](Self::start) returns
/// [`Error::Backend`] (no panic). The facade layer absorbs this as a no-op fallback (a hotplug
/// watch need not deliver anything if nothing changes). When a PipeWire session exists but is
/// empty, it runs normally.
///
/// ```no_run
/// use flexaudio_os_linux::PwDeviceWatcher;
///
/// // Err if PipeWire is absent (the facade falls back to NoopWatcher).
/// if let Ok(mut watcher) = PwDeviceWatcher::start() {
///     while let Some(ev) = watcher.poll_event() {
///         println!("device event: {ev:?}");
///     }
///     watcher.stop();
/// }
/// ```
pub struct PwDeviceWatcher {
    /// Delivery queue (unbounded, since hotplug is infrequent and must not be dropped). `Send`.
    /// The watch thread's callbacks push, and [`poll_event`](Self::poll_event) pops.
    events: Arc<Mutex<VecDeque<DeviceEvent>>>,
    /// Watching flag (double-start guard / drop check). `Send`.
    running: Arc<AtomicBool>,
    /// Sender that tells the watch thread to stop. `Some` after [`start`](Self::start).
    /// Uses the same [`Terminate`] as [`PwSystemBackend`].
    stop_tx: Option<pw::channel::Sender<Terminate>>,
    /// Handle of the watch thread. `Some` after [`start`](Self::start).
    handle: Option<JoinHandle<()>>,
}

impl PwDeviceWatcher {
    /// Starts watching. The setup covers creating `MainLoop` + `Context` + `Core` + `Registry` on
    /// a dedicated thread, attaching the `global` / `global_remove` listeners to the registry,
    /// and finishing the initial scan; its success/failure is returned synchronously. After
    /// success the thread keeps spinning in `run()` and pushes hotplug events to the delivery
    /// queue.
    ///
    /// Returns [`Error::Backend`] when the PipeWire daemon is absent or the connection fails
    /// (no panic).
    pub fn start() -> Result<Self> {
        // Create the delivery queue before start, and pass a clone to the setup.
        let events: Arc<Mutex<VecDeque<DeviceEvent>>> = Arc::new(Mutex::new(VecDeque::new()));

        // Stop channel to the watch thread (the receiver is attached to the loop).
        let (stop_tx, stop_rx) = pw::channel::channel::<Terminate>();
        // Channel that reports setup success/failure back to start() synchronously
        // (Ok if everything through registry listener registration + initial scan succeeds).
        let (ready_tx, ready_rx) = mpsc::channel::<std::result::Result<(), String>>();

        let running = Arc::new(AtomicBool::new(true));

        let events_for_thread = events.clone();
        let handle = thread::Builder::new()
            .name("flexaudio-pw-watch".into())
            .spawn(move || {
                run_watch_loop(events_for_thread, stop_rx, &ready_tx);
            })
            .map_err(|e| Error::Backend(format!("spawn pipewire watch thread: {e}")))?;

        // Wait for the setup result. A thread that exits without sending ready is also a failure.
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                events,
                running,
                stop_tx: Some(stop_tx),
                handle: Some(handle),
            }),
            Ok(Err(msg)) => {
                // Setup failed (pipewire absent, connect/registry failure, etc.).
                // The thread has already returned, so join it to clean up.
                running.store(false, Ordering::SeqCst);
                let _ = handle.join();
                Err(Error::Backend(msg))
            }
            Err(_) => {
                // The thread vanished without ever sending ready (unexpected panic, etc.).
                running.store(false, Ordering::SeqCst);
                let _ = handle.join();
                Err(Error::Backend(
                    "pipewire watch thread terminated before signaling readiness".into(),
                ))
            }
        }
    }

    /// Takes the next hotplug event from the delivery queue (`None` if there is none).
    /// Non-blocking. Even if locking fails, it does not panic and returns `None`.
    pub fn poll_event(&mut self) -> Option<DeviceEvent> {
        self.events.lock().ok().and_then(|mut q| q.pop_front())
    }

    /// Stops watching (safe against double stop / stop before start).
    ///
    /// As in [`PwSystemBackend::stop`], sending `Terminate` to the watch thread makes the receiver
    /// callback attached to the loop call `main_loop.quit()` from the thread itself, exiting
    /// `run()`. `join()` waits until destruction completes.
    pub fn stop(&mut self) {
        // Safe against double stop / stop before start.
        if !self.running.swap(false, Ordering::SeqCst) {
            // Already stopped or never started. Join leftovers just in case.
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
            self.stop_tx = None;
            return;
        }

        // Notify the watch thread to stop (the receiver callback calls loop.quit()).
        // Failure (receiver gone) is ignored (already finished).
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(Terminate);
        }

        // Wait for the thread to exit run() and terminate. On exit,
        // Registry→Core→Context→MainLoop are destroyed in drop order (all on the watch thread).
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

/// Local state shared across the whole watch loop thread (`!Send`; confined to the thread).
#[derive(Default)]
struct WatchState {
    /// Reverse lookup from registry global id → the [`DeviceInfo`] to deliver.
    /// `global_remove` passes only the numeric id, so this table maps it back to `node.name`.
    by_global_id: std::collections::HashMap<u32, DeviceInfo>,
    /// Whether the initial scan (the first two-stage sync→done barrier) has completed.
    /// Globals arriving while this is `false` are only registered; `Added` is not delivered.
    initial_scan_done: bool,
    /// `node.name` of the default sink (from the `default.audio.sink` metadata).
    /// Changes after the initial scan completes are delivered as [`DeviceEvent::DefaultChanged`].
    default_sink: Option<String>,
    /// `node.name` of the default source (from the `default.audio.source` metadata).
    default_source: Option<String>,
}

/// Body of the PipeWire watch loop thread.
///
/// Creates, runs, and destroys `MainLoop`/`Context`/`Core`/`Registry` (all `!Send`) only inside
/// this function. Reports setup completion/failure to the caller via `ready_tx`, and on success
/// spins in `main_loop.run()` until the stop instruction ([`Terminate`]).
fn run_watch_loop(
    events: Arc<Mutex<VecDeque<DeviceEvent>>>,
    stop_rx: pw::channel::Receiver<Terminate>,
    ready_tx: &mpsc::Sender<std::result::Result<(), String>>,
) {
    // Setup (connection, registry listener registration, initial scan) is a separate function.
    // The return value is kept alive for the whole run (dropping it stops the watch).
    let (main_loop, _core, _registry, _listeners) = match setup_watch(events) {
        Ok(t) => t,
        Err(msg) => {
            // Report the setup failure and exit (no panic).
            let _ = ready_tx.send(Err(msg));
            return;
        }
    };

    // Attach the stop channel's receiver to the loop. quit() on receiving Terminate.
    // quit() is called inside a loop-driven callback, i.e. from this thread.
    let main_loop_for_quit = main_loop.clone();
    let _attached = stop_rx.attach(main_loop.loop_(), move |_terminate| {
        main_loop_for_quit.quit();
    });

    // Report setup success. From here run() blocks and keeps delivering hotplug events.
    if ready_tx.send(Ok(())).is_err() {
        // The caller is gone (start already dropped, etc.). Do not start.
        return;
    }

    // Spins until Terminate is received or the process exits. Unlike enumerate_pw it does not
    // quit on done, so it spins permanently.
    main_loop.run();
    // On exit, drops happen in the order _attached → _listeners → _registry → _core → main_loop
    // (reverse declaration order), and PipeWire resources are destroyed on this thread.
}

/// Things the watcher owns for the whole run. Dropping them stops the watch, so they are kept
/// on the stack of `run_watch_loop`.
///
/// - `MainLoopRc`: the subject of `run()`/`quit()`.
/// - `CoreRc`: parent of registry / sync (downgraded and used in the done callback).
/// - `RegistryRc`: the registry proxy itself.
/// - Listeners: the registry listener, the core (done) listener, and the bound default
///   metadata's proxy + listener. Dropping them removes the callbacks, so they are held
///   type-erased in a Box.
#[allow(clippy::type_complexity)]
type WatchKeep = (
    pw::main_loop::MainLoopRc,
    pw::core::CoreRc,
    pw::registry::RegistryRc,
    WatchListeners,
);

/// One pair of a bound default metadata proxy + listener (dropping it removes the callbacks).
/// Same shape as the local `MetaKeep` in [`enumerate_pw`].
type MetaKeepEntry = (Box<dyn pw::proxy::ProxyT>, Box<dyn pw::proxy::Listener>);

/// Storage of `MetaKeepEntry` (shared via Rc inside the watch thread; `!Send`).
type MetaKeepStore = std::rc::Rc<std::cell::RefCell<Vec<MetaKeepEntry>>>;

/// Listeners kept alive for the watch (dropping them removes the callbacks).
struct WatchListeners {
    /// The registry's global/global_remove listener.
    _registry_listener: pw::registry::Listener,
    /// The core's done listener (detects completion of the initial scan's two-stage barrier).
    _core_listener: pw::core::Listener,
    /// Storage for the default metadata proxy + listener bound inside the global callback
    /// (same type as in [`enumerate_pw`]; shared via Rc and confined to the watch thread).
    _meta_keep: MetaKeepStore,
}

/// The full PipeWire watch setup. Failures are `Err(String)` (no panic).
///
/// Reuses [`enumerate_pw`]'s registry global extraction logic and two-stage sync→done barrier,
/// but on `done` it does not `quit()`; it only sets the initial-scan-complete flag, and from then
/// on keeps receiving global/global_remove permanently.
#[allow(clippy::type_complexity)]
fn setup_watch(
    events: Arc<Mutex<VecDeque<DeviceEvent>>>,
) -> std::result::Result<WatchKeep, String> {
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

    // Watch-thread-local state (!Send). Shared with each closure via Rc.
    let state = Rc::new(RefCell::new(WatchState::default()));
    // The delivery queue (events: Arc<Mutex<VecDeque>>) is cloned and moved into each closure.

    // Storage that keeps the default metadata's property listeners alive
    // (same type as enumerate_pw. MetaKeepStore = Rc<RefCell<Vec<MetaKeepEntry>>>).
    let meta_keep: MetaKeepStore = Rc::new(RefCell::new(Vec::new()));

    // Registry global / global_remove listeners.
    let state_for_global = state.clone();
    let events_for_global = events.clone();
    let registry_for_global = registry.clone();
    let meta_keep_for_global = meta_keep.clone();
    let state_for_remove = state.clone();
    let events_for_remove = events.clone();
    let _registry_listener = registry
        .add_listener_local()
        .global(move |global| {
            // A panic across FFI is UB, so wrap the body in catch_unwind.
            let _ = catch_unwind(AssertUnwindSafe(|| {
                let Some(props) = global.props else {
                    return;
                };
                match global.type_ {
                    pw::types::ObjectType::Node => {
                        // Same extraction logic as enumerate_pw.
                        // Pick up only nodes whose media.class is Audio/Sink|Source.
                        let media_class = props.get(*pw::keys::MEDIA_CLASS).unwrap_or("");
                        if media_class != "Audio/Sink" && media_class != "Audio/Source" {
                            return;
                        }
                        let node_name = props.get(*pw::keys::NODE_NAME).unwrap_or("");
                        if node_name.is_empty() {
                            // A node without a stable key cannot be handled (skip).
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
                        // is_default is matched against the known default metadata values.
                        // During the initial scan the metadata may not have arrived yet; in that
                        // case it is false (DefaultChanged corrects it later).
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
                            // If unavailable, default to the requested native values (48000/2)
                            // (same as enumerate_pw).
                            sample_rate: rate.unwrap_or(NATIVE_RATE),
                            channels: channels.unwrap_or(NATIVE_CHANNELS),
                            is_loopback,
                            is_default,
                        };
                        st.by_global_id.insert(global.id, info.clone());
                        let initial_scan_done = st.initial_scan_done;
                        drop(st);

                        // Only register during the initial scan. Deliver Added only for appearances
                        // after it.
                        if initial_scan_done {
                            enqueue_event(&events_for_global, DeviceEvent::Added(info));
                        }
                    }
                    pw::types::ObjectType::Metadata => {
                        // Bind only the "default" metadata, which holds the default sink/source
                        // (same as enumerate_pw).
                        let meta_name = props.get("metadata.name").unwrap_or("");
                        if meta_name != "default" {
                            return;
                        }
                        let metadata: pw::metadata::Metadata =
                            match registry_for_global.bind(global) {
                                Ok(m) => m,
                                Err(_) => return,
                            };
                        let state_for_meta = state_for_global.clone();
                        let events_for_meta = events_for_global.clone();
                        let listener = metadata
                            .add_listener_local()
                            .property(move |_subject, key, _type, value| {
                                // The property callback also crosses FFI, so wrap it in
                                // catch_unwind.
                                catch_unwind(AssertUnwindSafe(|| {
                                    // value is JSON (e.g. {"name":"alsa_output...."}). Extract
                                    // name.
                                    if let (Some(key), Some(value)) = (key, value) {
                                        let new_name = extract_json_name(value);
                                        let mut st = state_for_meta.borrow_mut();
                                        if key == "default.audio.sink" {
                                            if st.default_sink != new_name {
                                                st.default_sink = new_name.clone();
                                                // Deliver only changes after the initial scan
                                                // completes.
                                                if st.initial_scan_done {
                                                    if let Some(id) = new_name {
                                                        drop(st);
                                                        enqueue_event(
                                                            &events_for_meta,
                                                            DeviceEvent::DefaultChanged {
                                                                kind: SourceKind::SystemLoopback,
                                                                id,
                                                            },
                                                        );
                                                    }
                                                }
                                            }
                                        } else if key == "default.audio.source"
                                            && st.default_source != new_name
                                        {
                                            st.default_source = new_name.clone();
                                            if st.initial_scan_done {
                                                if let Some(id) = new_name {
                                                    drop(st);
                                                    enqueue_event(
                                                        &events_for_meta,
                                                        DeviceEvent::DefaultChanged {
                                                            kind: SourceKind::Mic,
                                                            id,
                                                        },
                                                    );
                                                }
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
            // A panic across FFI is UB, so wrap the body in catch_unwind.
            let _ = catch_unwind(AssertUnwindSafe(|| {
                // Deliver Removed only for nodes found in the reverse lookup table. Ids not in the
                // table are ignored (removals of non-node globals such as Metadata also arrive,
                // but they are not in the table, so they pass through).
                let removed = state_for_remove.borrow_mut().by_global_id.remove(&id);
                if let Some(info) = removed {
                    enqueue_event(&events_for_remove, DeviceEvent::Removed { id: info.id });
                }
            }));
        })
        .register();

    // Detect initial-scan completion with a two-stage sync→done barrier (same as enumerate_pw).
    // However, done does not quit(); it only sets initial_scan_done. By the time the stage-2 done
    // is received, the initial global enumeration and the default metadata's initial property
    // dump are both in, so subsequent global/global_remove/property changes can be delivered as
    // user-initiated hotplug / default changes.
    let stage = Rc::new(Cell::new(0u8));
    let pending = core
        .sync(0)
        .map_err(|e| format!("pipewire sync failed: {e}"))?;
    let pending = Rc::new(Cell::new(pending.seq()));

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
                    // Stage 1 done → issue the stage-2 sync to wait for the metadata properties.
                    stage_for_cb.set(1);
                    if let Some(core) = core_weak.upgrade() {
                        match core.sync(0) {
                            Ok(p) => pending_for_cb.set(p.seq()),
                            Err(_) => {
                                // If stage 2 cannot be issued, treat the initial scan as complete.
                                stage_for_cb.set(2);
                                state_for_done.borrow_mut().initial_scan_done = true;
                                loop_for_done.quit();
                            }
                        }
                    } else {
                        stage_for_cb.set(2);
                        state_for_done.borrow_mut().initial_scan_done = true;
                        loop_for_done.quit();
                    }
                }
                1 if seq == pending_for_cb.get() => {
                    // Stage 2 done → initial scan finished. quit() is called here only to exit
                    // the initial-scan run() (the while loop below). The permanent watch run()
                    // is spun by run_watch_loop. stage has been advanced to 2, so any later done
                    // hits no arm of this match and quit() is never called again.
                    stage_for_cb.set(2);
                    state_for_done.borrow_mut().initial_scan_done = true;
                    loop_for_done.quit();
                }
                _ => {}
            }
        })
        .register();

    // Spin run() until the initial scan completes (= both round trips complete). done sets
    // initial_scan_done and calls quit(), so like enumerate_pw it always exits. This returns only
    // once the initial global enumeration and the default metadata's initial dump are both in.
    // The permanent watch run() is spun by run_watch_loop. Once stage reaches 2, done no longer
    // quits, so that run() does not stop.
    while !state.borrow().initial_scan_done {
        main_loop.run();
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

/// Pushes one event onto the delivery queue. Does nothing if locking fails (no panic).
///
/// If the consumer does not call `poll_event` for a long time, or devices are hotplugged
/// repeatedly, the `VecDeque` grows without limit. To prevent this, it is capped at
/// [`MAX_WATCH_EVENTS`]; on overflow, the oldest event is dropped and the new one is pushed.
fn enqueue_event(events: &Arc<Mutex<VecDeque<DeviceEvent>>>, ev: DeviceEvent) {
    if let Ok(mut q) = events.lock() {
        // If at the cap, drop the oldest before pushing.
        while q.len() >= MAX_WATCH_EVENTS {
            q.pop_front();
        }
        q.push_back(ev);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flexaudio_core::raw_ring::raw_ring;

    /// `PwSystemBackend: Send`, as the [`CaptureBackend`] contract requires
    /// (evidence that PipeWire's `!Send` is confined to the dedicated thread).
    /// Holds if it compiles.
    #[test]
    fn backend_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<PwSystemBackend>();
    }

    /// Right after construction, the native format is (48000, 2) per the fixed contract.
    #[test]
    fn native_format_is_48k_stereo() {
        let be = PwSystemBackend::new(false, None);
        assert_eq!(be.native_format(), (NATIVE_RATE, NATIVE_CHANNELS));
        assert_eq!(be.native_format(), (48_000, 2));
        assert!(!be.exclude_self());
    }

    /// stop before start / double stop is safe (no panic).
    #[test]
    fn stop_without_start_is_safe() {
        let mut be = PwSystemBackend::new(false, None);
        be.stop();
        be.stop();
    }

    /// The system `exclude_self=true` is implemented by reusing the process Exclude mechanism.
    /// `start` does not return `Unsupported`; it yields [`Error::Backend`] in a headless
    /// environment without PipeWire, and `Ok(())` (successful wait) where a PipeWire session
    /// exists. Checks that neither case panics and that, if Ok, it can go all the way to
    /// `stop()`.
    #[test]
    fn system_exclude_self_is_graceful() {
        let (prod, _cons) = raw_ring(1 << 16);
        let sink = RawSink::new(prod, NATIVE_RATE, NATIVE_CHANNELS);
        let mut be = PwSystemBackend::new(true, None);
        assert!(be.exclude_self());
        match be.start(sink) {
            Ok(()) => {
                // An environment with a PipeWire session. Delegates to the Exclude mechanism that
                // fan-ins everything but itself, and waits successfully even if no target has
                // appeared. It must be able to go all the way to stop.
                be.stop();
            }
            Err(Error::Backend(_)) => {
                // PipeWire absent/registry failure: expected. The point is that it did not panic.
            }
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }

    /// `extract_json_name` extracts name from a PipeWire metadata value (JSON).
    #[test]
    fn extract_json_name_parses_default_metadata_value() {
        assert_eq!(
            extract_json_name(r#"{"name":"alsa_output.pci-0000_00_1f.3.analog-stereo"}"#)
                .as_deref(),
            Some("alsa_output.pci-0000_00_1f.3.analog-stereo")
        );
        // Works with whitespace too.
        assert_eq!(
            extract_json_name(r#"{ "name" : "foo.bar" }"#).as_deref(),
            Some("foo.bar")
        );
        // None if the name key is missing / empty / malformed.
        assert_eq!(extract_json_name(r#"{"other":"x"}"#), None);
        assert_eq!(extract_json_name(r#"{"name":""}"#), None);
        assert_eq!(extract_json_name("not json"), None);
    }

    /// `list_devices` does not panic and returns `Ok(Vec)` even in a headless environment without
    /// PipeWire (daemon absence is swallowed as "nothing to enumerate" = empty Vec). If devices
    /// are returned, verifies Sink→SystemLoopback / Source→Mic consistency and that id
    /// (=node.name) is non-empty.
    #[test]
    fn list_devices_is_graceful_without_pipewire() {
        let devices = list_devices().expect("list_devices is designed never to return Err");
        for d in &devices {
            assert!(!d.id.is_empty(), "id (=node.name) is non-empty");
            match d.source_kind {
                SourceKind::SystemLoopback => assert!(d.is_loopback, "a Sink is loopback"),
                SourceKind::Mic => assert!(!d.is_loopback, "a Source is not loopback"),
                other => panic!("unexpected source_kind: {other:?}"),
            }
            assert!(d.sample_rate > 0);
            assert!(d.channels > 0);
        }
        // At most one default sink and at most one default source.
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

    /// Smoke test: in a headless environment without PipeWire/a sink, `start` may become
    /// `Err(Error::Backend)` but does not panic. Both Ok (an environment with PipeWire and a
    /// running sink) and Err(Backend) are accepted.
    ///
    /// On a desktop/laptop running PipeWire it is Ok and can go all the way to `stop()`. For
    /// actual end-to-end audio verification, see the `#[ignore]` test below.
    #[test]
    fn start_is_graceful_without_pipewire() {
        let (prod, _cons) = raw_ring(1 << 16);
        let sink = RawSink::new(prod, NATIVE_RATE, NATIVE_CHANNELS);
        let mut be = PwSystemBackend::new(false, None);
        match be.start(sink) {
            Ok(()) => {
                // An environment with a running PipeWire/sink. It must be able to go all the way to
                // stop.
                be.stop();
            }
            Err(Error::Backend(_)) => {
                // PipeWire absent/no sink: expected. The point is that it did not panic.
            }
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }

    /// `start` when `device_id` points to a sink that does not exist. Where PipeWire runs, that
    /// sink does not appear in the enumeration, so [`Error::DeviceNotFound`]. Where PipeWire is
    /// absent, enumerate_pw's Err is swallowed and the normal path's connection failure yields
    /// [`Error::Backend`]. Checks that neither case panics and that it is never Ok.
    #[test]
    fn start_with_unknown_device_id_is_not_found_or_backend() {
        let (prod, _cons) = raw_ring(1 << 16);
        let sink = RawSink::new(prod, NATIVE_RATE, NATIVE_CHANNELS);
        let mut be = PwSystemBackend::new(false, Some("flexaudio-no-such-sink-zzz".to_string()));
        match be.start(sink) {
            Err(Error::DeviceNotFound) => {}
            Err(Error::Backend(_)) => {}
            Ok(()) => {
                be.stop();
                panic!("start should not succeed for an unknown device_id");
            }
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }

    /// Real capture end-to-end (only on a desktop/laptop running PipeWire).
    ///
    /// How to run (on a laptop etc., with PipeWire and some sound playing):
    /// ```text
    /// cargo test -p flexaudio-os-linux -- --ignored capture_smoke
    /// ```
    /// Records the default sink's monitor for a while and expects samples to flow in
    /// (observed via overflow or pop). Headless environments/CI have neither a sound source nor
    /// PipeWire, hence `#[ignore]`.
    #[test]
    #[ignore = "requires a running PipeWire session with audio playing (desktop/laptop)"]
    fn capture_smoke() {
        use std::time::Duration;
        let (prod, mut cons) = raw_ring(1 << 18);
        let sink = RawSink::new(prod, NATIVE_RATE, NATIVE_CHANNELS);
        let mut be = PwSystemBackend::new(false, None);
        be.start(sink)
            .expect("start should succeed on a PipeWire desktop");
        // Wait briefly for recording to get going.
        thread::sleep(Duration::from_millis(500));
        be.stop();
        // Some samples have arrived (even a silent sink streams 0.0 samples).
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

    /// `PwProcessBackend: Send`, as the [`CaptureBackend`] contract requires
    /// (evidence that PipeWire's `!Send` is confined to the dedicated thread).
    /// Holds if it compiles.
    #[test]
    fn process_backend_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<PwProcessBackend>();
    }

    /// Right after construction, the native format is (48000, 2) per the fixed contract.
    /// Also checks that the PID / mode are retained.
    #[test]
    fn process_native_format_is_48k_stereo() {
        let be = PwProcessBackend::new(4242, ProcessMode::Exclude);
        assert_eq!(be.native_format(), (NATIVE_RATE, NATIVE_CHANNELS));
        assert_eq!(be.native_format(), (48_000, 2));
        // The construction arguments are retained.
        assert_eq!(be.target_pid(), 4242);
        assert_eq!(be.mode(), ProcessMode::Exclude);
        let be2 = PwProcessBackend::new(1, ProcessMode::Include);
        assert_eq!(be2.mode(), ProcessMode::Include);
    }

    /// stop before start / double stop is safe (no panic).
    #[test]
    fn process_stop_without_start_is_safe() {
        let mut be = PwProcessBackend::new(1234, ProcessMode::Include);
        be.stop();
        be.stop();
    }

    /// Process [`ProcessMode::Exclude`] fan-ins and records everything but the target PID.
    /// `start` does not return `Unsupported`; it yields [`Error::Backend`] in a headless
    /// environment without PipeWire, and `Ok(())` (successful wait) where a PipeWire session
    /// exists. Checks that neither case panics and that, if Ok, it can go through double start
    /// no-op + stop + double stop.
    #[test]
    fn process_exclude_mode_is_graceful() {
        let (prod, _cons) = raw_ring(1 << 16);
        let sink = RawSink::new(prod, NATIVE_RATE, NATIVE_CHANNELS);
        let mut be = PwProcessBackend::new(u32::MAX, ProcessMode::Exclude);
        match be.start(sink) {
            Ok(()) => {
                // An environment with a PipeWire session. Delegates to the Exclude mechanism that
                // fan-ins everything but the target PID, and waits successfully. Safe against
                // double start (no-op, Ok).
                let (prod2, _cons2) = raw_ring(1 << 16);
                let sink2 = RawSink::new(prod2, NATIVE_RATE, NATIVE_CHANNELS);
                assert!(be.start(sink2).is_ok());
                // It must be able to go all the way to stop (safe to destroy even before linking).
                be.stop();
                // Double stop is also safe.
                be.stop();
            }
            Err(Error::Backend(_)) => {
                // PipeWire absent/registry failure: expected. The point is that it did not panic.
            }
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }

    /// Verifies `resolve_node_pid` (PipeWire-independent).
    ///
    /// Fact confirmed with pw-dump on a real device: the PID lives on the Client, and a node only
    /// points to the Client via `client.id`. So PID resolution has two stages (node → client.id →
    /// the Client's PID). Whether the Client or the Node arrives first, re-evaluating on each
    /// arrival resolves correctly. This is checked before and after putting values into the
    /// `client_pid` table.
    #[test]
    fn resolve_node_pid_via_client_table() {
        use std::collections::HashMap;

        // A real pw-cat example: node.id=62 points to client.id=60, and the Client with
        // client.id=60 has application.process.id=13394.
        let node = NodeEntry {
            owning_client_id: Some(60),
            app_pid: None,
        };

        // --- The Node arrived first and the Client is not in the table yet → unresolved (None).
        let mut client_pid: HashMap<u32, u32> = HashMap::new();
        assert_eq!(
            resolve_node_pid(&node, &client_pid),
            None,
            "PID is unresolved while no Client matching client.id exists yet"
        );

        // --- The Client (global id=60, pid=13394) arrives later and enters the table → resolved.
        client_pid.insert(60, 13394);
        assert_eq!(
            resolve_node_pid(&node, &client_pid),
            Some(13394),
            "client.id=60 → Client pid=13394, resolved in two stages"
        );

        // --- A node without client.id cannot be resolved (unless it has a direct PID).
        let orphan = NodeEntry {
            owning_client_id: None,
            app_pid: None,
        };
        assert_eq!(resolve_node_pid(&orphan, &client_pid), None);

        // If the node itself carries application.process.id, it resolves directly without the
        // Client, even with an empty client_pid table (in preparation for future setups).
        let node_with_pid = NodeEntry {
            owning_client_id: Some(99), // even with a client.id not in the table
            app_pid: Some(424242),
        };
        let empty: HashMap<u32, u32> = HashMap::new();
        assert_eq!(
            resolve_node_pid(&node_with_pid, &empty),
            Some(424242),
            "the node's own PID takes precedence and resolves directly"
        );

        // --- A node of another client.id has another PID (never mixed up).
        let other_node = NodeEntry {
            owning_client_id: Some(61),
            app_pid: None,
        };
        // client 61 is unregistered, so None; once registered, its PID.
        assert_eq!(resolve_node_pid(&other_node, &client_pid), None);
        client_pid.insert(61, 555);
        assert_eq!(resolve_node_pid(&other_node, &client_pid), Some(555));
        // Resolution of node(client 60) is unaffected.
        assert_eq!(resolve_node_pid(&node, &client_pid), Some(13394));
    }

    /// Verifies the channel matching of `pair_ports` (PipeWire-independent).
    #[test]
    fn pair_ports_maps_channels() {
        // Stereo→stereo: FL→FL / FR→FR (channel-name match).
        // Output ports: id 10=FL, 11=FR. Input ports: id 20=FL, 21=FR.
        let out = vec![(10u32, "FL".to_string()), (11u32, "FR".to_string())];
        let inp = vec![(20u32, "FL".to_string()), (21u32, "FR".to_string())];
        let mut pairs = pair_ports(&out, &inp);
        pairs.sort();
        assert_eq!(pairs, vec![(10, 20), (11, 21)], "FL→FL / FR→FR");

        // Even with the inputs in reverse order, they are matched correctly by channel name.
        let inp_rev = vec![(21u32, "FR".to_string()), (20u32, "FL".to_string())];
        let mut pairs = pair_ports(&out, &inp_rev);
        pairs.sort();
        assert_eq!(
            pairs,
            vec![(10, 20), (11, 21)],
            "FL→FL / FR→FR even in reverse order"
        );

        // Mono output → stereo input: the single output is duplicated to both FL/FR.
        let mono_out = vec![(30u32, "MONO".to_string())];
        let stereo_in = vec![(40u32, "FL".to_string()), (41u32, "FR".to_string())];
        let mut pairs = pair_ports(&mono_out, &stereo_in);
        pairs.sort();
        assert_eq!(
            pairs,
            vec![(30, 40), (30, 41)],
            "mono is duplicated to FL/FR"
        );

        // Outputs without channel names (empty) → order fallback.
        let out_noch = vec![(50u32, String::new()), (51u32, String::new())];
        let in_noch = vec![(60u32, String::new()), (61u32, String::new())];
        let pairs = pair_ports(&out_noch, &in_noch);
        // The 2 ports on each side match one-to-one (each input at most once).
        assert_eq!(pairs.len(), 2);
        let ins: std::collections::HashSet<u32> = pairs.iter().map(|(_, i)| *i).collect();
        assert_eq!(ins.len(), 2, "each input port at most once");

        // Empty sets give no links (if either side has not appeared, do not link).
        assert!(pair_ports(&[], &inp).is_empty());
        assert!(pair_ports(&out, &[]).is_empty());

        // Even when a matching channel exists on only one side, fill by order rather than mono
        // duplication.
        // Output FL only, input FR only (names do not match) → one match via order fallback.
        let out_fl = vec![(70u32, "FL".to_string())];
        let in_fr = vec![(80u32, "FR".to_string())];
        // There is 1 output port, so the mono duplication rule runs and duplicates to the remaining
        // inputs.
        let pairs = pair_ports(&out_fl, &in_fr);
        assert_eq!(
            pairs,
            vec![(70, 80)],
            "a single output port is duplicated to the remaining inputs"
        );
    }

    /// `(port_id, channel)` ports of one side, as [`pair_ports`] takes them.
    type Ports = Vec<(u32, String)>;

    /// Replays one arrival order of ports (each step: the target's output ports and our own
    /// input ports present at that moment) through the same re-planning `try_link` does
    /// ([`pair_ports`] → [`plan_link_changes`] → apply) and returns the settled pairs.
    fn replay_arrivals(steps: &[(Ports, Ports)]) -> Vec<(u32, u32)> {
        let mut linked: Vec<(u32, u32)> = Vec::new();
        for (out, inp) in steps {
            let changes = plan_link_changes(&linked, &pair_ports(out, inp));
            linked.retain(|pair| !changes.remove.contains(pair));
            linked.extend(changes.add);
        }
        linked.sort_unstable();
        linked
    }

    #[test]
    fn plan_link_changes_reports_only_the_difference() {
        // Already matching: nothing to do.
        let changes = plan_link_changes(&[(10, 20), (11, 21)], &[(11, 21), (10, 20)]);
        assert!(changes.remove.is_empty() && changes.add.is_empty());

        // Not linked yet: every wanted pair is added, in plan order.
        let changes = plan_link_changes(&[], &[(10, 20), (11, 21)]);
        assert_eq!(changes.add, vec![(10, 20), (11, 21)]);
        assert!(changes.remove.is_empty());

        // A pair the plan no longer wants is removed, and the kept one is not recreated.
        let changes = plan_link_changes(&[(10, 20), (10, 21)], &[(10, 20), (11, 21)]);
        assert_eq!(changes.remove, vec![(10, 21)]);
        assert_eq!(changes.add, vec![(11, 21)]);
    }

    #[test]
    fn late_own_input_port_is_linked_instead_of_leaving_fr_silent() {
        // Our own input_FR arrives after the first plan was made from input_FL alone. Linking
        // FL→FL and latching it left FR silent (stereo at half level, measured on PipeWire).
        let out = vec![(10u32, "FL".to_string()), (11u32, "FR".to_string())];
        let settled = replay_arrivals(&[
            (out.clone(), vec![(20u32, "FL".to_string())]),
            (
                out,
                vec![(20u32, "FL".to_string()), (21u32, "FR".to_string())],
            ),
        ]);
        assert_eq!(settled, vec![(10, 20), (11, 21)], "FL→FL / FR→FR");
    }

    #[test]
    fn late_target_output_port_replaces_the_mono_duplication() {
        // The target's output_FR arrives after the first plan was made from output_FL alone,
        // which duplicated FL to both inputs as if it were mono. The duplication FL→FR is cut
        // and FR→FR takes its place.
        let inp = vec![(20u32, "FL".to_string()), (21u32, "FR".to_string())];
        let settled = replay_arrivals(&[
            (vec![(10u32, "FL".to_string())], inp.clone()),
            (
                vec![(10u32, "FL".to_string()), (11u32, "FR".to_string())],
                inp,
            ),
        ]);
        assert_eq!(settled, vec![(10, 20), (11, 21)], "FL→FL / FR→FR");
    }

    #[test]
    fn a_real_mono_target_stays_duplicated() {
        // A target that only ever has one output port keeps the mono duplication.
        let settled = replay_arrivals(&[(
            vec![(30u32, "MONO".to_string())],
            vec![(40u32, "FL".to_string()), (41u32, "FR".to_string())],
        )]);
        assert_eq!(settled, vec![(30, 40), (30, 41)]);
    }

    /// Smoke test: in a headless environment where PipeWire is absent / getting the registry
    /// fails, process capture's `start` may become `Err(Error::Backend)` but does not panic.
    /// Where a PipeWire session exists, it is treated as successful and waits even if the target
    /// PID has not appeared (it succeeds once the registry is obtained, and links when the target
    /// appears). If Ok, checks that it can go all the way to `stop()` even if the target PID is
    /// not playing (destruction is safe).
    #[test]
    fn process_start_is_graceful_without_pipewire() {
        let (prod, _cons) = raw_ring(1 << 16);
        let sink = RawSink::new(prod, NATIVE_RATE, NATIVE_CHANNELS);
        // A PID that presumably does not exist. start may succeed and wait even if it never appears
        // (Include).
        let mut be = PwProcessBackend::new(u32::MAX, ProcessMode::Include);
        match be.start(sink) {
            Ok(()) => {
                // An environment with a PipeWire session. Waits successfully even if the target PID
                // has not appeared. Safe against double start (no-op, Ok).
                let (prod2, _cons2) = raw_ring(1 << 16);
                let sink2 = RawSink::new(prod2, NATIVE_RATE, NATIVE_CHANNELS);
                assert!(be.start(sink2).is_ok());
                // It must be able to go all the way to stop (safe to destroy even before linking).
                be.stop();
                // Double stop is also safe.
                be.stop();
            }
            Err(Error::Backend(_)) => {
                // PipeWire absent/registry failure: expected. The point is that it did not panic.
            }
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }

    /// Real capture end-to-end (only on a desktop/laptop running PipeWire).
    ///
    /// How to run (on a laptop etc., with PipeWire and some sound playing from the target PID):
    /// ```text
    /// # Example: play speaker-test and take its PID
    /// speaker-test -t sine -f 1000 -c 2 &  # → note the PID
    /// FLEXAUDIO_TEST_PID=<PID> \
    ///   cargo test -p flexaudio-os-linux -- --ignored process_capture_smoke
    /// ```
    /// Links to the target PID's app output ports with link-factory and expects samples to flow
    /// in. Skipped if `FLEXAUDIO_TEST_PID` is not set (the PID is unknown).
    /// Headless environments/CI have neither PipeWire nor a sound source, hence `#[ignore]`.
    #[test]
    #[ignore = "requires a running PipeWire session with the target PID playing audio (set FLEXAUDIO_TEST_PID)"]
    fn process_capture_smoke() {
        use std::time::Duration;
        let Ok(pid_str) = std::env::var("FLEXAUDIO_TEST_PID") else {
            eprintln!("skipping: FLEXAUDIO_TEST_PID is not set");
            return;
        };
        let pid: u32 = pid_str.parse().expect("FLEXAUDIO_TEST_PID must be a u32");
        let (prod, mut cons) = raw_ring(1 << 18);
        let sink = RawSink::new(prod, NATIVE_RATE, NATIVE_CHANNELS);
        let mut be = PwProcessBackend::new(pid, ProcessMode::Include);
        be.start(sink)
            .expect("start should succeed on a PipeWire desktop");
        // Wait briefly for the link to be established + recording to get going.
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
    // PwDeviceWatcher (hotplug notifications)
    // ------------------------------------------------------------------------

    /// [`PwDeviceWatcher`] is `Send` (evidence that PipeWire's `!Send` types are confined to the
    /// dedicated thread). Holds if it compiles.
    /// Modeled on the same test for `PwSystemBackend`.
    #[test]
    fn watcher_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<PwDeviceWatcher>();
    }

    /// `start()` does not panic even in a headless environment without PipeWire. With a PipeWire
    /// session it is `Ok`, without one it may be `Err(Backend)`, but the point is that neither
    /// panics (the facade swallows Err as a no-op fallback). If it becomes Ok, also checks that
    /// it can go all the way to stop (destruction is safe).
    #[test]
    fn watcher_graceful_without_pipewire() {
        match PwDeviceWatcher::start() {
            Ok(mut w) => {
                // An environment with a PipeWire session. poll_event is non-blocking, and the
                // initial scan is suppressed, so it may be None right away (fine if not).
                let _ = w.poll_event();
                w.stop();
            }
            Err(Error::Backend(_)) => {
                // PipeWire absent: expected. The point is that it did not panic.
            }
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }

    /// After a successful `start()`, calling `stop()` twice is safe (no panic; the second is a
    /// no-op). Skipped where `start()` returns Err because PipeWire is absent.
    #[test]
    fn watcher_double_stop_is_safe() {
        if let Ok(mut w) = PwDeviceWatcher::start() {
            w.stop();
            w.stop();
        }
        // Where start failed (PipeWire absent) there is nothing to verify = OK as long as it does
        // not panic.
    }

    /// Queue input/output equivalent to `enqueue_event` / `poll` works FIFO
    /// (PipeWire-independent; verifies only the delivery queue logic).
    #[test]
    fn enqueue_and_drain_is_fifo() {
        let events: Arc<Mutex<VecDeque<DeviceEvent>>> = Arc::new(Mutex::new(VecDeque::new()));
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
                kind: SourceKind::SystemLoopback,
                id: "sink.x".into(),
            },
        );
        // Equivalent of poll_event (take out FIFO).
        let mut drained = Vec::new();
        while let Some(ev) = events.lock().unwrap().pop_front() {
            drained.push(ev);
        }
        assert_eq!(
            drained,
            vec![
                DeviceEvent::Added(mic),
                DeviceEvent::Removed { id: "mic.a".into() },
                DeviceEvent::DefaultChanged {
                    kind: SourceKind::SystemLoopback,
                    id: "sink.x".into(),
                },
            ]
        );
    }

    /// `enqueue_event` caps the delivery queue at [`MAX_WATCH_EVENTS`], dropping the oldest and
    /// pushing the new one on overflow. Pushes cap + α and checks that the length does not
    /// exceed the cap and that the newest side remains.
    #[test]
    fn enqueue_event_caps_queue_and_drops_oldest() {
        let events: Arc<Mutex<VecDeque<DeviceEvent>>> = Arc::new(Mutex::new(VecDeque::new()));
        // Push cap + 10. The id embeds the node number so we can tell which ones remained.
        let total = MAX_WATCH_EVENTS + 10;
        for i in 0..total {
            enqueue_event(
                &events,
                DeviceEvent::Removed {
                    id: format!("n{i}"),
                },
            );
        }
        let q = events.lock().unwrap();
        // The length does not exceed the cap.
        assert_eq!(
            q.len(),
            MAX_WATCH_EVENTS,
            "queue length plateaus at the cap"
        );
        // The oldest 10 (n0..n9) are dropped, and the head becomes n10.
        match q.front().unwrap() {
            DeviceEvent::Removed { id } => assert_eq!(id, "n10", "dropped from the oldest"),
            other => panic!("unexpected event: {other:?}"),
        }
        // The newest (n{total-1}) remains.
        match q.back().unwrap() {
            DeviceEvent::Removed { id } => assert_eq!(id, &format!("n{}", total - 1)),
            other => panic!("unexpected event: {other:?}"),
        }
    }
}
