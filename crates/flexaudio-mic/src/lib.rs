//! flexaudio-mic: microphone input backend via cpal (all OSes).
//!
//! [`CpalMicBackend`] is a [`CaptureBackend`] implementation that takes raw interleaved `f32`
//! frames from an input device via cpal and pushes them into a [`RawSink`] without blocking.
//! It is verified mainly on Linux/ALSA, but works on any OS that cpal supports.
//!
//! # `cpal::Stream` is `!Send`, so it is confined to an owner thread
//! [`CaptureBackend`] requires `Send`, but [`cpal::Stream`] is `!Send`, so it cannot be held
//! directly in the backend struct. [`start`](CpalMicBackend::start) spawns a thread, which
//! builds the stream + calls `play()` and then `park`s until the stop signal. On stop, that
//! thread drops the Stream and capture stops. The struct itself holds only `Send` things (the
//! stop flag, the [`JoinHandle`], and the cached format).
//!
//! ```no_run
//! use flexaudio_mic::CpalMicBackend;
//! use flexaudio_core::{CaptureBackend, RawSink, raw_ring};
//!
//! // Default input device (device_id = None). To select a specific device, use
//! // `CpalMicBackend::new(Some("device name".into()))` (id = device name).
//! let mut backend = CpalMicBackend::new(None);
//! let (rate, channels) = backend.native_format();
//! let (prod, _cons) = raw_ring(rate as usize * channels as usize); // 1 second's worth
//! let sink = RawSink::new(prod, rate, channels);
//! backend.start(sink).unwrap();
//! // ... pop raw frames from _cons ...
//! backend.stop();
//! ```

#![warn(missing_docs)]

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, SampleFormat};

use flexaudio_core::backend::{CaptureBackend, RawSink};
use flexaudio_core::clock::monotonic_now_ns;
use flexaudio_core::types::{DeviceInfo, Error, Result, SourceKind};

#[cfg(windows)]
mod windows_keeper;

/// Default format `(48000 Hz, mono)` returned by
/// [`native_format`](CpalMicBackend::native_format) when no input device can be obtained. If
/// there is no device at `start`, it fails with [`Error::DeviceNotFound`].
const FALLBACK_FORMAT: (u32, u16) = (48_000, 1);

/// The single entry point that returns cpal's default host.
///
/// On Windows, cpal 0.16's process-wide WASAPI enumerator depends on the COM lifetime of the
/// STA thread that first created it. By finishing initialization of the enumerator on the
/// keeper before returning the host, this structurally prevents the race where a short-lived
/// calling thread becomes the first creator. On other OSes no keeper is created, and
/// `cpal::default_host()` is returned as before.
fn cpal_default_host() -> Result<cpal::Host> {
    #[cfg(windows)]
    windows_keeper::ensure()?;

    Ok(cpal::default_host())
}

/// Microphone input capture backend via cpal.
///
/// Takes raw interleaved `f32` frames from the default input device (`device_id = None`) or an
/// input device selected by device name (`device_id = Some(id)`) and streams them into a
/// [`RawSink`]. See the module documentation for details.
///
/// `Send`. It holds only the stop flag, the [`JoinHandle`], the cached format, and the
/// device_id; the `!Send` [`cpal::Stream`] is confined within the owner thread.
pub struct CpalMicBackend {
    /// Stop instruction for the owner thread. `true` makes it drop the stream and exit.
    stop_flag: Arc<AtomicBool>,
    /// Handle of the thread that owns the cpal stream (`Some` after start).
    handle: Option<JoinHandle<()>>,
    /// Native format queried and cached at `new`.
    native: (u32, u16),
    /// ID (device name) of the input device to select. `None` for the default input device.
    device_id: Option<String>,
}

