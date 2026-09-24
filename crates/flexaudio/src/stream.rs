//! [`Stream`], which owns the capture pipeline for one source.
//!
//! Wires the core components ([`RawRing`](mod@flexaudio_core::raw_ring) / [`Normalizer`] /
//! [`ChunkRing`](mod@flexaudio_core::chunk_ring) / [`ClockNormalizer`] /
//! [`CaptureBackend`]) together and supplies the consumer through a pull-style API
//! ([`poll_chunk`](Stream::poll_chunk) / [`poll_event`](Stream::poll_event)).
//!
//! # Thread layout
//! - The backend's RT thread: only pushes raw frames to
//!   [`RawRing`](mod@flexaudio_core::raw_ring) via [`RawSink`] (non-blocking).
//! - Ingest/processing thread (one, normal priority): pops the RawRing -> converts to
//!   48k/stereo/20ms with the [`Normalizer`] -> assigns a monotonically increasing `seq` ->
//!   pushes to [`ChunkRing`](mod@flexaudio_core::chunk_ring) (DROP_OLDEST).
//!   Updates the time of the last processed sample in an `AtomicI64`.
//! - Watchdog thread (one, ~250ms tick): when sample updates stop for a certain time, judges
//!   the stream silently dead and reopens the backend with exponential backoff (250ms -> 5s,
//!   jitter). Fires [`Event::StreamStalled`] on a stall and [`Event::StreamRecovered`] on
//!   recovery, and sets [`ChunkFlags::RECOVERED`] | [`ChunkFlags::DISCONTINUITY`] on the first
//!   chunk after recovery.

