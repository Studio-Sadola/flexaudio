//! Device-free policy for one controlled self-probe per capture generation.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::capture_health::ADVISORY_DETAIL;
use flexaudio_core::{Error, ErrorContext, ErrorGroup, NativeStatus, Operation};

pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// The adapter must verify generated content, rather than infer it from output I/O activity.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ProbeOutcome {
    Present,
    Denied { detail: String },
    Failed { error: Error },
    TimedOut,
    Cancelled,
}

/// A synchronous, owner-thread adapter. Native resources must be released before return.
/// All setup and waiting must respect the shared deadline and cancellation signal;
/// never detach a worker to make a blocking native operation appear bounded.
pub(crate) trait Probe {
    fn run(&mut self, control: &ProbeControl) -> ProbeOutcome;
}

trait Clock: Send + Sync {
    fn now(&self) -> Instant;
}

struct SystemClock;
impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// Control-thread teardown diagnostics. Native callbacks never touch this mailbox.
#[derive(Default)]
pub(crate) struct ProbeCleanup(Mutex<Vec<Error>>);

impl ProbeCleanup {
    pub(crate) fn record_native(&self, call: &'static str, status: i32) {
        if status == 0 {
            return;
        }
        let mut failures = match self.0.lock() {
            Ok(failures) => failures,
            Err(poisoned) => {
                let mut failures = poisoned.into_inner();
                failures.push(Error::Backend(
                    "self-probe cleanup diagnostics were poisoned; retry capture".into(),
                ));
                failures
            }
        };
        failures.push(
            Error::Backend(format!("self-probe {call} failed; retry capture")).with_context(
                ErrorContext::new(Operation::Stop).with_native_status(NativeStatus::OsStatus {
                    call,
                    value: status,
                }),
            ),
        );
    }

    fn take(&self) -> Vec<Error> {
        let mut failures = match self.0.lock() {
            Ok(failures) => failures,
            Err(poisoned) => {
                let mut failures = poisoned.into_inner();
                failures.push(Error::Backend(
                    "self-probe cleanup diagnostics were poisoned; retry capture".into(),
                ));
                failures
            }
        };
        std::mem::take(&mut *failures)
    }
}

pub(crate) struct ProbeControl {
    deadline: Instant,
    cancellation: Arc<AtomicBool>,
    clock: Arc<dyn Clock>,
    cleanup: Arc<ProbeCleanup>,
}

impl ProbeControl {
    pub(crate) fn new(deadline: Instant, cancellation: Arc<AtomicBool>) -> Self {
        Self {
            deadline,
            cancellation,
            clock: Arc::new(SystemClock),
            cleanup: Arc::new(ProbeCleanup::default()),
        }
    }

    pub(crate) fn cleanup_diagnostics(&self) -> Arc<ProbeCleanup> {
        self.cleanup.clone()
    }

    pub(crate) fn deadline(&self) -> Instant {
        self.deadline
    }

    pub(crate) fn cancelled(&self) -> bool {
        self.cancellation.load(Ordering::Acquire)
    }

    pub(crate) fn cancellation_flag(&self) -> Arc<AtomicBool> {
        self.cancellation.clone()
    }

    pub(crate) fn timed_out(&self) -> bool {
        self.clock.now() >= self.deadline
    }

    pub(crate) fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(self.clock.now())
    }
}

/// Linearizes stop against publishing a completed probe result. Only control
/// threads use this lock; the native callbacks read the cancellation atomic.
#[derive(Default)]
pub(crate) struct PublicationGate(Mutex<()>);

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PublicationError {
    Poisoned,
}

impl PublicationGate {
    pub(crate) fn cancel(&self, cancellation: &AtomicBool) -> Result<(), PublicationError> {
        match self.0.lock() {
            Ok(_guard) => {
                cancellation.store(true, Ordering::Release);
                Ok(())
            }
            Err(poisoned) => {
                // This mutex protects ordering only, with no mutable payload to
                // repair. Retain the recovered guard while setting cancellation.
                let _guard = poisoned.into_inner();
                cancellation.store(true, Ordering::Release);
                Err(PublicationError::Poisoned)
            }
        }
    }

