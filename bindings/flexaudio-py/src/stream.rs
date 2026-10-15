//! Recording stream [`Stream`], the `open()` entry point, and integrated VAD / denoise.
//!
//! Python's [`Stream`] is pull/poll based. It has no bridge thread like napi; the caller periodically
//! invokes `poll_chunk` / `poll_event`. Integrated VAD / denoise processing also runs when
//! poll_chunk is called (under the GIL; process on demand).

#[path = "stream_stop.rs"]
mod shutdown;
#[path = "stream_vad.rs"]
mod vad_capture;

use pyo3::prelude::*;

use ::flexaudio as fa;
use flexaudio_denoise::Denoiser as CoreDenoiser;

use crate::config::{build_config, parse_exclude_pids, validate_denoise};
use crate::marshal::{chunk_to_py, event_to_py, PyAudioChunk, PyStreamEvent};
use crate::{denoise_err_to_py, to_py_err};

/// Recording stream handle. Polls the internal `flexaudio::Stream` directly.
///
/// `open(...)` returns after calling `start()`. The caller periodically invokes `poll_chunk` /
/// `poll_event`. Stop with `stop()`; a context manager (`with`) calls stop from `__exit__`.
///
/// Integrated addons:
/// - Open with `denoise=True` to overwrite chunk audio with RNNoise before `poll_chunk` returns
///   (48kHz output only; the first 480 samples/channel are silent due to denoise latency).
/// - Open with `vad=StreamVadOptions(...)` to attach VAD to canonical capture.
///   Ordered events and epoch origins are available through `chunk.vad_events`.
///
/// The rubato resampler held by integrated VAD is !Sync, so it cannot satisfy pyclass's default
/// Send+Sync requirement. Python uses polling on a single thread under the GIL, so mark the class
/// unsendable and keep it on the creating thread (also when VAD is disabled).
#[pyclass(module = "flexaudio", unsendable)]
pub struct Stream {
    inner: fa::Stream,
    shutdown: Option<fa::ShutdownReport>,
    local_events: std::collections::VecDeque<fa::Event>,
    output_end: Option<(u64, i64, u64, u32)>,
    vad_tap: Option<flexaudio_vad::VadTap>,
    vad_events: Vec<flexaudio_vad::AttachedVadEvent>,
    vad_origin: (u64, i64),
    vad_error: Option<flexaudio_vad::VadTapError>,
    vad_cleanup: Vec<fa::Error>,
    ready_chunks: std::collections::VecDeque<PyAudioChunk>,
    // Integrated denoise (requires 48kHz). None when disabled.
    denoiser: Option<CoreDenoiser>,
    // Output format. Passed to VAD's process_pcm and used to validate denoise's 48kHz requirement.
    // switch_source cannot change the output format, so this remains the value from open.
    output_rate: u32,
    output_channels: u16,
}

impl Stream {
    fn build_denoiser(
        denoise: bool,
        output_rate: u32,
        output_channels: u16,
    ) -> PyResult<Option<CoreDenoiser>> {
        validate_denoise(denoise, output_rate)?;
        if denoise {
            CoreDenoiser::new(output_channels)
                .map(Some)
                .map_err(denoise_err_to_py)
        } else {
            Ok(None)
        }
    }
    fn switch_with_addons(
        &mut self,
        new_denoiser: Option<CoreDenoiser>,
        switch: impl FnOnce(&mut fa::Stream) -> fa::Result<()>,
    ) -> PyResult<()> {
        if self.vad_tap.is_none() {
            // Drain bounded published output and close retained local addon state.
            // Core owns in-flight generations: later old PCM remains deliverable
            // through the new owner and core discontinuity anchors. Leave the user's
            // pause state untouched; pausing would discard core startup prefixes.
            self.drain_capture();
            self.drain_output()?;
            self.finish_addon_gap().map_err(to_py_err)?;
        }
        switch(&mut self.inner).map_err(to_py_err)?;
        // Native switching remains in core; only successful switches replace addon owners.
        self.denoiser = if self.vad_tap.is_some() {
            None
        } else {
            new_denoiser
        };
        Ok(())
    }

