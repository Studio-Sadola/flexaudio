//! Strict Node boundary for the shared whisper-compatible VAD owners.

use flexaudio_vad::{
    AttachedWhisperVadEvent, EpochEndReason, PreviewCloseReason, PreviewCutReason,
    WhisperVad as CoreWhisperVad, WhisperVadError, WhisperVadEvent, WhisperVadEventKind,
    WhisperVadFailure, WhisperVadOptions, WhisperVadParams,
};
use napi::bindgen_prelude::{BigInt, Float32Array, FromNapiValue, ToNapiValue, TypeName};
use napi::{
    sys, Env, Error, JsObject, JsUnknown, KeyCollectionMode, KeyConversion, KeyFilter, NapiRaw,
    Status, ValueType,
};
use napi_derive::napi;

const PARAM_KEYS: &[&str] = &[
    "threshold",
    "minSpeechDurationMs",
    "minSilenceDurationMs",
    "maxSpeechDurationS",
    "speechPadMs",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BoundaryError {
    InvalidParameter,
    ConflictingVad,
    UnsupportedTap,
    UnsupportedConversionClock,
}

impl BoundaryError {
    fn code(&self) -> &'static str {
        match self {
            Self::InvalidParameter => "InvalidParameter",
            Self::ConflictingVad => "ConflictingVad",
            Self::UnsupportedTap => "UnsupportedTap",
            Self::UnsupportedConversionClock => "UnsupportedConversionClock",
        }
    }

    fn message(&self) -> &'static str {
        match self {
            Self::InvalidParameter => "invalid whisper VAD options",
            Self::ConflictingVad => "whisperVad cannot be combined with vad or vadTap",
            Self::UnsupportedTap => "whisperVad tap must be primary or an enabled secondary output",
            Self::UnsupportedConversionClock => {
                "whisperVad requires a canonical capture feed with producer frame provenance"
            }
        }
    }
}

fn invalid() -> BoundaryError {
    BoundaryError::InvalidParameter
}

/// Validate the original JS number before the intentional f64-to-f32 conversion.
fn float_parameter(value: f64, upper: f64) -> Result<f32, BoundaryError> {
    if !value.is_finite() || !(0.0..=upper).contains(&value) {
        return Err(invalid());
    }
    Ok(value as f32)
}

fn duration(value: f64) -> Result<u32, BoundaryError> {
    if !value.is_finite() || !(0.0..=134_217.0).contains(&value) || value.fract() != 0.0 {
        return Err(invalid());
    }
    Ok(value as u32)
}

fn keys(object: &JsObject, allowed: &[&str]) -> Result<(), BoundaryError> {
    let names = object
        .get_all_property_names(
            KeyCollectionMode::OwnOnly,
            KeyFilter::AllProperties,
            KeyConversion::NumbersToStrings,
        )
        .map_err(|_| invalid())?;
    for index in 0..names.get_array_length().map_err(|_| invalid())? {
        let key = names
            .get_element::<napi::JsString>(index)
            .map_err(|_| invalid())?
            .into_utf8()
            .map_err(|_| invalid())?;
        if !allowed.contains(&key.as_str().map_err(|_| invalid())?) {
            return Err(invalid());
        }
    }
    Ok(())
}

fn number(object: &JsObject, name: &str) -> Result<Option<f64>, BoundaryError> {
    let value = object
        .get_named_property::<JsUnknown>(name)
        .map_err(|_| invalid())?;
    match value.get_type().map_err(|_| invalid())? {
        ValueType::Undefined => Ok(None),
        ValueType::Number => object
            .get_named_property::<f64>(name)
            .map(Some)
            .map_err(|_| invalid()),
        _ => Err(invalid()),
    }
}

fn provisional(object: &JsObject) -> Result<bool, BoundaryError> {
    let value = object
        .get_named_property::<JsUnknown>("provisional")
        .map_err(|_| invalid())?;
    match value.get_type().map_err(|_| invalid())? {
        ValueType::Undefined => Ok(false),
        ValueType::Boolean => object
            .get_named_property::<bool>("provisional")
            .map_err(|_| invalid()),
        _ => Err(invalid()),
    }
}

