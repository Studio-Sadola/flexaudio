//! Capture backends for tests and verification that require no hardware.
//!
//! [`MockBackend`] generates a sine wave instead of using a real OS device and pushes it to
//! [`RawSink`] at roughly real-time speed. This lets you run [`Stream`](crate::Stream)
//! end-to-end without real hardware.

use std::f32::consts::PI;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use flexaudio_core::backend::{CaptureBackend, RawSink};
use flexaudio_core::types::Result;

/// Duration in milliseconds of data generated in each push.
const BLOCK_MS: u32 = 10;

/// Mock backend that generates a sine wave and sends it to [`RawSink`] in the native format.
///
/// `native_format` returns the `(sample_rate, channels)` passed to `new` unchanged.
/// [`start`](Self::start) starts a generator thread that continuously pushes `BLOCK_MS`
/// (default 10 ms) of interleaved `f32` samples at real-time speed. [`stop`](Self::stop) stops
/// and joins the generator thread.
///
/// ```no_run
/// use flexaudio::mock::MockBackend;
/// use flexaudio::Stream;
/// use flexaudio_core::types::StreamConfig;
///
/// // Mock source simulating a 44.1 kHz / mono microphone.
/// let backend = Box::new(MockBackend::new(44_100, 1, 440.0));
/// let mut stream = Stream::open(StreamConfig::default(), backend).unwrap();
/// stream.start().unwrap();
/// // ... retrieve chunks with stream.poll_chunk() ...
/// stream.stop();
/// ```
pub struct MockBackend {
    sample_rate: u32,
    channels: u16,
    freq_hz: f32,
    /// Stop signal for the generator thread.
    running: Arc<AtomicBool>,
    /// Generator thread handle (Some after start).
    handle: Option<JoinHandle<()>>,
    shutdown: MockShutdown,
}

impl MockBackend {
    /// Create with a native `(sample_rate, channels)` format and sine wave frequency.
    ///
    /// A frequency of `0.0` or below generates effectively silent output (DC 0).
    pub fn new(sample_rate: u32, channels: u16, freq_hz: f32) -> Self {
        Self {
            sample_rate: sample_rate.max(1),
            channels: channels.max(1),
            freq_hz,
            running: Arc::new(AtomicBool::new(false)),
            handle: None,
            shutdown: MockShutdown::default(),
        }
    }
}

impl CaptureBackend for MockBackend {
    fn native_format(&self) -> (u32, u16) {
        (self.sample_rate, self.channels)
    }

    fn start(&mut self, mut sink: RawSink) -> Result<()> {
        // Do nothing if already running (safe for repeated start calls).
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
                // Device PTS (ns) -- monotonic timestamp based on the native sample rate.
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
                        // Interleaved: same sample in every channel (mono-equivalent content).
                        for _ in 0..channels {
                            scratch.push(s);
                        }
                        phase_frame = phase_frame.wrapping_add(1);
                    }

                    // Device PTS (ns) of the first frame in this block, approximated by elapsed time.
                    let pts_ns = start.elapsed().as_nanos() as i64;
                    sink.push(&scratch, pts_ns);

                    // Sleep to maintain roughly real-time pacing.
                    thread::sleep(block_dur);
                }
            })
            .map_err(|e| {
                flexaudio_core::types::Error::Backend(format!("spawn mock thread: {e}"))
            })?;

        self.handle = Some(handle);
        self.shutdown.retire();
        self.shutdown = MockShutdown::default();
        Ok(())
    }

    fn stop(&mut self) {
        self.shutdown.retain(&self.running, &mut self.handle);
    }

    fn stop_checked(&mut self) -> Result<()> {
        self.stop();
        self.shutdown.result()
    }

    fn poll_event(&mut self) -> Option<flexaudio_core::Event> {
        self.shutdown.poll_event()
    }
}

impl Drop for MockBackend {
    fn drop(&mut self) {
        self.shutdown.retain_drop(&self.running, &mut self.handle);
    }
}

