//! [`Stream`] owns a single-source capture pipeline.
//!
//! It wires core components ([`RawRing`](mod@flexaudio_core::raw_ring) / [`Normalizer`] /
//! [`ChunkRing`](mod@flexaudio_core::chunk_ring) / [`ClockNormalizer`] /
//! [`CaptureBackend`]) and supplies the consumer through the pull API
//! ([`poll_chunk`](Stream::poll_chunk) / [`poll_event`](Stream::poll_event)).
//!
//! # Thread layout
//! - Backend RT thread: only pushes raw frames through [`RawSink`] to
//!   [`RawRing`](mod@flexaudio_core::raw_ring) (non-blocking).
//! - Intake/processing thread (one, normal priority): pops RawRing → normalizes to
//!   48k/stereo/20ms → assigns monotonically increasing `seq` → pushes to
//!   [`ChunkRing`](mod@flexaudio_core::chunk_ring) (DROP_OLDEST). Updates the last sample processing
//!   time in `AtomicI64`.
//! - Watchdog thread (one, ~250ms tick): if samples stop arriving for long enough, treats the stream
//!   as stalled and reopens the backend with exponential backoff (250ms→5s, with jitter). Fires
//!   [`Event::StreamStalled`] on a stall and [`Event::StreamRecovered`] on recovery, and marks the
//!   first post-recovery chunk with [`ChunkFlags::RECOVERED`] | [`ChunkFlags::DISCONTINUITY`].

use std::collections::VecDeque;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use flexaudio_core::backend::{CaptureBackend, RawSink};
use flexaudio_core::chunk_ring::{chunk_ring, ChunkConsumer, ChunkProducer};
use flexaudio_core::clock::{monotonic_now_ns, ClockNormalizer};
use flexaudio_core::normalizer::{InnerProcessor, Normalizer};
use flexaudio_core::raw_ring::{raw_ring, RawConsumer};
use flexaudio_core::secondary_ring::{
    secondary_chunk_ring, SecondaryChunkConsumer, SecondaryChunkProducer,
};
use flexaudio_core::types::{
    AudioChunk, ChunkFlags, Error, Event, OutputFormat, Permission, Result, SecondaryChunk,
    StreamConfig,
};

mod tap_drain;
mod terminal;
use terminal::TerminalFailure;

#[cfg(test)]
mod permission_tests;
#[cfg(test)]
mod recovery_tests;

/// Wraps [`flexaudio_denoise::Denoiser`] as a core [`InnerProcessor`] so the
/// core stays independent of the concrete noise-suppression implementation.
///
/// The processor runs on the internal normalized form (48kHz / stereo), so the
/// denoiser is always constructed for two channels regardless of the primary
/// output channel count. `process`/`flush` forward to the denoiser (any error
/// is swallowed: the normalized form always has an even length, so
/// [`Denoiser::process`](flexaudio_denoise::Denoiser::process) never rejects it).
struct DenoiseInnerProcessor {
    denoiser: flexaudio_denoise::Denoiser,
}

impl DenoiseInnerProcessor {
    /// Build a fresh stereo denoiser. Called once per intake generation so a
    /// source switch / reopen starts with clean RNNoise state (no bleed across
    /// the discontinuity).
    fn new() -> Self {
        // Denoiser::new(2) only fails for an out-of-range channel count; 2 is
        // always valid, so unwrap is safe here.
        Self {
            denoiser: flexaudio_denoise::Denoiser::new(2)
                .expect("stereo denoiser construction is infallible"),
        }
    }
}

impl InnerProcessor for DenoiseInnerProcessor {
    fn process(&mut self, samples: &mut [f32]) {
        let _ = self.denoiser.process(samples);
    }
    fn flush(&mut self) -> Vec<f32> {
        self.denoiser.flush()
    }
}

/// Build the optional inner processor for a Normalizer, honoring the current
/// `denoise_enabled` flag. Returns `None` when denoise is off.
fn build_inner_processor(denoise_enabled: bool) -> Option<Box<dyn InnerProcessor>> {
    if denoise_enabled {
        Some(Box::new(DenoiseInnerProcessor::new()))
    } else {
        None
    }
}

/// Build the [`Normalizer`] for the current generation: primary output, the
/// optional secondary tap, and the optional denoise inner processor. Kept in one
/// place so `start` and the intake generation-change path stay in sync.
fn build_normalizer(
    shared: &SharedState,
    rate: u32,
    channels: u16,
    output: OutputFormat,
    secondary_output: Option<OutputFormat>,
    denoise_enabled: bool,
) -> Result<Normalizer> {
    let mut n = Normalizer::new(rate, channels, output)?;
    if shared.capture_enabled.load(Ordering::SeqCst) {
        n = n.with_capture_tap()?;
    }
    if let Some(sec) = secondary_output {
        n = n.with_secondary(sec)?;
    }
    let processor = build_inner_processor(denoise_enabled);
    if let Some(processor) = processor {
        n = n.with_inner_processor(processor);
    }
    Ok(n)
}

/// RawRing capacity in f32 samples. Keep it independent of native SR×ch and large enough to avoid
/// drops on the RT path (about 0.5 seconds of 48k stereo headroom). The mix source's composite
/// backend (`mix.rs`) uses the same capacity for its child rings.
pub(crate) const RAW_RING_SAMPLES: usize = 48_000;

/// Watchdog tick interval.
const WATCHDOG_TICK: Duration = Duration::from_millis(250);

/// Bound mailbox work so a noisy custom backend cannot monopolize the control thread.
const MAX_BACKEND_EVENTS_PER_TICK: usize = 64;
/// Final reconciliation is finite even for a backend that violates mailbox semantics.
const MAX_FINAL_EVENT_BATCHES: usize = 64;

/// Default threshold for treating a sample gap this long as a stall.
const STALL_THRESHOLD: Duration = Duration::from_secs(2);

/// Lower bound for exponential reopen backoff.
const BACKOFF_MIN: Duration = Duration::from_millis(250);
/// Upper bound for exponential reopen backoff.
const BACKOFF_MAX: Duration = Duration::from_secs(5);

/// Capture pipeline for a single source.
///
/// Configure it with [`open`](Self::open) and start capture with [`start`](Self::start).
/// Consumers call [`poll_chunk`](Self::poll_chunk) / [`poll_event`](Self::poll_event) without
/// blocking. [`stop`](Self::stop) joins all threads.
pub struct Stream {
    config: StreamConfig,

    /// Shared backend, reopened by the intake/watchdog threads.
    shared: Arc<SharedState>,

    /// Consumer end of the chunk ring, read by the caller.
    chunk_consumer: ChunkConsumer,
    capture_consumer: Option<ChunkConsumer>,

    /// Secondary tap consumer (only when `config.secondary_output` is `Some`).
    secondary_consumer: Option<SecondaryChunkConsumer>,

    /// Consumer end of the shared event queue.
    events: Arc<Mutex<VecDeque<Event>>>,

    /// Intake/processing thread.
    worker: Option<JoinHandle<()>>,
    /// Watchdog thread.
    watchdog: Option<JoinHandle<()>>,

    /// Whether started (prevents duplicate start calls).
    started: bool,
}

/// State shared by the intake thread, watchdog thread, and main thread.
struct SharedState {
    primary_frame_index: AtomicU64,
    secondary_frame_index: AtomicU64,
    capture_frame_index: AtomicU64,
    capture_enabled: AtomicBool,
    capture_producer: Mutex<Option<ChunkProducer>>,
    /// Backend implementation, protected by a lock for reopen.
    backend: Mutex<Box<dyn CaptureBackend>>,

    /// Currently active RawConsumer. Replaced by the watchdog on reopen.
    /// This lock also guards publication and snapshots of native format, generation and recovery.
    /// The intake thread does not pop while this is `None` (during reopen).
    raw_consumer: Mutex<Option<RawConsumer>>,

    /// Generation of `raw_consumer`, incremented on each reopen. The intake thread detects changes
    /// and resets internal state such as the Normalizer.
    raw_generation: AtomicU64,

    /// Monotonic time (ns) when the last sample was processed (popped and fed to Normalizer).
    last_sample_ns: AtomicI64,

    /// Stop request for all threads.
    stopping: AtomicBool,

    /// Confirmed denial persists after all capture threads have stopped.
    terminal: TerminalFailure,

    /// Recovery for the current raw generation, published and consumed under `raw_consumer`.
    /// Intake consumes it only with captured samples; replacing the generation or stopping clears
    /// it. The first delivered data chunk carries RECOVERED|DISCONTINUITY and announces recovery.
    recovered_pending: AtomicBool,

    /// Event queue shared by producer and consumer.
    events: Arc<Mutex<VecDeque<Event>>>,

    /// Producer end of ChunkRing (used by the intake thread).
    chunk_producer: Mutex<Option<ChunkProducer>>,

    /// Native format `(sample_rate, channels)` of the current `backend`.
    ///
    /// Unchanged when the watchdog reopens the same backend, but updated to the new backend's value
    /// when [`Stream::switch_source`] changes the source (native SR/ch commonly differ between
    /// mic↔system/process). On a generation change, the intake thread rereads this and rebuilds the
    /// native-dependent first stage of [`Normalizer`].
    native_format: Mutex<(u32, u16)>,

    /// Source-switch flag. [`Stream::switch_backend`] sets it to true during a switch. The watchdog
    /// skips stall handling during a switch to avoid concurrent reopen (the old backend is briefly
    /// idle while stopped, which must not trigger an incorrect reopen).
    switching: AtomicBool,

    /// Intentional discontinuity flag. [`Stream::switch_backend`] sets it to true on a successful
    /// source switch; the intake thread marks the next chunk DISCONTINUITY (without RECOVERED,
    /// because this was an intentional switch, not automatic recovery) and resets it to false.
    discontinuity_pending: AtomicBool,

    /// Pause flag. pause() sets it to true. The intake thread discards completed chunks instead of
    /// delivering them (RawRing capture continues, so the device stays active and watchdog stall
    /// detection remains accurate). resume() resets it to false.
    paused: AtomicBool,

    /// Serializes `pause()` with chunk enqueueing so the intake thread cannot enqueue a chunk
    /// after `pause()` returns.
    delivery: Mutex<()>,

    /// Generation for each actual pause→resume transition. Each tap observes it under the delivery
    /// lock and marks its first chunk enqueued after a change with DISCONTINUITY.
    resume_generation: AtomicU64,

    /// Input gain (linear multiplier) stored as f32 bits (using `f32::to_bits`/`from_bits`).
    /// Initialized from config.gain in open(); set_gain() can update it during recording. The
    /// intake thread reads it per completed chunk and multiplies each sample when it is not 1.0.
    gain_bits: Arc<AtomicU32>,

    /// Recording epoch (ns). Set once to the computed PTS of the first delivered primary chunk, then
    /// subtract it from all later chunks (primary and secondary) so recording starts at zero. The
    /// `i64::MIN` sentinel means unset. Never reset across reopen/switch (one clock per recording).
    recording_epoch_ns: AtomicI64,

    /// Whether to enable noise suppression in the internal canonical format. Read when the intake
    /// thread (re)builds Normalizer; if enabled, denoise is injected into core's InnerProcessor.
    denoise_enabled: AtomicBool,

    /// Producer end of the secondary ChunkRing (Some only when the secondary tap is configured).
    secondary_producer: Mutex<Option<SecondaryChunkProducer>>,
}

/// Metadata and samples observed together under the raw-consumer lock.
struct RawSnapshot {
    generation: u64,
    denoise_enabled: bool,
    native_format: (u32, u16),
    samples: usize,
    overflows: u64,
    recovered: bool,
    discontinuity: bool,
}

/// Flags published atomically with a new raw generation.
#[derive(Clone, Copy)]
enum GenerationChange {
    Initial,
    Recovery,
    Switch,
}

impl SharedState {
    fn snapshot_raw(&self, scratch: &mut [f32]) -> RawSnapshot {
        let mut consumer = self.raw_consumer.lock().unwrap_or_else(|e| e.into_inner());
        let native_format = *self.native_format.lock().unwrap_or_else(|e| e.into_inner());
        let (samples, overflows) = match consumer.as_mut() {
            Some(consumer) => (consumer.pop_slice(scratch), consumer.overflow_count()),
            None => (0, 0),
        };
        RawSnapshot {
            generation: self.raw_generation.load(Ordering::SeqCst),
            denoise_enabled: self.denoise_enabled.load(Ordering::SeqCst),
            native_format,
            samples,
            overflows,
            // Empty generations must leave recovery pending until captured samples arrive.
            recovered: samples > 0 && self.recovered_pending.swap(false, Ordering::SeqCst),
            discontinuity: !scratch.is_empty()
                && self.discontinuity_pending.swap(false, Ordering::SeqCst),
        }
    }

    /// Require the raw-consumer guard across this entire publication.
    fn publish_raw(
        &self,
        consumer: &mut MutexGuard<'_, Option<RawConsumer>>,
        new_consumer: RawConsumer,
        native_format: (u32, u16),
        change: GenerationChange,
    ) {
        *self.native_format.lock().unwrap_or_else(|e| e.into_inner()) = native_format;
        **consumer = Some(new_consumer);
        self.raw_generation.fetch_add(1, Ordering::SeqCst);
        self.recovered_pending.store(
            matches!(change, GenerationChange::Recovery) && !self.stopping.load(Ordering::SeqCst),
            Ordering::SeqCst,
        );
        self.discontinuity_pending
            .store(matches!(change, GenerationChange::Switch), Ordering::SeqCst);
    }

