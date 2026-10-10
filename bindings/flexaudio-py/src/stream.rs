//! Recording stream [`Stream`], the `open()` entry point, and integrated VAD / denoise.
//!
//! Python's [`Stream`] is pull/poll based. It has no bridge thread like napi; the caller periodically
//! invokes `poll_chunk` / `poll_event`. Integrated VAD / denoise processing also runs when
//! poll_chunk is called (under the GIL; process on demand).

#[path = "stream_stop.rs"]
mod shutdown;
#[path = "stream_whisper.rs"]
mod whisper_capture;

use pyo3::prelude::*;
use pyo3::types::PyDict;

use ::flexaudio as fa;
use flexaudio_denoise::Denoiser as CoreDenoiser;
use flexaudio_vad::Vad as CoreVad;

use crate::config::{build_config, parse_exclude_pids, vad_config_from_dict, validate_denoise};
use crate::marshal::{chunk_to_py, event_to_py, PyAudioChunk, PyStreamEvent};
use crate::{denoise_err_to_py, to_py_err, vad_err_to_py};

/// Recording stream handle. Polls the internal `flexaudio::Stream` directly.
///
/// `open(...)` returns after calling `start()`. The caller periodically invokes `poll_chunk` /
/// `poll_event`. Stop with `stop()`; a context manager (`with`) calls stop from `__exit__`.
///
/// Integrated addons:
/// - Open with `denoise=True` to overwrite chunk audio with RNNoise before `poll_chunk` returns
///   (48kHz output only; the first 480 samples/channel are silent due to denoise latency).
/// - Open with `vad={...}` to run VAD on each chunk and attach confirmed speech boundaries to
///   `chunk.vad_events`. Processing order is denoise → VAD.
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
    whisper: Option<flexaudio_vad::WhisperVadTap>,
    whisper_events: Vec<flexaudio_vad::AttachedWhisperVadEvent>,
    whisper_origin: (u64, i64),
    whisper_error: Option<flexaudio_vad::WhisperVadTapError>,
    whisper_error_reported: bool,
    ready_chunks: std::collections::VecDeque<PyAudioChunk>,
    // Integrated denoise (requires 48kHz). None when disabled.
    denoiser: Option<CoreDenoiser>,
    // Integrated VAD. None when disabled.
    vad: Option<CoreVad>,
    // Output format. Passed to VAD's process_pcm and used to validate denoise's 48kHz requirement.
    // switch_source cannot change the output format, so this remains the value from open.
    output_rate: u32,
    output_channels: u16,
}

impl Stream {
    /// Build addon state (VAD / denoise) from Python arguments. Validate denoise's 48kHz requirement
    /// and handle Denoiser / Vad construction failures here.
    fn build_addons(
        vad: Option<&Bound<'_, PyDict>>,
        denoise: bool,
        output_rate: u32,
        output_channels: u16,
    ) -> PyResult<(Option<CoreVad>, Option<CoreDenoiser>)> {
        validate_denoise(denoise, output_rate)?;
        let denoiser = if denoise {
            Some(CoreDenoiser::new(output_channels).map_err(denoise_err_to_py)?)
        } else {
            None
        };
        let vad_state = match vad {
            Some(d) => Some(CoreVad::new(vad_config_from_dict(d)?).map_err(vad_err_to_py)?),
            None => None,
        };
        Ok((vad_state, denoiser))
    }

