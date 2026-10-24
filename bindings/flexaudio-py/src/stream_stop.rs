//! The single retained shutdown path for explicit stop, context exit, and drop.
use super::*;

impl Stream {
    pub(super) fn retain_shutdown_outcome(&mut self) {
        if self.shutdown.is_some() {
            return;
        }
        self.inner.stop();
        self.route_events();
        let core_report = self.inner.shutdown_report();
        let primary = self.inner.terminal_error();
        let mut cleanup = core_report
            .map(|r| r.cleanup().to_vec())
            .unwrap_or_default();
        let before = cleanup.len();
        self.drain_capture();
        if primary.is_none() {
            if let Err(error) = self.drain_output() {
                let cause = Python::attach(|py| {
                    error
                        .value(py)
                        .getattr("audio_error")
                        .and_then(|value| {
                            value
                                .extract::<PyRef<'_, crate::errors::AudioError>>()
                                .map(|error| error.0.clone())
                                .map_err(PyErr::from)
                        })
                        .unwrap_or_else(|_| {
                            fa::Error::Backend("binding output drain failed".into())
                        })
                });
                cleanup.push(cause.with_context(fa::ErrorContext::new(fa::Operation::Flush)));
            }
        } else {
            self.ready_chunks.retain(|chunk| chunk.samples().is_empty());
            self.ready_secondary
                .retain(|chunk| chunk.chunk.samples.is_empty());
        }
        if let Some(vad_tap) = self.vad_tap.as_mut() {
            let result = vad_tap.stop();
            self.accept_vad_tap(result, fa::Operation::Stop);
            self.vad_carrier();
        }
        cleanup.extend(self.vad_cleanup.iter().cloned());
        self.local_events.extend(
            cleanup[before..]
                .iter()
                .cloned()
                .map(|error| fa::Event::ShutdownError { error }),
        );
        let report = fa::ShutdownReport::new(primary, cleanup);
        self.shutdown = Some(report);
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        let first_shutdown = self.shutdown.is_none();
        self.retain_shutdown_outcome();
        if first_shutdown {
            if let Some(report) = &self.shutdown {
                // Isolated VAD failures do not fail explicit stop, but GC must
                // still deliver their retained diagnostics before destroying them.
                let failure = report.result().err().or_else(|| {
                    report.cleanup().split_first().and_then(|(first, rest)| {
                        fa::ShutdownReport::new(Some(first.clone()), rest.to_vec())
                            .result()
                            .err()
                    })
                });
                if let Some(error) = failure {
                    Python::attach(|py| to_py_err(error).write_unraisable(py, None));
                }
            }
        }
    }
}

#[cfg(any(test, feature = "test-faults"))]
#[pyfunction]
pub(crate) fn _stream_stop_failure() -> PyResult<Stream> {
    let inner =
        fa::test_support::stop_failure(fa::test_support::StopPhase::Stop).map_err(to_py_err)?;
    Ok(Stream {
        inner,
        shutdown: None,
        local_events: Default::default(),
        lifecycle_events: Default::default(),
        capture_events: Default::default(),
        settings: Default::default(),
        vad_tap: None,
        vad_events: Vec::new(),
        vad_origin: (0, 0),
        vad_error: None,
        vad_cleanup: Vec::new(),
        secondary: None,
        ready_secondary: Default::default(),
        vad_secondary: false,
        secondary_metadata: (0, 0, 0, 0),
        ready_chunks: Default::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pyo3::ffi::c_str;
    use pyo3::types::PyDict;

    fn fixture() -> Stream {
        _stream_stop_failure().unwrap()
    }
    #[test]
    fn cleanup_only_failure_is_retained_and_context_body_remains_primary() {
        Python::initialize();
        Python::attach(|py| {
            let mut stream = fixture();
            assert!(stream.shutdown_report().is_none());
            let first = stream.stop().unwrap_err();
            let audio_error = first.value(py).getattr("audio_error").unwrap();
            assert_eq!(
                audio_error
                    .getattr("kind")
                    .unwrap()
                    .extract::<String>()
                    .unwrap(),
                "deviceLost"
            );
            let second = stream.stop().unwrap_err();
            assert!(first.get_type(py).is(second.get_type(py)));
            let report = stream.shutdown_report().unwrap().0;
            assert!(report.primary().is_none());
            assert_eq!(report.cleanup().len(), 1);
            assert_eq!(report.cleanup()[0].kind(), fa::ErrorKind::DeviceLost);
            assert!(stream.terminal_error().is_none());
            assert_eq!(
                stream
                    .poll_event()
                    .unwrap()
                    .to_dict(py)
                    .unwrap()
                    .get_item("type")
                    .unwrap()
                    .unwrap()
                    .extract::<String>()
                    .unwrap(),
                "shutdownError"
            );
            assert!(stream.poll_event().is_none());
            let locals = PyDict::new(py);
            locals
                .set_item("stream", Py::new(py, fixture()).unwrap())
                .unwrap();
            py.run(
                c_str!(
                    r#"
try:
    with stream:
        raise KeyError('body')
except KeyError as body:
    assert str(body) == "'body'"
    assert body.__context__.audio_error.kind == 'deviceLost'
else:
    raise AssertionError('body error was lost')
"#
                ),
                None,
                Some(&locals),
            )
            .unwrap();
            locals
                .set_item("stream", Py::new(py, fixture()).unwrap())
                .unwrap();
            py.run(
                c_str!(
                    r#"
try:
    with stream:
        pass
except RuntimeError as cleanup:
    assert cleanup.audio_error.kind == 'deviceLost'
else:
    raise AssertionError('normal context exit hid cleanup failure')
"#
                ),
                None,
                Some(&locals),
            )
            .unwrap();
        });
    }
}