/// Test-only backend that can stall.
///
/// The regular [`MockBackend`] keeps pushing a sine wave, so it cannot exercise the watchdog's
/// stall detection -> reopen -> [`ChunkFlags::RECOVERED`](flexaudio_core::types::ChunkFlags::RECOVERED)
/// path. This backend stops feeding (stalls) the first `start()` session after `stall_after`;
/// sessions after the watchdog reopens with `stop()` -> `start()` feed normally. This reproduces
/// stall -> automatic recovery -> RECOVERED on the recovery chunk without real hardware.
///
/// A shared [`AtomicU32`] counts `start()` calls; only generation 0 (the first call) stalls.
/// For tests only (not a public API).
#[doc(hidden)]
pub struct StallableMockBackend {
    sample_rate: u32,
    channels: u16,
    freq_hz: f32,
    /// Time until feeding stops in the first session.
    stall_after: Duration,
    /// Stop signal for the generator thread.
    running: Arc<AtomicBool>,
    /// Number of `start()` calls so far (session generation), shared with the generator thread.
    start_count: Arc<AtomicU32>,
    /// Generator thread handle.
    handle: Option<JoinHandle<()>>,
    shutdown: MockShutdown,
}

impl StallableMockBackend {
    /// Create with a native `(sample_rate, channels)` format, frequency, and time until the
    /// first session stalls.
    pub fn new(sample_rate: u32, channels: u16, freq_hz: f32, stall_after: Duration) -> Self {
        Self {
            sample_rate: sample_rate.max(1),
            channels: channels.max(1),
            freq_hz,
            stall_after,
            running: Arc::new(AtomicBool::new(false)),
            start_count: Arc::new(AtomicU32::new(0)),
            handle: None,
            shutdown: MockShutdown::default(),
        }
    }

    /// Number of `start()` calls so far (reopen count + 1), for test observation.
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

        // Generation of this session (zero-based). Only generation 0 stalls.
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
                    // Stop feeding (stall) after stall_after in generation 0.
                    // Keep the thread alive but stop pushing, so the watchdog detects the
                    // unchanged last_sample_ns after STALL_THRESHOLD.
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
        self.shutdown.retire();
        self.shutdown = MockShutdown::default();
        Ok(())
    }

    fn stop(&mut self) {
        self.shutdown.retain(&self.running, &mut self.handle);
    }

    fn stop_checked(&mut self) -> Result<()> {
        self.stop();
        self.shutdown.result()
    }

    fn poll_event(&mut self) -> Option<flexaudio_core::Event> {
        self.shutdown.poll_event()
    }
}

impl Drop for StallableMockBackend {
    fn drop(&mut self) {
        self.shutdown.retain_drop(&self.running, &mut self.handle);
    }
}

/// Test-only backend that panics in `start()` or `stop()`.
///
/// From flexaudio's perspective, OS backend start/stop methods are external code and may panic
/// when they violate their contract. Such a panic can poison the `SharedState.backend` `Mutex`
/// (locked across start/stop), causing the ingest / watchdog thread to die silently with a
/// cascading panic the next time it locks the mutex. This backend exercises that regression.
/// [`Stream`](crate::Stream) surfaces the panic as
/// [`Error::Backend`](flexaudio_core::types::Error::Backend) /
/// [`Event::Error`](flexaudio_core::types::Event::Error) and prevents cascading panics in other
/// threads (see the panic regression tests in `stream.rs`).
///
/// - [`PanicMode::Start`]: Panics on the first `start()` call.
/// - [`PanicMode::Stop`]: Panics on `stop()` (start succeeds and feeds data).
///
/// For tests only (not a public API).
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanicMode {
    /// Panics when `start()` is called.
    Start,
    /// Panics when `stop()` is called.
    Stop,
}

/// Mock backend that panics at the specified point (see [`PanicMode`]).
///
/// In `PanicMode::Stop`, it feeds a sine wave like [`MockBackend`] and then panics in `stop()`.
/// In `PanicMode::Start`, it panics inside `start()` before spawning the feed thread. For tests
/// only (not a public API).
#[doc(hidden)]
pub struct PanickingMockBackend {
    sample_rate: u32,
    channels: u16,
    freq_hz: f32,
    mode: PanicMode,
    /// Feed thread (Some only if start succeeds in `PanicMode::Stop`).
    running: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    shutdown: MockShutdown,
}