    fn poll_processed_chunk(&mut self) -> PyResult<Option<PyAudioChunk>> {
        self.drain_capture();
        if let Some(error) = self.inner.terminal_error() {
            return Err(to_py_err(error));
        }
        let Some(mut chunk) = self.inner.poll_chunk() else {
            return Ok(None);
        };
        let output = self.inner.config().output;
        self.output_end = Some((
            chunk
                .frame_index
                .saturating_add(chunk.frames as u64 * 48_000 / u64::from(output.sample_rate)),
            chunk.pts_ns.saturating_add(
                chunk.frames as i64 * 1_000_000_000 / i64::from(output.sample_rate),
            ),
            chunk.seq.saturating_add(1),
            chunk.dropped_before,
        ));
        self.vad_origin.1 = self.vad_origin.1.max(chunk.pts_ns);
        if let Some(denoiser) = self.denoiser.as_mut() {
            if chunk.flags.contains(fa::ChunkFlags::DISCONTINUITY) {
                denoiser.reset();
            }
            denoiser
                .process(&mut chunk.data)
                .map_err(denoise_err_to_py)?;
        }
        let mut result = chunk_to_py(chunk);
        result.update_metrics();
        if self.vad_tap.is_some() {
            result.set_vad_events(std::mem::take(&mut self.vad_events));
        }
        Ok(Some(result))
    }
}

#[pymethods]
impl Stream {
    /// Stop once, drain graceful addon tails, and raise any retained capture or cleanup failure.
    /// The stopped stream is spent; open a new stream to capture again.
    fn stop(&mut self) -> PyResult<()> {
        self.finish_shutdown().map_err(to_py_err)
    }

    /// Final capture primary and ordered cleanup failures; absent until shutdown completes.
    fn shutdown_report(&self) -> Option<crate::errors::ShutdownReport> {
        self.shutdown.clone().map(crate::errors::ShutdownReport)
    }

    /// Flush an attached VAD epoch. Disabled attachment is a no-op.
    /// Raise a typed failure immediately; poll_chunk still delivers queued closing events.
    fn flush_vad(&mut self) -> PyResult<()> {
        self.flush_vad_tap_with(flexaudio_vad::VadTap::flush)
    }

    /// Pause delivery without stopping recording. Resume with `resume`.
    fn pause(&self) {
        self.inner.pause();
    }

    /// End the pause and resume delivery.
    fn resume(&self) -> PyResult<()> {
        self.inner.resume().map_err(to_py_err)
    }

    /// Return whether delivery is paused.
    fn is_paused(&self) -> bool {
        self.inner.is_paused()
    }

    /// Change input gain (linear multiplier). 1.0 leaves audio unchanged, 2.0 is about +6dB, and 0.0
    /// is silence. May be called during recording and takes effect from the next chunk (20ms
    /// granularity). Samples are clamped to ±1.0 after multiplication. Must be finite and >= 0, or
    /// `ValueError` is raised.
    fn set_gain(&self, gain: f32) -> PyResult<()> {
        self.inner.set_gain(gain).map_err(to_py_err)
    }

    /// Return the current input gain (linear multiplier).
    fn gain(&self) -> f32 {
        self.inner.gain()
    }

    /// Return the source's native format `(sample_rate, channels)` (actual input before the first
    /// resampling stage). After a source switch, this reflects the new source.
    fn native_format(&self) -> (u32, u16) {
        self.inner.native_format()
    }

    /// Return the cumulative number of chunks discarded when the ring overflowed (since start).
    fn dropped_chunks(&self) -> u64 {
        self.inner.dropped_chunks()
    }

