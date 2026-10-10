//! Allocation-free realtime rejection summaries, materialized off callback.
use crate::loss::CaptureRejection;
use crate::{AudioLoss, Result};
use std::num::NonZeroU64;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

#[cfg(not(target_has_atomic = "64"))]
compile_error!("capture diagnostics require lock-free 64-bit atomics");

/// Clone before entering the callback; recording never allocates or locks.
#[derive(Clone)]
pub struct CaptureDiagnostics {
    inner: Arc<Pending>,
}
struct Pending {
    summaries: [AtomicU64; 3],
    rate: u32,
    channels: u16,
}
impl CaptureDiagnostics {
    /// Configure a handle for one negotiated generation (off callback).
    pub fn new(rate: u32, channels: u16) -> Self {
        Self {
            inner: Arc::new(Pending {
                summaries: std::array::from_fn(|_| AtomicU64::new(0)),
                rate,
                channels,
            }),
        }
    }
    fn record(&self, index: usize, samples: Option<NonZeroU64>) {
        let summary = &self.inner.summaries[index];
        let Some(samples) = samples else {
            summary.store(u64::MAX, Ordering::Release);
            return;
        };
        let mut previous = summary.load(Ordering::Acquire);
        for _ in 0..8 {
            if previous == u64::MAX {
                return;
            }
            let next = previous.saturating_add(samples.get());
            match summary.compare_exchange_weak(previous, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return,
                Err(current) => previous = current,
            }
        }
        // Saturate rather than silently lose a diagnostic under contention.
        summary.store(u64::MAX, Ordering::Release);
    }
    /// Reject a corrupt buffer without trusting invalid metadata for a count.
    pub fn record_corrupt_buffer(&self, samples: Option<NonZeroU64>) {
        self.record(0, samples);
    }
    /// Reject malformed native buffer layout.
    pub fn record_malformed_buffer(&self, samples: Option<NonZeroU64>) {
        self.record(1, samples);
    }
    /// Reject a callback whose exclusive state cannot be accessed.
    pub fn record_callback_rejected(&self, samples: Option<NonZeroU64>) {
        self.record(2, samples);
    }
    /// Drain once after a control-thread poll or producer shutdown; no duplicate reports.
    pub fn drain(&self) -> Result<Vec<AudioLoss>> {
        let mut losses = Vec::new();
        for (summary, reason) in self.inner.summaries.iter().zip([
            CaptureRejection::CorruptBuffer,
            CaptureRejection::MalformedBuffer,
            CaptureRejection::CallbackRejected,
        ]) {
            let pending = summary.swap(0, Ordering::AcqRel);
            if pending != 0 {
                losses.push(AudioLoss::rejected(
                    reason,
                    if pending == u64::MAX {
                        None
                    } else {
                        NonZeroU64::new(pending)
                    },
                    self.inner.rate,
                    self.inner.channels,
                )?);
            }
        }
        Ok(losses)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn known_unknown_saturated_and_final_drain_are_observable_once() {
        let diagnostics = CaptureDiagnostics::new(48_000, 2);
        diagnostics.record_corrupt_buffer(NonZeroU64::new(100));
        diagnostics.record_corrupt_buffer(NonZeroU64::new(100));
        diagnostics.record_malformed_buffer(None);
        diagnostics.record_malformed_buffer(NonZeroU64::new(7));
        diagnostics.record_callback_rejected(NonZeroU64::new(u64::MAX - 1));
        diagnostics.record_callback_rejected(NonZeroU64::new(2));
        let losses = diagnostics.drain().unwrap();
        assert_eq!(losses.len(), 3);
        assert_eq!(losses[0].samples().unwrap().get(), 200);
        assert_eq!(losses[1].samples(), None);
        assert_eq!(losses[2].samples(), None);
        for loss in &losses {
            assert_eq!((loss.sample_rate(), loss.channels()), (48_000, 2));
        }
        assert!(diagnostics.drain().unwrap().is_empty());
    }
    #[test]
    fn concurrent_recording_preserves_every_observation() {
        let diagnostics = CaptureDiagnostics::new(48_000, 2);
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let diagnostics = diagnostics.clone();
                std::thread::spawn(move || {
                    for _ in 0..1_000 {
                        diagnostics.record_corrupt_buffer(NonZeroU64::new(1));
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        let loss = diagnostics.drain().unwrap()[0];
        assert!(loss.samples().is_none_or(|samples| samples.get() == 4_000));
        assert!(diagnostics.drain().unwrap().is_empty());
    }
}
