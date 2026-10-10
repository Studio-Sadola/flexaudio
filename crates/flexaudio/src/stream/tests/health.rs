//! Offline health regressions.
use super::*;

// --- Watchdog: detect stall → auto-recover → RECOVERED ---

/// End-to-end verification with a bounded burst: the first session stops receiving data,
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
    struct WholeChunkBurst;
    impl CaptureBackend for WholeChunkBurst {
        fn native_format(&self) -> (u32, u16) {
            (48_000, 2)
        }
        fn start(&mut self, mut sink: RawSink) -> Result<()> {
            // Exactly 300 ms in whole chunks. A wall-clock limited 10 ms producer
            // can leave a half-chunk at recovery when the test runner is busy.
            // This fixture tests recovery without that independent remainder gap.
            sink.push(&[0.25; 15 * 960 * 2], 0);
            Ok(())
        }
        fn stop(&mut self) {}
    }
    let backend = Box::new(WholeChunkBurst);
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

// --- Robustness: backend panics do not cause silent death (prevent poison-related panic cascades) ---
//
// These tests prove there is no silent death or panic cascade by verifying that the test process
// itself does not panic (a panic would make the test result FAILED). They also assert that the
// panic is observable as Err / RecoverableError, so it is not merely swallowed and hidden.

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
/// panic cascade; the failure is surfaced as typed `RecoverableError` with Reopen context. The process
/// stays alive because catch_unwind prevents mutex poisoning and the reopen failure becomes
/// RecoverableError through `open_backend_once`'s Err.
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

    // Wait long enough for stall detection (>=2s) and a reopen attempt (panic → RecoverableError), up to 8 s.
    let mut saw_stalled = false;
    let mut saw_reopen_error = false;
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline && !saw_reopen_error {
        // Also call poll_chunk so a full ring does not block other paths.
        while stream.poll_chunk().is_some() {}
        while let Some(ev) = stream.poll_event() {
            match ev {
                Event::StreamStalled => saw_stalled = true,
                Event::RecoverableError {
                    error: Error::Context { context, .. },
                } if context.operation() == Operation::Reopen => {
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
        "backend panic during reopen should surface as RecoverableError with Reopen context \
             (no silent death)"
    );
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