    /// Stop and recovery publication share the raw lock; delivery cannot announce after this.
    fn begin_stopping(&self, _delivery: &MutexGuard<'_, ()>) {
        let _consumer = self.raw_consumer.lock().unwrap_or_else(|e| e.into_inner());
        self.stopping.store(true, Ordering::SeqCst);
        self.recovered_pending.store(false, Ordering::SeqCst);
    }

    fn push_event(&self, ev: Event) {
        // Recover the VecDeque and continue even if poisoned; events are not torn.
        let mut q = self.events.lock().unwrap_or_else(|e| e.into_inner());
        q.push_back(ev);
    }

    /// Close both delivery paths before publishing the terminal event. The caller
    /// stops the backend directly, without joining the watchdog from itself.
    fn deny_permission(&self, permission: Permission, detail: String) {
        let delivery = self.delivery.lock().unwrap_or_else(|e| e.into_inner());
        self.deny_permission_locked(permission, detail, &delivery);
    }

    fn deny_permission_locked(
        &self,
        permission: Permission,
        detail: String,
        delivery: &MutexGuard<'_, ()>,
    ) {
        self.fail_terminal_locked(Error::PermissionDenied { permission, detail }, delivery);
    }

    fn fail_terminal_locked(&self, error: Error, delivery: &MutexGuard<'_, ()>) {
        if self.terminal.record(error.clone()) {
            self.begin_stopping(delivery);
            self.push_event(match error {
                Error::PermissionDenied { permission, detail } => {
                    Event::PermissionDenied { permission, detail }
                }
                error => Event::TerminalError { error },
            });
        }
    }
}

/// Call backend `start(sink)` inside [`catch_unwind`](std::panic::catch_unwind). If the backend
/// panics, convert it to [`Error::Backend`] before it can poison the mutex (the caller can surface it
/// as `Event::Error`/`Err`).
///
/// `&mut Box<dyn CaptureBackend>` is not `UnwindSafe`, so wrap it in [`AssertUnwindSafe`]. This is
/// safe because after catching a panic this function only returns `Err`; the caller treats the
/// possibly corrupted backend as failed and proceeds to stop/reopen/drop instead of reusing it. The
/// lock guard is held and dropped normally, so the mutex is not poisoned.
fn start_backend_catching(be: &mut Box<dyn CaptureBackend>, sink: RawSink) -> Result<()> {
    match std::panic::catch_unwind(AssertUnwindSafe(|| be.start(sink))) {
        Ok(res) => res,
        Err(_) => Err(Error::Backend("backend panicked during start()".into())),
    }
}

/// Call backend `stop()` inside [`catch_unwind`](std::panic::catch_unwind). Since stop returns `()`,
/// swallow a panic and continue (propagating another panic during shutdown has no benefit; this
/// prevents mutex poisoning and panic cascades). Returns `true` on normal stop and `false` if a panic
/// was caught (for observation/diagnostics).
///
/// The safety rationale for `AssertUnwindSafe` is the same as in [`start_backend_catching`]: the
/// backend is not reused after a caught panic, and the guard is dropped normally.
#[must_use]
fn stop_backend_catching(be: &mut Box<dyn CaptureBackend>) -> bool {
    std::panic::catch_unwind(AssertUnwindSafe(|| be.stop())).is_ok()
}

impl Stream {
    /// Open a stream from the configuration and backend (capture has not started yet).
    ///
    /// The fixed contract assumes `config.chunk_ms` is 20ms. `ring_capacity_chunks` sets the chunk
    /// ring capacity. Configure [`Normalizer`] from the backend's
    /// [`native_format`](CaptureBackend::native_format).
    pub fn open(config: StreamConfig, backend: Box<dyn CaptureBackend>) -> Result<Stream> {
        if config.ring_capacity_chunks == 0 {
            return Err(Error::InvalidArg("ring_capacity_chunks must be > 0".into()));
        }
        // Input gain must be finite and at least 0.0 (NaN, infinity, and negative values are InvalidArg).
        if !config.gain.is_finite() || config.gain < 0.0 {
            return Err(Error::InvalidArg(format!(
                "gain must be finite and >= 0.0, got {}",
                config.gain
            )));
        }
        // Validate that the output format is supported (otherwise UnsupportedFormat).
        config.output.validate()?;
        crate::validate_exclude_pids(&config)?;
        // Validate the secondary output format the same way, when configured.
        if let Some(sec) = config.secondary_output {
            sec.validate()?;
        }
        let native_format = backend.native_format();
        if native_format.0 == 0 || native_format.1 == 0 {
            return Err(Error::InvalidArg(
                "backend native_format must have non-zero rate and channels".into(),
            ));
        }

        let (chunk_producer, chunk_consumer) = chunk_ring(config.ring_capacity_chunks);
        // Create a dedicated ring only when a secondary tap is configured (public chunk_ring<AudioChunk> stays unchanged).
        let (secondary_producer, secondary_consumer) = if config.secondary_output.is_some() {
            let (p, c) = secondary_chunk_ring(config.ring_capacity_chunks);
            (Some(p), Some(c))
        } else {
            (None, None)
        };
        let events = Arc::new(Mutex::new(VecDeque::new()));

        let shared = Arc::new(SharedState {
            primary_frame_index: AtomicU64::new(0),
            secondary_frame_index: AtomicU64::new(0),
            capture_frame_index: AtomicU64::new(0),
            capture_enabled: AtomicBool::new(false),
            capture_producer: Mutex::new(None),
            backend: Mutex::new(backend),
            raw_consumer: Mutex::new(None),
            raw_generation: AtomicU64::new(0),
            last_sample_ns: AtomicI64::new(0),
            stopping: AtomicBool::new(false),
            terminal: TerminalFailure::default(),
            recovered_pending: AtomicBool::new(false),
            events: events.clone(),
            chunk_producer: Mutex::new(Some(chunk_producer)),
            native_format: Mutex::new(native_format),
            switching: AtomicBool::new(false),
            discontinuity_pending: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            delivery: Mutex::new(()),
            resume_generation: AtomicU64::new(0),
            gain_bits: Arc::new(AtomicU32::new(config.gain.to_bits())),
            recording_epoch_ns: AtomicI64::new(i64::MIN),
            denoise_enabled: AtomicBool::new(false),
            secondary_producer: Mutex::new(secondary_producer),
        });

        Ok(Stream {
            config,
            shared,
            chunk_consumer,
            capture_consumer: None,
            secondary_consumer,
            events,
            worker: None,
            watchdog: None,
            started: false,
        })
    }

    /// Enable the canonical 48 kHz stereo branch before starting capture.
    /// The producer applies the same denoise and gain snapshot used by output taps.
    /// This bounded queue is consumed off-callback; gaps retain producer frame indices.
    pub fn enable_capture_tap(&mut self) -> Result<()> {
        if self.started {
            return Err(Error::InvalidArg(
                "capture tap must be enabled before start".into(),
            ));
        }
        if self.capture_consumer.is_none() {
            let (producer, consumer) = chunk_ring(self.config.ring_capacity_chunks);
            *self
                .shared
                .capture_producer
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(producer);
            self.capture_consumer = Some(consumer);
            self.shared.capture_enabled.store(true, Ordering::SeqCst);
        }
        Ok(())
    }

    /// Poll valid canonical PCM for attached processing; frames exclude transport padding.
    /// `frame_index` is the authoritative canonical source origin for WhisperVadTap.
    pub fn poll_capture(&mut self) -> Option<AudioChunk> {
        self.capture_consumer.as_mut()?.try_pop()
    }

    /// Start capture, the intake worker and watchdog. Repeated calls while running are a no-op.
    /// A stopped stream is spent; native backend restarts happen inside the same intake lifetime.
    pub fn start(&mut self) -> Result<()> {
        if let Some(error) = self.terminal_error() {
            return Err(error);
        }
        if self.started {
            return Ok(());
        }
        // The intake owns these producers for its lifetime. Reject a spent
        // stream before changing state or starting the backend.
        let chunk_producer = self
            .shared
            .chunk_producer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .ok_or_else(|| Error::InvalidState("chunk producer already taken".into()))?;
        let secondary_producer = self
            .shared
            .secondary_producer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        self.shared.stopping.store(false, Ordering::SeqCst);

        // A new start does not carry over a paused state (even if the previous run was stopped while
        // paused, restart in the normal state).
        self.shared.paused.store(false, Ordering::SeqCst);

        // Start a new zero-based clock for this recording (do not carry over the previous epoch).
        self.shared
            .recording_epoch_ns
            .store(i64::MIN, Ordering::SeqCst);

        // First backend startup: create RawRing and pass its sink to the backend.
        if let Err(error) = Self::open_backend_once(&self.shared, GenerationChange::Initial) {
            // Permission denial already closes delivery and stops the backend
            // in open_backend_once. Other failures still need startup cleanup.
            if !matches!(error, Error::PermissionDenied { .. }) {
                self.stop();
            }
            *self
                .shared
                .chunk_producer
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(chunk_producer);
            *self
                .shared
                .secondary_producer
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = secondary_producer;
            return Err(error);
        }

        // Intake takes its own snapshot when scheduled, since this format hint
        // can become stale before the worker starts.
        let worker_shared = self.shared.clone();
        // Recover and continue even if poisoned (only reads the inner (u32, u16)).
        let initial_native = *self
            .shared
            .native_format
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let output = self.config.output;
        let secondary_output = self.config.secondary_output;
        let worker = match thread::Builder::new()
            .name("flexaudio-intake".into())
            .spawn(move || {
                run_intake(
                    worker_shared,
                    chunk_producer,
                    secondary_producer,
                    initial_native,
                    output,
                    secondary_output,
                );
            }) {
            Ok(worker) => worker,
            Err(error) => {
                self.stop();
                return Err(Error::Backend(format!("spawn intake thread: {error}")));
            }
        };
        self.worker = Some(worker);

        // Watchdog thread.
        let wd_shared = self.shared.clone();
        let watchdog = match thread::Builder::new()
            .name("flexaudio-watchdog".into())
            .spawn(move || {
                run_watchdog(wd_shared);
            }) {
            Ok(watchdog) => watchdog,
            Err(error) => {
                self.stop();
                return Err(Error::Backend(format!("spawn watchdog thread: {error}")));
            }
        };
        self.watchdog = Some(watchdog);

        self.started = true;
        Ok(())
    }

    /// Stop capture and join all threads.
    ///
    /// Safe for reentry and repeated stop calls. After stop, drain chunks already buffered in the
    /// ring with [`poll_chunk`](Self::poll_chunk).
    pub fn stop(&mut self) {
        // Stop the backend to end its producer thread (stop RT pushes).
        // Recover even if poisoned and attempt stop. If stop panics, catch_unwind swallows it so the
        // mutex is not poisoned again and we can proceed to join (no silent death).
        {
            let mut be = self
                .shared
                .backend
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            stop_backend_reconciling(&self.shared, &mut be, true);
        }

        // Join threads.
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
        if let Some(h) = self.watchdog.take() {
            let _ = h.join();
        }

        self.started = false;
    }

    /// Temporarily pause capture delivery.
    ///
    /// Keep OS-side capture running but stop delivering completed chunks. While paused,
    /// [`poll_chunk`](Self::poll_chunk) returns no new chunks. Intake continues internally to keep the
    /// device active, enabling a quick resume and avoiding false watchdog stall reports. Does nothing
    /// if already paused (safe to call repeatedly).
    ///
    /// When this call returns, any chunk being assembled by the intake thread has either been queued
    /// or discarded. After draining chunks already queued, [`poll_chunk`](Self::poll_chunk) returns no
    /// new chunks.
    ///
    /// Calling this before [`start`](Self::start) sets the flag, but it takes effect only once intake starts.
    pub fn pause(&self) {
        // Set the flag while holding delivery. The intake thread uses the same lock for pushes, so no
        // assembled chunk can enter the queue after this returns.
        let _g = self
            .shared
            .delivery
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        self.shared.paused.store(true, Ordering::SeqCst);
    }

    /// Resume delivery after [`pause`](Self::pause).
    ///
    /// After resume, mark the first chunk delivered on each stream (primary and secondary) with
    /// [`ChunkFlags::DISCONTINUITY`] to signal the time gap to consumers. Each stream's `seq` remains
    /// continuous across the pause; no silence is inserted for the paused interval. Does nothing if
    /// not paused (safe to call repeatedly).
    /// Returns the stored permission error if capture has terminally failed.
    pub fn resume(&self) -> Result<()> {
        // Hold delivery while advancing the generation and setting paused=false, making this one
        // enqueue boundary. Intake reads the generation under the same lock, so the first chunk for
        // each tap after resume is reliably marked DISCONTINUITY.
        let _g = self
            .shared
            .delivery
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Delivery is already held: inspect the recorded cause without acquiring
        // backend in reverse order. Only terminal_error() waits for OS shutdown.
        if let Some(error) = self.shared.terminal.error() {
            return Err(error);
        }
        // Advance the generation only if actually paused (resume on an unpaused stream must not add
        // an unnecessary DISCONTINUITY).
        if self.shared.paused.load(Ordering::SeqCst) {
            self.shared.resume_generation.fetch_add(1, Ordering::SeqCst);
            self.shared.paused.store(false, Ordering::SeqCst);
        }
        Ok(())
    }

