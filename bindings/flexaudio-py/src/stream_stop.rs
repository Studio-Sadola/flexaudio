//! The single retained shutdown path for explicit stop, context exit, and drop.
use super::*;

impl Stream {
    pub(super) fn finish_shutdown(&mut self) -> fa::Result<()> {
        if let Some(report) = &self.shutdown {
            return report.result();
        }
        let _ = self.inner.stop_checked();
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
        let result = report.result();
        self.shutdown = Some(report);
        result
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        let _ = self.finish_shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pyo3::ffi::c_str;
    use pyo3::types::PyDict;

    struct CleanupFailure;
    impl fa::CaptureBackend for CleanupFailure {
        fn native_format(&self) -> (u32, u16) {
            (48_000, 2)
        }
        fn start(&mut self, _sink: fa::core::backend::RawSink) -> fa::Result<()> {
            Ok(())
        }
        fn stop(&mut self) {}
        fn stop_checked(&mut self) -> fa::Result<()> {
            Err(fa::Error::DeviceLost.with_context(fa::ErrorContext::new(fa::Operation::Stop)))
        }
    }
    fn fixture() -> Stream {
        let mut inner =
            fa::Stream::open(fa::StreamConfig::default(), Box::new(CleanupFailure)).unwrap();
        inner.start().unwrap();
        Stream {
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
        }
    }
    #[test]
    fn cleanup_only_failure_is_retained_and_context_body_remains_primary() {
        Python::initialize();
        Python::attach(|py| {
            let mut stream = fixture();
            assert!(stream.shutdown_report().is_none());
            let first = stream.stop().unwrap_err();
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
