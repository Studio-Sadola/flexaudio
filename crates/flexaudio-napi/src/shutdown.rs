//! One retained outcome for core teardown and attached addon processing.
use super::*;
use flexaudio::core::{ErrorContext, Operation};
use flexaudio_vad::WhisperVadTapError;
use napi::bindgen_prelude::Null;

#[derive(Clone)]
pub(super) struct WhisperFailure {
    pub error: WhisperVadTapError,
    pub stage: WhisperFailureStage,
}

#[derive(Clone, Copy)]
pub(super) enum WhisperFailureStage {
    Process,
    RuntimeFlush,
    StopFlush,
}

impl WhisperFailureStage {
    pub fn operation(self) -> Operation {
        match self {
            Self::Process => Operation::Normalize,
            Self::RuntimeFlush | Self::StopFlush => Operation::Flush,
        }
    }

    pub fn is_primary(self) -> bool {
        matches!(self, Self::Process | Self::RuntimeFlush)
    }
}

#[derive(Default)]
pub(super) struct ShutdownReport {
    pub core: Option<flexaudio::core::ShutdownReport>,
    pub whisper: Option<WhisperFailure>,
    pub binding_cleanup: Vec<flexaudio::Error>,
}

impl ShutdownReport {
    pub fn from_core(core: Option<flexaudio::core::ShutdownReport>) -> Self {
        Self {
            core,
            whisper: None,
            binding_cleanup: Vec::new(),
        }
    }

    fn whisper_error(&self) -> Option<boundary::JsAudioError> {
        self.whisper.as_ref().map(|failure| {
            let error = flexaudio::Error::Backend("attached whisper VAD failure".into())
                .with_context(ErrorContext::new(failure.stage.operation()));
            let mut mapped = boundary::audio_error(&error);
            mapped.message = failure.error.to_string();
            mapped.whisper_code = Some(whisper_integration::error_code(&failure.error).into());
            mapped
        })
    }

    pub fn to_js(&self) -> boundary::JsShutdownReport {
        let mut primary = self
            .core
            .as_ref()
            .and_then(|report| report.primary())
            .map(boundary::audio_error);
        let mut cleanup_errors = self
            .core
            .as_ref()
            .map(|report| {
                report
                    .cleanup()
                    .iter()
                    .map(boundary::audio_error)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if let Some(error) = self.whisper_error() {
            if self
                .whisper
                .as_ref()
                .is_some_and(|failure| failure.stage.is_primary())
            {
                // Processing failed during capture. Keep any core primary and retain the
                // addon as a related primary; teardown errors remain in cleanupErrors.
                if let Some(primary) = primary.as_mut() {
                    primary.secondary.push(error);
                } else {
                    primary = Some(error);
                }
            } else {
                // A final addon flush is cleanup, even when core capture was clean.
                cleanup_errors.push(error);
            }
        }
        cleanup_errors.extend(self.binding_cleanup.iter().map(boundary::audio_error));
        boundary::JsShutdownReport {
            primary: primary.map(Either::A).unwrap_or(Either::B(Null)),
            cleanup_errors,
        }
    }

    pub fn exception(&self, env: &Env) -> napi::Result<Option<JsObject>> {
        let core_primary = self.core.as_ref().and_then(|report| report.primary());
        let whisper_primary = self
            .whisper
            .as_ref()
            .filter(|failure| failure.stage.is_primary());
        let core_cleanup = self
            .core
            .as_ref()
            .and_then(|report| report.cleanup().first());
        // Match the report's primary/cleanup order, keeping the existing Whisper code
        // when the addon is the root cause. Every cause is also in audioError.
        let mut exception = if let Some(error) = core_primary {
            boundary::js_error(env, error)?
        } else if let Some(failure) = whisper_primary {
            whisper_integration::js_error(env, &failure.error)?
        } else if let Some(error) = core_cleanup {
            boundary::js_error(env, error)?
        } else if let Some(failure) = self.whisper.as_ref() {
            whisper_integration::js_error(env, &failure.error)?
        } else if let Some(error) = self.binding_cleanup.first() {
            boundary::js_error(env, error)?
        } else {
            return Ok(None);
        };
        let report = self.to_js();
        let mut causes = match report.primary {
            Either::A(primary) => std::iter::once(primary)
                .chain(report.cleanup_errors)
                .collect::<Vec<_>>(),
            Either::B(_) => report.cleanup_errors,
        }
        .into_iter();
        let mut root = causes
            .next()
            .expect("exception requires a retained failure");
        root.secondary.extend(causes);
        exception.set_named_property("audioError", root)?;
        Ok(Some(exception))
    }
}
