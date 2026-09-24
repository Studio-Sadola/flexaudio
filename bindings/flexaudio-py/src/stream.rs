//! The recording stream [`Stream`], the entry point `open()`, and VAD / denoise integration.
//!
//! The Python [`Stream`] is pull/poll-style. It has no bridge thread as in napi; the caller
//! calls `poll_chunk` / `poll_event` periodically. The integrated VAD / denoise processing
//! also runs on the spot where poll_chunk is called (under the GIL) (processed when called).

use pyo3::prelude::*;
use pyo3::types::PyDict;

use ::flexaudio as fa;
use flexaudio_denoise::Denoiser as CoreDenoiser;
use flexaudio_vad::Vad as CoreVad;

use crate::config::{build_config, vad_config_from_dict, validate_denoise};
use crate::marshal::{chunk_to_py, event_to_py, PyAudioChunk, PyStreamEvent};
use crate::{denoise_err_to_py, to_py_err, vad_err_to_py};

/// Handle to a recording stream. Polls the inner `flexaudio::Stream` directly.
///
/// `open(...)` returns it after going through `start()`. The caller calls `poll_chunk` /
/// `poll_event` periodically. `stop()` stops it, and as a context manager (`with`) it stops in
/// `__exit__`.
///
/// Integrated add-ons:
/// - Opened with `denoise=True`, the chunk audio is overwritten via RNNoise before
///   `poll_chunk` returns it (48kHz output only; the first 480 samples/ch are silent due to
///   the denoise latency).
/// - Opened with `vad={...}`, each chunk's audio is run through the VAD and the finalized
///   speech boundaries are attached to `chunk.vad_events`. The processing order is
///   denoise → VAD.
///
/// The rubato resampler held by the integrated VAD is !Sync, so the pyclass default of
/// Send+Sync cannot be met. Python usage is assumed to be poll-style and single-threaded
/// (under the GIL), so the class is made unsendable and pinned to the thread that created it
/// (uniformly unsendable, even when VAD is not used).
#[pyclass(module = "flexaudio", unsendable)]
pub struct Stream {
    inner: fa::Stream,
    // Integrated denoise (assumes 48kHz). None when disabled.
    denoiser: Option<CoreDenoiser>,
    // Integrated VAD. None when disabled.
    vad: Option<CoreVad>,
    // Output format. Passed to the VAD's process_pcm and also used for denoise's 48kHz
    // precondition check. switch_source cannot change the output format, so these keep the
    // values from open.
    output_rate: u32,
    output_channels: u16,
}

impl Stream {
    /// Builds the add-on state (VAD / denoise) from Python arguments. The denoise 48kHz
    /// precondition and Denoiser / Vad construction failures are validated and converted here.
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
    /// Stops recording. Safe to call twice (idempotent on the flexaudio side).
    fn stop(&mut self) {
        self.inner.stop();
    }

    /// Pauses delivery only, without stopping recording. `resume` resumes it.
    fn pause(&self) {
        self.inner.pause();
    }

    /// Clears the pause and resumes delivery.
    fn resume(&self) {
        self.inner.resume();
    }

    /// Returns whether delivery is paused.
    fn is_paused(&self) -> bool {
        self.inner.is_paused()
    }

    /// Changes the input gain (linear multiplier). 1.0 leaves it unchanged, 2.0 is about +6dB,
    /// 0.0 is silence. Can be called at any time while recording and takes effect from the
    /// next chunk (20ms granularity). Samples after multiplication are clamped to ±1.0. Raises
    /// `ValueError` unless finite and 0 or greater.
    fn set_gain(&self, gain: f32) -> PyResult<()> {
        self.inner.set_gain(gain).map_err(to_py_err)
    }

    /// Returns the current input gain (linear multiplier).
    fn gain(&self) -> f32 {
        self.inner.gain()
    }

    /// Returns the source's native format `(sample_rate, channels)` (the shape of the actual
    /// input before the first resampling stage). After a source switch it reflects the new
    /// source.
    fn native_format(&self) -> (u32, u16) {
        self.inner.native_format()
    }

    /// Returns the cumulative number of chunks discarded because the ring overflowed (total
    /// since start).
    fn dropped_chunks(&self) -> u64 {
        self.inner.dropped_chunks()
    }