impl PanickingMockBackend {
    /// Create with a native `(sample_rate, channels)` format, frequency, and panic timing.
    pub fn new(sample_rate: u32, channels: u16, freq_hz: f32, mode: PanicMode) -> Self {
        Self {
            sample_rate: sample_rate.max(1),
            channels: channels.max(1),
            freq_hz,
            mode,
            running: Arc::new(AtomicBool::new(false)),
            handle: None,
            shutdown: MockShutdown::default(),
        }
    }
}

impl CaptureBackend for PanickingMockBackend {
    fn native_format(&self) -> (u32, u16) {
        (self.sample_rate, self.channels)
    }

    fn start(&mut self, mut sink: RawSink) -> Result<()> {
        if self.mode == PanicMode::Start {
            // Panic in start to simulate a backend contract violation.
            panic!("PanickingMockBackend: intentional panic in start()");
        }

        // PanicMode::Stop: start the feed thread as usual (stop will panic).
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
        self.shutdown.retire();
        self.shutdown = MockShutdown::default();
        Ok(())
    }

    fn stop(&mut self) {
        // Stop and join the feed thread first to prevent leaks or hangs.
        self.shutdown.retain(&self.running, &mut self.handle);
        if self.mode == PanicMode::Stop {
            // Panic in stop to simulate a backend contract violation.
            panic!("PanickingMockBackend: intentional panic in stop()");
        }
    }

    fn stop_checked(&mut self) -> Result<()> {
        self.stop();
        self.shutdown.result()
    }

    fn poll_event(&mut self) -> Option<flexaudio_core::Event> {
        self.shutdown.poll_event()
    }
}

impl Drop for PanickingMockBackend {
    fn drop(&mut self) {
        // Do not panic from Drop (to avoid a double panic leading to abort). Just make sure the
        // feed thread stops. PanicMode::Stop panics only on an explicit `stop()` call.
        self.shutdown.retain_drop(&self.running, &mut self.handle);
    }
}

/// Test-only backend that succeeds and feeds data on the first `start()`, stops feeding (stalls)
/// after `stall_after`, then panics when the watchdog reopens it (on the second or later
/// `start()`).
///
/// Regression backend that verifies a panic in the watchdog thread does not poison the
/// `SharedState.backend` mutex and silently kill the ingest / watchdog threads with cascading
/// panics. Instead, the error is surfaced as [`Event::Error`](flexaudio_core::types::Event::Error)
/// ("reopen failed: ..."). Combines the stall mechanism of [`StallableMockBackend`] with the
/// panic from [`PanickingMockBackend`].
///
/// For tests only (not a public API).
#[doc(hidden)]
pub struct StallThenPanicOnReopenBackend {
    sample_rate: u32,
    channels: u16,
    freq_hz: f32,
    stall_after: Duration,
    running: Arc<AtomicBool>,
    /// Number of `start()` calls. Generation 0 (first call) succeeds and feeds data; later
    /// generations panic.
    start_count: Arc<AtomicU32>,
    handle: Option<JoinHandle<()>>,
    shutdown: MockShutdown,
}

impl StallThenPanicOnReopenBackend {
    /// Create with a native `(sample_rate, channels)` format, frequency, and time until the
    /// first stall.
    pub fn new(sample_rate: u32, channels: u16, freq_hz: f32, stall_after: Duration) -> Self {
        Self {
            sample_rate: sample_rate.max(1),
            channels: channels.max(1),
            freq_hz,
            stall_after,
            running: Arc::new(AtomicBool::new(false)),
            start_count: Arc::new(AtomicU32::new(0)),
            handle: None,
            shutdown: MockShutdown::default(),
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
        // Determine the generation (zero-based). Generation 1 and later panic on watchdog reopen.
        let generation = self.start_count.fetch_add(1, Ordering::SeqCst);
        if generation >= 1 {
            // Reopen from the watchdog thread; panic here.
            panic!("StallThenPanicOnReopenBackend: intentional panic on reopen start()");
        }

        // Generation 0: feed normally, then stop feeding (stall) after stall_after.
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
        self.shutdown.retire();
        self.shutdown = MockShutdown::default();
        Ok(())
    }

    fn stop(&mut self) {
        self.shutdown.retain(&self.running, &mut self.handle);
    }

    fn stop_checked(&mut self) -> Result<()> {
        self.stop();
        self.shutdown.result()
    }

    fn poll_event(&mut self) -> Option<flexaudio_core::Event> {
        self.shutdown.poll_event()
    }
}

impl Drop for StallThenPanicOnReopenBackend {
    fn drop(&mut self) {
        self.shutdown.retain_drop(&self.running, &mut self.handle);
    }
}

/// Generator teardown has the same retained outcome for explicit stop and Drop.
#[derive(Default)]
struct MockShutdown {
    report: Option<crate::ShutdownReport>,
    receipt: flexaudio_core::ShutdownReceipt,
    events: std::collections::VecDeque<flexaudio_core::Event>,
}

impl MockShutdown {
    fn retire(&self) {
        if let Some(report) = &self.report {
            self.receipt
                .report_unobserved(flexaudio_core::DropFailureOwner::MockCapture, report);
        }
    }

