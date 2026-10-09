//! Deterministic offline lifecycle and delivery reproductions.
use super::*;
use std::sync::mpsc;
use std::time::Instant;

struct Backend {
    starts: Arc<AtomicU32>,
    live: Arc<AtomicBool>,
    fail_restart: bool,
    fail_start: bool,
    samples: Vec<f32>,
    sink: Option<RawSink>,
    stop_panic: bool,
    stop_gate: Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>,
}
impl Backend {
    fn silent() -> Self {
        Self {
            starts: Arc::new(AtomicU32::new(0)),
            live: Arc::new(AtomicBool::new(false)),
            fail_restart: false,
            fail_start: false,
            samples: Vec::new(),
            sink: None,
            stop_panic: false,
            stop_gate: None,
        }
    }
}
impl CaptureBackend for Backend {
    fn native_format(&self) -> (u32, u16) {
        (48_000, 2)
    }
    fn start(&mut self, mut sink: RawSink) -> Result<()> {
        let previous = self.starts.fetch_add(1, Ordering::SeqCst);
        if self.fail_start {
            return Err(Error::Backend("replacement startup failure".into()));
        }
        if self.fail_restart && previous > 0 {
            return Err(Error::Backend("rollback startup failure".into()));
        }
        if !self.samples.is_empty() {
            sink.push(&self.samples, 0);
        }
        self.live.store(true, Ordering::SeqCst);
        self.sink = Some(sink);
        Ok(())
    }
    fn stop(&mut self) {
        if let Some((entered, release)) = self.stop_gate.take() {
            entered.send(()).unwrap();
            release.recv_timeout(Duration::from_secs(5)).unwrap();
            self.sink.as_mut().unwrap().push(&vec![0.25; 1920], 0);
        }
        self.live.store(false, Ordering::SeqCst);
        self.sink = None;
        if std::mem::take(&mut self.stop_panic) {
            panic!("injected teardown failure");
        }
    }
}
fn open(backend: Backend) -> Stream {
    Stream::open(StreamConfig::default(), Box::new(backend)).unwrap()
}
fn wait(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(Instant::now() < deadline, "harness timeout");
        thread::sleep(Duration::from_millis(1));
    }
}
fn restart(restart: bool) {
    let be = Backend::silent();
    let live = be.live.clone();
    let starts = be.starts.clone();
    let mut stream = open(be);
    stream.start().unwrap();
    stream.stop();
    if restart {
        assert!(stream.start().is_err());
    }
    let active = live.load(Ordering::SeqCst);
    let count = starts.load(Ordering::SeqCst);
    // Clean up explicitly before the assertion, because failed start is not owned by Drop.
    stream.stop();
    assert!(
        !active,
        "NEW-restart: failed second start left backend active={active}, start_calls={count}"
    );
}
#[test]
fn repro_p2_restart() {
    restart(true);
}
#[test]
fn repro_p2_restart_control() {
    restart(false);
}

fn rollback(fail: bool) {
    let mut old = Backend::silent();
    old.fail_restart = fail;
    let live = old.live.clone();
    let starts = old.starts.clone();
    let mut stream = open(old);
    stream.start().unwrap();
    let mut replacement = Backend::silent();
    replacement.fail_start = true;
    let error = stream
        .switch_backend(Box::new(replacement))
        .unwrap_err()
        .to_string();
    let restored_live = live.load(Ordering::SeqCst);
    let attempts = starts.load(Ordering::SeqCst);
    stream.stop();
    assert_eq!(attempts, 2, "fixture must attempt rollback");
    assert_eq!(
        restored_live, !fail,
        "control must actually restore the old source"
    );
    assert!(
        !fail || error.contains("rollback"),
        "F10: both opens failed but returned cause={error:?}; rollback cause absent"
    );
}
#[test]
#[ignore = "repro: F10"]
fn repro_p2_rollback() {
    rollback(true);
}
#[test]
fn repro_p2_rollback_control() {
    rollback(false);
}

fn recovery(feed: bool) {
    let mut be = Backend::silent();
    if feed {
        be.samples = vec![0.25; 1920];
    }
    let mut stream = open(be);
    stream.start().unwrap();
    // Advance only the health timestamp to trigger the actual watchdog promptly.
    stream
        .shared
        .last_sample_ns
        .store(monotonic_now_ns() - 10_000_000_000, Ordering::SeqCst);
    let mut recovered = false;
    wait(|| {
        while let Some(event) = stream.poll_event() {
            recovered |= matches!(event, Event::StreamRecovered);
        }
        recovered
    });
    let mut delivered = 0;
    while let Some(c) = stream.poll_chunk() {
        delivered += c.frames;
    }
    stream.stop();
    assert!(delivered > 0, "F11: StreamRecovered emitted with no samples ever supplied: delivered_frames={delivered}, recovered={recovered}");
}
#[test]
#[ignore = "repro: F11"]
fn repro_p2_false_recovery() {
    recovery(false);
}
#[test]
fn repro_p2_false_recovery_control() {
    recovery(true);
}

