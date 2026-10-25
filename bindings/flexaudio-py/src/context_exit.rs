//! Preserve the body exception when context-manager cleanup also fails.
use pyo3::prelude::*;

pub(crate) fn attach_cleanup_failure(body: Bound<'_, PyAny>, error: PyErr, operation: &str) {
    let note = format!("{operation} also failed: {error}");
    // Notes aid display, while the context chain retains the typed exception itself.
    let _note_result = body.call_method1("add_note", (note,));

    // Python <3.11 has no add_note, and subclasses may reject it. Use the
    // exception API rather than user-overridable attribute assignment.
    let py = body.py();
    let mut current = PyErr::from_value(body);
    let mut seen = Vec::new();
    loop {
        if current.value(py).is(error.value(py)) {
            return;
        }
        if seen
            .iter()
            .any(|previous: &PyErr| previous.value(py).is(current.value(py)))
        {
            // A user-created context cycle has no tail to append to safely.
            error.write_unraisable(py, None);
            return;
        }
        seen.push(current.clone_ref(py));
        match current.context(py) {
            Some(context) => current = context,
            None => {
                current.set_context(py, Some(error));
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pyo3::exceptions::{PyKeyError, PyRuntimeError, PyValueError};

    #[test]
    fn successful_note_keeps_both_distinct_typed_failures() {
        Python::initialize();
        Python::attach(|py| {
            let body = crate::to_py_err(::flexaudio::Error::DeviceLost);
            let cleanup = crate::to_py_err(::flexaudio::Error::Backend("cleanup failed".into()));
            attach_cleanup_failure(
                body.value(py).clone().into_any(),
                cleanup.clone_ref(py),
                "Stop",
            );
            let retained = body.context(py).expect("typed cleanup context");
            assert!(retained.value(py).is(cleanup.value(py)));
            let audio = retained.value(py).getattr("audio_error").unwrap();
            let audio = audio
                .extract::<PyRef<'_, crate::errors::AudioError>>()
                .unwrap();
            assert_eq!(
                audio.0,
                ::flexaudio::Error::Backend("cleanup failed".into())
            );
        });
    }

    #[test]
    fn encoder_error_preserves_distinct_write_and_close_causes() {
        Python::initialize();
        Python::attach(|py| {
            flexaudio_core::test_support::assert_distinct_failures_retained(
                ::flexaudio::Error::Backend("write failed".into()),
                ::flexaudio::Error::Backend("close failed".into()),
                |first, second| {
                    let mut retained = Some(first);
                    ::flexaudio::core::terminal_causes::retain_terminal_cause(
                        &mut retained,
                        second,
                    );
                    let error = crate::encode_err_to_py(flexaudio_encode::EncodeError::Audio(
                        retained.unwrap(),
                    ));
                    let value = error.value(py).getattr("audio_error").unwrap();
                    value
                        .extract::<PyRef<'_, crate::errors::AudioError>>()
                        .unwrap()
                        .0
                        .clone()
                },
            );
        });
    }

    #[test]
    fn cleanup_note_preserves_existing_context() {
        Python::initialize();
        Python::attach(|py| {
            let body = PyValueError::new_err("body failure");
            let previous = PyKeyError::new_err("previous failure");
            body.set_context(py, Some(previous.clone_ref(py)));
            attach_cleanup_failure(
                body.value(py).clone().into_any(),
                PyRuntimeError::new_err("cleanup failure"),
                "Test cleanup",
            );
            assert!(body.context(py).unwrap().value(py).is(previous.value(py)));
            if let Ok(notes) = body.value(py).getattr("__notes__") {
                let notes: Vec<String> = notes.extract().unwrap();
                assert!(notes.iter().any(|note| note.contains("cleanup failure")));
            } else {
                assert!(previous
                    .context(py)
                    .unwrap()
                    .to_string()
                    .contains("cleanup failure"));
            }
        });
    }

    #[test]
    fn rejected_notes_append_cleanup_without_overwriting_existing_context() {
        Python::initialize();
        Python::attach(|py| {
            let module = pyo3::types::PyModule::from_code(
                py,
                pyo3::ffi::c_str!(
                    "class HostileError(Exception):\n    def add_note(self, note):\n        raise RuntimeError('note rejected')\n    def __setattr__(self, name, value):\n        raise RuntimeError('assignment rejected')\nbody = HostileError('body failure')"
                ),
                pyo3::ffi::c_str!("context_exit_test.py"),
                pyo3::ffi::c_str!("context_exit_test"),
            )
            .unwrap();
            let body = PyErr::from_value(module.getattr("body").unwrap());
            let previous = PyKeyError::new_err("previous failure");
            let cleanup = PyRuntimeError::new_err("cleanup failure");
            body.set_context(py, Some(previous.clone_ref(py)));
            attach_cleanup_failure(
                body.value(py).clone().into_any(),
                cleanup.clone_ref(py),
                "Test cleanup",
            );
            assert!(body.context(py).unwrap().value(py).is(previous.value(py)));
            assert!(previous
                .context(py)
                .unwrap()
                .value(py)
                .is(cleanup.value(py)));
            assert_eq!(body.value(py).to_string(), "body failure");
        });
    }
}
