//! Device-free microphone consent policy and single-flight prompt coordination.

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use flexaudio_core::types::{Error, Event, Permission, Result};

pub(crate) const PROMPT_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const POLL_INTERVAL: Duration = Duration::from_millis(500);
pub(crate) const POLL_SLOW_AFTER: Duration = Duration::from_secs(60);
pub(crate) const POLL_SLOW_INTERVAL: Duration = Duration::from_secs(2);
pub(crate) const PENDING_GRACE: Duration = Duration::from_secs(5);
pub(crate) const PENDING_DETAIL: &str = "Microphone permission has not been decided. macOS delivers silence until microphone permission is granted. If the process was started from ssh, launchd, or another context that cannot show the consent prompt, run it from Terminal or an app bundle with a nonempty NSMicrophoneUsageDescription. Grant access to the responsible app in System Settings > Privacy & Security > Microphone, then retry from that host if necessary.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Status {
    Authorized,
    Denied,
    Restricted,
    NotDetermined,
}

pub(crate) trait Provider: Send + Sync {
    fn status(&self) -> Result<Status>;
    fn has_usage_description(&self) -> Result<bool>;
    fn request_access(&self, completion: Box<dyn Fn(bool) + Send + Sync>) -> Result<()>;
}

fn denied(detail: &str) -> Error {
    Error::PermissionDenied {
        permission: Permission::Microphone,
        detail: detail.into(),
    }
}

#[derive(Default)]
struct PromptState {
    result: Option<Result<bool>>,
    wait_expired: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Decision {
    Granted,
    Refused,
    Pending,
}

#[derive(Default)]
struct Prompt {
    state: Mutex<PromptState>,
    changed: Condvar,
}

impl Prompt {
    fn complete(&self, result: Result<bool>) {
        if let Ok(mut state) = self.state.lock() {
            if state.result.is_none() {
                state.result = Some(result);
                self.changed.notify_all();
            }
        }
    }

    fn wait(&self, timeout: Duration) -> Result<Decision> {
        let state = self.state.lock().map_err(|_| poisoned())?;
        let (mut state, _) = self
            .changed
            .wait_timeout_while(state, timeout, |state| {
                state.result.is_none() && !state.wait_expired
            })
            .map_err(|_| poisoned())?;
        match state.result.as_ref() {
            Some(result) => result.clone().map(|granted| {
                if granted {
                    Decision::Granted
                } else {
                    Decision::Refused
                }
            }),
            None => {
                // Expiration is shared by all openers, including capture startup.
                // The native request stays outstanding and can complete later.
                state.wait_expired = true;
                self.changed.notify_all();
                Ok(Decision::Pending)
            }
        }
    }
}

fn poisoned() -> Error {
    Error::Backend("microphone permission coordinator lock poisoned".into())
}

/// Shares only the outstanding prompt. Consent itself is always queried again.
#[derive(Default)]
pub(crate) struct PromptCoordinator {
    current: Mutex<Option<Arc<Prompt>>>,
}

impl PromptCoordinator {
    fn request(&self, provider: &dyn Provider, timeout: Duration) -> Result<Decision> {
        let (prompt, should_request) = {
            let mut current = self.current.lock().map_err(|_| poisoned())?;
            if let Some(prompt) = current.as_ref() {
                let pending = prompt
                    .state
                    .lock()
                    .map_err(|_| poisoned())?
                    .result
                    .is_none();
                if pending {
                    (prompt.clone(), false)
                } else {
                    // Another opener may have observed NotDetermined before the
                    // first callback completed. Recheck before creating another
                    // native request, rather than prompting twice for that race.
                    match provider.status()? {
                        Status::Authorized => return Ok(Decision::Granted),
                        Status::Denied | Status::Restricted => return Ok(Decision::Refused),
                        Status::NotDetermined => {}
                    }
                    let prompt = Arc::new(Prompt::default());
                    *current = Some(prompt.clone());
                    (prompt, true)
                }
            } else {
                let prompt = Arc::new(Prompt::default());
                *current = Some(prompt.clone());
                (prompt, true)
            }
        };
        if should_request {
            let completion = prompt.clone();
            if let Err(error) = provider.request_access(Box::new(move |granted| {
                completion.complete(Ok(granted));
            })) {
                prompt.complete(Err(error));
            }
        }
        // A timeout does not clear the outstanding native request. Further callers share
        // that request until its callback arrives, including a late completion.
        prompt.wait(timeout)
    }
}

/// True means consent is undecided and the capture owner must keep polling.
pub(crate) fn preflight(
    provider: &dyn Provider,
    coordinator: &PromptCoordinator,
    timeout: Duration,
) -> Result<bool> {
    match provider.status()? {
        Status::Authorized => Ok(false),
        Status::Denied => Err(denied("macOS denied microphone access")),
        Status::Restricted => Err(denied("macOS restricted microphone access")),
        Status::NotDetermined => {
            if !provider.has_usage_description()? {
                return Ok(true);
            }
            if coordinator.request(provider, timeout)? == Decision::Refused {
                return Err(denied("The microphone consent prompt was refused"));
            }
            match provider.status()? {
                Status::Authorized => Ok(false),
                Status::Denied => Err(denied("macOS denied microphone access after the prompt")),
                Status::Restricted => Err(denied(
                    "macOS restricted microphone access after the prompt",
                )),
                Status::NotDetermined => Ok(true),
            }
        }
    }
}

/// Owner-thread polling state. Elapsed time is injected; no clock or OS calls occur here.
pub(crate) struct ConsentPoll {
    next: Duration,
    finished: bool,
    pending_emitted: bool,
}

impl ConsentPoll {
    pub(crate) fn new() -> Self {
        Self {
            next: POLL_INTERVAL,
            finished: false,
            pending_emitted: false,
        }
    }

    pub(crate) fn active(&self) -> bool {
        !self.finished
    }

    pub(crate) fn wait_duration(&self, elapsed: Duration) -> Duration {
        self.next.saturating_sub(elapsed)
    }

    pub(crate) fn poll(
        &mut self,
        elapsed: Duration,
        provider: &dyn Provider,
    ) -> Result<Option<Event>> {
        if self.finished || elapsed < self.next {
            return Ok(None);
        }
        self.next = elapsed.saturating_add(if elapsed < POLL_SLOW_AFTER {
            POLL_INTERVAL
        } else {
            POLL_SLOW_INTERVAL
        });
        let status = match provider.status() {
            Ok(status) => status,
            Err(error) => {
                self.finished = true;
                return Err(error);
            }
        };
        match status {
            Status::Authorized => {
                self.finished = true;
                Ok(None)
            }
            Status::NotDetermined => {
                if elapsed >= PENDING_GRACE && !self.pending_emitted {
                    self.pending_emitted = true;
                    return Ok(Some(Event::PermissionPending {
                        permission: Permission::Microphone,
                        detail: PENDING_DETAIL.into(),
                    }));
                }
                Ok(None)
            }
            Status::Denied | Status::Restricted => {
                self.finished = true;
                Ok(Some(Event::PermissionDenied {
                    permission: Permission::Microphone,
                    detail: "macOS denied or restricted microphone access after capture startup"
                        .into(),
                }))
            }
        }
    }
}

#[cfg(test)]
mod tests;
