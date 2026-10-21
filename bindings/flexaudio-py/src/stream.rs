//! Recording stream [`Stream`], the `open()` entry point, and integrated VAD / denoise.
//!
//! Python's [`Stream`] is pull/poll based. It has no bridge thread like napi; the caller periodically
//! invokes `poll_chunk` / `poll_event`. Integrated VAD runs on demand under the GIL;
//! denoise runs in the facade before output conversion for both taps.

#[path = "stream_secondary.rs"]
mod secondary_delivery;
#[path = "stream_settings.rs"]
mod settings_capture;
#[path = "stream_stop.rs"]
mod shutdown;
#[cfg(feature = "test-faults")]
#[path = "stream_test_backend.rs"]
mod test_backend;
#[path = "stream_vad.rs"]
mod vad_capture;

use pyo3::prelude::*;

use ::flexaudio as fa;

use crate::config::{build_config, parse_exclude_pids, parse_ring_capacity, validate_denoise};
use crate::marshal::{chunk_to_py, event_to_py, PyAudioChunk, PyStreamEvent};
use crate::to_py_err;

const LIFECYCLE_EVENT_CAPACITY: usize = 256;

/// Recording stream handle. Polls the internal `flexaudio::Stream` directly.
///
/// `open(...)` returns after calling `start()`. The caller periodically invokes `poll_chunk` /
/// `poll_event`. Stop with `stop()`; a context manager (`with`) calls stop from `__exit__`.
///
/// Integrated addons:
/// - Open with `denoise=True` to apply RNNoise to canonical capture before both output taps
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
    secondary: Option<crate::secondary::SecondaryOutputOptions>,
    ready_secondary: std::collections::VecDeque<crate::secondary::PySecondaryChunk>,
    vad_secondary: bool,
    secondary_metadata: (u64, i64, u64, u32),
    shutdown: Option<fa::ShutdownReport>,
    local_events: std::collections::VecDeque<fa::Event>,
    capture_events: std::collections::VecDeque<fa::Event>,
    lifecycle_events: std::collections::VecDeque<crate::lifecycle::LifecycleEvent>,
    settings: crate::settings::SettingsState,
    vad_tap: Option<flexaudio_vad::VadTap>,
    vad_events: Vec<flexaudio_vad::AttachedVadEvent>,
    vad_origin: (u64, i64),
    vad_error: Option<flexaudio_vad::VadTapError>,
    vad_cleanup: Vec<fa::Error>,
    ready_chunks: std::collections::VecDeque<PyAudioChunk>,
}

impl Stream {
    fn route_events(&mut self) {
        while let Some(event) = self.inner.poll_event() {
            if let Some(event) = crate::lifecycle::from_event(&event) {
                if self.lifecycle_events.len() == LIFECYCLE_EVENT_CAPACITY {
                    self.lifecycle_events.pop_front();
                }
                self.lifecycle_events.push_back(event);
            } else {
                self.capture_events.push_back(event);
            }
        }
    }

