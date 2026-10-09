//! Synthetic child backends and bounded offline mixer reproductions.
use super::*;
use std::sync::atomic::AtomicU64;

struct Burst {
    samples: usize,
    value: f32,
    final_samples: usize,
    overflow: Arc<AtomicU64>,
    sink: Option<RawSink>,
    terminal: bool,
    panic_stop: bool,
}
impl Burst {
    fn new(samples: usize) -> Self {
        Self {
            samples,
            value: 0.25,
            final_samples: 0,
            overflow: Arc::new(AtomicU64::new(0)),
            sink: None,
            terminal: false,
            panic_stop: false,
        }
    }
}
impl CaptureBackend for Burst {
    fn native_format(&self) -> (u32, u16) {
        (48_000, 2)
    }
    fn start(&mut self, mut sink: RawSink) -> Result<()> {
        sink.push(&vec![self.value; self.samples], 0);
        self.overflow.store(sink.overflow_count(), Ordering::SeqCst);
        self.sink = Some(sink);
        Ok(())
    }
    fn stop(&mut self) {
        if let Some(mut sink) = self.sink.take() {
            sink.push(&vec![0.25; self.final_samples], 0);
        }
        if std::mem::take(&mut self.panic_stop) {
            panic!("injected child stop failure");
        }
    }
    fn poll_event(&mut self) -> Option<Event> {
        if std::mem::take(&mut self.terminal) {
            Some(Event::TerminalError {
                error: Error::Backend("injected mic failure".into()),
            })
        } else {
            None
        }
    }
}
fn collect(consumer: &mut RawConsumer) -> usize {
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut samples = vec![0.0; RAW_RING_SAMPLES];
    loop {
        let got = consumer.pop_slice(&mut samples);
        if got > 0 {
            return got;
        }
        assert!(Instant::now() < deadline, "mixer harness timeout");
        thread::sleep(Duration::from_millis(1));
    }
}
fn child_overflow(overflow: bool) {
    let count = if overflow {
        RAW_RING_SAMPLES + 1920
    } else {
        1920
    };
    let mic = Burst::new(count);
    let lost = mic.overflow.clone();
    let mut be = CompositeBackend::new(Box::new(mic), Box::new(Burst::new(count)), 1.0, 1.0);
    let (producer, mut consumer) = raw_ring(RAW_RING_SAMPLES * 2);
    be.start(RawSink::new(producer, SAMPLE_RATE, CHANNELS))
        .unwrap();
    let delivered = collect(&mut consumer);
    be.stop();
    let event = be.poll_event();
    let dropped = lost.load(Ordering::SeqCst);
    assert!(!overflow || dropped > 0, "fixture must overflow child ring");
    assert!(dropped == 0 || event.is_some(), "F08: child_dropped_samples={dropped}, delivered_mixed_samples={delivered}, event={event:?}; loss disappeared at mixer boundary");
}
#[test]
#[ignore = "repro: F08"]
fn repro_p3_child_overflow() {
    child_overflow(true);
}
#[test]
fn repro_p3_child_overflow_control() {
    child_overflow(false);
}

fn fifo_overflow(overflow: bool) {
    let mut be: Box<dyn CaptureBackend> = Box::new(Burst::new(1920));
    let mut lane = start_child(&mut be).unwrap();
    if overflow {
        lane.fifo = vec![0.1; FIFO_MAX_SAMPLES];
    }
    let before = lane.fifo.len();
    lane.ingest(&mut vec![0.0; RAW_RING_SAMPLES]).unwrap();
    let after = lane.fifo.len();
    let event = be.poll_event();
    be.stop();
    let lost = before + 1920 - after;
    assert!(
        lost == 0 || event.is_some(),
        "F08/M3: fifo_discarded_samples={lost}, child_ring_overflow={}, event={event:?}",
        lane.consumer.overflow_count()
    );
}
#[test]
#[ignore = "repro: F08 / D M3"]
fn repro_p3_fifo_overflow() {
    fifo_overflow(true);
}
#[test]
fn repro_p3_fifo_overflow_control() {
    fifo_overflow(false);
}

fn final_tail(late: bool) {
    let mut mic = Burst::new(if late { 0 } else { 1920 });
    let mut system = Burst::new(if late { 0 } else { 1920 });
    if late {
        mic.final_samples = 1920;
        system.final_samples = 1920;
    }
    let mut be = CompositeBackend::new(Box::new(mic), Box::new(system), 1.0, 1.0);
    let (producer, mut consumer) = raw_ring(RAW_RING_SAMPLES);
    be.start(RawSink::new(producer, SAMPLE_RATE, CHANNELS))
        .unwrap();
    let before = if late { 0 } else { collect(&mut consumer) };
    be.stop();
    let got = consumer.pop_slice(&mut vec![0.0; RAW_RING_SAMPLES]) + before;
    assert!(
        got > 0,
        "F13: children each supplied 1920 final samples during stop, mixer delivered_samples={got}"
    );
}
#[test]
#[ignore = "repro: F13"]
fn repro_p3_final_tail() {
    final_tail(true);
}
#[test]
fn repro_p3_final_tail_control() {
    final_tail(false);
}

