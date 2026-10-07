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
fn prompt_refusal_and_timeout_are_permission_errors() {
    for answer in [Some(false), None] {
        let fake = Fake::new(Status::NotDetermined, true, answer);
        assert!(matches!(
            check(&fake),
            Err(Error::PermissionDenied {
                permission: Permission::Microphone,
                ..
            })
        ));
        assert_eq!(fake.requests.load(Ordering::SeqCst), 1);
    }
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
        assert!(matches!(
            preflight(&fake, &coordinator, Duration::ZERO),
            Err(Error::PermissionDenied { .. })
        ));
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
fn completion_requires_authoritative_authorization() {
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
    assert!(matches!(
        preflight(&Inconsistent, &PromptCoordinator::default(), Duration::ZERO),
        Err(Error::PermissionDenied { .. })
    ));
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
    assert_eq!(coordinator.request(&fake, Duration::ZERO), Ok(true));
    assert_eq!(fake.requests.load(Ordering::SeqCst), 1);
    *fake.status.lock().unwrap() = Ok(Status::Denied);
    assert_eq!(coordinator.request(&fake, Duration::ZERO), Ok(false));
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
fn polling_ends_at_sixty_seconds_and_query_failure_is_not_denial() {
    let fake = Fake::new(Status::NotDetermined, false, None);
    let mut poll = ConsentPoll::new();
    assert_eq!(poll.poll(POLL_TIMEOUT, &fake), Ok(None));
    assert!(!poll.active());
    let mut poll = ConsentPoll::new();
    assert_eq!(
        poll.poll(POLL_TIMEOUT + Duration::from_millis(1), &fake),
        Ok(None)
    );
    assert!(!poll.active());
    *fake.status.lock().unwrap() = Err(Error::Backend("injected failure".into()));
    assert!(matches!(
        ConsentPoll::new().poll(POLL_INTERVAL, &fake),
        Err(Error::Backend(_))
    ));
}

#[test]
fn default_deadlines_match_contract() {
    assert_eq!(PROMPT_TIMEOUT, Duration::from_secs(30));
    assert_eq!(POLL_INTERVAL, Duration::from_millis(500));
    assert_eq!(POLL_TIMEOUT, Duration::from_secs(60));
}