    /// Whether currently paused.
    pub fn is_paused(&self) -> bool {
        self.shared.paused.load(Ordering::SeqCst)
    }

    /// Change input gain (linear multiplier). 1.0 = unchanged, 2.0 ≈ +6 dB, 0.0 = silence.
    ///
    /// May be called at any time during recording; takes effect from the next completed chunk (20 ms
    /// granularity). Samples are clamped to `-1.0..=1.0` after multiplication. At 1.0, samples are
    /// untouched (byte-for-byte passthrough). Values must be finite and >= 0, or
    /// [`Error::InvalidArg`] is returned and the current value remains unchanged.
    pub fn set_gain(&self, gain: f32) -> Result<()> {
        if !gain.is_finite() || gain < 0.0 {
            return Err(Error::InvalidArg(format!(
                "gain must be finite and >= 0.0, got {gain}"
            )));
        }
        self.shared
            .gain_bits
            .store(gain.to_bits(), Ordering::Relaxed);
        Ok(())
    }

    /// Current input gain (linear multiplier).
    pub fn gain(&self) -> f32 {
        f32::from_bits(self.shared.gain_bits.load(Ordering::Relaxed))
    }

    /// Retrieve one completed chunk (non-blocking). Returns `None` if none is available.
    ///
    /// Returned chunks contain interleaved `f32` in the output format (`config.output`). Chunks are
    /// fixed at 20 ms, with `data.len() == frames * output.channels`. With the default `{48000, 2}`,
    /// `frames == 960` (`data.len() == 1920`). `peak`/`rms` are computed from final data. `seq`
    /// increases monotonically.
    pub fn poll_chunk(&mut self) -> Option<AudioChunk> {
        let _delivery = self
            .shared
            .delivery
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if self.shared.terminal.is_failed() {
            return None;
        }
        self.chunk_consumer.try_pop()
    }

    /// Retrieve one completed secondary tap chunk (non-blocking). Returns `None` if none is available.
    ///
    /// Secondary chunks are generated only when `config.secondary_output` is `Some`; otherwise this
    /// always returns `None`. Secondary PTS values use the same zero-based recording clock as the
    /// primary [`AudioChunk`], but are independent and lag the primary by 20–60 ms due to group delay
    /// in the secondary Stage2 resampler. Match primary and secondary by `pts_ns` (time), since each
    /// tap has its own `seq` counter.
    pub fn poll_secondary(&mut self) -> Option<SecondaryChunk> {
        let _delivery = self
            .shared
            .delivery
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if self.shared.terminal.is_failed() {
            return None;
        }
        self.secondary_consumer.as_mut().and_then(|c| c.try_pop())
    }

    /// Terminal capture failure, retained after stop. Confirmed permission denial
    /// is terminal; a backend mailbox that cannot be reconciled also fails closed.
    /// Once this returns `Some`, backend shutdown has finished, including both
    /// children of a Mix source. This may wait for the control thread to finish
    /// stopping capture; audio delivery is gated immediately when failure is recorded.
    /// Create a new stream after changing OS settings and restarting the app.
    pub fn terminal_error(&self) -> Option<Error> {
        if !self.shared.terminal.is_failed() {
            return None;
        }
        // Every terminal shutdown holds backend until stop returns. Take the same
        // lock before exposing the error, without holding delivery or joining any
        // threads here. Internal callers already holding backend read the stored
        // cause directly rather than recursively entering this accessor.
        let _backend = self
            .shared
            .backend
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        self.shared.terminal.error()
    }

    /// Enable or disable noise suppression (RNNoise) in the internal canonical format.
    ///
    /// Call before [`start`](Self::start) (applied when the intake thread builds Normalizer; changes
    /// during recording take effect on the next generation change, a source switch or automatic
    /// recovery). When enabled, denoise runs once on the 48kHz/stereo internal canonical format, so
    /// both taps receive denoised audio (+10ms fixed latency). Core does not depend on denoise; this
    /// facade injects the implementation.
    pub fn set_denoise(&self, enabled: bool) {
        let _consumer = self
            .shared
            .raw_consumer
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        self.shared.denoise_enabled.store(enabled, Ordering::SeqCst);
    }

    /// Retrieve one undelivered event (non-blocking). Returns `None` if none is available.
    pub fn poll_event(&mut self) -> Option<Event> {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
    }

    /// Total chunks discarded by the chunk ring through DROP_OLDEST.
    pub fn dropped_chunks(&self) -> u64 {
        self.chunk_consumer.dropped_count()
    }

    /// Reference to the current configuration.
    pub fn config(&self) -> &StreamConfig {
        &self.config
    }

