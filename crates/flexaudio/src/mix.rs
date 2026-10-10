//! A composite backend that mixes mic + system into one stream: [`CompositeBackend`].
//!
//! It owns two child backends, mic and system, normalizes each child's audio to the
//! internal canonical form (48 kHz/stereo), then sums them with per-side gain. It
//! appears as a single backend to [`Stream`](crate::Stream). The Stream itself is
//! unchanged, so seq/PTS, the watchdog, pause, global gain, and switch_source all work
//! as before.
//!
//! # Thread layout
//! - Child backend RT threads: each only pushes to its dedicated child RawRing (the
//!   existing backends remain untouched).
//! - Mixer thread (one, `flexaudio-mix`): pops child rings → converts each with its
//!   [`Normalizer`] to 48 kHz/stereo → sums aligned frames using per-side gain (clamped
//!   to ±1.0) → pushes to the real sink. Mic and system use separate clocks and can
//!   differ by several to hundreds of ppm, so drift correction ([`LinearStitcher`] +
//!   [`DriftController`]) compensates by slightly resampling only the system side. This
//!   is not an RT thread, so heap allocation is allowed (but scratch buffers are reused
//!   to avoid steady-state allocations in the loop).

use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use flexaudio_core::backend::{CaptureBackend, RawSink};
use flexaudio_core::clock::monotonic_now_ns;
use flexaudio_core::normalizer::Normalizer;
use flexaudio_core::raw_ring::{raw_ring, RawConsumer};
use flexaudio_core::types::{Error, Event, OutputFormat, Result, CHANNELS, SAMPLE_RATE};

use crate::stream::RAW_RING_SAMPLES;

#[cfg(test)]
mod permission_tests;

/// If one side supplies nothing for longer than this, continue mixing and fill the
/// missing samples with silence (0.0). Normalization emits only 20 ms chunks, so arrival
/// jitter of 2–3 chunks is considered normal. Continue the overall recording even if
/// supply stops for longer, for example when nothing is playing on the system side.
const STARVATION_FILL_THRESHOLD: Duration = Duration::from_millis(60);

/// Maximum size of one side's normalized FIFO, in f32 samples. This is a safety limit:
/// discard the oldest samples above about 500 ms (48 kHz × 2 channels × 0.5 s = 48,000).
/// Drift correction ([`DriftController`]) handles rate differences between child clocks,
/// so this limit should not normally be reached while correction is working. Keep it as
/// a last resort for anomalies beyond its ±500 ppm range, such as runaway supply on one
/// side.
const FIFO_MAX_SAMPLES: usize = 48_000;

/// How long to wait when neither side has data to mix (same approach as stream.rs's
/// capture thread).
const IDLE_SLEEP: Duration = Duration::from_millis(2);

/// Range in which drift correction can adjust the system-side read ratio r (1.0 ± 500
/// ppm). Consumer-device clock drift is usually tens to a few hundred ppm, so this
/// covers it. Within this range, linear interpolation distortion is negligible because
/// interpolation points remain very close to the source samples.
const DRIFT_RATIO_LIMIT: f64 = 500e-6;

/// Interval for reevaluating the ratio, in f32 samples of mixed output: once per 100 ms
/// of output. This is five 20 ms normalization chunks, coarser than the arrival
/// granularity but much finer than the drift timescale (on the order of minutes).
const DRIFT_UPDATE_INTERVAL_SAMPLES: usize = (SAMPLE_RATE as usize / 10) * CHANNELS as usize;

/// EMA coefficient for the FIFO level difference. With a 100 ms update interval, the
/// time constant is about one second. It smooths chunk-arrival jitter (sawtooth FIFO
/// changes at 20 ms granularity) while still tracking drift changes.
const DRIFT_EMA_ALPHA: f64 = 0.1;

/// P-control gain. A 200 ms FIFO difference (19,200 samples) reaches the 500 ppm
/// clamp limit. This gentle gain keeps the steady-state difference for real drift
/// (tens to hundreds of ppm) at tens to a little over a hundred milliseconds (drift ÷
/// gain), well below the 500 ms safety limit.
const DRIFT_GAIN: f64 = DRIFT_RATIO_LIMIT / 19_200.0;

/// Maximum ratio change per update (slew): 20 ppm per 100 ms, so even a full clamp-range
/// change takes five seconds. This prevents FIFO measurement noise from abruptly
/// changing the ratio and causing pitch fluctuations.
const DRIFT_SLEW_PER_UPDATE: f64 = 20e-6;

/// Composite backend that sums the mic and system child backends in the internal
/// canonical form.
///
/// [`native_format`](CaptureBackend::native_format) always returns the internal
/// canonical form `(48000, 2)`, making Stream's first resampler stage effectively a
/// pass-through. Children are injected through the constructor (tests can pass mocks).
/// The facade's `build_backend` constructs the real children.
pub(crate) struct CompositeBackend {
    mic: Box<dyn CaptureBackend>,
    system: Box<dyn CaptureBackend>,
    mic_gain: f32,
    system_gain: f32,
    /// Stop signal for the mixer thread. Replace it with a new Arc on every start so it
    /// cannot be confused with a leftover from an old thread.
    stopping: Arc<AtomicBool>,
    /// Mixer thread handle. `Some` means it is running.
    mixer: Option<JoinHandle<()>>,
    /// Alternate mailbox priority so a busy child cannot starve the other lane.
    poll_system_first: bool,
}

impl CompositeBackend {
    /// Create with two injected children and per-side gains. The caller (facade's
    /// `build_backend`) must validate that gains are finite and non-negative.
    pub(crate) fn new(
        mic: Box<dyn CaptureBackend>,
        system: Box<dyn CaptureBackend>,
        mic_gain: f32,
        system_gain: f32,
    ) -> Self {
        Self {
            mic,
            system,
            mic_gain,
            system_gain,
            stopping: Arc::new(AtomicBool::new(false)),
            mixer: None,
            poll_system_first: false,
        }
    }
}

impl CaptureBackend for CompositeBackend {
    fn native_format(&self) -> (u32, u16) {
        // Mixing always uses the internal canonical form. Stream's first stage is
        // effectively a pass-through.
        (SAMPLE_RATE, CHANNELS)
    }

    fn start(&mut self, sink: RawSink) -> Result<()> {
        // A second start while running is a no-op (CaptureBackend contract).
        if self.mixer.is_some() {
            return Ok(());
        }

        // Return immediately if mic fails to start. If system fails, stop mic before
        // returning the error; never report success with only one side running.
        let mic_lane = start_child(&mut self.mic)?;
        let system_lane = match start_child(&mut self.system) {
            Ok(lane) => lane,
            Err(e) => {
                stop_child(&mut self.mic);
                return Err(e);
            }
        };

        // Start the mixer thread. Create a fresh stop flag on every start so the flag
        // from a previous stop is not reused.
        self.stopping = Arc::new(AtomicBool::new(false));
        let stopping = self.stopping.clone();
        let mic_gain = self.mic_gain;
        let system_gain = self.system_gain;
        let mixer = thread::Builder::new()
            .name("flexaudio-mix".into())
            .spawn(move || {
                run_mixer(mic_lane, system_lane, mic_gain, system_gain, sink, stopping);
            })
            .map_err(|e| Error::Backend(format!("spawn mix thread: {e}")));
        match mixer {
            Ok(handle) => {
                self.mixer = Some(handle);
                Ok(())
            }
            Err(e) => {
                // If the thread cannot start, stop both children and return an error;
                // never leave only one side running.
                stop_child(&mut self.mic);
                stop_child(&mut self.system);
                Err(e)
            }
        }
    }

