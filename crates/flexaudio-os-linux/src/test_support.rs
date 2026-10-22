//! Internal shutdown injection fixtures, with no stability guarantee.
//!
//! Enabled only for tests or by the non-default `test-support` feature.

use crate::*;

/// Real owner operations selected for failure injection.
#[derive(Clone, Copy)]
pub enum Phase {
    /// System capture owner join failure.
    SystemJoin,
    /// System capture terminal failure followed by a join failure.
    SystemTerminalAndJoin,
    /// System capture terminal event consumed before its owner join fails.
    SystemConsumedTerminalAndJoin,
    /// Process capture owner join failure.
    ProcessJoin,
    /// Process capture terminal failure followed by a join failure.
    ProcessTerminalAndJoin,
    /// Process capture terminal event consumed before its owner join fails.
    ProcessConsumedTerminalAndJoin,
    /// Device watcher owner join failure.
    WatcherJoin,
    /// Device watcher terminal failure followed by a join failure.
    WatcherTerminalAndJoin,
}

enum Fixture {
    System(PwSystemBackend),
    Process(PwProcessBackend),
    Watcher(PwDeviceWatcher),
}

impl Fixture {
    fn stop(&mut self) {
        match self {
            Self::System(owner) => owner.stop(),
            Self::Process(owner) => owner.stop(),
            Self::Watcher(owner) => owner.stop(),
        }
    }

    fn stop_checked(&mut self) -> Result<()> {
        match self {
            Self::System(owner) => owner.stop_checked(),
            Self::Process(owner) => owner.stop_checked(),
            Self::Watcher(owner) => owner.stop_checked(),
        }
    }

    fn report(&self) -> &ShutdownReport {
        match self {
            Self::System(owner) => owner.shutdown_report(),
            Self::Process(owner) => owner.shutdown_report(),
            Self::Watcher(owner) => owner.shutdown_report(),
        }
        .expect("stop must immediately retain its report")
    }

    fn consume_terminal(&mut self) {
        let event = match self {
            Self::System(owner) => owner.poll_event(),
            Self::Process(owner) => owner.poll_event(),
            Self::Watcher(_) => unreachable!("watcher phases do not consume capture events"),
        };
        assert_eq!(
            event,
            Some(Event::TerminalError {
                error: Error::DeviceLost
            })
        );
    }
}

fn capture_worker(events: BackendEvents, terminal: bool) -> JoinHandle<()> {
    let (ready, received) = mpsc::channel();
    let handle = thread::spawn(move || {
        if terminal {
            push_backend_event(
                &events,
                Event::TerminalError {
                    error: Error::DeviceLost,
                },
            );
        }
        ready
            .send(())
            .expect("fixture remains alive until its worker is ready");
        panic!("injected capture join failure");
    });
    received
        .recv()
        .expect("fixture worker must reach its injected operation");
    handle
}

fn fixture(phase: Phase) -> (Fixture, bool, bool) {
    match phase {
        Phase::SystemJoin | Phase::SystemTerminalAndJoin | Phase::SystemConsumedTerminalAndJoin => {
            let terminal = !matches!(phase, Phase::SystemJoin);
            let consumed = matches!(phase, Phase::SystemConsumedTerminalAndJoin);
            let mut owner = PwSystemBackend::new(false, None);
            owner.handle = Some(capture_worker(owner.events.clone(), terminal));
            owner.running.store(true, Ordering::SeqCst);
            (Fixture::System(owner), terminal, consumed)
        }
        Phase::ProcessJoin
        | Phase::ProcessTerminalAndJoin
        | Phase::ProcessConsumedTerminalAndJoin => {
            let terminal = !matches!(phase, Phase::ProcessJoin);
            let consumed = matches!(phase, Phase::ProcessConsumedTerminalAndJoin);
            let mut owner = PwProcessBackend::new(1, ProcessMode::Include);
            owner.handle = Some(capture_worker(owner.events.clone(), terminal));
            owner.running.store(true, Ordering::SeqCst);
            (Fixture::Process(owner), terminal, consumed)
        }
        Phase::WatcherJoin | Phase::WatcherTerminalAndJoin => {
            let terminal = matches!(phase, Phase::WatcherTerminalAndJoin);
            let events = Arc::new(Mutex::new(WatchEvents::default()));
            let worker_events = events.clone();
            let handle = thread::spawn(move || {
                if terminal {
                    fail_watcher(&worker_events, Error::DeviceLost);
                }
                panic!("injected watcher join failure");
            });
            (
                Fixture::Watcher(PwDeviceWatcher {
                    events,
                    running: Arc::new(AtomicBool::new(true)),
                    stop_tx: None,
                    handle: Some(handle),
                    shutdown: None,
                }),
                terminal,
                false,
            )
        }
    }
}

/// Exercise void stop, checked stop, and repeated stop through a real worker join.
/// This internal test entry has no stability guarantee and runs on Linux CI.
pub fn check(phase: Phase) {
    for checked_first in [false, true] {
        let (mut fixture, terminal, consumed) = fixture(phase);
        if consumed {
            fixture.consume_terminal();
        }
        let cleanup = Error::Backend("pipewire worker panicked".into())
            .with_context(ErrorContext::new(Operation::Join));
        let expected = ShutdownReport::new(terminal.then_some(Error::DeviceLost), vec![cleanup]);
        if checked_first {
            assert_eq!(fixture.stop_checked(), expected.result());
        } else {
            fixture.stop();
        }
        assert_eq!(fixture.report(), &expected);
        assert_eq!(fixture.stop_checked(), expected.result());
        assert_eq!(fixture.report(), &expected);
        fixture.stop();
        assert_eq!(fixture.report(), &expected);
        assert_eq!(fixture.stop_checked(), expected.result());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_shutdown_retains_primary_and_join_cleanup() {
        check(Phase::SystemJoin);
        check(Phase::SystemTerminalAndJoin);
        check(Phase::SystemConsumedTerminalAndJoin);
    }

    #[test]
    fn process_shutdown_retains_primary_and_join_cleanup() {
        check(Phase::ProcessJoin);
        check(Phase::ProcessTerminalAndJoin);
        check(Phase::ProcessConsumedTerminalAndJoin);
    }

    #[test]
    fn watcher_shutdown_retains_primary_and_join_cleanup() {
        check(Phase::WatcherJoin);
        check(Phase::WatcherTerminalAndJoin);
    }

    #[test]
    fn capture_rollback_retains_current_and_previous_join_cleanup() {
        let mut owner = PwSystemBackend::new(false, None);
        owner.handle = Some(thread::spawn(|| panic!("previous generation join failure")));
        owner.stop();
        let first_cleanup = owner.shutdown_report().unwrap().cleanup()[0].clone();
        let handle = thread::spawn(|| panic!("rollback join failure"));
        let error = rollback_capture(
            Error::DeviceLost,
            handle,
            Arc::new(Mutex::new(Default::default())),
            &mut owner.events,
            &mut owner.shutdown,
        );
        let report = owner
            .shutdown_report()
            .expect("rollback retains its report")
            .clone();
        assert_eq!(report.primary(), Some(&Error::DeviceLost));
        assert_eq!(
            report.cleanup(),
            &[
                first_cleanup.clone(),
                first_cleanup.with_context(ErrorContext::new(Operation::Rollback)),
            ]
        );
        assert_eq!(Err(error), report.result());
        owner.stop();
        assert_eq!(owner.shutdown_report(), Some(&report));
        assert_eq!(owner.stop_checked(), report.result());
    }
}
