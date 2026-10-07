//! Device-free capture lifecycle and wait-result validation.

use flexaudio_core::types::{Error, Result};

/// Add startup context without erasing errors callers handle by variant.
pub(crate) fn keepalive_error(context: &str, error: Error) -> Error {
    match error {
        Error::Backend(message) => Error::Backend(format!("{context}: {message}")),
        error => error,
    }
}

/// Operations used by the owner thread; implementations own their OS resources.
pub(crate) trait StreamClient {
    fn start(&mut self) -> Result<()>;
    fn stop(&mut self) -> Result<()>;
    fn fill_silence(&mut self) -> Result<()>;
}

/// Keeps render running throughout capture, including startup and teardown.
pub(crate) struct Session<C: StreamClient, K: StreamClient> {
    capture: C,
    keepalive: Option<K>,
    capture_started: bool,
    render_started: bool,
}

impl<C: StreamClient, K: StreamClient> Session<C, K> {
    /// Report readiness only after both clients start. Failed startup rolls back
    /// before returning, and losing the receiver also stops both clients.
    pub(crate) fn start_and_report(
        capture: C,
        keepalive: Option<K>,
        report: impl FnOnce(Result<()>) -> bool,
    ) -> Option<Self> {
        let mut session = Self {
            capture,
            keepalive,
            capture_started: false,
            render_started: false,
        };
        let start = (|| {
            if let Some(render) = session.keepalive.as_mut() {
                render.fill_silence().map_err(|error| {
                    keepalive_error("cannot prime classic loopback silent keepalive", error)
                })?;
                render.start()?;
                session.render_started = true;
            }
            session.capture.start()?;
            session.capture_started = true;
            Ok(())
        })();
        if let Err(error) = start {
            let _ = session.stop();
            report(Err(error));
            return None;
        }
        if !report(Ok(())) {
            return None;
        }
        Some(session)
    }

    pub(crate) fn service_keepalive(&mut self) -> Result<()> {
        if let Some(render) = self.keepalive.as_mut() {
            render.fill_silence()?;
        }
        Ok(())
    }

    /// Attempt both stops even when capture stop fails; preserve the first error.
    pub(crate) fn stop(&mut self) -> Result<()> {
        let capture = if self.capture_started {
            self.capture_started = false;
            self.capture.stop()
        } else {
            Ok(())
        };
        let render = if self.render_started {
            self.render_started = false;
            self.keepalive
                .as_mut()
                .expect("started render exists")
                .stop()
        } else {
            Ok(())
        };
        capture.and(render)
    }
}

