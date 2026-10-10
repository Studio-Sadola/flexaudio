//! Typed domain projections at the Node boundary. Messages never classify failures.
use super::*;
use flexaudio::core::{
    AudioLoss, AudioPath, Error, ErrorContext, ErrorKind, LossReason, MixLane, NativeStatus,
    Operation, OutputTap,
};
use napi::bindgen_prelude::Null;

pub(super) fn error_code(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::InvalidArg => "FLEX_INVALID_ARG",
        ErrorKind::InvalidState => "FLEX_INVALID_STATE",
        ErrorKind::DeviceNotFound => "FLEX_DEVICE_NOT_FOUND",
        ErrorKind::PermissionDenied => "FLEX_PERMISSION_DENIED",
        ErrorKind::UnsupportedOsVersion => "FLEX_UNSUPPORTED_OS_VERSION",
        ErrorKind::DeviceLost => "FLEX_DEVICE_LOST",
        ErrorKind::Backend => "FLEX_FAILURE",
        ErrorKind::UnsupportedFormat => "FLEX_UNSUPPORTED_FORMAT",
        ErrorKind::NativeFormatChanged => "FLEX_NATIVE_FORMAT_CHANGED",
        ErrorKind::Unsupported => "FLEX_UNSUPPORTED",
        ErrorKind::AmbiguousDeviceName => "FLEX_AMBIGUOUS_DEVICE_NAME",
        _ => "FLEX_FAILURE",
    }
}

pub(super) fn to_napi_err(error: Error) -> NapiError {
    let status = match error.kind() {
        ErrorKind::InvalidArg | ErrorKind::UnsupportedFormat => Status::InvalidArg,
        _ => Status::GenericFailure,
    };
    NapiError::new(status, error.to_string())
}

pub(super) fn js_error(env: &Env, error: &Error) -> napi::Result<JsObject> {
    let mut object = env.create_error(to_napi_err(error.clone()))?;
    object.set_named_property("code", error_code(error.kind()))?;
    object.set_named_property("audioError", audio_error(error))?;
    Ok(object)
}

pub(super) fn throw_error(env: &Env, error: Error) -> NapiError {
    match js_error(env, &error).and_then(|exception| env.throw(exception)) {
        Ok(()) => NapiError::new(Status::PendingException, error.to_string()),
        Err(error) => error,
    }
}

/// AsyncTask rejects with the referenced exception; leaving a pending exception in
/// Task::resolve would prevent napi-rs from settling its promise.
pub(super) fn async_error(env: &Env, error: Error) -> NapiError {
    match js_error(env, &error) {
        Ok(exception) => {
            let mut mapped = NapiError::from(exception.into_unknown());
            mapped.status = to_napi_err(error).status;
            mapped
        }
        Err(error) => error,
    }
}

#[napi(object)]
pub struct JsErrorContext {
    pub operation: String,
    pub lane: Either<String, Null>,
    pub native_status: Either<JsNativeStatus, Null>,
}

#[napi(object)]
pub struct JsNativeStatus {
    #[napi(js_name = "type")]
    pub kind: String,
    pub call: String,
    pub bits: Option<u32>,
    pub value: Option<i32>,
}

#[napi(object)]
pub struct JsAudioError {
    pub kind: String,
    pub message: String,
    pub contexts: Vec<JsErrorContext>,
    pub secondary: Vec<JsAudioError>,
    pub permission: Option<String>,
    pub advertised: Option<JsNativeFormat>,
    pub actual: Option<JsNativeFormat>,
}

#[napi(object)]
pub struct JsShutdownReport {
    pub primary: Either<JsAudioError, Null>,
    pub cleanup_errors: Vec<JsAudioError>,
}

pub(super) fn shutdown_report(report: &flexaudio::core::ShutdownReport) -> JsShutdownReport {
    JsShutdownReport {
        primary: report
            .primary()
            .map(|error| Either::A(audio_error(error)))
            .unwrap_or(Either::B(Null)),
        cleanup_errors: report.cleanup().iter().map(audio_error).collect(),
    }
}

