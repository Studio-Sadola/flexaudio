//! Hardware-free capture backends for testing / verification.
//!
//! [`MockBackend`] generates a sine wave instead of a real OS device and pushes it to the
//! [`RawSink`] at roughly real-time pace. This lets
//! [`Stream`](crate::Stream) be driven end-to-end without real hardware.

use std::f32::consts::PI;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use flexaudio_core::backend::{CaptureBackend, RawSink};
use flexaudio_core::types::Result;

/// Duration generated per push (milliseconds).
const BLOCK_MS: u32 = 10;

/// Pseudo backend that generates a sine wave and streams it to the [`RawSink`] in the native
/// format.
///
/// `native_format` returns the `(sample_rate, channels)` passed to `new` as-is.
/// [`start`](Self::start) launches the generator thread, which keeps pushing `BLOCK_MS`
/// (default 10ms) worth of interleaved `f32` samples at real-time pace. [`stop`](Self::stop)
/// stops and joins the generator thread.
///
/// ```no_run
/// use flexaudio::mock::MockBackend;
/// use flexaudio::Stream;
/// use flexaudio_core::types::StreamConfig;
///
/// // Pseudo source imitating a 44.1kHz / mono microphone.
/// let backend = Box::new(MockBackend::new(44_100, 1, 440.0));
/// let mut stream = Stream::open(StreamConfig::default(), backend).unwrap();
/// stream.start().unwrap();
/// // ... take chunks with stream.poll_chunk() ...
/// stream.stop();
/// ```
pub struct MockBackend {
    sample_rate: u32,
    channels: u16,
    freq_hz: f32,
    /// Stop signal to the generator thread.
    running: Arc<AtomicBool>,
    /// Handle of the generator thread (Some after start).
    handle: Option<JoinHandle<()>>,
}

impl MockBackend {
    /// Creates one with the native `(sample_rate, channels)` and the frequency of the sine
    /// wave to generate.
    ///
    /// A frequency of `0.0` or below generates effectively silence (DC 0).
    pub fn new(sample_rate: u32, channels: u16, freq_hz: f32) -> Self {
        Self {
            sample_rate: sample_rate.max(1),
            channels: channels.max(1),
            freq_hz,
            running: Arc::new(AtomicBool::new(false)),
            handle: None,
        }
    }
}

impl CaptureBackend for MockBackend {
    fn native_format(&self) -> (u32, u16) {
        (self.sample_rate, self.channels)
    }

    fn start(&mut self, mut sink: RawSink) -> Result<()> {
        // Do nothing if already running (safe against a double start).
        if self.running.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.running.store(true, Ordering::SeqCst);

        let running = self.running.clone();
        let sample_rate = self.sample_rate;
        let channels = self.channels as usize;
        let freq = self.freq_hz;

        let handle = thread::Builder::new()
            .name("flexaudio-mock-gen".into())
            .spawn(move || {
                let frames_per_block =
                    ((sample_rate as u64 * BLOCK_MS as u64) / 1000).max(1) as usize;
                let block_dur = Duration::from_millis(BLOCK_MS as u64);
                let two_pi_f_over_sr = 2.0 * PI * freq / sample_rate as f32;

                // Global frame index for continuous phase (avoids phase discontinuities).
                let mut phase_frame: u64 = 0;
                // device PTS (ns) — a monotonic timestamp based on the native SR.
                let start = Instant::now();

                let mut scratch: Vec<f32> = Vec::with_capacity(frames_per_block * channels);

                while running.load(Ordering::SeqCst) {
                    scratch.clear();
                    for _ in 0..frames_per_block {
                        let s = if freq > 0.0 {
                            (two_pi_f_over_sr * phase_frame as f32).sin() * 0.5
                        } else {
                            0.0
                        };
                        // interleaved: the same sample on every channel (mono-equivalent content).
                        for _ in 0..channels {
                            scratch.push(s);
                        }
                        phase_frame = phase_frame.wrapping_add(1);
                    }

                    // device PTS (ns) of this block's first frame = approximation based on elapsed
                    // time.
                    let pts_ns = start.elapsed().as_nanos() as i64;
                    sink.push(&scratch, pts_ns);

                    // Sleep at roughly real-time pace.
                    thread::sleep(block_dur);
                }
            })
            .map_err(|e| {
                flexaudio_core::types::Error::Backend(format!("spawn mock thread: {e}"))
            })?;

        self.handle = Some(handle);
        Ok(())
    }

    fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            // Join the generator thread. Even while sleeping, it stops at the top of the next loop.
            let _ = h.join();
        }
    }
}

impl Drop for MockBackend {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Test-only backend that can stall.
///
/// The regular [`MockBackend`] keeps pushing the sine wave without interruption, so the
/// watchdog path of stall detection -> reopen ->
/// [`ChunkFlags::RECOVERED`](flexaudio_core::types::ChunkFlags::RECOVERED) cannot be tested
/// with it. This backend stops feeding (= stall) only in the first `start()` session once
/// `stall_after` has elapsed, and the second and later sessions, reopened by the watchdog via
/// `stop()` -> `start()`, return to normal feeding. It reproduces stall -> automatic recovery
/// -> RECOVERED on the recovery chunk without real hardware.
///
/// The number of `start()` calls is counted with a shared [`AtomicU32`], and it stalls only in
/// generation 0 (the first). Test use only (not a public API).
#[doc(hidden)]
pub struct StallableMockBackend {
    sample_rate: u32,
    channels: u16,
    freq_hz: f32,
    /// Elapsed time until feeding stops in the first session.
    stall_after: Duration,
    /// Stop signal to the generator thread.
    running: Arc<AtomicBool>,
    /// Number of `start()` calls so far (= session generation). Shared and read by the
    /// generator thread.
    start_count: Arc<AtomicU32>,
    /// Handle of the generator thread.
    handle: Option<JoinHandle<()>>,
}

impl StallableMockBackend {
    /// Creates one with the native `(sample_rate, channels)`, the frequency, and the elapsed
    /// time until the first session stalls.
    pub fn new(sample_rate: u32, channels: u16, freq_hz: f32, stall_after: Duration) -> Self {
        Self {
            sample_rate: sample_rate.max(1),
            channels: channels.max(1),
            freq_hz,
            stall_after,
            running: Arc::new(AtomicBool::new(false)),
            start_count: Arc::new(AtomicU32::new(0)),
            handle: None,
        }
    }

    /// Number of `start()` calls so far (= number of reopens + 1). For observation in tests.
    pub fn start_count(&self) -> u32 {
        self.start_count.load(Ordering::SeqCst)
    }
}

impl CaptureBackend for StallableMockBackend {
    fn native_format(&self) -> (u32, u16) {
        (self.sample_rate, self.channels)
    }

