//! Offline output regressions.
use super::*;

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
