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
    metadata_queries: AtomicUsize,
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
            metadata_queries: AtomicUsize::new(0),
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
        self.metadata_queries.fetch_add(1, Ordering::SeqCst);
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

fn check(fake: &Fake) -> Result<()> {
    preflight(
        crate::permission::Purpose::Capture,
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
    assert_eq!(check(&fake), Ok(()));
    assert_eq!(fake.requests.load(Ordering::SeqCst), 0);
}

#[test]
fn unanswered_prompt_returns_bounded_error_without_entering_blocking_hal_query() {
    let fake = Fake::new(Status::NotDetermined, true, None);
    let queries = AtomicUsize::new(0);
    let (_release, blocked) = std::sync::mpsc::channel::<()>();
    let started = Instant::now();
    let result = crate::permission::format_after_preflight(
        || {
            preflight(
                crate::permission::Purpose::Capture,
                &fake,
                &PromptCoordinator::default(),
                Duration::from_millis(5),
            )
        },
        || {
            queries.fetch_add(1, Ordering::SeqCst);
            // Simulate a native query that waits for undecided consent. The test
            // has its own bound so a regression cannot hang CI.
            let _ = blocked.recv_timeout(Duration::from_millis(100));
            None
        },
    );
    assert_eq!(
        queries.load(Ordering::SeqCst),
        0,
        "undecided consent must not enter HAL"
    );
    assert!(started.elapsed() < Duration::from_secs(1));
    let error = result.expect_err("an unanswered prompt must fail closed");
    assert_eq!(error.kind(), flexaudio_core::ErrorKind::Backend);
    assert_eq!(
        error.permission(),
        None,
        "an unanswered prompt is not a denial"
    );
    for guidance in ["unanswered", "retry", "Microphone"] {
        assert!(error.to_string().contains(guidance));
    }
    assert_eq!(fake.requests.load(Ordering::SeqCst), 1);
}

#[test]
fn authorized_format_query_control_reads_native_format_once() {
    let fake = Fake::new(Status::Authorized, false, None);
    let queries = AtomicUsize::new(0);
    let result = crate::permission::format_after_preflight(
        || check(&fake),
        || {
            queries.fetch_add(1, Ordering::SeqCst);
            Some((44_100, 2))
        },
    );
    assert_eq!(result, Ok((44_100, 2)));
    assert_eq!(queries.load(Ordering::SeqCst), 1);
    assert_eq!(fake.requests.load(Ordering::SeqCst), 0);
}

#[test]
fn revoked_consent_is_rechecked_before_another_native_query() {
    let fake = Fake::new(Status::Authorized, false, None);
    assert!(check(&fake).is_ok());
    *fake.status.lock().expect("test status lock") = Ok(Status::Denied);
    let result = crate::permission::format_after_preflight(
        || check(&fake),
        || panic!("revoked consent must not enter the native query"),
    );
    assert!(matches!(
        result,
        Err(Error::PermissionDenied {
            permission: Permission::Microphone,
            ..
        })
    ));
    assert_eq!(fake.requests.load(Ordering::SeqCst), 0);
}

#[test]
fn missing_usage_description_fails_without_entering_hal_or_requesting_access() {
    let fake = Fake::new(Status::NotDetermined, false, None);
    let result = crate::permission::format_after_preflight(
        || check(&fake),
        || panic!("undecided consent without usage metadata must not query HAL"),
    );
    let error = result.expect_err("missing prompt metadata must fail closed");
    assert_eq!(error.kind(), flexaudio_core::ErrorKind::Backend);
    for guidance in ["NSMicrophoneUsageDescription", "retry", "responsible"] {
        assert!(error.to_string().contains(guidance));
    }
    assert_eq!(fake.requests.load(Ordering::SeqCst), 0);
}

#[test]
fn pending_without_usage_description_fails_closed() {
    let fake = Fake::new(Status::NotDetermined, false, None);
    assert!(matches!(check(&fake), Err(Error::Backend(_))));
    assert_eq!(fake.requests.load(Ordering::SeqCst), 0);
}

#[test]
fn pending_with_usage_description_grant_proceeds() {
    let fake = Fake::new(Status::NotDetermined, true, Some(true));
    assert_eq!(check(&fake), Ok(()));
    assert_eq!(fake.requests.load(Ordering::SeqCst), 1);
}

#[test]
fn prompt_refusal_is_a_permission_error_and_timeout_is_a_backend_error() {
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
    assert!(matches!(check(&fake), Err(Error::Backend(_))));
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
            thread::spawn(move || {
                preflight(
                    crate::permission::Purpose::Capture,
                    &*fake,
                    &coordinator,
                    Duration::from_secs(2),
                )
            })
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
        assert_eq!(handle.join().unwrap(), Ok(()));
    }
    assert_eq!(fake.requests.load(Ordering::SeqCst), 1);
}

