//! Backend event classification, recovery and producer shutdown.
use super::*;
/// Watchdog thread body.
///
/// Check the last sample arrival time on ~250 ms ticks. If samples stop for longer than
/// [`STALL_THRESHOLD`], reopen the backend with exponential backoff. Fire
/// [`Event::StreamStalled`] on a stall. On a successful reopen, set `recovered_pending` so the
/// intake thread announces [`Event::StreamRecovered`] only once real samples from the new
/// generation have been delivered (a silent reopen is not a recovery).
pub(super) fn run_watchdog(shared: Arc<SharedState>) {
    let mut stalled = false;
    let mut backoff = BACKOFF_MIN;

    loop {
        if shared.stopping.load(Ordering::SeqCst) {
            break;
        }
        thread::sleep(WATCHDOG_TICK);
        if shared.stopping.load(Ordering::SeqCst) {
            break;
        }

        // Do not check for stalls or reopen during a source switch (switch_backend temporarily stops
        // the old backend, making it idle, so this prevents an incorrect concurrent reopen).
        // The switch updates last_sample_ns to now before finishing, so normal checks resume next tick.
        if shared.switching.load(Ordering::SeqCst) {
            continue;
        }

        // Notifications take precedence over stall recovery, including while audio
        // is flowing or delivery is paused. A denial never enters the reopen loop.
        if drain_backend_events(&shared) == MailboxDrain::BudgetExhausted {
            // More events may include a denial. Process them on the next tick
            // before any reopen can replace this generation's mailbox.
            continue;
        }
        if shared.stopping.load(Ordering::SeqCst) {
            break;
        }

        let now = monotonic_now_ns();
        let last = shared.last_sample_ns.load(Ordering::SeqCst);
        let idle_ns = now.saturating_sub(last);
        let idle = Duration::from_nanos(idle_ns.max(0) as u64);

        if !stalled {
            if idle >= STALL_THRESHOLD {
                // Stall detected.
                stalled = true;
                backoff = BACKOFF_MIN;
                shared.push_event(Event::StreamStalled);
            }
            continue;
        }

        // During a stall, stop the backend and try to reopen it.
        // Recover a poisoned lock and attempt stop. If stop panics, catch_unwind swallows it so the
        // watchdog can proceed to reopen instead of dying silently.
        {
            let mut be = shared.backend.lock().unwrap_or_else(|e| e.into_inner());
            stop_backend_reconciling(&shared, &mut be, false);
        }

        if shared.stopping.load(Ordering::SeqCst) {
            break;
        }

        let reopened = match Stream::open_backend_once(&shared, GenerationChange::Recovery) {
            Ok(()) => true,
            Err(e) => {
                if shared.terminal.is_failed() {
                    break;
                }
                shared.push_event(Event::RecoverableError {
                    error: e.with_context(ErrorContext::new(Operation::Reopen)),
                });
                false
            }
        };

        if reopened {
            // The recovery flag was published with the generation before intake could pop it.
            // Only delivery announces recovery; a silent reopen remains pending until data arrives.
            stalled = false;
            backoff = BACKOFF_MIN;
        } else {
            // On failure, wait with exponential backoff and jitter before retrying.
            let jittered = jittered_backoff(backoff);
            sleep_interruptible(&shared, jittered);
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }
    }
}

/// Drain the backend's control mailbox with the same serialization as start/stop.
/// A backend poll panic is observable and cannot poison the backend mutex.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MailboxDrain {
    Empty,
    Terminal,
    BudgetExhausted,
}

pub(super) fn drain_backend_events(shared: &SharedState) -> MailboxDrain {
    let mut be = shared.backend.lock().unwrap_or_else(|e| e.into_inner());
    if shared.switching.load(Ordering::SeqCst) || shared.stopping.load(Ordering::SeqCst) {
        return MailboxDrain::Empty;
    }
    drain_backend_events_locked(shared, &mut be, None)
}