impl CpalMicBackend {
    /// Constructs a microphone backend.
    ///
    /// `device_id`:
    /// - `None` → the default input device (`host.default_input_device()`).
    /// - `Some(id)` → scans `host.input_devices()` for the first device with
    ///   `device.name()? == id` (id is a device name returned by [`list_devices`]).
    ///
    /// Queries and caches the native format of the selected device. If there is no device, no
    /// match, or the query fails, `FALLBACK_FORMAT` (`(48000, 1)`) is cached. new itself never
    /// panics or errors and always succeeds; if device_id does not match,
    /// [`start`](Self::start) fails with [`Error::DeviceNotFound`].
    pub fn new(device_id: Option<String>) -> Self {
        let native = query_native_format(device_id.as_deref()).unwrap_or(FALLBACK_FORMAT);
        Self {
            stop_flag: Arc::new(AtomicBool::new(false)),
            handle: None,
            native,
            device_id,
        }
    }
}

impl Default for CpalMicBackend {
    fn default() -> Self {
        Self::new(None)
    }
}

/// Resolves the input device for `device_id` from the cpal host.
///
/// - `None` → `host.default_input_device()` ([`Error::DeviceNotFound`] if unavailable).
/// - `Some(id)` → scans `host.input_devices()` and returns the first match with
///   `device.name()? == id`. [`Error::DeviceNotFound`] if none.
///
/// Devices whose name cannot be obtained cannot be compared, so they are skipped. Environments
/// where `input_devices()` itself fails (no ALSA, etc.) are also mapped to
/// [`Error::DeviceNotFound`].
fn resolve_input_device(host: &cpal::Host, device_id: Option<&str>) -> Result<Device> {
    match device_id {
        None => host.default_input_device().ok_or(Error::DeviceNotFound),
        // The first device whose device name matches.
        Some(id) => {
            let devices = host.input_devices().map_err(|_| Error::DeviceNotFound)?;
            for device in devices {
                // Devices whose name cannot be obtained cannot be compared, so skip them.
                if let Ok(name) = device.name() {
                    if name == id {
                        return Ok(device);
                    }
                }
            }
            Err(Error::DeviceNotFound)
        }
    }
}

/// Obtains the native format `(sample_rate, channels)` of the input device selected by
/// `device_id`. Returns `None` if resolving the device or getting its config fails (the caller
/// falls back to [`FALLBACK_FORMAT`]).
fn query_native_format(device_id: Option<&str>) -> Option<(u32, u16)> {
    let host = cpal_default_host().ok()?;
    let device = resolve_input_device(&host, device_id).ok()?;
    let config = device.default_input_config().ok()?;
    Some((config.sample_rate().0, config.channels()))
}

/// Enumerates input (microphone) devices. The microphone part of `devices()`.
///
/// Scans `host.input_devices()` and maps each device to a [`DeviceInfo`]:
/// - `id` / `name`: cpal has no persistent ID, so the device name is put in both as a stand-in
///   ID (because the index changes on reconnect; the same configuration gives the same name).
/// - `sample_rate` / `channels`: from `default_input_config()`. Devices where it cannot be
///   obtained (e.g. that cannot actually be opened) are skipped.
/// - `source_kind = Mic` / `is_loopback = false`.
/// - `is_default`: `true` if it matches the name of `host.default_input_device()`.
///
/// In environments with no device or where host initialization fails, returns an empty `Vec`
/// (no panic). Multiple devices with the same name can produce duplicate ids, but cpal offers
/// no more stable key, so this is accepted.
pub fn list_devices() -> Result<Vec<DeviceInfo>> {
    let host = cpal_default_host()?;

    // Default input device name (for the is_default check). If unavailable, nothing is marked
    // as default.
    let default_name = host.default_input_device().and_then(|d| d.name().ok());

    // Environments where input_devices() itself fails (no ALSA, etc.) are treated as an empty
    // list.
    let devices = match host.input_devices() {
        Ok(it) => it,
        Err(_) => return Ok(Vec::new()),
    };

    let mut out = Vec::new();
    for device in devices {
        // Devices whose name cannot be obtained cannot produce an ID, so skip them.
        let Ok(name) = device.name() else {
            continue;
        };
        // No default input config = advertised but cannot actually be opened. Skip.
        let Ok(config) = device.default_input_config() else {
            continue;
        };
        let is_default = default_name.as_deref() == Some(name.as_str());
        out.push(DeviceInfo {
            id: name.clone(),
            name,
            source_kind: SourceKind::Mic,
            sample_rate: config.sample_rate().0,
            channels: config.channels(),
            is_loopback: false,
            is_default,
        });
    }
    Ok(out)
}