fn params(object: &JsObject) -> Result<WhisperVadParams, BoundaryError> {
    let mut params = WhisperVadParams::default();
    if let Some(value) = number(object, "threshold")? {
        params.threshold = float_parameter(value, 1.0)?;
    }
    if let Some(value) = number(object, "maxSpeechDurationS")? {
        params.max_speech_duration_s = float_parameter(value, f64::from(f32::MAX))?;
    }
    if let Some(value) = number(object, "minSpeechDurationMs")? {
        params.min_speech_duration_ms = duration(value)?;
    }
    if let Some(value) = number(object, "minSilenceDurationMs")? {
        params.min_silence_duration_ms = duration(value)?;
    }
    if let Some(value) = number(object, "speechPadMs")? {
        params.speech_pad_ms = duration(value)?;
    }
    params.validate().map_err(|_| invalid())?;
    Ok(params)
}

fn object(value: JsUnknown) -> Result<JsObject, BoundaryError> {
    if value.get_type().map_err(|_| invalid())? != ValueType::Object {
        return Err(invalid());
    }
    // SAFETY: the value's object type was checked above; arrays are rejected below.
    let object: JsObject = unsafe { value.cast() };
    if object.is_array().map_err(|_| invalid())? {
        return Err(invalid());
    }
    Ok(object)
}

pub(crate) fn stream_options(
    value: JsUnknown,
    legacy: bool,
    legacy_tap: bool,
    secondary: bool,
) -> Result<(), BoundaryError> {
    if legacy || legacy_tap {
        return Err(BoundaryError::ConflictingVad);
    }
    let object = object(value)?;
    let mut allowed = PARAM_KEYS.to_vec();
    allowed.extend(["provisional", "tap"]);
    keys(&object, &allowed)?;
    params(&object)?;
    let tap = object
        .get_named_property::<String>("tap")
        .map_err(|_| invalid())?;
    validate_tap(&tap, secondary)?;
    provisional(&object)?;
    Ok(())
}

fn validate_tap(tap: &str, secondary: bool) -> Result<crate::VadTap, BoundaryError> {
    match tap {
        "primary" => Ok(crate::VadTap::Primary),
        "secondary" if secondary => Ok(crate::VadTap::Secondary),
        _ => Err(BoundaryError::UnsupportedTap),
    }
}

pub(crate) fn core_code(error: &WhisperVadError) -> (&'static str, Status) {
    match error {
        WhisperVadError::InvalidParameter { .. } => ("InvalidParameter", Status::InvalidArg),
        WhisperVadError::InvalidPcm { .. } => ("InvalidPcm", Status::InvalidArg),
        WhisperVadError::InvalidProbability { .. } => ("InvalidProbability", Status::InvalidArg),
        WhisperVadError::Overflow { .. } => ("Overflow", Status::InvalidArg),
        WhisperVadError::SessionFinished => ("SessionFinished", Status::GenericFailure),
        WhisperVadError::Inference => ("Inference", Status::GenericFailure),
        WhisperVadError::ModelLoad => ("ModelLoad", Status::GenericFailure),
        WhisperVadError::FailedSession => ("FailedSession", Status::GenericFailure),
    }
}

/// Transport stores the exhaustive core enum; JS sees only its active variant's fields.
pub struct JsWhisperVadEvent(pub AttachedWhisperVadEvent);

impl TypeName for JsWhisperVadEvent {
    fn type_name() -> &'static str {
        "AttachedWhisperVadEvent"
    }
    fn value_type() -> ValueType {
        ValueType::Object
    }
}

