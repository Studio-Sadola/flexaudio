//! Source acquisition, generation publication and rollback.
use super::*;
impl Stream {
    // --- Internal ---

    /// (Re)start the current `shared.backend`, install a new RawRing/RawConsumer in shared state, and
    /// advance the generation. Used for both initial startup and watchdog reopen.
    ///
    /// Steps:
    /// 1. Read the current backend's [`native_format`](CaptureBackend::native_format).
    /// 2. Create a new RawRing with that rate/ch (do not carry over format residue from the old ring,
    ///    which would damage phase).
    /// 3. Start the backend.
    /// 4. Publish native format, RawConsumer, generation and flags under the raw-consumer lock.
    /// 5. Set `last_sample_ns` to now to avoid an immediate stall check.
    ///
    /// Acquire the backend lock only while starting (the caller must not hold the lock). The
    /// low-level switch ([`switch_backend`](Self::switch_backend)) directly replaces the backend and
    /// does not use this function, except when restoring the old source after a failed switch.
    pub(super) fn open_backend_once(
        shared: &Arc<SharedState>,
        change: GenerationChange,
    ) -> Result<()> {
        // Keep the backend stable from format lookup through raw-ring publication.
        let mut be = shared.backend.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(change, GenerationChange::Recovery)
            && (shared.switching.load(Ordering::SeqCst) || shared.stopping.load(Ordering::SeqCst))
        {
            return Err(Error::InvalidState("capture is stopping".into()));
        }
        let (rate, channels) = be.native_format();
        Normalizer::new(rate, channels, OutputFormat::default())?;
        shared.backend_stopped.store(false, Ordering::SeqCst);

        // New RawRing (do not carry over residue from the old format).
        let (producer, consumer) = raw_ring(RAW_RING_SAMPLES);
        let sink = RawSink::new(producer, rate, channels);
        let diagnostics = sink.diagnostics();

        {
            // Recover even if poisoned. If backend start() panics, catch_unwind converts it to
            // Error::Backend before mutex poisoning, so `?` returns it to the caller (start() returns
            // Err / watchdog emits RecoverableError).
            if let Some(error) = shared.terminal.error() {
                return Err(error);
            }
            if let Err(error) = start_backend_catching(&mut be, sink) {
                if is_terminal_kind(&error) {
                    shared.fail_terminal(error.clone());
                }
                // Failed acquisition still owns anything the backend may have started.
                // Quiesce it before draining its rejected-buffer summaries.
                shared.stop_backend_owned(&mut be);
                drain_unpublished_capture(shared, &consumer, &diagnostics, (rate, channels));
                return Err(error);
            }
        }

        // Install the new consumer in shared state and advance the generation (drop the old consumer).
        {
            // Recover and install it even if poisoned (only replace the inner Option).
            let mut rc = shared
                .raw_consumer
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            shared.publish_raw(&mut rc, consumer, diagnostics, (rate, channels), change);
        }

        // Treat startup as the last sample arrival to avoid an immediate stall check.
        shared
            .last_sample_ns
            .store(monotonic_now_ns(), Ordering::SeqCst);
        Ok(())
    }

    /// Low-level source switch. Replace the current backend with a new one while preserving chunk
    /// stream continuity (seq and PTS).
    ///
    /// `seq` is local to the intake thread and is not stored in the backend or `SharedState`, so it
    /// remains continuous if left untouched here. On a generation change, the intake thread rebuilds
    /// `Normalizer`/`ClockNormalizer` and re-anchors PTS to the actual arrival time of the new
    /// source's first sample, preserving monotonicity.
    ///
    /// Steps (increment generation once at the end; all atomics use SeqCst):
    /// - If not started, return [`Error::InvalidState`].
    /// - Set `switching = true` (prevent concurrent watchdog reopen).
    /// - Under the backend lock, stop the old backend → read the new backend's native format and
    ///   update `shared.native_format` → create a new RawRing → call `new_backend.start(sink)`.
    ///   - On success, replace the backend and install the new consumer (drop the old one).
    ///   - On failure, restart the old backend with [`open_backend_once`](Self::open_backend_once) and
    ///     continue the old source (preserve continuity). Set `discontinuity_pending`, increment
    ///     generation, set `switching=false`, and return `Err`.
    /// - On success, set `discontinuity_pending = true` (intentional switch, so do not set RECOVERED)
    ///   → increment generation once at the end → set `last_sample_ns = now` → set `switching = false`
    ///   → return `Ok`.
    ///
    /// Takes [`Box<dyn CaptureBackend>`] directly so mock backends can verify switch behavior. The
    /// high-level entry point is [`switch_source`](Self::switch_source).
    ///
    /// `#[doc(hidden)] pub`: not part of the public API (omitted from docs), but allows integration
    /// tests in another crate (`tests/integration.rs`) to call this with a MockBackend.
    #[doc(hidden)]
    pub fn switch_backend(&mut self, new_backend: Box<dyn CaptureBackend>) -> Result<()> {
        self.switch_backend_inner(new_backend, None)
    }

