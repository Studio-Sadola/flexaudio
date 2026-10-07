//! Capture owner lifecycle and control-thread consent monitoring.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;

use cpal::traits::StreamTrait;
use flexaudio_core::backend::RawSink;
use flexaudio_core::types::{Error, Event, Result};

use crate::{build_stream, permission};

pub(crate) fn run(
    sink: RawSink,
    device_id: Option<String>,
    stop_flag: Arc<AtomicBool>,
    ready_tx: mpsc::Sender<Result<()>>,
    event_tx: mpsc::Sender<Event>,
) {
    // Query again on the owner to cover revocation between construction and start.
    // No authorization work runs in a cpal audio callback.
    let watch_pending = match permission::preflight() {
        Ok(watch) => watch,
        Err(error) => {
            let _ = ready_tx.send(Err(error));
            return;
        }
    };
    let stream = match build_stream(sink, device_id.as_deref(), stop_flag.clone()) {
        Ok(stream) => stream,
        Err(error) => {
            let _ = ready_tx.send(Err(permission::after_failure(error)));
            return;
        }
    };
    if let Err(error) = stream.play() {
        let error = permission::after_failure(Error::Backend(format!("cpal play: {error}")));
        let _ = ready_tx.send(Err(error));
        return;
    }
    if ready_tx.send(Ok(())).is_err() {
        return;
    }

    wait_until_stopped(watch_pending, &stop_flag, &event_tx);
    // The owner drops its stream directly. It never calls a joining backend stop.
    drop(stream);
}

fn wait_until_stopped(
    _watch_pending: bool,
    stop_flag: &AtomicBool,
    _event_tx: &mpsc::Sender<Event>,
) {
    #[cfg(target_os = "macos")]
    let mut monitor = _watch_pending.then(crate::mac_policy::ConsentPoll::new);
    #[cfg(target_os = "macos")]
    let started = std::time::Instant::now();

    while !stop_flag.load(Ordering::SeqCst) {
        #[cfg(target_os = "macos")]
        if let Some(poll) = monitor.as_mut() {
            if check_pending(
                poll,
                started.elapsed(),
                &crate::mac_permission::Native,
                stop_flag,
                _event_tx,
            ) {
                break;
            }
            if poll.active() {
                thread::park_timeout(crate::mac_policy::POLL_INTERVAL);
                continue;
            }
            monitor = None;
        }
        thread::park();
    }
}

#[cfg(any(target_os = "macos", test))]
fn check_pending(
    poll: &mut crate::mac_policy::ConsentPoll,
    elapsed: std::time::Duration,
    provider: &dyn crate::mac_policy::Provider,
    stop_flag: &AtomicBool,
    events: &mpsc::Sender<Event>,
) -> bool {
    let event = match poll.poll(elapsed, provider) {
        Ok(Some(event)) => event,
        Err(error) => Event::TerminalError { error },
        Ok(None) => return false,
    };
    terminate(stop_flag, events, event);
    true
}

#[cfg(any(target_os = "macos", test))]
fn terminate(stop_flag: &AtomicBool, events: &mpsc::Sender<Event>, event: Event) {
    // Gate RT delivery before making denial visible. Dropping the stream happens
    // on this owner, never on the audio callback and never by joining ourselves.
    stop_flag.store(true, Ordering::SeqCst);
    let _ = events.send(event);
}

#[cfg(test)]
mod tests {
    use super::*;
    use flexaudio_core::types::Permission;

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

        let stop_flag = AtomicBool::new(false);
        let (tx, rx) = mpsc::channel();
        let mut poll = ConsentPoll::new();
        assert!(check_pending(
            &mut poll,
            POLL_INTERVAL,
            &FailedQuery,
            &stop_flag,
            &tx
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
            &tx
        ));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn owner_denial_gates_delivery_before_notification_without_joining() {
        let stop_flag = AtomicBool::new(false);
        let (tx, rx) = mpsc::channel();
        terminate(
            &stop_flag,
            &tx,
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
        let owner_provider = provider.clone();
        let owner_stop = stop_flag.clone();
        let owner = thread::spawn(move || {
            let mut poll = ConsentPoll::new();
            assert!(!check_pending(
                &mut poll,
                Duration::from_millis(500),
                &*owner_provider,
                &owner_stop,
                &tx
            ));
            assert!(!owner_stop.load(Ordering::SeqCst));
            assert!(check_pending(
                &mut poll,
                Duration::from_secs(1),
                &*owner_provider,
                &owner_stop,
                &tx
            ));
            assert!(!check_pending(
                &mut poll,
                Duration::from_secs(2),
                &*owner_provider,
                &owner_stop,
                &tx
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
}
