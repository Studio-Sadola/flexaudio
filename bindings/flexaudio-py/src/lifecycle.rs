//! Separate helper lifecycle projection, never passed to the ordinary event mapper.
use ::flexaudio as fa;
use pyo3::prelude::*;

#[pyclass(module = "flexaudio", name = "LifecycleEvent", frozen)]
pub(crate) struct LifecycleEvent {
    #[pyo3(get, name = "type")]
    kind: &'static str,
    #[pyo3(get)]
    reason: Option<&'static str>,
    #[pyo3(get)]
    message: Option<&'static str>,
}

pub(crate) fn from_event(event: &fa::Event) -> Option<LifecycleEvent> {
    let (kind, reason) = match event {
        fa::Event::SilenceProbeStarted => ("silenceProbeStarted", None),
        fa::Event::SilenceProbeStopped { reason } => ("silenceProbeStopped", Some(reason.as_str())),
        fa::Event::ClassicLoopbackKeepaliveStarted => ("classicLoopbackKeepaliveStarted", None),
        fa::Event::ClassicLoopbackKeepaliveStopped { reason } => {
            ("classicLoopbackKeepaliveStopped", Some(reason.as_str()))
        }
        _ => return None,
    };
    Some(LifecycleEvent {
        kind,
        reason,
        message: None,
    })
}
