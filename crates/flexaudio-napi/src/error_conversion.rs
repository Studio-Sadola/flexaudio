//! Preserve a typed native error when its JavaScript representation cannot be delivered.

use super::*;

pub(super) fn build(
    env: &Env,
    error: NapiError,
    code: Option<&str>,
    decorate: impl FnOnce(&mut JsObject) -> napi::Result<()>,
) -> napi::Result<JsObject> {
    checkpoint("create")?;
    let mut object = env.create_error(error)?;
    checkpoint("properties")?;
    if let Some(code) = code {
        object.set_named_property("code", code)?;
    }
    decorate(&mut object)?;
    Ok(object)
}

pub(super) struct Failure {
    original: flexaudio::Error,
    owner: flexaudio::DropFailureOwner,
    diagnostics: Vec<String>,
}

impl Failure {
    pub(super) fn new(original: flexaudio::Error, owner: flexaudio::DropFailureOwner) -> Self {
        Self {
            original,
            owner,
            diagnostics: Vec::new(),
        }
    }

    fn diagnostic(&mut self, detail: String) {
        if !self.diagnostics.contains(&detail) {
            self.diagnostics.push(detail);
        }
    }

    pub(super) fn napi(&mut self, role: &str, error: &NapiError) {
        // The status is library metadata; an external exception's text is not copied.
        self.diagnostic(format!(
            "Node error conversion {role} failed: {:?}",
            error.status
        ));
    }

    pub(super) fn native(&mut self, role: &str, error: &flexaudio::Error) {
        self.diagnostic(format!("Node error conversion {role} failed: {error}"));
        self.native_details(error);
    }

    fn native_details(&mut self, error: &flexaudio::Error) {
        for detail in error.diagnostic_details() {
            self.diagnostic(detail.into());
        }
        match error {
            flexaudio::Error::Multiple(group) => {
                for error in std::iter::once(group.primary()).chain(group.secondary()) {
                    self.native_details(error);
                }
            }
            flexaudio::Error::Context { source, .. }
            | flexaudio::Error::Diagnostic { source, .. } => self.native_details(source),
            _ => {}
        }
    }

    pub(super) fn finish(self) {
        self.finish_with(|owner, original| {
            flexaudio::core::report_drop_failures(
                owner,
                None,
                &flexaudio::ShutdownReport::new(Some(original), Vec::new()),
            );
        });
    }

    pub(super) fn finish_report(self, report: &FinalReport) {
        self.finish_report_with(report, |owner, original| {
            flexaudio::core::report_drop_failures(
                owner,
                None,
                &flexaudio::ShutdownReport::new(Some(original), Vec::new()),
            );
        });
    }

    fn finish_report_with(
        mut self,
        report: &FinalReport,
        dispatch: impl FnOnce(flexaudio::DropFailureOwner, flexaudio::Error),
    ) {
        if self.diagnostics.is_empty() {
            return;
        }
        fn unreported(
            error: &flexaudio::Error,
            previous: &flexaudio::ShutdownReport,
        ) -> Option<flexaudio::Error> {
            if previous
                .primary()
                .into_iter()
                .chain(previous.cleanup())
                .any(|retained| {
                    flexaudio::core::terminal_causes::retains_terminal_cause(retained, error)
                })
            {
                return None;
            }
            match error {
                flexaudio::Error::Multiple(group) => {
                    let mut retained = None;
                    for error in std::iter::once(group.primary()).chain(group.secondary()) {
                        if let Some(error) = unreported(error, previous) {
                            flexaudio::core::terminal_causes::retain_terminal_cause(
                                &mut retained,
                                error,
                            );
                        }
                    }
                    retained
                }
                _ => Some(error.clone()),
            }
        }
        let newly_reported = {
            let mut report = report.lock().unwrap_or_else(lock_poisoned);
            let retained = report.get_or_insert_with(shutdown::ShutdownReport::default);
            let previous = retained
                .delivered_report
                .clone()
                .unwrap_or_else(|| flexaudio::ShutdownReport::new(None, Vec::new()));
            let newly_reported = unreported(&self.original, &previous);
            if let Some(error) = &newly_reported {
                let mut primary = previous.primary().cloned();
                flexaudio::core::terminal_causes::retain_terminal_cause(
                    &mut primary,
                    error.clone(),
                );
                // Claim only dispatched identities, preserving earlier claims and unread cleanup.
                retained.delivered_report = Some(flexaudio::ShutdownReport::new(
                    primary,
                    previous.cleanup().to_vec(),
                ));
                retained.gc_reported = true;
            }
            newly_reported
        };
        if let Some(original) = newly_reported {
            self.original = original;
            self.finish_with(dispatch);
        }
    }

