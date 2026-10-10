//! Offline recovery regressions using synchronous backend bursts and real intake delivery.

use super::*;
use std::sync::TryLockError;
use std::time::Instant;

struct BurstBackend {
    bursts: VecDeque<Vec<f32>>,
    sink: Option<RawSink>,
}

impl CaptureBackend for BurstBackend {
    fn native_format(&self) -> (u32, u16) {
        (48_000, 2)
    }

    fn start(&mut self, mut sink: RawSink) -> Result<()> {
        if let Some(samples) = self.bursts.pop_front() {
            sink.push(&samples, 0);
        }
        self.sink = Some(sink);
        Ok(())
    }

    fn stop(&mut self) {
        self.sink = None;
    }
}

fn open(bursts: Vec<Vec<f32>>, denoise: bool) -> Stream {
    let config = StreamConfig {
        secondary_output: Some(OutputFormat {
            sample_rate: 48_000,
            channels: 2,
        }),
        ..StreamConfig::default()
    };
    let stream = Stream::open(
        config,
        Box::new(BurstBackend {
            bursts: bursts.into(),
            sink: None,
        }),
    )
    .unwrap();
    stream.set_denoise(denoise);
    stream
}

fn wait(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(Instant::now() < deadline, "recovery fixture timeout");
        thread::sleep(Duration::from_millis(1));
    }
}

fn reopen(stream: &Stream, change: GenerationChange) {
    stream.shared.backend.lock().unwrap().stop();
    Stream::open_backend_once(&stream.shared, change).unwrap();
}

// Own only intake here: synchronous fake starts exercise raw publication without watchdog timing.
fn start_intake(stream: &mut Stream) {
    let producer = stream.shared.chunk_producer.lock().unwrap().take().unwrap();
    let secondary = stream.shared.secondary_producer.lock().unwrap().take();
    let shared = stream.shared.clone();
    let output = stream.config.output;
    let secondary_output = stream.config.secondary_output;
    stream.worker = Some(thread::spawn(move || {
        run_intake(
            shared,
            producer,
            secondary,
            (48_000, 2),
            output,
            secondary_output,
        );
    }));
    stream.started = true;
}

fn recovery_count(stream: &mut Stream) -> usize {
    let mut count = 0;
    while let Some(event) = stream.poll_event() {
        count += usize::from(matches!(event, Event::StreamRecovered));
    }
    count
}

fn assert_chunk_flags(stream: &mut Stream, expected_recoveries: usize) {
    let mut primary = 0;
    let mut secondary = 0;
    while let Some(chunk) = stream.poll_chunk() {
        if chunk.flags.contains(ChunkFlags::RECOVERED) {
            assert!(chunk.flags.contains(ChunkFlags::DISCONTINUITY));
            primary += 1;
        }
    }
    while let Some(chunk) = stream.poll_secondary() {
        if chunk.flags.contains(ChunkFlags::RECOVERED) {
            assert!(chunk.flags.contains(ChunkFlags::DISCONTINUITY));
            secondary += 1;
        }
    }
    assert_eq!(primary, expected_recoveries, "primary recovery flags");
    assert_eq!(secondary, expected_recoveries, "secondary recovery flags");
}

fn stop_tail(captured: bool) {
    let burst = if captured { vec![0.25; 1920] } else { vec![] };
    let mut stream = open(vec![vec![], burst], true);
    reopen(&stream, GenerationChange::Initial);
    reopen(&stream, GenerationChange::Recovery);
    assert_eq!(stream.shared.snapshot_raw(&mut []).generation, 2);
    start_intake(&mut stream);
    let mut recovered = 0;
    if captured {
        wait(|| {
            recovered += recovery_count(&mut stream);
            recovered == 1
        });
        wait(|| !stream.secondary_consumer.as_ref().unwrap().is_empty());
    }
    stream.stop();
    recovered += recovery_count(&mut stream);
    assert_eq!(recovered, usize::from(captured));
    assert!(!stream.shared.recovered_pending.load(Ordering::SeqCst));
    // A fresh denoiser really did flush its primed delay into padded chunks on both taps.
    assert!(
        !stream.chunk_consumer.is_empty(),
        "fixture must emit a stop tail"
    );
    assert!(!stream.secondary_consumer.as_ref().unwrap().is_empty());
    assert_chunk_flags(&mut stream, usize::from(captured));
}

#[test]
fn silent_reopen_denoiser_stop_never_recovers() {
    stop_tail(false);
}

#[test]
fn captured_reopen_denoiser_stop_control() {
    stop_tail(true);
}

fn first_burst(recovery: bool) {
    let mut stream = open(vec![vec![0.25; 5760]], false);
    reopen(
        &stream,
        if recovery {
            GenerationChange::Recovery
        } else {
            GenerationChange::Initial
        },
    );
    // Metadata snapshots cannot consume the flag before the already-captured burst is processed.
    let snapshot = stream.shared.snapshot_raw(&mut []);
    assert_eq!(snapshot.generation, 1);
    assert_eq!(snapshot.samples, 0);
    assert!(!snapshot.recovered);
    assert_eq!(
        stream.shared.recovered_pending.load(Ordering::SeqCst),
        recovery
    );
    start_intake(&mut stream);
    wait(|| stream.chunk_consumer.len() == 3);
    wait(|| stream.secondary_consumer.as_ref().unwrap().len() == 3);
    stream.stop();
    assert_eq!(recovery_count(&mut stream), usize::from(recovery));
    assert_chunk_flags(&mut stream, usize::from(recovery));
}

