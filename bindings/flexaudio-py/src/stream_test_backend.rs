//! Device-free integration adapter, compiled only with the test-faults feature.
use super::*;

pub(super) fn backend() -> Box<dyn fa::CaptureBackend> {
    Box::new(fa::MockBackend::new(48_000, 2, 0.0))
}

pub(super) fn switch(inner: &mut fa::Stream, denoise: Option<bool>) -> fa::Result<()> {
    match denoise {
        Some(enabled) => inner.switch_backend_with_denoise(backend(), enabled),
        None => inner.switch_backend(backend()),
    }
}

impl Stream {
    pub(super) fn test_source_kind<'a>(&self, kind: &'a str) -> PyResult<&'a str> {
        if kind != "mock" {
            return Ok(kind);
        }
        if self.inner.effective_settings().is_some() {
            return Err(to_py_err(fa::Error::InvalidArg(
                "mock source switches require a mock stream".into(),
            )));
        }
        Ok("mic")
    }
}
