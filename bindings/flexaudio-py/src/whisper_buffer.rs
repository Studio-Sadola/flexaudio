//! Python 3.8 stable-ABI canonical PCM access without a mandatory NumPy dependency.
use pyo3::{exceptions::PyTypeError, prelude::*, types::PyMemoryView};
use std::{
    ffi::{c_int, c_void},
    mem, ptr, slice,
};

// This stable-ABI function predates Python 3.8. PyO3's PyBuffer is available only
// with abi3-py311, while this extension retains abi3-py38. A live memoryview pins
// the exporter for the complete Rust call; the deprecated accessor does not own it.
unsafe extern "C" {
    fn PyObject_AsReadBuffer(
        object: *mut pyo3::ffi::PyObject,
        buffer: *mut *const c_void,
        length: *mut pyo3::ffi::Py_ssize_t,
    ) -> c_int;
}

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
            let mut pointer = ptr::null();
            let mut bytes = 0;
            // SAFETY: view is a live pinned contiguous buffer. Mandatory outputs are initialized.
            if unsafe { PyObject_AsReadBuffer(view.as_ptr(), &mut pointer, &mut bytes) } != 0 {
                return Err(PyErr::fetch(samples.py()));
            }
            let bytes = usize::try_from(bytes).map_err(|_| {
                crate::whisper_vad::boundary_error(
                    samples.py(),
                    "InvalidPcm",
                    "invalid buffer length",
                )
            })?;
            if bytes % mem::size_of::<f32>() != 0
                || bytes > isize::MAX as usize
                || (bytes != 0 && (pointer.is_null() || !pointer.cast::<f32>().is_aligned()))
            {
                return Err(crate::whisper_vad::boundary_error(
                    samples.py(),
                    "InvalidPcm",
                    "invalid buffer size or alignment",
                ));
            }
            let values = if bytes == 0 {
                &[]
            } else {
                // SAFETY: format, alignment, layout and byte length were checked. The memoryview
                // keeps the allocation alive. run calls only Rust, retains no input, and never
                // calls Python or releases the GIL, so no Python mutation can invalidate this borrow.
                unsafe {
                    slice::from_raw_parts(pointer.cast::<f32>(), bytes / mem::size_of::<f32>())
                }
            };
            Ok(run(values))
        }
        Err(error) if error.is_instance_of::<PyTypeError>(samples.py()) => {
            let values = samples.extract::<Vec<f32>>()?;
            Ok(run(&values))
        }
        Err(error) => Err(error),
    }
}
