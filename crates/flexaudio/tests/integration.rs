//! End-to-end verification of the flexaudio facade (driven by MockBackend, no hardware needed).
//!
//! Uses `MockBackend` (synthetic sine wave) to confirm that the whole wiring "backend → RawRing →
//! processing thread → Normalizer → ChunkRing → poll" works.

use std::time::{Duration, Instant};

use flexaudio::core::types::{AudioChunk, ChunkFlags, OutputFormat, StreamConfig};
use flexaudio::{MockBackend, Stream};

/// Helper that polls and collects chunks until a condition is met.
///
/// `done` receives all chunks collected so far and collection ends when it returns true. A fixed
/// wall-clock window ("collect for 500ms and N should arrive") cannot guarantee production
/// within the window when threads get descheduled under load and is inherently flaky, so it
/// "waits until the condition is reached" instead. `max_wait` is a hang guard so that it does
/// not run forever even under extreme load; when exceeded it returns what has been collected
/// (any shortfall is detected by the caller's assertions).
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

/// Wait limit for [`collect_until`]. In a normal environment it exits as soon as the condition
/// is reached, so this is only a hang guard for the case where "threads can barely run under
/// extreme load".
const COLLECT_MAX_WAIT: Duration = Duration::from_secs(30);

/// Minimum number of chunks accepted as "the pipeline actually flowed" (20ms × 10 = about 200ms
/// worth).
const MIN_CHUNKS: usize = 10;

/// mono 44100 input → normalized to stereo 48000 / 960frame chunks.
#[test]
fn mock_mono_44100_to_stereo_960_chunks() {
    let backend = Box::new(MockBackend::new(44100, 1, 440.0));
    let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");
    stream.start().expect("start");

    let chunks = collect_until(&mut stream, COLLECT_MAX_WAIT, |c| c.len() >= MIN_CHUNKS);
    stream.stop();

    assert!(
        chunks.len() >= MIN_CHUNKS,
        "not enough chunks arrived: {}",
        chunks.len()
    );
    for c in &chunks {
        assert_eq!(c.frames, 960, "not 20ms@48k = 960 frame");
        assert_eq!(c.data.len(), 960 * 2, "not stereo interleaved (960*2)");
    }
    // seq increases monotonically (the increase holds even if DROP_OLDEST happens).
    for w in chunks.windows(2) {
        assert!(
            w[1].seq > w[0].seq,
            "seq is not monotonically increasing: {} -> {}",
            w[0].seq,
            w[1].seq
        );
    }
}

/// 48000 stereo becomes 960frame chunks via the passthrough path.
#[test]
fn mock_passthrough_48000_stereo() {
    let backend = Box::new(MockBackend::new(48000, 2, 440.0));
    let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");
    stream.start().expect("start");

    let chunks = collect_until(&mut stream, COLLECT_MAX_WAIT, |c| c.len() >= MIN_CHUNKS);
    stream.stop();

    assert!(
        chunks.len() >= MIN_CHUNKS,
        "not enough chunks arrived: {}",
        chunks.len()
    );
    for c in &chunks {
        assert_eq!(c.frames, 960);
        assert_eq!(c.data.len(), 1920);
    }
}

/// Output {16000, 1}: 48k/stereo input → 320 frame, 320 sample (mono) chunks.
/// peak/rms are sane (non-zero for the synthetic sine wave, not far above 1.0).
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
        "not enough 16k/mono chunks arrived: {}",
        chunks.len()
    );
    for c in &chunks {
        assert_eq!(c.frames, 320, "not 16k 20ms = 320 frame");
        assert_eq!(c.data.len(), 320, "not mono interleaved (320*1)");
        // peak/rms sanity (synthetic sine wave amplitude 0.5).
        assert!(
            c.peak > 0.0 && c.peak <= 1.5,
            "peak is not sane: {}",
            c.peak
        );
        assert!(c.rms > 0.0 && c.rms <= 1.0, "rms is not sane: {}", c.rms);
        assert!(
            c.peak >= c.rms,
            "should be peak >= rms: peak={} rms={}",
            c.peak,
            c.rms
        );
    }
}