    fn finish_with(self, report: impl FnOnce(flexaudio::DropFailureOwner, flexaudio::Error)) {
        if !self.diagnostics.is_empty() {
            let mut original = self.original;
            for diagnostic in self.diagnostics {
                original = original.with_diagnostic_detail(diagnostic);
            }
            // Conversion failure adds diagnostics to A; it never becomes another root cause.
            report(self.owner, original);
        }
    }
}

pub(super) fn attempt<T>(
    original: &flexaudio::Error,
    owner: flexaudio::DropFailureOwner,
    role: &str,
    convert: impl FnOnce() -> napi::Result<T>,
) -> napi::Result<T> {
    attempt_with(original, role, convert, |error| {
        let mut failure = Failure::new(original.clone(), owner);
        failure.napi(role, error);
        failure.finish();
    })
}

fn attempt_with<T>(
    _original: &flexaudio::Error,
    _role: &str,
    convert: impl FnOnce() -> napi::Result<T>,
    report: impl FnOnce(&NapiError),
) -> napi::Result<T> {
    let result = convert();
    if let Err(error) = &result {
        report(error);
    }
    result
}

pub(super) fn throw(
    env: &Env,
    original: &flexaudio::Error,
    owner: flexaudio::DropFailureOwner,
    role: &str,
    message: &str,
    build: impl FnOnce() -> napi::Result<JsObject>,
) -> NapiError {
    match attempt(original, owner, role, || {
        let object = build()?;
        checkpoint("throw")?;
        env.throw(object)
    }) {
        Ok(()) => NapiError::new(Status::PendingException, message),
        Err(error) => error,
    }
}

pub(super) fn report_failure(report: &flexaudio::ShutdownReport) -> Option<flexaudio::Error> {
    let mut retained = None;
    for error in report.primary().into_iter().chain(report.cleanup()) {
        flexaudio::core::terminal_causes::retain_terminal_cause(&mut retained, error.clone());
    }
    retained
}

pub(super) fn checkpoint(_stage: &'static str) -> napi::Result<()> {
    #[cfg(all(flexaudio_test_addon, feature = "test-faults"))]
    if FAIL_NEXT.with(|next| next.get() == Some(_stage)) {
        FAIL_NEXT.with(|next| next.set(None));
        return Err(NapiError::new(
            Status::GenericFailure,
            "injected error conversion failure",
        ));
    }
    Ok(())
}

#[cfg(all(flexaudio_test_addon, feature = "test-faults"))]
thread_local! {
    static FAIL_NEXT: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) };
}

#[cfg(all(flexaudio_test_addon, feature = "test-faults"))]
static OBSERVED: Mutex<Vec<flexaudio::DropFailure>> = Mutex::new(Vec::new());

#[cfg(all(flexaudio_test_addon, feature = "test-faults"))]
#[napi(js_name = "__failNextErrorConversion")]
pub fn fail_next_error_conversion(stage: String) -> napi::Result<()> {
    let stage = match stage.as_str() {
        "create" => "create",
        "properties" => "properties",
        "throw" => "throw",
        _ => {
            return Err(NapiError::new(
                Status::InvalidArg,
                "unknown conversion fault stage",
            ))
        }
    };
    OBSERVED.lock().unwrap_or_else(lock_poisoned).clear();
    FAIL_NEXT.with(|next| next.set(Some(stage)));
    flexaudio::set_drop_failure_handler(Some(Box::new(|failure| {
        OBSERVED
            .lock()
            .unwrap_or_else(lock_poisoned)
            .push(failure.clone());
    })));
    Ok(())
}