    /// Returns a chunk if one is available. `None` if there is none (non-blocking).
    ///
    /// If integrated add-ons are enabled, the chunk is processed here before being returned
    /// (the order is denoise → VAD). denoise overwrites the chunk audio in place, and VAD
    /// detects speech boundaries on the processed audio and attaches them to
    /// `chunk.vad_events`. With both disabled, the chunk passes through unchanged.
    fn poll_chunk(&mut self) -> Option<PyAudioChunk> {
        let chunk = self.inner.poll_chunk()?;
        let mut py_chunk = chunk_to_py(chunk);

        // 1) denoise: overwrite the chunk audio in place. The length is a multiple of the
        //    output channels (frames * channels) and always divides evenly, so no error occurs,
        //    but an unexpected failure (length mismatch) falls back best-effort to passing
        //    through (poll is not stopped).
        if let Some(dn) = self.denoiser.as_mut() {
            let _ = dn.process(py_chunk.samples_mut());
        }

        // 2) VAD: detect speech boundaries on the processed audio and attach them.
        //    process_pcm downmixes to mono and resamples to the VAD rate internally, so the
        //    output format can be passed as-is.
        if let Some(vad) = self.vad.as_mut() {
            let events: Vec<(bool, u64)> = vad
                .process_pcm(py_chunk.samples(), self.output_rate, self.output_channels)
                .into_iter()
                .map(|ev| match ev {
                    flexaudio_vad::VadEvent::SpeechStart { at_sample } => (true, at_sample),
                    flexaudio_vad::VadEvent::SpeechEnd { at_sample } => (false, at_sample),
                })
                .collect();
            py_chunk.set_vad_events(events);
        }

        Some(py_chunk)
    }

    /// Returns an event if one is available. `None` if there is none (non-blocking).
    fn poll_event(&mut self) -> Option<PyStreamEvent> {
        self.inner.poll_event().map(event_to_py)
    }

    /// Hot-swaps the input source (mic/system/process/mix) without stopping recording.
    ///
    /// The output format (output_rate/output_channels) cannot be changed by a switch.
    /// Requesting a change makes `switch_source` return an error, which is raised here.
    /// `gain` is accepted too but the core ignores it (gain is stream state; change it with
    /// `set_gain`). `mic_device_id`/`system_device_id`/`mic_gain`/`system_gain` are mix-only
    /// (ignored for other sources).
    ///
    /// `vad` / `denoise` re-specify the integrated add-ons. Changing the source makes the audio
    /// discontinuous, so the add-ons are rebuilt as specified (internal state is reset).
    /// Omitting them means the defaults (`vad=None` / `denoise=False`) = add-ons disabled
    /// (same convention as open; specify them explicitly on every switch).
    #[pyo3(signature = (
        kind,
        *,
        device_id = None,
        process_id = None,
        mode = "include".to_string(),
        exclude_self = false,
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
        // The add-ons depend on the output format. A switch does not change the output format,
        // so validate and build them using the output_rate/output_channels from open (the
        // output_rate argument is used by the core's switch_source to check the shape matches).
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

        // Replace the add-ons only after the switch succeeds (on failure the old add-ons are
        // kept).
        self.vad = new_vad;
        self.denoiser = new_denoiser;
        Ok(())
    }

    /// Context manager support. Usable as `with flexaudio.open("mic") as s:`.
    fn __enter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    /// Stops when leaving the `with` block. Exceptions are not swallowed (returns False).
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

/// Opens a stream, goes through `start()`, and returns a [`Stream`].
///
/// `kind` is "mic"|"system"|"process"|"mix". Invalid values raise `ValueError`. In an
/// environment without devices, open / start raise flexaudio's error (converted to
/// `RuntimeError` etc.). `mic_device_id`/`system_device_id`/`mic_gain`/`system_gain` are
/// mix-only and determine the device selection and pre-mix multiplier for mix's mic side /
/// system side (ignored for other sources; the global `gain` is applied after mixing).
///
/// Integrated add-ons:
/// - `vad` (dict, default None): when given, the integrated VAD is enabled. The keys are the
///   same as the arguments of the standalone `Vad` (`threshold` / `min_speech_ms` /
///   `min_silence_ms` / `speech_pad_ms` / `max_speech_ms` / `sample_rate` /
///   `neg_threshold`). Speech boundaries go into each chunk's `vad_events`.
/// - `denoise` (bool, default False): True enables RNNoise noise suppression. 48kHz output
///   only; `denoise=True` with `output_rate!=48000` raises `ValueError`. The processing order
///   is denoise → VAD.
#[pyfunction]
#[pyo3(signature = (
    kind,
    *,
    device_id = None,
    process_id = None,
    mode = "include".to_string(),
    exclude_self = false,
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
    // Validate and build the add-ons first (the denoise 48kHz precondition and invalid VAD
    // settings are rejected here; we want to fail before grabbing the device).
    let (vad_state, denoiser) =
        Stream::build_addons(vad.as_ref(), denoise, output_rate, output_channels)?;

    let config = build_config(
        kind,
        device_id,
        process_id,
        &mode,
        exclude_self,
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
