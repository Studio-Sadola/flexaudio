//! One retained outcome for core teardown and attached addon processing.
use super::*;
use flexaudio::core::{ErrorContext, Operation};
use flexaudio_vad::VadTapError;
use napi::bindgen_prelude::Null;

#[derive(Clone)]
pub(super) struct VadFailure {
    pub error: VadTapError,
    pub stage: VadFailureStage,
}

impl VadFailure {
    /// Typed audio-error projection: a backend failure carrying the operation context and the
    /// stable attachment code that names the addon cause. Diagnostics name the cause only, never
    /// PCM, a device name or a secret.
    pub(super) fn audio_error(&self) -> boundary::JsAudioError {
        boundary::audio_error(
            &self
                .error
                .audio_error()
                .with_context(ErrorContext::new(self.stage.operation())),
        )
    }
}

#[derive(Clone, Copy)]
pub(super) enum VadFailureStage {
    Process,
    RuntimeFlush,
    StopFlush,
}

impl VadFailureStage {
    pub fn operation(self) -> Operation {
        match self {
            Self::Process => Operation::Normalize,
            Self::RuntimeFlush | Self::StopFlush => Operation::Flush,
        }
    }
}

#[derive(Default)]
pub(super) struct ShutdownReport {
    pub core: Option<flexaudio::core::ShutdownReport>,
    pub attachment: Vec<VadFailure>,
    pub binding_cleanup: Vec<flexaudio::Error>,
}

impl ShutdownReport {
    pub fn from_core(core: Option<flexaudio::core::ShutdownReport>) -> Self {
        Self {
            core,
            attachment: Vec::new(),
            binding_cleanup: Vec::new(),
        }
    }

    /// One shared outcome contract decides both retained diagnostics and checked stop.
    fn outcome(&self) -> flexaudio::core::ShutdownReport {
        let primary = self
            .core
            .as_ref()
            .and_then(|report| report.primary())
            .cloned();
        let mut cleanup = self
            .core
            .as_ref()
            .map(|report| report.cleanup().to_vec())
            .unwrap_or_default();
        cleanup.extend(self.attachment.iter().map(|failure| {
            failure
                .error
                .audio_error()
                .with_context(ErrorContext::new(failure.stage.operation()))
        }));
        cleanup.extend(self.binding_cleanup.iter().cloned());
        flexaudio::core::ShutdownReport::new(primary, cleanup)
    }

    pub fn to_js(&self) -> boundary::JsShutdownReport {
        let report = self.outcome();
        boundary::JsShutdownReport {
            primary: report
                .primary()
                .map(boundary::audio_error)
                .map(Either::A)
                .unwrap_or(Either::B(Null)),
            cleanup_errors: report.cleanup().iter().map(boundary::audio_error).collect(),
        }
    }

    pub fn exception(&self, env: &Env) -> napi::Result<Option<JsObject>> {
        match self.outcome().result() {
            Ok(()) => Ok(None),
            Err(error) => boundary::js_error(env, &error).map(Some),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attachment_failure() -> VadFailure {
        VadFailure {
            error: VadTapError::Vad(flexaudio_vad::VadError::Inference),
            stage: VadFailureStage::Process,
        }
    }

    #[test]
    fn attached_vad_failure_is_cleanup_only_and_keeps_its_typed_code() {
        let report = ShutdownReport {
            core: None,
            attachment: vec![attachment_failure()],
            binding_cleanup: Vec::new(),
        };
        let js = report.to_js();
        assert!(
            matches!(js.primary, Either::B(Null)),
            "an attached VAD-only failure must not be the capture primary"
        );
        assert_eq!(js.cleanup_errors.len(), 1);
        assert_eq!(js.cleanup_errors[0].kind, "backend");
        assert_eq!(js.cleanup_errors[0].vad_code.as_deref(), Some("Inference"));
        assert_eq!(js.cleanup_errors[0].contexts[0].operation, "normalize");
    }

    #[test]
    fn capture_primary_keeps_precedence_over_an_attached_vad_failure() {
        let report = ShutdownReport {
            core: Some(flexaudio::core::ShutdownReport::new(
                Some(flexaudio::Error::DeviceLost),
                Vec::new(),
            )),
            attachment: vec![attachment_failure()],
            binding_cleanup: Vec::new(),
        };
        let js = report.to_js();
        let Either::A(primary) = js.primary else {
            panic!("a real capture failure remains the primary")
        };
        assert_eq!(primary.kind, "deviceLost");
        assert_eq!(js.cleanup_errors.len(), 1);
        assert_eq!(js.cleanup_errors[0].vad_code.as_deref(), Some("Inference"));
    }

    #[test]
    fn a_stop_flush_vad_failure_is_cleanup_and_keeps_the_flush_context() {
        let report = ShutdownReport {
            core: None,
            attachment: vec![VadFailure {
                error: VadTapError::Conversion,
                stage: VadFailureStage::StopFlush,
            }],
            binding_cleanup: Vec::new(),
        };
        let js = report.to_js();
        assert!(matches!(js.primary, Either::B(Null)));
        assert_eq!(js.cleanup_errors.len(), 1);
        assert_eq!(js.cleanup_errors[0].vad_code.as_deref(), Some("Conversion"));
        assert_eq!(js.cleanup_errors[0].contexts[0].operation, "flush");
    }
    #[test]
    fn attachment_only_stop_succeeds_and_binding_cleanup_keeps_the_rejecting_root() {
        let mut report = ShutdownReport {
            core: None,
            attachment: vec![attachment_failure()],
            binding_cleanup: Vec::new(),
        };
        assert!(report.outcome().result().is_ok());
        report.binding_cleanup.push(flexaudio::Error::Backend(
            "the bridge could not join because its worker failed".into(),
        ));
        let error = report
            .outcome()
            .result()
            .expect_err("a bridge failure must still reject stop");
        let mapped = boundary::audio_error(&error);
        assert!(mapped.vad_code.is_none());
        assert_eq!(mapped.secondary.len(), 1);
        assert_eq!(mapped.secondary[0].vad_code.as_deref(), Some("Inference"));
    }
}