impl CaptureBackend for CpalMicBackend {
    fn native_format(&self) -> (u32, u16) {
        self.native
    }

    fn start(&mut self, sink: RawSink) -> Result<()> {
        // Do nothing if the owner thread is already alive (safe against a double start).
        if self.handle.is_some() {
            return Ok(());
        }
        // Reset the flag so it can be started again even after a previous stop.
        self.stop_flag.store(false, Ordering::SeqCst);

        let stop_flag = self.stop_flag.clone();
        // cpal::Device is !Send, so pass only the device_id string and resolve it in the thread.
        let device_id = self.device_id.clone();
        // Ready channel that returns the build/play result from the owner thread to start().
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();

        let handle = thread::Builder::new()
            .name("flexaudio-mic-cpal".into())
            .spawn(move || {
                run_capture_thread(sink, device_id, stop_flag, ready_tx);
            })
            .map_err(|e| Error::Backend(format!("spawn cpal mic thread: {e}")))?;

        // Wait for the owner thread to report whether it could build + play the stream.
        match ready_rx.recv() {
            Ok(Ok(())) => {
                self.handle = Some(handle);
                Ok(())
            }
            Ok(Err(e)) => {
                // build/play failed. The owner thread exits right after sending ready, so join.
                let _ = handle.join();
                Err(e)
            }
            // The owner thread died before sending ready (should not normally happen).
            Err(_) => {
                let _ = handle.join();
                Err(Error::Backend(
                    "cpal mic thread exited before reporting readiness".into(),
                ))
            }
        }
    }

    fn stop(&mut self) {
        // Do nothing if there is no handle (safe against re-entry and a double stop).
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            // The owner thread is parked. Wake it with unpark so it drops the Stream and exits.
            h.thread().unpark();
            let _ = h.join();
        }
    }
}

impl Drop for CpalMicBackend {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Owner thread body. Builds + plays the cpal input stream and parks until stopped.
///
/// Reports the build/play result to [`CpalMicBackend::start`] via `ready_tx`. After success, it
/// parks while keeping `stream` alive until `stop_flag` is set, and once it is set, it returns
/// from the function, dropping `stream` and thereby stopping capture.
fn run_capture_thread(
    sink: RawSink,
    device_id: Option<String>,
    stop_flag: Arc<AtomicBool>,
    ready_tx: mpsc::Sender<Result<()>>,
) {
    let stream = match build_stream(sink, device_id.as_deref()) {
        Ok(s) => s,
        Err(e) => {
            // Report the failure and exit immediately.
            let _ = ready_tx.send(Err(e));
            return;
        }
    };

    if let Err(e) = stream.play() {
        let _ = ready_tx.send(Err(Error::Backend(format!("cpal play: {e}"))));
        return;
    }

    // Reaching here means startup succeeded.
    let _ = ready_tx.send(Ok(()));

    // Park while keeping the stream alive until the stop signal.
    // Check stop_flag every time in case of spurious wakeups.
    while !stop_flag.load(Ordering::SeqCst) {
        thread::park();
    }
    // Leaving here drops the stream and stops capture.
    drop(stream);
}

/// Maximum block length (seconds) assumed when pre-allocating the scratch for the RT
/// conversion callback.
///
/// The maximum frames per callback is estimated as native SR×ch × this many seconds and
/// allocated at stream setup. Real hardware blocks are normally a few ms to tens of ms, so with
/// 1 second's worth no capacity growth (= allocation inside RT) happens in steady state. Even if
/// an unexpectedly huge block arrives, [`fill_scratch`] grows it once with `reserve` and keeps
/// that capacity afterwards (no panic).
const MAX_SCRATCH_SECONDS: usize = 1;

/// In the RT callback of the conversion paths (I16/U16/I32), fills the pre-allocated scratch
/// while converting the interleaved input.
///
/// `scratch` is allocated at stream setup with the maximum block length. In steady state it
/// stays within capacity, so `clear` + `push` cause no reallocation. Only when `n` exceeds the
/// capacity is it grown once with `reserve`, and that capacity is kept afterwards. `convert`
/// is applied to each sample.
#[inline]
fn fill_scratch<T: Copy>(scratch: &mut Vec<f32>, data: &[T], convert: impl Fn(T) -> f32) {
    let n = data.len();
    // Within capacity, reserve does nothing. It grows once only when capacity is exceeded.
    if n > scratch.capacity() {
        scratch.reserve(n - scratch.capacity());
    }
    scratch.clear();
    for &s in data {
        scratch.push(convert(s));
    }
}

/// f32 peak amplitude threshold for judging a buffer to be a priming transient.
///
/// By contract, flexaudio's f32 samples are in `[-1.0, 1.0]`. On Linux, the PipeWire ALSA
/// compatibility bridge (`default` PCM), when a stream is opened from a cold state, emits
/// full-scale square-wave dummy priming buffers far outside the range for the first few hundred
/// ms (measured peak ≈ 3.3; left and right are nearly antiphase, so DC≈0 at the source stage,
/// but downstream rate conversion turns it into a huge DC and clipping). Normal audio is capped
/// at 1.0 by contract, so a buffer whose peak clearly exceeds 1.0 can be treated as a
/// transient. It is placed just above 1.0 so that normal audio at exactly ±1.0 is not caught.
const PRIMING_PEAK_LIMIT: f32 = 1.001;

/// Guard that discards priming transient buffers right after capture starts.
///
/// Transient buffers are full-scale square waves exceeding `[-1.0, 1.0]`, so only buffers whose
/// peak exceeds [`PRIMING_PEAK_LIMIT`] are discarded.
///
/// Discarding a fixed number of seconds from the head would create leading silence even in
/// environments without the transient (Mac / Windows / warm Linux). Because this judges a
/// transient by the peak alone, it discards not a single buffer in environments without the
/// transient, and automatically follows the actual length of the transient.
///
/// It discards only while the leading buffers look like a transient (out of range); once one
/// in-range buffer arrives, it lets everything through forever after (latch-open). The
/// transient appears only right after startup and decays monotonically, so it does not recur
/// midway. It is used only inside the RT callback, so the check is just an `abs` comparison in
/// a single pass over the buffer.
struct TransientGuard {
    /// Whether a normal (in-range) buffer has already been passed. From `true` on, everything
    /// passes.
    latched: bool,
}

impl TransientGuard {
    fn new() -> Self {
        Self { latched: false }
    }