    fn start(&mut self, mut sink: RawSink) -> Result<()> {
        if self.running.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.running.store(true, Ordering::SeqCst);

        // Generation of this session (0-based). Stalls only in generation 0.
        let generation = self.start_count.fetch_add(1, Ordering::SeqCst);

        let running = self.running.clone();
        let sample_rate = self.sample_rate;
        let channels = self.channels as usize;
        let freq = self.freq_hz;
        let stall_after = self.stall_after;

        let handle = thread::Builder::new()
            .name("flexaudio-stallable-mock-gen".into())
            .spawn(move || {
                let frames_per_block =
                    ((sample_rate as u64 * BLOCK_MS as u64) / 1000).max(1) as usize;
                let block_dur = Duration::from_millis(BLOCK_MS as u64);
                let two_pi_f_over_sr = 2.0 * PI * freq / sample_rate as f32;

                let mut phase_frame: u64 = 0;
                let session_start = Instant::now();
                let mut scratch: Vec<f32> = Vec::with_capacity(frames_per_block * channels);

                while running.load(Ordering::SeqCst) {
                    // Stop feeding (= stall) in generation 0 once stall_after is exceeded.
                    // The thread stays alive and only the push stops, so the watchdog detects
                    // the stagnation of last_sample_ns after STALL_THRESHOLD.
                    let stalled = generation == 0 && session_start.elapsed() >= stall_after;

                    if !stalled {
                        scratch.clear();
                        for _ in 0..frames_per_block {
                            let s = if freq > 0.0 {
                                (two_pi_f_over_sr * phase_frame as f32).sin() * 0.5
                            } else {
                                0.0
                            };
                            for _ in 0..channels {
                                scratch.push(s);
                            }
                            phase_frame = phase_frame.wrapping_add(1);
                        }
                        let pts_ns = session_start.elapsed().as_nanos() as i64;
                        sink.push(&scratch, pts_ns);
                    }

                    thread::sleep(block_dur);
                }
            })
            .map_err(|e| {
                flexaudio_core::types::Error::Backend(format!("spawn stallable mock thread: {e}"))
            })?;

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

impl Drop for StallableMockBackend {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Test-only backend that panics in `start()` or `stop()`.
///
/// OS backends' start/stop is external code from flexaudio's point of view and may panic as a
/// contract violation. That panic poisons `SharedState.backend` (a `Mutex` locked across
/// start/stop), and the ingest/watchdog thread dies silently in a cascading panic the moment
/// it next takes that lock — this backend exercises that regression.
/// [`Stream`](crate::Stream) surfaces this panic as
/// [`Error::Backend`](flexaudio_core::types::Error::Backend) /
/// [`Event::Error`](flexaudio_core::types::Event::Error) and does not make other threads
/// panic in cascade (see the panic regression tests in `stream.rs`).
///
/// - [`PanicMode::Start`]: panics on the first `start()`.
/// - [`PanicMode::Stop`]: panics on `stop()` (start succeeds and feeding happens too).
///
/// Test use only (not a public API).
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanicMode {
    /// Panics on a `start()` call.
    Start,
    /// Panics on a `stop()` call.
    Stop,
}

/// Pseudo backend that panics at the specified timing (see [`PanicMode`]).
///
/// With `PanicMode::Stop`, it feeds a sine wave like [`MockBackend`] and then panics in
/// `stop()`. With `PanicMode::Start`, it panics inside `start()` before starting the feeder
/// thread. Test use only (not a public API).
#[doc(hidden)]
pub struct PanickingMockBackend {
    sample_rate: u32,
    channels: u16,
    freq_hz: f32,
    mode: PanicMode,
    /// Feeder thread (Some only when start succeeded with `PanicMode::Stop`).
    running: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl PanickingMockBackend {
    /// Creates one with the native `(sample_rate, channels)`, frequency, and panic timing.
    pub fn new(sample_rate: u32, channels: u16, freq_hz: f32, mode: PanicMode) -> Self {
        Self {
            sample_rate: sample_rate.max(1),
            channels: channels.max(1),
            freq_hz,
            mode,
            running: Arc::new(AtomicBool::new(false)),
            handle: None,
        }
    }
}

impl CaptureBackend for PanickingMockBackend {
    fn native_format(&self) -> (u32, u16) {
        (self.sample_rate, self.channels)
    }

    fn start(&mut self, mut sink: RawSink) -> Result<()> {
        if self.mode == PanicMode::Start {
            // Panic in start (imitates an arbitrary backend's contract violation).
            panic!("PanickingMockBackend: intentional panic in start()");
        }

        // PanicMode::Stop: start the feeder thread as usual (it panics in stop).
        if self.running.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.running.store(true, Ordering::SeqCst);

        let running = self.running.clone();
        let sample_rate = self.sample_rate;
        let channels = self.channels as usize;
        let freq = self.freq_hz;

        let handle = thread::Builder::new()
            .name("flexaudio-panicking-mock-gen".into())
            .spawn(move || {
                let frames_per_block =
                    ((sample_rate as u64 * BLOCK_MS as u64) / 1000).max(1) as usize;
                let block_dur = Duration::from_millis(BLOCK_MS as u64);
                let two_pi_f_over_sr = 2.0 * PI * freq / sample_rate as f32;

                let mut phase_frame: u64 = 0;
                let start = Instant::now();
                let mut scratch: Vec<f32> = Vec::with_capacity(frames_per_block * channels);

                while running.load(Ordering::SeqCst) {
                    scratch.clear();
                    for _ in 0..frames_per_block {
                        let s = if freq > 0.0 {
                            (two_pi_f_over_sr * phase_frame as f32).sin() * 0.5
                        } else {
                            0.0
                        };
                        for _ in 0..channels {
                            scratch.push(s);
                        }
                        phase_frame = phase_frame.wrapping_add(1);
                    }
                    let pts_ns = start.elapsed().as_nanos() as i64;
                    sink.push(&scratch, pts_ns);
                    thread::sleep(block_dur);
                }
            })
            .map_err(|e| {
                flexaudio_core::types::Error::Backend(format!("spawn panicking mock thread: {e}"))
            })?;

        self.handle = Some(handle);
        Ok(())
    }

    fn stop(&mut self) {
        // First reliably stop and join the feeder thread (prevents leaks / hangs).
        self.running.store(false, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        if self.mode == PanicMode::Stop {
            // Panic in stop (imitates an arbitrary backend's contract violation).
            panic!("PanickingMockBackend: intentional panic in stop()");
        }
    }
}

impl Drop for PanickingMockBackend {
    fn drop(&mut self) {
        // Do not panic from Drop (avoids double panic -> abort). Only reliably stop the
        // feeder thread. The PanicMode::Stop panic happens only on an explicit `stop()` call.
        self.running.store(false, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Test-only backend whose first `start()` succeeds and feeds, which stops feeding (= stall)
/// once `stall_after` has elapsed, and which panics on the watchdog's reopen (the second and
/// later `start()`).
///
/// A regression backend that verifies that even if the backend panics inside the watchdog
/// thread, it does not poison the `SharedState.backend` mutex and make the ingest/watchdog
/// threads die silently in cascade, but is surfaced as
/// [`Event::Error`](flexaudio_core::types::Event::Error) ("reopen failed: ..."). It combines
/// the stall mechanism of [`StallableMockBackend`] with the panic of
/// [`PanickingMockBackend`].
///
/// Test use only (not a public API).
#[doc(hidden)]
pub struct StallThenPanicOnReopenBackend {
    sample_rate: u32,
    channels: u16,
    freq_hz: f32,
    stall_after: Duration,
    running: Arc<AtomicBool>,
    /// Number of `start()` calls. Succeeds and feeds only in generation 0 (the first); panics
    /// in generation 1 and later.
    start_count: Arc<AtomicU32>,
    handle: Option<JoinHandle<()>>,
}

impl StallThenPanicOnReopenBackend {
    /// Creates one with the native `(sample_rate, channels)`, frequency, and elapsed time
    /// until the first stall.
    pub fn new(sample_rate: u32, channels: u16, freq_hz: f32, stall_after: Duration) -> Self {
        Self {
            sample_rate: sample_rate.max(1),
            channels: channels.max(1),
            freq_hz,
            stall_after,
            running: Arc::new(AtomicBool::new(false)),
            start_count: Arc::new(AtomicU32::new(0)),
            handle: None,
        }
    }
}

impl CaptureBackend for StallThenPanicOnReopenBackend {
    fn native_format(&self) -> (u32, u16) {
        (self.sample_rate, self.channels)
    }

    fn start(&mut self, mut sink: RawSink) -> Result<()> {
        if self.running.load(Ordering::SeqCst) {
            return Ok(());
        }
        // Fix the generation (0-based). Generation 1 and later = watchdog reopen, which panics.
        let generation = self.start_count.fetch_add(1, Ordering::SeqCst);
        if generation >= 1 {
            // Reopen from the watchdog thread. Panic here.
            panic!("StallThenPanicOnReopenBackend: intentional panic on reopen start()");
        }

        // Generation 0: normal feeding (stops feeding at stall_after = stall).
        self.running.store(true, Ordering::SeqCst);
        let running = self.running.clone();
        let sample_rate = self.sample_rate;
        let channels = self.channels as usize;
        let freq = self.freq_hz;
        let stall_after = self.stall_after;

        let handle = thread::Builder::new()
            .name("flexaudio-stall-then-panic-gen".into())
            .spawn(move || {
                let frames_per_block =
                    ((sample_rate as u64 * BLOCK_MS as u64) / 1000).max(1) as usize;
                let block_dur = Duration::from_millis(BLOCK_MS as u64);
                let two_pi_f_over_sr = 2.0 * PI * freq / sample_rate as f32;

                let mut phase_frame: u64 = 0;
                let session_start = Instant::now();
                let mut scratch: Vec<f32> = Vec::with_capacity(frames_per_block * channels);

                while running.load(Ordering::SeqCst) {
                    let stalled = session_start.elapsed() >= stall_after;
                    if !stalled {
                        scratch.clear();
                        for _ in 0..frames_per_block {
                            let s = if freq > 0.0 {
                                (two_pi_f_over_sr * phase_frame as f32).sin() * 0.5
                            } else {
                                0.0
                            };
                            for _ in 0..channels {
                                scratch.push(s);
                            }
                            phase_frame = phase_frame.wrapping_add(1);
                        }
                        let pts_ns = session_start.elapsed().as_nanos() as i64;
                        sink.push(&scratch, pts_ns);
                    }
                    thread::sleep(block_dur);
                }
            })
            .map_err(|e| {
                flexaudio_core::types::Error::Backend(format!(
                    "spawn stall-then-panic mock thread: {e}"
                ))
            })?;

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

impl Drop for StallThenPanicOnReopenBackend {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}
