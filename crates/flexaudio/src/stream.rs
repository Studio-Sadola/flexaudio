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
use flexaudio_core::normalizer::{InnerProcessor, NormalizedChunk, Normalizer};
use flexaudio_core::raw_ring::{raw_ring, RawConsumer};
use flexaudio_core::secondary_ring::{
    secondary_chunk_ring, SecondaryChunkConsumer, SecondaryChunkProducer,
};
use flexaudio_core::types::{
    AudioChunk, AudioLoss, ChunkFlags, Error, ErrorContext, ErrorGroup, ErrorKind, Event,
    Operation, OutputFormat, OutputTap, Permission, Result, SecondaryChunk, ShutdownReport,
    StreamConfig,
};
use flexaudio_core::CaptureDiagnostics;
use std::num::NonZeroU64;

mod control;
mod delivery;
mod intake;
mod lifecycle;
mod processor;
mod shared;
mod source;
mod tap_drain;
#[cfg(test)]
use control::{drain_backend_events, MailboxDrain};
use control::{run_watchdog, stop_backend_reconciling};
use intake::run_intake;
use processor::build_normalizer;
mod terminal;
use terminal::TerminalFailure;

#[cfg(test)]
mod contract_tests;
#[cfg(test)]
mod permission_tests;
#[cfg(test)]
mod recovery_tests;

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
    shutdown: Option<ShutdownReport>,
}

/// State shared by the intake thread, watchdog thread, and main thread.
struct SharedState {
    cleanup: Mutex<Vec<Error>>,
    backend_stopped: AtomicBool,
    raw_diagnostics: Mutex<Option<CaptureDiagnostics>>,
    raw_overflow_reported: Mutex<u64>,
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
    losses: Result<Vec<AudioLoss>>,
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

/// Catch owner panics without retaining the panic payload or poisoning the mutex.
fn stop_backend_catching(be: &mut Box<dyn CaptureBackend>) -> Result<()> {
    match std::panic::catch_unwind(AssertUnwindSafe(|| be.stop_checked())) {
        Ok(result) => result,
        Err(_) => Err(Error::Backend("backend panicked during stop".into())),
    }
    .map_err(|error| error.with_context(ErrorContext::new(Operation::Stop)))
}

impl Stream {
    /// Open a stream from the configuration and backend (capture has not started yet).
    ///
    /// The fixed contract assumes `config.chunk_ms` is 20ms. `ring_capacity_chunks` sets the chunk
    /// ring capacity. Configure [`Normalizer`] from the backend's
    /// [`native_format`](CaptureBackend::native_format).
    pub fn open(config: StreamConfig, backend: Box<dyn CaptureBackend>) -> Result<Stream> {
        validate_chunk_ms(config.chunk_ms)?;
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

        Normalizer::new(native_format.0, native_format.1, config.output)?;
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
            cleanup: Mutex::new(Vec::new()),
            backend_stopped: AtomicBool::new(false),
            raw_diagnostics: Mutex::new(None),
            raw_overflow_reported: Mutex::new(0),
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
            shutdown: None,
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
        let _delivery = self
            .shared
            .delivery
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if self.shared.terminal.is_failed() {
            return None;
        }
        self.capture_consumer.as_mut()?.try_pop()
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.stop();
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
fn apply_gain(data: &mut [f32], gain: f32) -> bool {
    let mut clipped = false;
    if gain != 1.0 {
        for x in data.iter_mut() {
            let scaled = *x * gain;
            let clamped = scaled.clamp(-1.0, 1.0);
            clipped |= !(-1.0..=1.0).contains(&scaled) && !scaled.is_nan();
            *x = clamped;
        }
    }
    clipped
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
mod tests;

#[cfg(test)]
mod repro_tests;

#[cfg(test)]
#[path = "stream_frame_tests.rs"]
mod frame_tests;

pub(crate) fn validate_chunk_ms(chunk_ms: u32) -> Result<()> {
    if chunk_ms == 20 {
        Ok(())
    } else {
        Err(Error::InvalidArg("chunk_ms must be 20".into()))
    }
}
fn with_cleanup(primary: Error, cleanup: Option<Error>) -> Error {
    match cleanup {
        Some(error) => Error::Multiple(ErrorGroup::new(primary, error, Vec::new())),
        None => primary,
    }
}

fn cleanup_error_since(shared: &SharedState, first: usize) -> Option<Error> {
    let mut errors = shared
        .cleanup
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .into_iter()
        .skip(first);
    match (errors.next(), errors.next()) {
        (None, _) => None,
        (Some(error), None) => Some(error),
        (Some(primary), Some(first)) => Some(Error::Multiple(ErrorGroup::new(
            primary,
            first,
            errors.collect(),
        ))),
    }
}

fn is_terminal_kind(error: &Error) -> bool {
    matches!(
        error.kind(),
        ErrorKind::PermissionDenied | ErrorKind::NativeFormatChanged
    )
}
