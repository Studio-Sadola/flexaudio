// Native capture errors keep daemon context out of the public display message.
use flexaudio_core::{Error, ErrorContext, Operation};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CoreErrorDisposition {
    ReleasedProxy,
    VanishedLink,
    Fatal,
}

pub(crate) fn classify_core_error(
    object_id: u32,
    status: i32,
    message: &str,
    was_released: impl FnOnce(u32) -> bool,
) -> CoreErrorDisposition {
    // PipeWire reports a redundant destroy on the core, not the destroyed proxy.
    // There is no structured target/operation in this event, so accept only the
    // exact native destroy diagnostic, corroborated by this connection's owner.
    if object_id == 0 && status == -2 {
        if let Some(id) = destroy_target(message) {
            if was_released(id) {
                return CoreErrorDisposition::ReleasedProxy;
            }
        }
    }
    CoreErrorDisposition::Fatal
}

pub(crate) fn destroy_target(message: &str) -> Option<u32> {
    message
        .strip_prefix("unknown resource ")
        .and_then(|message| message.strip_suffix(" op:7"))
        .filter(|id| {
            !id.is_empty() && !id.starts_with('0') && id.bytes().all(|byte| byte.is_ascii_digit())
        })
        .and_then(|id| id.parse::<u32>().ok())
        .filter(|id| *id != 0 && *id != u32::MAX)
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum CapturePhase {
    Setup,
    Running,
}

pub(crate) fn core_error(
    object_id: u32,
    sequence: i32,
    status: i32,
    message: &str,
    phase: CapturePhase,
    disposition: CoreErrorDisposition,
) -> Error {
    let (guidance, operation) = match (disposition, phase) {
        (CoreErrorDisposition::ReleasedProxy, _) => (
            "PipeWire confirmed an already released capture source proxy; capture continues.",
            Operation::Link,
        ),
        (CoreErrorDisposition::VanishedLink, _) => (
            "PipeWire capture source disappeared during linking; capture continues watching for playback.",
            Operation::Link,
        ),
        (CoreErrorDisposition::Fatal, CapturePhase::Setup) => (
            "PipeWire rejected capture setup; check audio-session access, restart the audio session and retry capture.",
            Operation::Start,
        ),
        (CoreErrorDisposition::Fatal, CapturePhase::Running) => (
            "PipeWire capture failed; check audio-session access, restart the audio session and retry capture.",
            Operation::Link,
        ),
    };
    Error::Backend(guidance.into())
        .with_context(ErrorContext::new(operation))
        .with_diagnostic_detail(format!(
            "PipeWire core error: id={object_id} sequence={sequence} status={status}; {message}"
        ))
}

#[cfg(test)]
mod tests {
    use super::{classify_core_error, CapturePhase, CoreErrorDisposition};

    #[test]
    fn destroy_error_for_a_released_proxy_does_not_abort_capture() {
        assert_eq!(
            classify_core_error(0, -2, "unknown resource 5 op:7", |id| id == 5),
            CoreErrorDisposition::ReleasedProxy
        );
    }

    #[test]
    fn unrelated_missing_resources_and_setup_failures_remain_fatal() {
        for (object_id, status, message) in [
            (0, -2, "unknown resource 3 op:7"),
            (5, -2, "unknown resource 5 op:7"),
            (0, -13, "unknown resource 5 op:7"),
            (0, -1, "unknown resource 5 op:7"),
            (0, -32, "unknown resource 5 op:7"),
            (0, -2, "unknown factory name link-factory"),
            (0, -2, "no global 5"),
            (0, -2, "unknown resource 5 op:6"),
            (0, -2, "unknown resource 5 op:7 trailing text"),
            (0, -2, "unknown resource +5 op:7"),
            (0, -2, "unknown resource 05 op:7"),
            (0, -2, "unknown resource  5 op:7"),
            (0, -2, "unknown resource 4294967296 op:7"),
            (0, -2, "unknown resource 4294967295 op:7"),
            (0, -2, "unknown resource 0 op:7"),
            (0, -2, "unknown resource  op:7"),
        ] {
            assert_eq!(
                classify_core_error(object_id, status, message, |id| id == 5),
                CoreErrorDisposition::Fatal,
                "{object_id} {status} {message}"
            );
        }
        assert_eq!(
            classify_core_error(0, -2, "unknown resource 5 op:7", |_| false),
            CoreErrorDisposition::Fatal
        );
    }

    #[test]
    fn core_error_retains_native_cause_without_exposing_it_in_display() {
        let error = super::core_error(
            0,
            37,
            -2,
            "synthetic private daemon context",
            CapturePhase::Setup,
            CoreErrorDisposition::Fatal,
        );
        assert_eq!(
            error.diagnostic_details(),
            ["PipeWire core error: id=0 sequence=37 status=-2; synthetic private daemon context"]
        );
        assert_eq!(error.kind(), flexaudio_core::ErrorKind::Backend);
        assert!(!error.to_string().contains("synthetic private"));
        assert!(!error.to_string().contains("sequence=37"));
    }

    #[test]
    fn runtime_and_recoverable_errors_have_accurate_guidance_and_diagnostics() {
        for (disposition, expected) in [
            (CoreErrorDisposition::Fatal, "PipeWire capture failed"),
            (CoreErrorDisposition::ReleasedProxy, "capture continues"),
        ] {
            let error = super::core_error(
                0,
                45,
                -2,
                "unknown resource 5 op:7",
                CapturePhase::Running,
                disposition,
            );
            assert!(error.to_string().contains(expected));
            assert!(!error.to_string().contains("capture setup"));
            assert_eq!(
                error.diagnostic_details(),
                ["PipeWire core error: id=0 sequence=45 status=-2; unknown resource 5 op:7"]
            );
        }
    }
}
