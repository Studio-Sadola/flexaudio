use super::*;
use flexaudio_core::types::Permission;
use flexaudio_core::CaptureBackend;

#[test]
fn consent_cancel_or_generation_change_no_late_grant() {
    use crate::mac_policy::{ConsentPoll, Provider, Status, PENDING_GRACE, POLL_INTERVAL};

    struct StatusProvider<'a> {
        status: Status,
        cancel_during_query: Option<&'a Generation>,
    }
    impl Provider for StatusProvider<'_> {
        fn status(&self) -> Result<Status> {
            if let Some(generation) = self.cancel_during_query {
                generation.cancel();
            }
            Ok(self.status)
        }
        fn has_usage_description(&self) -> Result<bool> {
            panic!("polling must not inspect the bundle")
        }
        fn request_access(&self, _: Box<dyn Fn(bool) + Send + Sync>) -> Result<()> {
            panic!("polling must not request consent")
        }
    }
    let pending = StatusProvider {
        status: Status::NotDetermined,
        cancel_during_query: None,
    };
    let authorized = StatusProvider {
        status: Status::Authorized,
        cancel_during_query: None,
    };
    for cancel_during_query in [false, true] {
        let old_stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let old_generation = Generation::new(old_stop.clone(), tx.clone());
        let mut old_poll = ConsentPoll::new();
        assert!(!check_pending(
            &mut old_poll,
            PENDING_GRACE,
            &pending,
            &old_stop,
            &old_generation
        ));
        assert!(matches!(rx.try_recv(), Ok(Event::PermissionPending { .. })));
        let observed = StatusProvider {
            status: Status::Authorized,
            cancel_during_query: cancel_during_query.then_some(&old_generation),
        };
        if !cancel_during_query {
            old_generation.cancel();
        }
        assert!(!check_pending(
            &mut old_poll,
            PENDING_GRACE + POLL_INTERVAL,
            &observed,
            &old_stop,
            &old_generation
        ));
        assert!(
            rx.try_recv().is_err(),
            "cancelled authorization must never commit a grant"
        );
        // A fresh live generation shares the queue but has its own gate/poll.
        let new_stop = Arc::new(AtomicBool::new(false));
        let new_generation = Generation::new(new_stop.clone(), tx);
        let mut new_poll = ConsentPoll::new();
        assert!(!check_pending(
            &mut old_poll,
            PENDING_GRACE + POLL_INTERVAL * 2,
            &authorized,
            &old_stop,
            &old_generation
        ));
        assert!(!check_pending(
            &mut new_poll,
            PENDING_GRACE,
            &authorized,
            &new_stop,
            &new_generation
        ));
        assert!(
            rx.try_recv().is_err(),
            "old pending state must not migrate to a new generation"
        );
        assert!(!new_stop.load(Ordering::SeqCst));
    }
}

#[test]
fn owner_late_grant_is_advisory_and_committed_event_survives_cancellation() {
    use crate::mac_policy::{ConsentPoll, Provider, Status, PENDING_GRACE, POLL_INTERVAL};
    struct Fixed(Status);
    impl Provider for Fixed {
        fn status(&self) -> Result<Status> {
            Ok(self.0)
        }
        fn has_usage_description(&self) -> Result<bool> {
            panic!("not a preflight")
        }
        fn request_access(&self, _: Box<dyn Fn(bool) + Send + Sync>) -> Result<()> {
            panic!("not a preflight")
        }
    }
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let generation = Generation::new(stop.clone(), tx);
    let mut poll = ConsentPoll::new();
    assert!(!check_pending(
        &mut poll,
        PENDING_GRACE,
        &Fixed(Status::NotDetermined),
        &stop,
        &generation
    ));
    assert!(!check_pending(
        &mut poll,
        PENDING_GRACE + POLL_INTERVAL,
        &Fixed(Status::Authorized),
        &stop,
        &generation
    ));
    assert!(
        !stop.load(Ordering::SeqCst),
        "grant must not terminate capture"
    );
    assert!(!check_pending(
        &mut poll,
        PENDING_GRACE + POLL_INTERVAL * 2,
        &Fixed(Status::Authorized),
        &stop,
        &generation
    ));
    generation.cancel();
    assert!(matches!(rx.try_recv(), Ok(Event::PermissionPending { .. })));
    assert_eq!(rx.try_recv(), Ok(Event::PermissionGranted));
    assert!(rx.try_recv().is_err());
}