/// Output {16000, 2}: → 320 frame, 640 sample (stereo) chunks.
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
        "not enough 16k/stereo chunks arrived: {}",
        chunks.len()
    );
    for c in &chunks {
        assert_eq!(c.frames, 320, "not 16k 20ms = 320 frame");
        assert_eq!(c.data.len(), 640, "not stereo interleaved (320*2)");
        assert!(
            c.peak > 0.0 && c.peak <= 1.5,
            "peak is not sane: {}",
            c.peak
        );
        assert!(c.rms > 0.0 && c.rms <= 1.0, "rms is not sane: {}", c.rms);
    }
}

/// Regression for the default output {48000, 2}: frames==960 / data.len()==1920 / peak/rms sane.
#[test]
fn mock_default_output_regression_with_peak_rms() {
    let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
    let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");
    stream.start().expect("start");

    let chunks = collect_until(&mut stream, COLLECT_MAX_WAIT, |c| c.len() >= MIN_CHUNKS);
    stream.stop();

    assert!(
        chunks.len() >= MIN_CHUNKS,
        "not enough chunks arrived: {}",
        chunks.len()
    );
    for c in &chunks {
        assert_eq!(c.frames, 960);
        assert_eq!(c.data.len(), 1920);
        assert!(c.peak > 0.0 && c.peak <= 1.5, "peak: {}", c.peak);
        assert!(c.rms > 0.0 && c.rms <= 1.0, "rms: {}", c.rms);
    }
}

/// The unified `devices()` enumeration returns `Ok(Vec)` without panicking, and each DeviceInfo
/// satisfies its invariants (non-empty id / loopback consistent with source_kind / positive
/// rate and ch). In headless/CI environments there may be no devices and the Vec may be empty,
/// which is also fine (the point is not to panic).
#[test]
fn devices_enumeration_never_panics_and_is_consistent() {
    use flexaudio::core::types::SourceKind;

    let devices = flexaudio::devices().expect("devices() is designed never to return Err");
    for d in &devices {
        assert!(!d.id.is_empty(), "id (stable key) is non-empty");
        assert!(d.sample_rate > 0, "sample_rate is positive");
        assert!(d.channels > 0, "channels is positive");
        match d.source_kind {
            SourceKind::Mic => assert!(!d.is_loopback, "Mic is not loopback"),
            SourceKind::SystemLoopback => assert!(d.is_loopback, "SystemLoopback is loopback"),
            other => panic!("source_kind that devices() should never return: {other:?}"),
        }
    }
}

/// open → start → stop completes without hanging or panicking (soundness of the thread join).
#[test]
fn open_start_stop_is_clean() {
    let backend = Box::new(MockBackend::new(48000, 2, 440.0));
    let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");
    stream.start().expect("start");
    std::thread::sleep(Duration::from_millis(50));
    stream.stop();
}

