use super::*;
use crate::mock::{MockBackend, PanicMode, PanickingMockBackend, StallThenPanicOnReopenBackend};
use flexaudio_core::types::SourceKind;
use std::time::Instant;

/// Helper to collect chunks by polling until the deadline.
fn collect_for(stream: &mut Stream, dur: Duration) -> Vec<AudioChunk> {
    let mut chunks = Vec::new();
    let start = Instant::now();
    while start.elapsed() < dur {
        while let Some(c) = stream.poll_chunk() {
            chunks.push(c);
        }
        thread::sleep(Duration::from_millis(5));
    }
    chunks
}

/// Wait until `cond` is true (up to `timeout`); return true if it becomes true.
fn wait_until<F: FnMut() -> bool>(mut cond: F, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if cond() {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    cond()
}

/// Extract Err from a `Stream::open` result (`Stream` does not implement `Debug`, so
/// `expect_err` cannot be used). Panic with a message if the result is Ok.
fn open_err(result: Result<Stream>, ctx: &str) -> Error {
    match result {
        Ok(_) => panic!("{ctx}: expected an error, got Ok"),
        Err(e) => e,
    }
}

// --- RawRing overflow → DISCONTINUITY ---

/// Test-only mock backend that overflows RawRing. On start, each burst keeps pushing twice the
/// ring capacity, guaranteeing overflow.
struct FloodingMockBackend {
    running: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl FloodingMockBackend {
    fn new() -> Self {
        Self {
            running: Arc::new(AtomicBool::new(false)),
            handle: None,
        }
    }
}

impl CaptureBackend for FloodingMockBackend {
    fn native_format(&self) -> (u32, u16) {
        (48_000, 2)
    }
    fn start(&mut self, mut sink: RawSink) -> Result<()> {
        self.running.store(true, Ordering::SeqCst);
        let running = self.running.clone();
        let handle = thread::spawn(move || {
            // Push twice the ring capacity per burst (guaranteed overflow).
            let burst = vec![0.1f32; RAW_RING_SAMPLES * 2];
            while running.load(Ordering::SeqCst) {
                sink.push(&burst, 0);
                thread::sleep(Duration::from_millis(5));
            }
        });
        self.handle = Some(handle);
        Ok(())
    }
    fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

// --- Inject denoise into the internal canonical format (through core InnerProcessor) ---

/// Failure-detection bound, not a collection window. Debug RNNoise processing can
/// delay the first delivery beyond 500 ms under load; exit as soon as it arrives.
const DENOISE_TAP_TIMEOUT: Duration = Duration::from_secs(15);

#[path = "tests/validation.rs"]
mod validation;

#[path = "tests/output.rs"]
mod output;

#[path = "tests/health.rs"]
mod health;

#[path = "tests/pause.rs"]
mod pause;

#[path = "tests/gain.rs"]
mod gain;