/// Also used at shutdown/source replacement so final owner notifications cannot
/// be lost. Shutdown holds delivery across the owner's join to suppress a tail
/// if a denial arrives while capture is stopping.
fn drain_backend_events_locked(
    shared: &SharedState,
    be: &mut Box<dyn CaptureBackend>,
    delivery: Option<&MutexGuard<'_, ()>>,
) -> MailboxDrain {
    for _ in 0..MAX_BACKEND_EVENTS_PER_TICK {
        let event = match std::panic::catch_unwind(AssertUnwindSafe(|| be.poll_event())) {
            Ok(event) => event,
            Err(_) => {
                shared.push_event(Event::RecoverableError {
                    error: Error::Backend("backend panicked during poll_event".into()),
                });
                return MailboxDrain::BudgetExhausted;
            }
        };
        match event {
            Some(Event::PermissionDenied { permission, detail }) => {
                if let Some(delivery) = delivery {
                    shared.deny_permission_locked(permission, detail, delivery);
                } else {
                    shared.deny_permission(permission, detail);
                }
                shared.stop_backend_owned(
                    be,
                    delivery,
                    flexaudio_core::HelperStopReason::RuntimeFailed,
                );
                return MailboxDrain::Terminal;
            }
            Some(Event::Error(_)) => {
                // Built-in producers use typed errors with safe library-authored explanations.
                // A third-party backend's legacy string has no provenance or private-detail
                // slot, so discard it rather than placing arbitrary text in a Display payload.
                let error = Error::Backend(
                    "backend reported a failure through a legacy error event".into(),
                );
                if let Some(delivery) = delivery {
                    shared.fail_terminal_locked(error, delivery);
                } else {
                    shared.fail_terminal(error);
                }
                shared.stop_backend_owned(
                    be,
                    delivery,
                    flexaudio_core::HelperStopReason::RuntimeFailed,
                );
                return MailboxDrain::Terminal;
            }
            Some(Event::RecoverableError { error }) if is_terminal_kind(&error) => {
                // Confirmed denial/format invalidation cannot become advisory through a wrapper.
                if let Some(delivery) = delivery {
                    shared.fail_terminal_locked(error, delivery);
                } else {
                    shared.fail_terminal(error);
                }
                shared.stop_backend_owned(
                    be,
                    delivery,
                    flexaudio_core::HelperStopReason::RuntimeFailed,
                );
                return MailboxDrain::Terminal;
            }
            Some(Event::TerminalError { error }) => {
                if let Some(delivery) = delivery {
                    shared.fail_terminal_locked(error, delivery);
                } else {
                    let delivery = shared.delivery.lock().unwrap_or_else(|e| e.into_inner());
                    shared.fail_terminal_locked(error, &delivery);
                }
                shared.stop_backend_owned(
                    be,
                    delivery,
                    flexaudio_core::HelperStopReason::RuntimeFailed,
                );
                return MailboxDrain::Terminal;
            }
            Some(Event::ShutdownError { error }) => shared.record_backend_cleanup(error),
            Some(Event::AudioLoss { loss }) => {
                if matches!(
                    loss.path(),
                    flexaudio_core::AudioPath::Capture { .. }
                        | flexaudio_core::AudioPath::MixFifo { .. }
                ) {
                    // Reuse intake's pending discontinuity fan-out for capture-side
                    // losses reported by a backend, including either Mix lane.
                    shared.discontinuity_pending.store(true, Ordering::SeqCst);
                }
                shared.push_event(Event::AudioLoss { loss });
            }
            Some(event) => shared.push_event(event),
            None => return MailboxDrain::Empty,
        }
    }
    MailboxDrain::BudgetExhausted
}

pub(super) fn drain_final_backend_events(
    shared: &SharedState,
    be: &mut Box<dyn CaptureBackend>,
    delivery: &MutexGuard<'_, ()>,
) {
    for _ in 0..MAX_FINAL_EVENT_BATCHES {
        if drain_backend_events_locked(shared, be, Some(delivery)) == MailboxDrain::Empty {
            return;
        }
    }
    shared.fail_terminal_locked(
        Error::Backend("backend event mailbox could not be reconciled; capture terminated before delivering buffered audio".into()),
        delivery,
    );
    shared.stop_backend_owned(
        be,
        Some(delivery),
        flexaudio_core::HelperStopReason::RuntimeFailed,
    );
}

/// One shutdown path for explicit stop, source replacement, and recovery.
/// Hold delivery across the owner's join and reconcile both sides of it, so a
/// queued or final denial suppresses the tail and prevents reopening.
pub(super) fn stop_backend_reconciling(
    shared: &SharedState,
    be: &mut Box<dyn CaptureBackend>,
    stop_intake: bool,
) {
    let reason = if stop_intake {
        flexaudio_core::HelperStopReason::StreamStopped
    } else if shared.switching.load(Ordering::SeqCst) {
        flexaudio_core::HelperStopReason::SourceSwitched
    } else {
        flexaudio_core::HelperStopReason::Recovery
    };
    stop_backend_reconciling_with_reason(shared, be, stop_intake, reason);
}

pub(super) fn stop_backend_reconciling_with_reason(
    shared: &SharedState,
    be: &mut Box<dyn CaptureBackend>,
    stop_intake: bool,
    reason: flexaudio_core::HelperStopReason,
) {
    let delivery = shared.delivery.lock().unwrap_or_else(|e| e.into_inner());
    drain_final_backend_events(shared, be, &delivery);
    let reason = if shared.terminal.is_failed()
        && reason != flexaudio_core::HelperStopReason::StartupFailed
    {
        flexaudio_core::HelperStopReason::RuntimeFailed
    } else {
        reason
    };
    shared.stop_backend_owned(be, Some(&delivery), reason);
    if stop_intake {
        shared.begin_stopping(&delivery);
    }
}

/// Preserve only the additive helper notices of a rejected replacement. Its
/// ordinary errors still follow the existing startup/rollback result path.
pub(super) fn drain_replacement_lifecycle(shared: &SharedState, be: &mut Box<dyn CaptureBackend>) {
    for _ in 0..MAX_BACKEND_EVENTS_PER_TICK * MAX_FINAL_EVENT_BATCHES {
        match std::panic::catch_unwind(AssertUnwindSafe(|| be.poll_event())) {
            Ok(Some(event)) if is_helper_lifecycle(&event) => shared.push_event(event),
            Ok(Some(_)) => {}
            Ok(None) => return,
            Err(_) => {
                shared.record_cleanup(Error::Backend(
                    "replacement backend panicked during final event drain".into(),
                ));
                return;
            }
        }
    }
    shared.record_cleanup(Error::Backend(
        "replacement backend event mailbox could not be reconciled".into(),
    ));
}

pub(crate) fn is_helper_lifecycle(event: &Event) -> bool {
    matches!(
        event,
        Event::SilenceProbeStarted
            | Event::SilenceProbeStopped { .. }
            | Event::ClassicLoopbackKeepaliveStarted
            | Event::ClassicLoopbackKeepaliveStopped { .. }
    )
}

#[cfg(test)]
#[path = "helper_lifecycle_tests.rs"]
mod helper_lifecycle_tests;