    fn stop(&mut self) {
        // Set the stop flag → join the mixer thread → stop both children. This is
        // idempotent: if not running, only the child stops run, and those are idempotent
        // by contract too.
        self.stopping.store(true, Ordering::SeqCst);
        if let Some(h) = self.mixer.take() {
            let _ = h.join();
        }
        stop_child(&mut self.mic);
        stop_child(&mut self.system);
    }

    fn poll_event(&mut self) -> Option<Event> {
        self.poll_system_first = !self.poll_system_first;
        let event = if self.poll_system_first {
            self.system.poll_event().or_else(|| self.mic.poll_event())
        } else {
            self.mic.poll_event().or_else(|| self.system.poll_event())
        };
        // The stream closes delivery before invoking stop on this composite.
        // Joining children here would allow buffered delivery during shutdown.
        event
    }
}

impl Drop for CompositeBackend {
    fn drop(&mut self) {
        // Do not leave the mixer thread or children running if dropped without stop.
        self.stop();
    }
}

/// All capture state for one child side (child ring consumer, normalizer, and
/// normalized FIFO).
struct ChildLane {
    consumer: RawConsumer,
    /// Child native format → internal canonical form (48 kHz/stereo). Since output is
    /// fixed to the canonical form, the second stage is a pass-through.
    normalizer: Normalizer,
    /// FIFO of normalized samples (48 kHz/stereo interleaved).
    fifo: Vec<f32>,
    /// Time when this side last supplied normalized samples (for starvation detection).
    last_supply: Instant,
}

impl ChildLane {
    /// Pop from the child ring, normalize, and append completed output to the FIFO.
    ///
    /// If the FIFO exceeds [`FIFO_MAX_SAMPLES`], discard the oldest samples as a guard
    /// against unbounded growth. Return `Err` on a rubato processing failure so the
    /// caller can stop the mixer thread.
    fn ingest(&mut self, scratch: &mut [f32]) -> Result<()> {
        let got = self.consumer.pop_slice(scratch);
        if got == 0 {
            return Ok(());
        }
        // The sink does not use pts (the wiring layer handles it separately by
        // contract), but pass monotonic now as the normalizer's anchor.
        self.normalizer.push(&scratch[..got], monotonic_now_ns())?;
        let mut supplied = false;
        while let Some((chunk, _pts)) = self.normalizer.pop_chunk() {
            self.fifo.extend_from_slice(&chunk);
            supplied = true;
        }
        if supplied {
            self.last_supply = Instant::now();
            if self.fifo.len() > FIFO_MAX_SAMPLES {
                let excess = self.fifo.len() - FIFO_MAX_SAMPLES;
                self.fifo.drain(..excess);
            }
        }
        Ok(())
    }

    /// Whether this side has supplied nothing for at least
    /// [`STARVATION_FILL_THRESHOLD`].
    fn is_starved(&self, now: Instant) -> bool {
        now.duration_since(self.last_supply) >= STARVATION_FILL_THRESHOLD
    }
}

/// Fine resampler for the system lane (linear interpolation stitcher).
///
/// Use mic as the reference clock (it is natural to align the recording timeline with the
/// person's voice on the mic side). Read only the system FIFO at ratio r using linear
/// interpolation to compensate for the rate difference between child clocks. Keep only
/// the fractional read position as state.
struct LinearStitcher {
    /// Fractional read position from the FIFO's first frame (in frames, [0, 1)).
    frac: f64,
}

impl LinearStitcher {
    fn new() -> Self {
        Self { frac: 0.0 }
    }

    /// Number of output frames that can be interpolated at `ratio` when the FIFO has
    /// `fifo_frames` frames. The zero-based k-th output uses the two frames around
    /// position `frac + k×ratio`, so output is possible only while that position does
    /// not exceed the final frame F-1 (the exact final frame is valid because the right-hand
    /// interpolation term has zero weight).
    fn producible(&self, fifo_frames: usize, ratio: f64) -> usize {
        if fifo_frames == 0 {
            return 0;
        }
        let span = (fifo_frames - 1) as f64 - self.frac;
        if span < 0.0 {
            return 0;
        }
        (span / ratio) as usize + 1
    }

    /// Read `out_frames` from the system FIFO using linear interpolation at `ratio`,
    /// appending to `out` in interleaved form. Discard consumed frames from the FIFO and
    /// carry the fractional position forward so phase remains continuous across block
    /// boundaries. The caller must ensure `out_frames <= producible(...)`.
    fn pull(&mut self, fifo: &mut Vec<f32>, ratio: f64, out_frames: usize, out: &mut Vec<f32>) {
        let ch = CHANNELS as usize;
        let frames = fifo.len() / ch;
        debug_assert!(out_frames <= self.producible(frames, ratio));
        for k in 0..out_frames {
            let pos = self.frac + k as f64 * ratio;
            let left = pos as usize;
            // Clamp the right edge only when the position is exactly on the final frame
            // (the weight is then zero, so the interpolation result is unchanged).
            let right = (left + 1).min(frames - 1);
            let w = (pos - left as f64) as f32;
            for c in 0..ch {
                let a = fifo[left * ch + c];
                let b = fifo[right * ch + c];
                out.push(a + w * (b - a));
            }
        }
        // Compute the next read position. Discard consumed whole frames and keep only
        // the fractional part.
        let end = self.frac + out_frames as f64 * ratio;
        let consumed = (end as usize).min(frames);
        self.frac = end - consumed as f64;
        fifo.drain(..consumed * ch);
    }

    /// Reset phase when the starvation path flushes the entire system FIFO.
    fn reset(&mut self) {
        self.frac = 0.0;
    }
}

/// Feedback controller that sets read ratio r from the FIFO level difference (EMA + P
/// control + slew).
///
/// Every [`DRIFT_UPDATE_INTERVAL_SAMPLES`] of mixed output, take an exponential moving
/// average of the post-consumption FIFO level difference (mic - system). If the
/// difference is negative (system is accumulating), increase r to consume it faster; if
/// positive, decrease r. P control is sufficient because the level difference itself
/// integrates the rate difference, so proportional correction balances it at a finite
/// level.
struct DriftController {
    /// Current read ratio r, clamped around 1.0 by ±[`DRIFT_RATIO_LIMIT`].
    ratio: f64,
    /// Exponential moving average of the level difference (mic_len - system_len), in
    /// f32 samples.
    ema_diff: f64,
    /// Mixed output samples since the last update.
    pending_samples: usize,
}

impl DriftController {
    fn new() -> Self {
        Self {
            ratio: 1.0,
            ema_diff: 0.0,
            pending_samples: 0,
        }
    }

