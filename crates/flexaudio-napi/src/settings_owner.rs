//! Preserve the facade snapshot when the bridge exits, including during unwinding.

use super::{lock_poisoned, shutdown, Arc, FinalReport, Mutex};
use flexaudio::{SettingsReport, Stream};
use std::ops::{Deref, DerefMut};

pub(super) struct SettingsOwner {
    stream: Stream,
    final_report: Arc<Mutex<Option<SettingsReport>>>,
    shutdown: FinalReport,
}

impl SettingsOwner {
    pub(super) fn new(
        stream: Stream,
        final_report: Arc<Mutex<Option<SettingsReport>>>,
        shutdown: FinalReport,
    ) -> Self {
        Self {
            stream,
            final_report,
            shutdown,
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
        self.stream.stop();
        {
            let mut report = self.shutdown.lock().unwrap_or_else(lock_poisoned);
            let report = report.get_or_insert_with(shutdown::ShutdownReport::default);
            if let Some(core) = self.stream.shutdown_report() {
                report.retain_core(core);
            }
        }
        if let Some(report) = self.stream.effective_settings() {
            *self.final_report.lock().unwrap_or_else(lock_poisoned) = Some(report.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_reconciles_shutdown_even_when_a_core_report_already_exists() {
        let first = flexaudio::Error::Backend("previous bridge snapshot".into());
        let report: FinalReport =
            Arc::new(Mutex::new(Some(shutdown::ShutdownReport::from_core(Some(
                flexaudio::ShutdownReport::new(Some(first.clone()), Vec::new()),
            )))));
        let mut stream =
            flexaudio::test_support::stop_failure(flexaudio::test_support::StopPhase::Stop)
                .unwrap();
        stream.stop();
        let second = stream.shutdown_report().unwrap().cleanup()[0].clone();
        drop(SettingsOwner::new(
            stream,
            Arc::new(Mutex::new(None)),
            report.clone(),
        ));
        let report = report.lock().unwrap();
        let core = report.as_ref().unwrap().core.as_ref().unwrap();
        assert_eq!(core.primary(), Some(&first));
        assert_eq!(core.cleanup(), std::slice::from_ref(&second));
        let failure = core.result().unwrap_err();
        assert!(flexaudio::core::terminal_causes::retains_terminal_cause(
            &failure, &first
        ));
        assert!(flexaudio::core::terminal_causes::retains_terminal_cause(
            &failure, &second
        ));
    }

    #[test]
    fn owner_unwind_retains_real_backend_stop_failure() {
        let report: FinalReport = Arc::new(Mutex::new(None));
        let retained = report.clone();
        let result = std::panic::catch_unwind(move || {
            let stream =
                flexaudio::test_support::stop_failure(flexaudio::test_support::StopPhase::Stop)
                    .unwrap();
            let _owner = SettingsOwner::new(stream, Arc::new(Mutex::new(None)), retained);
            panic!("injected owner unwind");
        });
        assert!(result.is_err());
        let report = report.lock().unwrap();
        let core = report.as_ref().unwrap().core.as_ref().unwrap();
        assert!(core.primary().is_none());
        assert_eq!(core.cleanup().len(), 1);
        assert_eq!(core.cleanup()[0].kind(), flexaudio::ErrorKind::DeviceLost);
        assert!(core.result().is_err());
    }
}