#[test]
fn timeout_keeps_native_prompt_shared_and_accepts_late_completion() {
    let fake = Fake::new(Status::NotDetermined, true, None);
    let coordinator = PromptCoordinator::default();
    for _ in 0..2 {
        assert!(matches!(
            preflight(
                crate::permission::Purpose::Capture,
                &fake,
                &coordinator,
                Duration::ZERO
            ),
            Err(Error::Backend(_))
        ));
    }
    assert_eq!(fake.requests.load(Ordering::SeqCst), 1);
    fake.complete(true);
    assert_eq!(
        preflight(
            crate::permission::Purpose::Capture,
            &fake,
            &coordinator,
            Duration::ZERO
        ),
        Ok(())
    );
    *fake.status.lock().unwrap() = Ok(Status::Denied);
    assert!(matches!(
        preflight(
            crate::permission::Purpose::Capture,
            &fake,
            &coordinator,
            Duration::ZERO
        ),
        Err(Error::PermissionDenied { .. })
    ));
}

#[test]
fn completion_without_authoritative_authorization_fails_closed() {
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
        preflight(
            crate::permission::Purpose::Capture,
            &Inconsistent,
            &PromptCoordinator::default(),
            Duration::ZERO
        ),
        Err(Error::Backend(_))
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
            preflight(
                crate::permission::Purpose::Capture,
                &fake,
                &PromptCoordinator::default(),
                Duration::ZERO
            ),
            Err(Error::PermissionDenied { .. })
        ));
    }
    let fake = Fake::new(Status::NotDetermined, true, Some(true));
    let coordinator = PromptCoordinator::default();
    assert_eq!(
        preflight(
            crate::permission::Purpose::Capture,
            &fake,
            &coordinator,
            Duration::ZERO
        ),
        Ok(())
    );
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
    assert_eq!(
        preflight(
            crate::permission::Purpose::Capture,
            &fake,
            &coordinator,
            Duration::ZERO
        ),
        Ok(())
    );
    assert_eq!(fake.requests.load(Ordering::SeqCst), 2);
}

#[test]
fn expired_wait_is_shared_without_prompting_or_waiting_again() {
    let fake = Fake::new(Status::NotDetermined, true, None);
    let coordinator = PromptCoordinator::default();
    assert!(matches!(
        preflight(
            crate::permission::Purpose::Capture,
            &fake,
            &coordinator,
            Duration::ZERO
        ),
        Err(Error::Backend(_))
    ));
    // A real wait would take 30 seconds; an already expired prompt must return now.
    assert!(matches!(
        preflight(
            crate::permission::Purpose::Capture,
            &fake,
            &coordinator,
            PROMPT_TIMEOUT
        ),
        Err(Error::Backend(_))
    ));
    assert_eq!(fake.requests.load(Ordering::SeqCst), 1);
    fake.complete(false);
    assert!(matches!(
        preflight(
            crate::permission::Purpose::Capture,
            &fake,
            &coordinator,
            Duration::ZERO
        ),
        Err(Error::PermissionDenied { .. })
    ));
}

#[test]
fn prompt_timeout_matches_documented_contract() {
    assert_eq!(PROMPT_TIMEOUT, Duration::from_secs(30));
}

#[test]
fn enumeration_undecided_never_prompts_or_enters_hal() {
    let fake = Fake::new(Status::NotDetermined, true, Some(true));
    let queries = AtomicUsize::new(0);
    let result = crate::permission::query_after_preflight(
        || {
            preflight(
                Purpose::Enumeration,
                &fake,
                &PromptCoordinator::default(),
                Duration::from_millis(5),
            )
        },
        || {
            queries.fetch_add(1, Ordering::SeqCst);
            Ok(())
        },
    );
    assert_eq!(
        fake.requests.load(Ordering::SeqCst),
        0,
        "enumeration must not request microphone access"
    );
    assert_eq!(
        fake.metadata_queries.load(Ordering::SeqCst),
        0,
        "enumeration must not inspect prompting metadata"
    );
    assert_eq!(
        queries.load(Ordering::SeqCst),
        0,
        "undecided enumeration must not enter HAL"
    );
    let error =
        result.expect_err("mandatory device format cannot be queried with undecided consent");
    assert_eq!(error.kind(), flexaudio_core::ErrorKind::Backend);
    for guidance in ["undecided", "enumerat", "Microphone", "retry"] {
        assert!(error.to_string().contains(guidance));
    }
}

#[test]
fn enumeration_authorized_and_denied_controls_never_prompt() {
    for status in [Status::Authorized, Status::Denied, Status::Restricted] {
        let fake = Fake::new(status, false, None);
        let queries = AtomicUsize::new(0);
        let result = crate::permission::query_after_preflight(
            || {
                preflight(
                    Purpose::Enumeration,
                    &fake,
                    &PromptCoordinator::default(),
                    Duration::ZERO,
                )
            },
            || {
                queries.fetch_add(1, Ordering::SeqCst);
                Ok((44_100, 2))
            },
        );
        if status == Status::Authorized {
            assert_eq!(result, Ok((44_100, 2)));
            assert_eq!(queries.load(Ordering::SeqCst), 1);
        } else {
            assert!(matches!(
                result,
                Err(Error::PermissionDenied {
                    permission: Permission::Microphone,
                    ..
                })
            ));
            assert_eq!(queries.load(Ordering::SeqCst), 0);
        }
        assert_eq!(fake.requests.load(Ordering::SeqCst), 0);
        assert_eq!(fake.metadata_queries.load(Ordering::SeqCst), 0);
    }
}
