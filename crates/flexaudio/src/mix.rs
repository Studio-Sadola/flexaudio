//! Composite backend [`CompositeBackend`] that mixes mic + system into one stream.
//!
//! It holds two child backends, mic and system, internally; it brings each child's audio to the
//! internal canonical format (48kHz/stereo), then sums them with per-side gains, so that to
//! [`Stream`](crate::Stream) it looks like just one backend. The Stream itself is not touched,
//! so seq/PTS, the watchdog, pause, the global gain and switch_source all keep working as is.
//!
//! # Thread layout
//! - Each child backend's RT thread: only pushes into its own dedicated child RawRing (the
//!   existing backend as is, untouched).
//! - Mix thread (one, `flexaudio-mix`): pops from the child rings → converts to 48k/stereo with a
//!   per-child [`Normalizer`] → sums the aligned frames of both sides with per-side gains (±1.0
//!   clamp) → pushes to the real sink. mic and system run on separate crystals, so their rates
//!   drift apart by several to several hundred ppm; this is absorbed by drift correction that
//!   slightly resamples only the system side ([`LinearStitcher`] + [`DriftController`]). It is
//!   not RT, so heap allocation is allowed (but steady-state allocation inside the loop is
//!   avoided by reusing scratch buffers).

use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use flexaudio_core::backend::{CaptureBackend, RawSink};
use flexaudio_core::clock::monotonic_now_ns;
use flexaudio_core::normalizer::Normalizer;
use flexaudio_core::raw_ring::{raw_ring, RawConsumer};
use flexaudio_core::types::{Error, OutputFormat, Result, CHANNELS, SAMPLE_RATE};

use crate::stream::RAW_RING_SAMPLES;

/// When one side has supplied nothing for longer than this, mixing continues with the missing
/// part treated as silence (0.0). Rationale: normalized output only arrives in 20ms chunks, so
/// arrival jitter of up to 2-3 chunks is considered normal, and an outage beyond that (e.g. a
/// period when nothing is playing on the system side) must not stop the recording as a whole.
const STARVATION_FILL_THRESHOLD: Duration = Duration::from_millis(60);

/// Upper bound (in f32 samples) of one side's normalized FIFO. A safety valve that drops the
/// oldest samples once roughly 500ms worth (48kHz × 2ch × 0.5s = 48_000) is exceeded.
/// The rate difference between the child clocks is absorbed by drift correction
/// ([`DriftController`]), so as long as the correction works this is normally never reached. It
/// is kept as the last line of defense against anomalies the correction range (±500ppm) cannot
/// keep up with (such as runaway supply on one side).
const FIFO_MAX_SAMPLES: usize = 48_000;

/// Wait time when neither side has material to mix (same approach as the ingest thread in
/// stream.rs).
const IDLE_SLEEP: Duration = Duration::from_millis(2);

/// Range within which drift correction may move the system-side read ratio r (1.0 ± 500ppm).
/// Real crystal drift in consumer hardware is usually tens to a little over a hundred ppm, so
/// this covers it comfortably, and while r stays in this range the distortion from linear
/// interpolation stays negligible (the interpolation point always stays very close to an
/// original sample).
const DRIFT_RATIO_LIMIT: f64 = 500e-6;

/// Interval (in mixed-output f32 samples) at which the ratio is revisited. Once per 100ms of
/// mixed output. That is 5 normalized chunks (20ms), coarser than the arrival granularity and
/// much finer than the time scale of drift (minutes).
const DRIFT_UPDATE_INTERVAL_SAMPLES: usize = (SAMPLE_RATE as usize / 10) * CHANNELS as usize;

/// EMA coefficient for the backlog difference. Together with the 100ms update interval the time
/// constant is about 1 second. It smooths out chunk-arrival jitter (sawtooth backlog variation at
/// 20ms granularity) while still following changes in drift closely enough.
const DRIFT_EMA_ALPHA: f64 = 0.1;

/// P-control gain. The slope at which a backlog difference of 200ms (19_200 samples) uses up the
/// full 500ppm clamp limit. With this gentleness, the steady-state backlog difference for real
/// drift (tens to hundreds of ppm) settles at tens to a little over a hundred ms (= drift ÷
/// gain), far below the 500ms safety valve.
const DRIFT_GAIN: f64 = DRIFT_RATIO_LIMIT / 19_200.0;

/// Upper bound on how far one revision may move the ratio (slew). 20ppm per 100ms = even going
/// from one end of the clamp range to the other takes 5 seconds. Prevents the ratio from jumping
/// on measurement noise in the backlog difference and making the pitch wobble.
const DRIFT_SLEW_PER_UPDATE: f64 = 20e-6;

/// Composite backend that sums the two child backends, mic + system, in the internal canonical
/// format.
///
/// [`native_format`](CaptureBackend::native_format) always returns the internal canonical format
/// `(48000, 2)`, so the Stream's first-stage resampler is effectively a passthrough. The children
/// are injected through the constructor (tests can pass mocks). Building the real children is
/// the job of the facade's `build_backend`.
pub(crate) struct CompositeBackend {
    mic: Box<dyn CaptureBackend>,
    system: Box<dyn CaptureBackend>,
    mic_gain: f32,
    system_gain: f32,
    /// Stop request to the mix thread. Replaced with a new Arc on every start
    /// (so it never gets crossed with leftovers of an old thread).
    stopping: Arc<AtomicBool>,
    /// Handle of the mix thread. `Some` means running.
    mixer: Option<JoinHandle<()>>,
}

