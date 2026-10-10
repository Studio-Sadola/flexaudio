//! Offline pause regressions.
use super::*;

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