    fn retain_drop(&mut self, running: &AtomicBool, handle: &mut Option<JoinHandle<()>>) {
        self.retain(running, handle);
        self.retire();
    }
    fn retain(&mut self, running: &AtomicBool, handle: &mut Option<JoinHandle<()>>) {
        running.store(false, Ordering::SeqCst);
        if self.report.is_some() {
            return;
        }
        let mut cleanup = Vec::new();
        if let Some(handle) = handle.take() {
            if handle.join().is_err() {
                let error = crate::Error::Backend("mock capture owner panicked".into())
                    .with_context(crate::ErrorContext::new(crate::Operation::Join));
                self.events.push_back(flexaudio_core::Event::ShutdownError {
                    error: error.clone(),
                });
                cleanup.push(error);
            }
        }
        self.report = Some(crate::ShutdownReport::new(None, cleanup));
    }

    fn result(&self) -> Result<()> {
        let result = self
            .report
            .as_ref()
            .expect("completed mock shutdown")
            .result();
        if let Err(error) = &result {
            self.receipt.acknowledge_error(error);
        }
        result
    }

    fn poll_event(&mut self) -> Option<flexaudio_core::Event> {
        let event = self.events.pop_front()?;
        if let Some(error) = event.failure() {
            self.receipt.acknowledge_error(&error);
        }
        Some(event)
    }
}

#[cfg(test)]
pub(crate) fn drop_failure_fixtures() {
    macro_rules! exercise {
        ($construct:expr) => {{
            let mut backend = $construct;
            backend.handle = Some(thread::spawn(|| {
                panic!("injected implicit generator join failure")
            }));
            drop(backend);
        }};
    }
    exercise!(MockBackend::new(48_000, 2, 0.0));
    exercise!(StallableMockBackend::new(48_000, 2, 0.0, Duration::ZERO));
    exercise!(PanickingMockBackend::new(48_000, 2, 0.0, PanicMode::Stop));
    exercise!(StallThenPanicOnReopenBackend::new(
        48_000,
        2,
        0.0,
        Duration::ZERO
    ));
}

#[cfg(test)]
macro_rules! stop_fixture {
    ($name:ident, $backend:ty, $construct:expr) => {
        pub(crate) fn $name(observe: fn(&$backend)) {
            for checked_first in [false, true] {
                let mut backend: $backend = $construct;
                backend.handle = Some(thread::spawn(|| panic!("injected generator join failure")));
                if checked_first {
                    assert!(backend.stop_checked().is_err());
                } else {
                    backend.stop();
                }
                let report = backend
                    .shutdown
                    .report
                    .clone()
                    .expect("void mock stop retains report");
                assert!(report.primary().is_none());
                assert_eq!(report.cleanup().len(), 1);
                let cause = crate::Error::Backend("mock capture owner panicked".into())
                    .with_context(crate::ErrorContext::new(crate::Operation::Join));
                assert_eq!(report.cleanup(), std::slice::from_ref(&cause));
                assert_eq!(backend.stop_checked(), report.result());
                backend.stop();
                assert_eq!(backend.shutdown.report, Some(report.clone()));
                assert_eq!(backend.stop_checked(), report.result());
                observe(&backend);
            }
        }
    };
}

#[cfg(test)]
stop_fixture!(check_sine, MockBackend, MockBackend::new(48_000, 2, 0.0));
#[cfg(test)]
stop_fixture!(
    check_stallable,
    StallableMockBackend,
    StallableMockBackend::new(48_000, 2, 0.0, Duration::ZERO)
);
#[cfg(test)]
stop_fixture!(
    check_panicking,
    PanickingMockBackend,
    PanickingMockBackend::new(48_000, 2, 0.0, PanicMode::Start)
);
#[cfg(test)]
stop_fixture!(
    check_reopening,
    StallThenPanicOnReopenBackend,
    StallThenPanicOnReopenBackend::new(48_000, 2, 0.0, Duration::ZERO)
);

#[cfg(test)]
mod receipt_tests {
    use super::*;