    /// Given an interleaved f32 buffer, returns `true` if it is a priming transient (to be
    /// discarded). Once any in-range buffer has been passed, it always returns `false` (pass).
    fn should_drop(&mut self, data: &[f32]) -> bool {
        if self.latched || data.is_empty() {
            // Already in the normal section. Or an empty buffer (no point in discarding it).
            self.latched = true;
            return false;
        }
        // Find the peak amplitude in a single pass over the buffer.
        let mut peak = 0.0f32;
        for &s in data {
            let a = s.abs();
            if a > peak {
                peak = a;
            }
        }
        // Clearly exceeding the contract range = priming transient.
        let is_transient = peak > PRIMING_PEAK_LIMIT;
        if !is_transient {
            self.latched = true;
        }
        is_transient
    }
}

/// Builds an input stream on the input device for `device_id` (does not `play` yet).
/// `device_id = None` selects the default input device. [`Error::DeviceNotFound`] if no device
/// matches.
///
/// Branches the callback by sample format: F32 is passed as-is, and I16/U16/I32 are converted to
/// `f32` `[-1.0, 1.0]` and passed to [`RawSink::push`]. Priming transient buffers right after
/// start are discarded by [`TransientGuard`] (a workaround for the PipeWire ALSA bridge).
fn build_stream(sink: RawSink, device_id: Option<&str>) -> Result<cpal::Stream> {
    let host = cpal_default_host()?;
    // None=default / Some=first name match. No match is DeviceNotFound.
    let device = resolve_input_device(&host, device_id)?;

    // No default input config = the advertised device cannot actually be opened
    // (including the case where the ALSA "default" PCM cannot be opened, e.g. on a server with
    // no sound card). This is equivalent to having no usable input device, so map it to
    // DeviceNotFound.
    let supported = device
        .default_input_config()
        .map_err(|_| Error::DeviceNotFound)?;
    let sample_format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();

    let err_fn = |e: cpal::StreamError| {
        // Error callback outside the RT path. No logging is wired yet, so it is silently
        // ignored for now (TODO: map to Event::DeviceLost etc. in the wiring layer).
        let _ = e;
    };

    // Pre-allocate the conversion-path scratch with the maximum block length (native SR×ch ×
    // MAX_SCRATCH_SECONDS), to avoid first/growth allocations inside the RT callback (xrun
    // risk) in steady state. At least 1 is allocated.
    let scratch_cap = (config.sample_rate.0 as usize)
        .saturating_mul(config.channels as usize)
        .saturating_mul(MAX_SCRATCH_SECONDS)
        .max(1);

    // The sink is moved into the callback. Non-F32 formats are enclosed for conversion. The
    // transient check operates on f32 values, so for converted formats it runs after conversion.
    //
    // cpal's data callback is called across an FFI (C ABI) boundary, so a panic here could be
    // undefined behavior. There is no live panic path now, but as a precaution each callback
    // body is wrapped in catch_unwind so that an unexpected panic only discards that block.
    let stream = match sample_format {
        SampleFormat::F32 => {
            let mut sink = sink;
            let mut guard = TransientGuard::new();
            device.build_input_stream(
                &config,
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    let _ = catch_unwind(AssertUnwindSafe(|| {
                        // Already interleaved f32. Discard if it is a priming transient.
                        if guard.should_drop(data) {
                            return;
                        }
                        sink.push(data, monotonic_now_ns());
                    }));
                },
                err_fn,
                None,
            )
        }
        SampleFormat::I16 => {
            let mut sink = sink;
            // Conversion scratch. Pre-allocated with the maximum block length so its capacity
            // does not grow inside RT.
            let mut scratch: Vec<f32> = Vec::with_capacity(scratch_cap);
            let mut guard = TransientGuard::new();
            device.build_input_stream(
                &config,
                move |data: &[i16], _: &cpal::InputCallbackInfo| {
                    let _ = catch_unwind(AssertUnwindSafe(|| {
                        fill_scratch(&mut scratch, data, |s| s as f32 / -(i16::MIN as f32));
                        if guard.should_drop(&scratch) {
                            return;
                        }
                        sink.push(&scratch, monotonic_now_ns());
                    }));
                },
                err_fn,
                None,
            )
        }
        SampleFormat::U16 => {
            let mut sink = sink;
            let mut scratch: Vec<f32> = Vec::with_capacity(scratch_cap);
            let mut guard = TransientGuard::new();
            device.build_input_stream(
                &config,
                move |data: &[u16], _: &cpal::InputCallbackInfo| {
                    let _ = catch_unwind(AssertUnwindSafe(|| {
                        // u16 [0, 65535] to [-1, 1) around the midpoint 32768.
                        fill_scratch(&mut scratch, data, |s| (s as f32 - 32_768.0) / 32_768.0);
                        if guard.should_drop(&scratch) {
                            return;
                        }
                        sink.push(&scratch, monotonic_now_ns());
                    }));
                },
                err_fn,
                None,
            )
        }
        SampleFormat::I32 => {
            let mut sink = sink;
            let mut scratch: Vec<f32> = Vec::with_capacity(scratch_cap);
            let mut guard = TransientGuard::new();
            device.build_input_stream(
                &config,
                move |data: &[i32], _: &cpal::InputCallbackInfo| {
                    let _ = catch_unwind(AssertUnwindSafe(|| {
                        fill_scratch(&mut scratch, data, |s| s as f32 / -(i32::MIN as f32));
                        if guard.should_drop(&scratch) {
                            return;
                        }
                        sink.push(&scratch, monotonic_now_ns());
                    }));
                },
                err_fn,
                None,
            )
        }
        other => {
            return Err(Error::Backend(format!(
                "unsupported cpal sample format: {other:?}"
            )));
        }
    };

