//! End-to-end tests for the flexaudio facade (driven by MockBackend; no hardware required).
//!
//! Use `MockBackend` (a generated sine wave) to verify the full pipeline:
//! backend → RawRing → processing thread → Normalizer → ChunkRing → poll.

use std::time::{Duration, Instant};

use flexaudio::core::types::{AudioChunk, ChunkFlags, OutputFormat, StreamConfig};
use flexaudio::{MockBackend, Stream};

/// Helper that polls and collects chunks until a condition is met.
///
/// `done` receives all collected chunks and ends collection when it returns true. Fixed
/// wall-clock windows ("collect for 500 ms and expect N items") are inherently flaky because
/// load can deschedule threads and prevent a guaranteed production rate. Instead, wait until
/// the condition is met. `max_wait` prevents hangs under extreme load; on timeout, return the
/// chunks collected so far (the caller's assertion detects any shortfall).
fn collect_until(
    stream: &mut Stream,
    max_wait: Duration,
    mut done: impl FnMut(&[AudioChunk]) -> bool,
) -> Vec<AudioChunk> {
    let mut chunks = Vec::new();
    let start = Instant::now();
    loop {
        while let Some(c) = stream.poll_chunk() {
            chunks.push(c);
        }
        if done(&chunks) || start.elapsed() >= max_wait {
            return chunks;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Wait limit for [`collect_until`]. Under normal conditions, it exits as soon as the
/// condition is met; this only prevents hangs if extreme load leaves threads almost no runtime.
const COLLECT_MAX_WAIT: Duration = Duration::from_secs(30);

/// Minimum chunks required to confirm that the pipeline is flowing (20 ms × 10 = about 200 ms).
const MIN_CHUNKS: usize = 10;

/// Mono 44100 input is normalized to stereo 48000 / 960-frame chunks.
#[test]
fn mock_mono_44100_to_stereo_960_chunks() {
    let backend = Box::new(MockBackend::new(44100, 1, 440.0));
    let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");
    stream.start().expect("start");

    let chunks = collect_until(&mut stream, COLLECT_MAX_WAIT, |c| c.len() >= MIN_CHUNKS);
    stream.stop();

    assert!(
        chunks.len() >= MIN_CHUNKS,
        "too few chunks arrived: {}",
        chunks.len()
    );
    for c in &chunks {
        assert_eq!(c.frames, 960, "20 ms at 48 kHz is 960 frames");
        assert_eq!(c.data.len(), 960 * 2, "stereo interleaved length is 960*2");
    }
    // seq increases monotonically, even when DROP_OLDEST occurs.
    for w in chunks.windows(2) {
        assert!(
            w[1].seq > w[0].seq,
            "seq is not monotonically increasing: {} -> {}",
            w[0].seq,
            w[1].seq
        );
    }
}

/// 48000 stereo uses the pass-through path and produces 960-frame chunks.
#[test]
fn mock_passthrough_48000_stereo() {
    let backend = Box::new(MockBackend::new(48000, 2, 440.0));
    let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");
    stream.start().expect("start");

    let chunks = collect_until(&mut stream, COLLECT_MAX_WAIT, |c| c.len() >= MIN_CHUNKS);
    stream.stop();

    assert!(
        chunks.len() >= MIN_CHUNKS,
        "too few chunks arrived: {}",
        chunks.len()
    );
    for c in &chunks {
        assert_eq!(c.frames, 960);
        assert_eq!(c.data.len(), 1920);
    }
}

/// Output {16000, 1}: 48 kHz stereo input produces 320-frame, 320-sample mono chunks.
/// peak/rms are valid (nonzero for a generated sine wave and not much greater than 1.0).
#[test]
fn mock_output_16k_mono() {
    let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
    let config = StreamConfig {
        output: OutputFormat {
            sample_rate: 16_000,
            channels: 1,
        },
        ..Default::default()
    };
    let mut stream = Stream::open(config, backend).expect("open");
    stream.start().expect("start");

    let chunks = collect_until(&mut stream, COLLECT_MAX_WAIT, |c| c.len() >= MIN_CHUNKS);
    stream.stop();

    assert!(
        chunks.len() >= MIN_CHUNKS,
        "too few 16 kHz mono chunks arrived: {}",
        chunks.len()
    );
    for c in &chunks {
        assert_eq!(c.frames, 320, "16 kHz 20 ms is 320 frames");
        assert_eq!(c.data.len(), 320, "mono interleaved length is 320*1");
        // peak/rms validity (generated sine wave amplitude 0.5).
        assert!(
            c.peak > 0.0 && c.peak <= 1.5,
            "peak is out of range: {}",
            c.peak
        );
        assert!(
            c.rms > 0.0 && c.rms <= 1.0,
            "rms is out of range: {}",
            c.rms
        );
        assert!(
            c.peak >= c.rms,
            "peak should be >= rms: peak={} rms={}",
            c.peak,
            c.rms
        );
    }
}

/// Output {16000, 2}: produces 320-frame, 640-sample stereo chunks.
#[test]
fn mock_output_16k_stereo() {
    let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
    let config = StreamConfig {
        output: OutputFormat {
            sample_rate: 16_000,
            channels: 2,
        },
        ..Default::default()
    };
    let mut stream = Stream::open(config, backend).expect("open");
    stream.start().expect("start");

    let chunks = collect_until(&mut stream, COLLECT_MAX_WAIT, |c| c.len() >= MIN_CHUNKS);
    stream.stop();

    assert!(
        chunks.len() >= MIN_CHUNKS,
        "too few 16 kHz stereo chunks arrived: {}",
        chunks.len()
    );
    for c in &chunks {
        assert_eq!(c.frames, 320, "16 kHz 20 ms is 320 frames");
        assert_eq!(c.data.len(), 640, "stereo interleaved length is 320*2");
        assert!(
            c.peak > 0.0 && c.peak <= 1.5,
            "peak is out of range: {}",
            c.peak
        );
        assert!(
            c.rms > 0.0 && c.rms <= 1.0,
            "rms is out of range: {}",
            c.rms
        );
    }
}

/// Regression for default output {48000, 2}: frames==960 / data.len()==1920 / valid peak/rms.
#[test]
fn mock_default_output_regression_with_peak_rms() {
    let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
    let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");
    stream.start().expect("start");

    let chunks = collect_until(&mut stream, COLLECT_MAX_WAIT, |c| c.len() >= MIN_CHUNKS);
    stream.stop();

    assert!(
        chunks.len() >= MIN_CHUNKS,
        "too few chunks arrived: {}",
        chunks.len()
    );
    for c in &chunks {
        assert_eq!(c.frames, 960);
        assert_eq!(c.data.len(), 1920);
        assert!(c.peak > 0.0 && c.peak <= 1.5, "peak: {}", c.peak);
        assert!(c.rms > 0.0 && c.rms <= 1.0, "rms: {}", c.rms);
    }
}

/// Integrated `devices()` enumeration returns a complete inventory or a discovery error, and every
/// DeviceInfo satisfies its invariants (nonempty id / loopback matches source_kind / positive
/// rate and channel count). A completed empty inventory is valid. Headless/CI environments
/// without PipeWire return a backend error with Enumerate context, never partial success.
#[test]
fn devices_enumeration_never_panics_and_is_consistent() {
    use flexaudio::{Error, ErrorKind, Operation, SourceKind};

    let devices = match flexaudio::devices() {
        Ok(devices) => devices,
        Err(error) => {
            assert!(matches!(
                error.root().kind(),
                ErrorKind::Backend | ErrorKind::Unsupported | ErrorKind::PermissionDenied
            ));
            if error.kind() == ErrorKind::Backend {
                let Error::Context { source, context } = &error else {
                    panic!("discovery failure must retain Enumerate context");
                };
                assert_eq!(context.operation(), Operation::Enumerate);
                assert!(
                    matches!(source.as_ref(), Error::Backend(_)),
                    "discovery context must be attached once"
                );
            }
            return;
        }
    };
    for d in &devices {
        assert!(!d.id.is_empty(), "id (stable key) is nonempty");
        assert!(d.sample_rate > 0, "sample_rate is positive");
        assert!(d.channels > 0, "channels is positive");
        match d.source_kind {
            SourceKind::Mic => assert!(!d.is_loopback, "Mic is not loopback"),
            SourceKind::SystemLoopback => assert!(d.is_loopback, "SystemLoopback is loopback"),
            other => panic!("unexpected source_kind from devices(): {other:?}"),
        }
    }
}

/// open → start → stop completes without hanging or panicking (thread join is sound).
#[test]
fn open_start_stop_is_clean() {
    let backend = Box::new(MockBackend::new(48000, 2, 440.0));
    let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");
    stream.start().expect("start");
    std::thread::sleep(Duration::from_millis(50));
    stream.stop();
}

/// End-to-end source hot swap (`switch_backend`, driven by MockBackend).
///
/// Open and start with MockBackend(44100/mono/440Hz), collect several chunks, swap to
/// `switch_backend(MockBackend(48000/stereo/220Hz))`, then collect more chunks. Assert:
/// 1. Every chunk seq is contiguous from 0 (swapping does not change seq).
/// 2. Flags are limited to empty / DISCONTINUITY alone for a swap / RECOVERED|DISCONTINUITY
///    for watchdog recovery. The swap marker appears at most once and no earlier than the
///    boundary; at least one discontinuity notification appears at or after the boundary.
/// 3. frames/data.len remain fixed at output format (default 48k/2 -> 960 frames/1920 samples).
/// 4. Rebuilding the first stage for 44100/mono -> 48000/stereo does not panic or break.
/// 5. pts_ns regression stays within the structural bound (burst-arrival re-anchor window).
#[test]
fn switch_backend_keeps_seq_continuous_and_flags_discontinuity() {
    // Predicate for the swap marker. Intentional swaps set only DISCONTINUITY, not RECOVERED,
    // so identify it as DISCONTINUITY without RECOVERED. A bug that sets RECOVERED on a swap
    // will not match this predicate and is detected.
    fn is_switch_marker(c: &AudioChunk) -> bool {
        c.flags.contains(ChunkFlags::DISCONTINUITY) && !c.flags.contains(ChunkFlags::RECOVERED)
    }

    // Default output {48000, 2}: frames=960 / data.len=1920 remain unchanged across the swap.
    //
    // With default ring capacity 50 (=1 second), load can stall only the poll thread long
    // enough to trigger DROP_OLDEST, making the main contiguous-seq check scheduler-dependent.
    // MockBackend produces in real time (≤50 chunks/s); even with the hang guard, two
    // 30-second collection windows cap this test at just over 3,000 chunks. Set capacity high
    // enough to hold all of them and make drops structurally impossible.
    let config = StreamConfig {
        ring_capacity_chunks: 4096,
        ..Default::default()
    };
    let backend = Box::new(MockBackend::new(44_100, 1, 440.0));
    let mut stream = Stream::open(config, backend).expect("open");
    stream.start().expect("start");

    // Native format before the swap (mono 44100).
    assert_eq!(stream.native_format(), (44_100, 1));

    // --- Collect chunks before the swap (wait until enough have arrived) ---
    let before = collect_until(&mut stream, COLLECT_MAX_WAIT, |c| c.len() >= MIN_CHUNKS);
    assert!(
        before.len() >= MIN_CHUNKS,
        "too few chunks arrived before the swap: {}",
        before.len()
    );
    let before_count = before.len();

    // --- Hot-swap source to 48000/stereo/220Hz ---
    let new_backend = Box::new(MockBackend::new(48_000, 2, 220.0));
    stream
        .switch_backend(new_backend)
        .expect("switch_backend should succeed");

    // Native format after the swap matches the new source.
    assert_eq!(stream.native_format(), (48_000, 2));

    // --- Collect chunks after the swap ---
    // Wait until a discontinuity notification (DISCONTINUITY) is observed and chunks continue
    // afterward; stopping at the notification alone would not prove the stream continues.
    // Wait for any DISCONTINUITY rather than only the swap marker because, under extreme load,
    // the swap notification can merge into a watchdog-recovery chunk (see comment (2)).
    let mut after = collect_until(&mut stream, COLLECT_MAX_WAIT, |c| {
        c.iter()
            .position(|chunk| chunk.flags.contains(ChunkFlags::DISCONTINUITY))
            .is_some_and(|pos| c.len() - (pos + 1) >= MIN_CHUNKS)
    });
    stream.stop();
    // Drain the ring after stop.
    while let Some(c) = stream.poll_chunk() {
        after.push(c);
    }
    assert!(!after.is_empty(), "no chunks arrived after the swap");

    // --- Concatenate all chunks in chronological order ---
    let mut all: Vec<AudioChunk> = Vec::with_capacity(before.len() + after.len());
    all.extend(before);
    all.extend(after);

    // (1) seq is contiguous from 0.
    for (i, c) in all.iter().enumerate() {
        assert_eq!(
            c.seq, i as u64,
            "seq is not contiguous: index {i} has seq {} (gap)",
            c.seq
        );
    }

    // (2) Validate the allowed flag set (as in mix.rs): the scheduler can affect chunk quantity
    //     and timing but cannot produce flags outside the set. Allowed values:
    //       - empty: normal capture
    //       - DISCONTINUITY alone: intentional swap marker (RECOVERED is not set)
    //       - RECOVERED|DISCONTINUITY: watchdog recovery, valid if extreme load stalls ingest
    //         for over 2 seconds (not tested here, but may occur).
    let recovery_flags = ChunkFlags::RECOVERED | ChunkFlags::DISCONTINUITY;
    for c in &all {
        assert!(
            c.flags.is_empty() || c.flags == ChunkFlags::DISCONTINUITY || c.flags == recovery_flags,
            "flag outside allowed set (swap/recovery flagging is broken): seq={} flags={:?}",
            c.seq,
            c.flags
        );
    }
    //     Normally the swap marker appears exactly once at or after the boundary. If watchdog
    //     recovery overlaps the swap, its DISCONTINUITY may merge into a recovery chunk
    //     (RECOVERED|DISCONTINUITY), so the standalone marker may not appear (both pending flags
    //     are ORed into the next chunk). Split this into three deterministic checks:
    //       (2a) at most one standalone marker (more means swap logic is broken)
    //       (2b) no standalone marker before the swap boundary (= before_count)
    //       (2c) at least one discontinuity notification (standalone or merged) at/after boundary
    //     When no watchdog recovery occurs (normal case), (2a)+(2c) and the allowed flag set
    //     establish exactly one marker at/after the boundary with no RECOVERED flag.
    let marker_positions: Vec<usize> = all
        .iter()
        .enumerate()
        .filter(|(_, c)| is_switch_marker(c))
        .map(|(i, _)| i)
        .collect();
    assert!(
        marker_positions.len() <= 1,
        "swap DISCONTINUITY was set multiple times: positions={marker_positions:?}"
    );
    if let Some(&idx) = marker_positions.first() {
        assert!(
            idx >= before_count,
            "DISCONTINUITY was set before the swap: idx={idx} < before_count={before_count}"
        );
    }
    assert!(
        all[before_count..]
            .iter()
            .any(|c| c.flags.contains(ChunkFlags::DISCONTINUITY)),
        "no DISCONTINUITY at or after the swap boundary (swap did not report discontinuity)"
    );

    // (3)/(4) Every chunk keeps the output frames/data.len (48k/2 -> 960 frames/1920 samples),
    //         even when the first stage is rebuilt for 44100/mono -> 48000/stereo.
    for c in &all {
        assert_eq!(c.frames, 960, "frames is not 960: seq={}", c.seq);
        assert_eq!(
            c.data.len(),
            1920,
            "data.len is not 1920 (960*2): seq={}",
            c.seq
        );
    }

    // (5) pts_ns continuity. PTS is extrapolated from an anchor that maps arrival time to
    //     audio position (normalizer's update_pts_anchor), so burst arrivals (ingest stalls
    //     under load and buffered raw samples are popped at once) can cause a small regression
    //     at the next re-anchor. Strict monotonicity cannot be asserted because it depends on
    //     wall-clock timing, but regression is structurally bounded: a single pop is capped at
    //     48,000 RawRing/scratch samples, at most 48000 / 44100 (mono) ≈ 1.09 seconds (or 0.5 s
    //     for 48k/stereo after the swap), plus 1-2 chunks held by the normalizer. Detect only
    //     regressions beyond this bound (such as a bug that rewinds the clock origin on swap).
    const MAX_PTS_BACKWARD_NS: i64 = 1_200_000_000;
    for w in all.windows(2) {
        assert!(
            w[1].pts_ns >= w[0].pts_ns - MAX_PTS_BACKWARD_NS,
            "pts_ns regressed beyond the re-anchor bound: {} -> {}",
            w[0].pts_ns,
            w[1].pts_ns
        );
    }
}

/// `switch_source` rejects output-format changes with InvalidArg (preserves continuity).
#[test]
fn switch_source_rejects_output_change() {
    use flexaudio::core::types::{Error, SourceKind};

    let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
    let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");
    stream.start().expect("start");

    // new_config changes only output.
    let new_config = StreamConfig {
        kind: SourceKind::Mic,
        output: OutputFormat {
            sample_rate: 16_000,
            channels: 1,
        },
        ..Default::default()
    };
    let err = stream
        .switch_source(new_config)
        .expect_err("output change should be rejected");
    assert!(
        matches!(err, Error::InvalidArg(_)),
        "expected InvalidArg: {err:?}"
    );

    stream.stop();
}

/// Calling `switch_backend` before start returns InvalidState (does not start the backend).
#[test]
fn switch_backend_on_unstarted_is_invalid_state() {
    use flexaudio::core::types::Error;

    let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
    // Open but do not start.
    let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");

    let new_backend = Box::new(MockBackend::new(48_000, 2, 220.0));
    let err = stream
        .switch_backend(new_backend)
        .expect_err("before start, expected InvalidState");
    assert!(
        matches!(err, Error::InvalidState(_)),
        "expected InvalidState: {err:?}"
    );
    // stop is a no-op because the stream was not started (does not hang).
    stream.stop();
}

/// Calling `switch_source` before start returns InvalidState.
#[test]
fn switch_source_on_unstarted_is_invalid_state() {
    use flexaudio::core::types::{Error, SourceKind};

    let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
    let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");

    let new_config = StreamConfig {
        kind: SourceKind::Mic,
        ..Default::default()
    };
    let err = stream
        .switch_source(new_config)
        .expect_err("before start, expected InvalidState");
    assert!(
        matches!(err, Error::InvalidState(_)),
        "expected InvalidState: {err:?}"
    );
    stream.stop();
}
