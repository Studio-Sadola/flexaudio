//! SPSC ring of finished [`SecondaryChunk`]s (ringbuf-backed, DROP_OLDEST).
//!
//! This mirrors [`chunk_ring`](mod@crate::chunk_ring) but for the secondary
//! output tap. It is a separate type on purpose: the public
//! `chunk_ring`/`ChunkProducer`/`ChunkConsumer` re-exports are hard-coded to
//! [`AudioChunk`](crate::types::AudioChunk) and must stay byte-compatible, so
//! the secondary tap gets its own dedicated ring rather than a breaking
//! generalization of the primary one.
//!
//! When full, the producer pops the oldest chunk before pushing the new one
//! (DROP_OLDEST) and counts drops in an [`AtomicU64`] so the next chunk's
//! `dropped_before` reflects the loss. The consumer uses `try_pop()`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use ringbuf::traits::{Consumer, Observer, RingBuffer};
use ringbuf::HeapRb;

use crate::types::SecondaryChunk;

type Shared = Arc<Mutex<HeapRb<SecondaryChunk>>>;

/// Create a secondary-chunk ring with capacity `capacity_chunks`.
///
/// The producer goes to the intake/processing thread, the consumer to the poll
/// thread. The `dropped` counter counts chunks evicted by DROP_OLDEST and is
/// written into the next pushed chunk's `dropped_before`.
pub fn secondary_chunk_ring(
    capacity_chunks: usize,
) -> (SecondaryChunkProducer, SecondaryChunkConsumer) {
    let cap = capacity_chunks.max(1);
    let rb: Shared = Arc::new(Mutex::new(HeapRb::<SecondaryChunk>::new(cap)));
    let dropped = Arc::new(AtomicU64::new(0));
    (
        SecondaryChunkProducer {
            rb: rb.clone(),
            dropped: dropped.clone(),
        },
        SecondaryChunkConsumer { rb, dropped },
    )
}

/// Processing-thread handle. Pushes with a DROP_OLDEST policy.
pub struct SecondaryChunkProducer {
    rb: Shared,
    dropped: Arc<AtomicU64>,
}

impl SecondaryChunkProducer {
    /// Push a chunk. When full, evict the oldest (DROP_OLDEST) and count it.
    ///
    /// `dropped_before` receives the cumulative number of chunks dropped so far
    /// (including this push's eviction), matching the primary ring's semantics.
    /// Returns `Some(total)` when this push evicted a chunk, else `None`.
    pub fn push(&mut self, mut chunk: SecondaryChunk) -> Option<u64> {
        let mut rb = self.rb.lock().unwrap_or_else(|e| e.into_inner());

        let will_evict = rb.is_full();
        if will_evict {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        let total = self.dropped.load(Ordering::Relaxed);
        chunk.dropped_before = u32::try_from(total).unwrap_or(u32::MAX);

        let evicted = rb.push_overwrite(chunk);
        drop(rb);

        debug_assert_eq!(evicted.is_some(), will_evict);

        if will_evict {
            Some(total)
        } else {
            None
        }
    }

    /// Cumulative number of chunks dropped via DROP_OLDEST.
    pub fn dropped_count(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// Poll-thread handle. Consumes with `try_pop`.
pub struct SecondaryChunkConsumer {
    rb: Shared,
    dropped: Arc<AtomicU64>,
}

impl SecondaryChunkConsumer {
    /// Pop the oldest chunk, or `None` if empty (non-blocking).
    pub fn try_pop(&mut self) -> Option<SecondaryChunk> {
        let mut rb = self.rb.lock().unwrap_or_else(|e| e.into_inner());
        rb.try_pop()
    }

    /// Number of chunks currently buffered.
    pub fn len(&self) -> usize {
        self.rb
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .occupied_len()
    }

    /// Whether the ring is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Cumulative number of chunks dropped via DROP_OLDEST.
    pub fn dropped_count(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ChunkFlags;

    fn chunk(seq: u64) -> SecondaryChunk {
        SecondaryChunk {
            samples: vec![0.0; 320],
            frames: 320,
            pts_ns: seq as i64 * 20_000_000,
            seq,
            flags: ChunkFlags::empty(),
            dropped_before: 0,
            peak: 0.0,
            rms: 0.0,
        }
    }

    #[test]
    fn fifo_order_when_not_full() {
        let (mut p, mut c) = secondary_chunk_ring(4);
        assert!(c.is_empty());
        for s in 0..3 {
            assert_eq!(p.push(chunk(s)), None);
        }
        assert_eq!(c.len(), 3);
        assert_eq!(c.try_pop().unwrap().seq, 0);
        assert_eq!(c.try_pop().unwrap().seq, 1);
        assert_eq!(c.try_pop().unwrap().seq, 2);
        assert!(c.try_pop().is_none());
    }

    #[test]
    fn drop_oldest_when_full_and_counts() {
        let (mut p, mut c) = secondary_chunk_ring(2);
        assert_eq!(p.push(chunk(0)), None);
        assert_eq!(p.push(chunk(1)), None);
        assert_eq!(p.dropped_count(), 0);

        assert_eq!(p.push(chunk(2)), Some(1));
        assert_eq!(p.push(chunk(3)), Some(2));

        let first = c.try_pop().unwrap();
        assert_eq!(first.seq, 2);
        assert_eq!(first.dropped_before, 1);
        let second = c.try_pop().unwrap();
        assert_eq!(second.seq, 3);
        assert_eq!(second.dropped_before, 2);
        assert!(c.try_pop().is_none());
    }

    #[test]
    fn dropped_count_is_shared_between_ends() {
        let (mut p, c) = secondary_chunk_ring(1);
        p.push(chunk(0));
        p.push(chunk(1)); // evicts seq0
        assert_eq!(p.dropped_count(), 1);
        assert_eq!(c.dropped_count(), 1);
    }
}
