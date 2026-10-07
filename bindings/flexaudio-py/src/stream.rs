//! Recording stream [`Stream`], the `open()` entry point, and integrated VAD / denoise.
//!
//! Python's [`Stream`] is pull/poll based. It has no bridge thread like napi; the caller periodically
//! invokes `poll_chunk` / `poll_event`. Integrated VAD / denoise processing also runs when
//! poll_chunk is called (under the GIL; process on demand).

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
}

#[pymethods]
impl Stream {
    /// Stop recording. Safe to call repeatedly (flexaudio is idempotent).
    fn stop(&mut self) {
        self.inner.stop();
    }

    /// Pause delivery without stopping recording. Resume with `resume`.
    fn pause(&self) {
        self.inner.pause();
    }

    /// End the pause and resume delivery.
    fn resume(&self) {
        self.inner.resume();
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
    /// If integrated addons are enabled, process the chunk here before returning it (denoise → VAD).
    /// denoise overwrites the chunk audio in place. VAD detects speech boundaries from the processed
    /// audio and attaches them to `chunk.vad_events`. If both are disabled, pass the chunk through.
    fn poll_chunk(&mut self) -> PyResult<Option<PyAudioChunk>> {
        let Some(chunk) = self.inner.poll_chunk() else {
            return Ok(None);
        };
        let mut py_chunk = chunk_to_py(chunk);

        // 1) denoise: overwrite chunk audio in place. Its length is always divisible by the number of
        //    output channels (frames * channels), so errors are not expected. If one occurs (length
        //    mismatch), pass through on a best-effort basis so polling continues.
        if let Some(dn) = self.denoiser.as_mut() {
            let _ = dn.process(py_chunk.samples_mut());
        }

        // 2) VAD: detect speech boundaries from processed audio and attach them. process_pcm converts
        //    to mono and resamples to the VAD rate internally, so pass the output format as is.
        if let Some(vad) = self.vad.as_mut() {
            let events: Vec<(bool, u64)> = vad
                .process_pcm(py_chunk.samples(), self.output_rate, self.output_channels)
                .map_err(vad_err_to_py)?
                .into_iter()
                .map(|ev| match ev {
                    flexaudio_vad::VadEvent::SpeechStart { at_sample } => (true, at_sample),
                    flexaudio_vad::VadEvent::SpeechEnd { at_sample } => (false, at_sample),
                })
                .collect();
            py_chunk.set_vad_events(events);
        }

        Ok(Some(py_chunk))
    }

    /// Return an event if one is available. Otherwise return `None` (non-blocking).
    fn poll_event(&mut self) -> Option<PyStreamEvent> {
        self.inner.poll_event().map(event_to_py)
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
        let exclude_pids = parse_exclude_pids(exclude_pids.as_ref())?;

        // Addons depend on output format. Since a switch cannot change it, validate and build using
        // output_rate/output_channels from open (the output_rate argument is used by core's
        // switch_source to check that formats match).
        let (new_vad, new_denoiser) = Self::build_addons(
            vad.as_ref(),
            denoise,
            self.output_rate,
            self.output_channels,
        )?;

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
        self.inner.switch_source(config).map_err(to_py_err)?;

        // Replace addons only after the switch succeeds (keep old addons on failure).
        self.vad = new_vad;
        self.denoiser = new_denoiser;
        Ok(())
    }

    /// Context manager support. Use as `with flexaudio.open("mic") as s:`.
    fn __enter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    /// Calls stop when leaving the `with` block. Does not swallow exceptions (returns False).
    fn __exit__(
        &mut self,
        _exc_type: Option<Bound<'_, PyAny>>,
        _exc_value: Option<Bound<'_, PyAny>>,
        _traceback: Option<Bound<'_, PyAny>>,
    ) -> bool {
        self.inner.stop();
        false
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
) -> PyResult<Stream> {
    let exclude_pids = parse_exclude_pids(exclude_pids.as_ref())?;

    // Validate and build addons first. Reject denoise unless the output rate is 48 kHz, and reject
    // invalid VAD settings, before acquiring a device.
    let (vad_state, denoiser) =
        Stream::build_addons(vad.as_ref(), denoise, output_rate, output_channels)?;

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
    let mut stream = fa::open(config).map_err(to_py_err)?;
    stream.start().map_err(to_py_err)?;
    Ok(Stream {
        inner: stream,
        denoiser,
        vad: vad_state,
        output_rate,
        output_channels,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pyo3::ffi::c_str;

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
                    inner,
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
}