    #[test]
    fn plain_stop_keeps_mock_failure_for_drop_but_typed_handoffs_do_not_repeat() {
        const CHILD: &str = "FLEXAUDIO_MOCK_SHUTDOWN_RECEIPT_TEST";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "mock::receipt_tests::plain_stop_keeps_mock_failure_for_drop_but_typed_handoffs_do_not_repeat",
                    "--exact",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let received = Arc::new(std::sync::Mutex::new(Vec::new()));
        let output = received.clone();
        crate::set_drop_failure_handler(Some(Box::new(move |failure| {
            output.lock().unwrap().push(failure.clone());
        })));
        for handoff in ["plain", "checked", "event"] {
            macro_rules! exercise {
                ($construct:expr) => {{
                    let mut backend = $construct;
                    backend.handle = Some(thread::spawn(|| panic!("injected mock join failure")));
                    backend.stop();
                    backend.stop();
                    match handoff {
                        "plain" => {}
                        "checked" => assert!(backend.stop_checked().is_err()),
                        "event" => assert!(matches!(
                            backend.poll_event(),
                            Some(flexaudio_core::Event::ShutdownError { .. })
                        )),
                        _ => unreachable!(),
                    }
                    drop(backend);
                    let failures = std::mem::take(&mut *received.lock().unwrap());
                    if handoff == "plain" {
                        assert_eq!(failures.len(), 1);
                        assert_eq!(failures[0].owner, crate::DropFailureOwner::MockCapture);
                        assert_eq!(failures[0].report.cleanup().len(), 1);
                        assert_eq!(
                            failures[0].report.cleanup()[0],
                            crate::Error::Backend("mock capture owner panicked".into())
                                .with_context(crate::ErrorContext::new(crate::Operation::Join))
                        );
                    } else {
                        assert!(failures.is_empty(), "{handoff}: {failures:?}");
                    }
                }};
            }
            exercise!(MockBackend::new(48_000, 2, 0.0));
            exercise!(StallableMockBackend::new(48_000, 2, 0.0, Duration::ZERO));
            exercise!(PanickingMockBackend::new(48_000, 2, 0.0, PanicMode::Start));
            exercise!(StallThenPanicOnReopenBackend::new(
                48_000,
                2,
                0.0,
                Duration::ZERO
            ));
        }
        // Restart the real generator with an in-memory sink; retirement must precede reset.
        for checked in [false, true] {
            let mut backend = MockBackend::new(48_000, 2, 0.0);
            backend.handle = Some(thread::spawn(|| panic!("injected old generator failure")));
            backend.stop();
            let snapshot = backend.shutdown.report.clone().unwrap();
            if checked {
                assert!(backend.stop_checked().is_err());
            }
            let (producer, _consumer) = flexaudio_core::raw_ring(128);
            backend.start(RawSink::new(producer, 48_000, 2)).unwrap();
            assert!(backend.shutdown.report.is_none());
            let failures = std::mem::take(&mut *received.lock().unwrap());
            if checked {
                assert!(failures.is_empty());
            } else {
                assert_eq!(failures.len(), 1);
                assert_eq!(failures[0].report, snapshot);
            }
            backend.stop_checked().unwrap();
            drop(backend);
            assert!(received.lock().unwrap().is_empty());
        }
        crate::set_drop_failure_handler(None);
    }
}
