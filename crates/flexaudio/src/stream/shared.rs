//! Atomic generation snapshots, terminal latching and diagnostic ownership.
use super::*;
impl SharedState {
    pub(super) fn snapshot_raw(&self, scratch: &mut [f32]) -> RawSnapshot {
        let mut consumer = self.raw_consumer.lock().unwrap_or_else(|e| e.into_inner());
        let native_format = *self.native_format.lock().unwrap_or_else(|e| e.into_inner());
        let (samples, overflows) = match consumer.as_mut() {
            Some(consumer) => (consumer.pop_slice(scratch), consumer.overflow_count()),
            None => (0, 0),
        };
        RawSnapshot {
            microphone: self.microphone.load(Ordering::SeqCst),
            startup_drop: if scratch.is_empty() {
                None
            } else {
                self.raw_diagnostics
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                    .and_then(CaptureDiagnostics::take_startup_drop)
            },
            losses: if scratch.is_empty() {
                Ok(Vec::new())
            } else {
                self.capture_losses_locked(consumer.as_ref(), native_format)
            },
            generation: self.raw_generation.load(Ordering::SeqCst),
            denoise_enabled: self.denoise_enabled.load(Ordering::SeqCst),
            native_format,
            samples,
            overflows,
            // Empty generations must leave recovery pending until captured samples arrive.
            recovered: samples > 0 && self.recovered_pending.swap(false, Ordering::SeqCst),
            discontinuity: !scratch.is_empty()
                && (self.discontinuity_pending.swap(false, Ordering::SeqCst)
                    | self
                        .raw_diagnostics
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .as_ref()
                        .is_some_and(CaptureDiagnostics::take_discontinuity)),
        }
    }

    /// Require the raw-consumer guard across this entire publication.
    pub(super) fn publish_raw(
        &self,
        consumer: &mut MutexGuard<'_, Option<RawConsumer>>,
        new_consumer: RawConsumer,
        diagnostics: CaptureDiagnostics,
        native_format: (u32, u16),
        change: GenerationChange,
    ) {
        self.drain_pending_startup_drop();
        let old_format = *self.native_format.lock().unwrap_or_else(|e| e.into_inner());
        self.publish_capture_losses(self.capture_losses_locked(consumer.as_ref(), old_format));
        *self.native_format.lock().unwrap_or_else(|e| e.into_inner()) = native_format;
        *self
            .raw_diagnostics
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(diagnostics);
        *self
            .raw_overflow_reported
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = 0;
        **consumer = Some(new_consumer);
        self.raw_generation.fetch_add(1, Ordering::SeqCst);
        self.recovered_pending.store(
            matches!(change, GenerationChange::Recovery) && !self.stopping.load(Ordering::SeqCst),
            Ordering::SeqCst,
        );
        self.discontinuity_pending
            .store(matches!(change, GenerationChange::Switch), Ordering::SeqCst);
    }

    fn capture_losses_locked(
        &self,
        consumer: Option<&RawConsumer>,
        format: (u32, u16),
    ) -> Result<Vec<AudioLoss>> {
        let mut losses = self
            .raw_diagnostics
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map_or_else(|| Ok(Vec::new()), CaptureDiagnostics::drain)?;
        let total = consumer.map_or(0, RawConsumer::overflow_count);
        let mut reported = self
            .raw_overflow_reported
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if total > *reported {
            losses.push(AudioLoss::raw_overflow(
                None,
                NonZeroU64::new(total - *reported),
                format.0,
                format.1,
            )?);
            *reported = total;
        }
        Ok(losses)
    }

    fn publish_capture_losses(&self, losses: Result<Vec<AudioLoss>>) {
        match losses {
            Ok(losses) => {
                for loss in losses {
                    self.push_event(Event::AudioLoss { loss });
                }
            }
            Err(error) => self.record_cleanup(error),
        }
    }

    /// Shared loss drain for final shutdown, including failures before acquiring intake.
    pub(super) fn drain_capture_losses(&self) {
        self.drain_pending_startup_drop();
        let consumer = self.raw_consumer.lock().unwrap_or_else(|e| e.into_inner());
        let format = *self.native_format.lock().unwrap_or_else(|e| e.into_inner());
        self.publish_capture_losses(self.capture_losses_locked(consumer.as_ref(), format));
    }

    /// Capture replacement/final shutdown must not erase an undelivered Mix report.
    fn drain_pending_startup_drop(&self) {
        let pending = self
            .raw_diagnostics
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(CaptureDiagnostics::take_startup_drop);
        if let Some((frames, rate)) = pending {
            if let Err(error) = self.advance_startup_gap(frames, rate) {
                self.record_cleanup(error);
            }
            if rate > 0 {
                let duration = i64::try_from(u128::from(frames) * 1_000_000_000 / u128::from(rate))
                    .unwrap_or(i64::MAX);
                let origin = monotonic_now_ns().saturating_sub(duration);
                self.recording_epoch_ns
                    .compare_exchange(i64::MIN, origin, Ordering::SeqCst, Ordering::SeqCst)
                    .ok();
            }
            self.push_event(Event::StartupSamplesDropped {
                frames,
                reason: flexaudio_core::StartupDropReason::OutOfRange,
            });
        }
    }
    /// Each counter stores frames at its tap rate; returned indices are canonical.
    pub(super) fn advance_startup_gap(&self, frames: u64, native_rate: u32) -> Result<()> {
        if native_rate == 0 {
            return Err(Error::InvalidState(
                "startup gap has no native sample rate; recreate the capture stream".into(),
            ));
        }
        let taps = [
            (&self.primary_frame_index, self.output_format.sample_rate),
            (
                &self.secondary_frame_index,
                self.secondary_format
                    .map_or(48_000, |format| format.sample_rate),
            ),
            (&self.capture_frame_index, 48_000),
        ];
        for (counter, tap_rate) in taps {
            let skipped =
                u64::try_from(u128::from(frames) * u128::from(tap_rate) / u128::from(native_rate))
                    .map_err(|_| {
                        Error::InvalidState(
                            "startup frame timeline exhausted; reopen the stream".into(),
                        )
                    })?;
            advance_frame_index(counter, skipped, tap_rate)?;
        }
        Ok(())
    }