#[test]
fn owner_keeps_polling_past_sixty_seconds_and_terminates_on_late_denial() {
    use crate::mac_policy::{Provider, Status};
    use std::sync::Mutex;
    use std::time::Duration;

    struct TimedProvider(Arc<Mutex<Duration>>);
    impl Provider for TimedProvider {
        fn status(&self) -> Result<Status> {
            Ok(if *self.0.lock().unwrap() >= Duration::from_secs(70) {
                Status::Denied
            } else {
                Status::NotDetermined
            })
        }
        fn has_usage_description(&self) -> Result<bool> {
            panic!("monitor must not inspect the bundle")
        }
        fn request_access(&self, _: Box<dyn Fn(bool) + Send + Sync>) -> Result<()> {
            panic!("monitor must not request consent")
        }
    }

    let clock = Arc::new(Mutex::new(Duration::ZERO));
    let stop_flag = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let generation = Generation::new(stop_flag.clone(), tx);
    monitor_until_stopped(
        &TimedProvider(clock.clone()),
        &stop_flag,
        &generation,
        || *clock.lock().unwrap(),
        |delay| {
            let delay = delay.expect("undecided consent must keep polling");
            assert!(delay <= Duration::from_secs(2));
            let mut elapsed = clock.lock().unwrap();
            *elapsed += delay;
            assert!(*elapsed <= Duration::from_secs(70));
        },
    );
    assert_eq!(*clock.lock().unwrap(), Duration::from_secs(70));
    assert!(matches!(rx.try_recv(), Ok(Event::PermissionPending { .. })));
    assert!(matches!(rx.try_recv(), Ok(Event::PermissionDenied { .. })));
    assert!(rx.try_recv().is_err());
    assert!(stop_flag.load(Ordering::SeqCst));
}

