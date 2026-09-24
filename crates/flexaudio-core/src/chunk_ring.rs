//! SPSC ring of finished [`AudioChunk`]s (backed by ringbuf).
//!
//! When full, it pops the oldest and pushes the new one (DROP_OLDEST). The drop count is counted
//! with an [`AtomicU64`] and reflected in the next chunk's `dropped_before`. The consumer calls
//! `try_pop()`.
//!
//! ringbuf's overwrite (`push_overwrite`) requires the producer to pop the oldest element, which
//! also touches the consumer-side index and so breaks the lock-free SPSC assumption (concurrent
//! overwrite requires a lock). The producer of this ring is not an RT thread but the
//! ingest/processing thread (normal priority), so the ring itself is protected by a short-held
//! [`Mutex`]. The RT path ([`mod@crate::raw_ring`]) never touches this lock.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use ringbuf::traits::{Consumer, Observer, RingBuffer};
use ringbuf::HeapRb;

use crate::types::AudioChunk;

type Shared = Arc<Mutex<HeapRb<AudioChunk>>>;

/// Creates a chunk ring with capacity `capacity_chunks`.
///
/// The producer goes to the processing thread and the consumer to the poll thread. The `dropped`
/// counter counts chunks discarded by DROP_OLDEST and is reflected in the `dropped_before` of the
/// next pushed chunk.
pub fn chunk_ring(capacity_chunks: usize) -> (ChunkProducer, ChunkConsumer) {
    let cap = capacity_chunks.max(1);
    let rb: Shared = Arc::new(Mutex::new(HeapRb::<AudioChunk>::new(cap)));
    let dropped = Arc::new(AtomicU64::new(0));
    (
        ChunkProducer {
            rb: rb.clone(),
            dropped: dropped.clone(),
        },
        ChunkConsumer { rb, dropped },
    )
}

/// Processing-thread-side handle. Pushes with the DROP_OLDEST policy.
pub struct ChunkProducer {
    rb: Shared,
    dropped: Arc<AtomicU64>,
}