    /// Keep the flush boundary injectable so recovery can be tested without changing the VAD API.
    fn poll_chunk_with_flush(
        &mut self,
        flush: impl FnOnce(
            &mut CoreVad,
        ) -> Result<Vec<flexaudio_vad::VadEvent>, flexaudio_vad::VadError>,
    ) -> PyResult<Option<PyAudioChunk>> {
        self.drain_capture();
        if let Some(error) = self.inner.terminal_error() {
            return Err(to_py_err(error));
        }
        let Some(chunk) = self.inner.poll_chunk() else {
            return match self.inner.terminal_error() {
                Some(error) => Err(to_py_err(error)),
                None => Ok(None),
            };
        };
        // The core marks the first chunk after a pause/resume, a source switch, or dropped audio with
        // DISCONTINUITY: it is not contiguous with what came before, so clear the add-on history and
        // start a fresh timeline at this chunk. Otherwise the denoise delay line replays pre-gap
        // audio and VAD keeps its sample clock and resampler across the gap.
        //
        // Flush VAD before clearing it. A bare reset discards an unreported open segment: both
        // SpeechStart and SpeechEnd are emitted only when that segment is finalized. Flush returns
        // that pair on the old sample clock and resets before processing post-gap audio.
        let discontinuity = chunk.flags.contains(fa::ChunkFlags::DISCONTINUITY);
        self.output_end = Some((
            chunk
                .frame_index
                .saturating_add((chunk.frames as u64 * 48_000) / u64::from(self.output_rate)),
            chunk.pts_ns.saturating_add(
                (chunk.frames as i64 * 1_000_000_000) / i64::from(self.output_rate),
            ),
            chunk.seq.saturating_add(1),
            chunk.dropped_before,
        ));
        self.whisper_origin.1 = self.whisper_origin.1.max(chunk.pts_ns);
        let mut py_chunk = chunk_to_py(chunk);
        let mut vad_events: Vec<(bool, u64)> = Vec::new();
        if discontinuity {
            let mut flush_error = None;
            if let Some(vad) = self.vad.as_mut() {
                match flush(vad) {
                    Ok(events) => vad_events = vad_event_pairs(events),
                    Err(error) => {
                        // A latched failure makes flush return before resetting. Attempt recovery
                        // anyway; if reset also fails, VAD retains that failure for later polls.
                        let _ = vad.reset();
                        flush_error = Some(error);
                    }
                }
            }
            if let Some(dn) = self.denoiser.as_mut() {
                dn.reset();
            }
            // Report the original flush error once, after both reset attempts. This poll consumes
            // the discontinuity chunk; the next poll starts with fresh state if reset succeeded.
            if let Some(error) = flush_error {
                return Err(vad_err_to_py(error));
            }
        }

        // Denoise before measuring or detecting speech in delivered PCM.
        if let Some(dn) = self.denoiser.as_mut() {
            dn.process(py_chunk.samples_mut())
                .map_err(denoise_err_to_py)?;
        }

        py_chunk.update_metrics();

        // 2) VAD: detect speech boundaries from processed audio and attach them. process_pcm converts
        //    to mono and resamples to the VAD rate internally, so pass the output format as is. New
        //    events follow any events the discontinuity flush above already produced.
        if let Some(vad) = self.vad.as_mut() {
            let events = vad
                .process_pcm(py_chunk.samples(), self.output_rate, self.output_channels)
                .map_err(vad_err_to_py)?;
            vad_events.extend(vad_event_pairs(events));
        }
        if self.vad.is_some() {
            py_chunk.set_vad_events(vad_events);
        }

        if self.whisper.is_some() {
            py_chunk.set_whisper_events(std::mem::take(&mut self.whisper_events));
        }
        Ok(Some(py_chunk))
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

    /// Flush an attached whisper epoch. Disabled attachment is a no-op.
    fn flush_whisper_vad(&mut self) -> PyResult<()> {
        if self.whisper.is_none() {
            return Ok(());
        }
        self.drain_capture();
        let drained = self.drain_output();
        if let Some(whisper) = self.whisper.as_mut() {
            let result = whisper.flush();
            self.whisper_error = None;
            self.whisper_error_reported = false;
            self.accept_whisper(result);
            self.whisper_carrier();
        }
        drained
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
    /// If integrated addons are enabled, process the chunk here before returning it (denoise → VAD).
    /// denoise overwrites the chunk audio in place. VAD detects speech boundaries from the processed
    /// audio and attaches them to `chunk.vad_events`. If both are disabled, pass the chunk through.
    /// On DISCONTINUITY (flags bit 0), flushed pre-gap events come first. A fixed 20 ms chunk
    /// cannot complete a fresh 32 ms VAD frame, so all events on that chunk belong to the old
    /// timeline. Events on later chunks use the new VAD sample clock, restarted at zero.
    /// Both boundaries are delivered together when a segment finalizes. If flushing fails,
    /// reset VAD and denoise before raising the flush error; the consumed chunk is not returned.
    fn poll_chunk(&mut self) -> PyResult<Option<PyAudioChunk>> {
        if let Some(error) = self.inner.terminal_error() {
            // Closing event carriers remain deliverable; captured PCM does not.
            self.ready_chunks.retain(|chunk| chunk.samples().is_empty());
            return match self.ready_chunks.pop_front() {
                Some(carrier) => Ok(Some(carrier)),
                None => Err(to_py_err(error)),
            };
        }
        if let Some(chunk) = self.ready_chunks.pop_front() {
            return Ok(Some(chunk));
        }
        if !self.whisper_error_reported && self.whisper_events.is_empty() {
            if let Some(error) = self.whisper_error.clone() {
                self.whisper_error_reported = true;
                return Err(crate::whisper_vad::tap_error(error));
            }
        }
        let result = match self.poll_chunk_with_flush(CoreVad::flush) {
            Ok(chunk) => chunk,
            Err(error) => {
                self.whisper_carrier();
                if let Some(chunk) = self.ready_chunks.pop_front() {
                    return Ok(Some(chunk));
                }
                return Err(error);
            }
        };
        if result.is_some() {
            return Ok(result);
        }
        self.whisper_carrier();
        if let Some(chunk) = self.ready_chunks.pop_front() {
            return Ok(Some(chunk));
        }
        if !self.whisper_error_reported {
            if let Some(error) = self.whisper_error.clone() {
                self.whisper_error_reported = true;
                return Err(crate::whisper_vad::tap_error(error));
            }
        }
        Ok(None)
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
        vad = None,
        denoise = false,
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
        vad: Option<Bound<'_, PyDict>>,
        denoise: bool,
    ) -> PyResult<()> {
        if let Some(error) = self.inner.terminal_error() {
            return Err(to_py_err(error));
        }
        let exclude_pids = parse_exclude_pids(exclude_pids.as_ref())?;

        let config = build_config(
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

        // Addons depend on output format. Since a switch cannot change it, validate and build using
        // output_rate/output_channels from open (the output_rate argument is used by core's
        // switch_source to check that formats match).
        if self.whisper.is_some() && vad.is_some() {
            return Python::attach(|py| {
                Err(crate::whisper_vad::boundary_error(
                    py,
                    "ConflictingVad",
                    "vad and whisper_vad are mutually exclusive",
                ))
            });
        }
        let (new_vad, new_denoiser) = Self::build_addons(
            vad.as_ref(),
            denoise,
            self.output_rate,
            self.output_channels,
        )?;

        if self.whisper.is_some() {
            self.inner
                .switch_source_with_denoise(config, denoise)
                .map_err(to_py_err)?;
        } else {
            self.inner.switch_source(config).map_err(to_py_err)?;
        }

        // Replace addons only after the switch succeeds (keep old addons on failure).
        self.vad = new_vad;
        if self.whisper.is_some() {
            self.denoiser = None;
        } else {
            self.denoiser = new_denoiser;
        }
        Ok(())
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

/// Map VAD events to Python-facing `(is_start, at_sample)` pairs (start = true, end = false).
fn vad_event_pairs(events: Vec<flexaudio_vad::VadEvent>) -> Vec<(bool, u64)> {
    events
        .into_iter()
        .map(|ev| match ev {
            flexaudio_vad::VadEvent::SpeechStart { at_sample } => (true, at_sample),
            flexaudio_vad::VadEvent::SpeechEnd { at_sample } => (false, at_sample),
        })
        .collect()
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
/// Integrated addons:
/// - `vad` (dict, default None): specifying it enables integrated VAD. Keys match the standalone
///   `Vad` arguments (`threshold` / `min_speech_ms` / `min_silence_ms` / `speech_pad_ms` /
///   `max_speech_ms` / `sample_rate` / `neg_threshold`). Speech boundaries are added to each chunk's
///   `vad_events`.
/// - `denoise` (bool, default False): True enables RNNoise noise suppression. Only supported for
///   48kHz output; `denoise=True` with `output_rate!=48000` raises `ValueError`. Processing order is
///   denoise → VAD.
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
    vad = None,
    denoise = false,
    whisper_vad = None,
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
    vad: Option<Bound<'_, PyDict>>,
    denoise: bool,
    whisper_vad: Option<&crate::whisper_vad::WhisperVadStreamOptions>,
) -> PyResult<Stream> {
    crate::whisper_vad::validate_attachment(whisper_vad, vad.is_some())?;
    let exclude_pids = parse_exclude_pids(exclude_pids.as_ref())?;
    let config = build_config(
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
    let whisper = whisper_vad
        .map(|options| {
            flexaudio_vad::WhisperVadTap::new(
                options.params.inner.clone(),
                flexaudio_vad::WhisperVadOptions {
                    provisional: options.provisional,
                },
            )
        })
        .transpose()
        .map_err(crate::whisper_vad::tap_error)?;

    // Validate and build addons before acquiring a device. Reject denoise unless the output rate is 48 kHz, and reject
    // invalid VAD settings, before acquiring a device.
    let (vad_state, denoiser) =
        Stream::build_addons(vad.as_ref(), denoise, output_rate, output_channels)?;

    let mut stream = fa::open(config).map_err(to_py_err)?;
    if whisper.is_some() {
        stream.enable_capture_tap().map_err(to_py_err)?;
        stream.set_denoise(denoise);
    }
    stream.start().map_err(to_py_err)?;
    Ok(Stream {
        whisper,
        whisper_events: Vec::new(),
        whisper_origin: (0, 0),
        whisper_error: None,
        whisper_error_reported: false,
        ready_chunks: std::collections::VecDeque::new(),
        inner: stream,
        shutdown: None,
        local_events: Default::default(),
        output_end: None,
        denoiser: if whisper_vad.is_some() {
            None
        } else {
            denoiser
        },
        vad: vad_state,
        output_rate,
        output_channels,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pyo3::ffi::c_str;

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
                whisper: None,
                whisper_events: Vec::new(),
                whisper_origin: (0, 0),
                whisper_error: None,
                whisper_error_reported: false,
                ready_chunks: std::collections::VecDeque::new(),
                inner: denied_stream(),
                shutdown: None,
                local_events: Default::default(),
                output_end: None,
                denoiser: None,
                vad: None,
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
                    whisper: None,
                    whisper_events: Vec::new(),
                    whisper_origin: (0, 0),
                    whisper_error: None,
                    whisper_error_reported: false,
                    ready_chunks: std::collections::VecDeque::new(),
                    inner,
                    shutdown: None,
                    local_events: Default::default(),
                    output_end: None,
                    denoiser: None,
                    vad: None,
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
            stream.switch_source(kind, exclude_pids=value, denoise=True,
                                 vad={"threshold": "invalid"})
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
    fn whisper_source_switch_uses_atomic_generation_denoise_contract() {
        let source = include_str!("stream.rs")
            .split("fn switch_source(")
            .nth(1)
            .unwrap()
            .split("/// Context manager support")
            .next()
            .unwrap();
        let compact: String = source.split_whitespace().collect();
        assert!(
            compact.contains("self.inner.switch_source_with_denoise(config,denoise)"),
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
        vad: bool,
    ) -> (Stream, Arc<Mutex<Option<fa::core::backend::RawSink>>>) {
        let sink = Arc::new(Mutex::new(None));
        let mut config = fa::StreamConfig::default();
        config.output.channels = 1;
        let mut inner = fa::Stream::open(config, Box::new(ManualBackend(sink.clone()))).unwrap();
        inner.start().unwrap();
        (
            Stream {
                whisper: None,
                whisper_events: Vec::new(),
                whisper_origin: (0, 0),
                whisper_error: None,
                whisper_error_reported: false,
                ready_chunks: std::collections::VecDeque::new(),
                inner,
                shutdown: None,
                local_events: Default::default(),
                output_end: None,
                denoiser: denoise.then(|| CoreDenoiser::new(1).unwrap()),
                vad: vad.then(|| CoreVad::new(Default::default()).unwrap()),
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
    fn repro_p10_f42_stop_retains_denoise_tail() {
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

    #[test]
    fn repro_p10_f43_resume_keeps_vad_history() {
        Python::initialize();
        Python::attach(|py| {
            let (mut stream, sink) = fixture(false, true);
            send(&mut stream, &sink);
            let before = stream.vad.as_ref().unwrap().converted_sample_position();
            // 319, not 320: `converted_sample_position` counts the frames rubato actually emitted
            // (flexaudio-vad/src/lib.rs:177), and the sinc resampler emits 319 frames for the first
            // fixed 960-frame chunk at 48k->16k (resample.rs:198 chunk = 48000/50 = 960;
            // rubato 3.0 asynchro.rs:383-386 floors (960-129-(-127))*1/3 = 319 for FixedAsync::Input
            // with asynchro_sinc.rs:518-520 last_index = -(sinc_len-1)). Later chunks emit 320.
            assert_eq!(before, 319);
            stream.pause();
            stream.resume().unwrap();
            let chunk = send(&mut stream, &sink);
            let chunk = Py::new(py, chunk).unwrap();
            let flags: u32 = chunk.bind(py).getattr("flags").unwrap().extract().unwrap();
            assert_ne!(flags & fa::ChunkFlags::DISCONTINUITY.bits(), 0);
            assert_eq!(
                stream.vad.as_ref().unwrap().converted_sample_position(),
                before,
                "VAD sample history should restart at the resumed discontinuity"
            );
            let _ = stream.stop();
        });
    }
    #[test]
    fn repro_p10_control_plain_stop_and_continuous_vad() {
        let (mut stream, sink) = fixture(false, true);
        send(&mut stream, &sink);
        send(&mut stream, &sink);
        // Two 960-frame chunks yield 319 + 320 = 639 actual resampler frames, not 640; see the
        // note in repro_p10_f43_resume_keeps_vad_history.
        assert_eq!(
            stream.vad.as_ref().unwrap().converted_sample_position(),
            639
        );
        let _ = stream.stop();
        while stream.poll_chunk().unwrap().is_some() {}
        assert!(stream.denoiser.is_none());
    }

    /// Like `fixture`, but with an integrated VAD configured with explicit `threshold` (used for
    /// both the speech and silence thresholds) so segment state is deterministic without real audio.
    fn vad_fixture(threshold: f32) -> (Stream, Arc<Mutex<Option<fa::core::backend::RawSink>>>) {
        vad_fixture_with_max(threshold, 0)
    }

    fn vad_fixture_with_max(
        threshold: f32,
        max_speech_ms: u32,
    ) -> (Stream, Arc<Mutex<Option<fa::core::backend::RawSink>>>) {
        let sink = Arc::new(Mutex::new(None));
        let mut config = fa::StreamConfig::default();
        config.output.channels = 1;
        let mut inner = fa::Stream::open(config, Box::new(ManualBackend(sink.clone()))).unwrap();
        let vad = CoreVad::new(flexaudio_vad::VadConfig {
            threshold,
            neg_threshold: Some(threshold),
            min_speech_ms: 0,
            min_silence_ms: 0,
            speech_pad_ms: 0,
            max_speech_ms,
            sample_rate: 16_000,
        })
        .unwrap();
        // Model construction can exceed the watchdog's idle limit under parallel tests. Start
        // capture only when the fixture is ready to supply PCM, avoiding unrelated recovery flags.
        inner.start().unwrap();
        (
            Stream {
                whisper: None,
                whisper_events: Vec::new(),
                whisper_origin: (0, 0),
                whisper_error: None,
                whisper_error_reported: false,
                ready_chunks: std::collections::VecDeque::new(),
                inner,
                shutdown: None,
                local_events: Default::default(),
                output_end: None,
                denoiser: None,
                vad: Some(vad),
                output_rate: 48_000,
                output_channels: 1,
            },
            sink,
        )
    }

    /// Read the `type` of each VAD event delivered on a Python `AudioChunk` object.
    fn delivered_event_types(chunk: &Bound<'_, PyAny>) -> Vec<String> {
        chunk
            .getattr("vad_events")
            .unwrap()
            .try_iter()
            .unwrap()
            .map(|event| {
                event
                    .unwrap()
                    .getattr("type")
                    .unwrap()
                    .extract::<String>()
                    .unwrap()
            })
            .collect()
    }

    /// A DISCONTINUITY chunk must flush an open VAD speech segment: the caller receives exactly one
    /// SpeechEnd (with its matching SpeechStart) attached to that chunk, and no later event refers to
    /// the old segment.
    #[test]
    fn discontinuity_flushes_open_vad_segment() {
        Python::initialize();
        Python::attach(|py| {
            // threshold 0 makes every frame "speech"; min_speech 0 keeps the flushed segment.
            let (mut stream, sink) = vad_fixture(0.0);
            // Two chunks (~639 converted 16k frames) exceed one 512-sample inference frame, so the
            // segmenter is triggered before the discontinuity.
            send(&mut stream, &sink);
            send(&mut stream, &sink);
            stream.pause();
            stream.resume().unwrap();
            let chunk = send(&mut stream, &sink);
            let chunk = Py::new(py, chunk).unwrap();
            let bound = chunk.bind(py);
            let flags: u32 = bound.getattr("flags").unwrap().extract().unwrap();
            assert_ne!(flags & fa::ChunkFlags::DISCONTINUITY.bits(), 0);
            let types = delivered_event_types(bound);
            let _ = stream.stop();
            assert_eq!(
                types.iter().filter(|t| *t == "speech_end").count(),
                1,
                "types: {types:?}"
            );
            assert_eq!(
                types.last().map(String::as_str),
                Some("speech_end"),
                "nothing after the SpeechEnd may refer to the flushed segment: {types:?}"
            );
            let end_index = types
                .iter()
                .position(|t| t == "speech_end")
                .expect("one SpeechEnd");
            assert_eq!(types[end_index - 1], "speech_start", "types: {types:?}");
        });
    }

    /// Control: with no open segment, a DISCONTINUITY chunk emits no VAD event (the flush is a no-op).
    #[test]
    fn discontinuity_without_open_segment_emits_nothing() {
        Python::initialize();
        Python::attach(|py| {
            // threshold 1 is unreachable for a sigmoid probability, so the segmenter never triggers.
            let (mut stream, sink) = vad_fixture(1.0);
            send(&mut stream, &sink);
            send(&mut stream, &sink);
            stream.pause();
            stream.resume().unwrap();
            let chunk = send(&mut stream, &sink);
            let chunk = Py::new(py, chunk).unwrap();
            let bound = chunk.bind(py);
            let flags: u32 = bound.getattr("flags").unwrap().extract().unwrap();
            assert_ne!(flags & fa::ChunkFlags::DISCONTINUITY.bits(), 0);
            let types = delivered_event_types(bound);
            let _ = stream.stop();
            assert!(types.is_empty(), "types: {types:?}");
        });
    }

    fn delivered_events(chunk: &Bound<'_, PyAny>) -> Vec<(String, u64)> {
        chunk
            .getattr("vad_events")
            .unwrap()
            .try_iter()
            .unwrap()
            .map(|event| {
                let event = event.unwrap();
                (
                    event.getattr("type").unwrap().extract().unwrap(),
                    event.getattr("at_sample").unwrap().extract().unwrap(),
                )
            })
            .collect()
    }

    #[test]
    fn discontinuity_chunk_separates_old_and_new_vad_timelines() {
        Python::initialize();
        let (mut stream, sink) = vad_fixture_with_max(0.0, 32);
        let before = (0..2).map(|_| send(&mut stream, &sink)).collect::<Vec<_>>();
        stream.pause();
        stream.resume().unwrap();
        let discontinuity = send(&mut stream, &sink);
        assert_eq!(
            stream.vad.as_ref().unwrap().converted_sample_position(),
            319
        );
        // Post-gap speech starts inside the same chunk, but 319 converted samples cannot complete
        // a fresh 512-sample frame. Its boundaries arrive on a later chunk.
        let after = (0..3).map(|_| send(&mut stream, &sink)).collect::<Vec<_>>();
        let _ = stream.stop();
        // Initialize/read Python objects after stopping: lazy type initialization can release the
        // GIL to other tests for longer than the mock capture's watchdog idle limit.
        Python::attach(|py| {
            for chunk in before {
                let chunk = Py::new(py, chunk).unwrap();
                assert!(
                    delivered_events(chunk.bind(py)).is_empty(),
                    "open segment is not yet reported"
                );
            }
            let chunk = Py::new(py, discontinuity).unwrap();
            let bound = chunk.bind(py);
            let flags: u32 = bound.getattr("flags").unwrap().extract().unwrap();
            assert_ne!(flags & fa::ChunkFlags::DISCONTINUITY.bits(), 0);
            assert_eq!(
                bound.getattr("frames").unwrap().extract::<u32>().unwrap(),
                960
            );
            assert_eq!(
                delivered_events(bound),
                [("speech_start".into(), 0), ("speech_end".into(), 512)],
                "old timeline only"
            );
            for (index, chunk) in after.into_iter().enumerate() {
                let chunk = Py::new(py, chunk).unwrap();
                let bound = chunk.bind(py);
                let flags: u32 = bound.getattr("flags").unwrap().extract().unwrap();
                assert_eq!(flags & fa::ChunkFlags::DISCONTINUITY.bits(), 0);
                if index == 2 {
                    assert_eq!(
                        delivered_events(bound),
                        [("speech_start".into(), 0), ("speech_end".into(), 1024)],
                        "new timeline only"
                    );
                } else {
                    assert!(delivered_events(bound).is_empty());
                }
            }
        });
    }

    #[test]
    fn discontinuity_flush_error_resets_both_addons_and_is_reported_once() {
        use std::cell::Cell;
        use std::time::{Duration, Instant};

        Python::initialize();
        // Inject at the flush boundary: a latched core failure returns this error without resetting.
        // The no-error case uses the real flush as a control for the same reset/processing path.
        for fail_flush in [false, true] {
            let (mut stream, sink) = vad_fixture(0.0);
            send(&mut stream, &sink);
            send(&mut stream, &sink);
            let old_position = stream.vad.as_ref().unwrap().converted_sample_position();
            let mut denoiser = CoreDenoiser::new(1).unwrap();
            denoiser.process(&mut [0.8; 137]).unwrap();
            stream.denoiser = Some(denoiser);
            stream.pause();
            stream.resume().unwrap();
            assert_eq!(
                sink.lock().unwrap().as_mut().unwrap().push(&[0.0; 960], 0),
                960
            );
            let error = flexaudio_vad::VadError::Inference("injected latched VAD failure".into());
            let flush_calls = Cell::new(0);
            let deadline = Instant::now() + Duration::from_secs(2);
            let mut reference = CoreDenoiser::new(1).unwrap();
            let mut errors = Vec::new();
            let mut control_chunk = None;
            loop {
                let result = stream.poll_chunk_with_flush(|vad| {
                    flush_calls.set(flush_calls.get() + 1);
                    assert_eq!(vad.converted_sample_position(), old_position);
                    if fail_flush {
                        Err(error.clone())
                    } else {
                        vad.flush()
                    }
                });
                match result {
                    Err(actual) => {
                        assert!(fail_flush);
                        errors.push(actual);
                        assert_eq!(stream.vad.as_ref().unwrap().converted_sample_position(), 0);
                        break;
                    }
                    Ok(Some(chunk)) => {
                        assert!(!fail_flush);
                        let mut expected = [0.0; 960];
                        reference.process(&mut expected).unwrap();
                        assert_eq!(chunk.samples(), expected);
                        control_chunk = Some(chunk);
                        break;
                    }
                    Ok(None) => {
                        assert!(Instant::now() < deadline, "mock chunk timed out");
                        std::thread::sleep(Duration::from_millis(2));
                    }
                }
            }
            assert_eq!(flush_calls.get(), 1);
            for _ in 0..3 {
                let chunk = send(&mut stream, &sink);
                let mut expected = [0.0; 960];
                reference.process(&mut expected).unwrap();
                assert_eq!(chunk.samples(), expected);
            }
            assert_eq!(errors.len(), usize::from(fail_flush));
            assert_eq!(
                stream.vad.as_ref().unwrap().converted_sample_position(),
                if fail_flush { 959 } else { 1279 }
            );
            assert!(stream.poll_chunk().unwrap().is_none());
            let _ = stream.stop();
            // Inspect Python objects after capture stops so parallel type initialization cannot
            // introduce a watchdog recovery discontinuity into the mock's otherwise continuous PCM.
            Python::attach(|py| {
                for actual in errors {
                    assert!(actual.is_instance_of::<pyo3::exceptions::PyRuntimeError>(py));
                    assert_eq!(actual.value(py).to_string(), error.to_string());
                }
                if let Some(chunk) = control_chunk {
                    let chunk = Py::new(py, chunk).unwrap();
                    assert_eq!(
                        delivered_events(chunk.bind(py)),
                        [("speech_start".into(), 0), ("speech_end".into(), 512)]
                    );
                }
            });
        }
    }
}