    /// Native format `(sample_rate, channels)` of the current backend.
    ///
    /// Value obtained from the backend at open. Unchanged on watchdog recovery, but updated to the
    /// new backend's value when [`switch_source`](Self::switch_source) changes the source. For display
    /// and diagnostics (output format is `config().output`).
    pub fn native_format(&self) -> (u32, u16) {
        // Recover and read the value even if poisoned (avoid a panic cascade).
        *self
            .shared
            .native_format
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    // --- Internal ---

    /// (Re)start the current `shared.backend`, install a new RawRing/RawConsumer in shared state, and
    /// advance the generation. Used for both initial startup and watchdog reopen.
    ///
    /// Steps:
    /// 1. Read the current backend's [`native_format`](CaptureBackend::native_format).
    /// 2. Create a new RawRing with that rate/ch (do not carry over format residue from the old ring,
    ///    which would damage phase).
    /// 3. Start the backend.
    /// 4. Publish native format, RawConsumer, generation and flags under the raw-consumer lock.
    /// 5. Set `last_sample_ns` to now to avoid an immediate stall check.
    ///
    /// Acquire the backend lock only while starting (the caller must not hold the lock). The
    /// low-level switch ([`switch_backend`](Self::switch_backend)) directly replaces the backend and
    /// does not use this function, except when restoring the old source after a failed switch.
    fn open_backend_once(shared: &Arc<SharedState>, change: GenerationChange) -> Result<()> {
        // Keep the backend stable from format lookup through raw-ring publication.
        let mut be = shared.backend.lock().unwrap_or_else(|e| e.into_inner());
        let (rate, channels) = be.native_format();

        // New RawRing (do not carry over residue from the old format).
        let (producer, consumer) = raw_ring(RAW_RING_SAMPLES);
        let sink = RawSink::new(producer, rate, channels);

        {
            // Recover even if poisoned. If backend start() panics, catch_unwind converts it to
            // Error::Backend before mutex poisoning, so `?` returns it to the caller (start() returns
            // Err / watchdog emits Event::Error).
            if let Some(error) = shared.terminal.error() {
                return Err(error);
            }
            if let Err(error) = start_backend_catching(&mut be, sink) {
                if let Error::PermissionDenied { permission, detail } = &error {
                    shared.deny_permission(*permission, detail.clone());
                    let _ = stop_backend_catching(&mut be);
                }
                return Err(error);
            }
        }

        // Install the new consumer in shared state and advance the generation (drop the old consumer).
        {
            // Recover and install it even if poisoned (only replace the inner Option).
            let mut rc = shared
                .raw_consumer
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            shared.publish_raw(&mut rc, consumer, (rate, channels), change);
        }

        // Treat startup as the last sample arrival to avoid an immediate stall check.
        shared
            .last_sample_ns
            .store(monotonic_now_ns(), Ordering::SeqCst);
        Ok(())
    }

    /// Low-level source switch. Replace the current backend with a new one while preserving chunk
    /// stream continuity (seq and PTS).
    ///
    /// `seq` is local to the intake thread and is not stored in the backend or `SharedState`, so it
    /// remains continuous if left untouched here. On a generation change, the intake thread rebuilds
    /// `Normalizer`/`ClockNormalizer` and re-anchors PTS to the actual arrival time of the new
    /// source's first sample, preserving monotonicity.
    ///
    /// Steps (increment generation once at the end; all atomics use SeqCst):
    /// - If not started, return [`Error::InvalidState`].
    /// - Set `switching = true` (prevent concurrent watchdog reopen).
    /// - Under the backend lock, stop the old backend → read the new backend's native format and
    ///   update `shared.native_format` → create a new RawRing → call `new_backend.start(sink)`.
    ///   - On success, replace the backend and install the new consumer (drop the old one).
    ///   - On failure, restart the old backend with [`open_backend_once`](Self::open_backend_once) and
    ///     continue the old source (preserve continuity). Set `discontinuity_pending`, increment
    ///     generation, set `switching=false`, and return `Err`.
    /// - On success, set `discontinuity_pending = true` (intentional switch, so do not set RECOVERED)
    ///   → increment generation once at the end → set `last_sample_ns = now` → set `switching = false`
    ///   → return `Ok`.
    ///
    /// Takes [`Box<dyn CaptureBackend>`] directly so mock backends can verify switch behavior. The
    /// high-level entry point is [`switch_source`](Self::switch_source).
    ///
    /// `#[doc(hidden)] pub`: not part of the public API (omitted from docs), but allows integration
    /// tests in another crate (`tests/integration.rs`) to call this with a MockBackend.
    #[doc(hidden)]
    pub fn switch_backend(&mut self, new_backend: Box<dyn CaptureBackend>) -> Result<()> {
        self.switch_backend_inner(new_backend, None)
    }

    /// Publish a backend and its denoise setting as one capture generation.
    #[doc(hidden)]
    pub fn switch_backend_with_denoise(
        &mut self,
        new_backend: Box<dyn CaptureBackend>,
        denoise: bool,
    ) -> Result<()> {
        self.switch_backend_inner(new_backend, Some(denoise))
    }

    fn switch_backend_inner(
        &mut self,
        new_backend: Box<dyn CaptureBackend>,
        denoise: Option<bool>,
    ) -> Result<()> {
        if let Some(error) = self.terminal_error() {
            return Err(error);
        }
        if !self.started {
            return Err(Error::InvalidState(
                "switch_backend is only available on a started stream".into(),
            ));
        }

        // Begin switch: pause the watchdog first to avoid racing its stall recovery.
        self.shared.switching.store(true, Ordering::SeqCst);

        // Under the backend lock, stop the old backend and start the new one as one operation.
        // Recover even if poisoned (the lock spans stop/start and may be poisoned; recovering it lets
        // the replacement proceed correctly).
        {
            let mut be = self
                .shared
                .backend
                .lock()
                .unwrap_or_else(|e| e.into_inner());

            // Switching and recovery share shutdown reconciliation so neither can
            // discard a denial produced while the previous owner is joining.
            stop_backend_reconciling(&self.shared, &mut be, false);
            // Shutdown has completed and backend is already locked here.
            if let Some(error) = self.shared.terminal.error() {
                self.shared.switching.store(false, Ordering::SeqCst);
                return Err(error);
            }

            // Native format of the new backend.
            let (rate, channels) = new_backend.native_format();

            // New RawRing (do not carry over old format residue).
            let (producer, consumer) = raw_ring(RAW_RING_SAMPLES);
            let sink = RawSink::new(producer, rate, channels);

            // Start the new backend. catch_unwind converts a panic to Error::Backend, so it follows
            // the Err branch below (restore old source → return Err). Restore the old source on failure.
            let mut new_backend = new_backend;
            match start_backend_catching(&mut new_backend, sink) {
                Ok(()) => {
                    // Publish the format, ring, generation and pending flag as one
                    // operation with respect to intake snapshots.
                    let mut rc = self
                        .shared
                        .raw_consumer
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    // Treat startup as the last arrival time (avoid an immediate stall check).
                    self.shared
                        .last_sample_ns
                        .store(monotonic_now_ns(), Ordering::SeqCst);
                    // Replace with the new backend (drop the old backend).
                    *be = new_backend;
                    if let Some(enabled) = denoise {
                        self.shared.denoise_enabled.store(enabled, Ordering::SeqCst);
                    }
                    self.shared.publish_raw(
                        &mut rc,
                        consumer,
                        (rate, channels),
                        GenerationChange::Switch,
                    );
                }
                Err(e) => {
                    let _ = stop_backend_catching(&mut new_backend);
                    if let Error::PermissionDenied { permission, detail } = &e {
                        self.shared.deny_permission(*permission, detail.clone());
                        self.shared.switching.store(false, Ordering::SeqCst);
                        return Err(e);
                    }
                    // New source startup failed → restart the old backend (still `*be`) and continue.
                    // Release the backend lock before restoring it, or open_backend_once will lock it
                    // again and deadlock.
                    drop(be);
                    // Reopen the old backend (native_format returns to the old backend's value).
                    // Resuming the old source is also discontinuous (it was interrupted briefly).
                    let restored = Self::open_backend_once(&self.shared, GenerationChange::Switch);
                    // open_backend_once already incremented generation. Reset switching and return Err.
                    self.shared.switching.store(false, Ordering::SeqCst);
                    if let Err(error @ Error::PermissionDenied { .. }) = restored {
                        return Err(error);
                    }
                    return Err(e);
                }
            }
        }

        // --- Switch succeeded ---
        // The new generation was published under the backend and raw-consumer locks.
        self.shared.switching.store(false, Ordering::SeqCst);
        Ok(())
    }

    /// High-level entry point to switch input source (mic/system/process) without stopping recording.
    ///
    /// Build the source-specific backend from `new_config` with `build_backend` (private to the
    /// facade); on failure, return `Err` with the old source untouched. Replace it with
    /// [`switch_backend`](Self::switch_backend). The output format (`output`) cannot change because
    /// changes to chunk frames/data.len would break the continuous stream. Reject such requests with
    /// [`Error::InvalidArg`].
    ///
    /// On success, update only the mutable `config` fields (`kind` / `device_id` / `target_pid` /
    /// `mode` / `exclude_self` / `exclude_pids`). Keep `output` / `chunk_ms` /
    /// `ring_capacity_chunks` unchanged. Ignore `new_config.gain` too (gain is stream state and does
    /// not change on source switch; change it with [`set_gain`](Self::set_gain)).
    ///
    /// # Errors
    /// - Not started → [`Error::InvalidState`].
    /// - Request to change `output` → [`Error::InvalidArg`].
    /// - New backend construction fails (missing process PID, unsupported OS, etc.) → error from
    ///   `build_backend` (private to the facade); the old source remains untouched.
    /// - New backend start fails → [`switch_backend`](Self::switch_backend) restores the old source
    ///   and returns the error.
    pub fn switch_source(&mut self, new_config: StreamConfig) -> Result<()> {
        self.switch_source_inner(new_config, None)
    }

    /// Switch source and apply denoise atomically with the new capture generation.
    pub fn switch_source_with_denoise(
        &mut self,
        new_config: StreamConfig,
        denoise: bool,
    ) -> Result<()> {
        self.switch_source_inner(new_config, Some(denoise))
    }

    fn switch_source_inner(
        &mut self,
        new_config: StreamConfig,
        denoise: Option<bool>,
    ) -> Result<()> {
        if let Some(error) = self.terminal_error() {
            return Err(error);
        }
        if !self.started {
            return Err(Error::InvalidState(
                "switch_source is only available on a started stream".into(),
            ));
        }
        if new_config.output != self.config.output {
            return Err(Error::InvalidArg(
                "output format cannot change during switch_source".into(),
            ));
        }
        // The secondary tap format is also fixed at open (do not rebuild its Normalizer/ring on switch).
        if new_config.secondary_output != self.config.secondary_output {
            return Err(Error::InvalidArg(
                "secondary output format cannot change during switch_source".into(),
            ));
        }
        // Build the new source backend (on failure, return early with the old source untouched).
        crate::validate_exclude_pids(&new_config)?;
        let backend = match crate::build_backend(&new_config) {
            Ok(backend) => backend,
            Err(error) => {
                if let Error::PermissionDenied { permission, detail } = &error {
                    let mut be = self
                        .shared
                        .backend
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    self.shared.deny_permission(*permission, detail.clone());
                    let _ = stop_backend_catching(&mut be);
                }
                return Err(error);
            }
        };
        // Swap it in (switch_backend guarantees continuity).
        self.switch_backend_inner(backend, denoise)?;
        // Update mutable config fields only on success (keep output and other fields unchanged).
        self.config = StreamConfig {
            kind: new_config.kind,
            device_id: new_config.device_id,
            target_pid: new_config.target_pid,
            mode: new_config.mode,
            exclude_self: new_config.exclude_self,
            exclude_pids: new_config.exclude_pids,
            ..self.config.clone()
        };
        Ok(())
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        if self.started {
            self.stop();
        }
    }
}

/// Convert to an absolute timestamp starting at zero when recording begins. Set the computed PTS of
/// the first delivered (primary) chunk as the recording epoch once (replacing the `i64::MIN`
/// sentinel), then subtract it from every chunk. Only the intake thread writes this, so there is no
/// race. The epoch remains fixed across reopen/switch, keeping PTS continuous from zero throughout
/// the recording.
fn apply_epoch(shared: &SharedState, raw_pts: i64) -> i64 {
    let epoch = shared.recording_epoch_ns.load(Ordering::SeqCst);
    if epoch == i64::MIN {
        shared.recording_epoch_ns.store(raw_pts, Ordering::SeqCst);
        0
    } else {
        raw_pts - epoch
    }
}

/// Apply input gain (linear multiplier) to a completed chunk's `data`. At 1.0, leave samples
/// untouched (byte-for-byte passthrough). Otherwise multiply each sample and clamp to ±1.0.
fn apply_gain(data: &mut [f32], gain: f32) {
    if gain != 1.0 {
        for x in data.iter_mut() {
            *x = (*x * gain).clamp(-1.0, 1.0);
        }
    }
}

/// Intake/processing thread body.
///
/// Pop from RawConsumer → feed [`Normalizer`] (shared Stage1 → split to primary/secondary Stage2)
/// → add `seq`, recording-zero-based PTS, peak/rms, and discontinuity flags to completed chunks →
/// push to the primary/secondary rings. On a generation change (reopen/source switch), rebuild the
/// Normalizer/Clock and set RECOVERED|DISCONTINUITY on the next chunk. Detect RawRing overflow
/// (capture-side data loss) too, and set DISCONTINUITY on the next primary and secondary chunks.
/// Flush the Normalizer on stop to emit its final tail.
fn run_intake(
    shared: Arc<SharedState>,
    mut chunk_producer: ChunkProducer,
    mut secondary_producer: Option<SecondaryChunkProducer>,
    _initial_native: (u32, u16),
    output: OutputFormat,
    secondary_output: Option<OutputFormat>,
) {
    // Startup arguments can become stale before this thread is scheduled.
    let initial = shared.snapshot_raw(&mut []);
    let (rate, channels) = initial.native_format;
    // If Normalizer construction fails (for example, rubato setup), emit Event::Error and exit rather
    // than dying silently.
    let mut normalizer = match build_normalizer(
        &shared,
        rate,
        channels,
        output,
        secondary_output,
        initial.denoise_enabled,
    ) {
        Ok(n) => n,
        Err(e) => {
            shared.push_event(Event::Error(format!("normalizer init failed: {e}")));
            return;
        }
    };
    let mut clock = ClockNormalizer::new();
    let mut primary_state = tap_drain::TapState::default();
    let mut secondary_state = tap_drain::TapState::default();
    let mut current_generation = initial.generation;
    let mut capture_state = tap_drain::TapState {
        discontinuity: shared.primary_frame_index.load(Ordering::SeqCst) != 0,
        ..Default::default()
    };
    let mut capture_producer = shared
        .capture_producer
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    // Each tap retains flags and resume state until its own first delivery.
    let mut overflow_baseline: u64 = 0;

    // Pop scratch buffer (sized to the RawRing capacity so the whole ring can be drained in one call).
    let mut scratch = vec![0.0f32; RAW_RING_SAMPLES];

    'intake: loop {
        if shared.terminal.is_failed() {
            break;
        }
        let stopping = shared.stopping.load(Ordering::SeqCst);

        // Detect a generation change (reopen/source switch) and reset for the new source. Since
        // native_format may change, reread shared state and rebuild the native-dependent Stage 1
        // Normalizer. Do not reset the recording epoch here; timestamps stay continuous from zero
        // across generations.
        let snapshot = shared.snapshot_raw(&mut scratch);
        let gen = snapshot.generation;
        if gen != current_generation {
            if let Err(error) = advance_frame_index(
                &shared.capture_frame_index,
                normalizer.buffered_capture_frames() as u64,
                48_000,
            ) {
                shared.push_event(Event::Error(error.to_string()));
                break 'intake;
            }
            current_generation = gen;
            capture_state.discontinuity = true;
            // Pending delivery flags belong to the discarded normalizer, never its replacement.
            primary_state.recovered = false;
            secondary_state.recovered = false;
            let (rate, channels) = snapshot.native_format;
            normalizer = match build_normalizer(
                &shared,
                rate,
                channels,
                output,
                secondary_output,
                snapshot.denoise_enabled,
            ) {
                Ok(n) => n,
                Err(e) => {
                    shared.push_event(Event::Error(format!(
                        "normalizer rebuild failed after source change: {e}"
                    )));
                    return;
                }
            };
            clock = ClockNormalizer::new();
            // The new RawConsumer counts overflows from 0 (avoid a falsely huge delta).
            overflow_baseline = 0;
        }

        // Fan out shared pending flags to each tap's local state. The watchdog is stopped by
        // switching during a source switch, so both flags should not be set together; if they are,
        // they are combined with OR.
        if snapshot.recovered || snapshot.discontinuity {
            capture_state.discontinuity = true;
        }
        if snapshot.recovered {
            primary_state.recovered = true;
            secondary_state.recovered = true;
        }
        if snapshot.discontinuity {
            primary_state.discontinuity = true;
            secondary_state.discontinuity = true;
        }

        // Drain RawRing into the Normalizer and observe overflows (capture-side data loss).
        let mut produced_any = false;
        let mut push_err: Option<Error> = None;
        let overflow_now = snapshot.overflows;
        if shared.capture_enabled.load(Ordering::SeqCst) && overflow_now > overflow_baseline {
            if let Err(error) = advance_frame_index(
                &shared.capture_frame_index,
                normalizer.buffered_capture_frames() as u64,
                48_000,
            ) {
                shared.push_event(Event::Error(error.to_string()));
                break 'intake;
            }
            // No DSP history or partial chunk may bridge capture-side loss. Rebuild
            // the normalized representation and reanchor its PTS, keeping stream counters.
            let (rate, channels) = snapshot.native_format;
            normalizer = match build_normalizer(
                &shared,
                rate,
                channels,
                output,
                secondary_output,
                snapshot.denoise_enabled,
            ) {
                Ok(normalizer) => normalizer,
                Err(error) => {
                    shared.push_event(Event::Error(format!(
                        "normalizer rebuild after capture loss: {error}"
                    )));
                    break 'intake;
                }
            };
            clock = ClockNormalizer::new();
        }
        if snapshot.samples > 0 {
            let samples = &scratch[..snapshot.samples];
            // Device PTS: monotonic approximation based on the native sample rate (arrival time).
            let device_pts = monotonic_now_ns();
            let norm_pts = clock.normalize(device_pts);
            if let Err(e) = normalizer.push(samples, norm_pts) {
                push_err = Some(e);
            } else {
                shared
                    .last_sample_ns
                    .store(monotonic_now_ns(), Ordering::SeqCst);
                produced_any = true;
            }
        }

        // If push failed, emit Event::Error and exit intake (do not die silently).
        if let Some(e) = push_err {
            shared.push_event(Event::Error(format!("normalizer push failed: {e}")));
            return;
        }

        // On RawRing overflow (RT outran intake and discarded samples), set DISCONTINUITY on the
        // next primary and secondary chunks (loss before normalization affects both taps equally).
        // The PTS already reflects the gap through wall-clock re-anchoring.
        if overflow_now > overflow_baseline {
            capture_state.discontinuity = true;
            primary_state.discontinuity = true;
            secondary_state.discontinuity = true;
        }
        overflow_baseline = overflow_now;

        // On stop, flush and emit the tail (denoise delay line + resampler remainder).
        if stopping {
            normalizer.flush();
        }

        // Output gain stays after conversion. The capture copy gets the same snapshot
        // independently, before WhisperVadTap's mono/16 kHz conversion.
        let gain = f32::from_bits(shared.gain_bits.load(Ordering::Relaxed));
        let drained = (|| -> Result<bool> {
            if let Some(producer) = capture_producer.as_mut() {
                tap_drain::drain(
                    &shared,
                    || normalizer.pop_capture(stopping),
                    OutputFormat::default(),
                    gain,
                    &shared.capture_frame_index,
                    &mut capture_state,
                    tap_drain::Ring::Capture(producer),
                )?;
            }
            let mut emitted = tap_drain::drain(
                &shared,
                || normalizer.pop_chunk(),
                output,
                gain,
                &shared.primary_frame_index,
                &mut primary_state,
                tap_drain::Ring::Primary(&mut chunk_producer),
            )?;
            if let (Some(producer), Some(format)) = (secondary_producer.as_mut(), secondary_output)
            {
                emitted |= tap_drain::drain(
                    &shared,
                    || normalizer.pop_secondary(),
                    format,
                    gain,
                    &shared.secondary_frame_index,
                    &mut secondary_state,
                    tap_drain::Ring::Secondary(producer),
                )?;
            }
            Ok(emitted)
        })();
        let emitted_any = match drained {
            Ok(emitted) => emitted,
            Err(error) => {
                shared.push_event(Event::Error(error.to_string()));
                break 'intake;
            }
        };

        // If stop was requested, the tail has been flushed; exit.
        if stopping {
            break;
        }

        // Sleep briefly when there is no data to avoid busy-spinning the CPU.
        if !produced_any && !emitted_any {
            thread::sleep(Duration::from_millis(2));
        }
    }
    *shared
        .capture_producer
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = capture_producer;
}

