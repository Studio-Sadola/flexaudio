//! SPSC, real-time-safe ring for raw interleaved f32 device frames, backed by rtrb.
//!
//! The producer (real-time callback) pushes slices without blocking. When full, it increments
//! the overflow counter ([`AtomicU64`]) and drops the excess, so the real-time thread never blocks.
//! The consumer pops data on the capture thread.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use rtrb::{Consumer, Producer, RingBuffer};

/// Create a raw-frame ring. `capacity_samples` is the capacity in f32 samples.
///
/// Pass the returned producer to the real-time callback thread and the consumer to the capture
/// thread (SPSC). Both share the `overflow` counter, which tracks dropped samples.
pub fn raw_ring(capacity_samples: usize) -> (RawProducer, RawConsumer) {
    let cap = capacity_samples.max(1);
    let (prod, cons) = RingBuffer::<f32>::new(cap);
    let overflow = Arc::new(AtomicU64::new(0));
    (
        RawProducer {
            inner: prod,
            overflow: overflow.clone(),
        },
        RawConsumer {
            inner: cons,
            overflow,
        },
    )
}

/// Handle for the real-time callback side. Only performs nonblocking pushes.
pub struct RawProducer {
    inner: Producer<f32>,
    overflow: Arc<AtomicU64>,
}

impl RawProducer {
    /// Push an interleaved sample slice without blocking.
    ///
    /// Writes as many samples as fit, drops the rest, and adds them to the overflow counter.
    /// Returns the number of samples written. Never blocks.
    pub fn push_slice(&mut self, samples: &[f32]) -> usize {
        if samples.is_empty() {
            return 0;
        }
        let free = self.inner.slots();
        let writable = free.min(samples.len());

        if writable > 0 {
            // Write the whole chunk without allocating, using write_chunk_uninit.
            if let Ok(mut chunk) = self.inner.write_chunk_uninit(writable) {
                let (a, b) = chunk.as_mut_slices();
                let (head, tail) = samples.split_at(a.len().min(samples.len()));
                for (dst, &src) in a.iter_mut().zip(head.iter()) {
                    dst.write(src);
                }
                let tail = &tail[..b.len().min(tail.len())];
                for (dst, &src) in b.iter_mut().zip(tail.iter()) {
                    dst.write(src);
                }
                // SAFETY: initialized exactly `writable` MaybeUninit values.
                unsafe { chunk.commit_all() };
            }
        }

        let dropped = samples.len() - writable;
        if dropped > 0 {
            self.overflow.fetch_add(dropped as u64, Ordering::Relaxed);
        }
        writable
    }

    /// Total number of samples dropped so far.
    pub fn overflow_count(&self) -> u64 {
        self.overflow.load(Ordering::Relaxed)
    }
}

/// Handle for the capture thread. Pops samples.
pub struct RawConsumer {
    inner: Consumer<f32>,
    overflow: Arc<AtomicU64>,
}

impl RawConsumer {
    /// Pop up to `dst.len()` available samples into `dst`. Returns the number popped.
    pub fn pop_slice(&mut self, dst: &mut [f32]) -> usize {
        let avail = self.inner.slots();
        let n = avail.min(dst.len());
        if n == 0 {
            return 0;
        }
        if let Ok(chunk) = self.inner.read_chunk(n) {
            let (a, b) = chunk.as_slices();
            let (alen, blen) = (a.len(), b.len());
            dst[..alen].copy_from_slice(a);
            dst[alen..alen + blen].copy_from_slice(b);
            chunk.commit_all();
            alen + blen
        } else {
            0
        }
    }

    /// Pop one sample, or return `None` if empty.
    pub fn pop(&mut self) -> Option<f32> {
        self.inner.pop().ok()
    }

    /// Number of samples available to pop.
    pub fn available(&self) -> usize {
        self.inner.slots()
    }