impl ToNapiValue for JsWhisperVadEvent {
    unsafe fn to_napi_value(env: sys::napi_env, value: Self) -> napi::Result<sys::napi_value> {
        // SAFETY: N-API supplies the live environment for this conversion on the JS thread.
        let env = unsafe { Env::from_raw(env) };
        let mut object = env.create_object()?;
        object.set_named_property("epoch", value.0.epoch())?;
        object.set_named_property("seq", safe_number(value.0.seq())?)?;
        match value.0 {
            AttachedWhisperVadEvent::EpochStart {
                capture_sample,
                pts_ns,
                ..
            } => {
                object.set_named_property("type", "epochStart")?;
                object.set_named_property("captureSample", BigInt::from(capture_sample))?;
                object.set_named_property("ptsNs", pts_ns)?;
            }
            AttachedWhisperVadEvent::Vad(event) => {
                let fields = event_fields(&event.kind);
                object.set_named_property("type", fields.kind)?;
                if let Some((start, end)) = fields.range {
                    object.set_named_property("startMs", safe_number(start)?)?;
                    object.set_named_property("endMs", safe_number(end)?)?;
                }
                if let Some(at) = fields.at {
                    object.set_named_property("atMs", safe_number(at)?)?;
                }
                if let Some(reason) = fields.reason {
                    object.set_named_property("reason", reason)?;
                }
            }
        }
        Ok(object.raw())
    }
}

fn safe_number(value: u64) -> napi::Result<f64> {
    if value > 9_007_199_254_740_991 {
        return Err(Error::new(
            Status::GenericFailure,
            "whisper VAD integer exceeds JS safe range",
        ));
    }
    Ok(value as f64)
}

struct EventFields {
    kind: &'static str,
    range: Option<(u64, u64)>,
    at: Option<u64>,
    reason: Option<&'static str>,
}

fn event_fields(kind: &WhisperVadEventKind) -> EventFields {
    use WhisperVadEventKind::*;
    let mut fields = EventFields {
        kind: "",
        range: None,
        at: None,
        reason: None,
    };
    match kind {
        Segment(segment) => {
            fields.kind = "segment";
            fields.range = Some((segment.start_ms, segment.end_ms));
        }
        ProvisionalSpeechStart { at_ms } => {
            fields.kind = "provisionalSpeechStart";
            fields.at = Some(*at_ms);
        }
        ProvisionalSpeechEnd { at_ms, reason } => {
            fields.kind = "provisionalSpeechEnd";
            fields.at = Some(*at_ms);
            fields.reason = Some(close_reason(*reason));
        }
        ProvisionalCut {
            start_ms,
            end_ms,
            reason,
        } => {
            fields.kind = "provisionalCut";
            fields.range = Some((*start_ms, *end_ms));
            fields.reason = Some(match reason {
                PreviewCutReason::Limit => "limit",
                PreviewCutReason::Hysteresis => "hysteresis",
                PreviewCutReason::Finish => "finish",
                PreviewCutReason::Reset => "reset",
                PreviewCutReason::Error => "error",
            });
        }
        EpochEnd { reason } => {
            fields.kind = "epochEnd";
            fields.reason = Some(match reason {
                EpochEndReason::Finish => "finish",
                EpochEndReason::Reset => "reset",
                EpochEndReason::Error => "error",
            });
        }
    }
    fields
}

fn close_reason(reason: PreviewCloseReason) -> &'static str {
    match reason {
        PreviewCloseReason::Hysteresis => "hysteresis",
        PreviewCloseReason::Finish => "finish",
        PreviewCloseReason::Reset => "reset",
        PreviewCloseReason::Error => "error",
    }
}

pub(crate) fn marshal(events: Vec<AttachedWhisperVadEvent>) -> Vec<JsWhisperVadEvent> {
    events.into_iter().map(JsWhisperVadEvent).collect()
}

fn standalone(events: Vec<WhisperVadEvent>) -> Vec<JsWhisperVadEvent> {
    marshal(
        events
            .into_iter()
            .map(AttachedWhisperVadEvent::Vad)
            .collect(),
    )
}

pub(crate) fn throw_boundary(env: &Env, error: BoundaryError) -> Error {
    throw_error(
        env,
        error.code(),
        error.message(),
        if error == BoundaryError::UnsupportedConversionClock {
            Status::GenericFailure
        } else {
            Status::InvalidArg
        },
        Vec::new(),
    )
}