fn lane_name(lane: MixLane) -> Option<&'static str> {
    match lane {
        MixLane::Microphone => Some("microphone"),
        MixLane::SystemAudio => Some("systemAudio"),
        _ => None,
    }
}

fn context_to_js(context: ErrorContext) -> JsErrorContext {
    let operation = match context.operation() {
        Operation::Enumerate => "enumerate",
        Operation::Start => "start",
        Operation::Normalize => "normalize",
        Operation::Flush => "flush",
        Operation::Reopen => "reopen",
        Operation::Rollback => "rollback",
        Operation::Stop => "stop",
        Operation::Join => "join",
        Operation::Link => "link",
        _ => "unknown",
    };
    let lane = context
        .lane()
        .and_then(lane_name)
        .map(|name| Either::A(name.into()))
        .unwrap_or(Either::B(Null));
    let native_status = match context.native_status() {
        Some(NativeStatus::HResult { call, bits }) => Either::A(JsNativeStatus {
            kind: "hresult".into(),
            call: call.into(),
            bits: Some(bits),
            value: None,
        }),
        Some(NativeStatus::OsStatus { call, value }) => Either::A(JsNativeStatus {
            kind: "osStatus".into(),
            call: call.into(),
            bits: None,
            value: Some(value),
        }),
        _ => Either::B(Null),
    };
    JsErrorContext {
        operation: operation.into(),
        lane,
        native_status,
    }
}

pub(super) fn audio_error(error: &Error) -> JsAudioError {
    let mut contexts = Vec::new();
    let mut secondary = Vec::new();
    let mut root = error;
    loop {
        match root {
            Error::Context { source, context } => {
                contexts.push(context_to_js(*context));
                root = source;
            }
            Error::Multiple(group) => {
                // The primary's already-related failures precede the outer group's
                // later cleanup failures. Related children retain their own trees.
                secondary.splice(0..0, group.secondary().map(audio_error));
                root = group.primary();
            }
            _ => break,
        }
    }
    let kind = match root {
        Error::InvalidArg(_) => "invalidArg",
        Error::InvalidState(_) => "invalidState",
        Error::DeviceNotFound => "deviceNotFound",
        Error::PermissionDenied { .. } => "permissionDenied",
        Error::UnsupportedOsVersion => "unsupportedOsVersion",
        Error::DeviceLost => "deviceLost",
        Error::Backend(_) => "backend",
        Error::UnsupportedFormat(_) => "unsupportedFormat",
        Error::NativeFormatChanged { .. } => "nativeFormatChanged",
        Error::Unsupported => "unsupported",
        Error::AmbiguousDeviceName => "ambiguousDeviceName",
        _ => "unknown",
    };
    let (advertised, actual) = match root {
        Error::NativeFormatChanged { advertised, actual } => (
            Some(JsNativeFormat {
                sample_rate: advertised.0,
                channels: advertised.1,
            }),
            Some(JsNativeFormat {
                sample_rate: actual.0,
                channels: actual.1,
            }),
        ),
        _ => (None, None),
    };
    JsAudioError {
        kind: kind.into(),
        message: error.to_string(),
        contexts,
        secondary,
        permission: error
            .permission()
            .map(|permission| permission.as_str().into()),
        advertised,
        actual,
    }
}

#[napi(object)]
pub struct JsAudioPath {
    #[napi(js_name = "type")]
    pub kind: String,
    pub lane: Option<Either<String, Null>>,
    pub tap: Option<String>,
}

#[napi(object)]
pub struct JsAudioLoss {
    pub path: JsAudioPath,
    pub reason: String,
    pub samples: Either<BigInt, Null>,
    pub sample_rate: u32,
    pub channels: u16,
}

