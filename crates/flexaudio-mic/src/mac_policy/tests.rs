use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Instant;

type Completion = Box<dyn Fn(bool) + Send + Sync>;

struct Fake {
    status: Mutex<Result<Status>>,
    usage: Result<bool>,
    answer: Option<bool>,
    request_error: bool,
    requests: AtomicUsize,
    completion: Mutex<Option<Completion>>,
}

impl Fake {
    fn new(status: Status, usage: bool, answer: Option<bool>) -> Self {
        Self {
            status: Mutex::new(Ok(status)),
            usage: Ok(usage),
            answer,
            request_error: false,
            requests: AtomicUsize::new(0),
            completion: Mutex::new(None),
        }
    }

    fn complete(&self, granted: bool) {
        *self.status.lock().unwrap() = Ok(if granted {
            Status::Authorized
        } else {
            Status::Denied
        });
        self.completion.lock().unwrap().take().unwrap()(granted);
    }
}

impl Provider for Fake {
    fn status(&self) -> Result<Status> {
        self.status.lock().unwrap().clone()
    }
    fn has_usage_description(&self) -> Result<bool> {
        self.usage.clone()
    }
    fn request_access(&self, completion: Completion) -> Result<()> {
        self.requests.fetch_add(1, Ordering::SeqCst);
        if self.request_error {
            return Err(Error::Backend("injected request failure".into()));
        }
        *self.completion.lock().unwrap() = Some(completion);
        if let Some(answer) = self.answer {
            self.complete(answer);
        }
        Ok(())
    }
}

fn check(fake: &Fake) -> Result<bool> {
    preflight(
        fake,
        &PromptCoordinator::default(),
        Duration::from_millis(1),
    )
}

#[test]
fn denied_and_restricted_fail_before_any_prompt() {
    for status in [Status::Denied, Status::Restricted] {
        let fake = Fake::new(status, true, None);
        assert!(matches!(
            check(&fake),
            Err(Error::PermissionDenied {
                permission: Permission::Microphone,
                ..
            })
        ));
        assert_eq!(fake.requests.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn authorized_proceeds_without_metadata_or_prompt() {
    let fake = Fake::new(Status::Authorized, false, None);
    assert_eq!(check(&fake), Ok(false));
    assert_eq!(fake.requests.load(Ordering::SeqCst), 0);
}

#[test]
fn pending_without_usage_description_proceeds_with_owner_polling() {
    let fake = Fake::new(Status::NotDetermined, false, None);
    assert_eq!(check(&fake), Ok(true));
    assert_eq!(fake.requests.load(Ordering::SeqCst), 0);
}

#[test]
fn pending_with_usage_description_grant_proceeds() {
    let fake = Fake::new(Status::NotDetermined, true, Some(true));
    assert_eq!(check(&fake), Ok(false));
    assert_eq!(fake.requests.load(Ordering::SeqCst), 1);
}

#[test]
fn prompt_refusal_is_a_permission_error_and_timeout_remains_pending() {
    let fake = Fake::new(Status::NotDetermined, true, Some(false));
    assert!(matches!(
        check(&fake),
        Err(Error::PermissionDenied {
            permission: Permission::Microphone,
            ..
        })
    ));
    assert_eq!(fake.requests.load(Ordering::SeqCst), 1);
    let fake = Fake::new(Status::NotDetermined, true, None);
    assert_eq!(check(&fake), Ok(true));
    assert_eq!(fake.requests.load(Ordering::SeqCst), 1);
}

#[test]
fn query_metadata_and_request_failures_fail_closed_without_inventing_denial() {
    let mut fake = Fake::new(Status::NotDetermined, true, None);
    *fake.status.lock().unwrap() = Err(Error::Backend("injected query failure".into()));
    assert!(matches!(check(&fake), Err(Error::Backend(_))));
    *fake.status.lock().unwrap() = Ok(Status::NotDetermined);
    fake.usage = Err(Error::Backend("injected metadata failure".into()));
    assert!(matches!(check(&fake), Err(Error::Backend(_))));
    fake.usage = Ok(true);
    fake.request_error = true;
    assert!(matches!(check(&fake), Err(Error::Backend(_))));
}

#[test]
fn concurrent_opens_share_one_prompt() {
    let fake = Arc::new(Fake::new(Status::NotDetermined, true, None));
    let coordinator = Arc::new(PromptCoordinator::default());
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let fake = fake.clone();
            let coordinator = coordinator.clone();
            thread::spawn(move || preflight(&*fake, &coordinator, Duration::from_secs(2)))
        })
        .collect();
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let both_waiting = coordinator
            .current
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|prompt| Arc::strong_count(prompt) >= 4);
        if both_waiting && fake.completion.lock().unwrap().is_some() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "both opens must join the same outstanding prompt"
        );
        thread::yield_now();
    }
    fake.complete(true);
    for handle in handles {
        assert_eq!(handle.join().unwrap(), Ok(false));
    }
    assert_eq!(fake.requests.load(Ordering::SeqCst), 1);
}