    /// Record mixed output progress and update the ratio once it reaches
    /// [`DRIFT_UPDATE_INTERVAL_SAMPLES`]. Pass post-consumption levels; pre-consumption
    /// levels would include the samples consumed in this iteration in the difference.
    fn on_output(&mut self, samples: usize, mic_len: usize, system_len: usize) {
        self.pending_samples += samples;
        if self.pending_samples >= DRIFT_UPDATE_INTERVAL_SAMPLES {
            self.pending_samples = 0;
            self.update(mic_len, system_len);
        }
    }

    /// Update the EMA of the level difference and adjust the ratio by one step using P
    /// control + slew.
    fn update(&mut self, mic_len: usize, system_len: usize) {
        let diff = mic_len as f64 - system_len as f64;
        self.ema_diff += DRIFT_EMA_ALPHA * (diff - self.ema_diff);
        let target = (1.0 - DRIFT_GAIN * self.ema_diff)
            .clamp(1.0 - DRIFT_RATIO_LIMIT, 1.0 + DRIFT_RATIO_LIMIT);
        let step = (target - self.ratio).clamp(-DRIFT_SLEW_PER_UPDATE, DRIFT_SLEW_PER_UPDATE);
        self.ratio += step;
    }
}

/// Drift correction state (stitcher + ratio controller), held once per mixer thread.
struct DriftCorrection {
    stitcher: LinearStitcher,
    controller: DriftController,
}

impl DriftCorrection {
    fn new() -> Self {
        Self {
            stitcher: LinearStitcher::new(),
            controller: DriftController::new(),
        }
    }
}

/// Start one child: create a dedicated child RawRing (same capacity as stream.rs) and
/// call `start` with a [`RawSink`] in the child's native format. Return a [`ChildLane`]
/// on success.
///
/// Convert a panic from the child's `start` into [`Error::Backend`] with catch_unwind
/// (same intent as stream.rs's start_backend_catching: prevent cascading panics in the
/// mixer thread or caller).
fn start_child(child: &mut Box<dyn CaptureBackend>) -> Result<ChildLane> {
    let (rate, channels) = child.native_format();
    if rate == 0 || channels == 0 {
        return Err(Error::InvalidArg(
            "mix child native_format must have non-zero rate and channels".into(),
        ));
    }
    let (producer, consumer) = raw_ring(RAW_RING_SAMPLES);
    let sink = RawSink::new(producer, rate, channels);
    match std::panic::catch_unwind(AssertUnwindSafe(|| child.start(sink))) {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(e),
        Err(_) => return Err(Error::Backend("mix child panicked during start()".into())),
    }
    // Child native format → internal canonical form (48 kHz/stereo). The Normalizer's
    // output is fixed to the canonical form, so the second stage is always pass-through.
    let normalizer = Normalizer::new(
        rate,
        channels,
        OutputFormat {
            sample_rate: SAMPLE_RATE,
            channels: CHANNELS,
        },
    )
    .inspect_err(|_| {
        // If the normalizer cannot be created, stop the child before returning an error
        // so no started child is left running.
        stop_child(child);
    })?;
    Ok(ChildLane {
        consumer,
        normalizer,
        fifo: Vec::with_capacity(FIFO_MAX_SAMPLES),
        last_supply: Instant::now(),
    })
}

/// Call the child's `stop` inside catch_unwind so a panic does not propagate (same
/// intent as stream.rs's stop_backend_catching).
fn stop_child(child: &mut Box<dyn CaptureBackend>) {
    let _ = std::panic::catch_unwind(AssertUnwindSafe(|| child.stop()));
}

/// Mixer thread entry point.
///
/// First wait for initial supply from both sides with [`prime_lanes`] (up to the
/// starvation threshold), then ingest each child (pop → normalize → FIFO), sum aligned
/// frames using per-side gain, and push them to the real sink. If one side supplies
/// nothing for at least [`STARVATION_FILL_THRESHOLD`], fill its missing data with
/// silence and continue (recording keeps flowing even when the system side is silent).
/// If neither side has data, sleep for [`IDLE_SLEEP`].
///
/// A normalization failure (a theoretical rubato failure) ends the loop. No more samples
/// will flow, so Stream's watchdog detects the stall and reopens the backend.
fn run_mixer(
    mut mic: ChildLane,
    mut system: ChildLane,
    mic_gain: f32,
    system_gain: f32,
    mut sink: RawSink,
    stopping: Arc<AtomicBool>,
) {
    // Scratch buffers for popping (child ring capacity) and mixed output. Reuse them in
    // the loop.
    let mut scratch = vec![0.0f32; RAW_RING_SAMPLES];
    let mut mixed: Vec<f32> = Vec::with_capacity(FIFO_MAX_SAMPLES);
    // Drift correction state for child clocks (fresh on each start so the previous
    // recording's ratio is not carried over).
    let mut drift = DriftCorrection::new();

    // Child threads may start at different times, so wait for both sides to begin
    // flowing (up to the starvation threshold) before mixing. This prevents the start
    // of a recording from containing only one side.
    if !prime_lanes(&mut mic, &mut system, &mut scratch, &stopping) {
        return;
    }

    loop {
        if stopping.load(Ordering::SeqCst) {
            break;
        }

        if mic.ingest(&mut scratch).is_err() || system.ingest(&mut scratch).is_err() {
            // Stop mixing if normalization fails; let the watchdog reopen the backend.
            return;
        }

        let pushed = mix_and_push(
            &mut mic,
            &mut system,
            mic_gain,
            system_gain,
            &mut drift,
            &mut sink,
            &mut mixed,
        );

        if !pushed {
            thread::sleep(IDLE_SLEEP);
        }
    }
}

/// Prime the lanes before mixing. Child backend threads may start at different times,
/// so poll with [`IDLE_SLEEP`] until both FIFOs receive their first normalized samples.
/// Without this, starvation fill could start while the slower side's ring is empty,
/// leaving only one side at the start of the recording.
///
/// The wait is limited by [`STARVATION_FILL_THRESHOLD`]. If one side never supplies
/// data (for example, nothing is playing on the system side), begin mixing after the
/// same timeout used for ordinary starvation. Stop waiting when a stop signal arrives.
/// Return `false` on normalization failure so the caller can end the mixer thread.
fn prime_lanes(
    mic: &mut ChildLane,
    system: &mut ChildLane,
    scratch: &mut [f32],
    stopping: &AtomicBool,
) -> bool {
    let start = Instant::now();
    while !stopping.load(Ordering::SeqCst) {
        if mic.ingest(scratch).is_err() || system.ingest(scratch).is_err() {
            return false;
        }
        if (!mic.fifo.is_empty() && !system.fifo.is_empty())
            || start.elapsed() >= STARVATION_FILL_THRESHOLD
        {
            break;
        }
        thread::sleep(IDLE_SLEEP);
    }
    true
}