    /// Return a chunk if one is available. Otherwise return `None` (non-blocking).
    ///
    /// Raise RuntimeError on a terminal permission denial, even after stop.
    /// The permission event remains available through poll_event.
    ///
    /// Attached VAD events retain explicit epochs and canonical capture origins.
    /// Noise suppression updates delivered metrics before PCM is returned.
    fn poll_chunk(&mut self) -> PyResult<Option<PyAudioChunk>> {
        if let Some(error) = self.inner.terminal_error() {
            // Closing event carriers remain deliverable; captured PCM does not.
            self.ready_chunks.retain(|chunk| chunk.samples().is_empty());
            return match self.ready_chunks.pop_front() {
                Some(carrier) => Ok(Some(carrier)),
                None => Err(to_py_err(error)),
            };
        }
        if !self.ready_chunks.is_empty() {
            return Ok(self.ready_chunks.pop_front());
        }
        let result = match self.poll_processed_chunk() {
            Ok(chunk) => chunk,
            Err(error) => {
                self.vad_carrier();
                // Report this failure now; finalized event carriers remain queued for the next poll.
                return Err(error);
            }
        };
        if result.is_some() {
            return Ok(result);
        }
        self.vad_carrier();
        Ok(self.ready_chunks.pop_front())
    }

    /// Retained typed capture primary, including after stop; does not consume events.
    fn terminal_error(&self) -> Option<crate::errors::AudioError> {
        self.inner.terminal_error().map(crate::errors::AudioError)
    }

    /// Return an event if one is available. Otherwise return `None` (non-blocking).
    fn poll_event(&mut self) -> Option<PyStreamEvent> {
        self.inner
            .poll_event()
            .or_else(|| self.local_events.pop_front())
            .map(event_to_py)
    }

    /// Hot-swap the input source (mic/system/process/mix) without stopping recording.
    ///
    /// A switch cannot change the output format (output_rate/output_channels). If a change is
    /// requested, `switch_source` returns an error and an exception is raised here. `gain` is
    /// accepted but ignored by the core (gain is stream state; change it with `set_gain`).
    /// `mic_device_id`/`system_device_id`/`mic_gain`/`system_gain` are for mix only and ignored for
    /// other sources.
    ///
    /// `exclude_pids` accepts a sequence of integer PIDs in 1..=4294967295 (max 4096);
    /// values are validated even for mic/process sources before accessing any device.
    /// Applies to system capture and the system side of mix, alongside `exclude_self`.
    ///
    /// Specify `vad` / `denoise` again to configure integrated addons. A source change makes audio
    /// discontinuous, so rebuild addons as requested (reset their internal state). If omitted, defaults
    /// (`vad=None` / `denoise=False`) disable addons, as with open; specify them on each switch.
    #[pyo3(signature = (
        kind,
        *,
        device_id = None,
        process_id = None,
        mode = "include".to_string(),
        exclude_self = false,
        exclude_pids = None,
        output_rate = 48_000,
        output_channels = 2,
        chunk_ms = 20,
        gain = 1.0,
        mic_device_id = None,
        system_device_id = None,
        mic_gain = 1.0,
        system_gain = 1.0,
        denoise = false,
        silence_probe = true,
    ))]
    #[allow(clippy::too_many_arguments)]
    fn switch_source(
        &mut self,
        kind: &str,
        device_id: Option<String>,
        process_id: Option<u32>,
        mode: String,
        exclude_self: bool,
        exclude_pids: Option<Bound<'_, PyAny>>,
        output_rate: u32,
        output_channels: u16,
        chunk_ms: u32,
        gain: f32,
        mic_device_id: Option<String>,
        system_device_id: Option<String>,
        mic_gain: f32,
        system_gain: f32,
        denoise: bool,
        silence_probe: bool,
    ) -> PyResult<()> {
        if let Some(error) = self.inner.terminal_error() {
            return Err(to_py_err(error));
        }
        let exclude_pids = parse_exclude_pids(exclude_pids.as_ref())?;

        let mut config = build_config(
            kind,
            device_id,
            process_id,
            &mode,
            exclude_self,
            exclude_pids,
            output_rate,
            output_channels,
            chunk_ms,
            gain,
            mic_device_id,
            system_device_id,
            mic_gain,
            system_gain,
        )?;

        config.silence_probe = silence_probe;

        // Addons depend on output format. Since a switch cannot change it, validate and build using
        // output_rate/output_channels from open (the output_rate argument is used by core's
        // switch_source to check that formats match).
        let new_denoiser = Self::build_denoiser(denoise, self.output_rate, self.output_channels)?;
        let vad_tap = self.vad_tap.is_some();
        self.switch_with_addons(new_denoiser, |inner| {
            if vad_tap {
                inner.switch_source_with_denoise(config, denoise)
            } else {
                inner.switch_source(config)
            }
        })
    }

    /// Context manager support. Use as `with flexaudio.open("mic") as s:`.
    fn __enter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    /// Keep an active body exception primary and attach any shutdown failure as context.
    fn __exit__(
        &mut self,
        _exc_type: Option<Bound<'_, PyAny>>,
        exc_value: Option<Bound<'_, PyAny>>,
        _traceback: Option<Bound<'_, PyAny>>,
    ) -> PyResult<bool> {
        if let Err(error) = self.stop() {
            if let Some(body) = exc_value {
                let py = body.py();
                body.setattr("__context__", error.value(py))?;
            } else {
                return Err(error);
            }
        }
        Ok(false)
    }
}

