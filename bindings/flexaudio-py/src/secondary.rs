//! Secondary tap format validation and little-endian PCM projection.
use ::flexaudio as fa;
use pyo3::prelude::*;
use pyo3::types::PyBytes;

#[derive(Clone, Copy, Debug)]
pub(crate) enum Encoding {
    F32,
    S16,
}
impl Encoding {
    fn parse(value: &str) -> PyResult<Self> {
        match value {
            "f32" => Ok(Self::F32),
            "s16" => Ok(Self::S16),
            _ => Err(pyo3::exceptions::PyValueError::new_err(
                "encoding must be f32 or s16",
            )),
        }
    }
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::F32 => "f32",
            Self::S16 => "s16",
        }
    }
    fn bytes(self, samples: &[f32]) -> Vec<u8> {
        match self {
            Self::F32 => samples.iter().flat_map(|s| s.to_le_bytes()).collect(),
            Self::S16 => samples
                .iter()
                .flat_map(|s| fa::core::quantize_i16(*s).to_le_bytes())
                .collect(),
        }
    }
}

/// Fixed secondary output format and encoding, selected before capture starts.
#[pyclass(module = "flexaudio", frozen, skip_from_py_object)]
#[derive(Clone, Copy, Debug)]
pub struct SecondaryOutputOptions {
    pub(crate) format: fa::OutputFormat,
    pub(crate) encoding: Encoding,
}
#[pymethods]
impl SecondaryOutputOptions {
    #[new]
    #[pyo3(signature = (*, rate, channels, encoding="f32"))]
    fn new(rate: u32, channels: u16, encoding: &str) -> PyResult<Self> {
        let format = fa::OutputFormat {
            sample_rate: rate,
            channels,
        };
        format.validate().map_err(crate::to_py_err)?;
        Ok(Self {
            format,
            encoding: Encoding::parse(encoding)?,
        })
    }
    #[getter]
    fn rate(&self) -> u32 {
        self.format.sample_rate
    }
    #[getter]
    fn channels(&self) -> u16 {
        self.format.channels
    }
    #[getter]
    fn encoding(&self) -> &'static str {
        self.encoding.name()
    }
}

/// An independent secondary chunk. Meters describe float PCM before encoding.
#[pyclass(module = "flexaudio", name = "SecondaryChunk", frozen)]
pub struct PySecondaryChunk {
    pub(crate) chunk: fa::SecondaryChunk,
    encoding: Encoding,
    pub(crate) vad_events: Option<Vec<flexaudio_vad::AttachedVadEvent>>,
}
impl PySecondaryChunk {
    pub(crate) fn new(chunk: fa::SecondaryChunk, encoding: Encoding) -> Self {
        Self {
            chunk,
            encoding,
            vad_events: None,
        }
    }
}
#[pymethods]
impl PySecondaryChunk {
    #[getter]
    fn data<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, &self.encoding.bytes(&self.chunk.samples))
    }
    #[getter]
    fn encoding(&self) -> &'static str {
        self.encoding.name()
    }
    #[getter]
    fn frames(&self) -> usize {
        self.chunk.frames
    }
    #[getter]
    fn frame_index(&self) -> u64 {
        self.chunk.frame_index
    }
    #[getter]
    fn pts_ns(&self) -> i64 {
        self.chunk.pts_ns
    }
    #[getter]
    fn seq(&self) -> u64 {
        self.chunk.seq
    }
    #[getter]
    fn flags(&self) -> u32 {
        self.chunk.flags.bits()
    }
    #[getter]
    fn dropped_before(&self) -> u32 {
        self.chunk.dropped_before
    }
    #[getter]
    fn peak(&self) -> f32 {
        self.chunk.peak
    }
    #[getter]
    fn rms(&self) -> f32 {
        self.chunk.rms
    }
    #[getter]
    fn vad_events(&self, py: Python<'_>) -> PyResult<Option<Vec<Py<PyAny>>>> {
        self.vad_events
            .as_ref()
            .map(|events| {
                events
                    .iter()
                    .cloned()
                    .map(|e| crate::vad_marshal::attached_to_py(py, e))
                    .collect()
            })
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn secondary_encodings_are_little_endian_and_use_shared_quantization() {
        let samples = [-1.0, -0.5, -0.0, 0.0, 0.5, 1.0, 0.123456];
        let bytes = Encoding::S16.bytes(&samples);
        let actual: Vec<_> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| i16::from_le_bytes(*b))
            .collect();
        assert_eq!(actual, samples.map(fa::core::quantize_i16));
        assert_eq!(&actual[..6], &[-32768, -16384, 0, 0, 16384, 32767]);
        assert_eq!(
            Encoding::F32.bytes(&samples),
            samples
                .iter()
                .flat_map(|s| s.to_le_bytes())
                .collect::<Vec<_>>()
        );
        assert!(Encoding::parse("other").is_err());
    }
    #[test]
    fn meters_are_retained_before_integer_quantization() {
        Python::initialize();
        Python::attach(|py| {
            let value = 0.123456_f32;
            let chunk = fa::SecondaryChunk {
                samples: vec![value],
                frames: 1,
                frame_index: 960,
                pts_ns: 20_000_000,
                seq: 7,
                flags: fa::ChunkFlags::DISCONTINUITY,
                dropped_before: 2,
                peak: value,
                rms: value,
            };
            let projected = PySecondaryChunk::new(chunk, Encoding::S16);
            assert_eq!(projected.peak(), value);
            assert_eq!(projected.rms(), value);
            assert_eq!(
                projected.data(py).as_bytes(),
                fa::core::quantize_i16(value).to_le_bytes()
            );
            assert_ne!(value, f32::from(fa::core::quantize_i16(value)) / 32768.0);
            assert_eq!(projected.frame_index(), 960);
            assert_eq!(projected.seq(), 7);
            assert_eq!(projected.dropped_before(), 2);
        });
    }
}