fn loss_to_js(loss: AudioLoss) -> Option<JsAudioLoss> {
    let path = match loss.path() {
        AudioPath::Capture { lane } => JsAudioPath {
            kind: "capture".into(),
            lane: Some(match lane {
                Some(lane) => Either::A(lane_name(lane)?.into()),
                None => Either::B(Null),
            }),
            tap: None,
        },
        AudioPath::MixFifo { lane } => JsAudioPath {
            kind: "mixFifo".into(),
            lane: Some(Either::A(lane_name(lane)?.into())),
            tap: None,
        },
        AudioPath::Output { tap } => JsAudioPath {
            kind: "output".into(),
            lane: None,
            tap: Some(
                match tap {
                    OutputTap::Primary => "primary",
                    OutputTap::Secondary => "secondary",
                    _ => return None,
                }
                .into(),
            ),
        },
        _ => return None,
    };
    let reason = match loss.reason() {
        LossReason::RawOverflow => "rawOverflow",
        LossReason::MixFifoOverflow => "mixFifoOverflow",
        LossReason::CorruptBuffer => "corruptBuffer",
        LossReason::MalformedBuffer => "malformedBuffer",
        LossReason::CallbackRejected => "callbackRejected",
        LossReason::OutputOverflow => "outputOverflow",
        _ => return None,
    };
    Some(JsAudioLoss {
        path,
        reason: reason.into(),
        samples: loss
            .samples()
            .map(|count| Either::A(BigInt::from(count.get())))
            .unwrap_or(Either::B(Null)),
        sample_rate: loss.sample_rate(),
        channels: loss.channels(),
    })
}

pub(super) fn event_to_js(event: Event) -> JsStreamEvent {
    let mut result = JsStreamEvent {
        kind: String::new(),
        permission: None,
        count: None,
        message: None,
        error: None,
        loss: None,
    };
    result.kind = match event {
        Event::ChunkDropped { count } => {
            result.count = Some(BigInt::from(count));
            "chunkDropped"
        }
        Event::StreamStalled => "stalled",
        Event::StreamRecovered => "recovered",
        Event::DeviceLost => "deviceLost",
        Event::PermissionPending { permission, detail } => {
            result.permission = Some(permission.as_str().into());
            result.message = Some(detail);
            "permissionPending"
        }
        Event::PermissionDenied { permission, detail } => {
            result.permission = Some(permission.as_str().into());
            result.message = Some(Error::PermissionDenied { permission, detail }.to_string());
            "permissionDenied"
        }
        Event::PermissionGranted => {
            result.permission = Some("microphone".into());
            "permissionGranted"
        }
        Event::SilenceWhileSourceActive { detail } => {
            result.message = Some(detail);
            "silenceWhileSourceActive"
        }
        Event::Error(_) => {
            result.message = Some("legacy capture failure".into());
            "error"
        }
        Event::TerminalError { error } => {
            result.error = Some(audio_error(&error));
            "terminalError"
        }
        Event::RecoverableError { error } => {
            result.error = Some(audio_error(&error));
            "recoverableError"
        }
        Event::ShutdownError { error } => {
            result.error = Some(audio_error(&error));
            "shutdownError"
        }
        Event::AudioLoss { loss } => match loss_to_js(loss) {
            Some(loss) => {
                result.loss = Some(loss);
                "audioLoss"
            }
            None => {
                result.message = Some("unknown audio loss diagnostic".into());
                "unknown"
            }
        },
        Event::Clipped => "clipped",
        _ => {
            result.message = Some("unknown stream event diagnostic".into());
            "unknown"
        }
    }
    .into();
    result
}