    /// Publish a backend and its denoise setting as one capture generation.
    #[doc(hidden)]
    pub fn switch_backend_with_denoise(
        &mut self,
        new_backend: Box<dyn CaptureBackend>,
        denoise: bool,
    ) -> Result<()> {
        self.switch_backend_inner(new_backend, Some(denoise))
    }

    fn switch_backend_inner(
        &mut self,
        new_backend: Box<dyn CaptureBackend>,
        denoise: Option<bool>,
    ) -> Result<()> {
        if let Some(error) = self.terminal_error() {
            return Err(error);
        }
        if !self.started {
            return Err(Error::InvalidState(
                "switch_backend is only available on a started stream".into(),
            ));
        }

        let (new_rate, new_channels) = new_backend.native_format();
        Normalizer::new(new_rate, new_channels, self.config.output)?;
        // Begin switch: pause the watchdog first to avoid racing its stall recovery.
        self.shared.switching.store(true, Ordering::SeqCst);

        // Under the backend lock, stop the old backend and start the new one as one operation.
        // Recover even if poisoned (the lock spans stop/start and may be poisoned; recovering it lets
        // the replacement proceed correctly).
        {
            let mut be = self
                .shared
                .backend
                .lock()
                .unwrap_or_else(|e| e.into_inner());

            // Switching and recovery share shutdown reconciliation so neither can
            // discard a denial produced while the previous owner is joining.
            stop_backend_reconciling(&self.shared, &mut be, false);
            // Shutdown has completed and backend is already locked here.
            if let Some(error) = self.shared.terminal.error() {
                self.shared.switching.store(false, Ordering::SeqCst);
                return Err(error);
            }

            // Native format of the new backend.
            let (rate, channels) = (new_rate, new_channels);

            // New RawRing (do not carry over old format residue).
            let (producer, consumer) = raw_ring(RAW_RING_SAMPLES);
            let sink = RawSink::new(producer, rate, channels);
            let diagnostics = sink.diagnostics();

            // Start the new backend. catch_unwind converts a panic to Error::Backend, so it follows
            // the Err branch below (restore old source → return Err). Restore the old source on failure.
            let mut new_backend = new_backend;
            match start_backend_catching(&mut new_backend, sink) {
                Ok(()) => {
                    // Publish the format, ring, generation and pending flag as one
                    // operation with respect to intake snapshots.
                    let mut rc = self
                        .shared
                        .raw_consumer
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    // Treat startup as the last arrival time (avoid an immediate stall check).
                    self.shared
                        .last_sample_ns
                        .store(monotonic_now_ns(), Ordering::SeqCst);
                    // Replace with the new backend (drop the old backend).
                    *be = new_backend;
                    self.shared.backend_stopped.store(false, Ordering::SeqCst);
                    if let Some(enabled) = denoise {
                        self.shared.denoise_enabled.store(enabled, Ordering::SeqCst);
                    }
                    self.shared.publish_raw(
                        &mut rc,
                        consumer,
                        diagnostics,
                        (rate, channels),
                        GenerationChange::Switch,
                    );
                }
                Err(e) => {
                    let replacement_cleanup = stop_backend_catching(&mut new_backend).err();
                    if let Some(error) = replacement_cleanup.clone() {
                        self.shared.record_cleanup(error);
                    }
                    drain_unpublished_capture(
                        &self.shared,
                        &consumer,
                        &diagnostics,
                        (rate, channels),
                    );
                    if is_terminal_kind(&e) {
                        self.shared.fail_terminal(e.clone());
                        self.shared.switching.store(false, Ordering::SeqCst);
                        return Err(with_cleanup(e, replacement_cleanup));
                    }
                    // New source startup failed → restart the old backend (still `*be`) and continue.
                    // Release the backend lock before restoring it, or open_backend_once will lock it
                    // again and deadlock.
                    drop(be);
                    // Reopen the old backend (native_format returns to the old backend's value).
                    // Resuming the old source is also discontinuous (it was interrupted briefly).
                    let restored = Self::open_backend_once(&self.shared, GenerationChange::Switch);
                    // open_backend_once already incremented generation. Reset switching and return Err.
                    self.shared.switching.store(false, Ordering::SeqCst);
                    if let Err(error) = restored {
                        let rollback = error.with_context(ErrorContext::new(Operation::Rollback));
                        let error = Error::Multiple(ErrorGroup::new(
                            e,
                            rollback,
                            replacement_cleanup.into_iter().collect(),
                        ));
                        self.shared.fail_terminal(error.clone());
                        return Err(error);
                    }
                    return Err(with_cleanup(e, replacement_cleanup));
                }
            }
        }

        // --- Switch succeeded ---
        // The new generation was published under the backend and raw-consumer locks.
        self.shared.switching.store(false, Ordering::SeqCst);
        Ok(())
    }