#[test]
fn timeout_keeps_native_prompt_shared_and_accepts_late_completion() {
    let fake = Fake::new(Status::NotDetermined, true, None);
    let coordinator = PromptCoordinator::default();
    for _ in 0..2 {
        assert_eq!(preflight(&fake, &coordinator, Duration::ZERO), Ok(true));
    }
    assert_eq!(fake.requests.load(Ordering::SeqCst), 1);
    fake.complete(true);
    assert_eq!(preflight(&fake, &coordinator, Duration::ZERO), Ok(false));
    *fake.status.lock().unwrap() = Ok(Status::Denied);
    assert!(matches!(
        preflight(&fake, &coordinator, Duration::ZERO),
        Err(Error::PermissionDenied { .. })
    ));
}

#[test]
fn completion_still_polls_until_authorization_is_authoritative() {
    struct Inconsistent;
    impl Provider for Inconsistent {
        fn status(&self) -> Result<Status> {
            Ok(Status::NotDetermined)
        }
        fn has_usage_description(&self) -> Result<bool> {
            Ok(true)
        }
        fn request_access(&self, completion: Completion) -> Result<()> {
            completion(true);
            Ok(())
        }
    }
    assert_eq!(
        preflight(&Inconsistent, &PromptCoordinator::default(), Duration::ZERO),
        Ok(true)
    );
}

#[test]
fn post_prompt_denial_and_restriction_fail_closed_and_consent_is_not_cached() {
    use std::sync::atomic::AtomicBool;

    struct AfterPrompt {
        requested: AtomicBool,
        status: Status,
    }
    impl Provider for AfterPrompt {
        fn status(&self) -> Result<Status> {
            Ok(if self.requested.load(Ordering::SeqCst) {
                self.status
            } else {
                Status::NotDetermined
            })
        }
        fn has_usage_description(&self) -> Result<bool> {
            Ok(true)
        }
        fn request_access(&self, completion: Completion) -> Result<()> {
            self.requested.store(true, Ordering::SeqCst);
            completion(true);
            Ok(())
        }
    }
    for status in [Status::Denied, Status::Restricted] {
        let fake = AfterPrompt {
            requested: AtomicBool::new(false),
            status,
        };
        assert!(matches!(
            preflight(&fake, &PromptCoordinator::default(), Duration::ZERO),
            Err(Error::PermissionDenied { .. })
        ));
    }
    let fake = Fake::new(Status::NotDetermined, true, Some(true));
    let coordinator = PromptCoordinator::default();
    assert_eq!(preflight(&fake, &coordinator, Duration::ZERO), Ok(false));
    // A caller that raced the completed grant must not initiate a second prompt.
    assert_eq!(
        coordinator.request(&fake, Duration::ZERO),
        Ok(Decision::Granted)
    );
    assert_eq!(fake.requests.load(Ordering::SeqCst), 1);
    *fake.status.lock().unwrap() = Ok(Status::Denied);
    assert_eq!(
        coordinator.request(&fake, Duration::ZERO),
        Ok(Decision::Refused)
    );
    // Resetting consent creates a new request, rather than caching the old grant.
    *fake.status.lock().unwrap() = Ok(Status::NotDetermined);
    assert_eq!(preflight(&fake, &coordinator, Duration::ZERO), Ok(false));
    assert_eq!(fake.requests.load(Ordering::SeqCst), 2);
}

#[test]
fn late_denial_is_emitted_once_and_authorization_ends_polling() {
    for status in [Status::Denied, Status::Restricted] {
        let fake = Fake::new(Status::NotDetermined, false, None);
        let mut poll = ConsentPoll::new();
        assert!(poll.active());
        assert_eq!(poll.poll(Duration::from_millis(499), &fake), Ok(None));
        assert_eq!(poll.poll(POLL_INTERVAL, &fake), Ok(None));
        *fake.status.lock().unwrap() = Ok(status);
        assert!(matches!(
            poll.poll(Duration::from_secs(1), &fake),
            Ok(Some(Event::PermissionDenied {
                permission: Permission::Microphone,
                ..
            }))
        ));
        assert!(!poll.active());
        assert_eq!(poll.poll(Duration::from_secs(2), &fake), Ok(None));
    }
    let fake = Fake::new(Status::Authorized, false, None);
    let mut poll = ConsentPoll::new();
    assert_eq!(poll.poll(POLL_INTERVAL, &fake), Ok(None));
    assert!(!poll.active());
}