impl ChunkProducer {
    /// Pushes a chunk. If full, discards the oldest (DROP_OLDEST) and counts the discards.
    ///
    /// `dropped_before` is set to the cumulative number of chunks discarded up to the point this
    /// chunk enters (including the one eviction made for this chunk). The consumer can learn the
    /// number of chunks just missed from the difference in `dropped_before` between consecutive
    /// chunks, and the cumulative number missed from its absolute value.
    ///
    /// Returns `Some(cumulative drop count)` if this push caused a drop (usable to decide whether
    /// to fire [`crate::types::Event::ChunkDropped`]). Otherwise returns `None`.
    pub fn push(&mut self, mut chunk: AudioChunk) -> Option<u64> {
        // Even on poison (another thread panicked while holding the lock) the ring itself is
        // not corrupted, so recover the inner value and continue instead of cascading the panic.
        let mut rb = self.rb.lock().unwrap_or_else(|e| e.into_inner());

        // Check up front whether this push evicts the oldest (full).
        let will_evict = rb.is_full();

        if will_evict {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        // Record the cumulative drop count, including this push's eviction.
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

    /// Cumulative number of chunks discarded by DROP_OLDEST so far.
    pub fn dropped_count(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// Poll-thread-side handle. Consumes with `try_pop`.
pub struct ChunkConsumer {
    rb: Shared,
    dropped: Arc<AtomicU64>,
}

impl ChunkConsumer {
    /// Takes out the oldest chunk. Returns `None` if there is none (non-blocking).
    pub fn try_pop(&mut self) -> Option<AudioChunk> {
        // The ring is not corrupted even on poison, so recover and continue.
        let mut rb = self.rb.lock().unwrap_or_else(|e| e.into_inner());
        rb.try_pop()
    }

    /// Number of chunks currently held in the ring.
    pub fn len(&self) -> usize {
        // The ring is not corrupted even on poison, so recover and continue.
        self.rb
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .occupied_len()
    }

    /// Whether the ring is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Cumulative number of chunks discarded by DROP_OLDEST so far.
    pub fn dropped_count(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ChunkFlags;

    fn chunk(seq: u64) -> AudioChunk {
        AudioChunk {
            data: vec![0.0; 1920],
            frames: 960,
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
        let (mut p, mut c) = chunk_ring(4);
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
        let (mut p, mut c) = chunk_ring(2);
        // Fill capacity 2.
        assert_eq!(p.push(chunk(0)), None);
        assert_eq!(p.push(chunk(1)), None);
        assert_eq!(p.dropped_count(), 0);

        // Full -> discard the oldest (seq0) and insert seq2.
        let dropped_total = p.push(chunk(2));
        assert_eq!(dropped_total, Some(1));
        assert_eq!(p.dropped_count(), 1);

        // The next push drops one more.
        let dropped_total = p.push(chunk(3));
        assert_eq!(dropped_total, Some(2));
        assert_eq!(p.dropped_count(), 2);

        // What remains is the newest 2: seq2, seq3.
        let first = c.try_pop().unwrap();
        assert_eq!(first.seq, 2);
        // Cumulative drops up to seq2 entering = 1 (seq0 was discarded).
        assert_eq!(first.dropped_before, 1);

        let second = c.try_pop().unwrap();
        assert_eq!(second.seq, 3);
        // Cumulative drops up to seq3 entering = 2 (seq0 and seq1 were discarded).
        assert_eq!(second.dropped_before, 2);

        assert!(c.try_pop().is_none());
    }

    #[test]
    fn dropped_before_is_cumulative() {
        let (mut p, mut c) = chunk_ring(1);
        p.push(chunk(0)); // enters (dropped_before=0)
                          // Capacity 1 and full -> drops every time.
        p.push(chunk(1)); // seq0 discarded, cumulative drops=1
        c.try_pop(); // take seq1 -> ring becomes empty
        let r = p.push(chunk(2)); // there is room, so it enters with no drop
        assert_eq!(r, None);
        let got = c.try_pop().unwrap();
        assert_eq!(got.seq, 2);
        // No new drop, but the cumulative count is kept at 1.
        assert_eq!(got.dropped_before, 1);
        assert_eq!(p.dropped_count(), 1);
    }

    /// Capacity 0 is rounded up by `max(1)` and works as a capacity-1 ring (no panic).
    #[test]
    fn zero_capacity_is_clamped_to_one() {
        let (mut p, mut c) = chunk_ring(0);
        assert!(c.is_empty());
        assert_eq!(p.push(chunk(0)), None); // one enters.
        assert_eq!(c.len(), 1);
        // Full -> the next one discards the oldest.
        assert_eq!(p.push(chunk(1)), Some(1));
        let got = c.try_pop().unwrap();
        assert_eq!(got.seq, 1);
    }

    /// `len` / `is_empty` track push/pop and never exceed capacity (off-by-one guard).
    #[test]
    fn len_tracks_occupancy_and_is_capped() {
        let (mut p, mut c) = chunk_ring(3);
        assert!(c.is_empty());
        for s in 0..3 {
            p.push(chunk(s));
        }
        assert_eq!(c.len(), 3);
        assert!(!c.is_empty());
        // 2 more after full -> capacity stays 3 (the oldest is discarded and replaced).
        p.push(chunk(3));
        p.push(chunk(4));
        assert_eq!(c.len(), 3, "occupancy does not exceed capacity 3");
        // What remains is the newest 3: seq2,3,4.
        assert_eq!(c.try_pop().unwrap().seq, 2);
        assert_eq!(c.try_pop().unwrap().seq, 3);
        assert_eq!(c.try_pop().unwrap().seq, 4);
        assert!(c.try_pop().is_none());
        assert!(c.is_empty());
    }

    /// `try_pop` on an empty ring returns None (non-blocking). The producer and consumer share
    /// the dropped counter.
    #[test]
    fn empty_pop_is_none_and_dropped_is_shared() {
        let (mut p, mut c) = chunk_ring(2);
        assert!(c.try_pop().is_none());
        // Capacity 2 -> cause one drop.
        p.push(chunk(0));
        p.push(chunk(1));
        p.push(chunk(2)); // discards seq0.
        assert_eq!(p.dropped_count(), 1);
        assert_eq!(
            c.dropped_count(),
            1,
            "the consumer side observes the same dropped count"
        );
    }
}
