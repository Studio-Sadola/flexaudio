//! Preserve the body exception when context-manager cleanup also fails.
use pyo3::prelude::*;

pub(crate) fn attach_cleanup_failure(body: Bound<'_, PyAny>, error: PyErr, operation: &str) {
    let note = format!("{operation} also failed: {error}");
    if body.call_method1("add_note", (note,)).is_ok() {
        return;
    }

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
