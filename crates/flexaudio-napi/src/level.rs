//! JS projection of the facade's shared latest-value meter.
use napi::bindgen_prelude::BigInt;
use napi_derive::napi;

/// Raw normalized input level before denoise and gain, including while paused.
#[napi(object, js_name = "Level")]
pub struct JsLevel {
    pub peak: f64,
    pub rms: f64,
    /// Monotonic nanoseconds since measurement, not hardware latency.
    pub age_ns: BigInt,
}

impl From<flexaudio::Level> for JsLevel {
    fn from(value: flexaudio::Level) -> Self {
        Self {
            peak: f64::from(value.peak),
            rms: f64::from(value.rms),
            age_ns: BigInt::from(value.age_ns),
        }
    }
}