fn final_buffer(late: bool) {
    let mut be = Backend::silent();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    if late {
        be.stop_gate = Some((entered_tx, release_rx));
    } else {
        be.samples = vec![0.25; 1920];
    }
    let mut stream = open(be);
    stream.start().unwrap();
    if !late {
        wait(|| !stream.chunk_consumer.is_empty());
    }
    // Keep the actual intake handle outside stop so the controller can prove it
    // exited before the fake producer is released. No intake implementation changes.
    let worker = stream.worker.take().unwrap();
    let stop = thread::spawn(move || {
        stream.stop();
        stream
    });
    if late {
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        wait(|| worker.is_finished());
        worker.join().unwrap();
        release_tx.send(()).unwrap();
    } else {
        worker.join().unwrap();
    }
    let mut stream = stop.join().unwrap();
    let pending = stream
        .shared
        .raw_consumer
        .lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .pop_slice(&mut vec![0.0; 1920]);
    let delivered = stream.poll_chunk().map_or(0, |c| c.frames);
    assert!(delivered > 0, "NEW-final-buffer: intake exited before backend final push: delivered_frames={delivered}, stranded_raw_samples={pending}");
}
#[test]
#[ignore = "repro: NEW-final-buffer"]
fn repro_p2_final_buffer() {
    final_buffer(true);
}
#[test]
fn repro_p2_final_buffer_control() {
    final_buffer(false);
}

fn initial_snapshot(stale: bool) {
    let mut stream = open(Backend::silent());
    let (prod, cons) = raw_ring(RAW_RING_SAMPLES);
    let mut sink = RawSink::new(prod, 48_000, 1);
    sink.push(&vec![0.25; 960], 0);
    *stream.shared.raw_consumer.lock().unwrap() = Some(cons);
    *stream.shared.native_format.lock().unwrap() = (48_000, 1);
    stream.shared.raw_generation.store(2, Ordering::SeqCst);
    stream.shared.stopping.store(true, Ordering::SeqCst);
    let producer = stream.shared.chunk_producer.lock().unwrap().take().unwrap();
    // Schedule: start snapshots stereo; switch publishes mono; worker snapshots
    // the new generation. This is exactly the split initial reads in start/intake.
    run_intake(
        stream.shared.clone(),
        producer,
        None,
        (48_000, if stale { 2 } else { 1 }),
        stream.config.output,
        None,
    );
    let chunk = stream.poll_chunk().unwrap();
    let real = chunk.data.iter().filter(|&&s| s == 0.25).count();
    assert_eq!(real, 1920, "F06: new mono ring interpreted with stale stereo snapshot: real_output_samples={real}, expected=1920");
}
#[test]
fn repro_p2_generation_snapshot() {
    initial_snapshot(true);
}
#[test]
fn repro_p2_generation_snapshot_control() {
    initial_snapshot(false);
}

fn poison(poison: bool) {
    let mut stream = open(Backend::silent());
    if poison {
        let events = stream.events.clone();
        let _ = thread::spawn(move || {
            let _g = events.lock().unwrap();
            panic!("injected queue poison");
        })
        .join();
    }
    stream
        .shared
        .push_event(Event::Error("retained diagnostic".into()));
    let event = stream.poll_event();
    assert!(
        event.is_some(),
        "M5: diagnostic queued after poison but poll_event returned {event:?}"
    );
}
#[test]
fn repro_p2_event_poison() {
    poison(true);
}
#[test]
fn repro_p2_event_poison_control() {
    poison(false);
}

fn shutdown(panic: bool) {
    let mut be = Backend::silent();
    be.stop_panic = panic;
    let mut stream = open(be);
    stream.start().unwrap();
    stream.stop();
    let event = stream.poll_event();
    let terminal = stream.terminal_error();
    assert!(
        !panic || event.is_some() || terminal.is_some(),
        "F37: injected stop panic swallowed: event={event:?}, terminal_error={terminal:?}"
    );
}
#[test]
#[ignore = "repro: F37"]
fn repro_p2_shutdown_panic() {
    shutdown(true);
}
#[test]
fn repro_p2_shutdown_panic_control() {
    shutdown(false);
}

fn tail_metadata(partial: bool) {
    let mut be = Backend::silent();
    be.samples = vec![0.25; if partial { 200 } else { 1920 }];
    let mut stream = open(be);
    stream.start().unwrap();
    stream.stop();
    let chunk = stream.poll_chunk().unwrap();
    let padding = chunk.data.iter().filter(|&&s| s == 0.0).count();
    assert!(
        !partial || !chunk.flags.is_empty(),
        "F14/M1: real_samples=200, generated_padding={padding}, flags={:?}, frames={}",
        chunk.flags,
        chunk.frames
    );
}
#[test]
#[ignore = "repro: F14 / D M1"]
fn repro_p2_padding_metadata() {
    tail_metadata(true);
}
#[test]
fn repro_p2_padding_metadata_control() {
    tail_metadata(false);
}