impl<C: StreamClient, K: StreamClient> Drop for Session<C, K> {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// Validate Win32 wait values without calling Win32. Timeout is a normal wakeup
/// for stop polling; abandoned, failed, and unknown results fail closed.
pub(crate) fn check_wait_result(value: u32, handles: u32) -> Result<()> {
    const WAIT_TIMEOUT: u32 = 258;
    const WAIT_ABANDONED_0: u32 = 128;
    const WAIT_FAILED: u32 = u32::MAX;
    if value < handles || value == WAIT_TIMEOUT {
        return Ok(());
    }
    let reason = if value == WAIT_FAILED {
        "WAIT_FAILED"
    } else if (WAIT_ABANDONED_0..WAIT_ABANDONED_0 + handles).contains(&value) {
        "abandoned handle"
    } else {
        "unexpected result"
    };
    Err(Error::Backend(format!(
        "WASAPI event wait: {reason} ({value:#x})"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    type Trace = Rc<RefCell<Vec<String>>>;

    struct FakeClient {
        name: &'static str,
        trace: Trace,
        fail: Option<&'static str>,
    }

    impl FakeClient {
        fn operation(&self, op: &str) -> Result<()> {
            let entry = format!("{}.{}", self.name, op);
            self.trace.borrow_mut().push(entry.clone());
            if self.fail == Some(op) {
                Err(Error::Backend(entry))
            } else {
                Ok(())
            }
        }
    }

    impl StreamClient for FakeClient {
        fn start(&mut self) -> Result<()> {
            self.operation("start")
        }
        fn stop(&mut self) -> Result<()> {
            self.operation("stop")
        }
        fn fill_silence(&mut self) -> Result<()> {
            self.operation("fill")
        }
    }

    fn client(name: &'static str, trace: &Trace, fail: Option<&'static str>) -> FakeClient {
        FakeClient {
            name,
            trace: trace.clone(),
            fail,
        }
    }

    #[test]
    fn render_surrounds_capture_and_readiness_follows_start() {
        let trace = Trace::default();
        let mut session = Session::start_and_report(
            client("capture", &trace, None),
            Some(client("render", &trace, None)),
            |r| {
                r.unwrap();
                trace.borrow_mut().push("ready".into());
                true
            },
        )
        .unwrap();
        session.service_keepalive().unwrap();
        session.stop().unwrap();
        session.stop().unwrap();
        drop(session);
        assert_eq!(
            *trace.borrow(),
            [
                "render.fill",
                "render.start",
                "capture.start",
                "ready",
                "render.fill",
                "capture.stop",
                "render.stop"
            ]
        );
    }

    #[test]
    fn failed_capture_start_rolls_back_render_before_reporting_error() {
        let trace = Trace::default();
        let session = Session::start_and_report(
            client("capture", &trace, Some("start")),
            Some(client("render", &trace, None)),
            |r| {
                assert!(r.is_err());
                trace.borrow_mut().push("error".into());
                true
            },
        );
        assert!(session.is_none());
        assert_eq!(
            *trace.borrow(),
            [
                "render.fill",
                "render.start",
                "capture.start",
                "render.stop",
                "error"
            ]
        );
    }

    #[test]
    fn render_fill_or_start_failure_never_starts_capture() {
        for fail in ["fill", "start"] {
            let trace = Trace::default();
            let session = Session::start_and_report(
                client("capture", &trace, None),
                Some(client("render", &trace, Some(fail))),
                |r| {
                    assert!(r.is_err());
                    true
                },
            );
            assert!(session.is_none());
            assert!(!trace.borrow().iter().any(|s| s.starts_with("capture.")));
        }
    }

    #[test]
    fn disconnected_readiness_receiver_stops_both_clients() {
        let trace = Trace::default();
        assert!(Session::start_and_report(
            client("capture", &trace, None),
            Some(client("render", &trace, None)),
            |_| false,
        )
        .is_none());
        assert_eq!(
            *trace.borrow(),
            [
                "render.fill",
                "render.start",
                "capture.start",
                "capture.stop",
                "render.stop"
            ]
        );
    }

    #[test]
    fn process_capture_has_no_render_operations() {
        let trace = Trace::default();
        let session = Session::<_, FakeClient>::start_and_report(
            client("capture", &trace, None),
            None,
            |r| {
                r.unwrap();
                true
            },
        )
        .unwrap();
        drop(session);
        assert_eq!(*trace.borrow(), ["capture.start", "capture.stop"]);
    }

    #[test]
    fn runtime_render_failure_is_returned_and_cleanup_remains_ordered() {
        let trace = Trace::default();
        let mut session = Session::start_and_report(
            client("capture", &trace, None),
            Some(client("render", &trace, None)),
            |_| true,
        )
        .unwrap();
        session.keepalive.as_mut().unwrap().fail = Some("fill");
        assert!(session.service_keepalive().is_err());
        drop(session);
        assert_eq!(
            &trace.borrow()[3..],
            ["render.fill", "capture.stop", "render.stop"]
        );
    }

    #[test]
    fn capture_stop_failure_still_stops_render_once() {
        let trace = Trace::default();
        let mut session = Session::start_and_report(
            client("capture", &trace, Some("stop")),
            Some(client("render", &trace, None)),
            |_| true,
        )
        .unwrap();
        assert!(session.stop().is_err());
        drop(session);
        assert_eq!(&trace.borrow()[3..], ["capture.stop", "render.stop"]);
    }

    #[test]
    fn wait_signals_and_timeout_are_normal() {
        for result in [0, 1, 258] {
            check_wait_result(result, 2).unwrap();
        }
    }

    #[test]
    fn wait_failures_abandonment_and_unknown_values_are_errors() {
        for result in [u32::MAX, 128, 129, 2, 300] {
            assert!(matches!(
                check_wait_result(result, 2),
                Err(Error::Backend(_))
            ));
        }
    }

    #[test]
    fn keepalive_context_preserves_typed_errors() {
        let denied = Error::PermissionDenied {
            permission: flexaudio_core::types::Permission::SystemAudio,
            detail: "keepalive access denied".into(),
        };
        assert_eq!(keepalive_error("create keepalive", denied.clone()), denied);
        assert!(matches!(
            keepalive_error("create keepalive", Error::DeviceNotFound),
            Error::DeviceNotFound
        ));
        assert!(matches!(
            keepalive_error("create keepalive", Error::UnsupportedFormat("format".into())),
            Error::UnsupportedFormat(message) if message == "format"
        ));
    }

    #[test]
    fn keepalive_context_enriches_backend_messages() {
        assert!(matches!(
            keepalive_error("create keepalive", Error::Backend("Initialize failed".into())),
            Error::Backend(message) if message == "create keepalive: Initialize failed"
        ));
    }
}