impl CompositeBackend {
    /// Builds it by injecting the two children and the per-side gains. The gains must already
    /// have been validated (finite, >= 0) by the caller (the facade's `build_backend`).
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
        }
    }
}

impl CaptureBackend for CompositeBackend {
    fn native_format(&self) -> (u32, u16) {
        // Mixing is always done in the internal canonical format. The Stream's first stage is
        // effectively a passthrough.
        (SAMPLE_RATE, CHANNELS)
    }

    fn start(&mut self, sink: RawSink) -> Result<()> {
        // A double start while running is a no-op (CaptureBackend contract).
        if self.mixer.is_some() {
            return Ok(());
        }

        // If mic fails to start, return Err immediately; if system fails to start, stop mic and
        // then return Err (never report success with only one side running).
        let mic_lane = start_child(&mut self.mic)?;
        let system_lane = match start_child(&mut self.system) {
            Ok(lane) => lane,
            Err(e) => {
                stop_child(&mut self.mic);
                return Err(e);
            }
        };

        // Start the mix thread. The stop flag is renewed on every start (it must not carry over
        // the flag from the previous stop).
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
                // If the thread cannot be spawned, stop the children and return the failure
                // (never leave only one side running).
                stop_child(&mut self.mic);
                stop_child(&mut self.system);
                Err(e)
            }
        }
    }

    fn stop(&mut self) {
        // Stop flag → join the mix thread → stop both children. Idempotent (if never started,
        // only the children's stop runs, which is harmless because the children are under the
        // idempotency contract too).
        self.stopping.store(true, Ordering::SeqCst);
        if let Some(h) = self.mixer.take() {
            let _ = h.join();
        }
        stop_child(&mut self.mic);
        stop_child(&mut self.system);
    }
}

impl Drop for CompositeBackend {
    fn drop(&mut self) {
        // Even when dropped without stop, do not leave the mix thread and children behind.
        self.stop();
    }
}

/// Full ingest state of one side's child (child ring consumer + normalizer + normalized FIFO).
struct ChildLane {
    consumer: RawConsumer,
    /// Child native → internal canonical format (48k/stereo). The output is fixed to the
    /// internal canonical format, so the second stage is a passthrough.
    normalizer: Normalizer,
    /// FIFO of normalized samples (48k/stereo interleaved).
    fifo: Vec<f32>,
    /// Time at which this side last supplied normalized samples (for starvation detection).
    last_supply: Instant,
}

impl ChildLane {
    /// Pops from the child ring, normalizes, and appends the completed part to the FIFO.
    ///
    /// When the FIFO exceeds [`FIFO_MAX_SAMPLES`], the oldest samples are dropped (safety valve
    /// against unbounded growth). A rubato processing failure is returned as `Err` (the caller
    /// ends the mix thread).
    fn ingest(&mut self, scratch: &mut [f32]) -> Result<()> {
        let got = self.consumer.pop_slice(scratch);
        if got == 0 {
            return Ok(());
        }
        // pts is not used on the sink side (by contract the wiring layer handles it
        // separately), but a monotonic now is passed as the normalizer's anchor.
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

    /// Whether this side has supplied nothing for [`STARVATION_FILL_THRESHOLD`] or longer.
    fn is_starved(&self, now: Instant) -> bool {
        now.duration_since(self.last_supply) >= STARVATION_FILL_THRESHOLD
    }
}

/// Fine resampler for the system lane (linear-interpolation stitcher).
///
/// mic is taken as the reference clock (it is natural to align the recording's time axis to
/// the human voice = the mic side), and only the system FIFO is read out at r times the speed
/// with linear interpolation, absorbing the rate difference between the child clocks. Only the
/// fractional part of the read position is kept as state.
struct LinearStitcher {
    /// Fractional part of the read position, relative to the first FIFO frame (in frames,
    /// [0, 1)).
    frac: f64,
}

impl LinearStitcher {
    fn new() -> Self {
        Self { frac: 0.0 }
    }

    /// Number of output frames that can be produced by interpolation at ratio `ratio` when the
    /// FIFO holds `fifo_frames` frames. The k-th output (0-based) is built from the two frames
    /// around position `frac + k×ratio`, so only k whose position does not go past the last
    /// frame F-1 can be produced (one landing exactly on the last frame can, since its weight
    /// is 0).
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

    /// Reads `out_frames` frames from the system FIFO with linear interpolation at `ratio` times
    /// the speed and appends them, still interleaved, to `out`. Frames that have been fully read
    /// are dropped from the FIFO and the fractional position is carried over to the next call
    /// (the phase stays continuous across block boundaries).
    /// The caller must guarantee `out_frames <= producible(...)`.
    fn pull(&mut self, fifo: &mut Vec<f32>, ratio: f64, out_frames: usize, out: &mut Vec<f32>) {
        let ch = CHANNELS as usize;
        let frames = fifo.len() / ch;
        debug_assert!(out_frames <= self.producible(frames, ratio));
        for k in 0..out_frames {
            let pos = self.frac + k as f64 * ratio;
            let left = pos as usize;
            // Clamp the right edge only when the position lands exactly on the last frame
            // (the weight is 0 then, so the interpolated value does not change).
            let right = (left + 1).min(frames - 1);
            let w = (pos - left as f64) as f32;
            for c in 0..ch {
                let a = fifo[left * ch + c];
                let b = fifo[right * ch + c];
                out.push(a + w * (b - a));
            }
        }
        // Next read position. The whole frames have been fully read, so drop them and keep
        // only the fractional part.
        let end = self.frac + out_frames as f64 * ratio;
        let consumed = (end as usize).min(frames);
        self.frac = end - consumed as f64;
        fifo.drain(..consumed * ch);
    }

