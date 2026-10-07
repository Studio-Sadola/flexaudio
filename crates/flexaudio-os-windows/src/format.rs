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
