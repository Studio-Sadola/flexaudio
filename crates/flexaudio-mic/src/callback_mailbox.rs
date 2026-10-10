// One preallocated slot for the first CPAL runtime failure of a capture generation.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub(crate) struct CallbackMailbox {
    claimed: AtomicBool,
    ready: AtomicBool,
    error: UnsafeCell<Option<cpal::StreamError>>,
    stop_flag: Arc<AtomicBool>,
}

// SAFETY: A single successful claimed CAS owns the only write. The writer
// publishes it with ready Release; exactly one ready Acquire swap can take it.
// The slot is never reused. Arc keeps the slot alive until all callbacks exit.
// StreamError is Send, and no reference to the slot's contents escapes.
unsafe impl Sync for CallbackMailbox {}

impl CallbackMailbox {
    pub(crate) fn new(stop_flag: Arc<AtomicBool>) -> Self {
        Self {
            claimed: AtomicBool::new(false),
            ready: AtomicBool::new(false),
            error: UnsafeCell::new(None),
            stop_flag,
        }
    }

    pub(crate) fn record(&self, error: cpal::StreamError) {
        if self
            .claimed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        // Close PCM before publication. Move the CPAL-owned error without
        // formatting, allocation, locking, logging, or an unbounded retry.
        self.stop_flag.store(true, Ordering::SeqCst);
        // SAFETY: Only the successful claimed CAS can write, before ready.
        unsafe { *self.error.get() = Some(error) };
        self.ready.store(true, Ordering::Release);
    }

    pub(crate) fn take(&self) -> Option<cpal::StreamError> {
        if !self.ready.swap(false, Ordering::Acquire) {
            return None;
        }
        // SAFETY: Acquire observes the completed write. Only this swap winner
        // reads; claimed stays true and prevents all subsequent writes.
        unsafe { (*self.error.get()).take() }
    }
}