/// Hot-swap e2e of the source (`switch_backend`, driven by MockBackend).
///
/// open+start with MockBackend(44100/mono/440Hz) → get a few chunks →
/// swap with `switch_backend(MockBackend(48000/stereo/220Hz))` → get a few more chunks.
/// Asserts:
/// 1. The seq of all chunks is 0,1,2,... with no gaps (switching does not touch seq).
/// 2. Flags are only within the allowed set (empty / DISCONTINUITY alone for the switch /
///    RECOVERED|DISCONTINUITY for automatic recovery). The switch marker (DISCONTINUITY alone)
///    appears at most once and at or after the switch boundary, and there is always at least
///    one discontinuity notification at or after the boundary.
/// 3. frames/data.len stay constant per the output before and after the switch (default 48k/2 →
///    960frame, 1920sample).
/// 4. No panic/breakage even after the first stage is rebuilt for 44100/mono → 48000/stereo.
/// 5. Backward steps of pts_ns do not exceed the structural upper bound (the re-anchoring width
///    for burst arrival).
#[test]
fn switch_backend_keeps_seq_continuous_and_flags_discontinuity() {
    // Predicate for the switch marker. An intentional switch sets only DISCONTINUITY, and by
    // design automatic recovery's RECOVERED is not set, so it is identified as "DISCONTINUITY
    // without RECOVERED" (a bug where the switch wrongly sets RECOVERED does not match this
    // predicate and is detected).
    fn is_switch_marker(c: &AudioChunk) -> bool {
        c.flags.contains(ChunkFlags::DISCONTINUITY) && !c.flags.contains(ChunkFlags::RECOVERED)
    }

    // Default output {48000, 2}: frames=960 / data.len=1920 must not change across the switch.
    //
    // With the default chunk ring of 50 (= 1 second worth), when only the poll side stalls for
    // long under load, DROP_OLDEST happens and the main verification, "seq is continuous",
    // becomes scheduler-dependent. MockBackend produces at real-time pace (≤50 chunks/s), and
    // the total duration of this test, even including the hang guard, is bounded by 2
    // collections × 30 seconds ≒ a little over 3,000 chunks, so the capacity is set to hold all
    // of them and dropping is made structurally impossible.
    let config = StreamConfig {
        ring_capacity_chunks: 4096,
        ..Default::default()
    };
    let backend = Box::new(MockBackend::new(44_100, 1, 440.0));
    let mut stream = Stream::open(config, backend).expect("open");
    stream.start().expect("start");

    // Native format before the switch (mono 44100).
    assert_eq!(stream.native_format(), (44_100, 1));

    // --- Collect chunks before the switch (wait until a reasonable number have arrived) ---
    let before = collect_until(&mut stream, COLLECT_MAX_WAIT, |c| c.len() >= MIN_CHUNKS);
    assert!(
        before.len() >= MIN_CHUNKS,
        "not enough chunks arrived before the switch: {}",
        before.len()
    );
    let before_count = before.len();

    // --- Hot-swap the source to 48000/stereo/220Hz ---
    let new_backend = Box::new(MockBackend::new(48_000, 2, 220.0));
    stream
        .switch_backend(new_backend)
        .expect("switch_backend should succeed");

    // The native format after the switch has been updated to the new source's values.
    assert_eq!(stream.native_format(), (48_000, 2));

    // --- Collect chunks after the switch ---
    // Wait until the discontinuity notification (DISCONTINUITY) is observed and a reasonable
    // number of chunks keep flowing after it (stopping the moment the notification appears
    // would not prove that "the stream continues after the switch"). It waits for any
    // DISCONTINUITY rather than only the switch marker because under extreme load the switch
    // notification can merge into an automatic-recovery chunk (see the comment on (2) for
    // details).
    let mut after = collect_until(&mut stream, COLLECT_MAX_WAIT, |c| {
        c.iter()
            .position(|chunk| chunk.flags.contains(ChunkFlags::DISCONTINUITY))
            .is_some_and(|pos| c.len() - (pos + 1) >= MIN_CHUNKS)
    });
    stream.stop();
    // After stop, drain what is left in the ring.
    while let Some(c) = stream.poll_chunk() {
        after.push(c);
    }
    assert!(!after.is_empty(), "no chunks arrived after the switch");

    // --- Concatenate all chunks in chronological order ---
    let mut all: Vec<AudioChunk> = Vec::with_capacity(before.len() + after.len());
    all.extend(before);
    all.extend(after);

    // (1) seq is 0,1,2,... with no gaps.
    for (i, c) in all.iter().enumerate() {
        assert_eq!(
            c.seq, i as u64,
            "seq is not continuous: seq {} at index {i} (gap)",
            c.seq
        );
    }

    // (2) Allowed-set verification of the flags (same idea as the "allowed set of values" in
    //     mix.rs: the scheduler can shift the amount and timing of chunks but cannot produce
    //     flags outside the set).
    //     The allowed ones are:
    //       - empty ... normal recording
    //       - DISCONTINUITY alone ... the intentional switch marker (by design RECOVERED is not
    //         set)
    //       - RECOVERED|DISCONTINUITY ... the watchdog's automatic recovery. It legitimately
    //         occurs when ingest stalls for more than 2 seconds under extreme load (not what
    //         this test verifies, but it can get mixed in)
    let recovery_flags = ChunkFlags::RECOVERED | ChunkFlags::DISCONTINUITY;
    for c in &all {
        assert!(
            c.flags.is_empty() || c.flags == ChunkFlags::DISCONTINUITY || c.flags == recovery_flags,
            "flags outside the allowed set (switch/recovery flagging is broken): seq={} flags={:?}",
            c.seq,
            c.flags
        );
    }
    //     The switch marker should be "exactly once, at or after the boundary", but when an
    //     automatic recovery overlaps right after the switch, the switch's DISCONTINUITY can
    //     merge into the recovery chunk (RECOVERED|DISCONTINUITY) and no standalone marker
    //     appears (both pending flags are OR-consumed by the same next chunk). So it is split
    //     into 3 checks that can be verified deterministically:
    //       (2a) the standalone marker appears at most once (2 or more means the switch
    //            implementation is broken)
    //       (2b) the standalone marker never appears before the switch boundary
    //            (= before_count)
    //       (2c) there is always at least one discontinuity notification (standalone or
    //            merged) at or after the boundary
    //     In a run with no automatic recovery at all (always the case in a normal environment),
    //     (2a)+(2c) and the allowed set fully pin it down to "exactly once, at or after the
    //     boundary, without RECOVERED".
    let marker_positions: Vec<usize> = all
        .iter()
        .enumerate()
        .filter(|(_, c)| is_switch_marker(c))
        .map(|(i, _)| i)
        .collect();
    assert!(
        marker_positions.len() <= 1,
        "the switch DISCONTINUITY is set more than once: positions={marker_positions:?}"
    );
    if let Some(&idx) = marker_positions.first() {
        assert!(
            idx >= before_count,
            "DISCONTINUITY is set before the switch: idx={idx} < before_count={before_count}"
        );
    }
    assert!(
        all[before_count..]
            .iter()
            .any(|c| c.flags.contains(ChunkFlags::DISCONTINUITY)),
        "no DISCONTINUITY at all at or after the switch boundary (the switch does not signal the \
         discontinuity)"
    );

    // (3)/(4) frames/data.len stay constant per the output in all chunks (48k/2 → 960frame,
    //         1920sample). Unchanged even when the switch rebuilds the first stage
    //         (44100/mono → 48000/stereo).
    for c in &all {
        assert_eq!(c.frames, 960, "frames is not 960: seq={}", c.seq);
        assert_eq!(
            c.data.len(),
            1920,
            "data.len is not 1920 (960*2): seq={}",
            c.seq
        );
    }

    // (5) Continuity of pts_ns. pts is extrapolated from "the anchor that re-pins arrival time
    //     to the audio position" (the normalizer's update_pts_anchor), so on burst arrival
    //     (ingest stalls under load and the accumulated raw samples are popped at once) it can
    //     step back slightly at the next re-anchor = strict monotonicity (non-decreasing)
    //     depends on the wall clock and cannot be asserted. However, the backward step is
    //     structurally bounded: a bulk pop is capped at the RawRing/scratch size of 48,000
    //     samples, i.e. at most 48000 / 44100 (mono) ≒ 1.09 seconds of audio (0.5 seconds at
    //     48k/stereo after the switch) + what the normalizer holds internally (1-2 chunks).
    //     Only backward steps beyond this bound (the kind of bug where the switch rewinds the
    //     clock origin) are detected.
    const MAX_PTS_BACKWARD_NS: i64 = 1_200_000_000;
    for w in all.windows(2) {
        assert!(
            w[1].pts_ns >= w[0].pts_ns - MAX_PTS_BACKWARD_NS,
            "pts_ns stepped back beyond the re-anchoring bound: {} -> {}",
            w[0].pts_ns,
            w[1].pts_ns
        );
    }
}

