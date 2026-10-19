//! Preserve the facade snapshot when the bridge exits, including during unwinding.

use super::{lock_poisoned, Arc, Mutex};
use flexaudio::{SettingsReport, Stream};
use std::ops::{Deref, DerefMut};

pub(super) struct SettingsOwner {
    stream: Stream,
    final_report: Arc<Mutex<Option<SettingsReport>>>,
}

impl SettingsOwner {
    pub(super) fn new(stream: Stream, final_report: Arc<Mutex<Option<SettingsReport>>>) -> Self {
        Self {
            stream,
            final_report,
        }
    }
}

impl Deref for SettingsOwner {
    type Target = Stream;
    fn deref(&self) -> &Self::Target {
        &self.stream
    }
}

impl DerefMut for SettingsOwner {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.stream
    }
}

impl Drop for SettingsOwner {
    fn drop(&mut self) {
        if let Some(report) = self.stream.effective_settings() {
            *self.final_report.lock().unwrap_or_else(lock_poisoned) = Some(report.clone());
        }
    }
}
