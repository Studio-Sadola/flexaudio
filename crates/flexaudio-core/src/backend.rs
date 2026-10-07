//! The [`CaptureBackend`] trait implemented by OS backends, and the [`RawSink`] handle used
//! by backends to pass raw frames to the core.
//!
//! The facade layer wires the path (backend → [`RawRing`](mod@crate::raw_ring) → [`Normalizer`](crate::normalizer)
//! → [`ChunkRing`](mod@crate::chunk_ring)). This module only defines backend contract types.

use crate::raw_ring::RawProducer;
use crate::types::{Event, Result};

/// Sink for passing raw interleaved f32 frames from a backend to the core.
///
/// Holds a [`RawProducer`] internally. [`push`](Self::push) writes without blocking and is RT-safe
/// (drops data when full). This is the SPSC producer side; call `push` only from the backend's
/// RT callback thread.
pub struct RawSink {
    producer: RawProducer,
    native_rate: u32,
    native_channels: u16,
}

impl RawSink {
    /// Create a raw-frame sink with the backend native format.
    pub fn new(producer: RawProducer, native_rate: u32, native_channels: u16) -> Self {
        Self {
            producer,
            native_rate,
            native_channels,
        }
    }

    /// Send raw interleaved f32 frames without blocking.
    ///
    /// `pts_ns` is a device-provided presentation timestamp. Never block;
    /// increment the internal overflow counter and drop data when full.
    ///
    /// PTS normalization and 20 ms chunking belong to the ingest thread. This sends only raw frames;
    /// the raw ring carries samples only, so the wiring layer handles `pts_ns` separately
    /// (backends that can provide it should do so for future frame association).
    ///
    /// `push` is intended for backend RT callbacks. Custom backends must not allocate, lock, block,
    /// or make system calls along the path that calls push.
    pub fn push(&mut self, interleaved: &[f32], pts_ns: i64) -> usize {
        let _ = pts_ns;
        self.producer.push_slice(interleaved)
    }

    /// Backend native sample rate (Hz).
    pub fn native_rate(&self) -> u32 {
        self.native_rate
    }

    /// Backend native channel count.
    pub fn native_channels(&self) -> u16 {
        self.native_channels
    }

    /// Cumulative samples dropped because the buffer was full.
    pub fn overflow_count(&self) -> u64 {
        self.producer.overflow_count()
    }
}

/// Trait implemented by OS-specific capture backends.
///
/// The facade reads the native format via [`native_format`](Self::native_format), configures a
/// [`Normalizer`](crate::normalizer), then passes a [`RawSink`] to [`start`](Self::start) to begin
/// capture. The backend calls `sink.push(...)` from its RT callback.
///
/// Pass a custom backend as `Box<dyn CaptureBackend>` to `Stream::open`.
///
/// Requirements for custom backends:
/// - Never panic on the capture thread / RT callback (a panic can silently stop capture).
///
/// - Call [`RawSink::push`] from RT callbacks in an RT-safe way (no allocation, locks, blocking,
///   or system calls; see [`RawSink::push`] for details).
/// - Make `start` / `stop` idempotent: calling `start` while running returns `Ok(())` without
///   doing anything; calling `stop` before startup does nothing.
pub trait CaptureBackend: Send {
    /// Backend native format `(sample_rate, channels)`.
    fn native_format(&self) -> (u32, u16);

    /// Start sending raw frames to the given sink.
    fn start(&mut self, sink: RawSink) -> Result<()>;

    /// Stop capture.
    fn stop(&mut self);

    /// Poll a backend notification on the control thread. Confirmed permission
    /// denial and [`Event::TerminalError`] are terminal; the stream stops capture
    /// and suppresses recovery.
    /// Return promptly and return `None` when empty. After `stop`, final owner
    /// notifications must remain pollable until drained; do not discard denial
    /// when stopping or replacing a capture generation.
    fn poll_event(&mut self) -> Option<Event> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raw_ring::raw_ring;

    /// Tiny backend for tests. Pushes one block from `start`.
    struct DummyBackend {
        sink: Option<RawSink>,
    }

    impl CaptureBackend for DummyBackend {
        fn native_format(&self) -> (u32, u16) {
            (44_100, 2)
        }
        fn start(&mut self, mut sink: RawSink) -> Result<()> {
            assert_eq!(sink.native_rate(), 44_100);
            assert_eq!(sink.native_channels(), 2);
            sink.push(&[0.1, 0.2, 0.3, 0.4], 0);
            self.sink = Some(sink);
            Ok(())
        }
        fn stop(&mut self) {
            self.sink = None;
        }
    }

    #[test]
    fn backend_pushes_into_raw_ring() {
        let (prod, mut cons) = raw_ring(16);
        let sink = RawSink::new(prod, 44_100, 2);
        let mut be = DummyBackend { sink: None };
        assert_eq!(be.native_format(), (44_100, 2));
        be.start(sink).unwrap();

        let mut out = [0.0f32; 4];
        let got = cons.pop_slice(&mut out);
        assert_eq!(got, 4);
        assert_eq!(out, [0.1, 0.2, 0.3, 0.4]);
        be.stop();
    }
}
