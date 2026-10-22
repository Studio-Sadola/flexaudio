//! Internal shutdown injection fixtures, with no stability guarantee.
//!
//! Enabled only for tests or by the non-default `test-support` feature.

use crate::*;

macro_rules! phases {
    ($( $(#[$doc:meta])* $variant:ident, )*) => {
        /// Real owner operations selected for failure injection.
        #[derive(Clone, Copy)]
        pub enum Phase {
            $( $(#[$doc])* $variant, )*
        }

        impl Phase {
            /// Every supported injection phase, generated from its definition.
            pub const ALL: &'static [Self] = &[$(Self::$variant),*];
        }
    };
}

phases! {
    /// System capture owner join failure.
    SystemJoin,
    /// System capture terminal failure followed by a join failure.
    SystemTerminalAndJoin,
    /// System capture terminal event consumed before its owner join fails.
    SystemConsumedTerminalAndJoin,
    /// Dropped system stop receiver with a retained terminal and failing owner join.
    SystemDroppedReceiverAndJoin,
    /// Process capture owner join failure.
    ProcessJoin,
    /// Process capture terminal failure followed by a join failure.
    ProcessTerminalAndJoin,
    /// Process capture terminal event consumed before its owner join fails.
    ProcessConsumedTerminalAndJoin,
    /// Dropped process stop receiver with a retained terminal and failing owner join.
    ProcessDroppedReceiverAndJoin,
    /// Device watcher owner join failure.
    WatcherJoin,
    /// Device watcher terminal failure followed by a join failure.
    WatcherTerminalAndJoin,
    /// Dropped watcher stop receiver with a retained terminal and failing owner join.
    WatcherDroppedReceiverAndJoin,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FixtureKind {
    System,
    Process,
    Watcher,
}

impl Phase {
    fn fixture_kind(self) -> FixtureKind {
        match self {
            Self::SystemJoin
            | Self::SystemTerminalAndJoin
            | Self::SystemConsumedTerminalAndJoin
            | Self::SystemDroppedReceiverAndJoin => FixtureKind::System,
            Self::ProcessJoin
            | Self::ProcessTerminalAndJoin
            | Self::ProcessConsumedTerminalAndJoin
            | Self::ProcessDroppedReceiverAndJoin => FixtureKind::Process,
            Self::WatcherJoin
            | Self::WatcherTerminalAndJoin
            | Self::WatcherDroppedReceiverAndJoin => FixtureKind::Watcher,
        }
    }
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

fn dropped_receiver_sender() -> pw::channel::Sender<Terminate> {
    let (sender, receiver) = pw::channel::channel();
    drop(receiver);
    // PipeWire 0.10 keeps both pipe descriptors in shared channel state. Receiver
    // drop alone does not make send fail or prove that the worker has exited.
    assert!(sender.send(Terminate).is_ok());
    sender
}

fn fixture(phase: Phase) -> (Fixture, bool, bool) {
    match phase {
        Phase::SystemJoin
        | Phase::SystemTerminalAndJoin
        | Phase::SystemConsumedTerminalAndJoin
        | Phase::SystemDroppedReceiverAndJoin => {
            let terminal = !matches!(phase, Phase::SystemJoin);
            let consumed = matches!(phase, Phase::SystemConsumedTerminalAndJoin);
            let mut owner = PwSystemBackend::new(false, None);
            owner.handle = Some(capture_worker(owner.events.clone(), terminal));
            if matches!(phase, Phase::SystemDroppedReceiverAndJoin) {
                owner.stop_tx = Some(dropped_receiver_sender());
            }
            owner.running.store(true, Ordering::SeqCst);
            (Fixture::System(owner), terminal, consumed)
        }
        Phase::ProcessJoin
        | Phase::ProcessTerminalAndJoin
        | Phase::ProcessConsumedTerminalAndJoin
        | Phase::ProcessDroppedReceiverAndJoin => {
            let terminal = !matches!(phase, Phase::ProcessJoin);
            let consumed = matches!(phase, Phase::ProcessConsumedTerminalAndJoin);
            let mut owner = PwProcessBackend::new(1, ProcessMode::Include);
            owner.handle = Some(capture_worker(owner.events.clone(), terminal));
            if matches!(phase, Phase::ProcessDroppedReceiverAndJoin) {
                owner.stop_tx = Some(dropped_receiver_sender());
            }
            owner.running.store(true, Ordering::SeqCst);
            (Fixture::Process(owner), terminal, consumed)
        }
        Phase::WatcherJoin
        | Phase::WatcherTerminalAndJoin
        | Phase::WatcherDroppedReceiverAndJoin => {
            let terminal = !matches!(phase, Phase::WatcherJoin);
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
                    stop_tx: matches!(phase, Phase::WatcherDroppedReceiverAndJoin)
                        .then(dropped_receiver_sender),
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
    check_observed(phase, |_| {});
}

fn check_observed(phase: Phase, observe: impl Fn(&Fixture)) {
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
        observe(&fixture);
    }
}

fn check_kind(kind: FixtureKind, observe: impl Fn(&Fixture)) {
    for &phase in Phase::ALL {
        if phase.fixture_kind() == kind {
            check_observed(phase, &observe);
        }
    }
}

/// Exercise every system capture phase and observe each tested concrete owner.
/// This internal test entry has no stability guarantee and runs on Linux CI.
pub fn check_system(observe: fn(&PwSystemBackend)) {
    check_kind(FixtureKind::System, |fixture| match fixture {
        Fixture::System(owner) => observe(owner),
        Fixture::Process(_) | Fixture::Watcher(_) => panic!("expected a system capture fixture"),
    });
}

/// Exercise every process capture phase and observe each tested concrete owner.
/// This internal test entry has no stability guarantee and runs on Linux CI.
pub fn check_process(observe: fn(&PwProcessBackend)) {
    check_kind(FixtureKind::Process, |fixture| match fixture {
        Fixture::Process(owner) => observe(owner),
        Fixture::System(_) | Fixture::Watcher(_) => panic!("expected a process capture fixture"),
    });
}

/// Exercise every watcher phase and observe each tested concrete owner.
/// This internal test entry has no stability guarantee and runs on Linux CI.
pub fn check_watcher(observe: fn(&PwDeviceWatcher)) {
    check_kind(FixtureKind::Watcher, |fixture| match fixture {
        Fixture::Watcher(owner) => observe(owner),
        Fixture::System(_) | Fixture::Process(_) => panic!("expected a device watcher fixture"),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_shutdown_retains_primary_and_join_cleanup() {
        check_system(|_| {});
    }

    #[test]
    fn process_shutdown_retains_primary_and_join_cleanup() {
        check_process(|_| {});
    }

    #[test]
    fn watcher_shutdown_retains_primary_and_join_cleanup() {
        check_watcher(|_| {});
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