    /// High-level entry point to switch input source (mic/system/process) without stopping recording.
    ///
    /// Build the source-specific backend from `new_config` with `build_backend` (private to the
    /// facade); on failure, return `Err` with the old source untouched. Replace it with
    /// [`switch_backend`](Self::switch_backend). The output format (`output`) cannot change because
    /// changes to chunk frames/data.len would break the continuous stream. Reject such requests with
    /// [`Error::InvalidArg`].
    ///
    /// On success, update only the mutable `config` fields (`kind` / `device_id` / `target_pid` /
    /// `mode` / `exclude_self` / `exclude_pids`). Keep `output` / `chunk_ms` /
    /// `ring_capacity_chunks` unchanged. Ignore `new_config.gain` too (gain is stream state and does
    /// not change on source switch; change it with [`set_gain`](Self::set_gain)).
    ///
    /// # Errors
    /// - Not started → [`Error::InvalidState`].
    /// - Request to change `output` → [`Error::InvalidArg`].
    /// - New backend construction fails (missing process PID, unsupported OS, etc.) → error from
    ///   `build_backend` (private to the facade); the old source remains untouched.
    /// - New backend start fails → [`switch_backend`](Self::switch_backend) restores the old source
    ///   and returns the error.
    pub fn switch_source(&mut self, new_config: StreamConfig) -> Result<()> {
        self.switch_source_inner(new_config, None)
    }

    /// Switch source and apply denoise atomically with the new capture generation.
    pub fn switch_source_with_denoise(
        &mut self,
        new_config: StreamConfig,
        denoise: bool,
    ) -> Result<()> {
        self.switch_source_inner(new_config, Some(denoise))
    }

    fn switch_source_inner(
        &mut self,
        new_config: StreamConfig,
        denoise: Option<bool>,
    ) -> Result<()> {
        if let Some(error) = self.terminal_error() {
            return Err(error);
        }
        if !self.started {
            return Err(Error::InvalidState(
                "switch_source is only available on a started stream".into(),
            ));
        }
        if new_config.output != self.config.output {
            return Err(Error::InvalidArg(
                "output format cannot change during switch_source".into(),
            ));
        }
        // The secondary tap format is also fixed at open (do not rebuild its Normalizer/ring on switch).
        if new_config.secondary_output != self.config.secondary_output {
            return Err(Error::InvalidArg(
                "secondary output format cannot change during switch_source".into(),
            ));
        }
        validate_chunk_ms(new_config.chunk_ms)?;
        // Build the new source backend (on failure, return early with the old source untouched).
        crate::validate_exclude_pids(&new_config)?;
        let backend = match crate::build_backend(&new_config) {
            Ok(backend) => backend,
            Err(error) => {
                if is_terminal_kind(&error) {
                    let mut be = self
                        .shared
                        .backend
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    self.shared.fail_terminal(error.clone());
                    self.shared.stop_backend_owned(&mut be);
                }
                return Err(error);
            }
        };
        // Swap it in (switch_backend guarantees continuity).
        self.switch_backend_inner(backend, denoise)?;
        // Update mutable config fields only on success (keep output and other fields unchanged).
        self.config = StreamConfig {
            kind: new_config.kind,
            device_id: new_config.device_id,
            target_pid: new_config.target_pid,
            mode: new_config.mode,
            exclude_self: new_config.exclude_self,
            exclude_pids: new_config.exclude_pids,
            ..self.config.clone()
        };
        Ok(())
    }
}

/// Failed startup still drains all diagnostics after its producer has been stopped.
fn drain_unpublished_capture(
    shared: &SharedState,
    consumer: &RawConsumer,
    diagnostics: &CaptureDiagnostics,
    format: (u32, u16),
) {
    match diagnostics.drain() {
        Ok(losses) => {
            for loss in losses {
                shared.push_event(Event::AudioLoss { loss });
            }
        }
        Err(error) => shared.record_cleanup(error),
    }
    if let Some(samples) = NonZeroU64::new(consumer.overflow_count()) {
        match AudioLoss::raw_overflow(None, Some(samples), format.0, format.1) {
            Ok(loss) => shared.push_event(Event::AudioLoss { loss }),
            Err(error) => shared.record_cleanup(error),
        }
    }
}