/// Take the mixable amount from both FIFOs, sum with per-side gain (clamped to ±1.0),
/// and push to the sink. Return `true` if anything was pushed.
///
/// How much to take:
/// - Data on both sides (steady state): the minimum of the mic inventory and the amount
///   interpolable from system. Mic is consumed at its native rate; [`LinearStitcher`]
///   reads system at ratio r using linear interpolation to compensate for child-clock
///   drift. [`DriftController`] adjusts r based on post-consumption levels.
/// - Data on one side and starvation on the other (no supply for at least 60 ms): mix
///   all available data against silence (fill missing samples with 0.0). Do not apply
///   correction on this path; retain the existing starvation semantics.
/// - Otherwise (both empty and the other side is not yet starved): do nothing and wait
///   for data to align.
fn mix_and_push(
    mic: &mut ChildLane,
    system: &mut ChildLane,
    mic_gain: f32,
    system_gain: f32,
    drift: &mut DriftCorrection,
    sink: &mut RawSink,
    mixed: &mut Vec<f32>,
) -> bool {
    let ch = CHANNELS as usize;
    let ratio = drift.controller.ratio;
    let steady_frames =
        (mic.fifo.len() / ch).min(drift.stitcher.producible(system.fifo.len() / ch, ratio));
    if steady_frames > 0 {
        // Steady-state path: read the system side at ratio r with linear interpolation,
        // then mix the sides.
        mixed.clear();
        drift
            .stitcher
            .pull(&mut system.fifo, ratio, steady_frames, mixed);
        let count = steady_frames * ch;
        for (i, out) in mixed.iter_mut().enumerate() {
            *out = (mic.fifo[i] * mic_gain + *out * system_gain).clamp(-1.0, 1.0);
        }
        mic.fifo.drain(..count);
        drift
            .controller
            .on_output(count, mic.fifo.len(), system.fifo.len());
        // As in stream.rs capture, monotonic now is sufficient for pts (the sink handles
        // it separately by contract).
        sink.push(mixed, monotonic_now_ns());
        return true;
    }

    // Starvation path (no correction; retain existing semantics).
    let now = Instant::now();
    let (mic_take, system_take) = if !mic.fifo.is_empty() && system.is_starved(now) {
        // System stopped supplying: output all mic data. Flush any remaining fraction
        // (at most one frame) that lacked a right interpolation frame, then reset phase.
        drift.stitcher.reset();
        (mic.fifo.len(), system.fifo.len())
    } else if mic.fifo.is_empty() && !system.fifo.is_empty() && mic.is_starved(now) {
        drift.stitcher.reset();
        (0, system.fifo.len())
    } else {
        return false;
    };

    let count = mic_take.max(system_take);
    mixed.clear();
    for i in 0..count {
        let m = if i < mic_take { mic.fifo[i] } else { 0.0 };
        let s = if i < system_take { system.fifo[i] } else { 0.0 };
        mixed.push((m * mic_gain + s * system_gain).clamp(-1.0, 1.0));
    }
    mic.fifo.drain(..mic_take);
    system.fifo.drain(..system_take);

    // As in stream.rs capture, monotonic now is sufficient for pts (the sink handles it
    // separately by contract).
    sink.push(mixed, monotonic_now_ns());
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use flexaudio_core::raw_ring::raw_ring;
    use std::sync::atomic::AtomicU32;

    /// Test-only child backend that supplies constant-amplitude (DC) samples as fast as
    /// possible.
    ///
    /// Sine waves (MockBackend) introduce phase issues when mixing two sources, so use
    /// DC signals for deterministic verification of the mix. If `feed_for` is set, feed
    /// for that duration and then stop pushing while keeping the thread alive, to
    /// reproduce starvation on one side.
    ///
    /// Supply uses a saturating strategy rather than real-time pacing (10 ms sleeps):
    /// push as fast as the child ring accepts data, yielding for 1 ms before retrying if
    /// it is full. Real-time pacing can fall behind due to coarse sleep granularity on a
    /// slow test machine. When the mixer wakes, one FIFO may be empty, causing a
    /// starvation-filled single-side chunk to make the mix-value check scheduler-
    /// dependent. Saturating supply keeps both FIFOs nonempty whenever the mixer wakes,
    /// limiting the check to mix arithmetic (scheduling resilience is covered by
    /// mix_survives_one_side_starvation). With DC, drops and partial writes when full do
    /// not affect the values.
    struct ConstBackend {
        sample_rate: u32,
        channels: u16,
        value: f32,
        feed_for: Option<Duration>,
        running: Arc<AtomicBool>,
        handle: Option<JoinHandle<()>>,
    }

    impl ConstBackend {
        fn new(value: f32, feed_for: Option<Duration>) -> Self {
            Self {
                sample_rate: 48_000,
                channels: 2,
                value,
                feed_for,
                running: Arc::new(AtomicBool::new(false)),
                handle: None,
            }
        }
    }

    impl CaptureBackend for ConstBackend {
        fn native_format(&self) -> (u32, u16) {
            (self.sample_rate, self.channels)
        }

        fn start(&mut self, mut sink: RawSink) -> Result<()> {
            if self.running.load(Ordering::SeqCst) {
                return Ok(());
            }
            self.running.store(true, Ordering::SeqCst);
            let running = self.running.clone();
            let sample_rate = self.sample_rate;
            let channels = self.channels as usize;
            let value = self.value;
            let feed_for = self.feed_for;
            let handle = thread::Builder::new()
                .name("flexaudio-const-gen".into())
                .spawn(move || {
                    let frames_per_block = (sample_rate as usize / 100).max(1); // About 10 ms
                    let block = vec![value; frames_per_block * channels];
                    let start = Instant::now();
                    while running.load(Ordering::SeqCst) {
                        let feeding = feed_for.is_none_or(|d| start.elapsed() < d);
                        if !feeding {
                            // Once the feed duration ends, supply nothing further to
                            // reproduce one-sided starvation. Sleep while waiting for
                            // the stop signal.
                            thread::sleep(Duration::from_millis(5));
                            continue;
                        }
                        // Saturating supply: push again immediately if all data fit;
                        // yield for 1 ms and retry if the ring was full (push is
                        // nonblocking and drops data that does not fit; harmless for DC).
                        let accepted = sink.push(&block, start.elapsed().as_nanos() as i64);
                        if accepted < block.len() {
                            thread::sleep(Duration::from_millis(1));
                        }
                    }
                })
                .map_err(|e| Error::Backend(format!("spawn const gen thread: {e}")))?;
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

    impl Drop for ConstBackend {
        fn drop(&mut self) {
            self.stop();
        }
    }

    /// Test-only backend whose `start` always returns Err.
    struct FailingStartBackend;

    impl CaptureBackend for FailingStartBackend {
        fn native_format(&self) -> (u32, u16) {
            (48_000, 2)
        }
        fn start(&mut self, _sink: RawSink) -> Result<()> {
            Err(Error::Backend("intentional start failure".into()))
        }
        fn stop(&mut self) {}
    }

    /// Test-only backend that records start/stop call counts in shared counters.
    struct TrackingBackend {
        starts: Arc<AtomicU32>,
        stops: Arc<AtomicU32>,
    }

    impl CaptureBackend for TrackingBackend {
        fn native_format(&self) -> (u32, u16) {
            (48_000, 2)
        }
        fn start(&mut self, _sink: RawSink) -> Result<()> {
            self.starts.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn stop(&mut self) {
            self.stops.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Helper that builds and starts a composite, then returns the real sink consumer.
    fn start_composite(
        mic: Box<dyn CaptureBackend>,
        system: Box<dyn CaptureBackend>,
        mic_gain: f32,
        system_gain: f32,
    ) -> (CompositeBackend, RawConsumer) {
        let mut be = CompositeBackend::new(mic, system, mic_gain, system_gain);
        assert_eq!(
            be.native_format(),
            (48_000, 2),
            "should report the internal canonical form"
        );
        let (producer, consumer) = raw_ring(RAW_RING_SAMPLES);
        let sink = RawSink::new(producer, 48_000, 2);
        be.start(sink).expect("composite start");
        (be, consumer)
    }

    /// Tolerance for value comparisons; enough to absorb f32 rounding when adding two
    /// DC values.
    const VALUE_TOL: f32 = 1e-4;

    /// Minimum sample count to consider a value "actually observed": one internal-form
    /// chunk (960 frames × 2 channels). Set the existence floor as an absolute count,
    /// not a fraction of all samples. A fraction (for example, 25%) can fail in a
    /// scheduler-dependent way when a large starvation-fill burst (up to the 48k-sample
    /// FIFO limit) inflates the denominator.
    const ONE_CHUNK_SAMPLES: usize = 1_920;

    /// Helper that collects samples from the consumer until a condition is met.
    ///
    /// `done` receives only the newly popped samples each time (the caller accumulates
    /// counts, etc. to check the condition). Return all collected samples once it returns
    /// true. A fixed wall-clock window ("N samples in 500 ms") can inherently flake if
    /// load deschedules the threads and prevents them from producing enough in that
    /// window. Instead, wait until the condition is met. `max_wait` prevents an
    /// indefinite wait under extreme load; on timeout, return what was collected (the
    /// caller's assertion detects any shortfall).
    fn collect_until(
        consumer: &mut RawConsumer,
        max_wait: Duration,
        mut done: impl FnMut(&[f32]) -> bool,
    ) -> Vec<f32> {
        let mut out = Vec::new();
        let mut scratch = vec![0.0f32; RAW_RING_SAMPLES];
        let start = Instant::now();
        loop {
            let got = consumer.pop_slice(&mut scratch);
            out.extend_from_slice(&scratch[..got]);
            if done(&scratch[..got]) || start.elapsed() >= max_wait {
                return out;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Wait limit for [`collect_until`]. Under normal conditions, it exits as soon as
    /// the condition is met; this only prevents a hang if extreme load barely lets the
    /// threads run.
    const COLLECT_MAX_WAIT: Duration = Duration::from_secs(30);

    /// Helper that counts samples matching `v` within [`VALUE_TOL`].
    fn count_near(samples: &[f32], v: f32) -> usize {
        samples
            .iter()
            .filter(|&&s| (s - v).abs() < VALUE_TOL)
            .count()
    }

    /// Helper that verifies every sample matches one of the theoretically possible
    /// values and returns the count matching the mixed value `mixed`.
    ///
    /// Why check membership in a set instead of a ratio? A ratio assertion such as "98%
    /// of steady-state samples are mixed" can fail under any threshold if load
    /// deschedules producer or mixer threads beyond the 60 ms starvation threshold:
    /// starvation-filled zeros or single-side values can appear in any interval, making
    /// the ratio inherently dependent on wall-clock timing. By contrast, for DC sources
    /// and constant gains, the mixer can produce only four values: mixed (both sides),
    /// mic alone (system starvation fill), system alone (mic starvation fill), or 0.0
    /// (priming boundary). An incorrect sum, misapplied gain, or missing clamp always
    /// produces a value outside this set. The scheduler can change the distribution but
    /// cannot create an out-of-set value, so this check is scheduler-independent.
    fn assert_only_allowed_values(
        samples: &[f32],
        mixed: f32,
        mic_only: f32,
        system_only: f32,
    ) -> usize {
        let allowed = [mixed, mic_only, system_only, 0.0];
        let mut mixed_count = 0usize;
        for (i, &s) in samples.iter().enumerate() {
            if (s - mixed).abs() < VALUE_TOL {
                mixed_count += 1;
            } else {
                assert!(
                    allowed.iter().any(|&a| (s - a).abs() < VALUE_TOL),
                    "value outside allowed set {allowed:?} (mix arithmetic error): samples[{i}] = {s}"
                );
            }
        }
        mixed_count
    }

    /// Mixing two DC sources with known amplitudes (0.2 and 0.3) at mic_gain=1.0 and
    /// system_gain=2.0 yields 0.2×1.0 + 0.3×2.0 = 0.8 (48 kHz/stereo children make
    /// every stage pass-through, so values are deterministic).
    #[test]
    fn mix_sums_two_sources_with_gains() {
        let mic = Box::new(ConstBackend::new(0.2, None));
        let system = Box::new(ConstBackend::new(0.3, None));
        let (mut be, mut consumer) = start_composite(mic, system, 1.0, 2.0);

        // Collect until there is enough data and one chunk of mixed values, waiting
        // through load-related delays.
        let (mut total, mut mixed) = (0usize, 0usize);
        let samples = collect_until(&mut consumer, COLLECT_MAX_WAIT, |new| {
            total += new.len();
            mixed += count_near(new, 0.8);
            total >= 10_000 && mixed >= ONE_CHUNK_SAMPLES
        });
        be.stop();

        assert!(
            samples.len() >= 10_000,
            "should produce enough samples: {}",
            samples.len()
        );
        // Check every sample against the allowed set: mixed value 0.2*1.0 + 0.3*2.0 =
        // 0.8, single-side values 0.2 (mic only) / 0.6 (system only) during starvation
        // fill, and 0.0 at the priming boundary. Even one out-of-set sample means the
        // mix arithmetic is wrong.
        let mixed_count = assert_only_allowed_values(&samples, 0.8, 0.2, 0.6);
        // Also require one chunk's worth to guarantee mixing actually occurred.
        assert!(
            mixed_count >= ONE_CHUNK_SAMPLES,
            "mixed value 0.8 should appear often enough: {mixed_count}/{}",
            samples.len()
        );
    }

    /// A mix that exceeds ±1.0 (0.8 + 0.8 = 1.6) is clamped to 1.0.
    #[test]
    fn mix_clamps_sum() {
        let mic = Box::new(ConstBackend::new(0.8, None));
        let system = Box::new(ConstBackend::new(0.8, None));
        let (mut be, mut consumer) = start_composite(mic, system, 1.0, 1.0);

        // Collect until one chunk of clamped mixed values is available, waiting through
        // load-related delays.
        let mut clamped = 0usize;
        let samples = collect_until(&mut consumer, COLLECT_MAX_WAIT, |new| {
            clamped += count_near(new, 1.0);
            clamped >= ONE_CHUNK_SAMPLES
        });
        be.stop();

        // The key clamp property: no sample exceeds 1.0.
        for (i, &s) in samples.iter().enumerate() {
            assert!(
                s <= 1.0,
                "should not exceed 1.0 after clamping: samples[{i}] = {s}"
            );
        }
        // Check every sample against the allowed set: clamp(0.8 + 0.8) = 1.0, single-
        // side value 0.8 during starvation fill, and 0.0 at the priming boundary. A
        // missed clamp such as 1.6 is immediately rejected as outside the set.
        let clamped_count = assert_only_allowed_values(&samples, 1.0, 0.8, 0.8);
        // Also require one chunk's worth to guarantee the clamped mix actually occurred.
        assert!(
            clamped_count >= ONE_CHUNK_SAMPLES,
            "clamped value 1.0 should appear often enough: {clamped_count}/{}",
            samples.len()
        );
    }

    /// If one side (system) stops supplying data, output continues with silence (0.0)
    /// for the starved side and only mic audio (mic-only value 0.2) begins to flow.
    #[test]
    fn mix_survives_one_side_starvation() {
        let mic = Box::new(ConstBackend::new(0.2, None));
        // System is fed for 150 ms, then stops while its thread remains alive.
        let system = Box::new(ConstBackend::new(0.3, Some(Duration::from_millis(150))));
        let (mut be, mut consumer) = start_composite(mic, system, 1.0, 1.0);

        // After system stops (150 ms wall time), the backlog drains and the starvation
        // threshold passes; mic-only value 0.2 must then start flowing. A wall-clock
        // window check such as "most values are 0.2 after 400 ms" can fail if load slows
        // backlog draining. Instead, wait until one chunk of mic-only values appears.
        let mut mic_only = 0usize;
        let samples = collect_until(&mut consumer, COLLECT_MAX_WAIT, |new| {
            mic_only += count_near(new, 0.2);
            mic_only >= ONE_CHUNK_SAMPLES
        });
        be.stop();

        // Allowed values: mixed 0.5, mic-only 0.2, system-only 0.3, and 0.0 at the
        // priming boundary.
        assert_only_allowed_values(&samples, 0.5, 0.2, 0.3);
        // Also guarantee that output continues after system stops and the mic-only
        // value actually flows with starvation fill.
        let mic_only_count = count_near(&samples, 0.2);
        assert!(
            mic_only_count >= ONE_CHUNK_SAMPLES,
            "mic-only value 0.2 should keep flowing after starvation: {mic_only_count}/{}",
            samples.len()
        );
    }

    /// If the system child's start returns Err, the already-started mic child is stopped
    /// and the composite also returns Err (it must not succeed with only one side).
    #[test]
    fn mix_start_failure_cleans_up() {
        let starts = Arc::new(AtomicU32::new(0));
        let stops = Arc::new(AtomicU32::new(0));
        let mic = Box::new(TrackingBackend {
            starts: starts.clone(),
            stops: stops.clone(),
        });
        let system = Box::new(FailingStartBackend);

        let mut be = CompositeBackend::new(mic, system, 1.0, 1.0);
        let (producer, _consumer) = raw_ring(RAW_RING_SAMPLES);
        let sink = RawSink::new(producer, 48_000, 2);

        let err = be
            .start(sink)
            .expect_err("composite should return Err when system start fails");
        assert!(
            matches!(err, Error::Backend(_)),
            "system child's Err should propagate: {err:?}"
        );
        assert_eq!(starts.load(Ordering::SeqCst), 1, "mic should start once");
        assert_eq!(
            stops.load(Ordering::SeqCst),
            1,
            "mic should stop when system fails"
        );
    }

    /// If the mic child's start returns Err, return Err immediately without touching
    /// the system child. Stop is idempotent and can be called twice.
    #[test]
    fn mix_mic_start_failure_is_immediate() {
        let starts = Arc::new(AtomicU32::new(0));
        let stops = Arc::new(AtomicU32::new(0));
        let mic = Box::new(FailingStartBackend);
        let system = Box::new(TrackingBackend {
            starts: starts.clone(),
            stops: stops.clone(),
        });

        let mut be = CompositeBackend::new(mic, system, 1.0, 1.0);
        let (producer, _consumer) = raw_ring(RAW_RING_SAMPLES);
        let sink = RawSink::new(producer, 48_000, 2);
        assert!(
            be.start(sink).is_err(),
            "mic start failure should return Err immediately"
        );
        assert_eq!(
            starts.load(Ordering::SeqCst),
            0,
            "system should not start if mic fails"
        );

        // Stop is idempotent even if not started (child stop is also idempotent by contract).
        be.stop();
        be.stop();
    }

    /// End-to-end test using the composite backend in a real [`Stream`](crate::Stream).
    /// It verifies that 20 ms/960-frame chunks flow and data has the mixed value (0.2 +
    /// 0.3 = 0.5), confirming the seq and chunk contracts work unchanged without
    /// modifying Stream itself.
    #[test]
    fn stream_delivers_mixed_chunks_end_to_end() {
        use flexaudio_core::types::StreamConfig;

        let mic = Box::new(ConstBackend::new(0.2, None));
        let system = Box::new(ConstBackend::new(0.3, None));
        let backend = Box::new(CompositeBackend::new(mic, system, 1.0, 1.0));
        let mut stream = crate::Stream::open(StreamConfig::default(), backend).expect("open");
        stream.start().expect("start");

        // Poll until one chunk of mixed samples arrives. Fixed wall-clock windows can
        // flake under load, so wait for the condition with a timeout as a hang safeguard.
        let mut chunks = Vec::new();
        let mut mixed = 0usize;
        let deadline = Instant::now() + COLLECT_MAX_WAIT;
        while Instant::now() < deadline && mixed < ONE_CHUNK_SAMPLES {
            while let Some(c) = stream.poll_chunk() {
                mixed += count_near(&c.data, 0.5);
                chunks.push(c);
            }
            thread::sleep(Duration::from_millis(5));
        }
        stream.stop();

        assert!(!chunks.is_empty(), "chunks should arrive");
        for (i, c) in chunks.iter().enumerate() {
            assert_eq!(c.frames, 960, "20ms@48k = 960 frame");
            assert_eq!(c.data.len(), 960 * 2, "stereo interleaved");
            if i > 0 {
                assert_eq!(
                    c.frame_index - chunks[i - 1].frame_index,
                    (c.seq - chunks[i - 1].seq) * 960
                );
                assert!(
                    c.seq > chunks[i - 1].seq,
                    "seq should increase monotonically"
                );
            }
        }
        // Check every sample in every chunk against the allowed set: mixed value 0.2 +
        // 0.3 = 0.5, single-side values 0.2 (mic only) / 0.3 (system only) during
        // starvation fill, and 0.0 at the priming boundary. Stream's first stage is
        // pass-through at 48 kHz/stereo (gain 1.0 leaves bytes unchanged), so mixer
        // output values arrive unchanged.
        let all: Vec<f32> = chunks.iter().flat_map(|c| c.data.iter().copied()).collect();
        let mixed_count = assert_only_allowed_values(&all, 0.5, 0.2, 0.3);
        // Also require one chunk's worth to guarantee that mixing actually occurred.
        assert!(
            mixed_count >= ONE_CHUNK_SAMPLES,
            "mixed value 0.5 should appear often enough: {mixed_count}/{}",
            all.len()
        );
    }

    // ---- Drift correction component tests (pure and deterministic) ----

    /// When system accumulates data (mic - system is negative), r moves above 1.0 to
    /// consume it faster; when mic accumulates data, r moves below 1.0.
    #[test]
    fn drift_controller_moves_toward_lagging_side() {
        let mut c = DriftController::new();
        for _ in 0..50 {
            c.update(0, 9_600);
        }
        assert!(
            c.ratio > 1.0,
            "r should be > 1.0 when system accumulates data: {}",
            c.ratio
        );

        let mut c = DriftController::new();
        for _ in 0..50 {
            c.update(9_600, 0);
        }
        assert!(
            c.ratio < 1.0,
            "r should be < 1.0 when mic accumulates data: {}",
            c.ratio
        );
    }

    /// No matter how large the level difference is, r is capped at 1.0 ± 500 ppm.
    #[test]
    fn drift_controller_clamps_at_ratio_limit() {
        let mut c = DriftController::new();
        for _ in 0..1_000 {
            c.update(0, 10_000_000);
        }
        assert!(
            (c.ratio - (1.0 + DRIFT_RATIO_LIMIT)).abs() < 1e-12,
            "should stop exactly at the upper clamp: {}",
            c.ratio
        );

        let mut c = DriftController::new();
        for _ in 0..1_000 {
            c.update(10_000_000, 0);
        }
        assert!(
            (c.ratio - (1.0 - DRIFT_RATIO_LIMIT)).abs() < 1e-12,
            "should stop exactly at the lower clamp: {}",
            c.ratio
        );
    }

    /// Even with a huge level difference, one update can move only by the slew limit.
    #[test]
    fn drift_controller_slew_limits_change_per_update() {
        let mut c = DriftController::new();
        c.update(0, 10_000_000);
        assert!(
            (c.ratio - (1.0 + DRIFT_SLEW_PER_UPDATE)).abs() < 1e-12,
            "first update should be capped exactly at the slew limit: {}",
            c.ratio
        );
        c.update(0, 10_000_000);
        assert!(
            (c.ratio - (1.0 + 2.0 * DRIFT_SLEW_PER_UPDATE)).abs() < 1e-12,
            "second update should also move by one step: {}",
            c.ratio
        );

        let mut c = DriftController::new();
        c.update(10_000_000, 0);
        assert!(
            (c.ratio - (1.0 - DRIFT_SLEW_PER_UPDATE)).abs() < 1e-12,
            "the reverse direction should also be capped at the slew limit: {}",
            c.ratio
        );
    }

    /// on_output does not update the ratio until 100 ms of mixed output (the update
    /// interval) has accumulated.
    #[test]
    fn drift_controller_updates_only_at_interval() {
        let mut c = DriftController::new();
        c.on_output(DRIFT_UPDATE_INTERVAL_SAMPLES - 1, 0, 10_000_000);
        assert!(
            (c.ratio - 1.0).abs() < 1e-15,
            "should not change before the interval: {}",
            c.ratio
        );
        c.on_output(1, 0, 10_000_000);
        assert!(
            c.ratio > 1.0,
            "should update once the interval is reached: {}",
            c.ratio
        );
    }

    /// At unity speed (r = 1.0, phase 0), the stitcher is a perfect pass-through: output
    /// matches input, the FIFO is fully consumed, and phase remains 0.
    #[test]
    fn stitcher_unity_ratio_is_passthrough() {
        let mut st = LinearStitcher::new();
        let src: Vec<f32> = (0..10).flat_map(|f| [f as f32, -(f as f32)]).collect();
        let mut fifo = src.clone();
        assert_eq!(st.producible(10, 1.0), 10);
        let mut out = Vec::new();
        st.pull(&mut fifo, 1.0, 10, &mut out);
        assert_eq!(out, src, "r=1.0 should be pass-through");
        assert!(
            fifo.is_empty(),
            "should be fully consumed: {} remaining",
            fifo.len()
        );
        assert!(st.frac.abs() < 1e-12, "phase should remain 0: {}", st.frac);
    }

    /// Reading a ramp (frame k has value k) at r = 1.25 yields the linear interpolation
    /// values at positions 0 / 1.25 / 2.5 / 3.75 (both channels, deterministically).
    #[test]
    fn stitcher_interpolates_between_frames() {
        let mut st = LinearStitcher::new();
        let mut fifo: Vec<f32> = (0..5).flat_map(|f| [f as f32, f as f32 * 10.0]).collect();
        // span = 4, floor(4 / 1.25) = 3 → 3 + 1 = 4 frames can be produced.
        assert_eq!(st.producible(5, 1.25), 4);
        let mut out = Vec::new();
        st.pull(&mut fifo, 1.25, 4, &mut out);
        let expect = [0.0f32, 1.25, 2.5, 3.75];
        for (k, &e) in expect.iter().enumerate() {
            assert!(
                (out[k * 2] - e).abs() < 1e-6,
                "interpolated value at position {e}: {}",
                out[k * 2]
            );
            assert!(
                (out[k * 2 + 1] - e * 10.0).abs() < 1e-5,
                "ch2 should interpolate at the same position: {}",
                out[k * 2 + 1]
            );
        }
        // floor(0 + 4×1.25) = 5, so all frames are consumed and phase returns to 0.
        assert!(
            fifo.is_empty(),
            "should be fully consumed: {} remaining",
            fifo.len()
        );
        assert!(st.frac.abs() < 1e-12, "phase: {}", st.frac);
    }

    // ---- Synchronous drift simulation without threads (deterministic) ----

    /// ChildLane for synchronous simulation without threads. The ring and normalizer
    /// exist only to satisfy the shape; the test supplies data directly to the FIFO via
    /// [`sim_feed`].
    fn sim_lane() -> ChildLane {
        let (_producer, consumer) = raw_ring(16);
        let normalizer = Normalizer::new(
            SAMPLE_RATE,
            CHANNELS,
            OutputFormat {
                sample_rate: SAMPLE_RATE,
                channels: CHANNELS,
            },
        )
        .expect("pass-through normalizer");
        ChildLane {
            consumer,
            normalizer,
            fifo: Vec::new(),
            last_supply: Instant::now(),
        }
    }

    /// Directly supply DC frames using the same approach as ingest (append to FIFO and
    /// enforce its safety limit).
    fn sim_feed(lane: &mut ChildLane, value: f32, frames: usize) {
        let new_len = lane.fifo.len() + frames * CHANNELS as usize;
        lane.fifo.resize(new_len, value);
        if lane.fifo.len() > FIFO_MAX_SAMPLES {
            let excess = lane.fifo.len() - FIFO_MAX_SAMPLES;
            lane.fifo.drain(..excess);
        }
        lane.last_supply = Instant::now();
    }

    /// Measurements from the synchronous drift simulation.
    struct DriftSimOutcome {
        /// Post-consumption FIFO levels (f32 samples) for each simulated second.
        system_backlog: Vec<usize>,
        mic_backlog: Vec<usize>,
        final_ratio: f64,
    }

    /// Synchronous simulation without threads. Each tick is 20 ms. Supply DC to mic at
    /// unity rate (960 frames/tick) and to system at (1 + ppm×1e-6) rate, simulating
    /// elapsed time through sample counts, then drive mix_and_push directly in a loop.
    /// It is fully deterministic and independent of threads and wall-clock time.
    ///
    /// If `fixed_unity_ratio` is set, reset the controller to 1.0 every tick, simulating
    /// "no correction." This self-check confirms the simulation reproduces the drift
    /// problem (monotonically growing FIFO levels).
    ///
    /// Check every output value on every tick: both sides are always supplying data, so
    /// only the mixed value (0.2 + 0.3 = 0.5; linear interpolation preserves DC) is
    /// allowed. Fail if even one starvation-filled 0.0 or single-side value appears;
    /// this also verifies that zero-fill does not occur in steady state.
    fn run_drift_sim(ppm: f64, seconds: usize, fixed_unity_ratio: bool) -> DriftSimOutcome {
        const TICK_FRAMES: usize = 960; // 20ms @48k
        const TICKS_PER_SEC: usize = 50;

        let mut mic = sim_lane();
        let mut system = sim_lane();
        let mut drift = DriftCorrection::new();
        let (producer, mut consumer) = raw_ring(RAW_RING_SAMPLES);
        let mut sink = RawSink::new(producer, SAMPLE_RATE, CHANNELS);
        let mut mixed: Vec<f32> = Vec::with_capacity(FIFO_MAX_SAMPLES);
        let mut scratch = vec![0.0f32; RAW_RING_SAMPLES];

        // Carry the fractional system supply frame count forward to simulate the rate
        // difference accurately in samples.
        let mut system_carry = 0.0f64;
        let mut outcome = DriftSimOutcome {
            system_backlog: Vec::new(),
            mic_backlog: Vec::new(),
            final_ratio: 1.0,
        };

        for tick in 0..seconds * TICKS_PER_SEC {
            sim_feed(&mut mic, 0.2, TICK_FRAMES);
            system_carry += TICK_FRAMES as f64 * (1.0 + ppm * 1e-6);
            let system_frames = system_carry as usize;
            system_carry -= system_frames as f64;
            sim_feed(&mut system, 0.3, system_frames);

            mix_and_push(
                &mut mic,
                &mut system,
                1.0,
                1.0,
                &mut drift,
                &mut sink,
                &mut mixed,
            );
            if fixed_unity_ratio {
                drift.controller.ratio = 1.0;
                drift.controller.ema_diff = 0.0;
            }

            let got = consumer.pop_slice(&mut scratch);
            for &s in &scratch[..got] {
                assert!(
                    (s - 0.5).abs() < VALUE_TOL,
                    "steady-state simulation should output only the mixed value 0.5 (no zero-fill or \
                     single-side values): {s}"
                );
            }

            if (tick + 1) % TICKS_PER_SEC == 0 {
                outcome.system_backlog.push(system.fifo.len());
                outcome.mic_backlog.push(mic.fifo.len());
            }
        }
        outcome.final_ratio = drift.controller.ratio;
        outcome
    }

    /// Simulate +300 ppm (system is faster) for 60 seconds: with correction, the system
    /// FIFO stays below the safety limit, grows more slowly, and remains smaller than
    /// without correction. With correction disabled (r fixed at 1.0), levels grow
    /// monotonically under the same conditions, confirming that the test reproduces the
    /// drift problem.
    #[test]
    fn mix_drift_sim_plus_300ppm_stays_bounded() {
        let corrected = run_drift_sim(300.0, 60, false);
        let uncorrected = run_drift_sim(300.0, 60, true);

        // Self-check: without correction, system levels grow monotonically each second
        // (+300 ppm ≈ +28.8 samples/second).
        for (i, w) in uncorrected.system_backlog.windows(2).enumerate() {
            assert!(
                w[1] > w[0],
                "levels should grow monotonically without correction: second {i}, {} → {}",
                w[0],
                w[1]
            );
        }

        // With correction, levels do not reach the safety limit (FIFO_MAX_SAMPLES = 500 ms).
        let max_corrected = corrected.system_backlog.iter().copied().max().unwrap();
        assert!(
            max_corrected < FIFO_MAX_SAMPLES,
            "with correction, should stay below the safety limit: max {max_corrected}"
        );

        // With correction, levels stay lower than without it, confirming that correction works.
        let last_c = *corrected.system_backlog.last().unwrap();
        let last_u = *uncorrected.system_backlog.last().unwrap();
        assert!(
            last_c < last_u,
            "corrected level {last_c} should be < uncorrected level {last_u}"
        );

        // With correction, growth slows (the increase over the first 9 seconds is > the
        // increase over the last 9 seconds).
        let sb = &corrected.system_backlog;
        let early = sb[9] - sb[0];
        let late = sb[59] - sb[50];
        assert!(
            late < early,
            "growth should slow as the ratio catches up: early +{early} vs late +{late}"
        );

        // The ratio moves toward faster system consumption and stays within the clamp.
        assert!(
            corrected.final_ratio > 1.0 + 2e-5,
            "r should move above 1.0: {}",
            corrected.final_ratio
        );
        assert!(
            corrected.final_ratio <= 1.0 + DRIFT_RATIO_LIMIT + 1e-12,
            "r should stay within the clamp: {}",
            corrected.final_ratio
        );
    }

    /// Simulate -300 ppm (system is slower) for 60 seconds: starvation zero-fill does
    /// not occur in steady state (run_drift_sim checks every output value on every
    /// tick). With correction, the mic-side level stays below the safety limit and
    /// lower than without correction.
    #[test]
    fn mix_drift_sim_minus_300ppm_no_steady_zero_fill() {
        let corrected = run_drift_sim(-300.0, 60, false);
        let uncorrected = run_drift_sim(-300.0, 60, true);

        // Self-check: without correction, mic levels grow monotonically to keep up with
        // the slower system.
        for (i, w) in uncorrected.mic_backlog.windows(2).enumerate() {
            assert!(
                w[1] > w[0],
                "mic levels should grow monotonically without correction: second {i}, {} → {}",
                w[0],
                w[1]
            );
        }

        // With correction, mic levels stay below the safety limit and lower than without it.
        let max_corrected = corrected.mic_backlog.iter().copied().max().unwrap();
        assert!(
            max_corrected < FIFO_MAX_SAMPLES,
            "with correction, should stay below the safety limit: max {max_corrected}"
        );
        let last_c = *corrected.mic_backlog.last().unwrap();
        let last_u = *uncorrected.mic_backlog.last().unwrap();
        assert!(
            last_c < last_u,
            "corrected mic level {last_c} should be < uncorrected level {last_u}"
        );

        // System does not accumulate because consumption tracks supply (at most an
        // interpolation fraction plus a recent partial chunk).
        let max_sys = corrected.system_backlog.iter().copied().max().unwrap();
        assert!(
            max_sys < ONE_CHUNK_SAMPLES,
            "system should not accumulate data: max {max_sys}"
        );

        // The ratio moves toward slower system consumption and stays within the clamp.
        assert!(
            corrected.final_ratio < 1.0 - 2e-5,
            "r should move below 1.0: {}",
            corrected.final_ratio
        );
        assert!(
            corrected.final_ratio >= 1.0 - DRIFT_RATIO_LIMIT - 1e-12,
            "r should stay within the clamp: {}",
            corrected.final_ratio
        );
    }
}

#[cfg(test)]
mod repro_tests;
