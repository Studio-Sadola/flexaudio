//! Refresh endpoint formats before sink construction, and validate startup races.

use std::cell::Cell;

use flexaudio_core::types::{Error, Result};

/// Query a stopped classic backend on every open; active and process-loopback
/// backends keep their configured format. Query failure leaves the last known
/// value available, but the actual open must still validate the device/format.
pub(crate) fn native_format_from_source(
    cached: &Cell<(u32, u16)>,
    refresh: bool,
    source: impl FnOnce() -> Option<(u32, u16)>,
) -> (u32, u16) {
    if refresh {
        if let Some(format) = source() {
            cached.set(format);
        }
    }
    cached.get()
}

/// Legacy format getters cannot return errors. Keep the query cause until a
/// checked start, rather than silently using the cache. A successful retry must
/// not erase failures that the legacy getter could not return.
pub(crate) fn retain_format_query(
    failure: &std::cell::RefCell<Option<Error>>,
    queried: Result<(u32, u16)>,
) -> Option<(u32, u16)> {
    match queried {
        Ok(format) => Some(format),
        Err(error) => {
            flexaudio_core::terminal_causes::retain_terminal_cause(
                &mut failure.borrow_mut(),
                error,
            );
            None
        }
    }
}

/// Return every failure already observed before attempting native startup.
pub(crate) fn check_start_failures<T>(
    pending: &mut Option<Error>,
    queried: &std::cell::RefCell<Option<Error>>,
    request: Result<T>,
) -> Result<T> {
    let mut retained = pending.take();
    if let Some(error) = queried.borrow_mut().take() {
        flexaudio_core::terminal_causes::retain_terminal_cause(&mut retained, error);
    }
    match request {
        Ok(value) => retained.map_or(Ok(value), Err),
        Err(error) => {
            flexaudio_core::terminal_causes::retain_terminal_cause(&mut retained, error);
            Err(retained.expect("failed request is retained"))
        }
    }
}