fn throw_error(
    env: &Env,
    code: &str,
    message: &str,
    status: Status,
    events: Vec<JsWhisperVadEvent>,
) -> Error {
    let result = (|| {
        let mut exception = env.create_error(Error::new(status, message))?;
        exception.set_named_property("code", code)?;
        exception.set_named_property("terminalEvents", events)?;
        env.throw(exception)
    })();
    match result {
        Ok(()) => Error::new(Status::PendingException, message),
        Err(error) => error,
    }
}

fn failure(env: &Env, error: WhisperVadFailure) -> Error {
    let (code, status) = core_code(&error.error);
    throw_error(
        env,
        code,
        &error.error.to_string(),
        status,
        standalone(error.terminal_events),
    )
}

#[napi(object)]
pub struct FrameProbabilities {
    pub first_frame_index: f64,
    pub values: Float32Array,
}

/// Mono float32 at 16 kHz. Run synchronous inference in a Node Worker.
#[napi]
pub struct WhisperVad {
    inner: CoreWhisperVad,
}

#[napi]
impl WhisperVad {
    #[napi(constructor)]
    pub fn new(
        env: Env,
        #[napi(ts_arg_type = "WhisperVadParams | undefined")] options: JsUnknown,
    ) -> napi::Result<Self> {
        let (params, provisional) = match options.get_type()? {
            ValueType::Undefined => (WhisperVadParams::default(), false),
            _ => {
                let object = object(options).map_err(|e| throw_boundary(&env, e))?;
                let mut allowed = PARAM_KEYS.to_vec();
                allowed.push("provisional");
                keys(&object, &allowed).map_err(|e| throw_boundary(&env, e))?;
                (
                    params(&object).map_err(|e| throw_boundary(&env, e))?,
                    provisional(&object).map_err(|e| throw_boundary(&env, e))?,
                )
            }
        };
        let inner = CoreWhisperVad::new(params, WhisperVadOptions { provisional })
            .map_err(|error| failure(&env, error.into()))?;
        Ok(Self { inner })
    }

    #[napi(ts_return_type = "WhisperVadEvent[]")]
    pub fn process(
        &mut self,
        env: Env,
        #[napi(ts_arg_type = "Float32Array")] input: JsUnknown,
    ) -> napi::Result<Vec<JsWhisperVadEvent>> {
        // SAFETY: FromNapiValue validates typed-array identity; input is borrowed only during this call.
        let input =
            unsafe { Float32Array::from_napi_value(env.raw(), input.raw()) }.map_err(|_| {
                throw_error(
                    &env,
                    "InvalidPcm",
                    "expected Float32Array mono16k PCM",
                    Status::InvalidArg,
                    Vec::new(),
                )
            })?;
        self.inner
            .process(input.as_ref())
            .map(standalone)
            .map_err(|e| failure(&env, e))
    }

    #[napi(ts_return_type = "WhisperVadEvent[]")]
    pub fn finish(&mut self, env: Env) -> napi::Result<Vec<JsWhisperVadEvent>> {
        self.inner
            .finish()
            .map(standalone)
            .map_err(|e| failure(&env, e))
    }

    #[napi(ts_return_type = "WhisperVadEvent[]")]
    pub fn reset(&mut self, env: Env) -> napi::Result<Vec<JsWhisperVadEvent>> {
        self.inner
            .reset()
            .map(standalone)
            .map_err(|e| failure(&env, e))
    }

