//! The [`CaptureBackend`] trait implemented by OS backends, and the [`RawSink`] handle that
//! backends use to hand raw frames to the core.
//!
//! The wiring (backend → [`RawRing`](mod@crate::raw_ring) → [`Normalizer`](crate::normalizer)
//! → [`ChunkRing`](mod@crate::chunk_ring)) is done later by the facade layer. This module only
//! defines the types of the backend contract.

use crate::raw_ring::RawProducer;
use crate::types::Result;

/// Sink through which a backend hands raw interleaved f32 frames to the core.
///
/// It holds a [`RawProducer`] internally, and [`push`](Self::push) performs an RT-safe,
/// non-blocking write (DROP when full). This is the producer side of an SPSC ring, and `push` is
/// expected to be called only from the backend's RT callback thread.
pub struct RawSink {
    producer: RawProducer,
    native_rate: u32,
    native_channels: u16,
}

impl RawSink {
    /// Creates a raw frame sink together with the backend's native format.
    pub fn new(producer: RawProducer, native_rate: u32, native_channels: u16) -> Self {
        Self {
            producer,
            native_rate,
            native_channels,
        }
    }

    /// Hands over raw interleaved f32 frames without blocking.
    ///
    /// `pts_ns` is the device-derived presentation timestamp. This never blocks; when full, it
    /// increments the internal overflow counter and drops.
    ///
    /// PTS normalization and 20ms chunking are the ingest thread's responsibility; this only
    /// hands over raw frames. Because the raw ring carries only samples, the wiring layer routes
    /// `pts_ns` separately (it is meant for mapping to frames in the future, so backends that can
    /// provide it should pass it).
    ///
    /// `push` is expected to be called from the backend's RT callback. In a custom backend, the
    /// path that calls push must not allocate on the heap, take locks, block, or make system
    /// calls.
    pub fn push(&mut self, interleaved: &[f32], pts_ns: i64) -> usize {
        let _ = pts_ns;
        self.producer.push_slice(interleaved)
    }

    /// The backend's native sample rate (Hz).
    pub fn native_rate(&self) -> u32 {
        self.native_rate
    }

    /// The backend's native channel count.
    pub fn native_channels(&self) -> u16 {
        self.native_channels
    }

    /// Cumulative number of samples dropped so far (because the ring was full).
    pub fn overflow_count(&self) -> u64 {
        self.producer.overflow_count()
    }
}

/// Trait implemented by OS-specific capture backends.
///
/// The facade obtains the native format via [`native_format`](Self::native_format), configures
/// the [`Normalizer`](crate::normalizer), and starts capture by passing a [`RawSink`] to
/// [`start`](Self::start). The backend calls `sink.push(...)` inside its own RT callback.
///
/// A custom backend can be plugged in by passing a `Box<dyn CaptureBackend>` to `Stream::open`.
///
/// Rules to follow when implementing a custom backend:
/// - Do not panic on the capture thread / RT callback (a panic can silently stop capture).
/// - Call [`RawSink::push`] from the RT callback in an RT-safe way (no heap allocation, locks,
///   blocking, or system calls; see [`RawSink::push`] for details).
/// - Make `start` / `stop` idempotent (a second `start` while running is a no-op returning `Ok`,
///   and `stop` when not started is also a no-op).
pub trait CaptureBackend: Send {
    /// The backend's native format `(sample_rate, channels)`.
    fn native_format(&self) -> (u32, u16);

    /// Starts streaming raw frames into the given sink.
    fn start(&mut self, sink: RawSink) -> Result<()>;

    /// Stops capture.
    fn stop(&mut self);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raw_ring::raw_ring;

    /// Minimal backend for tests. Pushes one block in `start`.
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
