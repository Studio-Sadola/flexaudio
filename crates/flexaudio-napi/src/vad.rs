//! Strict Node boundary for the shared streaming VAD owners.

use flexaudio_vad::{
    AttachedVadEvent, EpochEndReason, PreviewCloseReason, PreviewCutReason, Vad as CoreVad,
    VadError, VadEvent, VadEventKind, VadFailure, VadOptions, VadRuntimeOptions,
};
use napi::bindgen_prelude::{BigInt, Float32Array, FromNapiValue, ToNapiValue, TypeName};
use napi::{
    sys, Env, Error, JsObject, JsUnknown, KeyCollectionMode, KeyConversion, KeyFilter, NapiRaw,
    Status, ValueType,
};
use napi_derive::napi;

const PARAM_KEYS: &[&str] = &[
    "threshold",
    "minSpeechMs",
    "minSilenceMs",
    "maxSpeechMs",
    "speechPadMs",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BoundaryError {
    InvalidParameter,
    UnsupportedTap,
}

impl BoundaryError {
    fn code(&self) -> &'static str {
        match self {
            Self::InvalidParameter => "InvalidParameter",
            Self::UnsupportedTap => "UnsupportedTap",
        }
    }

    fn message(&self) -> &'static str {
        match self {
            Self::InvalidParameter => {
                "invalid VAD options because a field, type or value violates the supported contract"
            }
            Self::UnsupportedTap => "vad tap must be primary or an enabled secondary output",
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
    integer(value, 134_217.0)
}

fn integer(value: f64, upper: f64) -> Result<u32, BoundaryError> {
    if !value.is_finite() || !(0.0..=upper).contains(&value) || value.fract() != 0.0 {
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

fn params(object: &JsObject) -> Result<VadOptions, BoundaryError> {
    let mut params = VadOptions::default();
    if let Some(value) = number(object, "threshold")? {
        params.threshold = float_parameter(value, 1.0)?;
    }
    if let Some(value) = number(object, "maxSpeechMs")? {
        params.max_speech_ms = integer(value, f64::from(u32::MAX))?;
    }
    if let Some(value) = number(object, "minSpeechMs")? {
        params.min_speech_ms = duration(value)?;
    }
    if let Some(value) = number(object, "minSilenceMs")? {
        params.min_silence_ms = duration(value)?;
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
    secondary: bool,
) -> Result<(VadOptions, VadRuntimeOptions, crate::VadTap), BoundaryError> {
    let object = object(value)?;
    keys(&object, &["params", "provisional", "tap"])?;
    let parameters = object
        .get_named_property::<JsUnknown>("params")
        .map_err(|_| invalid())?;
    let parameters = match parameters.get_type().map_err(|_| invalid())? {
        ValueType::Undefined => VadOptions::default(),
        _ => {
            let parameters = self::object(parameters)?;
            keys(&parameters, PARAM_KEYS)?;
            params(&parameters)?
        }
    };
    let tap = object
        .get_named_property::<String>("tap")
        .map_err(|_| invalid())?;
    let tap = validate_tap(&tap, secondary)?;
    let options = VadRuntimeOptions {
        provisional: provisional(&object)?,
    };
    Ok((parameters, options, tap))
}

fn validate_tap(tap: &str, secondary: bool) -> Result<crate::VadTap, BoundaryError> {
    match tap {
        "primary" => Ok(crate::VadTap::Primary),
        "secondary" if secondary => Ok(crate::VadTap::Secondary),
        _ => Err(BoundaryError::UnsupportedTap),
    }
}

pub(crate) fn core_code(error: &VadError) -> (&'static str, Status) {
    match error.root() {
        VadError::InvalidParameter { .. } => ("InvalidParameter", Status::InvalidArg),
        VadError::InvalidPcm { .. } => ("InvalidPcm", Status::InvalidArg),
        VadError::InvalidProbability { .. } => ("InvalidProbability", Status::InvalidArg),
        VadError::Overflow { .. } => ("Overflow", Status::InvalidArg),
        VadError::SessionFinished => ("SessionFinished", Status::GenericFailure),
        VadError::Inference => ("Inference", Status::GenericFailure),
        VadError::ModelLoad => ("ModelLoad", Status::GenericFailure),
        VadError::FailedSession => ("FailedSession", Status::GenericFailure),
        VadError::Cause { primary, .. } => core_code(primary),
    }
}

/// Transport stores the exhaustive core enum; JS sees only its active variant's fields.
pub struct JsVadEvent(pub AttachedVadEvent);

impl TypeName for JsVadEvent {
    fn type_name() -> &'static str {
        "AttachedVadEvent"
    }
    fn value_type() -> ValueType {
        ValueType::Object
    }
}

impl ToNapiValue for JsVadEvent {
    unsafe fn to_napi_value(env: sys::napi_env, value: Self) -> napi::Result<sys::napi_value> {
        // SAFETY: N-API supplies the live environment for this conversion on the JS thread.
        let env = unsafe { Env::from_raw(env) };
        let mut object = env.create_object()?;
        object.set_named_property("epoch", value.0.epoch())?;
        object.set_named_property("seq", safe_number(value.0.seq())?)?;
        match value.0 {
            AttachedVadEvent::EpochStart {
                capture_sample,
                pts_ns,
                ..
            } => {
                object.set_named_property("type", "epochStart")?;
                object.set_named_property("captureSample", BigInt::from(capture_sample))?;
                object.set_named_property("ptsNs", pts_ns)?;
            }
            AttachedVadEvent::Vad(event) => {
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
            "VAD integer exceeds JS safe range",
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

fn event_fields(kind: &VadEventKind) -> EventFields {
    use VadEventKind::*;
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

pub(crate) fn marshal(events: Vec<AttachedVadEvent>) -> Vec<JsVadEvent> {
    events.into_iter().map(JsVadEvent).collect()
}

fn standalone(events: Vec<VadEvent>) -> Vec<JsVadEvent> {
    marshal(events.into_iter().map(AttachedVadEvent::Vad).collect())
}

pub(crate) fn throw_boundary(env: &Env, error: BoundaryError) -> Error {
    let original = flexaudio::Error::Vad {
        code: match error {
            BoundaryError::InvalidParameter => flexaudio::VadErrorCode::InvalidParameter,
            BoundaryError::UnsupportedTap => flexaudio::VadErrorCode::UnsupportedTap,
        },
    };
    throw_error(
        env,
        error.code(),
        error.message(),
        Status::InvalidArg,
        Vec::new(),
        None,
        original,
    )
}

fn throw_error(
    env: &Env,
    code: &str,
    message: &str,
    status: Status,
    events: Vec<JsVadEvent>,
    audio_error: Option<super::boundary::JsAudioError>,
    original: flexaudio::Error,
) -> Error {
    super::error_conversion::throw(
        env,
        &original,
        flexaudio::DropFailureOwner::Stream,
        "standalone VAD exception",
        message,
        || {
            super::error_conversion::build(
                env,
                Error::new(status, message),
                Some(code),
                |exception| {
                    exception.set_named_property("terminalEvents", events)?;
                    if let Some(error) = audio_error {
                        exception.set_named_property("audioError", error)?;
                    }
                    Ok(())
                },
            )
        },
    )
}

fn failure(env: &Env, error: VadFailure) -> Error {
    let (code, status) = core_code(&error.error);
    throw_error(
        env,
        code,
        &error.error.to_string(),
        status,
        standalone(error.terminal_events),
        Some(super::boundary::audio_error(&error.error.audio_error())),
        error.error.audio_error(),
    )
}

#[napi(object)]
pub struct FrameProbabilities {
    pub first_frame_index: f64,
    pub values: Float32Array,
}

/// Streaming probabilities and segments. Run synchronous inference in a Node Worker.
#[napi]
pub struct Vad {
    inner: CoreVad,
}

#[napi]
impl Vad {
    #[napi(constructor)]
    pub fn new(
        env: Env,
        #[napi(ts_arg_type = "VadOptions | undefined")] options: JsUnknown,
    ) -> napi::Result<Self> {
        let (params, provisional) = match options.get_type()? {
            ValueType::Undefined => (VadOptions::default(), false),
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
        let inner = CoreVad::new(params, VadRuntimeOptions { provisional })
            .map_err(|error| failure(&env, error.into()))?;
        Ok(Self { inner })
    }

    /// Convert other input formats with group-delay compensation; clip finite
    /// converter overshoot while rejecting invalid source PCM and nonfinite output.
    #[napi(ts_return_type = "VadEvent[]")]
    pub fn process(
        &mut self,
        env: Env,
        #[napi(ts_arg_type = "Float32Array")] input: JsUnknown,
        input_sample_rate: Option<f64>,
        input_channels: Option<f64>,
    ) -> napi::Result<Vec<JsVadEvent>> {
        // SAFETY: FromNapiValue validates typed-array identity; input is borrowed only during this call.
        let input =
            unsafe { Float32Array::from_napi_value(env.raw(), input.raw()) }.map_err(|_| {
                throw_error(
                    &env,
                    "InvalidPcm",
                    "expected interleaved Float32Array PCM",
                    Status::InvalidArg,
                    Vec::new(),
                    None,
                    flexaudio::Error::Vad {
                        code: flexaudio::VadErrorCode::InvalidPcm,
                    },
                )
            })?;
        let rate = integer(input_sample_rate.unwrap_or(16_000.0), f64::from(u32::MAX))
            .map_err(|error| throw_boundary(&env, error))?;
        let channels = integer(input_channels.unwrap_or(1.0), f64::from(u16::MAX))
            .and_then(|value| u16::try_from(value).map_err(|_| invalid()))
            .map_err(|error| throw_boundary(&env, error))?;
        self.inner
            .process_pcm(input.as_ref(), rate, channels)
            .map(standalone)
            .map_err(|e| failure(&env, e))
    }

    #[napi(ts_return_type = "VadEvent[]")]
    pub fn finish(&mut self, env: Env) -> napi::Result<Vec<JsVadEvent>> {
        self.inner
            .finish()
            .map(standalone)
            .map_err(|e| failure(&env, e))
    }

    #[napi(ts_return_type = "VadEvent[]")]
    pub fn reset(&mut self, env: Env) -> napi::Result<Vec<JsVadEvent>> {
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
        let segment = event_fields(&VadEventKind::Segment(flexaudio_vad::SpeechSegment {
            start_ms: 10,
            end_ms: 40,
        }));
        assert_eq!(segment.kind, "segment");
        assert_eq!(segment.range, Some((10, 40)));
        assert_eq!(segment.at, None);
        assert_eq!(segment.reason, None);
        let end = event_fields(&VadEventKind::ProvisionalSpeechEnd {
            at_ms: 32,
            reason: PreviewCloseReason::Reset,
        });
        assert_eq!(end.kind, "provisionalSpeechEnd");
        assert_eq!(end.at, Some(32));
        assert_eq!(end.reason, Some("reset"));
        assert_eq!(end.range, None);
        let cut = event_fields(&VadEventKind::ProvisionalCut {
            start_ms: 0,
            end_ms: 30_000,
            reason: PreviewCutReason::Limit,
        });
        assert_eq!(cut.kind, "provisionalCut");
        assert_eq!(cut.reason, Some("limit"));
        let start = event_fields(&VadEventKind::ProvisionalSpeechStart { at_ms: 0 });
        assert_eq!(start.kind, "provisionalSpeechStart");
        assert_eq!(start.at, Some(0));
        let terminal = event_fields(&VadEventKind::EpochEnd {
            reason: EpochEndReason::Error,
        });
        assert_eq!(terminal.kind, "epochEnd");
        assert_eq!(terminal.reason, Some("error"));
    }

    #[test]
    fn capture_indices_remain_bigints() {
        let event = AttachedVadEvent::EpochStart {
            epoch: 3,
            seq: 0,
            capture_sample: 9_007_199_254_740_993,
            pts_ns: 100,
        };
        assert_eq!(event.seq(), 0);
        if let AttachedVadEvent::EpochStart { capture_sample, .. } = event {
            let (_, value, lossless) = BigInt::from(capture_sample).get_u64();
            assert_eq!(value, capture_sample);
            assert!(lossless);
        }
        assert!(safe_number(9_007_199_254_740_993).is_err());
    }

    #[test]
    fn failures_have_stable_codes() {
        assert_eq!(
            core_code(&VadError::InvalidPcm { sample: 0 }),
            ("InvalidPcm", Status::InvalidArg)
        );
        assert_eq!(core_code(&VadError::SessionFinished).0, "SessionFinished");
        assert_eq!(core_code(&VadError::Inference).0, "Inference");
    }
}
