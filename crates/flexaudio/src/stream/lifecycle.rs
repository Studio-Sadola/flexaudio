//! Capture startup and the single retained checked teardown.
use super::*;
impl Stream {
    /// Start capture, the intake worker and watchdog. Repeated calls while running are a no-op.
    /// A stopped stream is spent; native backend restarts happen inside the same intake lifetime.
    pub fn start(&mut self) -> Result<()> {
        if let Some(error) = self.terminal_error() {
            return Err(error);
        }
        if self.started {
            return Ok(());
        }
        // The intake owns these producers for its lifetime. Reject a spent
        // stream before changing state or starting the backend.
        let chunk_producer = self
            .shared
            .chunk_producer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .ok_or_else(|| Error::InvalidState("chunk producer already taken".into()))?;
        let secondary_producer = self
            .shared
            .secondary_producer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        let previous_report = self.shutdown.take();
        let old_cleanup_count = self
            .shared
            .cleanup
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len();
        let old_event_count = self.events.lock().unwrap_or_else(|e| e.into_inner()).len();
        self.shared.stopping.store(false, Ordering::SeqCst);
        self.shared.switching.store(false, Ordering::SeqCst);

        // A new start does not carry over a paused state (even if the previous run was stopped while
        // paused, restart in the normal state).
        self.shared.paused.store(false, Ordering::SeqCst);

        // Start a new zero-based clock for this recording (do not carry over the previous epoch).
        self.shared
            .recording_epoch_ns
            .store(i64::MIN, Ordering::SeqCst);

        // First backend startup: create RawRing and pass its sink to the backend.
        if let Err(error) = Self::open_backend_once(&self.shared, GenerationChange::Initial) {
            // Permission denial already closes delivery and stops the backend
            // in open_backend_once. Other failures still need startup cleanup.
            let _ = self.stop_checked();
            let cleanup = cleanup_error_since(&self.shared, old_cleanup_count);
            *self
                .shared
                .chunk_producer
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(chunk_producer);
            *self
                .shared
                .secondary_producer
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = secondary_producer;
            return Err(with_cleanup(error, cleanup));
        }

        // Intake takes its own snapshot when scheduled, since this format hint
        // can become stale before the worker starts.
        let worker_shared = self.shared.clone();
        // Recover and continue even if poisoned (only reads the inner (u32, u16)).
        let initial_native = *self
            .shared
            .native_format
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let output = self.config.output;
        let secondary_output = self.config.secondary_output;
        let worker = match thread::Builder::new()
            .name("flexaudio-intake".into())
            .spawn(move || {
                run_intake(
                    worker_shared,
                    chunk_producer,
                    secondary_producer,
                    initial_native,
                    output,
                    secondary_output,
                );
            }) {
            Ok(worker) => worker,
            Err(error) => {
                let primary = Error::Backend(format!("spawn intake thread: {error}"))
                    .with_context(ErrorContext::new(Operation::Start));
                let _ = self.stop_checked();
                let cleanup = cleanup_error_since(&self.shared, old_cleanup_count);
                return Err(with_cleanup(primary, cleanup));
            }
        };
        self.worker = Some(worker);

        // Watchdog thread.
        let wd_shared = self.shared.clone();
        let watchdog = match thread::Builder::new()
            .name("flexaudio-watchdog".into())
            .spawn(move || {
                run_watchdog(wd_shared);
            }) {
            Ok(watchdog) => watchdog,
            Err(error) => {
                let primary = Error::Backend(format!("spawn watchdog thread: {error}"))
                    .with_context(ErrorContext::new(Operation::Start));
                let _ = self.stop_checked();
                let cleanup = cleanup_error_since(&self.shared, old_cleanup_count);
                return Err(with_cleanup(primary, cleanup));
            }
        };
        self.watchdog = Some(watchdog);

        if previous_report.is_some() {
            // Clear only the previous completed outcome; retain this attempt's observations.
            self.shared
                .cleanup
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .drain(..old_cleanup_count);
            self.events
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .drain(..old_event_count);
        }
        self.started = true;
        self.shutdown = None;
        Ok(())
    }

    /// Stop capture and join all threads.
    ///
    /// Safe for reentry and repeated stop calls. After stop, drain chunks already buffered in the
    /// ring with [`poll_chunk`](Self::poll_chunk).
    pub fn stop(&mut self) {
        let _ = self.stop_checked();
    }

    /// Stop producers before final intake drain, join every worker and retain the outcome.
    /// Repeated calls return the identical outcome without duplicate events, joins or tails.
    pub fn stop_checked(&mut self) -> Result<()> {
        if let Some(report) = &self.shutdown {
            return report.result();
        }
        // Block new recovery work while intake continues until producer quiescence.
        self.shared.switching.store(true, Ordering::SeqCst);
        {
            let mut be = self
                .shared
                .backend
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            stop_backend_reconciling(&self.shared, &mut be, true);
        }
        for handle in [self.worker.take(), self.watchdog.take()]
            .into_iter()
            .flatten()
        {
            if handle.join().is_err() {
                self.shared.record_cleanup(
                    Error::Backend("capture worker panicked".into())
                        .with_context(ErrorContext::new(Operation::Join)),
                );
            }
        }
        // One shared loss drain also covers acquisition failure and terminal stop.
        self.shared.drain_capture_losses();
        self.started = false;
        let report = ShutdownReport::new(
            self.shared.terminal.error(),
            self.shared
                .cleanup
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
        );
        let result = report.result();
        self.shutdown = Some(report);
        result
    }

    /// Completed shutdown outcome, or None while capture has not finished shutting down.
    pub fn shutdown_report(&self) -> Option<ShutdownReport> {
        self.shutdown.clone()
    }
}