fn clipping(clip: bool) {
    let mut be = Backend::silent();
    be.samples = vec![if clip { 0.8 } else { 0.2 }; 1920];
    let mut stream = open(be);
    stream.set_gain(2.0).unwrap();
    stream.start().unwrap();
    stream.stop();
    let chunk = stream.poll_chunk().unwrap();
    let event = stream.poll_event();
    assert!(
        !clip || !chunk.flags.is_empty() || event.is_some(),
        "F16: saturated sample={}, flags={:?}, event={event:?}",
        chunk.data[0],
        chunk.flags
    );
}
#[test]
#[ignore = "repro: F16"]
fn repro_p2_clipping_metadata() {
    clipping(true);
}
#[test]
fn repro_p2_clipping_metadata_control() {
    clipping(false);
}

fn secondary_loss(overflow: bool) {
    let config = StreamConfig {
        ring_capacity_chunks: 8,
        secondary_output: Some(OutputFormat {
            sample_rate: 48_000,
            channels: 2,
        }),
        ..StreamConfig::default()
    };
    let mut stream = Stream::open(config, Box::new(Backend::silent())).unwrap();
    // Use the actual secondary ring with capacity one; isolate secondary loss
    // from primary loss so any required event must identify the secondary path.
    let (secondary_producer, secondary_consumer) = secondary_chunk_ring(1);
    stream.secondary_consumer = Some(secondary_consumer);
    let producer = stream.shared.chunk_producer.lock().unwrap().take().unwrap();
    let (prod, cons) = raw_ring(RAW_RING_SAMPLES);
    let mut sink = RawSink::new(prod, 48_000, 2);
    sink.push(&vec![0.25; if overflow { 5760 } else { 1920 }], 0);
    *stream.shared.raw_consumer.lock().unwrap() = Some(cons);
    stream.shared.stopping.store(true, Ordering::SeqCst);
    run_intake(
        stream.shared.clone(),
        producer,
        Some(secondary_producer),
        (48_000, 2),
        stream.config.output,
        stream.config.secondary_output,
    );
    let dropped = stream.poll_secondary().unwrap().dropped_before;
    let event = stream.poll_event();
    assert!(
        dropped == 0 || event.is_some(),
        "M4: secondary_dropped_before={dropped}, primary_dropped={}, event={event:?}",
        stream.dropped_chunks()
    );
}
#[test]
#[ignore = "repro: D M4"]
fn repro_p2_secondary_drop_event() {
    secondary_loss(true);
}
#[test]
fn repro_p2_secondary_drop_event_control() {
    secondary_loss(false);
}

// Limit only a subprocess after its test-harness thread already exists. This
// makes the real Builder::spawn return an OS allocation error without changing
// production spawn code or exhausting the host's process/thread quota.
#[test]
#[cfg(target_os = "linux")]
#[ignore = "repro: F12"]
fn repro_p2_spawn_failure() {
    if std::env::var_os("FLEXAUDIO_REPRO_SPAWN_CHILD").is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "stream::repro_tests::repro_p2_spawn_failure",
                "--exact",
                "--include-ignored",
                "--nocapture",
            ])
            .env("FLEXAUDIO_REPRO_SPAWN_CHILD", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let be = Backend::silent();
    let live = be.live.clone();
    let mut stream = open(be);
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let kb: u64 = status
        .lines()
        .find(|line| line.starts_with("VmSize:"))
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    // Preserve both inherited limits; never raise the hard limit. The fixture
    // operates only in this subprocess, after its harness thread already exists.
    let previous = std::process::Command::new("prlimit")
        .args([
            "--pid",
            &std::process::id().to_string(),
            "--as",
            "--noheadings",
            "--output",
            "SOFT,HARD",
        ])
        .output()
        .unwrap();
    assert!(previous.status.success());
    let previous = String::from_utf8(previous.stdout).unwrap();
    let limits: Vec<&str> = previous.split_whitespace().collect();
    assert_eq!(limits.len(), 2);
    for limit in &limits {
        assert!(*limit == "unlimited" || limit.parse::<u64>().is_ok());
    }
    let desired = kb * 1024 + 1024 * 1024;
    let maximum = limits[1].parse::<u64>().unwrap_or(u64::MAX);
    let restricted = format!("--as={}:{}", desired.min(maximum), limits[1]);
    let original = format!("--as={}:{}", limits[0], limits[1]);
    let limited = std::process::Command::new("prlimit")
        .args(["--pid", &std::process::id().to_string(), &restricted])
        .status()
        .unwrap();
    assert!(
        limited.success(),
        "harness could not constrain child address space"
    );
    let result = stream.start();
    let restored = std::process::Command::new("prlimit")
        .args(["--pid", &std::process::id().to_string(), &original])
        .status()
        .unwrap();
    assert!(restored.success());
    let error = result.expect_err("fixture must fail a real worker spawn");
    assert!(
        error.to_string().contains("spawn intake thread"),
        "unexpected failure: {error}"
    );
    drop(stream);
    assert!(
        !live.load(Ordering::SeqCst),
        "F12: {error}; Drop after failed startup left backend active=true"
    );
}
#[test]
fn repro_p2_spawn_failure_control() {
    let be = Backend::silent();
    let live = be.live.clone();
    let mut stream = open(be);
    stream.start().unwrap();
    drop(stream);
    assert!(!live.load(Ordering::SeqCst));
}