pub(super) fn device_event_to_js(event: DeviceEvent) -> JsDeviceEvent {
    let mut result = JsDeviceEvent {
        kind: String::new(),
        device: None,
        id: None,
        source_kind: None,
        dropped_events: None,
        message: None,
    };
    result.kind = match event {
        DeviceEvent::Added(info) => {
            result.device = Some(device_info_to_js(info));
            "added"
        }
        DeviceEvent::Removed { id } => {
            result.id = Some(id);
            "removed"
        }
        DeviceEvent::DefaultChanged { kind, id } => {
            result.source_kind = Some(source_kind_str(kind.into()));
            result.id = Some(id);
            "defaultChanged"
        }
        DeviceEvent::DefaultCleared { kind } => {
            result.source_kind = Some(source_kind_str(kind.into()));
            "defaultCleared"
        }
        DeviceEvent::RescanRequired { dropped_events } => {
            result.dropped_events = Some(BigInt::from(dropped_events));
            "rescanRequired"
        }
        _ => {
            result.message = Some("unknown device event diagnostic".into());
            "unknown"
        }
    }
    .into();
    result
}

#[cfg(test)]
pub(super) fn fixture_error(kind: &str) -> Error {
    match kind {
        "invalidArg" => Error::InvalidArg("invalid rate".into()),
        "invalidState" => Error::InvalidState("stopped".into()),
        "deviceNotFound" => Error::DeviceNotFound,
        "permissionDenied" => Error::PermissionDenied {
            permission: flexaudio::Permission::Microphone,
            detail: "private detail".into(),
        },
        "unsupportedOsVersion" => Error::UnsupportedOsVersion,
        "deviceLost" => Error::DeviceLost,
        "backend" => Error::Backend("control cause".into()),
        "unsupportedFormat" => Error::UnsupportedFormat("unsupported rate".into()),
        "nativeFormatChanged" => Error::NativeFormatChanged {
            advertised: (48_000, 2),
            actual: (44_100, 1),
        },
        "unsupported" => Error::Unsupported,
        "ambiguousDeviceName" => Error::AmbiguousDeviceName,
        "wrapped" => Error::Multiple(flexaudio::core::ErrorGroup::new(
            fixture_error("permissionDenied").with_context(
                ErrorContext::new(Operation::Start)
                    .with_lane(MixLane::Microphone)
                    .with_native_status(NativeStatus::HResult {
                        call: "fixture_call",
                        bits: 0x8007_0005,
                    }),
            ),
            Error::Multiple(flexaudio::core::ErrorGroup::new(
                fixture_error("backend"),
                Error::DeviceLost,
                Vec::new(),
            ))
            .with_context(
                ErrorContext::new(Operation::Join)
                    .with_lane(MixLane::SystemAudio)
                    .with_native_status(NativeStatus::OsStatus {
                        call: "fixture_cleanup",
                        value: -50,
                    }),
            ),
            Vec::new(),
        ))
        .with_context(ErrorContext::new(Operation::Stop)),
        _ => Error::InvalidArg("unknown fixture kind".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_root_error_has_a_distinct_stable_code_and_payload() {
        let kinds = [
            "invalidArg",
            "invalidState",
            "deviceNotFound",
            "permissionDenied",
            "unsupportedOsVersion",
            "deviceLost",
            "backend",
            "unsupportedFormat",
            "nativeFormatChanged",
            "unsupported",
            "ambiguousDeviceName",
        ];
        let mut codes = std::collections::HashSet::new();
        for kind in kinds {
            let error = fixture_error(kind);
            assert!(codes.insert(error_code(error.kind())));
            let projected = audio_error(&error);
            assert_eq!(projected.kind, kind);
            assert_eq!(projected.message, error.to_string());
            assert!(projected.contexts.is_empty());
            assert!(projected.secondary.is_empty());
        }
    }

    #[test]
    fn nested_error_projection_preserves_primary_contexts_and_related_trees() {
        let error = fixture_error("wrapped");
        assert_eq!(error_code(error.kind()), "FLEX_PERMISSION_DENIED");
        let projected = audio_error(&error);
        assert_eq!(projected.kind, "permissionDenied");
        assert_eq!(projected.permission.as_deref(), Some("microphone"));
        assert_eq!(
            projected
                .contexts
                .iter()
                .map(|context| context.operation.as_str())
                .collect::<Vec<_>>(),
            ["stop", "start"]
        );
        assert_eq!(projected.secondary.len(), 1);
        assert_eq!(projected.secondary[0].kind, "backend");
        assert_eq!(projected.secondary[0].contexts[0].operation, "join");
        assert_eq!(projected.secondary[0].secondary[0].kind, "deviceLost");
        assert!(!projected.message.contains("private detail"));
        assert!(!projected.message.contains("fixture_call"));
    }

    #[test]
    fn nested_primary_related_errors_keep_observation_order() {
        let primary = Error::Multiple(flexaudio::core::ErrorGroup::new(
            Error::DeviceLost,
            Error::DeviceNotFound,
            Vec::new(),
        ))
        .with_context(ErrorContext::new(Operation::Normalize));
        let error = Error::Multiple(flexaudio::core::ErrorGroup::new(
            primary,
            Error::Unsupported,
            Vec::new(),
        ))
        .with_context(ErrorContext::new(Operation::Stop));
        let projected = audio_error(&error);
        assert_eq!(
            projected
                .contexts
                .iter()
                .map(|context| context.operation.as_str())
                .collect::<Vec<_>>(),
            ["stop", "normalize"]
        );
        assert_eq!(
            projected
                .secondary
                .iter()
                .map(|error| error.kind.as_str())
                .collect::<Vec<_>>(),
            ["deviceNotFound", "unsupported"]
        );
    }

    #[test]
    fn typed_events_and_defaults_never_fall_back_to_unknown() {
        for (event, expected) in [
            (
                Event::RecoverableError {
                    error: Error::DeviceLost,
                },
                "recoverableError",
            ),
            (
                Event::ShutdownError {
                    error: Error::DeviceLost,
                },
                "shutdownError",
            ),
            (
                Event::TerminalError {
                    error: Error::DeviceLost,
                },
                "terminalError",
            ),
            (Event::Clipped, "clipped"),
            (Event::PermissionGranted, "permissionGranted"),
        ] {
            assert_eq!(event_to_js(event).kind, expected);
        }
        for kind in [
            flexaudio::DefaultDeviceKind::Microphone,
            flexaudio::DefaultDeviceKind::SystemAudio,
        ] {
            let event = device_event_to_js(DeviceEvent::DefaultCleared { kind });
            assert_eq!(event.kind, "defaultCleared");
            assert!(event.id.is_none());
        }
        for count in [9_007_199_254_740_993, 9_223_372_036_854_775_808, u64::MAX] {
            assert_eq!(
                event_to_js(Event::ChunkDropped { count })
                    .count
                    .unwrap()
                    .get_u64(),
                (false, count, true)
            );
            assert_eq!(
                device_event_to_js(DeviceEvent::RescanRequired {
                    dropped_events: count
                })
                .dropped_events
                .unwrap()
                .get_u64(),
                (false, count, true)
            );
        }
    }

    #[test]
    fn loss_projection_preserves_known_and_unknown_scalar_counts() {
        let unknown = loss_to_js(AudioLoss::raw_overflow(None, None, 48_000, 2).unwrap()).unwrap();
        assert!(matches!(unknown.samples, Either::B(Null)));
        assert!(matches!(unknown.path.lane, Some(Either::B(Null))));
        for count in [9_007_199_254_740_993, 9_223_372_036_854_775_808, u64::MAX] {
            let loss = AudioLoss::output_overflow(
                OutputTap::Secondary,
                std::num::NonZeroU64::new(count),
                16_000,
                1,
            )
            .unwrap();
            let loss = loss_to_js(loss).unwrap();
            assert_eq!(loss.path.kind, "output");
            assert_eq!(loss.path.tap.as_deref(), Some("secondary"));
            match loss.samples {
                Either::A(count_js) => assert_eq!(count_js.get_u64(), (false, count, true)),
                _ => panic!("known count lost"),
            }
        }
    }
}