    /// Resets the phase when the starvation path has flushed the whole system FIFO.
    fn reset(&mut self) {
        self.frac = 0.0;
    }
}

/// Feedback control that sets the read ratio r from the backlog difference (EMA + P control +
/// slew).
///
/// Each time mixing advances by [`DRIFT_UPDATE_INTERVAL_SAMPLES`], it takes the exponential
/// moving average of the post-consumption FIFO backlog difference (mic - system); if the
/// difference is negative (the system side is accumulating) it raises r to consume faster, and
/// if positive it lowers r. P control suffices because the backlog difference itself is the
/// integral of the rate difference (pushing back proportionally balances the backlog difference
/// at a finite value).
struct DriftController {
    /// Current read ratio r (clamped to 1.0 ± [`DRIFT_RATIO_LIMIT`]).
    ratio: f64,
    /// Exponential moving average of the backlog difference (mic_len - system_len) (in f32
    /// samples).
    ema_diff: f64,
    /// Mixed-output samples since the last revision.
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

    /// Records how far the mixed output has advanced and revisits the ratio once when it
    /// reaches [`DRIFT_UPDATE_INTERVAL_SAMPLES`]. Pass the backlogs after consumption (before
    /// consumption, the part consumed this time would also show up as a difference).
    fn on_output(&mut self, samples: usize, mic_len: usize, system_len: usize) {
        self.pending_samples += samples;
        if self.pending_samples >= DRIFT_UPDATE_INTERVAL_SAMPLES {
            self.pending_samples = 0;
            self.update(mic_len, system_len);
        }
    }

    /// Updates the EMA of the backlog difference and moves the ratio one step with P control +
    /// slew.
    fn update(&mut self, mic_len: usize, system_len: usize) {
        let diff = mic_len as f64 - system_len as f64;
        self.ema_diff += DRIFT_EMA_ALPHA * (diff - self.ema_diff);
        let target = (1.0 - DRIFT_GAIN * self.ema_diff)
            .clamp(1.0 - DRIFT_RATIO_LIMIT, 1.0 + DRIFT_RATIO_LIMIT);
        let step = (target - self.ratio).clamp(-DRIFT_SLEW_PER_UPDATE, DRIFT_SLEW_PER_UPDATE);
        self.ratio += step;
    }
}

/// Full drift correction state (stitcher + ratio controller). One per mix thread.
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

/// Starts one child: creates a dedicated child RawRing (same capacity as in stream.rs) and calls
/// `start` with a [`RawSink`] in the child's native format. Returns a [`ChildLane`] on success.
///
/// A panic in the child's `start` is converted to [`Error::Backend`] with catch_unwind
/// (same intent as start_backend_catching in stream.rs: do not cascade the panic into the mix
/// thread or the caller).
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
    // Child native → internal canonical format (48k/stereo). This Normalizer's output is fixed
    // to the internal canonical format, so the second stage is always a passthrough.
    let normalizer = Normalizer::new(
        rate,
        channels,
        OutputFormat {
            sample_rate: SAMPLE_RATE,
            channels: CHANNELS,
        },
    )
    .inspect_err(|_| {
        // If the normalizer cannot be created, stop the child before returning the failure (do
        // not leave a started child behind).
        stop_child(child);
    })?;
    Ok(ChildLane {
        consumer,
        normalizer,
        fifo: Vec::with_capacity(FIFO_MAX_SAMPLES),
        last_supply: Instant::now(),
    })
}

/// Calls the child's `stop` wrapped in catch_unwind (does not propagate a panic; same intent as
/// stop_backend_catching in stream.rs).
fn stop_child(child: &mut Box<dyn CaptureBackend>) {
    let _ = std::panic::catch_unwind(AssertUnwindSafe(|| child.stop()));
}

