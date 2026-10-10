//! Producer-clock regressions driven exclusively by fake backends.
use super::*;
use std::time::Instant;

struct PushBackend(Arc<Mutex<Option<RawSink>>>);
impl CaptureBackend for PushBackend {
    fn native_format(&self) -> (u32, u16) {
        (48_000, 2)
    }
    fn start(&mut self, sink: RawSink) -> Result<()> {
        *self.0.lock().unwrap() = Some(sink);
        Ok(())
    }
    fn stop(&mut self) {
        self.0.lock().unwrap().take();
    }
}

fn wait(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !predicate() {
        assert!(Instant::now() < deadline, "fake capture timed out");
        thread::sleep(Duration::from_millis(2));
    }
}

fn next_chunk(stream: &mut Stream) -> AudioChunk {
    let mut chunk = None;
    wait(|| {
        chunk = stream.poll_chunk();
        chunk.is_some()
    });
    chunk.unwrap()
}

#[test]
fn attachment_preserves_output_pcm_and_metrics() {
    fn render(attached: bool) -> (Vec<f32>, f32, f32) {
        let sink = Arc::new(Mutex::new(None));
        let config = StreamConfig {
            gain: 2.0,
            output: OutputFormat {
                sample_rate: 16_000,
                channels: 1,
            },
            ..Default::default()
        };
        let mut stream = Stream::open(config, Box::new(PushBackend(sink.clone()))).unwrap();
        if attached {
            stream.enable_capture_tap().unwrap();
        }
        stream.start().unwrap();
        let stereo: Vec<_> = (0..2880).flat_map(|_| [0.8, 0.0]).collect();
        sink.lock().unwrap().as_mut().unwrap().push(&stereo, 0);
        let chunk = next_chunk(&mut stream);
        stream.stop();
        (chunk.data, chunk.peak, chunk.rms)
    }
    assert_eq!(render(true), render(false));
}

#[test]
fn rebuild_accounts_for_discarded_capture_remainder() {
    let sink = Arc::new(Mutex::new(None));
    let mut stream =
        Stream::open(StreamConfig::default(), Box::new(PushBackend(sink.clone()))).unwrap();
    stream.enable_capture_tap().unwrap();
    stream.start().unwrap();
    sink.lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .push(&[0.25; 973 * 2], 0);
    let _ = next_chunk(&mut stream);
    let mut first = None;
    wait(|| {
        first = stream.poll_capture();
        first.is_some()
    });
    assert_eq!(first.unwrap().frame_index, 0);
    stream
        .switch_backend(Box::new(PushBackend(sink.clone())))
        .unwrap();
    sink.lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .push(&[0.25; 960 * 2], 0);
    let mut next = None;
    wait(|| {
        next = stream.poll_capture();
        next.is_some()
    });
    stream.stop();
    assert_eq!(next.unwrap().frame_index, 973);
}

#[test]
fn empty_denoised_capture_has_no_samples_or_epoch() {
    let sink = Arc::new(Mutex::new(None));
    let mut stream = Stream::open(StreamConfig::default(), Box::new(PushBackend(sink))).unwrap();
    stream.set_denoise(true);
    stream.enable_capture_tap().unwrap();
    stream.start().unwrap();
    stream.stop();
    assert!(stream.poll_capture().is_none());
}

#[test]
fn zero_rate_frame_index_is_a_typed_error() {
    let counter = AtomicU64::new(0);
    assert!(matches!(
        advance_frame_index(&counter, 960, 0),
        Err(Error::InvalidArg(_))
    ));
    assert_eq!(counter.load(Ordering::SeqCst), 0);
}

#[test]
fn intake_uses_one_shared_drain_path() {
    let source = include_str!("stream.rs");
    let intake = source
        .split("fn run_intake(")
        .nth(1)
        .unwrap()
        .split("/// Producer-only advancement")
        .next()
        .unwrap();
    assert!(
        intake.lines().count() <= 250,
        "intake lines: {}",
        intake.lines().count()
    );
    assert_eq!(intake.matches("tap_drain::drain(").count(), 3);
}