/// Producer-only advancement, outside the realtime callback. Never wrap the clock.
fn advance_frame_index(counter: &AtomicU64, frames: u64, rate: u32) -> Result<u64> {
    if rate == 0 {
        return Err(Error::InvalidArg(
            "frame index sample rate must be nonzero".into(),
        ));
    }
    let previous = counter
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |index| {
            let next = index.checked_add(frames)?;
            u64::try_from(u128::from(next) * 48_000 / u128::from(rate)).ok()?;
            Some(next)
        })
        .map_err(|_| Error::InvalidState("canonical frame timeline exhausted".into()))?;
    u64::try_from(u128::from(previous) * 48_000 / u128::from(rate))
        .map_err(|_| Error::InvalidState("canonical frame timeline exhausted".into()))
}

/// Watchdog thread body.
///
/// Check the last sample arrival time on ~250 ms ticks. If samples stop for longer than
/// [`STALL_THRESHOLD`], reopen the backend with exponential backoff. Fire
/// [`Event::StreamStalled`] on a stall. On a successful reopen, set `recovered_pending` so the
/// intake thread announces [`Event::StreamRecovered`] only once real samples from the new
/// generation have been delivered (a silent reopen is not a recovery).
fn run_watchdog(shared: Arc<SharedState>) {
    let mut stalled = false;
    let mut backoff = BACKOFF_MIN;

    loop {
        if shared.stopping.load(Ordering::SeqCst) {
            break;
        }
        thread::sleep(WATCHDOG_TICK);
        if shared.stopping.load(Ordering::SeqCst) {
            break;
        }

        // Do not check for stalls or reopen during a source switch (switch_backend temporarily stops
        // the old backend, making it idle, so this prevents an incorrect concurrent reopen).
        // The switch updates last_sample_ns to now before finishing, so normal checks resume next tick.
        if shared.switching.load(Ordering::SeqCst) {
            continue;
        }

        // Notifications take precedence over stall recovery, including while audio
        // is flowing or delivery is paused. A denial never enters the reopen loop.
        if drain_backend_events(&shared) == MailboxDrain::BudgetExhausted {
            // More events may include a denial. Process them on the next tick
            // before any reopen can replace this generation's mailbox.
            continue;
        }
        if shared.stopping.load(Ordering::SeqCst) {
            break;
        }

        let now = monotonic_now_ns();
        let last = shared.last_sample_ns.load(Ordering::SeqCst);
        let idle_ns = now.saturating_sub(last);
        let idle = Duration::from_nanos(idle_ns.max(0) as u64);

        if !stalled {
            if idle >= STALL_THRESHOLD {
                // Stall detected.
                stalled = true;
                backoff = BACKOFF_MIN;
                shared.push_event(Event::StreamStalled);
            }
            continue;
        }

        // During a stall, stop the backend and try to reopen it.
        // Recover a poisoned lock and attempt stop. If stop panics, catch_unwind swallows it so the
        // watchdog can proceed to reopen instead of dying silently.
        {
            let mut be = shared.backend.lock().unwrap_or_else(|e| e.into_inner());
            stop_backend_reconciling(&shared, &mut be, false);
        }

        if shared.stopping.load(Ordering::SeqCst) {
            break;
        }

        let reopened = match Stream::open_backend_once(&shared, GenerationChange::Recovery) {
            Ok(()) => true,
            Err(e) => {
                if shared.terminal.is_failed() {
                    break;
                }
                shared.push_event(Event::Error(format!("reopen failed: {e}")));
                false
            }
        };

        if reopened {
            // The recovery flag was published with the generation before intake could pop it.
            // Only delivery announces recovery; a silent reopen remains pending until data arrives.
            stalled = false;
            backoff = BACKOFF_MIN;
        } else {
            // On failure, wait with exponential backoff and jitter before retrying.
            let jittered = jittered_backoff(backoff);
            sleep_interruptible(&shared, jittered);
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }
    }
}

/// Drain the backend's control mailbox with the same serialization as start/stop.
/// A backend poll panic is observable and cannot poison the backend mutex.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MailboxDrain {
    Empty,
    Terminal,
    BudgetExhausted,
}

fn drain_backend_events(shared: &SharedState) -> MailboxDrain {
    let mut be = shared.backend.lock().unwrap_or_else(|e| e.into_inner());
    if shared.switching.load(Ordering::SeqCst) || shared.stopping.load(Ordering::SeqCst) {
        return MailboxDrain::Empty;
    }
    drain_backend_events_locked(shared, &mut be, None)
}

/// Also used at shutdown/source replacement so final owner notifications cannot
/// be lost. Shutdown holds delivery across the owner's join to suppress a tail
/// if a denial arrives while capture is stopping.
fn drain_backend_events_locked(
    shared: &SharedState,
    be: &mut Box<dyn CaptureBackend>,
    delivery: Option<&MutexGuard<'_, ()>>,
) -> MailboxDrain {
    for _ in 0..MAX_BACKEND_EVENTS_PER_TICK {
        if shared.terminal.is_failed() {
            return MailboxDrain::Terminal;
        }
        let event = match std::panic::catch_unwind(AssertUnwindSafe(|| be.poll_event())) {
            Ok(event) => event,
            Err(_) => {
                shared.push_event(Event::Error("backend panicked during poll_event()".into()));
                return MailboxDrain::BudgetExhausted;
            }
        };
        match event {
            Some(Event::PermissionDenied { permission, detail }) => {
                if let Some(delivery) = delivery {
                    shared.deny_permission_locked(permission, detail, delivery);
                } else {
                    shared.deny_permission(permission, detail);
                }
                let _ = stop_backend_catching(be);
                return MailboxDrain::Terminal;
            }
            Some(Event::TerminalError { error }) => {
                if let Some(delivery) = delivery {
                    shared.fail_terminal_locked(error, delivery);
                } else {
                    let delivery = shared.delivery.lock().unwrap_or_else(|e| e.into_inner());
                    shared.fail_terminal_locked(error, &delivery);
                }
                let _ = stop_backend_catching(be);
                return MailboxDrain::Terminal;
            }
            Some(event) => shared.push_event(event),
            None => return MailboxDrain::Empty,
        }
    }
    MailboxDrain::BudgetExhausted
}

fn drain_final_backend_events(
    shared: &SharedState,
    be: &mut Box<dyn CaptureBackend>,
    delivery: &MutexGuard<'_, ()>,
) {
    for _ in 0..MAX_FINAL_EVENT_BATCHES {
        if drain_backend_events_locked(shared, be, Some(delivery)) != MailboxDrain::BudgetExhausted
        {
            return;
        }
    }
    shared.fail_terminal_locked(
        Error::Backend("backend event mailbox could not be reconciled; capture terminated before delivering buffered audio".into()),
        delivery,
    );
    let _ = stop_backend_catching(be);
}

/// One shutdown path for explicit stop, source replacement, and recovery.
/// Hold delivery across the owner's join and reconcile both sides of it, so a
/// queued or final denial suppresses the tail and prevents reopening.
fn stop_backend_reconciling(
    shared: &SharedState,
    be: &mut Box<dyn CaptureBackend>,
    stop_intake: bool,
) {
    let delivery = shared.delivery.lock().unwrap_or_else(|e| e.into_inner());
    drain_final_backend_events(shared, be, &delivery);
    if stop_intake {
        shared.begin_stopping(&delivery);
    }
    let _ = stop_backend_catching(be);
    drain_final_backend_events(shared, be, &delivery);
}

/// Compute peak (maximum absolute sample value) and RMS (root mean square, linear) from the final
/// interleaved `data` in the output format.
///
/// One pass over a 20 ms chunk (up to 1920 samples) is very cheap. Empty data returns `(0.0, 0.0)`.
fn peak_rms(data: &[f32]) -> (f32, f32) {
    if data.is_empty() {
        return (0.0, 0.0);
    }
    let mut peak = 0.0f32;
    let mut sum_sq = 0.0f64;
    for &x in data {
        let a = x.abs();
        if a > peak {
            peak = a;
        }
        sum_sq += (x as f64) * (x as f64);
    }
    let rms = (sum_sq / data.len() as f64).sqrt() as f32;
    (peak, rms)
}

/// Add slight time-based jitter (about ±12.5%) to the backoff, without using `rand`.
fn jittered_backoff(base: Duration) -> Duration {
    let base_ns = base.as_nanos() as u64;
    // Use the low bits of monotonic nanoseconds as a pseudo-random source.
    let entropy = monotonic_now_ns() as u64;
    // Range: ±(base/8).
    let span = (base_ns / 8).max(1);
    let delta = (entropy % (2 * span)) as i64 - span as i64;
    let result = base_ns as i64 + delta;
    Duration::from_nanos(result.max(0) as u64)
}

