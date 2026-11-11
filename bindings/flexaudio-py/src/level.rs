//! Python projection of the shared raw input level.
use pyo3::prelude::*;

/// Raw normalized input level before denoise and gain, including while paused.
#[pyclass(module = "flexaudio", name = "Level", frozen, get_all)]
pub struct Level {
    peak: f32,
    rms: f32,
    age_ns: u64,
}

impl From<::flexaudio::Level> for Level {
    fn from(value: ::flexaudio::Level) -> Self {
        Self {
            peak: value.peak,
            rms: value.rms,
            age_ns: value.age_ns,
        }
    }
}