    stream.map_err(|e| Error::Backend(format!("build_input_stream: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use flexaudio_core::raw_ring;

    /// Generates an interleaved stereo f32 buffer of `frames` frames.
    /// Each sample alternates `±peak` (a square wave of peak amplitude `peak`).
    fn make_buf(frames: usize, peak: f32) -> Vec<f32> {
        let mut v = Vec::with_capacity(frames * 2);
        for i in 0..frames {
            let s = if i % 2 == 0 { peak } else { -peak };
            v.push(s); // L
            v.push(s); // R
        }
        v
    }

    /// [`TransientGuard`], given the sequence "out-of-range full scale (transient) → decay →
    /// in range", discards only the leading part (buffers whose peak exceeds
    /// [`PRIMING_PEAK_LIMIT`]), latches once an in-range buffer arrives, and passes everything
    /// afterwards.
    #[test]
    fn transient_guard_drops_priming_then_latches_open() {
        let frames = 1024;
        let mut g = TransientGuard::new();

        // Out-of-range full-scale square wave (equivalent to the measured priming transient,
        // peak≈3.3) → discarded.
        assert!(g.should_drop(&make_buf(frames, 3.3)));
        // Decaying but still out of range (peak=1.5 > LIMIT) → discarded.
        assert!(g.should_drop(&make_buf(frames, 1.5)));
        // Normal audio back in range (peak=0.88) → passed = latches here.
        assert!(!g.should_drop(&make_buf(frames, 0.88)));
        // After the latch, everything is always passed, even an out-of-range buffer (prevents
        // a midway recurrence).
        assert!(!g.should_drop(&make_buf(frames, 3.3)));
    }

    /// In environments with no transient at all (Mac/Win/warm Linux) the audio is in range from
    /// the start, so [`TransientGuard`] discards not a single buffer (zero leading silence).
    #[test]
    fn transient_guard_passes_clean_audio_from_the_start() {
        let frames = 1024;
        let mut g = TransientGuard::new();
        // No false positive even at exactly digital full scale ±1.0 (LIMIT is just above 1.0).
        assert!(!g.should_drop(&make_buf(frames, 1.0)));
        assert!(!g.should_drop(&make_buf(frames, 0.5)));
        // Silence (all zeros) is not discarded either.
        assert!(!g.should_drop(&vec![0.0f32; frames * 2]));
    }

    /// An empty buffer is not discarded and latches.
    #[test]
    fn transient_guard_handles_empty_buffer() {
        let mut g = TransientGuard::new();
        assert!(!g.should_drop(&[]));
        // It latched on empty, so later out-of-range buffers are passed too.
        assert!(!g.should_drop(&make_buf(1024, 3.3)));
    }

    /// [`fill_scratch`] causes no reallocation within capacity, and the conversion is correct.
    /// Guarantees no steady-state allocation in the RT callback.
    #[test]
    fn fill_scratch_no_realloc_in_steady_state() {
        // Allocate with the maximum assumed block length.
        let cap = 480 * 2; // equivalent to 10ms @ 48k stereo
        let mut scratch: Vec<f32> = Vec::with_capacity(cap);
        let before = scratch.capacity();

        // No matter how many times an in-capacity block is filled, the capacity does not change
        // (= no reallocation happens).
        let data: Vec<i16> = (0..cap as i16).collect();
        for _ in 0..100 {
            fill_scratch(&mut scratch, &data, |s| s as f32 / -(i16::MIN as f32));
            assert_eq!(scratch.len(), data.len());
            assert_eq!(
                scratch.capacity(),
                before,
                "capacity does not grow in steady state"
            );
        }
        // The conversion is correct (i16::MIN maps to -1.0).
        let mut one = Vec::with_capacity(1);
        fill_scratch(&mut one, &[i16::MIN], |s| s as f32 / -(i16::MIN as f32));
        assert_eq!(one[0], -1.0);
    }

    /// `new` + `native_format` do not panic (whether or not an input device exists).
    /// new always succeeds with device_id = None (default) as well as Some (a specific device).
    #[test]
    fn new_and_native_format_do_not_panic() {
        // Default input device (device_id = None).
        let backend = CpalMicBackend::new(None);
        let (rate, channels) = backend.native_format();
        // The format is always positive (FALLBACK_FORMAT if there is no device).
        assert!(rate > 0);
        assert!(channels > 0);

        // Even with a nonexistent device_id, new succeeds without panicking and returns
        // FALLBACK_FORMAT (by design, surfacing the resolution failure is deferred to
        // start/build_stream).
        let backend = CpalMicBackend::new(Some("__no_such_device__".into()));
        let (rate, channels) = backend.native_format();
        assert_eq!((rate, channels), FALLBACK_FORMAT);
    }

    /// `start` with a nonexistent device_id does not panic and returns
    /// [`Error::DeviceNotFound`] (consistent with cold-start/TransientGuard).
    /// Regardless of whether a default input device exists, a non-matching id is always
    /// DeviceNotFound.
    #[test]
    fn start_with_unknown_device_id_yields_device_not_found() {
        let mut backend = CpalMicBackend::new(Some("__no_such_device__".into()));
        let (rate, channels) = backend.native_format();
        let cap = (rate as usize * channels as usize).max(1);
        let (prod, _cons) = raw_ring(cap);
        let sink = RawSink::new(prod, rate, channels);

        match backend.start(sink) {
            Err(Error::DeviceNotFound) => {}
            other => panic!("an unknown device_id should be DeviceNotFound: {other:?}"),
        }
    }

    /// [`list_devices`] never panics and returns `Ok(Vec)` whether or not devices exist.
    /// Every returned device is `Mic` / non-loopback and has a stable key with `id == name`.
    #[test]
    fn list_devices_never_panics_and_is_consistent() {
        let devices = list_devices().expect("list_devices is designed not to return Err");
        for d in &devices {
            assert_eq!(d.source_kind, SourceKind::Mic);
            assert!(!d.is_loopback, "a microphone is not a loopback");
            // Stable key: with cpal, the device name is used as the id.
            assert_eq!(d.id, d.name);
            assert!(!d.id.is_empty(), "id (=name) is not empty");
            assert!(d.sample_rate > 0);
            assert!(d.channels > 0);
        }
        // At most one default input.
        assert!(devices.iter().filter(|d| d.is_default).count() <= 1);
    }

    /// `start` can return `Err(DeviceNotFound)` in environments with no input device (servers,
    /// CI, etc.). Both Ok and Err(DeviceNotFound) are accepted; only a panic is not allowed.
    /// In environments with an input device, capture actually starts and stops with stop.
    #[test]
    fn start_then_stop_tolerates_missing_device() {
        let mut backend = CpalMicBackend::new(None);
        let (rate, channels) = backend.native_format();
        let cap = (rate as usize * channels as usize).max(1); // about 1 second
        let (prod, _cons) = raw_ring(cap);
        let sink = RawSink::new(prod, rate, channels);

        match backend.start(sink) {
            Ok(()) => {
                // In environments where it started, stopping must be safe.
                backend.stop();
                // A double stop is safe too.
                backend.stop();
            }
            Err(Error::DeviceNotFound) => {
                // Accepted in environments without an input device (CI/server).
            }
            Err(other) => panic!("unexpected error from start(): {other:?}"),
        }
    }

    /// End-to-end test that actually records from a real microphone. Run it with
    /// `cargo test -p flexaudio-mic -- --ignored` on a laptop or similar machine with an input
    /// device. Servers/CI have no input device, so it is `#[ignore]` by default.
    #[test]
    #[ignore = "needs a real mic; run on a laptop with `cargo test -p flexaudio-mic -- --ignored`"]
    fn end_to_end_captures_real_audio() {
        use std::time::Duration;

        let mut backend = CpalMicBackend::new(None);
        let (rate, channels) = backend.native_format();
        let cap = rate as usize * channels as usize * 2; // about 2 seconds
        let (prod, mut cons) = raw_ring(cap);
        let sink = RawSink::new(prod, rate, channels);

        backend
            .start(sink)
            .expect("start() should succeed with a real input device");

        // Capture for a few hundred milliseconds and confirm samples flow in.
        thread::sleep(Duration::from_millis(500));
        backend.stop();

        let mut buf = vec![0.0f32; cap];
        let got = cons.pop_slice(&mut buf);
        assert!(got > 0, "expected captured samples, got none");
        // Samples fall within the [-1, 1] range (sanity of the conversion).
        assert!(buf[..got].iter().all(|&s| (-1.5..=1.5).contains(&s)));
    }
}