#[test]
fn pending_advisory_does_not_terminate_and_stop_or_drop_joins_the_owner() {
    use crate::mac_policy::{Provider, Status, POLL_SLOW_INTERVAL};
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    struct Pending(AtomicUsize);
    impl Provider for Pending {
        fn status(&self) -> Result<Status> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(Status::NotDetermined)
        }
        fn has_usage_description(&self) -> Result<bool> {
            panic!("polling must not inspect the bundle")
        }
        fn request_access(&self, _: Box<dyn Fn(bool) + Send + Sync>) -> Result<()> {
            panic!("polling must not request consent")
        }
    }

    for drop_backend in [false, true] {
        // Inject only the owner monitor into the backend's real stop/drop path.
        // No CPAL host, configuration query, or native stream is opened.
        let mut backend = crate::CpalMicBackend::with_format(None, (48_000, 1));
        let provider = Arc::new(Pending(AtomicUsize::new(0)));
        let owner_provider = provider.clone();
        let owner_stop = backend.stop_flag.clone();
        let generation = backend.generation.clone();
        let (parked_tx, parked_rx) = mpsc::channel();
        let (exited_tx, exited_rx) = mpsc::channel();
        backend.handle = Some(thread::spawn(move || {
            monitor_until_stopped(
                &*owner_provider,
                &owner_stop,
                &generation,
                || Duration::from_secs(70),
                |delay| {
                    assert_eq!(delay, Some(POLL_SLOW_INTERVAL));
                    assert!(!owner_stop.load(Ordering::SeqCst));
                    parked_tx.send(()).unwrap();
                    thread::park_timeout(POLL_SLOW_INTERVAL);
                },
            );
            exited_tx.send(()).unwrap();
        }));
        parked_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(
            backend.events.try_recv(),
            Ok(Event::PermissionPending { .. })
        ));
        assert!(backend.terminal_error.is_none());
        if drop_backend {
            drop(backend);
        } else {
            backend.stop();
            assert!(backend.handle.is_none());
        }
        // stop/drop returned only after the owner exited; this is not a wait.
        exited_rx.try_recv().unwrap();
        assert_eq!(provider.0.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn consent_query_failure_is_terminal_typed_and_closes_owner_delivery() {
    use crate::mac_policy::{ConsentPoll, Provider, Status, POLL_INTERVAL};

    struct FailedQuery;
    impl Provider for FailedQuery {
        fn status(&self) -> Result<Status> {
            Err(Error::Backend(
                "injected authorization query failure".into(),
            ))
        }
        fn has_usage_description(&self) -> Result<bool> {
            panic!("polling must not inspect the bundle")
        }
        fn request_access(&self, _: Box<dyn Fn(bool) + Send + Sync>) -> Result<()> {
            panic!("polling must not request consent")
        }
    }

    let stop_flag = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let generation = Generation::new(stop_flag.clone(), tx);
    let mut poll = ConsentPoll::new();
    assert!(check_pending(
        &mut poll,
        POLL_INTERVAL,
        &FailedQuery,
        &stop_flag,
        &generation
    ));
    assert!(stop_flag.load(Ordering::SeqCst));
    assert!(!poll.active());
    assert_eq!(
        rx.try_recv().unwrap(),
        Event::TerminalError {
            error: Error::Backend("injected authorization query failure".into()),
        }
    );
    assert!(!check_pending(
        &mut poll,
        POLL_INTERVAL * 2,
        &FailedQuery,
        &stop_flag,
        &generation
    ));
    assert!(rx.try_recv().is_err());
}

#[test]
fn owner_denial_gates_delivery_before_notification_without_joining() {
    let stop_flag = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let generation = Generation::new(stop_flag.clone(), tx);
    terminate(
        &generation,
        Event::PermissionDenied {
            permission: Permission::Microphone,
            detail: "injected late denial".into(),
        },
    );
    assert!(stop_flag.load(Ordering::SeqCst));
    assert!(matches!(
        rx.try_recv(),
        Ok(Event::PermissionDenied {
            permission: Permission::Microphone,
            ..
        })
    ));
    assert!(rx.try_recv().is_err());
}

#[test]
fn late_consent_queries_and_shutdown_run_on_owner_with_injected_clock() {
    use crate::mac_policy::{ConsentPoll, Provider, Status};
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::time::Duration;

    struct Fake {
        statuses: Mutex<VecDeque<Status>>,
        calls: Mutex<Vec<thread::ThreadId>>,
    }
    impl Provider for Fake {
        fn status(&self) -> Result<Status> {
            self.calls.lock().unwrap().push(thread::current().id());
            Ok(self.statuses.lock().unwrap().pop_front().unwrap())
        }
        fn has_usage_description(&self) -> Result<bool> {
            panic!("polling must not inspect prompt prerequisites")
        }
        fn request_access(&self, _: Box<dyn Fn(bool) + Send + Sync>) -> Result<()> {
            panic!("polling must not request consent")
        }
    }
    let provider = Arc::new(Fake {
        statuses: Mutex::new(VecDeque::from([Status::NotDetermined, Status::Denied])),
        calls: Mutex::new(Vec::new()),
    });
    let stop_flag = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let generation = Generation::new(stop_flag.clone(), tx);
    let owner_provider = provider.clone();
    let owner_stop = stop_flag.clone();
    let owner = thread::spawn(move || {
        let mut poll = ConsentPoll::new();
        assert!(!check_pending(
            &mut poll,
            Duration::from_millis(500),
            &*owner_provider,
            &owner_stop,
            &generation
        ));
        assert!(!owner_stop.load(Ordering::SeqCst));
        assert!(check_pending(
            &mut poll,
            Duration::from_secs(1),
            &*owner_provider,
            &owner_stop,
            &generation
        ));
        assert!(!check_pending(
            &mut poll,
            Duration::from_secs(2),
            &*owner_provider,
            &owner_stop,
            &generation
        ));
        thread::current().id()
    });
    let owner_id = owner.join().unwrap();
    assert_ne!(owner_id, thread::current().id());
    assert_eq!(*provider.calls.lock().unwrap(), vec![owner_id, owner_id]);
    assert!(stop_flag.load(Ordering::SeqCst));
    assert!(matches!(rx.try_recv(), Ok(Event::PermissionDenied { .. })));
    assert!(rx.try_recv().is_err());
}