use std::collections::VecDeque;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
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
    AudioChunk, ChunkFlags, Error, Event, OutputFormat, Result, SecondaryChunk, StreamConfig,
};

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
fn build_inner_processor(shared: &SharedState) -> Option<Box<dyn InnerProcessor>> {
    if shared.denoise_enabled.load(Ordering::SeqCst) {
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
) -> Result<Normalizer> {
    let mut n = Normalizer::new(rate, channels, output)?;
    if let Some(sec) = secondary_output {
        n = n.with_secondary(sec)?;
    }
    if let Some(proc) = build_inner_processor(shared) {
        n = n.with_inner_processor(proc);
    }
    Ok(n)
}

/// Capacity of the RawRing (in f32 samples). Does not depend on the native SR x ch; it is
/// allocated generously to avoid drops on the RT path (headroom of about 0.5 seconds at 48k
/// stereo). The mix source's composite backend (`mix.rs`) uses the same capacity for its child
/// rings.
pub(crate) const RAW_RING_SAMPLES: usize = 48_000;

/// Watchdog tick interval.
const WATCHDOG_TICK: Duration = Duration::from_millis(250);

/// Default threshold: if sample arrival stops for this long, the stream is judged "silently
/// dead".
const STALL_THRESHOLD: Duration = Duration::from_secs(2);

/// Lower bound of the reopen exponential backoff.
const BACKOFF_MIN: Duration = Duration::from_millis(250);
/// Upper bound of the reopen exponential backoff.
const BACKOFF_MAX: Duration = Duration::from_secs(5);

/// Capture pipeline for one source.
///
/// Configure it with [`open`](Self::open) and begin capturing with [`start`](Self::start).
/// The consumer calls [`poll_chunk`](Self::poll_chunk) / [`poll_event`](Self::poll_event)
/// without blocking. [`stop`](Self::stop) joins all threads.
pub struct Stream {
    config: StreamConfig,

    /// Shares the backend; the ingest thread / watchdog thread (re)open it.
    shared: Arc<SharedState>,

    /// Consumer of the chunk ring that the consumer side takes from.
    chunk_consumer: ChunkConsumer,

    /// Consumer of the secondary tap (only when `config.secondary_output` is `Some`).
    secondary_consumer: Option<SecondaryChunkConsumer>,

    /// Consumer side of the event queue (shared).
    events: Arc<Mutex<VecDeque<Event>>>,

    /// Ingest/processing thread.
    worker: Option<JoinHandle<()>>,
    /// Watchdog thread.
    watchdog: Option<JoinHandle<()>>,

    /// Whether it has been started (prevents a double start).
    started: bool,
}

/// State shared by the ingest thread, the watchdog thread, and main.
struct SharedState {
    /// The backend itself (protected by a lock for reopening).
    backend: Mutex<Box<dyn CaptureBackend>>,

    /// The currently active RawConsumer. The watchdog swaps it on reopen.
    /// While it is `None` (during a reopen), the ingest thread does not pop.
    raw_consumer: Mutex<Option<RawConsumer>>,

    /// Generation of `raw_consumer`. Incremented on every reopen. The ingest thread detects a
    /// generation change and resets its internal state (Normalizer, etc.).
    raw_generation: AtomicU64,

    /// Monotonic time (ns) at which a sample was last processed (popped and fed to the
    /// Normalizer).
    last_sample_ns: AtomicI64,

    /// Stop signal to all threads.
    stopping: AtomicBool,

    /// Just-recovered flag. The watchdog sets it to true on recovery, and the ingest thread
    /// sets RECOVERED|DISCONTINUITY on the next chunk and resets it to false.
    recovered_pending: AtomicBool,

    /// Event queue (shared by producer/consumer).
    events: Arc<Mutex<VecDeque<Event>>>,

    /// Producer of the ChunkRing (used by the ingest thread).
    chunk_producer: Mutex<Option<ChunkProducer>>,

    /// Native format `(sample_rate, channels)` of the current `backend`.
    ///
    /// It does not change on watchdog recovery (reopening the same backend), but it is
    /// updated to the new backend's value when the source is replaced with
    /// [`Stream::switch_source`] (the native SR/ch normally differs between mic and
    /// system/process). The ingest thread detects the generation change, re-reads this, and
    /// rebuilds the stage-1 (native-dependent) [`Normalizer`].
    native_format: Mutex<(u32, u16)>,

    /// Source-switch-in-progress flag. [`Stream::switch_backend`] sets it to true while
    /// switching. While switching, the watchdog skips stall handling so that it does not
    /// reopen concurrently (it does not mistakenly reopen even if the stream goes briefly idle
    /// because the switch stops the old backend).
    switching: AtomicBool,

    /// Intentional-discontinuity flag. [`Stream::switch_backend`] sets it to true when a
    /// source switch succeeds, and the ingest thread sets DISCONTINUITY on the next chunk (not
    /// RECOVERED = an intentional switch, not an automatic recovery) and resets it to false.
    discontinuity_pending: AtomicBool,

    /// Paused flag. true on pause(). The ingest thread discards completed chunks and does not
    /// deliver them (ingest from the RawRing continues, so the device does not stop and the
    /// watchdog's stall judgment does not waver). Reset to false by resume().
    paused: AtomicBool,

    /// Mutual exclusion between `pause()` and chunk pushes. Closes the race where, after pause
    /// returns, the ingest thread later enqueues a chunk it had already assembled.
    delivery: Mutex<()>,

    /// Generation per actual pause -> resume transition. Each tap observes it under the
    /// delivery lock and sets DISCONTINUITY on the first chunk it enqueues after a change.
    resume_generation: AtomicU64,

    /// f32 bit representation of the input gain (linear factor) (held via
    /// `f32::to_bits`/`from_bits`). Initialized from config.gain in open(); set_gain()
    /// rewrites it at any time during capture. The ingest thread reads it for every completed
    /// chunk and multiplies each sample when it is not 1.0.
    gain_bits: AtomicU32,

    /// Recording epoch (ns). "The computed pts of the first delivered primary chunk" is fixed
    /// exactly once and then subtracted from every chunk (primary and secondary) so recording
    /// starts at 0. Sentinel `i64::MIN` = not fixed yet. Not reset across reopen/switch (one
    /// clock for the whole recording).
    recording_epoch_ns: AtomicI64,

    /// Whether noise suppression on the internal canonical form is enabled. The ingest thread
    /// reads it when (re)building the Normalizer, and when enabled injects denoise into core's
    /// InnerProcessor.
    denoise_enabled: AtomicBool,

    /// Producer of the secondary ChunkRing (Some only when a secondary tap is configured). Used
    /// by the ingest thread.
    secondary_producer: Mutex<Option<SecondaryChunkProducer>>,
}

impl SharedState {
    fn push_event(&self, ev: Event) {
        // Even on poison the events are not torn (recover the VecDeque and continue).
        let mut q = self.events.lock().unwrap_or_else(|e| e.into_inner());
        q.push_back(ev);
    }
}

/// Calls the backend's `start(sink)` wrapped in [`catch_unwind`](std::panic::catch_unwind).
/// Even if the backend panics, the panic is converted to [`Error::Backend`] and returned
/// before it can poison the mutex (the caller can surface it as `Event::Error`/`Err`).
///
/// `&mut Box<dyn CaptureBackend>` is not `UnwindSafe`, so it is wrapped in
/// [`AssertUnwindSafe`]. This is safe because, once a panic is caught, this function only
/// returns `Err` and the possibly logically broken backend is not used any further (the
/// caller treats it as a failure and proceeds to stop/reopen/drop). The lock guard is held
/// and dropped normally and does not poison.
fn start_backend_catching(be: &mut Box<dyn CaptureBackend>, sink: RawSink) -> Result<()> {
    match std::panic::catch_unwind(AssertUnwindSafe(|| be.start(sink))) {
        Ok(res) => res,
        Err(_) => Err(Error::Backend("backend panicked during start()".into())),
    }
}

/// Calls the backend's `stop()` wrapped in [`catch_unwind`](std::panic::catch_unwind).
/// stop returns `()`, so a panic is swallowed and execution continues (propagating a panic
/// again on the stop path gains nothing; the goal is to prevent mutex poisoning and cascading
/// panics). Returning `true` means a normal stop, and `false` means a panic was caught (for
/// observation / diagnostics).
///
/// The `AssertUnwindSafe` safety argument is the same as for [`start_backend_catching`] (after
/// a catch the backend is not used any further, and the guard is dropped normally).
#[must_use]
fn stop_backend_catching(be: &mut Box<dyn CaptureBackend>) -> bool {
    std::panic::catch_unwind(AssertUnwindSafe(|| be.stop())).is_ok()
}

impl Stream {
    /// Opens a stream from a configuration and a backend (does not start capturing yet).
    ///
    /// `config.chunk_ms` is assumed to be 20ms per the fixed contract. `ring_capacity_chunks`
    /// becomes the chunk ring capacity. The [`Normalizer`] is configured from the backend's
    /// [`native_format`](CaptureBackend::native_format).
    pub fn open(config: StreamConfig, backend: Box<dyn CaptureBackend>) -> Result<Stream> {
        if config.ring_capacity_chunks == 0 {
            return Err(Error::InvalidArg("ring_capacity_chunks must be > 0".into()));
        }
        // The input gain must be finite and >= 0.0 (NaN, infinity, and negatives are
        // InvalidArg).
        if !config.gain.is_finite() || config.gain < 0.0 {
            return Err(Error::InvalidArg(format!(
                "gain must be finite and >= 0.0, got {}",
                config.gain
            )));
        }
        // Validate that the output format is in the supported range (unsupported is
        // UnsupportedFormat).
        config.output.validate()?;
        // Validate the secondary output format the same way (only when configured).
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
        // Create the dedicated ring only when a secondary tap is configured (the public
        // chunk_ring<AudioChunk> is unchanged).
        let (secondary_producer, secondary_consumer) = if config.secondary_output.is_some() {
            let (p, c) = secondary_chunk_ring(config.ring_capacity_chunks);
            (Some(p), Some(c))
        } else {
            (None, None)
        };
        let events = Arc::new(Mutex::new(VecDeque::new()));

        let shared = Arc::new(SharedState {
            backend: Mutex::new(backend),
            raw_consumer: Mutex::new(None),
            raw_generation: AtomicU64::new(0),
            last_sample_ns: AtomicI64::new(0),
            stopping: AtomicBool::new(false),
            recovered_pending: AtomicBool::new(false),
            events: events.clone(),
            chunk_producer: Mutex::new(Some(chunk_producer)),
            native_format: Mutex::new(native_format),
            switching: AtomicBool::new(false),
            discontinuity_pending: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            delivery: Mutex::new(()),
            resume_generation: AtomicU64::new(0),
            gain_bits: AtomicU32::new(config.gain.to_bits()),
            recording_epoch_ns: AtomicI64::new(i64::MIN),
            denoise_enabled: AtomicBool::new(false),
            secondary_producer: Mutex::new(secondary_producer),
        });

        Ok(Stream {
            config,
            shared,
            chunk_consumer,
            secondary_consumer,
            events,
            worker: None,
            watchdog: None,
            started: false,
        })
    }

    /// Starts capturing.
    ///
    /// Creates the RawRing, starts the backend, and launches the ingest/processing thread and
    /// the watchdog thread. Does nothing if already started.
    pub fn start(&mut self) -> Result<()> {
        if self.started {
            return Ok(());
        }
        self.shared.stopping.store(false, Ordering::SeqCst);

        // A new start does not inherit the paused state (even if the previous run was stopped
        // while paused, a restart begins in the normal state).
        self.shared.paused.store(false, Ordering::SeqCst);

        // A new recording re-establishes a 0-based clock (does not carry over the previous
        // recording's epoch).
        self.shared
            .recording_epoch_ns
            .store(i64::MIN, Ordering::SeqCst);

        // First backend start: create the RawRing and hand the sink to the backend.
        Self::open_backend_once(&self.shared)?;

        // Take chunk_producer out to move it into the ingest/processing thread.
        // Recover and continue even on poison (it only takes the inner Option).
        let chunk_producer = self
            .shared
            .chunk_producer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .ok_or_else(|| Error::InvalidState("chunk producer already taken".into()))?;

        // When a secondary tap is configured, also take its producer and move it into the
        // ingest thread.
        let secondary_producer = self
            .shared
            .secondary_producer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();

        // Ingest/processing thread. The initial native_format is read from shared
        // (afterwards run_intake re-reads shared on every generation change to follow it).
        let worker_shared = self.shared.clone();
        // Recover and continue even on poison (it only reads the inner (u32, u16)).
        let initial_native = *self
            .shared
            .native_format
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let output = self.config.output;
        let secondary_output = self.config.secondary_output;
        let worker = thread::Builder::new()
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
            })
            .map_err(|e| Error::Backend(format!("spawn intake thread: {e}")))?;
        self.worker = Some(worker);

        // Watchdog thread.
        let wd_shared = self.shared.clone();
        let watchdog = thread::Builder::new()
            .name("flexaudio-watchdog".into())
            .spawn(move || {
                run_watchdog(wd_shared);
            })
            .map_err(|e| Error::Backend(format!("spawn watchdog thread: {e}")))?;
        self.watchdog = Some(watchdog);

        self.started = true;
        Ok(())
    }

    /// Stops capturing and joins all threads.
    ///
    /// Safe against reentry and a double stop. After stop, chunks already accumulated in the
    /// ring can be drained with [`poll_chunk`](Self::poll_chunk).
    pub fn stop(&mut self) {
        // Stop flag -> every thread exits at the top of its next loop.
        self.shared.stopping.store(true, Ordering::SeqCst);

        // Stop the backend and end its generator thread (stops the RT pushes).
        // Recover and try to stop even on poison. Even if stop panics, catch_unwind swallows
        // it and we proceed to join without re-poisoning the mutex (no silent death).
        {
            let mut be = self
                .shared
                .backend
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let _ = stop_backend_catching(&mut be);
        }

        // Join the threads.
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
        if let Some(h) = self.watchdog.take() {
            let _ = h.join();
        }

        self.started = false;
    }

    /// Pauses capture.
    ///
    /// Keeps the OS-side capture running and stops only the delivery of completed chunks.
    /// While paused, [`poll_chunk`](Self::poll_chunk) returns no new chunks. Ingest continues
    /// internally and keeps the device alive, so resuming is fast and the watchdog's stall
    /// judgment does not misfire. Does nothing if already paused (safe to call repeatedly).
    ///
    /// By the time this call returns, a chunk the ingest thread was assembling has either been
    /// enqueued or discarded. Once what is already queued has been drained,
    /// [`poll_chunk`](Self::poll_chunk) returns no new chunks.
    ///
    /// Calling it before [`start`](Self::start) still sets the flag, but it only takes effect
    /// once ingest starts running.
    pub fn pause(&self) {
        // Take delivery before setting the flag. It is the same lock as the ingest thread's
        // push, so after leaving here there is no window for "an assembled chunk entering the
        // queue later".
        let _g = self
            .shared
            .delivery
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        self.shared.paused.store(true, Ordering::SeqCst);
    }

    /// Releases [`pause`](Self::pause) and resumes delivery.
    ///
    /// After resuming, [`ChunkFlags::DISCONTINUITY`] is set on the first chunk delivered on
    /// each of the primary and secondary streams (telling the consumer that the audio jumped
    /// in time because of the pause). The `seq` of each chunk is continuous across the pause,
    /// and no silence is inserted for the paused interval.
    /// Does nothing if not paused (safe to call repeatedly).
    pub fn resume(&self) {
        // Take delivery so that the generation update -> paused=false forms a single enqueue
        // boundary. intake reads the generation under the same lock, so the first chunk of
        // each tap entering after resume is identified as DISCONTINUITY without being missed.
        let _g = self
            .shared
            .delivery
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Advance the generation only when actually paused (calling resume without a pause
        // does not produce a spurious DISCONTINUITY).
        if self.shared.paused.load(Ordering::SeqCst) {
            self.shared.resume_generation.fetch_add(1, Ordering::SeqCst);
            self.shared.paused.store(false, Ordering::SeqCst);
        }
    }

    /// Whether currently paused.
    pub fn is_paused(&self) -> bool {
        self.shared.paused.load(Ordering::SeqCst)
    }

    /// Changes the input gain (linear factor). 1.0 = unchanged, 2.0 = about +6dB, 0.0 =
    /// silence.
    ///
    /// Can be called at any time during capture and takes effect from the next completed chunk
    /// (chunks have 20ms granularity). Samples after multiplication are clamped to
    /// `-1.0..=1.0`. At 1.0 the samples are not touched at all (byte-exact pass-through).
    /// Unless finite and >= 0.0, returns [`Error::InvalidArg`] (the current value does not
    /// change).
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

    /// The current input gain (linear factor).
    pub fn gain(&self) -> f32 {
        f32::from_bits(self.shared.gain_bits.load(Ordering::Relaxed))
    }

    /// Takes one completed chunk (non-blocking). `None` if there is none.
    ///
    /// The returned chunk is interleaved `f32` in the output format (`config.output`).
    /// Chunks are a fixed 20ms by time, with `data.len() == frames * output.channels`.
    /// For the default `{48000, 2}`, `frames == 960` (`data.len() == 1920`).
    /// `peak`/`rms` are already computed over the final data. `seq` increases monotonically.
    pub fn poll_chunk(&mut self) -> Option<AudioChunk> {
        self.chunk_consumer.try_pop()
    }

    /// Takes one completed secondary-tap chunk (non-blocking). `None` if there is none.
    ///
    /// The secondary tap is produced only when `config.secondary_output` is `Some`. When it is
    /// not configured, this always returns `None`. Secondary chunks carry a `pts_ns` on the
    /// same recording clock (0-based) as the primary [`AudioChunk`], but the values are
    /// independent of the primary and lag it by 20-60ms, the group delay of the secondary
    /// Stage2 resampler. Correlate primary <-> secondary by `pts_ns` (time) (`seq` is an
    /// independent counter per tap).
    pub fn poll_secondary(&mut self) -> Option<SecondaryChunk> {
        self.secondary_consumer.as_mut().and_then(|c| c.try_pop())
    }

    /// Enables/disables noise suppression (RNNoise) on the internal canonical form.
    ///
    /// Call this before [`start`](Self::start) (it is applied when the ingest thread builds the
    /// Normalizer; a change during capture is applied at the next generation change = source
    /// switch / automatic recovery). When enabled, it is applied exactly once to the 48kHz/
    /// stereo internal canonical form, and both the primary and secondary taps receive the
    /// denoised audio (+10ms fixed latency). core does not depend on denoise; this facade
    /// plugs in the implementation.
    pub fn set_denoise(&self, enabled: bool) {
        self.shared.denoise_enabled.store(enabled, Ordering::SeqCst);
    }

    /// Takes one undelivered event (non-blocking). `None` if there is none.
    pub fn poll_event(&mut self) -> Option<Event> {
        self.events.lock().ok().and_then(|mut q| q.pop_front())
    }

    /// Total number of chunks the chunk ring has discarded with DROP_OLDEST so far.
    pub fn dropped_chunks(&self) -> u64 {
        self.chunk_consumer.dropped_count()
    }

    /// Reference to the current configuration.
    pub fn config(&self) -> &StreamConfig {
        &self.config
    }

    /// Native format `(sample_rate, channels)` of the current backend.
    ///
    /// The value obtained from the backend at open. It does not change on watchdog recovery,
    /// but it is updated to the new backend's value when the source is switched with
    /// [`switch_source`](Self::switch_source). For display / diagnostics (the output format
    /// is `config().output`).
    pub fn native_format(&self) -> (u32, u16) {
        // Recover and read the value even on poison (no cascading panic).
        *self
            .shared
            .native_format
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    // --- Internal ---

    /// (Re)starts the current `shared.backend`, puts a new RawRing/RawConsumer into the shared
    /// state, and advances the generation. Used both for the first start and for watchdog
    /// reopens.
    ///
    /// Steps:
    /// 1. Get the current backend's [`native_format`](CaptureBackend::native_format) and
    ///    update `shared.native_format` (unchanged when reopening the same backend; it also
    ///    follows if this is ever called with a different backend in the future).
    /// 2. Create a new RawRing with that rate/ch (so no leftovers in the old RawRing's format
    ///    are carried over = avoids phase corruption).
    /// 3. Start the backend.
    /// 4. Swap the new RawConsumer into the shared state (the old consumer is dropped) and
    ///    ++ the generation.
    /// 5. Set `last_sample_ns` to now to avoid an immediate stall judgment.
    ///
    /// The backend lock is taken only for start (assumes the caller does not hold the lock).
    /// The low-level switch ([`switch_backend`](Self::switch_backend)) replaces the backend
    /// directly and so does not go through this function (it reuses this function only when
    /// restoring the old source).
    fn open_backend_once(shared: &Arc<SharedState>) -> Result<()> {
        // Get the current backend's native format and reflect it into shared.
        // Recover and continue even on poison (the backend lock spans start and so may be
        // poisoned).
        let (rate, channels) = {
            let be = shared.backend.lock().unwrap_or_else(|e| e.into_inner());
            be.native_format()
        };
        {
            let mut nf = shared
                .native_format
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            *nf = (rate, channels);
        }

        // New RawRing (does not carry over leftovers in the old format).
        let (producer, consumer) = raw_ring(RAW_RING_SAMPLES);
        let sink = RawSink::new(producer, rate, channels);

        {
            // Recover even on poison. Even if the backend panics in start(), catch_unwind
            // converts it to Error::Backend before the mutex is poisoned, so the `?` here
            // propagates it to the caller (start() = Err to the caller / watchdog =
            // Event::Error).
            let mut be = shared.backend.lock().unwrap_or_else(|e| e.into_inner());
            start_backend_catching(&mut be, sink)?;
        }

        // Put the new consumer into the shared state and advance the generation (the old
        // consumer is dropped).
        {
            // Recover and swap even on poison (it only replaces the inner Option).
            let mut rc = shared
                .raw_consumer
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            *rc = Some(consumer);
        }
        shared.raw_generation.fetch_add(1, Ordering::SeqCst);

        // Treat the moment right after start as "the last arrival time" to avoid an immediate
        // stall judgment.
        shared
            .last_sample_ns
            .store(monotonic_now_ns(), Ordering::SeqCst);
        Ok(())
    }

    /// Low-level source switch. Replaces the current backend with a new backend and changes
    /// the input source while keeping the chunk stream (seq, PTS) continuous.
    ///
    /// `seq` is a local variable of the ingest thread and lives in neither the backend nor
    /// `SharedState`, so as long as it is not touched here it stays continuous across the
    /// replacement. For PTS, the ingest thread detects the generation change, rebuilds the
    /// `Normalizer`/`ClockNormalizer`, and re-anchors on the real time of the new source's
    /// first sample, so it stays monotonic.
    ///
    /// Steps (generation++ happens exactly once at the end; all atomics are SeqCst):
    /// - If not started, [`Error::InvalidState`].
    /// - `switching = true` (stops the watchdog's concurrent reopen).
    /// - Under the backend lock, `stop()` the old backend -> get the new backend's native
    ///   format and update `shared.native_format` -> new RawRing -> `new_backend.start(sink)`.
    ///   - Success: replace the backend with the new one and swap in the new consumer (the old
    ///     one is dropped).
    ///   - Failure: restart the old backend with
    ///     [`open_backend_once`](Self::open_backend_once) and continue with the old source
    ///     (continuity is not broken). Set `discontinuity_pending`, ++ the generation, set
    ///     `switching=false`, and return `Err`.
    /// - On success: `discontinuity_pending = true` (an intentional switch, so RECOVERED is not
    ///   set) -> `generation += 1` (exactly once at the end) -> `last_sample_ns = now` ->
    ///   `switching = false` -> `Ok`.
    ///
    /// Because it takes a [`Box<dyn CaptureBackend>`] directly, the switching behavior can be
    /// verified with a mock backend. The high-level entry point is
    /// [`switch_source`](Self::switch_source).
    ///
    /// `#[doc(hidden)] pub`: not a public API (not shown in the docs), but callable with a
    /// MockBackend from another crate's integration tests (`tests/integration.rs`).
    #[doc(hidden)]
    pub fn switch_backend(&mut self, new_backend: Box<dyn CaptureBackend>) -> Result<()> {
        if !self.started {
            return Err(Error::InvalidState(
                "switch_backend is only possible on a started stream".into(),
            ));
        }

        // Start of the switch: stop the watchdog first so it does not collide with its
        // stall -> reopen.
        self.shared.switching.store(true, Ordering::SeqCst);

        // Under the backend lock, do old stop -> new start in one go.
        // Recover and continue even on poison (the backend lock spans stop/start and so may be
        // poisoned; once recovered, the replacement can still be done correctly as is).
        {
            let mut be = self
                .shared
                .backend
                .lock()
                .unwrap_or_else(|e| e.into_inner());

            // Stop the old backend (stops the RT pushes). Even if it panics, catch_unwind
            // swallows it, avoiding mutex poisoning / cascading panics, and the switch
            // continues.
            let _ = stop_backend_catching(&mut be);

            // Native format of the new backend.
            let (rate, channels) = new_backend.native_format();

            // New RawRing (does not carry over leftovers in the old format).
            let (producer, consumer) = raw_ring(RAW_RING_SAMPLES);
            let sink = RawSink::new(producer, rate, channels);

            // Start the new backend. catch_unwind converts a panic to Error::Backend, so it
            // takes the Err branch below (restore the old source -> return Err). On failure,
            // fall back to the old source.
            let mut new_backend = new_backend;
            match start_backend_catching(&mut new_backend, sink) {
                Ok(()) => {
                    // The order matters. The ingest thread loads the generation outside the
                    // lock, then locks raw_consumer and pops. If the new consumer were put in
                    // first, the new source's native samples would flow into the old
                    // normalizer before the generation is ++'d, corrupting the phase. So the
                    // order is native_format update -> generation ++ (+ DISCONTINUITY etc.)
                    // -> finally swap the consumer/backend. This way, whenever the ingest side
                    // observes the new consumer it always sees the new generation, and it
                    // rebuilds the normalizer before popping.
                    //
                    // Update shared.native_format to the new source's value.
                    {
                        // Recover even on poison (it only updates the inner (u32, u16)).
                        let mut nf = self
                            .shared
                            .native_format
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        *nf = (rate, channels);
                    }
                    // An intentional switch, so only DISCONTINUITY, no RECOVERED.
                    self.shared
                        .discontinuity_pending
                        .store(true, Ordering::SeqCst);
                    // Use the moment right after start as the last arrival time (avoids an
                    // immediate stall judgment).
                    self.shared
                        .last_sample_ns
                        .store(monotonic_now_ns(), Ordering::SeqCst);
                    // Advance the generation (exactly once at the end). Done before swapping
                    // the consumer so the new generation is always visible when the new
                    // consumer is observed.
                    self.shared.raw_generation.fetch_add(1, Ordering::SeqCst);

                    // Replace the backend with the new one (the old backend is dropped).
                    *be = new_backend;
                    // Swap the new consumer into the shared state (the old consumer is
                    // dropped). Done last.
                    {
                        // Recover even on poison (it only replaces the Option).
                        let mut rc = self
                            .shared
                            .raw_consumer
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        *rc = Some(consumer);
                    }
                }
                Err(e) => {
                    // Starting the new source failed -> restart the old backend (still in
                    // `*be`) and continue. Holding the backend lock would make
                    // open_backend_once deadlock on re-locking, so release it here first and
                    // then restore.
                    drop(be);
                    // Reopen the old backend (native_format returns to the old backend's
                    // value).
                    let _ = Self::open_backend_once(&self.shared);
                    // Resuming the old source is also treated as a "discontinuity" (it was
                    // briefly interrupted).
                    self.shared
                        .discontinuity_pending
                        .store(true, Ordering::SeqCst);
                    // open_backend_once has already ++'d the generation. Reset switching and
                    // return Err.
                    self.shared.switching.store(false, Ordering::SeqCst);
                    return Err(e);
                }
            }
        }

        // --- Switch succeeded ---
        // generation++, the native_format update, and each flag were already done under the
        // backend lock (ordered so the new generation is visible before the new consumer is
        // observed). Here we only reset switching.
        self.shared.switching.store(false, Ordering::SeqCst);
        Ok(())
    }

    /// High-level entry point that switches the input source (mic/system/process) without
    /// stopping the recording.
    ///
    /// Builds the per-source backend from `new_config` with `build_backend` (private to the
    /// facade) (on failure the old source is left intact and `Err` is returned), and swaps it
    /// in with [`switch_backend`](Self::switch_backend). The output format (`output`) cannot
    /// be switched (changing the chunks' frames/data.len would break the continuous stream).
    /// A request to change it is rejected with [`Error::InvalidArg`].
    ///
    /// On success, only the mutable fields of `config` (`kind` / `device_id` / `target_pid` /
    /// `mode` / `exclude_self`) are updated to the new values. `output` / `chunk_ms`
    /// / `ring_capacity_chunks` are kept. `new_config.gain` is ignored too (the gain is stream
    /// state and does not change on a switch; change it with [`set_gain`](Self::set_gain)).
    ///
    /// # Errors
    /// - Not started -> [`Error::InvalidState`].
    /// - Request to change `output` -> [`Error::InvalidArg`].
    /// - Building the new source's backend fails (process PID missing, unsupported OS, etc.)
    ///   -> the error from `build_backend` (private to the facade) (the old source is intact).
    /// - Starting the new backend fails -> [`switch_backend`](Self::switch_backend) restores
    ///   the old source and then returns that error.
    pub fn switch_source(&mut self, new_config: StreamConfig) -> Result<()> {
        if !self.started {
            return Err(Error::InvalidState(
                "switch_source is only possible on a started stream".into(),
            ));
        }
        if new_config.output != self.config.output {
            return Err(Error::InvalidArg(
                "output format cannot change during switch_source".into(),
            ));
        }
        // The secondary tap's format is also fixed at open (the secondary Normalizer/ring is
        // not reconfigured during a switch).
        if new_config.secondary_output != self.config.secondary_output {
            return Err(Error::InvalidArg(
                "secondary output format cannot change during switch_source".into(),
            ));
        }
        // Build the new source's backend (on failure, return early with the old source
        // intact).
        let backend = crate::build_backend(&new_config)?;
        // Replace (switch_backend guarantees continuity).
        self.switch_backend(backend)?;
        // Only on success, update the mutable fields of config (output etc. are kept).
        self.config = StreamConfig {
            kind: new_config.kind,
            device_id: new_config.device_id,
            target_pid: new_config.target_pid,
            mode: new_config.mode,
            exclude_self: new_config.exclude_self,
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

/// Maps to an absolute time with recording start at 0. The computed pts of the first
/// delivered (primary) chunk is fixed exactly once as the recording epoch (overwriting the
/// sentinel `i64::MIN`) and subtracted from every chunk afterwards. Only the ingest thread
/// writes it, so there is no race. It stays fixed across reopen/switch from then on, so pts
/// is 0-based and continuous throughout the whole recording.
fn apply_epoch(shared: &SharedState, raw_pts: i64) -> i64 {
    let epoch = shared.recording_epoch_ns.load(Ordering::SeqCst);
    if epoch == i64::MIN {
        shared.recording_epoch_ns.store(raw_pts, Ordering::SeqCst);
        0
    } else {
        raw_pts - epoch
    }
}

/// Applies the input gain (linear factor) to a completed chunk's `data`. At 1.0 the samples
/// are not touched at all (byte-exact pass-through). Otherwise each sample is multiplied and
/// clamped to +/-1.0.
fn apply_gain(data: &mut [f32], gain: f32) {
    if gain != 1.0 {
        for x in data.iter_mut() {
            *x = (*x * gain).clamp(-1.0, 1.0);
        }
    }
}

/// Body of the ingest/processing thread.
///
/// Pops the RawConsumer -> feeds the [`Normalizer`] (shared Stage1 -> branches to the primary/
/// secondary Stage2 each) -> assigns `seq`, 0-based recording pts, peak/rms, and discontinuity
/// flags to completed chunks -> pushes to the primary/secondary rings. When it detects a
/// generation change (reopen / source switch), it rebuilds the Normalizer/Clock and sets
/// RECOVERED|DISCONTINUITY on the next chunk. It also detects RawRing overflow (loss on the
/// capture side) and sets DISCONTINUITY on both the next primary and secondary chunks. On
/// stop, it flushes the Normalizer to emit the trailing tail.
fn run_intake(
    shared: Arc<SharedState>,
    mut chunk_producer: ChunkProducer,
    mut secondary_producer: Option<SecondaryChunkProducer>,
    initial_native: (u32, u16),
    output: OutputFormat,
    secondary_output: Option<OutputFormat>,
) {
    let (mut rate, mut channels) = initial_native;
    // A Normalizer construction failure (rubato construction failure, etc.) does not die
    // silently; it emits Event::Error and exits.
    let mut normalizer = match build_normalizer(&shared, rate, channels, output, secondary_output) {
        Ok(n) => n,
        Err(e) => {
            shared.push_event(Event::Error(format!("normalizer init failed: {e}")));
            return;
        }
    };
    let mut clock = ClockNormalizer::new();
    let mut seq: u64 = 0; // Primary tap seq.
    let mut sec_seq: u64 = 0; // Secondary tap seq (a counter separate from the primary).
    let mut current_generation = shared.raw_generation.load(Ordering::SeqCst);
    // The resume generation is also observed independently per tap. The secondary tap has a
    // ring and consumer separate from the primary, so reusing the primary's observed
    // generation would fail to mark the secondary's first chunk.
    // A new intake starts from generation 0. Even if resume() lands between start() and the
    // worker launch, that resume must be reflected in the first delivery, so it does not
    // initialize from the shared current value and does not miss that transition.
    let mut primary_resume_generation = 0;
    let mut secondary_resume_generation = 0;
    // Previously observed cumulative RawRing overflow (resets to 0 when a generation change
    // recreates the ring).
    let mut overflow_baseline: u64 = 0;

    // Keep the discontinuity pending per tap (fan the shared AtomicBool out to both taps'
    // locals). rec_* is RECOVERED|DISCONTINUITY and disc_* is DISCONTINUITY only. Carrying
    // them over ensures they land on the first chunk after resume even if chunks are
    // discarded while paused.
    let mut rec_primary = false;
    let mut rec_secondary = false;
    let mut disc_primary = false;
    let mut disc_secondary = false;

    // Last delivered pts per tap. To guarantee the contract (0-based recording, non-
    // decreasing), each chunk's pts is clamped with this value and 0. Right after start, each
    // tap's PTS extrapolation can dip slightly negative (relative to the primary epoch, a few
    // ms), so it is floored at 0. A regression across a generation change is also absorbed
    // here.
    let mut last_pts_primary: i64 = 0;
    let mut last_pts_secondary: i64 = 0;

    let out_channels = output.channels.max(1) as usize;
    let sec_channels = secondary_output
        .map(|f| f.channels.max(1) as usize)
        .unwrap_or(1);

    // Scratch for popping (sized to the RawRing capacity; everything can be taken in one go).
    let mut scratch = vec![0.0f32; RAW_RING_SAMPLES];

    loop {
        let stopping = shared.stopping.load(Ordering::SeqCst);

        // Detect a generation change (reopen / source switch) -> reset to the new source.
        // native_format may change, so re-read shared and rebuild the stage-1
        // (native-dependent) Normalizer. The recording epoch is not reset here (0-based and
        // continuous across generations).
        let gen = shared.raw_generation.load(Ordering::SeqCst);
        if gen != current_generation {
            current_generation = gen;
            let nf = *shared
                .native_format
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            rate = nf.0;
            channels = nf.1;
            normalizer = match build_normalizer(&shared, rate, channels, output, secondary_output) {
                Ok(n) => n,
                Err(e) => {
                    shared.push_event(Event::Error(format!(
                        "normalizer rebuild failed after source change: {e}"
                    )));
                    return;
                }
            };
            clock = ClockNormalizer::new();
            // The new RawConsumer counts overflow from 0 (avoids a spurious huge delta).
            overflow_baseline = 0;
        }

        // Distribute the shared pendings to the per-tap locals. During a switch the watchdog
        // is stopped via switching, so both are not set at once, but if they are they are
        // combined with OR.
        if shared.recovered_pending.swap(false, Ordering::SeqCst) {
            rec_primary = true;
            rec_secondary = true;
        }
        if shared.discontinuity_pending.swap(false, Ordering::SeqCst) {
            disc_primary = true;
            disc_secondary = true;
        }

        // Take from the RawRing and feed the Normalizer. Also observe overflow (loss on the
        // capture side).
        let mut produced_any = false;
        let mut push_err: Option<Error> = None;
        let mut overflow_now = overflow_baseline;
        {
            let mut rc_guard = shared
                .raw_consumer
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(rc) = rc_guard.as_mut() {
                let got = rc.pop_slice(&mut scratch);
                overflow_now = rc.overflow_count();
                if got > 0 {
                    let samples = &scratch[..got];
                    // device PTS: monotonic approximation based on the native SR (arrival
                    // time).
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
            }
        }

        // If the push failed, emit Event::Error and end ingest (no silent death).
        if let Some(e) = push_err {
            shared.push_event(Event::Error(format!("normalizer push failed: {e}")));
            return;
        }

        // When a RawRing overflow is detected (RT overtook intake and discarded samples), set
        // DISCONTINUITY on both the next primary and secondary chunks (loss before the
        // canonical form = both taps are affected equally). pts already reflects the gap via
        // the wall-clock re-anchoring.
        if overflow_now > overflow_baseline {
            disc_primary = true;
            disc_secondary = true;
        }
        overflow_baseline = overflow_now;

        // On stop, flush the trailing tail (denoise delay line + resampler residue) and emit
        // all of it.
        if stopping {
            normalizer.flush();
        }

        // --- Primary tap: take all completed chunks and push them to the ChunkRing. ---
        let gain = f32::from_bits(shared.gain_bits.load(Ordering::Relaxed));
        let mut emitted_any = false;
        while let Some((mut data, raw_pts)) = normalizer.pop_chunk() {
            // Discard while paused (pop advances out_frame_origin, so pts keeps moving
            // forward). The pendings are not consumed but carried over, and land on the first
            // chunk after resume.
            if shared.paused.load(Ordering::SeqCst) {
                continue;
            }
            // Make 0-based -> clamp to non-negative, non-decreasing (contract: 0-based
            // recording, non-decreasing).
            let pts_ns = apply_epoch(&shared, raw_pts).max(last_pts_primary);
            last_pts_primary = pts_ns;
            debug_assert_eq!(data.len() % out_channels, 0);
            let frames = data.len() / out_channels;
            apply_gain(&mut data, gain);
            let (peak, rms) = peak_rms(&data);

            let mut flags = ChunkFlags::empty();
            if rec_primary {
                flags |= ChunkFlags::RECOVERED | ChunkFlags::DISCONTINUITY;
            }
            if disc_primary {
                flags |= ChunkFlags::DISCONTINUITY;
            }

            let mut chunk = AudioChunk {
                data,
                frames,
                pts_ns,
                seq,
                flags,
                dropped_before: 0, // Overwritten by the ChunkRing on push.
                peak,
                rms,
            };

            // Re-check under the same delivery lock as pause() before pushing.
            // Even if pause returned while the chunk was being assembled, it either does not
            // enter the queue (discarded), or the pause side waits for this push before
            // returning.
            {
                let _g = shared.delivery.lock().unwrap_or_else(|e| e.into_inner());
                if shared.paused.load(Ordering::SeqCst) {
                    continue;
                }
                let resume_generation = shared.resume_generation.load(Ordering::SeqCst);
                if resume_generation != primary_resume_generation {
                    chunk.flags |= ChunkFlags::DISCONTINUITY;
                }
                rec_primary = false;
                disc_primary = false;
                seq += 1;
                if let Some(total) = chunk_producer.push(chunk) {
                    shared.push_event(Event::ChunkDropped { count: total });
                }
                // Advance the observed generation only within the same critical section as
                // a successful push. It does not advance for chunks discarded during a pause,
                // so the mark carries over to the first delivery after resume.
                primary_resume_generation = resume_generation;
                emitted_any = true;
            }
        }

        // --- Secondary tap: only when configured. Pops, discards, and sets flags
        // symmetrically with the primary. ---
        if let Some(sec_prod) = secondary_producer.as_mut() {
            while let Some((mut samples, raw_pts)) = normalizer.pop_secondary() {
                // While paused, the secondary is also popped and discarded (prevents
                // unbounded growth of out_buf).
                if shared.paused.load(Ordering::SeqCst) {
                    continue;
                }
                // Make 0-based -> clamp to non-negative, non-decreasing (contract: 0-based
                // recording, non-decreasing).
                let pts_ns = apply_epoch(&shared, raw_pts).max(last_pts_secondary);
                last_pts_secondary = pts_ns;
                debug_assert_eq!(samples.len() % sec_channels, 0);
                let frames = samples.len() / sec_channels;
                apply_gain(&mut samples, gain);
                let (peak, rms) = peak_rms(&samples);

                let mut flags = ChunkFlags::empty();
                if rec_secondary {
                    flags |= ChunkFlags::RECOVERED | ChunkFlags::DISCONTINUITY;
                }
                if disc_secondary {
                    flags |= ChunkFlags::DISCONTINUITY;
                }

                let mut chunk = SecondaryChunk {
                    samples,
                    frames,
                    pts_ns,
                    seq: sec_seq,
                    flags,
                    dropped_before: 0, // Overwritten by the secondary ring on push.
                    peak,
                    rms,
                };
                {
                    let _g = shared.delivery.lock().unwrap_or_else(|e| e.into_inner());
                    if shared.paused.load(Ordering::SeqCst) {
                        continue;
                    }
                    let resume_generation = shared.resume_generation.load(Ordering::SeqCst);
                    if resume_generation != secondary_resume_generation {
                        chunk.flags |= ChunkFlags::DISCONTINUITY;
                    }
                    rec_secondary = false;
                    disc_secondary = false;
                    sec_seq += 1;
                    // Secondary-tap drops are observable via dropped_before (no dedicated
                    // event is emitted).
                    let _ = sec_prod.push(chunk);
                    // Advance only once actually enqueued into the secondary ring, which is
                    // independent of the primary.
                    secondary_resume_generation = resume_generation;
                    emitted_any = true;
                }
            }
        }

        // On a stop signal the tail has been emitted, so exit.
        if stopping {
            break;
        }

        // If there is no data, sleep briefly so the CPU does not spin.
        if !produced_any && !emitted_any {
            thread::sleep(Duration::from_millis(2));
        }
    }
}

/// Body of the watchdog thread.
///
/// Watches the last sample arrival time with a ~250ms tick, and when arrival stops for longer
/// than [`STALL_THRESHOLD`], reopens the backend with exponential backoff. Fires
/// [`Event::StreamStalled`] on a stall and [`Event::StreamRecovered`] on recovery.
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

        // During a source switch, do not judge stalls or reopen (switch_backend briefly stops
        // the old backend, making the stream idle, so this prevents a mistaken concurrent
        // reopen). A switch ends by updating last_sample_ns to now, so normal watching resumes
        // from the next tick.
        if shared.switching.load(Ordering::SeqCst) {
            continue;
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

        // Stalled: stop the backend and try to reopen.
        // Recover and try to stop even on poison, and even if stop panics catch_unwind
        // swallows it (the watchdog does not die silently and proceeds to the reopen).
        {
            let mut be = shared.backend.lock().unwrap_or_else(|e| e.into_inner());
            let _ = stop_backend_catching(&mut be);
        }

        if shared.stopping.load(Ordering::SeqCst) {
            break;
        }

        let reopened = match Stream::open_backend_once(&shared) {
            Ok(()) => true,
            Err(e) => {
                shared.push_event(Event::Error(format!("reopen failed: {e}")));
                false
            }
        };

        if reopened {
            // open_backend_once has already updated last_sample_ns to now and ++'d the
            // generation. Set recovered_pending so that RECOVERED|DISCONTINUITY is set on the
            // first chunk after recovery (the ingest thread consumes it on the next chunk).
            // Whether the recovery is real is confirmed on the next tick by looking at idle.
            shared.recovered_pending.store(true, Ordering::SeqCst);
            stalled = false;
            shared.push_event(Event::StreamRecovered);
            backoff = BACKOFF_MIN;
        } else {
            // Failure -> wait with exponential backoff (with jitter) and then retry.
            let jittered = jittered_backoff(backoff);
            sleep_interruptible(&shared, jittered);
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }
    }
}

/// Computes peak (maximum absolute value over all samples) and rms (root mean square,
/// linear) from the final interleaved `data` in the output format.
///
/// A single pass over a 20ms chunk (at most 1920 samples), so the cost is tiny. Empty data
/// gives `(0.0, 0.0)`.
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

/// Adds light time-based jitter (about +/-12.5%) to the backoff (does not use `rand`).
fn jittered_backoff(base: Duration) -> Duration {
    let base_ns = base.as_nanos() as u64;
    // Use the low bits of the monotonic ns as a pseudo-random source.
    let entropy = monotonic_now_ns() as u64;
    // Range of +/-(base/8).
    let span = (base_ns / 8).max(1);
    let delta = (entropy % (2 * span)) as i64 - span as i64;
    let result = base_ns as i64 + delta;
    Duration::from_nanos(result.max(0) as u64)
}

/// Sleeps in small steps while watching `stopping` (reacts quickly to a stop signal).
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
    use std::time::Instant;

    /// Helper that collects chunks by calling poll_chunk until the deadline.
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

    /// Waits until the condition `cond` becomes true (at most `timeout`). Returns true if it
    /// does.
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

    /// Extracts the Err from the result of `Stream::open` (`Stream` does not implement `Debug`,
    /// so `expect_err` cannot be used). Panics with a message if it was Ok.
    fn open_err(result: Result<Stream>, ctx: &str) -> Error {
        match result {
            Ok(_) => panic!("{ctx}: expected an error but got Ok"),
            Err(e) => e,
        }
    }

    // --- Input validation (error paths of Stream::open) ---

    /// `ring_capacity_chunks == 0` is rejected with InvalidArg (a ring capacity of 0 is
    /// invalid).
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
            "should be InvalidArg: {err:?}"
        );
    }

    /// An unsupported output format (channels=3) fails validate with UnsupportedFormat.
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
            "should be UnsupportedFormat: {err:?}"
        );
    }

    /// An extreme output rate (out of range) is also rejected with UnsupportedFormat.
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
            "should be UnsupportedFormat: {err:?}"
        );
    }

    /// If the backend's native_format is 0 (rate=0 / ch=0), it is rejected with InvalidArg.
    #[test]
    fn open_rejects_zero_native_format() {
        // MockBackend::new applies max(1) internally, so it cannot produce 0. Define a
        // test-only backend with a zero native_format to verify this.
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
            "should be InvalidArg: {err:?}"
        );
    }

    // --- poll_event (pull-style event retrieval) ---

    /// Events can be pulled with `poll_event`. Makes the ChunkRing capacity tiny to force
    /// DROP_OLDEST, and checks that `Event::ChunkDropped` is observable via poll_event.
    #[test]
    fn poll_event_yields_chunk_dropped() {
        // Capacity 1 + almost no polling -> DROP_OLDEST happens quickly.
        let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
        let config = StreamConfig {
            ring_capacity_chunks: 1,
            ..Default::default()
        };
        let mut stream = Stream::open(config, backend).expect("open");
        stream.start().expect("start");

        // Overflow the chunk ring by waiting without calling poll_chunk.
        let got_drop = wait_until(
            || {
                // Only run poll_event (no poll_chunk = clog the ring).
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
        assert!(got_drop, "ChunkDropped should be obtainable via poll_event");
    }

    /// With no events, `poll_event` returns None (non-blocking, empty queue).
    #[test]
    fn poll_event_is_none_when_empty() {
        let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
        let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");
        // Before start, the event queue is empty.
        assert!(stream.poll_event().is_none());
    }

    // --- Watchdog: stall detection -> automatic recovery -> RECOVERED ---

    /// Verifies end-to-end with StallableMockBackend that "the first session stops feeding
    /// midway -> the watchdog detects the stall after STALL_THRESHOLD -> reopens the backend
    /// -> RECOVERED|DISCONTINUITY on the first chunk after recovery".
    ///
    /// Observed:
    /// 1. `Event::StreamStalled` fires (stall judgment).
    /// 2. `Event::StreamRecovered` fires (reopen succeeded).
    /// 3. ChunkFlags::RECOVERED is set on the first chunk after recovery (together with
    ///    DISCONTINUITY).
    /// 4. seq increases monotonically throughout (not reset by the recovery).
    #[test]
    fn watchdog_detects_stall_and_flags_recovered() {
        // Feed for 300ms, then stall the first session.
        let backend = Box::new(StallableMockBackend::new(
            48_000,
            2,
            440.0,
            Duration::from_millis(300),
        ));
        let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");
        stream.start().expect("start");

        let mut chunks: Vec<AudioChunk> = Vec::new();
        let mut saw_stalled = false;
        let mut saw_recovered = false;

        // Wait long enough for stall detection (>=2s) -> reopen -> the recovery chunk (at
        // most 8 seconds).
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
        // Also drain what remains after stop.
        while let Some(c) = stream.poll_chunk() {
            if c.flags.contains(ChunkFlags::RECOVERED) {
                recovered_chunk_seen = true;
            }
            chunks.push(c);
        }

        assert!(saw_stalled, "Event::StreamStalled should fire");
        assert!(saw_recovered, "Event::StreamRecovered should fire");
        assert!(
            recovered_chunk_seen,
            "RECOVERED should be set on the first chunk after recovery"
        );

        // A chunk with RECOVERED also has DISCONTINUITY (as designed).
        let recovered: Vec<&AudioChunk> = chunks
            .iter()
            .filter(|c| c.flags.contains(ChunkFlags::RECOVERED))
            .collect();
        assert!(!recovered.is_empty());
        for c in &recovered {
            assert!(
                c.flags.contains(ChunkFlags::DISCONTINUITY),
                "RECOVERED should be accompanied by DISCONTINUITY: flags={:?}",
                c.flags
            );
        }

        // seq increases monotonically throughout (not reset by the recovery).
        for w in chunks.windows(2) {
            assert!(
                w[1].seq > w[0].seq,
                "seq increases monotonically even across the recovery: {} -> {}",
                w[0].seq,
                w[1].seq
            );
        }
    }

    /// Under steady feeding (no stall), RECOVERED is never set and StreamStalled does not
    /// arrive (regression: the watchdog does not false-positive). Rather than using
    /// StallableMockBackend with a value small enough not to stall, this checks briefly with
    /// the regular MockBackend.
    #[test]
    fn no_recovered_flag_under_steady_feed() {
        let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
        let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");
        stream.start().expect("start");

        // Check the flags and events over a short time below STALL_THRESHOLD.
        let chunks = collect_for(&mut stream, Duration::from_millis(500));
        let mut saw_stalled = false;
        while let Some(ev) = stream.poll_event() {
            if matches!(ev, Event::StreamStalled) {
                saw_stalled = true;
            }
        }
        stream.stop();

        assert!(
            !chunks.is_empty(),
            "chunks should arrive under steady feeding"
        );
        assert!(
            !saw_stalled,
            "no stall should be judged under steady feeding"
        );
        for c in &chunks {
            assert!(
                !c.flags.contains(ChunkFlags::RECOVERED),
                "RECOVERED is not set under steady feeding: flags={:?}",
                c.flags
            );
        }
    }

    // --- pause / resume (stop only the delivery) ---

    /// Pausing stops new chunks from arriving. Checks that at least one chunk is received
    /// before the pause and that there are zero new ones in a fixed window after the pause.
    #[test]
    fn pause_stops_delivering_chunks() {
        let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
        let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");
        stream.start().expect("start");

        // Wait until at least one chunk arrives before pausing.
        let got_before = wait_until(|| stream.poll_chunk().is_some(), Duration::from_secs(2));
        assert!(got_before, "chunks should arrive before the pause");

        // Pause. Drain whatever remained in the ring right after.
        stream.pause();
        while stream.poll_chunk().is_some() {}

        // No new chunks should arrive in the window after the pause.
        let after = collect_for(&mut stream, Duration::from_millis(300));
        stream.stop();
        assert!(
            after.is_empty(),
            "no new chunks should arrive while paused: {} arrived",
            after.len()
        );
    }

    /// Even a long pause exceeding STALL_THRESHOLD is not judged a stall. Delivery stops, but
    /// the OS-side ingest (updates of last_sample_ns) continues, so the watchdog does not
    /// detect idle. The pause window is made sufficiently longer than STALL_THRESHOLD + the
    /// watchdog tick.
    #[test]
    fn long_pause_does_not_trigger_stall() {
        let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
        let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");
        stream.start().expect("start");

        // Wait until at least one chunk arrives before pausing.
        let got_before = wait_until(|| stream.poll_chunk().is_some(), Duration::from_secs(2));
        assert!(got_before, "chunks should arrive before the pause");

        // Pause. Drain whatever remained in the ring right after.
        stream.pause();
        while stream.poll_chunk().is_some() {}

        // Keep the pause for a time that surely exceeds STALL_THRESHOLD (2s), collecting
        // events meanwhile.
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
            // It stays paused throughout the pause.
            assert!(
                stream.is_paused(),
                "is_paused should be true during the pause window"
            );
            thread::sleep(Duration::from_millis(20));
        }

        // No stall judgment or recovery happens at all even with a long pause (this is the
        // main point).
        assert!(
            !saw_stalled,
            "StreamStalled should not fire even with a long pause"
        );
        assert!(
            !saw_recovered,
            "StreamRecovered should not fire either, since there was no stall"
        );

        // Resuming restarts chunk delivery.
        stream.resume();
        let resumed = wait_until(|| stream.poll_chunk().is_some(), Duration::from_secs(2));
        stream.stop();
        assert!(resumed, "chunk delivery should restart after resume");
    }

    /// DISCONTINUITY is set on the first chunk after resume, and seq is continuous across the
    /// pause (if the last one before the pause is N, the first after resume is N+1).
    /// dropped_before is 0 as well.
    #[test]
    fn resume_flags_discontinuity_and_keeps_seq_continuous() {
        let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
        let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");
        stream.start().expect("start");

        // Collect the chunks before the pause and note the last seq.
        let before = collect_for(&mut stream, Duration::from_millis(200));
        assert!(!before.is_empty(), "chunks should arrive before the pause");
        let last_seq = before.last().unwrap().seq;

        // Pause and drain what remained in the ring. Update the last seq.
        stream.pause();
        let mut last_seq = last_seq;
        while let Some(c) = stream.poll_chunk() {
            last_seq = c.seq;
        }

        // Lightly check that nothing new arrives while paused, then resume.
        assert!(collect_for(&mut stream, Duration::from_millis(150)).is_empty());
        stream.resume();

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
        assert!(got, "chunks should arrive after resume");

        let first = first_after.unwrap();
        assert!(
            first.flags.contains(ChunkFlags::DISCONTINUITY),
            "DISCONTINUITY should be set on the first chunk after resume: flags={:?}",
            first.flags
        );
        assert_eq!(
            first.seq,
            last_seq + 1,
            "seq should be continuous across the pause ({last_seq} -> {})",
            first.seq
        );
        assert_eq!(first.dropped_before, 0, "the pause should cause no drops");
    }

    /// Even when resume is repeated after emptying both rings following a pause, the first
    /// chunk after resume on each independent stream always has DISCONTINUITY set.
    ///
    /// This is a stress test targeting the race between resume and intake. The primary and
    /// secondary are streams with separate rings and separate seqs, so checking only one of
    /// them is not enough.
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
            // pause() is mutually exclusive with delivery, so after draining here no old
            // chunk newly enters. Always empty the secondary ring at the same time too.
            while stream.poll_chunk().is_some() {}
            while stream.poll_secondary().is_some() {}

            stream.resume();

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
            "{failures} / {ROUNDS} resume attempts had no DISCONTINUITY on the first primary \
             or secondary chunk"
        );
    }

    /// Calling resume without a pause does not set DISCONTINUITY on the next chunk
    /// (no-op).
    #[test]
    fn resume_without_pause_is_noop() {
        let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
        let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");
        stream.start().expect("start");

        // Discard the first batch of chunks to flush out the RECOVERED/DISCONTINUITY right
        // after start.
        let _ = collect_for(&mut stream, Duration::from_millis(200));

        // Resume without being paused.
        stream.resume();

        // DISCONTINUITY is not set on subsequent chunks.
        let after = collect_for(&mut stream, Duration::from_millis(200));
        stream.stop();
        assert!(!after.is_empty(), "chunks should arrive");
        for c in &after {
            assert!(
                !c.flags.contains(ChunkFlags::DISCONTINUITY),
                "resume without a pause does not set DISCONTINUITY: flags={:?}",
                c.flags
            );
        }
    }

    /// Even when pause is called twice, a single resume restarts normally (safe to call
    /// repeatedly).
    #[test]
    fn double_pause_then_single_resume_recovers() {
        let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
        let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");
        stream.start().expect("start");

        let before = collect_for(&mut stream, Duration::from_millis(200));
        assert!(!before.is_empty(), "chunks should arrive before the pause");

        // Call pause twice.
        stream.pause();
        stream.pause();
        assert!(stream.is_paused());
        while stream.poll_chunk().is_some() {}
        assert!(collect_for(&mut stream, Duration::from_millis(150)).is_empty());

        // Resume once.
        stream.resume();
        assert!(!stream.is_paused());
        let got = wait_until(|| stream.poll_chunk().is_some(), Duration::from_secs(2));
        stream.stop();
        assert!(got, "delivery should restart with a single resume");
    }

    // --- Input gain (config.gain / set_gain) ---

    /// The gain specified in config is reflected in completed chunks' data and the peak/rms
    /// meters. MockBackend's sine wave has amplitude 0.5, so with gain 2.0 the chunk peak is
    /// about 1.0, and with gain 0.5 about 0.25. Also checks that peak is computed from the
    /// data after gain is applied (the meters show the actual post-gain level).
    #[test]
    fn gain_scales_samples_and_meters() {
        // (gain, expected peak range). Sine amplitude 0.5 x gain.
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
            assert!(!chunks.is_empty(), "chunks should arrive with gain={gain}");

            // peak matches the post-gain data (the meters show the actual post-gain level).
            let mut max_peak = 0.0f32;
            for c in &chunks {
                let recomputed = c.data.iter().fold(0.0f32, |m, &x| m.max(x.abs()));
                assert_eq!(
                    c.peak, recomputed,
                    "peak should be computed from the post-gain data"
                );
                max_peak = max_peak.max(c.peak);
            }
            assert!(
                (lo..=hi).contains(&max_peak),
                "the peak for gain={gain} should be within {lo}..={hi}: {max_peak}"
            );
        }
    }

    /// set_gain during capture takes effect from the next chunk. After starting at 1.0 and
    /// receiving a chunk, set_gain(0.0) makes all subsequent chunks all-zero with peak 0.
    #[test]
    fn set_gain_takes_effect_mid_stream() {
        let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
        let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");
        stream.start().expect("start");
        assert_eq!(stream.gain(), 1.0, "the default gain is 1.0");

        // First wait until a normal chunk arrives.
        let got_before = wait_until(|| stream.poll_chunk().is_some(), Duration::from_secs(2));
        assert!(got_before, "chunks should arrive before set_gain");

        // Set the gain to 0.0 (silence). It takes effect from the next completed chunk (20ms
        // granularity).
        stream.set_gain(0.0).expect("set_gain(0.0)");
        assert_eq!(stream.gain(), 0.0);

        // Chunks completed before the setting may still flow in, so wait for a silent chunk
        // to arrive.
        let got_silent = wait_until(
            || matches!(stream.poll_chunk(), Some(c) if c.peak == 0.0),
            Duration::from_secs(2),
        );
        assert!(
            got_silent,
            "a silent chunk should arrive after set_gain(0.0)"
        );

        // Subsequent chunks stay all-zero with peak 0 and rms 0.
        let after = collect_for(&mut stream, Duration::from_millis(300));
        stream.stop();
        assert!(
            !after.is_empty(),
            "chunks should keep flowing even when silent"
        );
        for c in &after {
            assert!(
                c.data.iter().all(|&x| x == 0.0),
                "all samples should be 0 with gain 0.0"
            );
            assert_eq!(c.peak, 0.0);
            assert_eq!(c.rms, 0.0);
        }
    }

    /// Even with a large gain, samples are clamped to +/-1.0. Sine amplitude 0.5 x gain 100
    /// would reach 50 without clamping, but all samples stay within +/-1.0 and the peak is
    /// exactly 1.0.
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
        assert!(!chunks.is_empty(), "chunks should arrive");

        let mut max_peak = 0.0f32;
        for c in &chunks {
            assert!(
                c.data.iter().all(|&x| (-1.0..=1.0).contains(&x)),
                "samples should not exceed +/-1.0"
            );
            max_peak = max_peak.max(c.peak);
        }
        assert_eq!(max_peak, 1.0, "clamping should make the peak exactly 1.0");
    }

    /// An invalid gain (negative, NaN) is rejected as InvalidArg by both open and set_gain.
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
            "should be InvalidArg: {err:?}"
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
            "should be InvalidArg: {err:?}"
        );

        // set_gain: negative and NaN are InvalidArg, and the current value does not change.
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
            "a failed set_gain should not change the current value"
        );
    }

    // --- Robustness: no silent death from a backend panic (prevents poison cascading panics) ---
    //
    // In these tests, "the test process itself does not go down from a panic" is itself the
    // proof of "no silent death / no cascading panic" (if it went down, the test result would
    // be FAILED). In addition, they assert that the panic is observable as Err /
    // Event::Error, confirming it is not just swallowed (a panic silently erased).

    /// Even if the backend's `start()` panics, the process does not go down and `start()`
    /// returns `Err(Error::Backend)` (catch_unwind converts it before the mutex is poisoned,
    /// so the ingest/watchdog threads are not even started and no cascading panic occurs).
    #[test]
    fn backend_panic_in_start_returns_err_not_silent_death() {
        let backend = Box::new(PanickingMockBackend::new(
            48_000,
            2,
            440.0,
            PanicMode::Start,
        ));
        let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");

        // start() must not propagate the panic and must return Err(Error::Backend).
        let result = stream.start();
        match result {
            Ok(()) => panic!("start() returned Ok even though the backend panicked in start"),
            Err(Error::Backend(msg)) => {
                assert!(
                    msg.contains("panicked"),
                    "Error::Backend should have a message showing it came from a panic: {msg}"
                );
            }
            Err(other) => panic!("expected Error::Backend but got a different error: {other:?}"),
        }

        // After the start failure it is in the not-started state. stop does not panic (even
        // with no threads started).
        stream.stop();
    }

    /// Even if the backend's `stop()` panics, the process does not go down and `stop()`
    /// returns normally (catch_unwind swallows it and the backend mutex is not poisoned, so
    /// the ingest/watchdog threads that were running until then do not panic in cascade).
    #[test]
    fn backend_panic_in_stop_does_not_kill_process() {
        let backend = Box::new(PanickingMockBackend::new(48_000, 2, 440.0, PanicMode::Stop));
        let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");
        stream.start().expect("start");

        // Run it for a bit to get the ingest/watchdog threads actually running
        // (confirm chunks flow = the happy path is unchanged).
        let chunks = collect_for(&mut stream, Duration::from_millis(300));
        assert!(
            !chunks.is_empty(),
            "chunks should flow as usual before stop (happy path unchanged)"
        );

        // Inside stop(), backend.stop() panics, but catch_unwind swallows it and the mutex is
        // not poisoned. This test not going down from a panic is itself the proof.
        stream.stop();

        // Even after stop, poll is usable without a cascading panic (additional check that
        // nothing is poisoned).
        let _ = stream.poll_chunk();
        let _ = stream.poll_event();
    }

    /// Even if the backend panics during the watchdog's reopen, the watchdog thread does not
    /// die silently in cascade, and the panic surfaces as `Event::Error` ("reopen failed:
    /// ..."). The process does not go down (catch_unwind prevents mutex poisoning, and the
    /// reopen failure becomes Event::Error via the `Err` of `open_backend_once`).
    #[test]
    fn backend_panic_on_watchdog_reopen_surfaces_event_error() {
        // Feed for 300ms -> stall -> panic on the watchdog reopen.
        let backend = Box::new(StallThenPanicOnReopenBackend::new(
            48_000,
            2,
            440.0,
            Duration::from_millis(300),
        ));
        let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");
        stream.start().expect("start");

        // Wait long enough for stall detection (>=2s) -> reopen attempt (panic ->
        // Event::Error) (at most 8 seconds).
        let mut saw_stalled = false;
        let mut saw_reopen_error = false;
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline && !saw_reopen_error {
            // Also run poll_chunk (so other paths do not stall on a clogged ring).
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
            "the stall should be detected (Event::StreamStalled)"
        );
        assert!(
            saw_reopen_error,
            "a backend panic on reopen should surface as Event::Error(\"reopen failed: ...\") \
             (no silent death)"
        );
    }

    // --- Absolute clock (recording starts at 0) ---

    /// The pts_ns of the first delivered chunk is the recording epoch itself, so it starts
    /// at exactly 0.
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
        assert!(got, "the first chunk should arrive");
        let first = first.unwrap();
        assert_eq!(
            first.pts_ns, 0,
            "the first delivered chunk should be 0-based for the recording (pts_ns == 0): {}",
            first.pts_ns
        );
    }

    /// Across a pause, pts advances by the pause duration (a clock of real capture time).
    /// Also checks DISCONTINUITY, continuous seq, and dropped_before 0 on the first chunk
    /// after resume.
    #[test]
    fn pause_preserves_absolute_clock() {
        let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
        let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");
        stream.start().expect("start");

        // Collect the chunks before the pause and note the last (pts_ns, seq).
        let before = collect_for(&mut stream, Duration::from_millis(250));
        assert!(!before.is_empty(), "chunks should arrive before the pause");
        let mut last = before.last().cloned().unwrap();

        stream.pause();
        while let Some(c) = stream.poll_chunk() {
            last = c;
        }

        // Hold a pause of known duration D (below STALL_THRESHOLD).
        let d = Duration::from_millis(600);
        thread::sleep(d);
        stream.resume();

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
        assert!(got, "chunks should arrive after resume");
        let first = first_after.unwrap();

        assert!(
            first.flags.contains(ChunkFlags::DISCONTINUITY),
            "DISCONTINUITY should be set on the first chunk after resume: {:?}",
            first.flags
        );
        assert_eq!(
            first.seq,
            last.seq + 1,
            "seq should be continuous across the pause"
        );
        assert_eq!(first.dropped_before, 0, "the pause causes no drops");

        // pts advances by the pause duration D (real capture time). To avoid CI jitter, it is
        // bounded below by D*0.8 and above by D + a margin.
        let delta = first.pts_ns - last.pts_ns;
        let d_ns = d.as_nanos() as i64;
        assert!(
            delta >= d_ns * 4 / 5,
            "pts should advance by the pause (>= {} ns): delta={delta} ns",
            d_ns * 4 / 5
        );
        assert!(
            delta <= d_ns + 500_000_000,
            "pts should not advance excessively (<= D + 500ms): delta={delta} ns"
        );
    }

    // --- Dual output (primary + secondary tap) ---

    /// Setting secondary_output delivers the primary (48k/stereo) and secondary (16k/mono)
    /// simultaneously. Secondary chunks are 320 samples, and their pts are on the same 0-based
    /// clock as the primary.
    #[test]
    fn dual_output_delivers_primary_and_secondary() {
        let config = StreamConfig {
            secondary_output: Some(OutputFormat {
                sample_rate: 16_000,
                channels: 1,
            }),
            // Make it large so the head (pts 0) is not flushed away by DROP_OLDEST during the
            // collection window.
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

        assert!(!primary.is_empty(), "primary chunks should arrive");
        assert!(!secondary.is_empty(), "secondary chunks should arrive");
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
        // Both taps are 0-based and non-decreasing. The primary's head is 0.
        assert_eq!(primary[0].pts_ns, 0, "the primary head is 0-based");
        for w in secondary.windows(2) {
            assert!(
                w[1].pts_ns >= w[0].pts_ns,
                "secondary pts is non-decreasing"
            );
        }
        assert!(
            secondary[0].pts_ns >= 0,
            "secondary pts is non-negative (relative to the primary epoch)"
        );
        // The secondary seq is its own counter, starting at 0 and monotonic.
        assert_eq!(secondary[0].seq, 0);
        for w in secondary.windows(2) {
            assert_eq!(
                w[1].seq,
                w[0].seq + 1,
                "secondary seq is a monotonic sequence"
            );
        }
    }

    /// switch_source cannot change secondary_output (fixed at open).
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

        // A switch request that changes the secondary format is rejected with InvalidArg
        // (before the backend is built).
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
            "changing the secondary format should be InvalidArg: {err:?}"
        );
    }

    // --- RawRing overflow -> DISCONTINUITY ---

    /// Pseudo backend that overflows the RawRing. After start it keeps pushing bursts of
    /// twice the ring capacity each, always causing overflow. Test-only.
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
                // Push twice the ring capacity per burst (always overflows).
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

    /// A sustained RawRing overflow is detected and DISCONTINUITY is set on some chunk (on a
    /// fresh start there is no source of discontinuity other than overflow, so DISCONTINUITY
    /// = from the overflow).
    #[test]
    fn ring_overflow_marks_discontinuity() {
        let backend = Box::new(FloodingMockBackend::new());
        // Make the ChunkRing large so chunks are not flushed away by DROP_OLDEST before being
        // observed.
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
            "a RawRing overflow should set DISCONTINUITY"
        );
    }

    // --- Inject denoise into the internal canonical form (via core InnerProcessor) ---

    /// After set_denoise(true), both the primary and secondary taps keep being delivered
    /// (a smoke test of the denoise wiring). The actual suppression effect is covered by
    /// core's flush/processing tests.
    #[test]
    fn denoise_enabled_still_delivers_both_taps() {
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
        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline {
            while let Some(_c) = stream.poll_chunk() {
                primary += 1;
            }
            while let Some(c) = stream.poll_secondary() {
                assert_eq!(c.samples.len(), 320, "secondary is 16k/mono = 320 samples");
                secondary += 1;
            }
            thread::sleep(Duration::from_millis(5));
        }
        stream.stop();
        assert!(
            primary > 0,
            "primary chunks should arrive even with denoise enabled"
        );
        assert!(
            secondary > 0,
            "secondary chunks should arrive even with denoise enabled"
        );
    }
}