/// Open and start a stream, then return [`Stream`].
///
/// `kind` is "mic"|"system"|"process"|"mix". Invalid values raise `ValueError`. If no device is
/// available, open / start raises a flexaudio error (converted to `RuntimeError`, etc.).
/// `mic_device_id`/`system_device_id`/`mic_gain`/`system_gain` are for mix only: they select the mic
/// and system devices and set pre-mix gain (ignored for other sources; global `gain` is applied after mixing).
///
/// `exclude_pids` accepts a sequence of integer PIDs in 1..=4294967295 (max 4096);
/// values are validated even for mic/process sources before accessing any device.
/// Applies to system capture and the system side of mix, alongside `exclude_self`.
///
/// `vad=StreamVadOptions(...)` attaches canonical VAD before output conversion.
/// `denoise=true` enables RNNoise and requires 48000 Hz output.
/// `silence_probe=false` disables the optional macOS silence diagnosis tone.
#[pyfunction]
#[pyo3(signature = (
    kind,
    *,
    device_id = None,
    process_id = None,
    mode = "include".to_string(),
    exclude_self = false,
    exclude_pids = None,
    output_rate = 48_000,
    output_channels = 2,
    chunk_ms = 20,
    gain = 1.0,
    mic_device_id = None,
    system_device_id = None,
    mic_gain = 1.0,
    system_gain = 1.0,
    denoise = false,
    silence_probe = true,
    vad = None,
))]
#[allow(clippy::too_many_arguments)]
pub fn open(
    kind: &str,
    device_id: Option<String>,
    process_id: Option<u32>,
    mode: String,
    exclude_self: bool,
    exclude_pids: Option<Bound<'_, PyAny>>,
    output_rate: u32,
    output_channels: u16,
    chunk_ms: u32,
    gain: f32,
    mic_device_id: Option<String>,
    system_device_id: Option<String>,
    mic_gain: f32,
    system_gain: f32,
    denoise: bool,
    silence_probe: bool,
    vad: Option<&crate::vad::StreamVadOptions>,
) -> PyResult<Stream> {
    let exclude_pids = parse_exclude_pids(exclude_pids.as_ref())?;
    let mut config = build_config(
        kind,
        device_id,
        process_id,
        &mode,
        exclude_self,
        exclude_pids,
        output_rate,
        output_channels,
        chunk_ms,
        gain,
        mic_device_id,
        system_device_id,
        mic_gain,
        system_gain,
    )?;
    config.silence_probe = silence_probe;
    let vad_tap = vad
        .map(|options| {
            flexaudio_vad::VadTap::new(
                options.params.inner.clone(),
                flexaudio_vad::VadRuntimeOptions {
                    provisional: options.provisional,
                },
            )
        })
        .transpose()
        .map_err(crate::vad::tap_error)?;

    // Validate and build addons before acquiring a device. Reject denoise unless the output rate is 48 kHz, and reject
    // invalid VAD settings, before acquiring a device.
    let denoiser = Stream::build_denoiser(denoise, output_rate, output_channels)?;

    let mut stream = fa::open(config).map_err(to_py_err)?;
    if vad_tap.is_some() {
        stream.enable_capture_tap().map_err(to_py_err)?;
        stream.set_denoise(denoise);
    }
    stream.start().map_err(to_py_err)?;
    Ok(Stream {
        vad_tap,
        vad_events: Vec::new(),
        vad_origin: (0, 0),
        vad_error: None,
        vad_cleanup: Vec::new(),
        ready_chunks: std::collections::VecDeque::new(),
        inner: stream,
        shutdown: None,
        local_events: Default::default(),
        output_end: None,
        denoiser: if vad.is_some() { None } else { denoiser },
        output_rate,
        output_channels,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pyo3::ffi::c_str;
    use pyo3::types::PyDict;

    struct DeniedBackend(Option<fa::Event>);

    impl fa::CaptureBackend for DeniedBackend {
        fn native_format(&self) -> (u32, u16) {
            (48_000, 2)
        }
        fn start(&mut self, _sink: fa::core::backend::RawSink) -> fa::Result<()> {
            Ok(())
        }
        fn stop(&mut self) {}
        fn poll_event(&mut self) -> Option<fa::Event> {
            self.0.take()
        }
    }

    fn denied_stream() -> fa::Stream {
        let mut stream = fa::Stream::open(
            fa::StreamConfig::default(),
            Box::new(DeniedBackend(Some(fa::Event::PermissionDenied {
                permission: fa::Permission::Microphone,
                detail: "denied by user".into(),
            }))),
        )
        .expect("open fake backend");
        stream.start().expect("start fake backend");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while stream.terminal_error().is_none() {
            assert!(
                std::time::Instant::now() < deadline,
                "terminal event was not processed"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        stream
    }

    #[test]
    fn polling_raises_terminal_error_without_consuming_event_after_stop() {
        Python::initialize();
        Python::attach(|py| {
            let mut stream = Stream {
                vad_tap: None,
                vad_events: Vec::new(),
                vad_origin: (0, 0),
                vad_error: None,
                vad_cleanup: Vec::new(),
                ready_chunks: std::collections::VecDeque::new(),
                inner: denied_stream(),
                shutdown: None,
                local_events: Default::default(),
                output_end: None,
                denoiser: None,
                output_rate: 48_000,
                output_channels: 2,
            };
            stream.ready_chunks.push_back(chunk_to_py(fa::AudioChunk {
                data: vec![0.25; 1920],
                frames: 960,
                frame_index: 0,
                pts_ns: 0,
                seq: 0,
                flags: fa::ChunkFlags::empty(),
                dropped_before: 0,
                peak: 0.25,
                rms: 0.25,
            }));
            let error = match stream.poll_chunk() {
                Err(error) => error,
                Ok(_) => panic!("terminal polling must raise"),
            };
            assert!(error.is_instance_of::<pyo3::exceptions::PyRuntimeError>(py));
            assert!(error.to_string().contains("recording permission denied"));
            assert!(!error.to_string().contains("denied by user"));
            assert!(stream.poll_event().is_some());
            assert!(stream.terminal_error().is_some());
            let _ = stream.stop();
            assert!(stream.poll_chunk().is_err());
            assert!(stream.resume().is_err());
            assert!(stream.terminal_error().is_some());
            let locals = PyDict::new(py);
            locals
                .set_item("stream", Py::new(py, stream).expect("Python stream"))
                .expect("fixture");
            py.run(
                c_str!(
                    r#"
try:
    stream.switch_source("invalid", exclude_pids=[False], denoise=True)
except RuntimeError as error:
    assert "recording permission denied" in str(error), str(error)
    assert "denied by user" not in str(error), str(error)
else:
    raise AssertionError("terminal error must precede source validation")
"#
                ),
                None,
                Some(&locals),
            )
            .expect("terminal failure precedence");
        });
    }

    #[test]
    fn switch_exclusion_validation_needs_no_hardware() {
        Python::initialize();
        Python::attach(|py| {
            let inner = fa::Stream::open(
                fa::StreamConfig::default(),
                Box::new(fa::MockBackend::new(48_000, 2, 0.0)),
            )
            .expect("open mock stream");
            let stream = Py::new(
                py,
                Stream {
                    vad_tap: None,
                    vad_events: Vec::new(),
                    vad_origin: (0, 0),
                    vad_error: None,
                    vad_cleanup: Vec::new(),
                    ready_chunks: std::collections::VecDeque::new(),
                    inner,
                    shutdown: None,
                    local_events: Default::default(),
                    output_end: None,
                    denoiser: None,
                    output_rate: 48_000,
                    output_channels: 2,
                },
            )
            .expect("create Python stream");
            let locals = PyDict::new(py);
            locals.set_item("stream", stream).expect("set fixture");
            py.run(
                c_str!(
                    r#"
for kind in ("mic", "system", "process", "mix"):
    for value, error_type, message in (
        ([True], TypeError, "exclude_pids[0]"),
        ([1.5], TypeError, "exclude_pids[0]"),
        (["1"], TypeError, "exclude_pids[0]"),
        ([1, 2, 3, 0], ValueError, "exclude_pids[3]"),
        ([-1], ValueError, "exclude_pids[0]"),
        ([4294967296], ValueError, "exclude_pids[0]"),
        ([2 ** 256], ValueError, "exclude_pids[0]"),
        ([10 ** 5000], ValueError, "exclude_pids[0]"),
        ("123", TypeError, "exclude_pids must be a sequence"),
        (b"123", TypeError, "exclude_pids must be a sequence"),
        ({1: 2}, TypeError, "exclude_pids must be a sequence"),
        ({1}, TypeError, "exclude_pids must be a sequence"),
        ((pid for pid in [1]), TypeError, "exclude_pids must be a sequence"),
        ([False] * 4097, ValueError, "exclude_pids: too many entries (max 4096)"),
    ):
        try:
            # Invalid addon settings ensure exclusion validation happens first.
            stream.switch_source(kind, exclude_pids=value, denoise=True)
        except error_type as error:
            assert message in str(error), str(error)
        else:
            raise AssertionError("invalid exclusion was accepted")
stream.stop()
"#
                ),
                None,
                Some(&locals),
            )
            .expect("switch validation errors");
        });
    }

    #[test]
    fn vad_tap_source_switch_uses_atomic_generation_denoise_contract() {
        let source = include_str!("stream.rs")
            .split("fn switch_source(")
            .nth(1)
            .unwrap()
            .split("/// Context manager support")
            .next()
            .unwrap();
        let compact: String = source.split_whitespace().collect();
        assert!(
            compact.contains("inner.switch_source_with_denoise(config,denoise)"),
            "Python currently publishes the generation through switch_source before set_denoise"
        );
    }
}

#[cfg(test)]
mod repro_tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    struct ManualBackend(Arc<Mutex<Option<fa::core::backend::RawSink>>>);
    impl fa::CaptureBackend for ManualBackend {
        fn native_format(&self) -> (u32, u16) {
            (48_000, 1)
        }
        fn start(&mut self, sink: fa::core::backend::RawSink) -> fa::Result<()> {
            *self.0.lock().unwrap() = Some(sink);
            Ok(())
        }
        fn stop(&mut self) {
            self.0.lock().unwrap().take();
        }
    }
    fn fixture(
        denoise: bool,
        _vad: bool,
    ) -> (Stream, Arc<Mutex<Option<fa::core::backend::RawSink>>>) {
        let sink = Arc::new(Mutex::new(None));
        let mut config = fa::StreamConfig::default();
        config.output.channels = 1;
        let mut inner = fa::Stream::open(config, Box::new(ManualBackend(sink.clone()))).unwrap();
        inner.start().unwrap();
        (
            Stream {
                vad_tap: None,
                vad_events: Vec::new(),
                vad_origin: (0, 0),
                vad_error: None,
                vad_cleanup: Vec::new(),
                ready_chunks: std::collections::VecDeque::new(),
                inner,
                shutdown: None,
                local_events: Default::default(),
                output_end: None,
                denoiser: denoise.then(|| CoreDenoiser::new(1).unwrap()),
                output_rate: 48_000,
                output_channels: 1,
            },
            sink,
        )
    }
    fn send(
        stream: &mut Stream,
        sink: &Arc<Mutex<Option<fa::core::backend::RawSink>>>,
    ) -> PyAudioChunk {
        assert_eq!(
            sink.lock()
                .unwrap()
                .as_mut()
                .unwrap()
                .push(&vec![0.0; 960], 0),
            960
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if let Some(chunk) = stream.poll_chunk().unwrap() {
                return chunk;
            }
            assert!(std::time::Instant::now() < deadline, "mock chunk timed out");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }
    #[test]
    fn stop_retains_denoise_tail() {
        let (mut stream, sink) = fixture(true, false);
        let mut delivered = send(&mut stream, &sink).samples().len();
        let _ = stream.stop();
        while let Some(chunk) = stream.poll_chunk().unwrap() {
            delivered += chunk.samples().len();
        }
        assert_eq!(
            delivered,
            960 + 480,
            "stop omitted the 480-sample denoiser latency tail"
        );
        stream.stop().unwrap();
        assert!(stream.poll_chunk().unwrap().is_none());
        let report = stream.shutdown_report().unwrap().0;
        assert!(report.primary().is_none());
        assert!(report.cleanup().is_empty());
    }
    #[test]
    fn metrics_match_delivered_denoise_pcm_and_tail() {
        Python::initialize();
        Python::attach(|py| {
            let (mut stream, sink) = fixture(true, false);
            sink.lock().unwrap().as_mut().unwrap().push(&[1.0; 960], 0);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            let chunk = loop {
                if let Some(chunk) = stream.poll_chunk().unwrap() {
                    break chunk;
                }
                assert!(std::time::Instant::now() < deadline);
                std::thread::sleep(std::time::Duration::from_millis(2));
            };
            let verify = |chunk: PyAudioChunk| {
                let expected_peak = chunk
                    .samples()
                    .iter()
                    .fold(0.0_f32, |peak, sample| peak.max(sample.abs()));
                let expected_rms = (chunk
                    .samples()
                    .iter()
                    .map(|&s| f64::from(s).powi(2))
                    .sum::<f64>()
                    / chunk.samples().len() as f64)
                    .sqrt() as f32;
                let object = Py::new(py, chunk).unwrap();
                assert_eq!(
                    object
                        .bind(py)
                        .getattr("peak")
                        .unwrap()
                        .extract::<f32>()
                        .unwrap(),
                    expected_peak
                );
                assert_eq!(
                    object
                        .bind(py)
                        .getattr("rms")
                        .unwrap()
                        .extract::<f32>()
                        .unwrap(),
                    expected_rms
                );
                (
                    object
                        .bind(py)
                        .getattr("seq")
                        .unwrap()
                        .extract::<u64>()
                        .unwrap(),
                    object
                        .bind(py)
                        .getattr("pts_ns")
                        .unwrap()
                        .extract::<i64>()
                        .unwrap(),
                    object
                        .bind(py)
                        .getattr("frame_index")
                        .unwrap()
                        .extract::<u64>()
                        .unwrap(),
                    object
                        .bind(py)
                        .getattr("frames")
                        .unwrap()
                        .extract::<usize>()
                        .unwrap(),
                )
            };
            assert!(chunk.samples()[..480].iter().all(|&s| s == 0.0));
            let (seq, pts, frame_index, frames) = verify(chunk);
            stream.stop().unwrap();
            let tail = stream.poll_chunk().unwrap().unwrap();
            assert_eq!(tail.samples().len(), 480);
            let (tail_seq, tail_pts, tail_index, tail_frames) = verify(tail);
            assert_eq!(tail_seq, seq + 1);
            assert_eq!(tail_pts, pts + frames as i64 * 1_000_000_000 / 48_000);
            assert_eq!(tail_index, frame_index + frames as u64);
            assert_eq!(tail_frames, 480);
            assert!(stream.poll_chunk().unwrap().is_none());
        });
    }
}
