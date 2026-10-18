//! One checked binding teardown, shared by explicit stop and free.
use crate::types::FlexStream;
use flexaudio::{ErrorContext, Operation, ShutdownReport};

impl FlexStream {
    pub(crate) fn poll_binding_event(&mut self) -> Option<flexaudio::Event> {
        if let Some(event) = self.inner.poll_event() {
            return Some(event);
        }
        let error = self
            .shutdown
            .as_ref()?
            .cleanup()
            .get(self.shutdown_event_index)?
            .clone();
        self.shutdown_event_index += 1;
        Some(flexaudio::Event::ShutdownError { error })
    }

    pub(crate) fn stop_binding(&mut self) -> flexaudio::Result<()> {
        if let Some(report) = &self.shutdown {
            return report.result();
        }
        let _ = self.inner.stop_checked();
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
        cleanup.extend(self.vad_cleanup.iter().cloned());
        let report = ShutdownReport::new(core.primary().cloned(), cleanup);
        let result = report.result();
        self.shutdown = Some(report);
        result
    }
}