    /// Stop and recovery publication share the raw lock; delivery cannot announce after this.
    pub(super) fn begin_stopping(&self, _delivery: &MutexGuard<'_, ()>) {
        let _consumer = self.raw_consumer.lock().unwrap_or_else(|e| e.into_inner());
        self.stopping.store(true, Ordering::SeqCst);
        self.recovered_pending.store(false, Ordering::SeqCst);
    }

    pub(super) fn record_cleanup(&self, error: Error) {
        let mut cleanup = self.cleanup.lock().unwrap_or_else(|e| e.into_inner());
        if cleanup.contains(&error) {
            return;
        }
        cleanup.push(error.clone());
        drop(cleanup);
        self.push_event(Event::ShutdownError { error });
    }

    /// Returned stop failures and backend notices share one classification/publication path.
    pub(super) fn record_backend_cleanup(&self, error: Error) {
        if self.terminal.error().as_ref() == Some(&error) {
            return;
        }
        self.record_cleanup(error.with_context(ErrorContext::new(Operation::Stop)));
    }

    pub(super) fn fail_terminal(&self, error: Error) {
        let delivery = self.delivery.lock().unwrap_or_else(|e| e.into_inner());
        self.fail_terminal_locked(error, &delivery);
    }

    pub(super) fn stop_backend_owned(
        &self,
        be: &mut Box<dyn CaptureBackend>,
        delivery: Option<&MutexGuard<'_, ()>>,
        helper_reason: flexaudio_core::HelperStopReason,
    ) {
        if !self.backend_stopped.swap(true, Ordering::SeqCst) {
            be.set_helper_stop_reason(helper_reason);
            // Runtime terminal handling has already closed delivery. Keep its
            // delivery lock free during the owner's join, so polling and resume
            // can observe the latched failure without waiting for native stop.
            let result = stop_backend_catching(be);
            let delivery_guard;
            let delivery = match delivery {
                Some(delivery) => delivery,
                None => {
                    delivery_guard = self.delivery.lock().unwrap_or_else(|e| e.into_inner());
                    &delivery_guard
                }
            };
            // Mix may discover its primary during stop. Reconcile that terminal notice
            // before classifying the returned report, so the primary is never cleanup.
            control::drain_final_backend_events(self, be, delivery);
            if let Err(error) = result {
                // Remove only the context added by stop_backend_catching; backend
                // contexts (including lane and native status) remain on each cause.
                let error = match error {
                    Error::Context { source, context }
                        if context == ErrorContext::new(Operation::Stop) =>
                    {
                        *source
                    }
                    error => error,
                };
                for error in backend_failures(error) {
                    self.record_backend_cleanup(error);
                }
            }
        }
    }

    /// Retire a FIFO event while holding its queue lock, whether delivered or discarded.
    pub(super) fn retire_event(&self, event: &Event) {
        if matches!(event, Event::StartupSamplesDropped { .. }) {
            self.startup_order
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .claim();
        }
    }

    pub(super) fn push_event(&self, ev: Event) {
        // One FIFO owns every event. Register the push-only boundary before publishing
        // the notice and before intake can publish the generation's first chunk.
        let mut q = self.events.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(ev, Event::StartupSamplesDropped { .. }) {
            self.startup_order
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .register(self);
        }
        q.push_back(ev);
    }

    /// Close both delivery paths before publishing the terminal event. The caller
    /// stops the backend directly, without joining the watchdog from itself.
    pub(super) fn deny_permission(&self, permission: Permission, detail: String) {
        let delivery = self.delivery.lock().unwrap_or_else(|e| e.into_inner());
        self.deny_permission_locked(permission, detail, &delivery);
    }

    pub(super) fn deny_permission_locked(
        &self,
        permission: Permission,
        detail: String,
        delivery: &MutexGuard<'_, ()>,
    ) {
        self.fail_terminal_locked(Error::PermissionDenied { permission, detail }, delivery);
    }

    pub(super) fn fail_terminal_locked(&self, error: Error, delivery: &MutexGuard<'_, ()>) {
        if self.terminal.record(error.clone()) {
            self.begin_stopping(delivery);
            self.push_event(match error {
                Error::PermissionDenied { permission, detail } => {
                    Event::PermissionDenied { permission, detail }
                }
                error => Event::TerminalError { error },
            });
        }
    }
}

/// A checked report may group the capture primary and several independent cleanups.
fn backend_failures(error: Error) -> Vec<Error> {
    match error {
        Error::Multiple(group) => std::iter::once(group.primary())
            .chain(group.secondary())
            .flat_map(|error| backend_failures(error.clone()))
            .collect(),
        Error::Context { source, context } => backend_failures(*source)
            .into_iter()
            .map(|error| error.with_context(context))
            .collect(),
        error => vec![error],
    }
}