#[test]
fn query_failure_is_not_a_permission_denial() {
    let fake = Fake::new(Status::NotDetermined, false, None);
    *fake.status.lock().unwrap() = Err(Error::Backend("injected failure".into()));
    assert!(matches!(
        ConsentPoll::new().poll(POLL_INTERVAL, &fake),
        Err(Error::Backend(_))
    ));
}

#[test]
fn default_intervals_and_grace_match_contract() {
    assert_eq!(PROMPT_TIMEOUT, Duration::from_secs(30));
    assert_eq!(POLL_INTERVAL, Duration::from_millis(500));
    assert_eq!(POLL_SLOW_AFTER, Duration::from_secs(60));
    assert_eq!(POLL_SLOW_INTERVAL, Duration::from_secs(2));
    assert_eq!(PENDING_GRACE, Duration::from_secs(5));
}

#[test]
fn undecided_permission_emits_one_advisory_after_five_seconds_per_generation() {
    let fake = Fake::new(Status::NotDetermined, false, None);
    for _ in 0..2 {
        let mut poll = ConsentPoll::new();
        for half_second in 1..10 {
            assert_eq!(
                poll.poll(Duration::from_millis(half_second * 500), &fake),
                Ok(None)
            );
        }
        assert_eq!(
            poll.poll(PENDING_GRACE, &fake),
            Ok(Some(Event::PermissionPending {
                permission: Permission::Microphone,
                detail: PENDING_DETAIL.into(),
            }))
        );
        assert!(poll.active());
        for seconds in [6, 60, 70, 3600] {
            assert_eq!(poll.poll(Duration::from_secs(seconds), &fake), Ok(None));
            assert!(poll.active());
        }
    }
    for guidance in [
        "not been decided",
        "silence",
        "ssh",
        "launchd",
        "Terminal",
        "NSMicrophoneUsageDescription",
        "Privacy & Security > Microphone",
    ] {
        assert!(PENDING_DETAIL.contains(guidance));
    }
}

#[test]
fn denial_or_restriction_after_seventy_seconds_is_still_terminal() {
    for status in [Status::Denied, Status::Restricted] {
        let fake = Fake::new(Status::NotDetermined, false, None);
        let mut poll = ConsentPoll::new();
        assert!(matches!(
            poll.poll(PENDING_GRACE, &fake),
            Ok(Some(Event::PermissionPending { .. }))
        ));
        assert_eq!(poll.poll(POLL_SLOW_AFTER, &fake), Ok(None));
        assert_eq!(poll.wait_duration(POLL_SLOW_AFTER), POLL_SLOW_INTERVAL);
        *fake.status.lock().unwrap() = Ok(status);
        assert!(matches!(
            poll.poll(Duration::from_secs(70), &fake),
            Ok(Some(Event::PermissionDenied {
                permission: Permission::Microphone,
                ..
            }))
        ));
        assert!(!poll.active());
        assert_eq!(poll.poll(Duration::from_secs(90), &fake), Ok(None));
    }
}

#[test]
fn authorization_before_grace_emits_no_advisory_and_late_grant_ends_polling() {
    let fake = Fake::new(Status::NotDetermined, false, None);
    let mut poll = ConsentPoll::new();
    assert_eq!(poll.poll(POLL_INTERVAL, &fake), Ok(None));
    *fake.status.lock().unwrap() = Ok(Status::Authorized);
    assert_eq!(poll.poll(PENDING_GRACE, &fake), Ok(None));
    assert!(!poll.active());
    *fake.status.lock().unwrap() = Err(Error::Backend("must not query again".into()));
    assert_eq!(poll.poll(Duration::from_secs(70), &fake), Ok(None));

    let fake = Fake::new(Status::NotDetermined, false, None);
    let mut poll = ConsentPoll::new();
    assert!(matches!(
        poll.poll(PENDING_GRACE, &fake),
        Ok(Some(Event::PermissionPending { .. }))
    ));
    *fake.status.lock().unwrap() = Ok(Status::Authorized);
    assert_eq!(poll.poll(Duration::from_secs(70), &fake), Ok(None));
    assert!(!poll.active());
}

#[test]
fn expired_wait_is_shared_and_owner_startup_does_not_wait_again() {
    let fake = Fake::new(Status::NotDetermined, true, None);
    let coordinator = PromptCoordinator::default();
    assert_eq!(preflight(&fake, &coordinator, Duration::ZERO), Ok(true));
    // A real wait would take 30 seconds; an already expired prompt must return now.
    assert_eq!(preflight(&fake, &coordinator, PROMPT_TIMEOUT), Ok(true));
    assert_eq!(fake.requests.load(Ordering::SeqCst), 1);
    fake.complete(false);
    assert!(matches!(
        preflight(&fake, &coordinator, Duration::ZERO),
        Err(Error::PermissionDenied { .. })
    ));
}
