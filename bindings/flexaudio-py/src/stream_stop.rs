//! The single retained shutdown path for explicit stop, context exit, and drop.
use super::*;

impl Stream {
    pub(super) fn finish_shutdown(&mut self) -> fa::Result<()> {
        if let Some(report) = &self.shutdown {
            return report.result();
        }
        let _ = self.inner.stop_checked();
        let core_report = self.inner.shutdown_report();
        let primary = self.inner.terminal_error();
        let mut cleanup = core_report
            .map(|r| r.cleanup().to_vec())
            .unwrap_or_default();
        let before = cleanup.len();
        self.drain_capture();
        if primary.is_none() {
            if self.drain_output().is_err() {
                cleanup.push(flush_error("binding output drain failed"));
            }
            let samples = if self.output_end.is_some() {
                self.denoiser
                    .as_mut()
                    .map(CoreDenoiser::flush)
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            let mut tail = self.final_chunk(samples);
            if let Some(vad) = self.vad.as_mut() {
                let process =
                    vad.process_pcm(tail.samples(), self.output_rate, self.output_channels);
                match process {
                    Ok(events) => tail.set_vad_events(vad_event_pairs(events)),
                    Err(_) => cleanup.push(flush_error("binding VAD tail processing failed")),
                }
                match vad.flush() {
                    Ok(events) => tail.extend_vad_events(vad_event_pairs(events)),
                    Err(_) => cleanup.push(flush_error("binding VAD flush failed")),
                }
            }
            if tail.has_audio_or_vad() {
                self.ready_chunks.push_back(tail);
            }
        } else {
            self.ready_chunks.retain(|chunk| chunk.samples().is_empty());
        }
        if let Some(whisper) = self.whisper.as_mut() {
            let result = whisper.stop();
            if result.is_err() {
                cleanup.push(flush_error("binding whisper VAD shutdown failed"));
            }
            self.accept_whisper(result);
            self.whisper_carrier();
        }
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

    fn final_chunk(&self, samples: Vec<f32>) -> PyAudioChunk {
        let (frame_index, pts_ns, next_seq, dropped_before) =
            self.output_end.unwrap_or((0, 0, 0, 0));
        let mut chunk = chunk_to_py(fa::AudioChunk {
            seq: if samples.is_empty() {
                next_seq.saturating_sub(1)
            } else {
                next_seq
            },
            frames: samples.len() / usize::from(self.output_channels),
            data: samples,
            frame_index,
            pts_ns,
            dropped_before,
            flags: fa::ChunkFlags::empty(),
            peak: 0.0,
            rms: 0.0,
        });
        chunk.update_metrics();
        chunk
    }
}
fn flush_error(message: &str) -> fa::Error {
    fa::Error::Backend(message.into()).with_context(fa::ErrorContext::new(fa::Operation::Flush))
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
            output_end: None,
            whisper: None,
            whisper_events: Vec::new(),
            whisper_origin: (0, 0),
            whisper_error: None,
            whisper_error_reported: false,
            ready_chunks: Default::default(),
            denoiser: None,
            vad: None,
            output_rate: 48_000,
            output_channels: 2,
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