fn shutdown(panic: bool) {
    let mut mic = Burst::new(0);
    mic.panic_stop = panic;
    let mut be = CompositeBackend::new(Box::new(mic), Box::new(Burst::new(0)), 1.0, 1.0);
    let (producer, _consumer) = raw_ring(RAW_RING_SAMPLES);
    be.start(RawSink::new(producer, SAMPLE_RATE, CHANNELS))
        .unwrap();
    be.stop();
    let event = be.poll_event();
    assert!(
        !panic || event.is_some(),
        "F37: child stop panic swallowed: event={event:?}"
    );
}
#[test]
#[ignore = "repro: F37"]
fn repro_p3_shutdown_panic() {
    shutdown(true);
}
#[test]
fn repro_p3_shutdown_panic_control() {
    shutdown(false);
}

// Control and regression evidence for the 0.4.0 child terminal-event transport.
#[test]
fn repro_p3_terminal_lane_already_fixed() {
    let mut mic = Burst::new(0);
    mic.terminal = true;
    let mut be = CompositeBackend::new(Box::new(mic), Box::new(Burst::new(1920)), 1.0, 1.0);
    let (producer, _consumer) = raw_ring(RAW_RING_SAMPLES);
    be.start(RawSink::new(producer, SAMPLE_RATE, CHANNELS))
        .unwrap();
    let event = be.poll_event();
    be.stop();
    assert!(
        matches!(event, Some(Event::TerminalError { .. })),
        "child failure must survive healthy system lane: {event:?}"
    );
}
#[test]
fn repro_p3_terminal_lane_control() {
    let mut be = CompositeBackend::new(
        Box::new(Burst::new(1920)),
        Box::new(Burst::new(1920)),
        1.0,
        1.0,
    );
    let (producer, mut consumer) = raw_ring(RAW_RING_SAMPLES);
    be.start(RawSink::new(producer, SAMPLE_RATE, CHANNELS))
        .unwrap();
    assert!(collect(&mut consumer) > 0);
    assert!(be.poll_event().is_none());
    be.stop();
}

fn clipping(clip: bool) {
    let mut mic = Burst::new(1920);
    let mut system = Burst::new(1920);
    mic.value = if clip { 0.8 } else { 0.2 };
    system.value = mic.value;
    let mut stream = crate::Stream::open(
        flexaudio_core::types::StreamConfig::default(),
        Box::new(CompositeBackend::new(
            Box::new(mic),
            Box::new(system),
            1.0,
            1.0,
        )),
    )
    .unwrap();
    stream.start().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let chunk = loop {
        if let Some(chunk) = stream.poll_chunk() {
            break chunk;
        }
        assert!(
            Instant::now() < deadline,
            "clipping fixture supplied no mixed output"
        );
        thread::sleep(Duration::from_millis(1));
    };
    stream.stop();
    let event = stream.poll_event();
    assert_eq!(chunk.data[0], if clip { 1.0 } else { 0.4 });
    assert!(
        !clip || !chunk.flags.is_empty() || event.is_some(),
        "F16: mixed 0.8 + 0.8 saturated to {}, flags={:?}, event={event:?}",
        chunk.data[0],
        chunk.flags
    );
}
#[test]
#[ignore = "repro: F16"]
fn repro_p3_clipping_metadata() {
    clipping(true);
}
#[test]
fn repro_p3_clipping_metadata_control() {
    clipping(false);
}

fn valid_dsp(rate: u32) {
    let (producer, consumer) = raw_ring(RAW_RING_SAMPLES);
    let mut sink = RawSink::new(producer, rate, 2);
    sink.push(&vec![0.25; (rate as usize / 50) * 2], 0);
    let mut lane = ChildLane {
        consumer,
        normalizer: Normalizer::new(rate, 2, OutputFormat::default()).unwrap(),
        fifo: Vec::new(),
        last_supply: Instant::now(),
    };
    let result = lane.ingest(&mut vec![0.0; RAW_RING_SAMPLES]);
    assert!(
        result.is_ok(),
        "F09: valid-format stock DSP unexpectedly failed: {result:?}"
    );
}
#[test]
fn repro_p3_dsp_failure_not_reproduced() {
    for rate in [8_000, 16_000, 44_100, 96_000, 192_000] {
        valid_dsp(rate);
    }
}
#[test]
fn repro_p3_dsp_failure_control() {
    valid_dsp(48_000);
}