/// The sink and negotiated endpoint format must describe the same frames.
pub(crate) fn verify_format(actual: (u32, u16), expected: (u32, u16)) -> Result<()> {
    if actual != expected || actual.0 == 0 || actual.1 == 0 {
        return Err(Error::UnsupportedFormat(format!(
            "WASAPI mix format {actual:?} does not match configured sink {expected:?}; reopen capture with the current endpoint format"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flexaudio_core::backend::{CaptureBackend, RawSink};
    use flexaudio_core::raw_ring;
    use std::sync::{Arc, Mutex};

    /// Uses the same format refresh and startup check as the Windows adapter,
    /// with an injected endpoint and no OS audio objects.
    struct Backend {
        source: Arc<Mutex<Option<(u32, u16)>>>,
        cached: Cell<(u32, u16)>,
        sink: Option<RawSink>,
    }

    impl CaptureBackend for Backend {
        fn native_format(&self) -> (u32, u16) {
            native_format_from_source(&self.cached, self.sink.is_none(), || {
                *self.source.lock().unwrap()
            })
        }

        fn start(&mut self, sink: RawSink) -> Result<()> {
            let actual = self.source.lock().unwrap().ok_or(Error::DeviceNotFound)?;
            verify_format(actual, (sink.native_rate(), sink.native_channels()))?;
            self.sink = Some(sink);
            Ok(())
        }

        fn stop(&mut self) {
            self.sink = None;
        }
    }

    fn sink_for(backend: &dyn CaptureBackend) -> RawSink {
        // Mirrors the facade's open_backend_once ordering: query, build sink,
        // then start. A stale query causes the subsequent start to fail.
        let (rate, channels) = backend.native_format();
        let (producer, _consumer) = raw_ring(1);
        RawSink::new(producer, rate, channels)
    }

    #[test]
    fn reopen_refreshes_changed_endpoint_and_configures_the_new_sink() {
        let source = Arc::new(Mutex::new(Some((48_000, 2))));
        let mut backend = Backend {
            source: source.clone(),
            cached: Cell::new((48_000, 2)),
            sink: None,
        };
        backend.start(sink_for(&backend)).unwrap();
        *source.lock().unwrap() = Some((44_100, 2));
        assert_eq!(backend.native_format(), (48_000, 2));
        backend.stop();

        backend.start(sink_for(&backend)).unwrap();
        let sink = backend.sink.as_ref().unwrap();
        assert_eq!((sink.native_rate(), sink.native_channels()), (44_100, 2));
        assert_eq!(backend.native_format(), (44_100, 2));
        backend.stop();
    }

    #[test]
    fn format_race_fails_closed_and_a_later_reopen_refreshes_again() {
        let source = Arc::new(Mutex::new(Some((48_000, 2))));
        let mut backend = Backend {
            source: source.clone(),
            cached: Cell::new((48_000, 2)),
            sink: None,
        };
        let old_sink = sink_for(&backend);
        *source.lock().unwrap() = Some((44_100, 1));
        assert!(matches!(
            backend.start(old_sink),
            Err(Error::UnsupportedFormat(_))
        ));
        assert!(backend.sink.is_none());
        backend.start(sink_for(&backend)).unwrap();
        let sink = backend.sink.as_ref().unwrap();
        assert_eq!((sink.native_rate(), sink.native_channels()), (44_100, 1));
        backend.stop();
    }

    #[test]
    fn unavailable_source_retains_cache_and_fixed_formats_never_query() {
        let cached = Cell::new((44_100, 2));
        assert_eq!(
            native_format_from_source(&cached, true, || None),
            (44_100, 2)
        );
        assert_eq!(
            native_format_from_source(&cached, false, || panic!("must not query")),
            (44_100, 2)
        );
    }

    #[test]
    fn negotiated_rate_and_channels_must_match_sink() {
        verify_format((48_000, 2), (48_000, 2)).unwrap();
        for actual in [(44_100, 2), (48_000, 1), (0, 2), (48_000, 0)] {
            assert!(matches!(
                verify_format(actual, (48_000, 2)),
                Err(Error::UnsupportedFormat(_))
            ));
        }
    }
}

#[cfg(test)]
mod query_tests {
    use super::*;

    #[test]
    fn native_format_query_failure_survives_success_until_checked_start() {
        let failure = std::cell::RefCell::new(None);
        let cause = Error::Backend("native query unavailable; retry capture".into());
        assert_eq!(retain_format_query(&failure, Err(cause.clone())), None);
        assert_eq!(*failure.borrow(), Some(cause.clone()));
        assert_eq!(
            retain_format_query(&failure, Ok((44_100, 2))),
            Some((44_100, 2))
        );
        assert_eq!(
            check_start_failures(&mut None, &failure, Ok(())),
            Err(cause)
        );
        assert!(failure.borrow().is_none());
    }

    #[test]
    fn distinct_format_query_failures_survive_a_successful_retry() {
        flexaudio_core::test_support::assert_distinct_failures_retained(
            Error::DeviceNotFound,
            Error::Backend("later format query failure".into()),
            |first, second| {
                let failure = std::cell::RefCell::new(None);
                assert!(retain_format_query(&failure, Err(first)).is_none());
                assert!(retain_format_query(&failure, Err(second)).is_none());
                assert!(retain_format_query(&failure, Ok((48_000, 2))).is_some());
                check_start_failures(&mut None, &failure, Ok(())).unwrap_err()
            },
        );
    }

    #[test]
    fn startup_retains_pending_query_and_invalid_request_failures() {
        flexaudio_core::test_support::assert_distinct_failures_retained(
            Error::DeviceLost,
            Error::Backend("format query failed".into()),
            |first, second| {
                let invalid = Error::InvalidArg("invalid exclusion request".into());
                let error = check_start_failures::<()>(
                    &mut Some(first),
                    &std::cell::RefCell::new(Some(second)),
                    Err(invalid.clone()),
                )
                .unwrap_err();
                assert!(flexaudio_core::terminal_causes::retains_terminal_cause(
                    &error, &invalid
                ));
                error
            },
        );
    }
}
