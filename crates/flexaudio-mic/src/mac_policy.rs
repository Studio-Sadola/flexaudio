//! Device-free microphone consent policy and single-flight prompt coordination.

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::permission::Purpose;
use flexaudio_core::types::{Error, Permission, Result};

pub(crate) const PROMPT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
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

fn unanswered_prompt() -> Error {
    Error::Backend("The microphone consent prompt is unanswered, so capture cannot safely query the input format. Answer the responsible app's prompt or grant access in System Settings > Privacy & Security > Microphone, then retry opening the stream. Consent waits at most 30 seconds.".into())
}

fn missing_usage_description() -> Error {
    Error::Backend("Microphone consent is undecided and the main app bundle has no nonempty NSMicrophoneUsageDescription, so capture cannot safely request consent or query the input format. Launch from an app bundle with that usage description, or grant Microphone access to the responsible host app in System Settings > Privacy & Security > Microphone, then retry opening the stream.".into())
}

fn undecided_enumeration() -> Error {
    Error::Backend("Microphone consent is undecided, so device enumeration cannot safely query the mandatory native input format. Enumeration does not request access. Open a microphone stream from a configured app bundle to answer its consent prompt, or grant the responsible host access in System Settings > Privacy & Security > Microphone, then retry device enumeration.".into())
}

#[derive(Default)]
struct PromptState {
    result: Option<Result<bool>>,
    wait_expired: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
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

/// Require decided consent before querying HAL; unanswered requests fail closed.
pub(crate) fn preflight(
    purpose: Purpose,
    provider: &dyn Provider,
    coordinator: &PromptCoordinator,
    timeout: Duration,
) -> Result<()> {
    match provider.status()? {
        Status::Authorized => Ok(()),
        Status::Denied => Err(denied("macOS denied microphone access")),
        Status::Restricted => Err(denied("macOS restricted microphone access")),
        Status::NotDetermined => {
            if purpose == Purpose::Enumeration {
                return Err(undecided_enumeration());
            }
            if !provider.has_usage_description()? {
                return Err(missing_usage_description());
            }
            if coordinator.request(provider, timeout)? == Decision::Refused {
                return Err(denied("The microphone consent prompt was refused"));
            }
            match provider.status()? {
                Status::Authorized => Ok(()),
                Status::Denied => Err(denied("macOS denied microphone access after the prompt")),
                Status::Restricted => Err(denied(
                    "macOS restricted microphone access after the prompt",
                )),
                Status::NotDetermined => Err(unanswered_prompt()),
            }
        }
    }
}

#[cfg(test)]
mod tests;