#[test]
fn rust_chunk_struct_literal_migration_is_documented() {
    for document in [
        include_str!("../../../CHANGELOG.md"),
        include_str!("../../../README.md"),
    ] {
        assert!(
            document.contains("AudioChunk")
                && document.contains("SecondaryChunk")
                && document.contains("frame_index")
                && document.contains("struct literals"),
            "missing Rust chunk construction migration"
        );
    }
}

#[test]
fn switched_generation_applies_both_denoise_toggle_directions_to_initial_burst() {
    struct Burst;
    impl CaptureBackend for Burst {
        fn native_format(&self) -> (u32, u16) {
            (48_000, 2)
        }
        fn start(&mut self, mut sink: RawSink) -> Result<()> {
            sink.push(&[0.8; 1920], 0);
            Ok(())
        }
        fn stop(&mut self) {}
    }
    for enabled in [false, true] {
        let sink = Arc::new(Mutex::new(None));
        let mut stream = Stream::open(Default::default(), Box::new(PushBackend(sink))).unwrap();
        stream.set_denoise(!enabled);
        stream.enable_capture_tap().unwrap();
        stream.start().unwrap();
        stream
            .switch_backend_with_denoise(Box::new(Burst), enabled)
            .unwrap();
        let snapshot = stream.shared.snapshot_raw(&mut []);
        assert_eq!(snapshot.denoise_enabled, enabled);
        let mut capture = None;
        wait(|| {
            capture = stream.poll_capture();
            capture.is_some()
        });
        let capture = capture.unwrap();
        if enabled {
            assert_eq!(&capture.data[..960], &[0.0; 960]);
        } else {
            assert_eq!(capture.data, vec![0.8; 1920]);
        }
        stream.stop();
    }
}

#[test]
fn native_restart_and_switch_preserve_produced_frame_indices() {
    let sink = Arc::new(Mutex::new(None));
    let mut stream =
        Stream::open(StreamConfig::default(), Box::new(PushBackend(sink.clone()))).unwrap();
    stream.enable_capture_tap().unwrap();
    stream.start().unwrap();
    sink.lock().unwrap().as_mut().unwrap().push(&[0.0; 1920], 0);
    let first = next_chunk(&mut stream);
    assert_eq!(first.frame_index, 0);
    stream
        .switch_backend(Box::new(PushBackend(sink.clone())))
        .unwrap();
    sink.lock().unwrap().as_mut().unwrap().push(&[0.0; 1920], 0);
    let switched = next_chunk(&mut stream);
    assert_eq!(
        switched.frame_index,
        first.frame_index + first.frames as u64
    );
    assert!(switched.flags.contains(ChunkFlags::DISCONTINUITY));
    let capture: Vec<_> = std::iter::from_fn(|| stream.poll_capture()).collect();
    assert_eq!(capture.last().unwrap().frame_index, switched.frame_index);
    stream.stop();
}

#[test]
fn simulated_queue_drop_preserves_primary_and_canonical_gap_lengths() {
    let sink = Arc::new(Mutex::new(None));
    let config = StreamConfig {
        ring_capacity_chunks: 1,
        ..Default::default()
    };
    let mut stream = Stream::open(config, Box::new(PushBackend(sink.clone()))).unwrap();
    stream.enable_capture_tap().unwrap();
    stream.start().unwrap();
    sink.lock().unwrap().as_mut().unwrap().push(&[0.0; 1920], 0);
    let first = next_chunk(&mut stream);
    let mut first_capture = None;
    wait(|| {
        first_capture = stream.poll_capture();
        first_capture.is_some()
    });
    sink.lock().unwrap().as_mut().unwrap().push(&[0.0; 3840], 0);
    wait(|| stream.dropped_chunks() == 1);
    let next = next_chunk(&mut stream);
    let capture = stream.poll_capture().unwrap();
    assert_eq!(
        next.frame_index - first.frame_index - first.frames as u64,
        960
    );
    let first_capture = first_capture.unwrap();
    assert_eq!(
        capture.frame_index - first_capture.frame_index - first_capture.frames as u64,
        960
    );
    assert_eq!(next.dropped_before, 1);
    stream.stop();
}

#[test]
fn fractional_output_rates_retain_the_cumulative_rational_remainder() {
    let counter = AtomicU64::new(0);
    for chunk in 0..5000_u64 {
        let index = advance_frame_index(&counter, 882, 44_101).unwrap();
        assert_eq!(index, chunk * 882 * 48_000 / 44_101);
    }
}

