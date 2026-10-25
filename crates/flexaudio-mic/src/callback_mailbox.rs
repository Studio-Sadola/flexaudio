// Preallocated failure slots for one capture generation; callback writes never wait.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use flexaudio_core::Error;

const CAPACITY: usize = 4;

pub(crate) enum CallbackFailure {
    Native(cpal::StreamError),
    Panicked,
}

struct Slot {
    claimed: AtomicBool,
    ready: AtomicBool,
    error: UnsafeCell<Option<CallbackFailure>>,
}

pub(crate) struct CallbackMailbox {
    slots: [Slot; CAPACITY],
    consumed: AtomicUsize,
    overflow: AtomicUsize,
    stop_flag: Arc<AtomicBool>,
}

// SAFETY: Each slot's successful claimed CAS owns its only write. The writer
// publishes with ready Release; exactly one ready Acquire swap can take it.
// Slots are never reused. Arc keeps them alive until all callbacks exit.
// StreamError is Send, and no reference to the slot's contents escapes.
unsafe impl Sync for CallbackMailbox {}

impl CallbackMailbox {
    pub(crate) fn new(stop_flag: Arc<AtomicBool>) -> Self {
        Self {
            slots: std::array::from_fn(|_| Slot {
                claimed: AtomicBool::new(false),
                ready: AtomicBool::new(false),
                error: UnsafeCell::new(None),
            }),
            consumed: AtomicUsize::new(0),
            overflow: AtomicUsize::new(0),
            stop_flag,
        }
    }

    pub(crate) fn guard(&self, callback: impl FnOnce()) {
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(callback)).is_err() {
            self.record_failure(CallbackFailure::Panicked);
        }
    }

    pub(crate) fn record(&self, error: cpal::StreamError) {
        self.record_failure(CallbackFailure::Native(error));
    }

    fn record_failure(&self, error: CallbackFailure) {
        for slot in &self.slots {
            if slot
                .claimed
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                // Close PCM before publication. Move the CPAL-owned error without
                // formatting, allocation, locking, logging, or an unbounded retry.
                self.stop_flag.store(true, Ordering::SeqCst);
                // SAFETY: Only the successful claimed CAS can write, before ready.
                unsafe { *slot.error.get() = Some(error) };
                slot.ready.store(true, Ordering::Release);
                return;
            }
        }
        self.overflow.fetch_add(1, Ordering::Relaxed);
    }

    /// Reconcile on the owner thread, where formatting and allocation are allowed.
    pub(crate) fn take(&self) -> Option<Error> {
        let consumed = self.consumed.load(Ordering::Relaxed);
        if let Some(slot) = self.slots.get(consumed) {
            if !slot.ready.swap(false, Ordering::Acquire) {
                return None;
            }
            // SAFETY: Acquire observes the completed write. Only this swap winner
            // reads; claimed stays true and prevents all subsequent writes.
            let error = unsafe { (*slot.error.get()).take() };
            self.consumed.store(consumed + 1, Ordering::Relaxed);
            return error.map(|error| match error {
                CallbackFailure::Native(cpal::StreamError::DeviceNotAvailable) => Error::DeviceLost,
                CallbackFailure::Native(cpal::StreamError::BackendSpecific { err }) => {
                    Error::Backend(format!("microphone capture failed: {err}"))
                }
                CallbackFailure::Panicked => Error::Backend(
                    "microphone data callback panicked; reopen the stream and retry".into(),
                ),
            });
        }
        let overflow = self.overflow.swap(0, Ordering::Relaxed);
        (overflow != 0).then(|| {
            Error::Backend(format!(
                "microphone callback failure mailbox full: {overflow} further callback failures were not retained"
            ))
        })
    }
}
