//! Device-free regressions through real intake, gain, pause, stop and replacement paths.
use super::*;
use std::time::Instant;

type Sink = Arc<Mutex<Option<RawSink>>>;
struct Input(Sink);
impl CaptureBackend for Input {
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

fn wait(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !condition() {
        assert!(Instant::now() < deadline, "level fixture timed out");
        thread::sleep(Duration::from_millis(1));
    }
}

fn push(sink: &Sink, value: f32) {
    assert_eq!(
        sink.lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .push(&[value; 5760], 0),
        5760
    );
}

fn fixture() -> (Stream, Sink) {
    let sink = Sink::default();
    let mut config = StreamConfig::default();
    config.secondary_output = Some(OutputFormat {
        sample_rate: 16_000,
        channels: 1,
    });
    let mut stream = Stream::open(config, Box::new(Input(sink.clone()))).unwrap();
    stream.enable_capture_tap().unwrap();
    assert!(stream.poll_level().is_none());
    stream.start().unwrap();
    (stream, sink)
}

fn drain_inflight_primary(stream: &Stream, generation: u64) {
    let format = OutputFormat::default();
    let mut normalizer = Normalizer::new(48_000, 2, format).unwrap();
    normalizer.push(&[0.875; 1920], 0).unwrap();
    let (mut producer, _consumer) = chunk_ring(4);
    let mut state = tap_drain::TapState {
        generation,
        ..Default::default()
    };
    tap_drain::drain(
        &stream.shared,
        || normalizer.pop_chunk_with_metadata(),
        format,
        1.0,
        &AtomicU64::new(0),
        &mut state,
        tap_drain::Ring::Primary(&mut producer),
    )
    .unwrap();
}

#[test]
fn paused_level_keeps_up_without_publishing_pcm_or_vad_input() {
    let (mut stream, sink) = fixture();
    push(&sink, 0.25);
    wait(|| stream.shared.primary_frame_index.load(Ordering::SeqCst) >= 2880);
    stream.pause();
    // The existing contract permits draining pre-pause queues on every tap.
    while stream.poll_chunk().is_some() {}
    while stream.poll_secondary().is_some() {}
    while stream.poll_capture().is_some() {}
    stream.set_gain(2.0).unwrap();
    push(&sink, 0.375);
    wait(|| stream.poll_level().is_some_and(|v| v.peak == 0.75));
    assert!(stream.is_paused());
    let shared = stream.shared.clone();
    let delivery = shared.delivery.lock().unwrap();
    let a = stream.poll_level().unwrap();
    let b = stream.level_reader().poll_level().unwrap().unwrap();
    assert_eq!((a.peak, a.rms), (0.75, 0.75));
    assert_eq!((a.peak, a.rms), (b.peak, b.rms));
    assert!(b.age_ns >= a.age_ns);
    drop(delivery);
    for _ in 0..100 {
        assert!(stream.poll_level().is_some());
        assert!(stream.poll_chunk().is_none());
        assert!(stream.poll_secondary().is_none());
        assert!(
            stream.poll_capture().is_none(),
            "paused PCM must not feed attached VAD"
        );
    }
    stream.set_gain(0.0).unwrap();
    push(&sink, 0.75);
    wait(|| {
        stream
            .poll_level()
            .is_some_and(|v| v.peak == 0.0 && v.rms == 0.0)
    });
    stream.resume().unwrap();
    push(&sink, 0.5);
    let mut resumed = None;
    wait(|| {
        resumed = stream.poll_chunk();
        resumed.is_some()
    });
    assert!(resumed.unwrap().flags.contains(ChunkFlags::DISCONTINUITY));
    stream.stop_checked().unwrap();
    assert!(stream.poll_level().is_none());
}

#[test]
fn levels_match_denoised_primary_pcm_and_clipping() {
    for denoise in [false, true] {
        let (mut stream, sink) = fixture();
        // Rebuild as a generation, just like a source switch applies denoise.
        stream
            .switch_backend_with_denoise(Box::new(Input(sink.clone())), denoise)
            .unwrap();
        stream.set_gain(4.0).unwrap();
        push(&sink, 0.5);
        wait(|| stream.poll_level().is_some());
        // Finish consumption before comparing to the most recently published chunk.
        wait(|| {
            stream
                .shared
                .raw_consumer
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .available()
                == 0
        });
        let shared = stream.shared.clone();
        let delivery = shared.delivery.lock().unwrap();
        let mut last = None;
        // Read the real output ring under its publication lock; poll_chunk would
        // reacquire delivery. This freezes the chunk/level comparison as one boundary.
        while let Some(c) = stream.chunk_consumer.try_pop() {
            last = Some(c);
        }
        let level = stream.poll_level().unwrap();
        let chunk = last.unwrap();
        assert_eq!((level.peak, level.rms), (chunk.peak, chunk.rms));
        if !denoise {
            assert_eq!((level.peak, level.rms), (1.0, 1.0));
        }
        drop(delivery);
        stream.stop_checked().unwrap();
        assert!(
            stream.poll_level().is_none(),
            "stop tails must not refresh the meter"
        );
    }
}

#[test]
fn level_cell_rejects_inflight_old_generation_and_stopped_publication() {
    let (mut stream, sink) = fixture();
    push(&sink, 0.25);
    wait(|| stream.poll_level().is_some());
    let old_generation = stream.shared.raw_generation.load(Ordering::SeqCst);
    let new_sink = Sink::default();
    stream
        .switch_backend(Box::new(Input(new_sink.clone())))
        .unwrap();
    assert!(stream.poll_level().is_none());
    drain_inflight_primary(&stream, old_generation);
    assert!(
        stream.poll_level().is_none(),
        "old worker result must not survive a switch"
    );
    push(&new_sink, 0.5);
    wait(|| stream.poll_level().is_some_and(|v| v.peak == 0.5));
    let previous = stream.shared.raw_generation.load(Ordering::SeqCst);
    Stream::open_backend_once(&stream.shared, GenerationChange::Recovery).unwrap();
    drain_inflight_primary(&stream, previous);
    assert!(
        stream.poll_level().is_none(),
        "old worker result must not survive recovery"
    );
    push(&new_sink, 0.625);
    wait(|| stream.poll_level().is_some_and(|v| v.peak == 0.625));
    // The JS stop request closes the reader before the bridge stops intake.
    stream.level_reader().close();
    assert!(!stream.shared.stopping.load(Ordering::SeqCst));
    drain_inflight_primary(&stream, stream.shared.raw_generation.load(Ordering::SeqCst));
    assert!(
        stream.poll_level().is_none(),
        "adapter stop must reject in-flight levels"
    );
    stream.stop_checked().unwrap();
    drain_inflight_primary(&stream, stream.shared.raw_generation.load(Ordering::SeqCst));
    assert!(stream.poll_level().is_none());
}

#[test]
fn transient_device_loss_preserves_level_but_terminal_failure_closes_it() {
    let (mut stream, sink) = fixture();
    push(&sink, 0.25);
    wait(|| stream.poll_level().is_some());
    stream.shared.push_event(Event::DeviceLost);
    assert!(stream.poll_level().is_some());
    stream.shared.fail_terminal(Error::Unsupported);
    assert!(stream.poll_level().is_none());
    assert!(matches!(
        stream.level_reader().poll_level(),
        Err(Error::Unsupported)
    ));
    assert!(stream.stop_checked().is_err());
}

#[test]
fn level_readers_never_observe_a_torn_pair() {
    let reader = LevelReader::default();
    reader.start();
    let writer = reader.clone();
    let worker = thread::spawn(move || {
        for i in 0..10_000 {
            let value = if i % 2 == 0 { 0.25 } else { 0.75 };
            writer.publish(0, value, value);
        }
    });
    while !worker.is_finished() {
        if let Some(value) = reader.poll_level().unwrap() {
            assert_eq!(value.peak, value.rms);
        }
    }
    worker.join().unwrap();
}