#[test]
fn recovery_published_with_first_backend_burst() {
    first_burst(true);
}

#[test]
fn initial_backend_burst_control() {
    first_burst(false);
}

fn superseded_pending(silent_first: bool) {
    let first = if silent_first {
        vec![]
    } else {
        vec![0.25; 1920]
    };
    let mut stream = open(vec![first, vec![0.5; 1920]], false);
    reopen(&stream, GenerationChange::Recovery);
    start_intake(&mut stream);
    let mut recovered = 0;
    if silent_first {
        // Exercise an empty intake snapshot before replacing its pending generation.
        let snapshot = stream.shared.snapshot_raw(&mut [0.0; 1920]);
        assert_eq!(snapshot.samples, 0);
        assert!(!snapshot.recovered);
        assert!(stream.shared.recovered_pending.load(Ordering::SeqCst));
    } else {
        wait(|| {
            recovered += recovery_count(&mut stream);
            recovered == 1
        });
        wait(|| !stream.secondary_consumer.as_ref().unwrap().is_empty());
    }
    reopen(&stream, GenerationChange::Recovery);
    let expected = if silent_first { 1 } else { 2 };
    wait(|| {
        recovered += recovery_count(&mut stream);
        recovered == expected
    });
    wait(|| stream.secondary_consumer.as_ref().unwrap().len() == expected);
    stream.stop();
    recovered += recovery_count(&mut stream);
    assert_eq!(recovered, expected);
    assert_chunk_flags(&mut stream, expected);
}

#[test]
fn silent_recovery_generation_does_not_leak_into_next() {
    superseded_pending(true);
}

#[test]
fn two_captured_recovery_generations_control() {
    superseded_pending(false);
}

fn latched_generation(recovery: bool) {
    let mut stream = open(vec![vec![0.25; 200], vec![0.5; 1920]], false);
    reopen(&stream, GenerationChange::Recovery);
    start_intake(&mut stream);
    wait(|| !stream.shared.recovered_pending.load(Ordering::SeqCst));
    assert!(
        stream.chunk_consumer.is_empty(),
        "first generation only supplies a partial chunk"
    );
    reopen(
        &stream,
        if recovery {
            GenerationChange::Recovery
        } else {
            GenerationChange::Switch
        },
    );
    wait(|| !stream.chunk_consumer.is_empty());
    wait(|| !stream.secondary_consumer.as_ref().unwrap().is_empty());
    stream.stop();
    assert_eq!(recovery_count(&mut stream), usize::from(recovery));
    assert_chunk_flags(&mut stream, usize::from(recovery));
}

#[test]
fn latched_recovery_is_discarded_with_its_generation() {
    latched_generation(false);
}

#[test]
fn latched_recovery_replaced_by_new_recovery_control() {
    latched_generation(true);
}

fn terminal_order(error: Error, recovery_first: bool) {
    let mut stream = open(vec![vec![0.25; 1920]], false);
    reopen(&stream, GenerationChange::Recovery);
    if recovery_first {
        // Block event enqueue after the chunk push. Terminal publication must still be excluded
        // by delivery until recovery has been enqueued, even when the event queue is contended.
        let events = stream.events.clone();
        let queue = events.lock().unwrap();
        start_intake(&mut stream);
        wait(|| !stream.chunk_consumer.is_empty());
        let recovery_holds_delivery = matches!(
            stream.shared.delivery.try_lock(),
            Err(TryLockError::WouldBlock)
        );
        let shared = stream.shared.clone();
        let terminal = thread::spawn(move || {
            let delivery = shared.delivery.lock().unwrap();
            shared.fail_terminal_locked(error, &delivery);
        });
        drop(queue);
        terminal.join().unwrap();
        stream.stop();
        assert!(
            recovery_holds_delivery,
            "recovery enqueue must exclude terminal publication"
        );
        assert_eq!(stream.poll_event(), Some(Event::StreamRecovered));
    } else {
        let shared = stream.shared.clone();
        let delivery = shared.delivery.lock().unwrap();
        start_intake(&mut stream);
        // Let intake consume the recovery snapshot, then publish terminal failure before delivery.
        wait(|| !shared.recovered_pending.load(Ordering::SeqCst));
        shared.fail_terminal_locked(error, &delivery);
        drop(delivery);
        stream.stop();
        assert!(stream.chunk_consumer.is_empty());
        assert!(stream.secondary_consumer.as_ref().unwrap().is_empty());
    }
    assert!(matches!(
        stream.poll_event(),
        Some(Event::TerminalError { .. } | Event::PermissionDenied { .. })
    ));
    assert_eq!(
        stream.poll_event(),
        None,
        "recovery cannot follow a terminal event"
    );
    assert!(stream.terminal_error().is_some());
}

#[test]
fn terminal_failure_prevents_pending_recovery() {
    terminal_order(Error::Backend("terminal fixture".into()), false);
}

#[test]
fn recovery_enqueue_precedes_terminal_failure_control() {
    terminal_order(Error::Backend("terminal fixture".into()), true);
}

#[test]
fn permission_denial_prevents_pending_recovery() {
    terminal_order(
        Error::PermissionDenied {
            permission: Permission::Microphone,
            detail: "fixture".into(),
        },
        false,
    );
}

#[test]
fn recovery_enqueue_precedes_permission_denial_control() {
    terminal_order(
        Error::PermissionDenied {
            permission: Permission::Microphone,
            detail: "fixture".into(),
        },
        true,
    );
}