    fn poll_processed_chunk(&mut self) -> PyResult<Option<PyAudioChunk>> {
        self.drain_capture();
        if let Some(error) = self.inner.terminal_error() {
            return Err(to_py_err(error));
        }
        let Some(chunk) = self.inner.poll_chunk() else {
            return Ok(None);
        };
        self.vad_origin.1 = self.vad_origin.1.max(chunk.pts_ns);
        let mut result = chunk_to_py(chunk);
        if self.vad_tap.is_some() && !self.vad_secondary {
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

    /// Poll the independent secondary tap; disabled output always returns None.
    fn poll_secondary(&mut self) -> PyResult<Option<crate::secondary::PySecondaryChunk>> {
        if self.secondary.is_none() {
            return Ok(None);
        }
        if let Some(error) = self.inner.terminal_error() {
            self.ready_secondary
                .retain(|chunk| chunk.chunk.samples.is_empty());
            return match self.ready_secondary.pop_front() {
                Some(carrier) => Ok(Some(carrier)),
                None => Err(to_py_err(error)),
            };
        }
        if let Some(chunk) = self.ready_secondary.pop_front() {
            return Ok(Some(chunk));
        }
        if let Some(chunk) = self.poll_processed_secondary() {
            return Ok(Some(chunk));
        }
        self.vad_carrier();
        Ok(self.ready_secondary.pop_front())
    }

    /// Retained typed capture primary, including after stop; does not consume events.
    fn terminal_error(&self) -> Option<crate::errors::AudioError> {
        self.inner.terminal_error().map(crate::errors::AudioError)
    }

    /// Return an event if one is available. Otherwise return `None` (non-blocking).
    fn poll_event(&mut self) -> Option<PyStreamEvent> {
        self.route_events();
        self.capture_events
            .pop_front()
            .or_else(|| self.local_events.pop_front())
            .map(event_to_py)
    }

    /// Consume only helper lifecycle notifications, including after stop.
    fn poll_lifecycle_event(&mut self) -> Option<crate::lifecycle::LifecycleEvent> {
        self.route_events();
        self.lifecycle_events.pop_front()
    }

    /// Return a fresh snapshot of the last successful open or source switch.
    fn effective_settings<'py>(
        &self,
        py: Python<'py>,
    ) -> PyResult<Bound<'py, pyo3::types::PyDict>> {
        let report = self.capture_settings().ok_or_else(|| {
            pyo3::exceptions::PyRuntimeError::new_err(
                "custom backend has no capture settings report",
            )
        })?;
        crate::settings::to_py(py, report)
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
    /// The VAD attachment configured with `vad` at `open()` persists across switches. New PCM
    /// starts a new VAD epoch at the discontinuity. `denoise` configures noise suppression for
    /// the new source; omitting it or passing `None` retains the current setting.
    /// Pass `True` or `False` explicitly to enable or disable noise suppression.
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
        denoise = None,
        silence_probe = true,
        windows_classic_loopback_keepalive = None,
        strict = None,
        ring_capacity_chunks = None,
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
        denoise: Option<bool>,
        silence_probe: bool,
        windows_classic_loopback_keepalive: Option<bool>,
        strict: Option<bool>,
        ring_capacity_chunks: Option<Bound<'_, PyAny>>,
    ) -> PyResult<()> {
        if let Some(error) = self.inner.terminal_error() {
            return Err(to_py_err(error));
        }
        let exclude_pids = parse_exclude_pids(exclude_pids.as_ref())?;

        #[cfg(feature = "test-faults")]
        let mock_source = kind == "mock";
        #[cfg(feature = "test-faults")]
        let kind = self.test_source_kind(kind)?;
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
        config.ring_capacity_chunks = parse_ring_capacity(ring_capacity_chunks.as_ref())?;
        config.secondary_output = self.inner.config().secondary_output;
        let options = self.inner.open_options().clone();
        let options = match windows_classic_loopback_keepalive {
            Some(enabled) => options.windows_classic_loopback_keepalive(enabled),
            None => options,
        };
        let options = options
            .strict(strict.unwrap_or(self.settings.strict))
            .settings_attachments(
                crate::settings::Request {
                    config: &config,
                    keepalive: windows_classic_loopback_keepalive,
                    denoise,
                    vad: self.vad_tap.is_some(),
                    secondary: self.secondary.as_ref(),
                }
                .attachments(true),
            );

        self.switch_with(
            config,
            denoise,
            windows_classic_loopback_keepalive,
            strict,
            |inner, config| {
                #[cfg(feature = "test-faults")]
                if mock_source {
                    return test_backend::switch(inner, denoise);
                }
                match denoise {
                    Some(denoise) => {
                        inner.switch_source_with_options_and_denoise(config, options, denoise)
                    }
                    None => inner.switch_source_with_options(config, options),
                }
            },
        )
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
/// Strict mode defaults to true; use `strict=False` to allow ignored non-default settings.
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
    windows_classic_loopback_keepalive = true,
    secondary_output = None,
    strict = None,
    ring_capacity_chunks = None,
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
    windows_classic_loopback_keepalive: bool,
    secondary_output: Option<&crate::secondary::SecondaryOutputOptions>,
    strict: Option<bool>,
    ring_capacity_chunks: Option<Bound<'_, PyAny>>,
) -> PyResult<Stream> {
    let exclude_pids = parse_exclude_pids(exclude_pids.as_ref())?;
    #[cfg(feature = "test-faults")]
    let mock_source = kind == "mock";
    #[cfg(feature = "test-faults")]
    let kind = if mock_source { "mic" } else { kind };
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
    config.ring_capacity_chunks = parse_ring_capacity(ring_capacity_chunks.as_ref())?;
    config.secondary_output = secondary_output.map(|options| options.format);
    if vad.is_some_and(|options| options.tap == "secondary") && secondary_output.is_none() {
        return Err(Python::attach(|py| {
            crate::vad::boundary_error(
                py,
                "UnsupportedTap",
                "secondary VAD requires secondary_output",
            )
        }));
    }
    validate_denoise(denoise, output_rate)?;
    if denoise && !(1..=2).contains(&output_channels) {
        return Err(crate::denoise_err_to_py(
            flexaudio_denoise::DenoiseError::InvalidChannels(output_channels),
        ));
    }
    crate::settings_validation::validate(&config, true).map_err(to_py_err)?;
    let strict = strict.unwrap_or(fa::OpenOptions::default().strict);
    let request = crate::settings::Request {
        config: &config,
        keepalive: Some(windows_classic_loopback_keepalive),
        denoise: Some(denoise),
        vad: vad.is_some(),
        secondary: secondary_output,
    };
    let report = request.resolve(None, strict).map_err(to_py_err)?;
    let _ = &report;
    let attachments = request.attachments(false);
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

    let options = fa::OpenOptions::new()
        .strict(strict)
        .settings_attachments(attachments)
        .windows_classic_loopback_keepalive(windows_classic_loopback_keepalive);
    #[cfg(feature = "test-faults")]
    let stream = if mock_source {
        fa::Stream::open(config, test_backend::backend())
    } else {
        fa::open_with_options(config, options)
    }
    .map_err(to_py_err)?;
    #[cfg(not(feature = "test-faults"))]
    let stream = fa::open_with_options(config, options).map_err(to_py_err)?;
    let stream = start_capture(stream, denoise, vad_tap.is_some())?;
    #[cfg(any(test, feature = "test-faults"))]
    let settings_report = stream.effective_settings().is_none().then_some(report);
    Ok(Stream {
        secondary: secondary_output.copied(),
        ready_secondary: Default::default(),
        vad_secondary: vad.is_some_and(|options| options.tap == "secondary"),
        secondary_metadata: (0, 0, 0, 0),
        vad_tap,
        vad_events: Vec::new(),
        vad_origin: (0, 0),
        vad_error: None,
        vad_cleanup: Vec::new(),
        ready_chunks: std::collections::VecDeque::new(),
        inner: stream,
        shutdown: None,
        local_events: Default::default(),
        lifecycle_events: Default::default(),
        capture_events: Default::default(),
        settings: crate::settings::SettingsState {
            #[cfg(any(test, feature = "test-faults"))]
            report: settings_report,
            strict,
        },
    })
}

/// Configure canonical processing before the intake thread starts.
fn start_capture(mut stream: fa::Stream, denoise: bool, capture_tap: bool) -> PyResult<fa::Stream> {
    if capture_tap {
        stream.enable_capture_tap().map_err(to_py_err)?;
    }
    stream.set_denoise(denoise);
    stream.start().map_err(to_py_err)?;
    Ok(stream)
}

#[cfg(test)]
#[path = "stream_denoise_tests.rs"]
mod denoise_tests;

#[cfg(test)]
#[path = "stream_settings_tests.rs"]
mod settings_tests;

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
                secondary: None,
                ready_secondary: Default::default(),
                vad_secondary: false,
                secondary_metadata: (0, 0, 0, 0),
                ready_chunks: std::collections::VecDeque::new(),
                inner: denied_stream(),
                shutdown: None,
                local_events: Default::default(),
                lifecycle_events: Default::default(),
                capture_events: Default::default(),
                settings: Default::default(),
            };
            stream.ready_chunks.push_back(chunk_to_py({
                let mut chunk_value = fa::AudioChunk::new(vec![0.25; 1920], 960);
                chunk_value.frame_index = 0;
                chunk_value.pts_ns = 0;
                chunk_value.seq = 0;
                chunk_value.flags = fa::ChunkFlags::empty();
                chunk_value.dropped_before = 0;
                chunk_value.peak = 0.25;
                chunk_value.rms = 0.25;
                chunk_value
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
                    secondary: None,
                    ready_secondary: Default::default(),
                    vad_secondary: false,
                    secondary_metadata: (0, 0, 0, 0),
                    ready_chunks: std::collections::VecDeque::new(),
                    inner,
                    shutdown: None,
                    local_events: Default::default(),
                    lifecycle_events: Default::default(),
                    capture_events: Default::default(),
                    settings: Default::default(),
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
            compact.contains(".switch_source_with_options_and_denoise(config,options,denoise)"),
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
        inner.set_denoise(denoise);
        inner.start().unwrap();
        (
            Stream {
                vad_tap: None,
                vad_events: Vec::new(),
                vad_origin: (0, 0),
                vad_error: None,
                vad_cleanup: Vec::new(),
                secondary: None,
                ready_secondary: Default::default(),
                vad_secondary: false,
                secondary_metadata: (0, 0, 0, 0),
                ready_chunks: std::collections::VecDeque::new(),
                inner,
                shutdown: None,
                local_events: Default::default(),
                lifecycle_events: Default::default(),
                capture_events: Default::default(),
                settings: Default::default(),
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
            960 + 960,
            "stop must retain the canonical denoise tail padded to a 20 ms chunk"
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
            assert_eq!(tail.samples().len(), 960);
            let (tail_seq, tail_pts, tail_index, tail_frames) = verify(tail);
            assert_eq!(tail_seq, seq + 1);
            assert_eq!(tail_pts, pts + frames as i64 * 1_000_000_000 / 48_000);
            assert_eq!(tail_index, frame_index + frames as u64);
            assert_eq!(tail_frames, 960);
            assert!(stream.poll_chunk().unwrap().is_none());
        });
    }
}
