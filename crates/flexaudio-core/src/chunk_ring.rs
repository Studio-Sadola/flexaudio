//! SPSC ring for completed [`AudioChunk`] values, backed by ringbuf.
//!
//! When full, pop the oldest item and push the new one (DROP_OLDEST). Count dropped items
//! with [`AtomicU64`] and record the count in the next chunk's `dropped_before`. Consumers
//! use `try_pop()`.
//!
//! ringbuf's overwrite operation (`push_overwrite`) requires the producer to pop the oldest
//! item, which also touches the consumer index and breaks the lock-free SPSC assumption
//! (concurrent overwrite requires a lock). This ring's producer runs on the normal-priority
//! ingest/processing thread, not the RT thread, so a short [`Mutex`] protects the ring itself.
//! The RT path ([`mod@crate::raw_ring`]) never takes this lock.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use ringbuf::traits::{Consumer, Observer, RingBuffer};
use ringbuf::HeapRb;

use crate::types::AudioChunk;

type Shared = Arc<Mutex<HeapRb<AudioChunk>>>;

/// Create a chunk ring with capacity `capacity_chunks`.
///
/// Give the producer to the processing thread and the consumer to the poll thread. The
/// `dropped` counter tracks chunks discarded by DROP_OLDEST and is recorded in the next
/// chunk pushed as `dropped_before`.
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

/// Handle for the processing thread. Pushes chunks using the DROP_OLDEST policy.
pub struct ChunkProducer {
    rb: Shared,
    dropped: Arc<AtomicU64>,
}

impl ChunkProducer {
    /// Push a chunk. If full, discard the oldest chunk (DROP_OLDEST) and count it.
    ///
    /// Set `dropped_before` to the cumulative number of chunks discarded before this chunk
    /// was inserted, including the one evicted to make room for this chunk. Consumers can
    /// find the number dropped since the previous chunk from the difference between
    /// consecutive `dropped_before` values, and the total from the absolute value.
    ///
    /// Return `Some(total_dropped)` if this push dropped a chunk (for deciding whether to
    /// fire [`crate::types::Event::ChunkDropped`]); otherwise return `None`.
    pub fn push(&mut self, mut chunk: AudioChunk) -> Option<u64> {
        // Poisoning (a panic while another thread held the lock) does not corrupt the ring,
        // so recover the inner value and continue instead of causing a second panic.
        let mut rb = self.rb.lock().unwrap_or_else(|e| e.into_inner());

        // Check first whether this push will evict the oldest item (the ring is full).
        let will_evict = rb.is_full();

        if will_evict {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        // Record the cumulative drop count, including any eviction caused by this push.
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

    /// Cumulative number of chunks discarded by DROP_OLDEST.
    pub fn dropped_count(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// Handle for the poll thread. Consume chunks with `try_pop`.
pub struct ChunkConsumer {
    rb: Shared,
    dropped: Arc<AtomicU64>,
}

impl ChunkConsumer {
    /// Take the oldest chunk, or return `None` without blocking if empty.
    pub fn try_pop(&mut self) -> Option<AudioChunk> {
        // Poisoning does not corrupt the ring, so recover and continue.
        let mut rb = self.rb.lock().unwrap_or_else(|e| e.into_inner());
        rb.try_pop()
    }

    /// Number of chunks currently buffered in the ring.
    pub fn len(&self) -> usize {
        // Poisoning does not corrupt the ring, so recover and continue.
        self.rb
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .occupied_len()
    }

    /// Whether the ring is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Cumulative number of chunks discarded by DROP_OLDEST.
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
        // Fill the ring to capacity 2.
        assert_eq!(p.push(chunk(0)), None);
        assert_eq!(p.push(chunk(1)), None);
        assert_eq!(p.dropped_count(), 0);

        // Full: discard the oldest (seq0) and insert seq2.
        let dropped_total = p.push(chunk(2));
        assert_eq!(dropped_total, Some(1));
        assert_eq!(p.dropped_count(), 1);

        // The next push drops one more item.
        let dropped_total = p.push(chunk(3));
        assert_eq!(dropped_total, Some(2));
        assert_eq!(p.dropped_count(), 2);

        // The two newest items remain: seq2 and seq3.
        let first = c.try_pop().unwrap();
        assert_eq!(first.seq, 2);
        // Cumulative drops before seq2 was inserted = 1 (seq0 was discarded).
        assert_eq!(first.dropped_before, 1);

        let second = c.try_pop().unwrap();
        assert_eq!(second.seq, 3);
        // Cumulative drops before seq3 was inserted = 2 (seq0 and seq1 were discarded).
        assert_eq!(second.dropped_before, 2);

        assert!(c.try_pop().is_none());
    }

    #[test]
    fn dropped_before_is_cumulative() {
        let (mut p, mut c) = chunk_ring(1);
        p.push(chunk(0)); // Inserted (dropped_before=0)
                          // Capacity 1 is full, so each push drops an item.
        p.push(chunk(1)); // Discard seq0; cumulative drops=1
        c.try_pop(); // Take seq1, leaving the ring empty
        let r = p.push(chunk(2)); // Inserted into the empty ring; no drops
        assert_eq!(r, None);
        let got = c.try_pop().unwrap();
        assert_eq!(got.seq, 2);
        // No new drops occurred, but the cumulative count remains 1.
        assert_eq!(got.dropped_before, 1);
        assert_eq!(p.dropped_count(), 1);
    }

    /// Capacity 0 is clamped to `max(1)` and works as a capacity-1 ring without panicking.
    #[test]
    fn zero_capacity_is_clamped_to_one() {
        let (mut p, mut c) = chunk_ring(0);
        assert!(c.is_empty());
        assert_eq!(p.push(chunk(0)), None); // One item is inserted.
        assert_eq!(c.len(), 1);
        // Full: the next push discards the oldest item.
        assert_eq!(p.push(chunk(1)), Some(1));
        let got = c.try_pop().unwrap();
        assert_eq!(got.seq, 1);
    }

    /// `len` / `is_empty` track push/pop and never exceed capacity (guards against off-by-one errors).
    #[test]
    fn len_tracks_occupancy_and_is_capped() {
        let (mut p, mut c) = chunk_ring(3);
        assert!(c.is_empty());
        for s in 0..3 {
            p.push(chunk(s));
        }
        assert_eq!(c.len(), 3);
        assert!(!c.is_empty());
        // Push two more items after filling: capacity stays at 3 as the oldest items are replaced.
        p.push(chunk(3));
        p.push(chunk(4));
        assert_eq!(c.len(), 3, "occupancy never exceeds capacity 3");
        // The three newest items remain: seq2, seq3, and seq4.
        assert_eq!(c.try_pop().unwrap().seq, 2);
        assert_eq!(c.try_pop().unwrap().seq, 3);
        assert_eq!(c.try_pop().unwrap().seq, 4);
        assert!(c.try_pop().is_none());
        assert!(c.is_empty());
    }

    /// `try_pop` on an empty ring returns `None` without blocking. Producer and consumer
    /// share the dropped counter.
    #[test]
    fn empty_pop_is_none_and_dropped_is_shared() {
        let (mut p, mut c) = chunk_ring(2);
        assert!(c.try_pop().is_none());
        // With capacity 2, cause one drop.
        p.push(chunk(0));
        p.push(chunk(1));
        p.push(chunk(2)); // Discard seq0.
        assert_eq!(p.dropped_count(), 1);
        assert_eq!(c.dropped_count(), 1, "consumer sees the same dropped count");
    }
}
