//! One checked binding teardown, shared by explicit stop and free.
use crate::types::FlexStream;
use flexaudio::{ErrorContext, Operation, ShutdownReport};

impl FlexStream {
    /// stream must be live; non-event operations must not overlap this call.
    pub(crate) unsafe fn poll_binding_event(stream: *mut Self) -> Option<flexaudio::Event> {
        let mut queues = (*stream).events.lock().unwrap_or_else(|e| e.into_inner());
        Self::drain_binding_events(stream, &mut queues);
        if let Some(event) = queues.ordinary.pop_front() {
            return Some(event);
        }
        let error = (*stream)
            .shutdown
            .as_ref()?
            .cleanup()
            .get((*stream).shutdown_event_index)?
            .clone();
        (*stream).shutdown_event_index += 1;
        Some(flexaudio::Event::ShutdownError { error })
    }

    pub(crate) fn stop_binding(&mut self) -> flexaudio::Result<()> {
        self.retain_shutdown_outcome();
        self.shutdown
            .as_ref()
            .expect("completed C shutdown")
            .result()
    }

    fn retain_shutdown_outcome(&mut self) {
        if self.shutdown.is_some() {
            return;
        }
        self.inner.stop();
        let core = self
            .inner
            .shutdown_report()
            .expect("checked stop retains report");
        let mut cleanup = core.cleanup().to_vec();
        self.shutdown_event_index = cleanup.len();
        if core.primary().is_none() {
            if self.vad_tap.is_some() {
                self.stop_vad_tap();
            } else {
                // Collect first, then queue: polling also reads the retained delivery queue.
                let mut chunks = Vec::new();
                loop {
                    match self.poll_processed() {
                        Ok(Some(chunk)) => chunks.push(chunk),
                        Ok(None) => break,
                        Err(error) => {
                            cleanup.push(
                                error
                                    .audio_error()
                                    .with_context(ErrorContext::new(Operation::Flush)),
                            );
                            break;
                        }
                    }
                }
                for chunk in chunks {
                    let chunk = self.versioned_chunk(chunk);
                    self.ready_chunks.push_back(chunk);
                }
            }
        } else {
            // Capture primary suppresses PCM while closing attached event epochs.
            self.stop_vad_tap();
        }
        self.queue_secondary_input();
        cleanup.extend(self.vad_state.cleanup_errors());
        let report = ShutdownReport::new(core.primary().cloned(), cleanup);
        self.shutdown = Some(report);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn facade_stop_failure_reaches_c_status_and_owned_shutdown_report() {
        let inner = flexaudio::test_support::stop_failure(flexaudio::test_support::StopPhase::Stop)
            .unwrap();
        let mut stream = FlexStream {
            events: Default::default(),
            inner,
            secondary: None,
            shutdown: None,
            shutdown_event_index: 0,
            vad_tap: None,
            vad_events: Vec::new(),
            vad_origin: (0, 0),
            vad_state: Default::default(),
            ready_chunks: Default::default(),
        };
        // The C caller receives both its legacy status and a separately owned typed report.
        unsafe {
            assert_eq!(
                crate::flexaudio_stop(&mut stream),
                crate::code::FLEX_DEVICE_LOST
            );
            let report = crate::v2::flexaudio_shutdown_report_v2(&stream);
            assert!(!report.is_null());
            assert!(crate::v2::flexaudio_shutdown_primary_v2(report).is_null());
            let mut count = 0;
            assert_eq!(
                crate::v2::flexaudio_shutdown_cleanup_count_v2(report, &mut count),
                1
            );
            assert_eq!(count, 1);
            let cleanup = crate::v2::flexaudio_shutdown_cleanup_v2(report, 0);
            assert!(!cleanup.is_null());
            let mut kind = 0;
            assert_eq!(crate::v2::flexaudio_error_kind_v2(cleanup, &mut kind), 1);
            assert_eq!(kind, crate::code::FLEX_DEVICE_LOST);
            crate::v2::flexaudio_shutdown_report_free_v2(report);
            let retained = stream.shutdown.clone();
            assert_eq!(
                crate::flexaudio_stop(&mut stream),
                crate::code::FLEX_DEVICE_LOST
            );
            assert_eq!(stream.shutdown, retained);
        }
    }
}