/// Sleep in short intervals while checking `stopping` (respond quickly to stop requests).
fn sleep_interruptible(shared: &Arc<SharedState>, dur: Duration) {
    let step = Duration::from_millis(50);
    let mut remaining = dur;
    while remaining > Duration::ZERO {
        if shared.stopping.load(Ordering::SeqCst) {
            return;
        }
        let s = step.min(remaining);
        thread::sleep(s);
        remaining = remaining.saturating_sub(s);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::{
        MockBackend, PanicMode, PanickingMockBackend, StallThenPanicOnReopenBackend,
        StallableMockBackend,
    };
    use flexaudio_core::types::SourceKind;
    use std::time::Instant;

    /// Helper to collect chunks by polling until the deadline.
    fn collect_for(stream: &mut Stream, dur: Duration) -> Vec<AudioChunk> {
        let mut chunks = Vec::new();
        let start = Instant::now();
        while start.elapsed() < dur {
            while let Some(c) = stream.poll_chunk() {
                chunks.push(c);
            }
            thread::sleep(Duration::from_millis(5));
        }
        chunks
    }

    /// Wait until `cond` is true (up to `timeout`); return true if it becomes true.
    fn wait_until<F: FnMut() -> bool>(mut cond: F, timeout: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if cond() {
                return true;
            }
            thread::sleep(Duration::from_millis(10));
        }
        cond()
    }

    /// Extract Err from a `Stream::open` result (`Stream` does not implement `Debug`, so
    /// `expect_err` cannot be used). Panic with a message if the result is Ok.
    fn open_err(result: Result<Stream>, ctx: &str) -> Error {
        match result {
            Ok(_) => panic!("{ctx}: expected an error, got Ok"),
            Err(e) => e,
        }
    }

    // --- Input validation (Stream::open error paths) ---

    #[test]
    fn open_validates_exclusion_pids_for_system_capture() {
        for kind in [SourceKind::SystemLoopback, SourceKind::Mix] {
            let config = StreamConfig {
                kind,
                exclude_pids: vec![0],
                ..Default::default()
            };
            let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
            assert!(matches!(
                Stream::open(config, backend),
                Err(Error::InvalidArg(message))
                    if message == "exclude_pids: pid 0 is not a valid process id"
            ));

            for exclude_pids in [vec![], vec![42]] {
                let config = StreamConfig {
                    kind,
                    exclude_pids,
                    ..Default::default()
                };
                let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
                assert!(Stream::open(config, backend).is_ok());
            }
        }
        for kind in [SourceKind::Mic, SourceKind::ProcessLoopback] {
            let config = StreamConfig {
                kind,
                exclude_pids: vec![0],
                ..Default::default()
            };
            let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
            assert!(Stream::open(config, backend).is_ok());
        }
    }

    #[test]
    fn switch_source_rejects_zero_exclusion_pid_before_replacing_backend() {
        let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
        let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");
        stream.start().expect("start mock capture");

        for kind in [SourceKind::SystemLoopback, SourceKind::Mix] {
            let new_config = StreamConfig {
                kind,
                exclude_pids: vec![42, 0],
                ..Default::default()
            };
            assert!(matches!(
                stream.switch_source(new_config),
                Err(Error::InvalidArg(message))
                    if message == "exclude_pids: pid 0 is not a valid process id"
            ));
            assert_eq!(stream.config.kind, SourceKind::Mic);
            assert!(stream.config.exclude_pids.is_empty());
        }
        stream.stop();
    }

    /// `ring_capacity_chunks == 0` is rejected with InvalidArg (a ring capacity of 0 is invalid).
    #[test]
    fn open_rejects_zero_ring_capacity() {
        let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
        let config = StreamConfig {
            ring_capacity_chunks: 0,
            ..Default::default()
        };
        let err = open_err(Stream::open(config, backend), "capacity 0");
        assert!(
            matches!(err, Error::InvalidArg(_)),
            "expected InvalidArg: {err:?}"
        );
    }

    /// An unsupported output format (channels=3) fails validation with UnsupportedFormat.
    #[test]
    fn open_rejects_invalid_output_channels() {
        let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
        let config = StreamConfig {
            output: OutputFormat {
                sample_rate: 48_000,
                channels: 3,
            },
            ..Default::default()
        };
        let err = open_err(Stream::open(config, backend), "ch=3");
        assert!(
            matches!(err, Error::UnsupportedFormat(_)),
            "expected UnsupportedFormat: {err:?}"
        );
    }

    /// An extreme, out-of-range output rate is also rejected with UnsupportedFormat.
    #[test]
    fn open_rejects_out_of_range_output_rate() {
        let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
        let config = StreamConfig {
            output: OutputFormat {
                sample_rate: 1_000_000,
                channels: 2,
            },
            ..Default::default()
        };
        let err = open_err(Stream::open(config, backend), "extreme rate");
        assert!(
            matches!(err, Error::UnsupportedFormat(_)),
            "expected UnsupportedFormat: {err:?}"
        );
    }

    /// If the backend's native_format is 0 (rate=0 / ch=0), it is rejected with InvalidArg.
    #[test]
    fn open_rejects_zero_native_format() {
        // MockBackend::new applies max(1) internally, so it cannot produce 0. Define a test-only
        // backend with a zero native_format to verify this.
        struct ZeroFormatBackend;
        impl CaptureBackend for ZeroFormatBackend {
            fn native_format(&self) -> (u32, u16) {
                (0, 0)
            }
            fn start(&mut self, _sink: RawSink) -> Result<()> {
                Ok(())
            }
            fn stop(&mut self) {}
        }
        let backend = Box::new(ZeroFormatBackend);
        let err = open_err(
            Stream::open(StreamConfig::default(), backend),
            "native_format 0",
        );
        assert!(
            matches!(err, Error::InvalidArg(_)),
            "expected InvalidArg: {err:?}"
        );
    }

    // --- poll_event (pull-style event retrieval) ---

    /// Events can be retrieved through `poll_event`. Set ChunkRing capacity very low to force
    /// DROP_OLDEST and verify that `Event::ChunkDropped` is observable through poll_event.
    #[test]
    fn poll_event_yields_chunk_dropped() {
        // Capacity 1 plus almost no polling quickly triggers DROP_OLDEST.
        let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
        let config = StreamConfig {
            ring_capacity_chunks: 1,
            ..Default::default()
        };
        let mut stream = Stream::open(config, backend).expect("open");
        stream.start().expect("start");

        // Let the chunk ring overflow by waiting without calling poll_chunk.
        let got_drop = wait_until(
            || {
                // Call only poll_event (not poll_chunk, so the ring fills up).
                while let Some(ev) = stream.poll_event() {
                    if matches!(ev, Event::ChunkDropped { .. }) {
                        return true;
                    }
                }
                false
            },
            Duration::from_secs(3),
        );
        stream.stop();
        assert!(
            got_drop,
            "expected to retrieve ChunkDropped through poll_event"
        );
    }

    /// `poll_event` returns None when there are no events (non-blocking, empty queue).
    #[test]
    fn poll_event_is_none_when_empty() {
        let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
        let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");
        // The event queue is empty before start.
        assert!(stream.poll_event().is_none());
    }

    // --- Watchdog: detect stall → auto-recover → RECOVERED ---

    /// End-to-end verification with StallableMockBackend: the first session stops receiving data,
    /// the watchdog detects a stall after STALL_THRESHOLD, reopens the backend, and marks the first
    /// post-recovery chunk RECOVERED|DISCONTINUITY.
    ///
    /// Check that:
    /// 1. `Event::StreamStalled` fires when the stall is detected.
    /// 2. `Event::StreamRecovered` fires after a successful reopen.
    /// 3. The first post-recovery chunk has ChunkFlags::RECOVERED (and DISCONTINUITY).
    /// 4. seq increases monotonically throughout and is not reset on recovery.
    #[test]
    fn watchdog_detects_stall_and_flags_recovered() {
        // Feed data for 300 ms, then stall the first session.
        let backend = Box::new(StallableMockBackend::new(
            48_000,
            2,
            440.0,
            Duration::from_millis(300),
        ));
        let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");
        stream.enable_capture_tap().expect("canonical tap");
        stream.start().expect("start");

        let mut chunks: Vec<AudioChunk> = Vec::new();
        let mut saw_stalled = false;
        let mut saw_recovered = false;

        // Wait long enough for stall detection (>=2s), reopen, and a recovered chunk (up to 8 s).
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut recovered_chunk_seen = false;
        while Instant::now() < deadline && !recovered_chunk_seen {
            while let Some(c) = stream.poll_chunk() {
                if c.flags.contains(ChunkFlags::RECOVERED) {
                    recovered_chunk_seen = true;
                }
                chunks.push(c);
            }
            while let Some(ev) = stream.poll_event() {
                match ev {
                    Event::StreamStalled => saw_stalled = true,
                    Event::StreamRecovered => saw_recovered = true,
                    _ => {}
                }
            }
            thread::sleep(Duration::from_millis(20));
        }
        stream.stop();
        // Drain any remaining chunks after stop.
        while let Some(c) = stream.poll_chunk() {
            if c.flags.contains(ChunkFlags::RECOVERED) {
                recovered_chunk_seen = true;
            }
            chunks.push(c);
        }

        let capture: Vec<_> = std::iter::from_fn(|| stream.poll_capture()).collect();
        assert!(!capture.is_empty());
        assert!(capture
            .iter()
            .any(|chunk| chunk.flags.contains(ChunkFlags::DISCONTINUITY)));
        for pair in capture.windows(2) {
            assert_eq!(
                pair[1].frame_index - pair[0].frame_index,
                (pair[1].seq - pair[0].seq) * 960
            );
        }
        for pair in chunks.windows(2) {
            assert_eq!(
                pair[1].frame_index - pair[0].frame_index,
                (pair[1].seq - pair[0].seq) * 960,
                "recovery must preserve the producer timeline, including queue drops"
            );
        }
        assert!(saw_stalled, "expected Event::StreamStalled to fire");
        assert!(saw_recovered, "expected Event::StreamRecovered to fire");
        assert!(
            recovered_chunk_seen,
            "expected RECOVERED on the first post-recovery chunk"
        );

        // The chunk marked RECOVERED also has DISCONTINUITY, as designed.
        let recovered: Vec<&AudioChunk> = chunks
            .iter()
            .filter(|c| c.flags.contains(ChunkFlags::RECOVERED))
            .collect();
        assert!(!recovered.is_empty());
        for c in &recovered {
            assert!(
                c.flags.contains(ChunkFlags::DISCONTINUITY),
                "expected DISCONTINUITY with RECOVERED: flags={:?}",
                c.flags
            );
        }

        // seq increases monotonically throughout and is not reset on recovery.
        for w in chunks.windows(2) {
            assert!(
                w[1].seq > w[0].seq,
                "seq should increase monotonically across recovery: {} -> {}",
                w[0].seq,
                w[1].seq
            );
        }
    }

    /// With steady input (no stall), RECOVERED is never set and StreamStalled never arrives (regression
    /// check: the watchdog must not report a false positive). Check briefly with the regular
    /// MockBackend instead of configuring StallableMockBackend with a small, non-stalling interval.

    #[test]
    fn no_recovered_flag_under_steady_feed() {
        let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
        let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");
        stream.start().expect("start");

        // Check flags and events for a short period, less than STALL_THRESHOLD.
        let chunks = collect_for(&mut stream, Duration::from_millis(500));
        let mut saw_stalled = false;
        while let Some(ev) = stream.poll_event() {
            if matches!(ev, Event::StreamStalled) {
                saw_stalled = true;
            }
        }
        stream.stop();

        assert!(!chunks.is_empty(), "expected chunks with steady input");
        assert!(
            !saw_stalled,
            "steady input should not be reported as stalled"
        );
        for c in &chunks {
            assert!(
                !c.flags.contains(ChunkFlags::RECOVERED),
                "RECOVERED should not be set with steady input: flags={:?}",
                c.flags
            );
        }
    }

    // --- pause / resume (pause delivery only) ---

    /// No new chunks arrive while paused. Verify that at least one arrives before pausing and none
    /// arrive during a fixed window afterward.
    #[test]
    fn pause_stops_delivering_chunks() {
        let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
        let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");
        stream.start().expect("start");

        // Wait for at least one chunk before pausing.
        let got_before = wait_until(|| stream.poll_chunk().is_some(), Duration::from_secs(2));
        assert!(got_before, "expected a chunk before pause");

        // Pause and drain anything left in the ring immediately afterward.
        stream.pause();
        while stream.poll_chunk().is_some() {}

        // No new chunks should arrive during the post-pause window.
        let after = collect_for(&mut stream, Duration::from_millis(300));
        stream.stop();
        assert!(
            after.is_empty(),
            "expected no new chunks while paused; received {}",
            after.len()
        );
    }

    /// A pause longer than STALL_THRESHOLD must not trigger a stall. OS-side capture and
    /// last_sample_ns updates continue while delivery is paused, so the watchdog should not detect
    /// idle. Make the pause window comfortably longer than STALL_THRESHOLD plus a watchdog tick.
    #[test]
    fn long_pause_does_not_trigger_stall() {
        let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
        let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");
        stream.start().expect("start");

        // Wait for at least one chunk before pausing.
        let got_before = wait_until(|| stream.poll_chunk().is_some(), Duration::from_secs(2));
        assert!(got_before, "expected a chunk before pause");

        // Pause and drain anything left in the ring immediately afterward.
        stream.pause();
        while stream.poll_chunk().is_some() {}

        // Stay paused beyond STALL_THRESHOLD (2s) and collect events during that time.
        let mut saw_stalled = false;
        let mut saw_recovered = false;
        let deadline = Instant::now() + Duration::from_millis(2800);
        while Instant::now() < deadline {
            while let Some(ev) = stream.poll_event() {
                match ev {
                    Event::StreamStalled => saw_stalled = true,
                    Event::StreamRecovered => saw_recovered = true,
                    _ => {}
                }
            }
            // Remain paused throughout.
            assert!(
                stream.is_paused(),
                "is_paused should remain true during the pause window"
            );
            thread::sleep(Duration::from_millis(20));
        }

        // The key check: no stall detection or recovery during a long pause.
        assert!(
            !saw_stalled,
            "StreamStalled should not fire during a long pause"
        );
        assert!(
            !saw_recovered,
            "StreamRecovered should not fire because there was no stall"
        );

        // Chunk delivery resumes after resume.
        stream.resume().expect("resume");
        let resumed = wait_until(|| stream.poll_chunk().is_some(), Duration::from_secs(2));
        stream.stop();
        assert!(resumed, "chunk delivery should resume after resume");
    }

    /// The first chunk after resume has DISCONTINUITY, seq remains continuous across pause (if the
    /// last before pause was N, the first after resume is N+1), and dropped_before is 0.
    #[test]
    fn resume_flags_discontinuity_and_keeps_seq_continuous() {
        let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
        let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");
        stream.start().expect("start");

        // Collect chunks before pausing and save the last seq.
        let before = collect_for(&mut stream, Duration::from_millis(200));
        assert!(!before.is_empty(), "expected chunks before pause");
        let last_seq = before.last().unwrap().seq;

        // Pause and drain the remaining ring contents. Update the last seq.
        stream.pause();
        let mut last_seq = last_seq;
        while let Some(c) = stream.poll_chunk() {
            last_seq = c.seq;
        }

        // Briefly confirm there are no new chunks while paused, then resume.
        assert!(collect_for(&mut stream, Duration::from_millis(150)).is_empty());
        stream.resume().expect("resume");

        // Wait for the first chunk after resume.
        let mut first_after: Option<AudioChunk> = None;
        let got = wait_until(
            || match stream.poll_chunk() {
                Some(c) => {
                    first_after = Some(c);
                    true
                }
                None => false,
            },
            Duration::from_secs(2),
        );
        stream.stop();
        assert!(got, "expected a chunk after resume");

        let first = first_after.unwrap();
        assert!(
            first.flags.contains(ChunkFlags::DISCONTINUITY),
            "expected DISCONTINUITY on the first chunk after resume: flags={:?}",
            first.flags
        );
        assert_eq!(
            first.seq,
            last_seq + 1,
            "seq should remain continuous across pause ({last_seq} -> {})",
            first.seq
        );
        assert_eq!(
            first.dropped_before, 0,
            "pause should not cause dropped chunks"
        );
    }

    /// After pausing and emptying both rings, repeatedly resume and verify that the first chunk after
    /// resume always has DISCONTINUITY in each independent stream.
    ///
    /// This stress test targets the race between resume and intake. Primary and secondary are streams
    /// with separate rings and seq values, so both must be checked.
    #[test]
    fn resume_stress_marks_first_chunk_of_each_tap_discontinuous() {
        const ROUNDS: usize = 300;
        let config = StreamConfig {
            secondary_output: Some(OutputFormat {
                sample_rate: 16_000,
                channels: 1,
            }),
            ring_capacity_chunks: 200,
            ..Default::default()
        };
        let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
        let mut stream = Stream::open(config, backend).expect("open");
        stream.start().expect("start");
        let mut failures = 0usize;

        for round in 0..ROUNDS {
            stream.pause();
            // pause() is exclusive with delivery, so no old chunk can arrive after draining here.
            // Always empty the secondary ring at the same time.
            while stream.poll_chunk().is_some() {}
            while stream.poll_secondary().is_some() {}

            stream.resume().expect("resume");

            let mut primary = None;
            let got_primary = wait_until(
                || match stream.poll_chunk() {
                    Some(chunk) => {
                        primary = Some(chunk);
                        true
                    }
                    None => false,
                },
                Duration::from_secs(2),
            );
            let mut secondary = None;
            let got_secondary = wait_until(
                || match stream.poll_secondary() {
                    Some(chunk) => {
                        secondary = Some(chunk);
                        true
                    }
                    None => false,
                },
                Duration::from_secs(2),
            );

            let primary_ok = got_primary
                && primary.is_some_and(|chunk| chunk.flags.contains(ChunkFlags::DISCONTINUITY));
            let secondary_ok = got_secondary
                && secondary.is_some_and(|chunk| chunk.flags.contains(ChunkFlags::DISCONTINUITY));
            if !primary_ok || !secondary_ok {
                failures += 1;
                eprintln!("round {round}: primary_ok={primary_ok}, secondary_ok={secondary_ok}");
            }
        }

        stream.stop();
        assert_eq!(
            failures, 0,
            "{failures} / {ROUNDS} resume attempts had no DISCONTINUITY on the first primary or \
             secondary chunk"
        );
    }

    /// Resume while raw intake is blocked, then check the secondary tap independently.
    /// The raw-consumer lock excludes new raw reads during resume; it does not prove that
    /// the worker has already read the pending flags at the top of its current iteration.
    #[test]
    fn resume_flags_secondary_first_chunk_with_raw_intake_blocked() {
        let config = StreamConfig {
            secondary_output: Some(OutputFormat {
                sample_rate: 16_000,
                channels: 1,
            }),
            ring_capacity_chunks: 200,
            ..Default::default()
        };
        let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
        let mut stream = Stream::open(config, backend).expect("open");
        stream.start().expect("start");

        let mut last_secondary_seq = None;
        let got_before = wait_until(
            || {
                while stream.poll_chunk().is_some() {}
                if let Some(chunk) = stream.poll_secondary() {
                    last_secondary_seq = Some(chunk.seq);
                    true
                } else {
                    false
                }
            },
            Duration::from_secs(2),
        );
        assert!(got_before, "expected a secondary chunk before pause");

        stream.pause();
        // Clone shared state so holding its lock does not borrow the stream while polling.
        let shared = stream.shared.clone();
        {
            let raw = shared
                .raw_consumer
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            while stream.poll_chunk().is_some() {}
            while let Some(chunk) = stream.poll_secondary() {
                last_secondary_seq = Some(chunk.seq);
            }
            // Keep the lock only across resume, without a sleep that could overflow RawRing.
            stream.resume().expect("resume");
            drop(raw);
        }

        let mut first_after = None;
        let got_after = wait_until(
            || {
                while stream.poll_chunk().is_some() {}
                if let Some(chunk) = stream.poll_secondary() {
                    first_after = Some(chunk);
                    true
                } else {
                    false
                }
            },
            Duration::from_secs(2),
        );
        let overflow_count = shared
            .raw_consumer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .expect("raw consumer")
            .overflow_count();
        stream.stop();

        assert!(got_after, "expected a secondary chunk after resume");
        // Otherwise overflow could supply DISCONTINUITY and hide missing resume wiring.
        assert_eq!(overflow_count, 0, "raw overflow must not mask resume flags");
        let first = first_after.expect("first secondary chunk after resume");
        assert!(
            first.flags.contains(ChunkFlags::DISCONTINUITY),
            "expected DISCONTINUITY on the first secondary chunk after resume: {:?}",
            first.flags
        );
        assert!(
            !first.flags.contains(ChunkFlags::RECOVERED),
            "watchdog recovery must not mask resume flags"
        );
        assert_eq!(
            first.seq,
            last_secondary_seq.expect("last secondary sequence before pause") + 1,
            "secondary sequence should remain continuous across pause"
        );
        assert_eq!(
            first.dropped_before, 0,
            "pause must not drop secondary chunks"
        );
    }

    /// Calling resume while not paused does not set DISCONTINUITY on the next chunk (no-op).
    #[test]
    fn resume_without_pause_is_noop() {
        let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
        let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");
        stream.start().expect("start");

        // Discard the initial chunks so startup RECOVERED/DISCONTINUITY flags have passed.
        let _ = collect_for(&mut stream, Duration::from_millis(200));

        // Resume while not paused.
        stream.resume().expect("resume");

        // DISCONTINUITY should not be set on subsequent chunks.
        let after = collect_for(&mut stream, Duration::from_millis(200));
        stream.stop();
        assert!(!after.is_empty(), "expected chunks to arrive");
        for c in &after {
            assert!(
                !c.flags.contains(ChunkFlags::DISCONTINUITY),
                "resume without pause should not set DISCONTINUITY: flags={:?}",
                c.flags
            );
        }
    }

    /// Calling pause twice is safe; one resume call should resume normally.
    #[test]
    fn double_pause_then_single_resume_recovers() {
        let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
        let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");
        stream.start().expect("start");

        let before = collect_for(&mut stream, Duration::from_millis(200));
        assert!(!before.is_empty(), "expected chunks before pause");

        // Call pause twice.
        stream.pause();
        stream.pause();
        assert!(stream.is_paused());
        while stream.poll_chunk().is_some() {}
        assert!(collect_for(&mut stream, Duration::from_millis(150)).is_empty());

        // Call resume once.
        stream.resume().expect("resume");
        assert!(!stream.is_paused());
        let got = wait_until(|| stream.poll_chunk().is_some(), Duration::from_secs(2));
        stream.stop();
        assert!(got, "delivery should resume after one resume call");
    }

    // --- Input gain (config.gain / set_gain) ---

    /// Verify that the gain from config affects completed chunk data and peak/rms meters. The
    /// MockBackend sine wave has amplitude 0.5, so gain 2.0 gives a chunk peak of about 1.0 and gain
    /// 0.5 gives about 0.25. Also verify that peak is computed from data after gain (the meter shows
    /// the actual post-gain level).
    #[test]
    fn gain_scales_samples_and_meters() {
        // (gain, expected peak range). Sine amplitude 0.5 × gain.
        for (gain, lo, hi) in [(2.0f32, 0.95f32, 1.0f32), (0.5, 0.2, 0.3)] {
            let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
            let config = StreamConfig {
                gain,
                ..Default::default()
            };
            let mut stream = Stream::open(config, backend).expect("open");
            stream.start().expect("start");
            let chunks = collect_for(&mut stream, Duration::from_millis(300));
            stream.stop();
            assert!(!chunks.is_empty(), "expected chunks with gain={gain}");

            // peak matches the data after gain is applied (the meter reports the actual post-gain level).
            let mut max_peak = 0.0f32;
            for c in &chunks {
                let recomputed = c.data.iter().fold(0.0f32, |m, &x| m.max(x.abs()));
                assert_eq!(
                    c.peak, recomputed,
                    "peak should be computed from data after gain is applied"
                );
                max_peak = max_peak.max(c.peak);
            }
            assert!(
                (lo..=hi).contains(&max_peak),
                "expected peak for gain={gain} in {lo}..={hi}: {max_peak}"
            );
        }
    }

    /// set_gain takes effect on the next chunk during recording. Start at 1.0, receive a chunk, then
    /// call set_gain(0.0) and verify that subsequent chunks have all-zero samples and peak 0.
    #[test]
    fn set_gain_takes_effect_mid_stream() {
        let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
        let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");
        stream.start().expect("start");
        assert_eq!(stream.gain(), 1.0, "default gain is 1.0");

        // First wait for a normal chunk.
        let got_before = wait_until(|| stream.poll_chunk().is_some(), Duration::from_secs(2));
        assert!(got_before, "expected a chunk before set_gain");

        // Set gain to 0.0 (silence); it takes effect from the next completed chunk (20 ms granularity).
        stream.set_gain(0.0).expect("set_gain(0.0)");
        assert_eq!(stream.gain(), 0.0);

        // Chunks completed before the setting may still arrive, so wait for a silent chunk.
        let got_silent = wait_until(
            || matches!(stream.poll_chunk(), Some(c) if c.peak == 0.0),
            Duration::from_secs(2),
        );
        assert!(got_silent, "expected a silent chunk after set_gain(0.0)");

        // Subsequent chunks should retain all-zero samples, peak 0, and rms 0.
        let after = collect_for(&mut stream, Duration::from_millis(300));
        stream.stop();
        assert!(
            !after.is_empty(),
            "chunks should continue to flow during silence"
        );
        for c in &after {
            assert!(
                c.data.iter().all(|&x| x == 0.0),
                "all samples should be 0 at gain 0.0"
            );
            assert_eq!(c.peak, 0.0);
            assert_eq!(c.rms, 0.0);
        }
    }

    /// Samples are clamped to ±1.0 even at high gain. With sine amplitude 0.5 × gain 100, samples
    /// would reach 50 without clamping, but all stay within ±1.0 and the peak is exactly 1.0.
    #[test]
    fn gain_clamps_to_unit_range() {
        let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
        let config = StreamConfig {
            gain: 100.0,
            ..Default::default()
        };
        let mut stream = Stream::open(config, backend).expect("open");
        stream.start().expect("start");
        let chunks = collect_for(&mut stream, Duration::from_millis(300));
        stream.stop();
        assert!(!chunks.is_empty(), "expected chunks to arrive");

        let mut max_peak = 0.0f32;
        for c in &chunks {
            assert!(
                c.data.iter().all(|&x| (-1.0..=1.0).contains(&x)),
                "samples should not exceed ±1.0"
            );
            max_peak = max_peak.max(c.peak);
        }
        assert_eq!(max_peak, 1.0, "clamping should make the peak exactly 1.0");
    }

    /// Invalid gains (negative and NaN) are rejected with InvalidArg by both open and set_gain.
    #[test]
    fn invalid_gain_rejected() {
        // open: config.gain is negative.
        let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
        let config = StreamConfig {
            gain: -1.0,
            ..Default::default()
        };
        let err = open_err(Stream::open(config, backend), "gain=-1.0");
        assert!(
            matches!(err, Error::InvalidArg(_)),
            "expected InvalidArg: {err:?}"
        );

        // open: config.gain is NaN.
        let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
        let config = StreamConfig {
            gain: f32::NAN,
            ..Default::default()
        };
        let err = open_err(Stream::open(config, backend), "gain=NaN");
        assert!(
            matches!(err, Error::InvalidArg(_)),
            "expected InvalidArg: {err:?}"
        );

        // set_gain: negative and NaN values return InvalidArg and leave the current value unchanged.
        let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
        let stream = Stream::open(StreamConfig::default(), backend).expect("open");
        assert!(matches!(stream.set_gain(-1.0), Err(Error::InvalidArg(_))));
        assert!(matches!(
            stream.set_gain(f32::NAN),
            Err(Error::InvalidArg(_))
        ));
        assert_eq!(
            stream.gain(),
            1.0,
            "failed set_gain should not change the current value"
        );
    }

    // --- Robustness: backend panics do not cause silent death (prevent poison-related panic cascades) ---
    //
    // These tests prove there is no silent death or panic cascade by verifying that the test process
    // itself does not panic (a panic would make the test result FAILED). They also assert that the
    // panic is observable as Err / Event::Error, so it is not merely swallowed and hidden.

    /// If backend `start()` panics, the process stays alive and `start()` returns
    /// `Err(Error::Backend)`. catch_unwind converts the panic before mutex poisoning, so the intake
    /// and watchdog threads never start and no panic cascade occurs.
    #[test]
    fn backend_panic_in_start_returns_err_not_silent_death() {
        let backend = Box::new(PanickingMockBackend::new(
            48_000,
            2,
            440.0,
            PanicMode::Start,
        ));
        let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");

        // start() must return Err(Error::Backend) without propagating the panic.
        let result = stream.start();
        match result {
            Ok(()) => panic!("backend panicked in start, but start() returned Ok"),
            Err(Error::Backend(msg)) => {
                assert!(
                    msg.contains("panicked"),
                    "expected Error::Backend with a message identifying the panic: {msg}"
                );
            }
            Err(other) => panic!("expected Error::Backend, got a different error: {other:?}"),
        }

        // After start fails, the stream is not started. stop must not panic, even though no threads started.
        stream.stop();
    }

    /// If backend `stop()` panics, the process stays alive and `stop()` returns normally. catch_unwind
    /// swallows the panic without poisoning the backend mutex, preventing cascaded panics in the
    /// intake and watchdog threads that were running.
    #[test]
    fn backend_panic_in_stop_does_not_kill_process() {
        let backend = Box::new(PanickingMockBackend::new(48_000, 2, 440.0, PanicMode::Stop));
        let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");
        stream.start().expect("start");

        // Let the stream run briefly so the intake and watchdog threads are active
        // (confirm chunks flow, so the happy path is unchanged).
        let chunks = collect_for(&mut stream, Duration::from_millis(300));
        assert!(
            !chunks.is_empty(),
            "chunks should flow normally before stop (happy path unchanged)"
        );

        // backend.stop() panics inside stop(), but catch_unwind swallows it without poisoning the
        // mutex. The fact that this test does not panic is itself proof.
        stream.stop();

        // Poll still works after stop without a panic cascade (additional check that the mutex was not poisoned).
        let _ = stream.poll_chunk();
        let _ = stream.poll_event();
    }

    /// If the backend panics during watchdog reopen, the watchdog thread does not die silently in a
    /// panic cascade; the failure is surfaced as `Event::Error` ("reopen failed: ..."). The process
    /// stays alive because catch_unwind prevents mutex poisoning and the reopen failure becomes
    /// Event::Error through `open_backend_once`'s Err.
    #[test]
    fn backend_panic_on_watchdog_reopen_surfaces_event_error() {
        // Feed for 300 ms → stall → panic during watchdog reopen.
        let backend = Box::new(StallThenPanicOnReopenBackend::new(
            48_000,
            2,
            440.0,
            Duration::from_millis(300),
        ));
        let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");
        stream.start().expect("start");

        // Wait long enough for stall detection (>=2s) and a reopen attempt (panic → Event::Error), up to 8 s.
        let mut saw_stalled = false;
        let mut saw_reopen_error = false;
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline && !saw_reopen_error {
            // Also call poll_chunk so a full ring does not block other paths.
            while stream.poll_chunk().is_some() {}
            while let Some(ev) = stream.poll_event() {
                match ev {
                    Event::StreamStalled => saw_stalled = true,
                    Event::Error(msg) if msg.contains("reopen failed") => {
                        saw_reopen_error = true;
                    }
                    _ => {}
                }
            }
            thread::sleep(Duration::from_millis(20));
        }
        stream.stop();

        assert!(
            saw_stalled,
            "expected stall detection (Event::StreamStalled)"
        );
        assert!(
            saw_reopen_error,
            "backend panic during reopen should surface as Event::Error(\"reopen failed: ...\") \
             (no silent death)"
        );
    }

    // --- Absolute clock (recording starts at zero) ---

    /// The first delivered chunk's pts_ns is the recording epoch itself, so it starts at exactly 0.
    #[test]
    fn recording_clock_is_zero_based() {
        let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
        let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");
        stream.start().expect("start");

        let mut first: Option<AudioChunk> = None;
        let got = wait_until(
            || match stream.poll_chunk() {
                Some(c) => {
                    first = Some(c);
                    true
                }
                None => false,
            },
            Duration::from_secs(2),
        );
        stream.stop();
        assert!(got, "expected the first chunk to arrive");
        let first = first.unwrap();
        assert_eq!(
            first.pts_ns, 0,
            "the first delivered chunk should start at recording time zero (pts_ns == 0): {}",
            first.pts_ns
        );
    }

    /// Across pause, PTS advances by the pause duration (capture wall-clock time). Also verify that
    /// the first chunk after resume has DISCONTINUITY, continuous seq, and dropped_before 0.
    #[test]
    fn pause_preserves_absolute_clock() {
        let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
        let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");
        stream.start().expect("start");

        // Collect chunks before pause and save the last (pts_ns, seq).
        let before = collect_for(&mut stream, Duration::from_millis(250));
        assert!(!before.is_empty(), "expected chunks before pause");
        let mut last = before.last().cloned().unwrap();

        stream.pause();
        while let Some(c) = stream.poll_chunk() {
            last = c;
        }

        // Pause for a known duration D (less than STALL_THRESHOLD).
        let d = Duration::from_millis(600);
        thread::sleep(d);
        stream.resume().expect("resume");

        let mut first_after: Option<AudioChunk> = None;
        let got = wait_until(
            || match stream.poll_chunk() {
                Some(c) => {
                    first_after = Some(c);
                    true
                }
                None => false,
            },
            Duration::from_secs(2),
        );
        stream.stop();
        assert!(got, "expected a chunk after resume");
        let first = first_after.unwrap();

        assert!(
            first.flags.contains(ChunkFlags::DISCONTINUITY),
            "expected DISCONTINUITY on the first chunk after resume: {:?}",
            first.flags
        );
        assert_eq!(
            first.seq,
            last.seq + 1,
            "seq should remain continuous across pause"
        );
        assert_eq!(first.dropped_before, 0, "pause should not drop chunks");

        // PTS advances by pause duration D (capture wall-clock time). Bound it below by D*0.8 and
        // above by D plus a margin to allow for CI timing variation.
        let delta = first.pts_ns - last.pts_ns;
        let d_ns = d.as_nanos() as i64;
        assert!(
            delta >= d_ns * 4 / 5,
            "pts should advance by at least the pause duration (>= {} ns): delta={delta} ns",
            d_ns * 4 / 5
        );
        assert!(
            delta <= d_ns + 500_000_000,
            "pts should not advance too far (<= D + 500ms): delta={delta} ns"
        );
    }

    // --- Dual output (primary + secondary taps) ---

    /// With secondary_output set, primary (48k/stereo) and secondary (16k/mono) chunks are delivered
    /// together. A secondary chunk has 320 samples, and its PTS uses the same zero-based clock as the primary.
    #[test]
    fn dual_output_delivers_primary_and_secondary() {
        let config = StreamConfig {
            secondary_output: Some(OutputFormat {
                sample_rate: 16_000,
                channels: 1,
            }),
            // Use a larger capacity so DROP_OLDEST does not discard the first chunk (pts 0) during collection.
            ring_capacity_chunks: 200,
            ..Default::default()
        };
        let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
        let mut stream = Stream::open(config, backend).expect("open");
        stream.start().expect("start");

        let mut primary: Vec<AudioChunk> = Vec::new();
        let mut secondary: Vec<SecondaryChunk> = Vec::new();
        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline {
            while let Some(c) = stream.poll_chunk() {
                primary.push(c);
            }
            while let Some(c) = stream.poll_secondary() {
                secondary.push(c);
            }
            thread::sleep(Duration::from_millis(5));
        }
        stream.stop();
        while let Some(c) = stream.poll_chunk() {
            primary.push(c);
        }
        while let Some(c) = stream.poll_secondary() {
            secondary.push(c);
        }

        assert!(!primary.is_empty(), "expected primary chunks");
        assert!(!secondary.is_empty(), "expected secondary chunks");
        for c in &primary {
            assert_eq!(
                c.data.len(),
                960 * 2,
                "primary is 48k/stereo = 1920 samples"
            );
        }
        for c in &secondary {
            assert_eq!(c.samples.len(), 320, "secondary is 16k/mono = 320 samples");
        }
        // Both taps start at zero and are non-decreasing. The first primary chunk starts at 0.
        assert_eq!(primary[0].pts_ns, 0, "first primary chunk starts at zero");
        for w in secondary.windows(2) {
            assert!(
                w[1].pts_ns >= w[0].pts_ns,
                "secondary PTS should not decrease"
            );
        }
        assert!(
            secondary[0].pts_ns >= 0,
            "secondary PTS should be non-negative (based on primary epoch)"
        );
        for pair in primary.windows(2) {
            assert_eq!(pair[1].frame_index, pair[0].frame_index + 960);
        }
        for pair in secondary.windows(2) {
            assert_eq!(pair[1].frame_index, pair[0].frame_index + 960);
        }
        assert_eq!(primary[0].frame_index, 0);
        assert_eq!(secondary[0].frame_index, 0);
        // Secondary seq uses its own counter and increases from 0.
        assert_eq!(secondary[0].seq, 0);
        for w in secondary.windows(2) {
            assert_eq!(
                w[1].seq,
                w[0].seq + 1,
                "secondary seq should increase consecutively"
            );
        }
    }

    /// Starting after a pre-start pause clears that state for both taps.
    #[test]
    fn start_after_pause_delivers_the_secondary_tap() {
        let config = StreamConfig {
            secondary_output: Some(OutputFormat {
                sample_rate: 16_000,
                channels: 1,
            }),
            ring_capacity_chunks: 200,
            ..Default::default()
        };
        let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
        let mut stream = Stream::open(config, backend).expect("open");
        stream.pause();
        stream.start().expect("start");
        assert!(
            !stream.is_paused(),
            "start should clear the pre-start pause"
        );

        let mut primary = false;
        let mut secondary = false;
        let got_both = wait_until(
            || {
                while stream.poll_chunk().is_some() {
                    primary = true;
                }
                while stream.poll_secondary().is_some() {
                    secondary = true;
                }
                primary && secondary
            },
            Duration::from_secs(2),
        );
        stream.stop();
        assert!(primary, "expected primary delivery after a pre-start pause");
        assert!(
            got_both && secondary,
            "expected secondary delivery after a pre-start pause"
        );
    }

    /// secondary_output cannot be changed with switch_source (it is fixed at open).
    #[test]
    fn secondary_output_cannot_change_on_switch() {
        let config = StreamConfig {
            secondary_output: Some(OutputFormat {
                sample_rate: 16_000,
                channels: 1,
            }),
            ..Default::default()
        };
        let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
        let mut stream = Stream::open(config, backend).expect("open");
        stream.start().expect("start");

        // A switch request that changes the secondary format is rejected with InvalidArg before backend construction.
        let new_config = StreamConfig {
            secondary_output: Some(OutputFormat {
                sample_rate: 8_000,
                channels: 1,
            }),
            ..Default::default()
        };
        let err = stream.switch_source(new_config);
        stream.stop();
        assert!(
            matches!(err, Err(Error::InvalidArg(_))),
            "expected InvalidArg when changing the secondary format: {err:?}"
        );
    }

    // --- RawRing overflow → DISCONTINUITY ---

    /// Test-only mock backend that overflows RawRing. On start, each burst keeps pushing twice the
    /// ring capacity, guaranteeing overflow.
    struct FloodingMockBackend {
        running: Arc<AtomicBool>,
        handle: Option<thread::JoinHandle<()>>,
    }

    impl FloodingMockBackend {
        fn new() -> Self {
            Self {
                running: Arc::new(AtomicBool::new(false)),
                handle: None,
            }
        }
    }

    impl CaptureBackend for FloodingMockBackend {
        fn native_format(&self) -> (u32, u16) {
            (48_000, 2)
        }
        fn start(&mut self, mut sink: RawSink) -> Result<()> {
            self.running.store(true, Ordering::SeqCst);
            let running = self.running.clone();
            let handle = thread::spawn(move || {
                // Push twice the ring capacity per burst (guaranteed overflow).
                let burst = vec![0.1f32; RAW_RING_SAMPLES * 2];
                while running.load(Ordering::SeqCst) {
                    sink.push(&burst, 0);
                    thread::sleep(Duration::from_millis(5));
                }
            });
            self.handle = Some(handle);
            Ok(())
        }
        fn stop(&mut self) {
            self.running.store(false, Ordering::SeqCst);
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }

    /// Detect sustained RawRing overflow and set DISCONTINUITY on at least one chunk. On a fresh
    /// start, overflow is the only possible source of discontinuity, so DISCONTINUITY must be due to
    /// overflow.
    #[test]
    fn ring_overflow_marks_discontinuity() {
        let backend = Box::new(FloodingMockBackend::new());
        // Use a larger ChunkRing so DROP_OLDEST does not discard chunks before they can be observed.
        let config = StreamConfig {
            ring_capacity_chunks: 200,
            ..Default::default()
        };
        let mut stream = Stream::open(config, backend).expect("open");
        stream.start().expect("start");

        let mut saw_discontinuity = false;
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline && !saw_discontinuity {
            while let Some(c) = stream.poll_chunk() {
                if c.flags.contains(ChunkFlags::DISCONTINUITY) {
                    saw_discontinuity = true;
                }
            }
            thread::sleep(Duration::from_millis(10));
        }
        stream.stop();
        while let Some(c) = stream.poll_chunk() {
            if c.flags.contains(ChunkFlags::DISCONTINUITY) {
                saw_discontinuity = true;
            }
        }
        assert!(
            saw_discontinuity,
            "RawRing overflow should set DISCONTINUITY"
        );
    }

    // --- Inject denoise into the internal canonical format (through core InnerProcessor) ---

    /// Failure-detection bound, not a collection window. Debug RNNoise processing can
    /// delay the first delivery beyond 500 ms under load; exit as soon as it arrives.
    const DENOISE_TAP_TIMEOUT: Duration = Duration::from_secs(15);

    /// Verify with a smoke test that both primary and secondary taps continue delivering after
    /// set_denoise(true). The core flush/processing tests cover actual noise reduction.
    #[test]
    fn denoise_enabled_still_delivers_both_taps() {
        fn drain_taps(stream: &mut Stream, primary: &mut usize, secondary: &mut usize) {
            while stream.poll_chunk().is_some() {
                *primary += 1;
            }
            while let Some(chunk) = stream.poll_secondary() {
                assert_eq!(
                    chunk.samples.len(),
                    320,
                    "secondary is 16k/mono = 320 samples"
                );
                *secondary += 1;
            }
        }

        let config = StreamConfig {
            secondary_output: Some(OutputFormat {
                sample_rate: 16_000,
                channels: 1,
            }),
            ..Default::default()
        };
        let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
        let mut stream = Stream::open(config, backend).expect("open");
        stream.set_denoise(true);
        stream.start().expect("start");

        let mut primary = 0usize;
        let mut secondary = 0usize;
        let primary_ok = wait_until(
            || {
                drain_taps(&mut stream, &mut primary, &mut secondary);
                primary > 0
            },
            DENOISE_TAP_TIMEOUT,
        );
        let secondary_ok = wait_until(
            || {
                drain_taps(&mut stream, &mut primary, &mut secondary);
                secondary > 0
            },
            DENOISE_TAP_TIMEOUT,
        );
        let last_sample_ns = stream.shared.last_sample_ns.load(Ordering::SeqCst);
        stream.stop();
        assert!(
            primary_ok,
            "no primary delivery with denoise within {DENOISE_TAP_TIMEOUT:?}: \
             primary={primary}, secondary={secondary}, last_sample_ns={last_sample_ns}"
        );
        assert!(
            secondary_ok,
            "no secondary delivery with denoise within {DENOISE_TAP_TIMEOUT:?}: \
             primary={primary}, secondary={secondary}, last_sample_ns={last_sample_ns}"
        );
    }
}

#[cfg(test)]
mod repro_tests;

#[cfg(test)]
#[path = "stream_frame_tests.rs"]
mod frame_tests;