#[test]
fn canonical_gain_is_applied_once_before_output_conversion() {
    let sink = Arc::new(Mutex::new(None));
    let config = StreamConfig {
        gain: 2.0,
        output: OutputFormat {
            sample_rate: 48_000,
            channels: 1,
        },
        ..Default::default()
    };
    let mut stream = Stream::open(config, Box::new(PushBackend(sink.clone()))).unwrap();
    stream.enable_capture_tap().unwrap();
    stream.start().unwrap();
    // Output preserves legacy gain after averaging: ((0.8 + 0.0) / 2) * 2 = 0.8.
    // The independent capture copy clamps each channel before VAD conversion.
    let stereo: Vec<_> = (0..960).flat_map(|_| [0.8, 0.0]).collect();
    sink.lock().unwrap().as_mut().unwrap().push(&stereo, 0);
    let output = next_chunk(&mut stream);
    assert!(output.data.iter().all(|sample| *sample == 0.8));
    let mut capture = None;
    wait(|| {
        capture = stream.poll_capture();
        capture.is_some()
    });
    let capture = capture.unwrap();
    assert!(capture
        .data
        .chunks_exact(2)
        .all(|frame| frame == [1.0, 0.0]));
    assert_eq!(capture.frame_index, output.frame_index);
    stream.stop();
}

#[test]
fn capture_tail_excludes_transport_padding() {
    let sink = Arc::new(Mutex::new(None));
    let mut stream =
        Stream::open(StreamConfig::default(), Box::new(PushBackend(sink.clone()))).unwrap();
    stream.enable_capture_tap().unwrap();
    stream.start().unwrap();
    sink.lock().unwrap().as_mut().unwrap().push(&[0.25; 26], 0);
    wait(|| stream.shared.last_sample_ns.load(Ordering::SeqCst) != 0);
    stream.stop();
    let output = stream.poll_chunk().unwrap();
    let capture = stream.poll_capture().unwrap();
    assert_eq!(output.frames, 960);
    assert_eq!(capture.frames, 13);
    assert_eq!(capture.data, vec![0.25; 26]);
    assert_eq!(capture.frame_index, output.frame_index);
    assert!(stream.poll_capture().is_none());
}

#[test]
fn exhausted_frame_index_fails_without_wrapping_or_advancing() {
    let counter = AtomicU64::new(u64::MAX - 959);
    assert!(advance_frame_index(&counter, 960, 48_000).is_err());
    assert_eq!(counter.load(Ordering::SeqCst), u64::MAX - 959);
}

#[test]
fn capture_loss_reanchors_pts_without_inserting_timeline_silence() {
    let sink = Arc::new(Mutex::new(None));
    let config = StreamConfig {
        ring_capacity_chunks: 200,
        ..Default::default()
    };
    let mut stream = Stream::open(config, Box::new(PushBackend(sink.clone()))).unwrap();
    stream.enable_capture_tap().unwrap();
    stream.start().unwrap();
    sink.lock().unwrap().as_mut().unwrap().push(&[0.0; 1920], 0);
    let first = next_chunk(&mut stream);
    let mut before = None;
    wait(|| {
        before = stream.poll_capture();
        before.is_some()
    });
    let before = before.unwrap();
    thread::sleep(Duration::from_millis(50));
    let burst = vec![0.0; RAW_RING_SAMPLES * 2];
    let accepted = sink.lock().unwrap().as_mut().unwrap().push(&burst, 0);
    assert!(
        accepted < burst.len(),
        "the fake callback must overflow its ring"
    );
    let after = next_chunk(&mut stream);
    let mut capture = None;
    wait(|| {
        capture = stream.poll_capture();
        capture.is_some()
    });
    let capture = capture.unwrap();
    assert_eq!(after.frame_index, first.frame_index + first.frames as u64);
    assert_eq!(
        capture.frame_index,
        before.frame_index + before.frames as u64
    );
    assert!(capture.flags.contains(ChunkFlags::DISCONTINUITY));
    assert!(capture.pts_ns - before.pts_ns >= 40_000_000);
    stream.stop();
}