    /// Total number of samples dropped by the producer so far.
    pub fn overflow_count(&self) -> u64 {
        self.overflow.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_and_pop_roundtrip() {
        let (mut p, mut c) = raw_ring(16);
        let n = p.push_slice(&[1.0, 2.0, 3.0, 4.0]);
        assert_eq!(n, 4);
        let mut out = [0.0f32; 4];
        let got = c.pop_slice(&mut out);
        assert_eq!(got, 4);
        assert_eq!(out, [1.0, 2.0, 3.0, 4.0]);
        assert_eq!(p.overflow_count(), 0);
    }

    #[test]
    fn overflow_counts_dropped_and_never_blocks() {
        let (mut p, mut c) = raw_ring(4);
        // Push 6 samples into a capacity of 4: write 4 and drop 2.
        let written = p.push_slice(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
        assert_eq!(written, 4);
        assert_eq!(p.overflow_count(), 2);
        assert_eq!(c.overflow_count(), 2);

        let mut out = [0.0f32; 8];
        let got = c.pop_slice(&mut out);
        assert_eq!(got, 4);
        assert_eq!(&out[..4], &[1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn wraps_around() {
        let (mut p, mut c) = raw_ring(4);
        p.push_slice(&[1.0, 2.0, 3.0]);
        let mut out = [0.0f32; 2];
        c.pop_slice(&mut out); // Consume 2 and advance the read index.
        assert_eq!(out, [1.0, 2.0]);
        // The remaining 1 plus 3 new samples fills the ring. Verify wrapped writes.
        let w = p.push_slice(&[4.0, 5.0, 6.0]);
        assert_eq!(w, 3);
        let mut out2 = [0.0f32; 4];
        let got = c.pop_slice(&mut out2);
        assert_eq!(got, 4);
        assert_eq!(out2, [3.0, 4.0, 5.0, 6.0]);
    }

    /// Pushing an empty slice returns 0 and does not increment overflow (early return path).
    #[test]
    fn push_empty_is_noop() {
        let (mut p, _c) = raw_ring(4);
        assert_eq!(p.push_slice(&[]), 0);
        assert_eq!(p.overflow_count(), 0);
    }

    /// Pushing to a full ring drops everything (`writable=0`) and increments overflow by that
    /// number of samples. This also verifies that the real-time path does not block.
    #[test]
    fn full_ring_drops_entire_push() {
        let (mut p, _c) = raw_ring(4);
        assert_eq!(p.push_slice(&[1.0, 2.0, 3.0, 4.0]), 4); // Fill the ring.
                                                            // No room remains; drop all 5 samples.
        let w = p.push_slice(&[5.0; 5]);
        assert_eq!(w, 0, "full ring cannot accept any samples");
        assert_eq!(
            p.overflow_count(),
            5,
            "all 5 samples are counted as overflow"
        );
    }

    /// `pop_slice` pops only the available samples even if `dst` is larger (prevents off-by-one).
    /// Verify a `dst` larger than the available data and that popping an empty ring returns 0.
    #[test]
    fn pop_slice_respects_available_and_dst_len() {
        let (mut p, mut c) = raw_ring(8);
        p.push_slice(&[1.0, 2.0, 3.0]);
        assert_eq!(c.available(), 3);
        // Pop only the 3 available samples, even though dst is larger.
        let mut big = [0.0f32; 16];
        assert_eq!(c.pop_slice(&mut big), 3);
        assert_eq!(&big[..3], &[1.0, 2.0, 3.0]);
        // The ring is empty, so the next pop returns 0.
        assert_eq!(c.available(), 0);
        assert_eq!(c.pop_slice(&mut big), 0);
    }

    /// Repeated drops keep incrementing the u64 overflow counter beyond u32::MAX without
    /// saturating (`overflow` uses `AtomicU64`, separate from `dropped_before`'s u32 saturation).
    #[test]
    fn overflow_counter_exceeds_u32_max() {
        let (mut p, _c) = raw_ring(1);
        // Fill the ring with one sample; all subsequent samples will be dropped.
        assert_eq!(p.push_slice(&[0.0]), 1);
        // Drop more than u32::MAX samples. A single large slice can add many at once.
        let big = vec![0.0f32; 1000];
        let over_u32 = u64::from(u32::MAX) + 2_000;
        let mut total_dropped = 0u64;
        while total_dropped < over_u32 {
            let w = p.push_slice(&big);
            assert_eq!(w, 0, "full ring cannot accept any samples");
            total_dropped += big.len() as u64;
        }
        assert!(
            p.overflow_count() > u64::from(u32::MAX),
            "overflow accumulates beyond u32::MAX: {}",
            p.overflow_count()
        );
    }

    /// A single `pop()` returns samples one at a time in FIFO order, then `None` when empty.
    #[test]
    fn single_pop_is_fifo_then_none() {
        let (mut p, mut c) = raw_ring(4);
        p.push_slice(&[10.0, 20.0]);
        assert_eq!(c.pop(), Some(10.0));
        assert_eq!(c.pop(), Some(20.0));
        assert_eq!(c.pop(), None);
    }

    /// A capacity of 0 is clamped to at least 1 with `max(1)`, and push/pop work without panicking.
    #[test]
    fn zero_capacity_is_clamped_to_one() {
        let (mut p, mut c) = raw_ring(0);
        assert_eq!(
            p.push_slice(&[7.0, 8.0]),
            1,
            "capacity is clamped to 1 and accepts one sample"
        );
        assert_eq!(p.overflow_count(), 1);
        assert_eq!(c.pop(), Some(7.0));
    }

    /// The producer and consumer share the overflow counter (the same `Arc`).
    #[test]
    fn overflow_count_is_shared_between_ends() {
        let (mut p, c) = raw_ring(2);
        p.push_slice(&[1.0, 2.0, 3.0, 4.0]); // Write 2 and drop 2.
        assert_eq!(p.overflow_count(), 2);
        assert_eq!(
            c.overflow_count(),
            2,
            "consumer sees the same overflow count"
        );
    }
}