/// `switch_source` rejects a request to change the output format with InvalidArg (continuity
/// protection).
#[test]
fn switch_source_rejects_output_change() {
    use flexaudio::core::types::{Error, SourceKind};

    let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
    let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");
    stream.start().expect("start");

    // new_config that changes only the output.
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
        .expect_err("an output change should be rejected");
    assert!(
        matches!(err, Error::InvalidArg(_)),
        "should be InvalidArg: {err:?}"
    );

    stream.stop();
}

/// Calling `switch_backend` without start yields InvalidState (the branch that does not start
/// the backend).
#[test]
fn switch_backend_on_unstarted_is_invalid_state() {
    use flexaudio::core::types::Error;

    let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
    // Open but do not start.
    let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");

    let new_backend = Box::new(MockBackend::new(48_000, 2, 220.0));
    let err = stream
        .switch_backend(new_backend)
        .expect_err("should be InvalidState when not started");
    assert!(
        matches!(err, Error::InvalidState(_)),
        "should be InvalidState: {err:?}"
    );
    // Not started, so stop is a no-op (does not hang).
    stream.stop();
}

/// Calling `switch_source` without start yields InvalidState.
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
        .expect_err("should be InvalidState when not started");
    assert!(
        matches!(err, Error::InvalidState(_)),
        "should be InvalidState: {err:?}"
    );
    stream.stop();
}