/// Body of the mix thread.
///
/// It first waits in [`prime_lanes`] until the first supply from both sides has arrived (bounded
/// by the starvation threshold), then ingests each child (pop → normalize → FIFO), sums the
/// aligned frames of both sides with per-side gains and pushes them to the real sink. If one
/// side has supplied nothing for [`STARVATION_FILL_THRESHOLD`] or longer, it continues with the
/// missing part as silence (the recording keeps flowing even while the system side is silent).
/// If neither side has material, it sleeps for [`IDLE_SLEEP`].
///
/// A normalization failure (a theoretical rubato failure) ends the loop. Samples stop flowing
/// after that, so the Stream's watchdog detects the stall and reopens the backend.
fn run_mixer(
    mut mic: ChildLane,
    mut system: ChildLane,
    mic_gain: f32,
    system_gain: f32,
    mut sink: RawSink,
    stopping: Arc<AtomicBool>,
) {
    // Scratch for pop (child ring capacity) and scratch for mixed output. Reused inside the loop.
    let mut scratch = vec![0.0f32; RAW_RING_SAMPLES];
    let mut mixed: Vec<f32> = Vec::with_capacity(FIFO_MAX_SAMPLES);
    // Drift correction state between the child clocks (renewed on every start = the ratio of
    // the previous recording does not carry over).
    let mut drift = DriftCorrection::new();

    // Right after startup the child threads come up at uneven times, so wait until both sides
    // start flowing (bounded by the starvation threshold) before mixing = prevents the head of
    // the recording from containing only one side.
    if !prime_lanes(&mut mic, &mut system, &mut scratch, &stopping) {
        return;
    }

    loop {
        if stopping.load(Ordering::SeqCst) {
            break;
        }

        if mic.ingest(&mut scratch).is_err() || system.ingest(&mut scratch).is_err() {
            // If normalization breaks, end mixing (leave it to the watchdog's reopen).
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

/// Priming before mixing starts. Right after startup the child backends' threads come up at
/// uneven times, so it polls every [`IDLE_SLEEP`] until the first normalized samples arrive in
/// both sides' FIFOs before mixing starts. If this is skipped, starvation fill can kick in while
/// the late side's ring is still empty, and the head of the recording may contain only one side.
///
/// The wait is bounded by [`STARVATION_FILL_THRESHOLD`]. The legitimate case where one side
/// supplies nothing from the start (e.g. nothing is playing on the system side) is cut off with
/// the same time sense as the existing starvation handling, and mixing starts. A stop request
/// aborts the wait. A normalization failure returns `false`, and the caller ends the mix thread.
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

/// Takes what can be mixed from both sides' FIFOs, sums it with per-side gains (±1.0 clamp) and
/// pushes it to the sink. Returns `true` if anything was pushed.
///
/// How the amount taken is decided:
/// - Both sides have data (steady path) → the min of the mic stock and the amount that can be
///   produced from system by interpolation. mic is read at unit speed; system is read by
///   [`LinearStitcher`] with linear interpolation at r times the speed, absorbing the drift
///   between the child clocks. r is fine-tuned by [`DriftController`] from the
///   post-consumption backlog difference.
/// - Only one side has data and the other is starved (nothing supplied for 60ms or more) → mix
///   the whole amount of the side that has data against silence (the missing part filled with
///   0.0). No correction is applied on this path (existing starvation semantics as is).
/// - Otherwise (both sides empty, or the other side not starved yet) → do nothing (wait for
///   them to line up).
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
        // Steady path: read only the system side with linear interpolation at r times the
        // speed, then sum.
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
        // pts can be a monotonic now, as in the ingest of stream.rs (by contract the sink side
        // handles it separately).
        sink.push(mixed, monotonic_now_ns());
        return true;
    }

    // Starvation path (no correction, existing semantics).
    let now = Instant::now();
    let (mic_take, system_take) = if !mic.fifo.is_empty() && system.is_starved(now) {
        // system outage: emit the whole mic amount. The fractional remainder left without its
        // right interpolation edge (at most 1 frame) is also flushed here, and the phase reset.
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

    // pts can be a monotonic now, as in the ingest of stream.rs (by contract the sink side
    // handles it separately).
    sink.push(mixed, monotonic_now_ns());
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use flexaudio_core::raw_ring::raw_ring;
    use std::sync::atomic::AtomicU32;

    /// Test-only child backend that supplies constant-amplitude (DC) samples at saturation.
    ///
    /// With a sine wave (MockBackend), mixing two sources raises phase issues, so a DC signal is
    /// used, which lets the mix result be verified deterministically. If `feed_for` is set, it
    /// feeds only for that long and then stops pushing (the thread stays alive) = for
    /// reproducing one-side starvation.
    ///
    /// Supply is saturating rather than real-time paced (10ms sleep): it keeps pushing
    /// immediately as long as the child ring accepts, and when the ring is full and the block
    /// does not fit it yields for just 1ms and retries. With real-time pacing, supply lags on
    /// slow test machines with coarse sleep granularity, and when the mixer wakes only one
    /// side's FIFO is empty → starvation fill mixes in chunks with only one side, making the
    /// verification of mixed values scheduler-dependent. With saturating supply both lanes'
    /// FIFOs are non-empty whenever the mixer wakes, narrowing what is verified to "the math of
    /// mixing" (scheduling tolerance itself is covered by mix_survives_one_side_starvation).
    /// Since it is DC, drops when full or partial writes do not affect the values.
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
                    let frames_per_block = (sample_rate as usize / 100).max(1); // 10ms worth
                    let block = vec![value; frames_per_block * channels];
                    let start = Instant::now();
                    while running.load(Ordering::SeqCst) {
                        let feeding = feed_for.is_none_or(|d| start.elapsed() < d);
                        if !feeding {
                            // After the feeding period ends, supply nothing more
                            // (reproduces one-side starvation). Only watch for the stop
                            // request and sleep.
                            thread::sleep(Duration::from_millis(5));
                            continue;
                        }
                        // Saturating supply: once everything fits, push the next block
                        // immediately; if the ring is full and it does not fit, yield for
                        // 1ms and retry (by contract push is non-blocking and drops what
                        // does not fit. It is DC, so gaps are harmless).
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

    /// Test-only backend that records the number of start / stop calls in shared counters.
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

    /// Helper that builds and starts a composite and returns the consumer of the real sink.
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
            "should report the internal canonical format"
        );
        let (producer, consumer) = raw_ring(RAW_RING_SAMPLES);
        let sink = RawSink::new(producer, 48_000, 2);
        be.start(sink).expect("composite start");
        (be, consumer)
    }

    /// Tolerance for value comparison. Enough to absorb the f32 rounding of adding two DC values.
    const VALUE_TOL: f32 = 1e-4;

    /// Minimum number of samples accepted as "actually appeared" = one chunk of the internal
    /// canonical format (960 frame × 2ch). The existence floor is an absolute count rather than a
    /// fraction of the total: with a fraction (e.g. 25%), when a large one-side starvation-fill
    /// burst gets mixed in (up to the FIFO limit of 48k samples can come out at once) only the
    /// denominator grows and the check breaks = it ends up scheduler-dependent after all.
    const ONE_CHUNK_SAMPLES: usize = 1_920;

    /// Helper that collects samples from the consumer until a condition is met.
    ///
    /// `done` receives only the newly popped samples each time (the caller accumulates counts
    /// etc. and decides). When it returns true, everything collected is returned. A fixed
    /// wall-clock window ("N samples within 500ms") cannot guarantee production within the
    /// window when threads get descheduled under load and is inherently flaky, so it "waits
    /// until the condition is reached" instead. `max_wait` is a hang guard so that it does not
    /// run forever even under extreme load; when exceeded it returns what has been collected
    /// (any shortfall is detected by the caller's assertions).
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

    /// Wait limit for [`collect_until`]. In a normal environment it exits as soon as the
    /// condition is reached, so this is only a hang guard for the case where "threads can
    /// barely run under extreme load".
    const COLLECT_MAX_WAIT: Duration = Duration::from_secs(30);

    /// Helper that counts the samples matching `v` (within [`VALUE_TOL`]).
    fn count_near(samples: &[f32], v: f32) -> usize {
        samples
            .iter()
            .filter(|&&s| (s - v).abs() < VALUE_TOL)
            .count()
    }

    /// Helper that verifies every sample matches one of "the set of values that can
    /// theoretically appear", and returns how many matched the mixed value `mixed`.
    ///
    /// Why a set check rather than a ratio: a ratio assertion such as "98% of the steady part is
    /// the mixed value" breaks eventually wherever the threshold is placed, because when the
    /// producer / mixer threads get descheduled beyond the starvation threshold (60ms) under
    /// load, starvation zero fill or one-side values can get mixed into any interval (a ratio is
    /// inherently wall-clock dependent). On the other hand, for DC sources + constant gains the
    /// only values the mixer can output are these 4: mixed (both sides mixed) / mic alone
    /// (system starvation fill) / system alone (mic starvation fill) / 0.0 (priming boundary);
    /// a wrong sum, a misapplied gain or a missed clamp always yields a value outside this set.
    /// The scheduler can shift the distribution of values but cannot produce a value outside
    /// the set, so this check is scheduler-independent.
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
                    "value outside the allowed set {allowed:?} (error in the mixing math): \
                     samples[{i}] = {s}"
                );
            }
        }
        mixed_count
    }

    /// Mixing two DC sources of known amplitude (0.2 and 0.3) with mic_gain=1.0 /
    /// system_gain=2.0 yields 0.2×1.0 + 0.3×2.0 = 0.8 (the children are 48k/stereo, so every
    /// stage is a passthrough and the value is deterministic).
    #[test]
    fn mix_sums_two_sources_with_gains() {
        let mic = Box::new(ConstBackend::new(0.2, None));
        let system = Box::new(ConstBackend::new(0.3, None));
        let (mut be, mut consumer) = start_composite(mic, system, 1.0, 2.0);

        // Collect until a reasonable amount + one chunk of mixed values has come out (wait even
        // if slow under load).
        let (mut total, mut mixed) = (0usize, 0usize);
        let samples = collect_until(&mut consumer, COLLECT_MAX_WAIT, |new| {
            total += new.len();
            mixed += count_near(new, 0.8);
            total >= 10_000 && mixed >= ONE_CHUNK_SAMPLES
        });
        be.stop();

        assert!(
            samples.len() >= 10_000,
            "a reasonable number of samples should come out: {}",
            samples.len()
        );
        // Set check over the whole range and every sample: the only values allowed are the mixed
        // value 0.2*1.0 + 0.3*2.0 = 0.8, the one-side values under starvation zero fill 0.2 (mic
        // alone) / 0.6 (system alone), and 0.0 at the priming boundary. Even one sample outside
        // the set means the mixing math is broken.
        let mixed_count = assert_only_allowed_values(&samples, 0.8, 0.2, 0.6);
        // Existence guarantee: mixing actually happens (absolute count of one chunk).
        assert!(
            mixed_count >= ONE_CHUNK_SAMPLES,
            "the mixed value 0.8 should appear in reasonable numbers: {mixed_count}/{}",
            samples.len()
        );
    }

    /// A combination whose sum exceeds ±1.0 (0.8 + 0.8 = 1.6) is clamped to 1.0.
    #[test]
    fn mix_clamps_sum() {
        let mic = Box::new(ConstBackend::new(0.8, None));
        let system = Box::new(ConstBackend::new(0.8, None));
        let (mut be, mut consumer) = start_composite(mic, system, 1.0, 1.0);

        // Collect until one chunk of clamped mixed values has come out (wait even if slow
        // under load).
        let mut clamped = 0usize;
        let samples = collect_until(&mut consumer, COLLECT_MAX_WAIT, |new| {
            clamped += count_near(new, 1.0);
            clamped >= ONE_CHUNK_SAMPLES
        });
        be.stop();

        // The essence of clamping: no sample exceeds 1.0.
        for (i, &s) in samples.iter().enumerate() {
            assert!(
                s <= 1.0,
                "should not exceed 1.0 after clamping: samples[{i}] = {s}"
            );
        }
        // Set check over the whole range and every sample: the only values allowed are
        // clamp(0.8 + 0.8) = 1.0, the one-side value 0.8 under starvation zero fill, and 0.0 at
        // the priming boundary (a missed clamp such as 1.6 is outside the set and FAILs
        // immediately).
        let clamped_count = assert_only_allowed_values(&samples, 1.0, 0.8, 0.8);
        // Existence guarantee: clamped mixed values actually appear (one chunk).
        assert!(
            clamped_count >= ONE_CHUNK_SAMPLES,
            "the clamped value 1.0 should appear in reasonable numbers: {clamped_count}/{}",
            samples.len()
        );
    }

    /// Even if one side (system) stops supplying midway, output does not stop: the starved side
    /// is treated as silence 0.0 and mixing continues with only the mic side's audio (the mic
    /// alone value 0.2 starts flowing).
    #[test]
    fn mix_survives_one_side_starvation() {
        let mic = Box::new(ConstBackend::new(0.2, None));
        // system feeds for only 150ms and then stops (the thread stays alive).
        let system = Box::new(ConstBackend::new(0.3, Some(Duration::from_millis(150))));
        let (mut be, mut consumer) = start_composite(mic, system, 1.0, 1.0);

        // After system stops (150ms of wall clock) → the backlog drains → the starvation
        // threshold elapses, the mic alone value 0.2 always starts flowing. A wall-clock window
        // check such as "most of the window after 400ms is 0.2" breaks merely because the
        // backlog drains late under load, so this is an existence guarantee that "waits until
        // one chunk of mic alone values has come out".
        let mut mic_only = 0usize;
        let samples = collect_until(&mut consumer, COLLECT_MAX_WAIT, |new| {
            mic_only += count_near(new, 0.2);
            mic_only >= ONE_CHUNK_SAMPLES
        });
        be.stop();

        // Set check: the only values allowed are the mixed value 0.5 / mic alone 0.2 / system
        // alone 0.3 / 0.0 at the priming boundary.
        assert_only_allowed_values(&samples, 0.5, 0.2, 0.3);
        // Existence guarantee: even after system stops, output does not stop, and the mic alone
        // value from zero-filling the starved side actually flows.
        let mic_only_count = count_near(&samples, 0.2);
        assert!(
            mic_only_count >= ONE_CHUNK_SAMPLES,
            "after starvation the mic alone value 0.2 should keep flowing: {mic_only_count}/{}",
            samples.len()
        );
    }

    /// If the system child's start returns Err, the mic child started first is stopped and the
    /// whole start is Err too (never report success with only one side running).
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
            .expect_err("system start failure should make the whole start Err");
        assert!(
            matches!(err, Error::Backend(_)),
            "the system child's Err should propagate: {err:?}"
        );
        assert_eq!(starts.load(Ordering::SeqCst), 1, "mic is started once");
        assert_eq!(
            stops.load(Ordering::SeqCst),
            1,
            "mic should be stopped when system fails"
        );
    }

    /// If the mic child's start is Err, the system child is not touched and it is Err
    /// immediately. stop is idempotent and can be called twice.
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
            "mic start failure should be Err immediately"
        );
        assert_eq!(
            starts.load(Ordering::SeqCst),
            0,
            "system is not started when mic fails"
        );

        // stop is idempotent even when never started (the children's stop is under the
        // idempotency contract too).
        be.stop();
        be.stop();
    }

    /// End-to-end with the composite backend mounted on a real [`Stream`](crate::Stream).
    /// 20ms/960frame chunks flow and the data is the mixed value (0.2 + 0.3 = 0.5).
    /// Confirms that seq and the chunk contract keep working as is with the Stream unchanged.
    #[test]
    fn stream_delivers_mixed_chunks_end_to_end() {
        use flexaudio_core::types::StreamConfig;

        let mic = Box::new(ConstBackend::new(0.2, None));
        let system = Box::new(ConstBackend::new(0.3, None));
        let backend = Box::new(CompositeBackend::new(mic, system, 1.0, 1.0));
        let mut stream = crate::Stream::open(StreamConfig::default(), backend).expect("open");
        stream.start().expect("start");

        // Poll until one chunk of mixed-value samples arrives (a fixed wall-clock window is
        // flaky under load, so wait until the condition is reached. The limit is a hang guard).
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
                assert!(c.seq > chunks[i - 1].seq, "seq increases monotonically");
            }
        }
        // Set check over every chunk and every sample: the only values allowed are the mixed
        // value 0.2 + 0.3 = 0.5, the one-side values under starvation zero fill 0.2 (mic alone)
        // / 0.3 (system alone), and 0.0 at the priming boundary. The Stream's first stage is a
        // passthrough at 48k/stereo (gain 1.0 leaves the bytes unchanged), so the mixer's output
        // values arrive as is.
        let all: Vec<f32> = chunks.iter().flat_map(|c| c.data.iter().copied()).collect();
        let mixed_count = assert_only_allowed_values(&all, 0.5, 0.2, 0.3);
        // Existence guarantee: mixing actually happens (absolute count of one chunk).
        assert!(
            mixed_count >= ONE_CHUNK_SAMPLES,
            "the mixed value 0.5 should appear in reasonable numbers: {mixed_count}/{}",
            all.len()
        );
    }

    // ---- Drift correction components in isolation (pure, deterministic) ----

    /// When the system side accumulates (mic - system is negative) r moves above 1.0 (towards
    /// consuming faster), and when the mic side accumulates it moves below 1.0.
    #[test]
    fn drift_controller_moves_toward_lagging_side() {
        let mut c = DriftController::new();
        for _ in 0..50 {
            c.update(0, 9_600);
        }
        assert!(
            c.ratio > 1.0,
            "r should be > 1.0 when the system side accumulates: {}",
            c.ratio
        );

        let mut c = DriftController::new();
        for _ in 0..50 {
            c.update(9_600, 0);
        }
        assert!(
            c.ratio < 1.0,
            "r should be < 1.0 when the mic side accumulates: {}",
            c.ratio
        );
    }

    /// No matter how large a backlog difference keeps being applied, r tops out at
    /// 1.0 ± 500ppm.
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

    /// Even when a huge backlog difference is applied in one shot, a single update can move
    /// only up to the slew width.
    #[test]
    fn drift_controller_slew_limits_change_per_update() {
        let mut c = DriftController::new();
        c.update(0, 10_000_000);
        assert!(
            (c.ratio - (1.0 + DRIFT_SLEW_PER_UPDATE)).abs() < 1e-12,
            "the first update should top out exactly at the slew width: {}",
            c.ratio
        );
        c.update(0, 10_000_000);
        assert!(
            (c.ratio - (1.0 + 2.0 * DRIFT_SLEW_PER_UPDATE)).abs() < 1e-12,
            "the second one also moves one step at a time: {}",
            c.ratio
        );

        let mut c = DriftController::new();
        c.update(10_000_000, 0);
        assert!(
            (c.ratio - (1.0 - DRIFT_SLEW_PER_UPDATE)).abs() < 1e-12,
            "the opposite direction is also exactly the slew width: {}",
            c.ratio
        );
    }

    /// on_output does not revisit the ratio until 100ms of mixed output (the update interval)
    /// has accumulated.
    #[test]
    fn drift_controller_updates_only_at_interval() {
        let mut c = DriftController::new();
        c.on_output(DRIFT_UPDATE_INTERVAL_SAMPLES - 1, 0, 10_000_000);
        assert!(
            (c.ratio - 1.0).abs() < 1e-15,
            "should not move below the interval: {}",
            c.ratio
        );
        c.on_output(1, 0, 10_000_000);
        assert!(
            c.ratio > 1.0,
            "should revisit once the interval is reached: {}",
            c.ratio
        );
    }

    /// At unit speed (r = 1.0, phase 0) the stitcher is a perfect passthrough: the same values
    /// as the input come out as is, the FIFO is fully consumed, and the phase stays 0.
    #[test]
    fn stitcher_unity_ratio_is_passthrough() {
        let mut st = LinearStitcher::new();
        let src: Vec<f32> = (0..10).flat_map(|f| [f as f32, -(f as f32)]).collect();
        let mut fifo = src.clone();
        assert_eq!(st.producible(10, 1.0), 10);
        let mut out = Vec::new();
        st.pull(&mut fifo, 1.0, 10, &mut out);
        assert_eq!(out, src, "r=1.0 should be a passthrough");
        assert!(
            fifo.is_empty(),
            "should be fully consumed: {} remaining",
            fifo.len()
        );
        assert!(st.frac.abs() < 1e-12, "phase stays 0: {}", st.frac);
    }

    /// Reading a ramp (value of frame k = k) with r = 1.25 yields exactly the linear
    /// interpolation values at positions 0 / 1.25 / 2.5 / 3.75 (both channels, deterministic).
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
                "ch2 is interpolated at the same position: {}",
                out[k * 2 + 1]
            );
        }
        // floor(0 + 4×1.25) = 5, so all frames have been read and the phase returns to 0.
        assert!(
            fifo.is_empty(),
            "should be fully consumed: {} remaining",
            fifo.len()
        );
        assert!(st.frac.abs() < 1e-12, "phase: {}", st.frac);
    }

    // ---- Synchronous drift simulation without threads (deterministic) ----

    /// ChildLane for the thread-free synchronous simulation. The ring and normalizer are only
    /// held for form; the test supplies data directly into the FIFO with [`sim_feed`].
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
        .expect("passthrough normalizer");
        ChildLane {
            consumer,
            normalizer,
            fifo: Vec::new(),
            last_supply: Instant::now(),
        }
    }

    /// Supplies DC frames directly, in the same way as ingest (append to the FIFO + the limit
    /// safety valve).
    fn sim_feed(lane: &mut ChildLane, value: f32, frames: usize) {
        let new_len = lane.fifo.len() + frames * CHANNELS as usize;
        lane.fifo.resize(new_len, value);
        if lane.fifo.len() > FIFO_MAX_SAMPLES {
            let excess = lane.fifo.len() - FIFO_MAX_SAMPLES;
            lane.fifo.drain(..excess);
        }
        lane.last_supply = Instant::now();
    }

    /// Measurement results of the synchronous drift simulation.
    struct DriftSimOutcome {
        /// Post-consumption FIFO backlog (in f32 samples) at each simulated second.
        system_backlog: Vec<usize>,
        mic_backlog: Vec<usize>,
        final_ratio: f64,
    }

    /// Synchronous simulation without threads. With 20ms as one tick, it supplies DC to mic at
    /// unit rate (960 frame/tick) and to system at (1 + ppm×1e-6) times the rate, "simulating
    /// the passage of time with sample counts", and drives mix_and_push directly in a loop.
    /// It does not depend on threads or the wall clock, so it is fully deterministic.
    ///
    /// `fixed_unity_ratio` is a path equivalent to "no correction" that resets the controller to
    /// 1.0 on every tick. Used to self-check that this simulation actually reproduces the drift
    /// problem (monotonic growth of the backlog).
    ///
    /// Every output value is verified on every tick: both sides always have supply, so the only
    /// value allowed is the mixed value (0.2 + 0.3 = 0.5; linear interpolation of DC is the same
    /// DC value). If even one sample of starvation zero fill 0.0 or a one-side value appears it
    /// FAILs immediately (this doubles as verifying that "zero fill does not occur in steady
    /// state").
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

        // Carry-over of the fractional system supply frame count (simulates the rate
        // difference exactly in sample counts).
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
                    "the steady-state simulation output should be only the mixed value 0.5 (no \
                     zero fill or one-side values): {s}"
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

    /// +300ppm (system is faster) for 60 seconds: with correction the system FIFO backlog does
    /// not reach the safety valve, its growth slows down, and it stays smaller than the
    /// no-correction equivalent. With the no-correction equivalent (r = 1.0 fixed) the backlog
    /// grows monotonically under the same conditions = self-check that the test itself
    /// reproduces the drift problem.
    #[test]
    fn mix_drift_sim_plus_300ppm_stays_bounded() {
        let corrected = run_drift_sim(300.0, 60, false);
        let uncorrected = run_drift_sim(300.0, 60, true);

        // Self-check: without correction the system backlog grows monotonically every second
        // (+300ppm ≒ +28.8 samples/s).
        for (i, w) in uncorrected.system_backlog.windows(2).enumerate() {
            assert!(
                w[1] > w[0],
                "without correction the backlog should grow monotonically: second {i} {} → {}",
                w[0],
                w[1]
            );
        }

        // With correction: the backlog does not reach the safety valve (FIFO_MAX_SAMPLES =
        // 500ms worth).
        let max_corrected = corrected.system_backlog.iter().copied().max().unwrap();
        assert!(
            max_corrected < FIFO_MAX_SAMPLES,
            "with correction it should not reach the safety valve: max {max_corrected}"
        );

        // With correction: it stays smaller than without correction (the correction actually
        // works).
        let last_c = *corrected.system_backlog.last().unwrap();
        let last_u = *uncorrected.system_backlog.last().unwrap();
        assert!(
            last_c < last_u,
            "backlog with correction {last_c} should be < without correction {last_u}"
        );

        // With correction: the growth of the backlog slows down (increase over the first 9
        // seconds > increase over the last 9 seconds).
        let sb = &corrected.system_backlog;
        let early = sb[9] - sb[0];
        let late = sb[59] - sb[50];
        assert!(
            late < early,
            "growth should slow down as the ratio catches up: early +{early} vs late +{late}"
        );

        // The ratio moves towards "consuming system faster" and stays within the clamp.
        assert!(
            corrected.final_ratio > 1.0 + 2e-5,
            "r should move above 1.0: {}",
            corrected.final_ratio
        );
        assert!(
            corrected.final_ratio <= 1.0 + DRIFT_RATIO_LIMIT + 1e-12,
            "r is within the clamp: {}",
            corrected.final_ratio
        );
    }

    /// -300ppm (system is slower) for 60 seconds: starvation zero fill does not occur in steady
    /// state (output values are fully verified on every tick inside run_drift_sim), and with
    /// correction the mic-side backlog does not reach the safety valve and stays smaller than
    /// the no-correction equivalent.
    #[test]
    fn mix_drift_sim_minus_300ppm_no_steady_zero_fill() {
        let corrected = run_drift_sim(-300.0, 60, false);
        let uncorrected = run_drift_sim(-300.0, 60, true);

        // Self-check: without correction the mic-side backlog grows monotonically, since it
        // keeps pace with the slow system.
        for (i, w) in uncorrected.mic_backlog.windows(2).enumerate() {
            assert!(
                w[1] > w[0],
                "without correction the mic backlog should grow monotonically: second {i} {} → {}",
                w[0],
                w[1]
            );
        }

        // With correction: the mic backlog does not reach the safety valve and stays smaller
        // than without correction.
        let max_corrected = corrected.mic_backlog.iter().copied().max().unwrap();
        assert!(
            max_corrected < FIFO_MAX_SAMPLES,
            "with correction it should not reach the safety valve: max {max_corrected}"
        );
        let last_c = *corrected.mic_backlog.last().unwrap();
        let last_u = *uncorrected.mic_backlog.last().unwrap();
        assert!(
            last_c < last_u,
            "mic backlog with correction {last_c} should be < without correction {last_u}"
        );

        // The system side does not accumulate, since consumption follows supply (at most the
        // interpolation remainder + the most recent partial chunk).
        let max_sys = corrected.system_backlog.iter().copied().max().unwrap();
        assert!(
            max_sys < ONE_CHUNK_SAMPLES,
            "the system side should not accumulate: max {max_sys}"
        );

        // The ratio moves towards "consuming system more slowly" and stays within the clamp.
        assert!(
            corrected.final_ratio < 1.0 - 2e-5,
            "r should move below 1.0: {}",
            corrected.final_ratio
        );
        assert!(
            corrected.final_ratio >= 1.0 - DRIFT_RATIO_LIMIT - 1e-12,
            "r is within the clamp: {}",
            corrected.final_ratio
        );
    }
}