    #[napi]
    pub fn last_frame_probabilities(&self) -> napi::Result<FrameProbabilities> {
        let probabilities = self.inner.last_frame_probabilities();
        Ok(FrameProbabilities {
            first_frame_index: safe_number(probabilities.first_frame_index)?,
            values: Float32Array::new(probabilities.values.to_vec()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wider_numbers_are_validated_before_narrowing() {
        for value in [f64::NAN, f64::INFINITY, -0.1, 1.0000000001] {
            assert_eq!(
                float_parameter(value, 1.0).unwrap_err().code(),
                "InvalidParameter"
            );
        }
        assert_eq!(float_parameter(0.0, 1.0).unwrap(), 0.0);
        assert_eq!(
            float_parameter(f64::from(f32::MAX), f64::from(f32::MAX)).unwrap(),
            f32::MAX
        );
        assert!(float_parameter(f64::from(f32::MAX) * 1.00000001, f64::from(f32::MAX)).is_err());
    }

    #[test]
    fn durations_do_not_wrap_or_truncate() {
        for value in [
            -1.0,
            0.5,
            134_218.0,
            4_294_967_296.0,
            f64::NAN,
            f64::INFINITY,
        ] {
            assert!(duration(value).is_err());
        }
        assert_eq!(duration(0.0).unwrap(), 0);
        assert_eq!(duration(134_217.0).unwrap(), 134_217);
    }

    #[test]
    fn tap_selection_requires_enabled_secondary() {
        assert!(validate_tap("primary", false).is_ok());
        assert!(validate_tap("secondary", true).is_ok());
        assert_eq!(
            validate_tap("secondary", false).err().unwrap().code(),
            "UnsupportedTap"
        );
        assert!(validate_tap("other", true).is_err());
    }

    #[test]
    fn marshal_active_payload_only() {
        let segment = event_fields(&WhisperVadEventKind::Segment(
            flexaudio_vad::WhisperSpeechSegment {
                start_ms: 10,
                end_ms: 40,
            },
        ));
        assert_eq!(segment.kind, "segment");
        assert_eq!(segment.range, Some((10, 40)));
        assert_eq!(segment.at, None);
        assert_eq!(segment.reason, None);
        let end = event_fields(&WhisperVadEventKind::ProvisionalSpeechEnd {
            at_ms: 32,
            reason: PreviewCloseReason::Reset,
        });
        assert_eq!(end.kind, "provisionalSpeechEnd");
        assert_eq!(end.at, Some(32));
        assert_eq!(end.reason, Some("reset"));
        assert_eq!(end.range, None);
        let cut = event_fields(&WhisperVadEventKind::ProvisionalCut {
            start_ms: 0,
            end_ms: 30_000,
            reason: PreviewCutReason::Limit,
        });
        assert_eq!(cut.kind, "provisionalCut");
        assert_eq!(cut.reason, Some("limit"));
        let start = event_fields(&WhisperVadEventKind::ProvisionalSpeechStart { at_ms: 0 });
        assert_eq!(start.kind, "provisionalSpeechStart");
        assert_eq!(start.at, Some(0));
        let terminal = event_fields(&WhisperVadEventKind::EpochEnd {
            reason: EpochEndReason::Error,
        });
        assert_eq!(terminal.kind, "epochEnd");
        assert_eq!(terminal.reason, Some("error"));
    }

    #[test]
    fn capture_indices_remain_bigints() {
        let event = AttachedWhisperVadEvent::EpochStart {
            epoch: 3,
            seq: 0,
            capture_sample: 9_007_199_254_740_993,
            pts_ns: 100,
        };
        assert_eq!(event.seq(), 0);
        if let AttachedWhisperVadEvent::EpochStart { capture_sample, .. } = event {
            let (_, value, lossless) = BigInt::from(capture_sample).get_u64();
            assert_eq!(value, capture_sample);
            assert!(lossless);
        }
        assert!(safe_number(9_007_199_254_740_993).is_err());
    }

    #[test]
    fn failures_have_stable_codes() {
        assert_eq!(
            core_code(&WhisperVadError::InvalidPcm { sample: 0 }),
            ("InvalidPcm", Status::InvalidArg)
        );
        assert_eq!(
            core_code(&WhisperVadError::SessionFinished).0,
            "SessionFinished"
        );
        assert_eq!(core_code(&WhisperVadError::Inference).0, "Inference");
    }
}
