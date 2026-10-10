//! Python 3.8 stable-ABI canonical PCM access without a mandatory NumPy dependency.
use pyo3::{
    exceptions::PyTypeError,
    prelude::*,
    types::{PyBytes, PyMemoryView},
};
use std::mem;

pub(crate) fn with_samples<T>(
    samples: &Bound<'_, PyAny>,
    run: impl FnOnce(&[f32]) -> T,
) -> PyResult<T> {
    match PyMemoryView::from(samples) {
        Ok(view) => {
            let dimensions = view.getattr("ndim")?.extract::<usize>()?;
            let contiguous = view.getattr("c_contiguous")?.extract::<bool>()?;
            let format = view.getattr("format")?.extract::<String>()?;
            let item_size = view.getattr("itemsize")?.extract::<usize>()?;
            if dimensions != 1
                || !contiguous
                || item_size != mem::size_of::<f32>()
                || !matches!(format.as_str(), "f" | "@f" | "=f")
            {
                return Err(crate::whisper_vad::boundary_error(
                    samples.py(),
                    "InvalidPcm",
                    "input must be a contiguous one-dimensional native float32 buffer",
                ));
            }
            // PyBuffer requires abi3-py311. For abi3-py38, use Python's memoryview
            // method through PyO3's stable-ABI APIs. This copies into Python bytes
            // and then an owned Rust Vec, avoiding raw buffer pointers and alignment
            // requirements while preserving each native float32 bit pattern.
            let owned = view.call_method0("tobytes")?;
            let bytes = owned.cast::<PyBytes>()?.as_bytes();
            if !bytes.len().is_multiple_of(mem::size_of::<f32>())
                || bytes.len() > isize::MAX as usize
            {
                return Err(crate::whisper_vad::boundary_error(
                    samples.py(),
                    "InvalidPcm",
                    "invalid buffer length: expected a whole number of float32 samples",
                ));
            }
            let values: Vec<f32> = bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|&chunk| f32::from_ne_bytes(chunk))
                .collect();
            Ok(run(&values))
        }
        Err(error) if error.is_instance_of::<PyTypeError>(samples.py()) => {
            let values = samples.extract::<Vec<f32>>()?;
            Ok(run(&values))
        }
        Err(error) => Err(error),
    }
}
