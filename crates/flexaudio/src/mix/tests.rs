//! Offline mixing arithmetic and drift tests.
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
        err.kind() == ErrorKind::Backend,
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
        lane: MixLane::Microphone,
        format: (SAMPLE_RATE, CHANNELS),
        diagnostics: CaptureDiagnostics::new(SAMPLE_RATE, CHANNELS),
        overflow_seen: 0,
        notices: Arc::new(Notices::default()),
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