#[cfg(all(flexaudio_test_addon, feature = "test-faults"))]
#[napi(js_name = "__takeErrorConversionFailures")]
pub fn take_error_conversion_failures() -> Vec<boundary::JsAudioError> {
    FAIL_NEXT.with(|next| next.set(None));
    flexaudio::set_drop_failure_handler(None);
    std::mem::take(&mut *OBSERVED.lock().unwrap_or_else(lock_poisoned))
        .into_iter()
        .filter_map(|failure| report_failure(&failure.report))
        .map(|error| boundary::audio_error(&error))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversion_refusal_keeps_one_original_root_and_returns_the_native_status() {
        for status in [Status::GenericFailure, Status::InvalidArg] {
            let original = flexaudio::Error::DeviceLost;
            let mut observed = Vec::new();
            let result: napi::Result<()> = attempt_with(
                &original,
                "unit projection",
                || Err(NapiError::new(status, "conversion refused")),
                |error| {
                    let mut failure =
                        Failure::new(original.clone(), flexaudio::DropFailureOwner::Stream);
                    failure.napi("unit projection", error);
                    failure.finish_with(|_, error| observed.push(error));
                },
            );
            assert_eq!(result.unwrap_err().status, status);
            assert_eq!(observed.len(), 1);
            assert_eq!(observed[0].root(), &original);
            assert!(!matches!(observed[0].root(), flexaudio::Error::Multiple(_)));
            assert!(observed[0]
                .diagnostic_details()
                .iter()
                .any(|detail| detail.contains(status.as_ref())));
        }
    }

    #[test]
    fn secondary_conversion_diagnostics_survive_group_and_context_wrappers() {
        for raw in [23, 24] {
            let diagnostic = format!("native N-API status code: {raw}");
            let conversion = flexaudio::Error::Multiple(flexaudio::ErrorGroup::new(
                flexaudio::Error::Backend("pending exception".into()),
                flexaudio::Error::Backend("unknown status".into())
                    .with_diagnostic_detail(diagnostic.clone()),
                Vec::new(),
            ))
            .with_context(flexaudio::ErrorContext::new(flexaudio::Operation::Stop));
            let mut failure = Failure::new(
                flexaudio::Error::DeviceLost,
                flexaudio::DropFailureOwner::Stream,
            );
            failure.native("unit rejection", &conversion);
            failure.finish_with(|_, original| {
                assert_eq!(original.root(), &flexaudio::Error::DeviceLost);
                assert!(original.diagnostic_details().contains(&diagnostic.as_str()));
            });
        }
    }

    #[test]
    fn conversion_claims_only_dispatched_causes_and_preserves_previous_claims() {
        let first = flexaudio::Error::DeviceLost;
        let later = flexaudio::Error::Unsupported;
        let unread = flexaudio::Error::DeviceNotFound;
        let report = Arc::new(Mutex::new(Some(shutdown::ShutdownReport {
            binding_cleanup: vec![unread.clone()],
            ..shutdown::ShutdownReport::default()
        })));
        let mut observed = Vec::new();
        let mut send = |original| {
            let mut failure = Failure::new(original, flexaudio::DropFailureOwner::Stream);
            failure.napi(
                "unit projection",
                &NapiError::new(Status::Closing, "refused"),
            );
            failure.finish_report_with(&report, |_, error| observed.push(error));
        };
        send(first.clone());
        send(flexaudio::Error::Multiple(flexaudio::ErrorGroup::new(
            first.clone(),
            later.clone(),
            Vec::new(),
        )));
        send(first.clone());
        {
            let retained = report.lock().unwrap();
            let delivered = retained
                .as_ref()
                .unwrap()
                .delivered_report
                .as_ref()
                .unwrap();
            let primary = delivered.primary().unwrap();
            for error in [&first, &later] {
                assert!(flexaudio::core::terminal_causes::retains_terminal_cause(
                    primary, error
                ));
            }
            assert!(!flexaudio::core::terminal_causes::retains_terminal_cause(
                primary, &unread
            ));
        }
        // A later reporting path must still be able to claim previously unread cleanup.
        send(unread.clone());
        assert_eq!(observed.len(), 3);
        for (actual, expected) in observed.iter().zip([first, later, unread]) {
            assert_eq!(actual.root(), &expected);
        }
    }

    #[test]
    fn successful_conversion_does_not_report_a_failure() {
        let result = attempt_with(
            &flexaudio::Error::DeviceLost,
            "unit projection",
            || Ok(7),
            |_| panic!("successful conversion must stay quiet"),
        );
        assert_eq!(result.unwrap(), 7);
    }
}
