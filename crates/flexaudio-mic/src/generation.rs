// Control-thread event admission and cancellation for a single owner generation.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};

use flexaudio_core::types::Event;

pub(crate) struct Generation {
    stop_flag: Arc<AtomicBool>,
    gate: Mutex<()>,
    #[cfg(any(target_os = "macos", test))]
    events: mpsc::Sender<Event>,
}

impl Generation {
    pub(crate) fn new(stop_flag: Arc<AtomicBool>, _events: mpsc::Sender<Event>) -> Self {
        Self {
            stop_flag,
            gate: Mutex::new(()),
            #[cfg(any(target_os = "macos", test))]
            events: _events,
        }
    }

    // This lock is confined to control/owner threads, never audio callbacks.
    #[cfg(any(target_os = "macos", test))]
    pub(crate) fn emit(&self, event: Event) {
        let _gate = self.gate.lock().unwrap_or_else(|error| error.into_inner());
        if self.stop_flag.load(Ordering::SeqCst) {
            return;
        }
        if !matches!(
            event,
            Event::PermissionPending { .. } | Event::PermissionGranted
        ) {
            self.stop_flag.store(true, Ordering::SeqCst);
        }
        let _ = self.events.send(event);
    }

    pub(crate) fn cancel(&self) {
        let _gate = self.gate.lock().unwrap_or_else(|error| error.into_inner());
        self.stop_flag.store(true, Ordering::SeqCst);
    }
}
