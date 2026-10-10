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
    // Clamp each canonical channel before averaging: (1.0 + 0.0) / 2 = 0.5.
    // Applying gain after averaging instead would incorrectly produce 0.8.
    let stereo: Vec<_> = (0..960).flat_map(|_| [0.8, 0.0]).collect();
    sink.lock().unwrap().as_mut().unwrap().push(&stereo, 0);
    let output = next_chunk(&mut stream);
    assert!(output.data.iter().all(|sample| *sample == 0.5));
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
