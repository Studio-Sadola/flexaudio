//! Device-free microphone consent policy and single-flight prompt coordination.

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use flexaudio_core::types::{Error, Event, Permission, Result};

pub(crate) const PROMPT_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const POLL_INTERVAL: Duration = Duration::from_millis(500);
pub(crate) const POLL_TIMEOUT: Duration = Duration::from_secs(60);

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
struct Prompt {
    result: Mutex<Option<Result<bool>>>,
    changed: Condvar,
}

impl Prompt {
    fn complete(&self, result: Result<bool>) {
        if let Ok(mut state) = self.result.lock() {
            if state.is_none() {
                *state = Some(result);
                self.changed.notify_all();
            }
        }
    }

    fn wait(&self, timeout: Duration) -> Result<bool> {
        let state = self.result.lock().map_err(|_| poisoned())?;
        let (state, _) = self
            .changed
            .wait_timeout_while(state, timeout, |state| state.is_none())
            .map_err(|_| poisoned())?;
        match state.as_ref() {
            Some(result) => result.clone(),
            None => Err(denied(
                "The microphone consent prompt was not answered before the deadline",
            )),
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
    fn request(&self, provider: &dyn Provider, timeout: Duration) -> Result<bool> {
        let (prompt, should_request) = {
            let mut current = self.current.lock().map_err(|_| poisoned())?;
            if let Some(prompt) = current.as_ref() {
                let pending = prompt.result.lock().map_err(|_| poisoned())?.is_none();
                if pending {
                    (prompt.clone(), false)
                } else {
                    // Another opener may have observed NotDetermined before the
                    // first callback completed. Recheck before creating another
                    // native request, rather than prompting twice for that race.
                    match provider.status()? {
                        Status::Authorized => return Ok(true),
                        Status::Denied | Status::Restricted => return Ok(false),
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

/// True means the responsible application may prompt during capture startup.
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
            if !coordinator.request(provider, timeout)? {
                return Err(denied("The microphone consent prompt was refused"));
            }
            match provider.status()? {
                Status::Authorized => Ok(false),
                Status::Denied => Err(denied("macOS denied microphone access after the prompt")),
                Status::Restricted => Err(denied(
                    "macOS restricted microphone access after the prompt",
                )),
                Status::NotDetermined => Err(denied(
                    "macOS did not confirm microphone authorization after the prompt",
                )),
            }
        }
    }
}

/// Owner-thread polling state. Elapsed time is injected; no clock or OS calls occur here.
pub(crate) struct ConsentPoll {
    next: Duration,
    finished: bool,
}

impl ConsentPoll {
    pub(crate) fn new() -> Self {
        Self {
            next: POLL_INTERVAL,
            finished: false,
        }
    }

    pub(crate) fn active(&self) -> bool {
        !self.finished
    }

    pub(crate) fn poll(
        &mut self,
        elapsed: Duration,
        provider: &dyn Provider,
    ) -> Result<Option<Event>> {
        if self.finished || elapsed < self.next {
            return Ok(None);
        }
        if elapsed > POLL_TIMEOUT {
            self.finished = true;
            return Ok(None);
        }
        self.next = elapsed.saturating_add(POLL_INTERVAL);
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
                if elapsed == POLL_TIMEOUT {
                    self.finished = true;
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