    pub(crate) fn when_active(
        &self,
        control: &ProbeControl,
        publish: impl FnOnce(),
    ) -> Result<bool, PublicationError> {
        let _guard = self.0.lock().map_err(|_| PublicationError::Poisoned)?;
        if control.cancelled() {
            return Ok(false);
        }
        publish();
        Ok(true)
    }

    /// Report poisoning through a terminal event while retaining stop ordering.
    /// Recovering is safe because the mutex payload is unit; this path never
    /// resumes capture or pretends the preceding publication succeeded.
    pub(crate) fn recover_when_active(
        &self,
        control: &ProbeControl,
        publish: impl FnOnce(),
    ) -> bool {
        let _guard = match self.0.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if control.cancelled() {
            return false;
        }
        publish();
        true
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ProbeDecision {
    Denied {
        detail: String,
    },
    Advisory {
        detail: String,
        error: Option<Error>,
    },
}

/// The silence trigger is latched before probing, including cancellation or failure.
/// A fresh instance represents a new capture generation; the emitted signal cannot
/// reset this state even if a user tap includes the host process's output.
#[derive(Default)]
pub(crate) struct GenerationProbe {
    attempted: bool,
}

impl GenerationProbe {
    pub(crate) fn on_silence(
        &mut self,
        probe: &mut dyn Probe,
        control: &ProbeControl,
    ) -> Option<ProbeDecision> {
        if self.attempted {
            return None;
        }
        self.attempted = true;
        if control.cancelled() {
            return None;
        }
        let outcome = if control.timed_out() {
            ProbeOutcome::TimedOut
        } else {
            probe.run(control)
        };
        // Stop wins even when requested during native teardown after collecting a result.
        if control.cancelled() {
            return None;
        }
        if matches!(outcome, ProbeOutcome::Cancelled) {
            return None;
        }
        let mut errors = control.cleanup.take();
        if let ProbeOutcome::Failed { error } = &outcome {
            errors.insert(0, error.clone());
        }
        if control.timed_out() || matches!(outcome, ProbeOutcome::TimedOut) {
            errors.push(timeout_error());
        }
        // Cleanup failure invalidates permission evidence, even if the adapter
        // recognized a signal or reported denial before resources were released.
        if !errors.is_empty() {
            return Some(advisory_failures(errors));
        }
        match outcome {
            ProbeOutcome::Present | ProbeOutcome::Cancelled => None,
            ProbeOutcome::Denied { detail } => Some(ProbeDecision::Denied { detail }),
            ProbeOutcome::Failed { .. } | ProbeOutcome::TimedOut => Some(advisory()),
        }
    }
}

fn advisory() -> ProbeDecision {
    ProbeDecision::Advisory {
        detail: ADVISORY_DETAIL.into(),
        error: None,
    }
}

fn timeout_error() -> Error {
    Error::Backend(
        "self-probe did not complete before its two-second deadline; retry capture".into(),
    )
    .with_context(ErrorContext::new(Operation::Start))
}

fn advisory_failures(errors: Vec<Error>) -> ProbeDecision {
    let detail = format!(
        "{ADVISORY_DETAIL} Self-probe detail: {}",
        errors
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; ")
    );
    let mut errors = errors.into_iter();
    let error = match (errors.next(), errors.next()) {
        (Some(primary), Some(secondary)) => Some(Error::Multiple(ErrorGroup::new(
            primary,
            secondary,
            errors.collect(),
        ))),
        (Some(error), None) => Some(error),
        _ => None,
    };
    ProbeDecision::Advisory { detail, error }
}

#[cfg(test)]
mod tests;
