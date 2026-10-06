//! flexaudio-mic — cross-platform microphone input backend using cpal.
//!
//! [`CpalMicBackend`] implements [`CaptureBackend`]. It reads raw interleaved `f32` frames
//! from an input device through cpal and pushes them to [`RawSink`] without blocking. It is
//! tested mainly on Linux/ALSA, but works on any OS supported by cpal.
//!
//! # Keep `cpal::Stream` on its owner thread because it is `!Send`
//! [`CaptureBackend`] requires `Send`, but [`cpal::Stream`] is `!Send` and cannot be stored
//! directly in the backend struct. [`start`](CpalMicBackend::start) spawns a thread that
//! builds and plays the stream, then parks until signaled to stop. That thread drops the
//! stream on shutdown, stopping capture. The struct stores only `Send` values (a stop flag,
//! [`JoinHandle`], and the cached format).
//!
//! ```no_run
//! use flexaudio_mic::CpalMicBackend;
//! use flexaudio_core::{CaptureBackend, RawSink, raw_ring};
//!
//! // Default input device (device_id = None). To choose a specific device, use
//! // `CpalMicBackend::new(Some("device name".into()))` (id = device name).
//! let mut backend = CpalMicBackend::new(None);
//! let (rate, channels) = backend.native_format();
//! let (prod, _cons) = raw_ring(rate as usize * channels as usize); // one second of audio
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

/// Fallback format `(48000 Hz, mono)` returned by
/// [`native_format`](CpalMicBackend::native_format) when no input device is available.
/// If no device exists at `start`, return [`Error::DeviceNotFound`].
const FALLBACK_FORMAT: (u32, u16) = (48_000, 1);

/// The only entry point that returns cpal's default host.
///
/// On Windows, cpal 0.16's process-wide WASAPI enumerator depends on the COM lifetime of the
/// STA thread that first creates it. Initialize the enumerator through a keeper before
/// returning the host, preventing a short-lived caller thread from becoming the first
/// creator. Other OSes skip the keeper and return `cpal::default_host()` as before.
fn cpal_default_host() -> Result<cpal::Host> {
    #[cfg(windows)]
    windows_keeper::ensure()?;

    Ok(cpal::default_host())
}

/// Microphone input capture backend using cpal.
///
/// Reads raw interleaved `f32` frames from the default input device (`device_id = None`) or
/// from a device selected by name (`device_id = Some(id)`), then sends them to [`RawSink`].
/// See the module docs for details.
///
/// This type is `Send`. It stores only a stop flag, [`JoinHandle`], cached format, and
/// device_id; the `!Send` [`cpal::Stream`] stays on its owner thread.
pub struct CpalMicBackend {
    /// Stop signal for the owner thread. When `true`, it drops the stream and exits.
    stop_flag: Arc<AtomicBool>,
    /// Handle to the thread that owns the cpal stream (`Some` after start).
    handle: Option<JoinHandle<()>>,
    /// Native format queried and cached by `new`.
    native: (u32, u16),
    /// ID (device name) of the selected input device. `None` uses the default input device.
    device_id: Option<String>,
}

impl CpalMicBackend {
    /// Create a microphone backend.
    ///
    /// `device_id`:
    /// - `None` → default input device (`host.default_input_device()`).
    /// - `Some(id)` → first device in `host.input_devices()` where `device.name()? == id`
    ///   (id is a device name returned by [`list_devices`]).
    ///
    /// Query and cache the selected device's native format. If the device is missing, does
    /// not match, or the query fails, cache `FALLBACK_FORMAT` (`(48000, 1)`). `new` always
    /// succeeds without panicking or returning an error; if device_id does not match,
    /// [`start`](Self::start) returns [`Error::DeviceNotFound`].
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

/// Resolve the input device for `device_id` from the cpal host.
///
/// - `None` → `host.default_input_device()` (or [`Error::DeviceNotFound`] if unavailable).
/// - `Some(id)` → Return the first device in `host.input_devices()` where `device.name()? == id`,
///   or [`Error::DeviceNotFound`] if none match.
///
/// Skip devices whose names cannot be read because they cannot be compared. If
/// `input_devices()` itself fails (for example, because ALSA is unavailable), map it to
/// [`Error::DeviceNotFound`].
fn resolve_input_device(host: &cpal::Host, device_id: Option<&str>) -> Result<Device> {
    match device_id {
        None => host.default_input_device().ok_or(Error::DeviceNotFound),
        // First device whose name matches.
        Some(id) => {
            let devices = host.input_devices().map_err(|_| Error::DeviceNotFound)?;
            for device in devices {
                // Skip devices whose names cannot be read because they cannot be compared.
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

/// Get the native `(sample_rate, channels)` format for the input device selected by
/// `device_id`. Return `None` if resolving the device or reading its configuration fails
/// (the caller falls back to [`FALLBACK_FORMAT`]).
fn query_native_format(device_id: Option<&str>) -> Option<(u32, u16)> {
    let host = cpal_default_host().ok()?;
    let device = resolve_input_device(&host, device_id).ok()?;
    let config = device.default_input_config().ok()?;
    Some((config.sample_rate().0, config.channels()))
}

/// Enumerate input (microphone) devices, for the microphone portion of `devices()`.
///
/// Walk `host.input_devices()` and convert each device to [`DeviceInfo`]:
/// - `id` / `name`: cpal has no persistent device ID, so use the device name for both. The
///   index can change after reconnecting, while the name remains the same for the same setup.
/// - `sample_rate` / `channels`: from `default_input_config()`. Skip devices whose config
///   cannot be read (for example, devices that cannot actually be opened).
/// - `source_kind = Mic` / `is_loopback = false`.
/// - `is_default`: `true` when the name matches `host.default_input_device()`.
///
/// Return an empty `Vec` without panicking if no devices exist or host initialization fails.
/// IDs may collide when multiple devices share a name, but cpal provides no more stable key,
/// so this is accepted.
pub fn list_devices() -> Result<Vec<DeviceInfo>> {
    let host = cpal_default_host()?;

    // Default input device name (for is_default); if unavailable, no device is marked default.
    let default_name = host.default_input_device().and_then(|d| d.name().ok());

    // Treat input_devices() failures (such as missing ALSA) as an empty list.
    let devices = match host.input_devices() {
        Ok(it) => it,
        Err(_) => return Ok(Vec::new()),
    };

    let mut out = Vec::new();
    for device in devices {
        // Skip devices whose name cannot be read because an ID cannot be created.
        let Ok(name) = device.name() else {
            continue;
        };
        // Skip devices whose default input config is unavailable; they may be advertised but cannot be opened.
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
        // Do nothing if the owner thread is already alive (safe for repeated start calls).
        if self.handle.is_some() {
            return Ok(());
        }
        // Reset the flag so start can be called again after stop.
        self.stop_flag.store(false, Ordering::SeqCst);

        let stop_flag = self.stop_flag.clone();
        // cpal::Device is !Send, so pass only the device_id string and resolve it on the thread.
        let device_id = self.device_id.clone();
        // Ready channel reports build/play success or failure from the owner thread to start().
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();

        let handle = thread::Builder::new()
            .name("flexaudio-mic-cpal".into())
            .spawn(move || {
                run_capture_thread(sink, device_id, stop_flag, ready_tx);
            })
            .map_err(|e| Error::Backend(format!("spawn cpal mic thread: {e}")))?;

        // Wait for the owner thread to build and play the stream.
        match ready_rx.recv() {
            Ok(Ok(())) => {
                self.handle = Some(handle);
                Ok(())
            }
            Ok(Err(e)) => {
                // Build/play failed. The owner thread exits after sending ready, so join it.
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
        // Do nothing if there is no handle (safe for re-entry and repeated stop calls).
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            // The owner thread is parked. Wake it so it drops the Stream and exits.
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

/// Owner-thread body. Builds and plays the cpal input stream, then parks until stopped.
///
/// Reports build/play success or failure to [`CpalMicBackend::start`] through `ready_tx`.
/// After success, parks with `stream` alive until `stop_flag` is set, then exits and drops
/// `stream` to stop capture.
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

    // The stream has started successfully.
    let _ = ready_tx.send(Ok(()));

    // Keep the stream alive while parked until the stop signal arrives.
    // Check stop_flag each time in case of a spurious wakeup.
    while !stop_flag.load(Ordering::SeqCst) {
        thread::park();
    }
    // Leaving this scope drops the stream and stops capture.
    drop(stream);
}

/// Maximum expected block duration (seconds) used to preallocate scratch for RT conversion callbacks.
///
/// Estimate the maximum frames per callback as native sample rate × channels × this duration,
/// then allocate during stream setup. Hardware blocks are usually a few to tens of ms, so a
/// one-second allocation avoids growth (and RT allocation) during steady state. If a larger
/// block arrives, [`fill_scratch`] grows once with `reserve` and retains that capacity without
/// panicking.
const MAX_SCRATCH_SECONDS: usize = 1;

/// In RT callbacks for conversion paths (I16/U16/I32), convert interleaved input into the
/// preallocated scratch buffer.
///
/// `scratch` is allocated for the maximum block size during stream setup. In steady state,
/// `clear` + `push` stay within capacity and do not reallocate. If `n` exceeds capacity, grow
/// once with `reserve` and retain that capacity. Apply `convert` to each sample.
#[inline]
fn fill_scratch<T: Copy>(scratch: &mut Vec<f32>, data: &[T], convert: impl Fn(T) -> f32) {
    let n = data.len();
    // reserve is a no-op within capacity; grow once only when capacity is exceeded.
    if n > scratch.capacity() {
        scratch.reserve(n - scratch.capacity());
    }
    scratch.clear();
    for &s in data {
        scratch.push(convert(s));
    }
}

/// f32 peak-amplitude threshold for identifying priming transient buffers.
///
/// flexaudio's f32 samples are contractually in `[-1.0, 1.0]`. When a Linux PipeWire ALSA
/// compatibility bridge (`default` PCM) opens a stream from a cold state, it emits full-scale
/// rectangular priming buffers for the first few hundred ms, far outside this range (measured
/// peak ≈ 3.3; the channels are nearly opposite-phase, so source-stage DC is ≈0, but later
/// resampling turns it into huge DC and clipping). Since valid audio is capped at 1.0 by the
/// contract, a peak clearly above 1.0 identifies a transient. Set the limit just above 1.0 so
/// valid audio at exactly ±1.0 is not caught.
const PRIMING_PEAK_LIMIT: f32 = 1.001;

/// Guard that discards priming transient buffers immediately after capture starts.
///
/// Transient buffers are full-scale rectangles outside `[-1.0, 1.0]`; discard only buffers
/// whose peak exceeds [`PRIMING_PEAK_LIMIT`].
///
/// Discarding a fixed duration would add leading silence even on systems without transients
/// (Mac / Windows / warm Linux). Checking only the peak means no buffers are discarded when
/// there is no transient, and the guard automatically follows the transient's actual duration.
///
/// Discard buffers only while the leading buffers look transient (out of range). Once one
/// in-range buffer arrives, latch open and pass everything from then on. Transients occur
/// only at startup and decay monotonically, so they do not recur. Used only in RT callbacks;
/// detection is a single buffer scan with `abs` comparisons.
struct TransientGuard {
    /// Whether a valid (in-range) buffer has already passed. Always pass buffers after this is `true`.
    latched: bool,
}

impl TransientGuard {
    fn new() -> Self {
        Self { latched: false }
    }

    /// Given an interleaved f32 buffer, return `true` if it is a priming transient and should
    /// be discarded. Once any in-range buffer passes, always return `false` (pass it through).
    fn should_drop(&mut self, data: &[f32]) -> bool {
        if self.latched || data.is_empty() {
            // Already past the transient, or the buffer is empty (nothing to discard).
            self.latched = true;
            return false;
        }
        // Find the peak amplitude in one pass over the buffer.
        let mut peak = 0.0f32;
        for &s in data {
            let a = s.abs();
            if a > peak {
                peak = a;
            }
        }
        // Clearly outside the contract range: this is a priming transient.
        let is_transient = peak > PRIMING_PEAK_LIMIT;
        if !is_transient {
            self.latched = true;
        }
        is_transient
    }
}

/// Build (but do not yet play) an input stream for the device selected by `device_id`.
/// `device_id = None` selects the default input device. Return [`Error::DeviceNotFound`] if
/// no device matches.
///
/// Select a callback for each sample format: pass F32 through directly and convert I16/U16/I32
/// to `f32` in `[-1.0, 1.0]` before sending to [`RawSink::push`]. [`TransientGuard`] discards
/// priming transient buffers after startup (for the PipeWire ALSA bridge).
fn build_stream(sink: RawSink, device_id: Option<&str>) -> Result<cpal::Stream> {
    let host = cpal_default_host()?;
    // None selects default; Some selects the first name match. A mismatch is DeviceNotFound.
    let device = resolve_input_device(&host, device_id)?;

    // If the default input config is unavailable, the advertised device cannot actually be
    // opened (including ALSA "default" PCM on a server without a sound card). Treat this as
    // having no usable input device and map it to DeviceNotFound.
    let supported = device
        .default_input_config()
        .map_err(|_| Error::DeviceNotFound)?;
    let sample_format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();

    let err_fn = |e: cpal::StreamError| {
        // Error callback outside the RT path. Swallow for now because logging is not wired up.
        // TODO: Map this to Event::DeviceLost or similar in the integration layer.
        let _ = e;
    };

    // Preallocate conversion scratch for the maximum block size (native sample rate × channels
    // × MAX_SCRATCH_SECONDS). This avoids first-use/growth allocations in RT callbacks during
    // steady state (xrun risk). Allocate at least one item.
    let scratch_cap = (config.sample_rate.0 as usize)
        .saturating_mul(config.channels as usize)
        .saturating_mul(MAX_SCRATCH_SECONDS)
        .max(1);

    // Move the sink into the callback. Non-F32 formats capture the converter. Since transient
    // detection uses f32 values, converted formats are checked after conversion.
    //
    // cpal data callbacks cross an FFI (C ABI) boundary, where a panic could cause undefined
    // behavior. No current path is known to panic, but wrap each callback body in catch_unwind
    // so an unexpected panic only discards that block.
    let stream = match sample_format {
        SampleFormat::F32 => {
            let mut sink = sink;
            let mut guard = TransientGuard::new();
            device.build_input_stream(
                &config,
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    let _ = catch_unwind(AssertUnwindSafe(|| {
                        // Already interleaved f32. Discard if this is a priming transient.
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
            // Conversion scratch, preallocated for the maximum block size to avoid growth in RT.
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
                        // Map u16 [0, 65535] to [-1, 1) around midpoint 32768.
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

    /// Generate `frames` of interleaved stereo f32 samples.
    /// Samples alternate between `±peak` (a square wave with peak amplitude `peak`).
    fn make_buf(frames: usize, peak: f32) -> Vec<f32> {
        let mut v = Vec::with_capacity(frames * 2);
        for i in 0..frames {
            let s = if i % 2 == 0 { peak } else { -peak };
            v.push(s); // L
            v.push(s); // R
        }
        v
    }

    /// Verify that [`TransientGuard`] discards only the leading buffers above
    /// [`PRIMING_PEAK_LIMIT`] in an out-of-range full-scale transient → decay → in-range
    /// sequence, then latches open and passes all later buffers.
    #[test]
    fn transient_guard_drops_priming_then_latches_open() {
        let frames = 1024;
        let mut g = TransientGuard::new();

        // Out-of-range full-scale square wave (measured priming transient equivalent, peak≈3.3): discard.
        assert!(g.should_drop(&make_buf(frames, 3.3)));
        // Decaying but still out of range (peak=1.5 > LIMIT): discard.
        assert!(g.should_drop(&make_buf(frames, 1.5)));
        // Back in range (valid audio, peak=0.88): pass and latch open.
        assert!(!g.should_drop(&make_buf(frames, 0.88)));
        // After latching, always pass later buffers, even if out of range (prevents recurrence).
        assert!(!g.should_drop(&make_buf(frames, 3.3)));
    }

    /// In environments without transients (Mac/Windows/warm Linux), audio is in range from
    /// the start, so [`TransientGuard`] discards no buffers and adds no leading silence.
    #[test]
    fn transient_guard_passes_clean_audio_from_the_start() {
        let frames = 1024;
        let mut g = TransientGuard::new();
        // Do not misclassify digital full scale ±1.0 (LIMIT is just above 1.0).
        assert!(!g.should_drop(&make_buf(frames, 1.0)));
        assert!(!g.should_drop(&make_buf(frames, 0.5)));
        // Do not discard silence (all zeros) either.
        assert!(!g.should_drop(&vec![0.0f32; frames * 2]));
    }

    /// Do not discard an empty buffer; latch open.
    #[test]
    fn transient_guard_handles_empty_buffer() {
        let mut g = TransientGuard::new();
        assert!(!g.should_drop(&[]));
        // Latching on empty means even later out-of-range buffers pass through.
        assert!(!g.should_drop(&make_buf(1024, 3.3)));
    }

    /// Verify that [`fill_scratch`] does not reallocate within capacity and converts correctly,
    /// ensuring no steady-state allocations in RT callbacks.
    #[test]
    fn fill_scratch_no_realloc_in_steady_state() {
        // Allocate for the maximum expected block size.
        let cap = 480 * 2; // equivalent to 10 ms at 48 kHz stereo
        let mut scratch: Vec<f32> = Vec::with_capacity(cap);
        let before = scratch.capacity();

        // Filling in-capacity blocks repeatedly does not change capacity (no reallocation).
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
        // Conversion is correct (i16::MIN maps to -1.0).
        let mut one = Vec::with_capacity(1);
        fill_scratch(&mut one, &[i16::MIN], |s| s as f32 / -(i16::MIN as f32));
        assert_eq!(one[0], -1.0);
    }

    /// `new` + `native_format` do not panic, whether or not an input device exists.
    /// `new` always succeeds with either device_id = None (default) or Some (specific device).
    #[test]
    fn new_and_native_format_do_not_panic() {
        // Default input device (device_id = None).
        let backend = CpalMicBackend::new(None);
        let (rate, channels) = backend.native_format();
        // Format values are always positive (FALLBACK_FORMAT when no device exists).
        assert!(rate > 0);
        assert!(channels > 0);

        // Even for a nonexistent device_id, new succeeds without panicking and returns
        // FALLBACK_FORMAT (resolution failure is deferred until start/build_stream).
        let backend = CpalMicBackend::new(Some("__no_such_device__".into()));
        let (rate, channels) = backend.native_format();
        assert_eq!((rate, channels), FALLBACK_FORMAT);
    }

    /// `start` with a nonexistent device_id returns [`Error::DeviceNotFound`] without
    /// panicking (consistent with cold-start/TransientGuard behavior). A mismatched ID always
    /// yields DeviceNotFound, regardless of default-device availability.
    #[test]
    fn start_with_unknown_device_id_yields_device_not_found() {
        let mut backend = CpalMicBackend::new(Some("__no_such_device__".into()));
        let (rate, channels) = backend.native_format();
        let cap = (rate as usize * channels as usize).max(1);
        let (prod, _cons) = raw_ring(cap);
        let sink = RawSink::new(prod, rate, channels);

        match backend.start(sink) {
            Err(Error::DeviceNotFound) => {}
            other => panic!("unknown device_id should return DeviceNotFound: {other:?}"),
        }
    }

    /// [`list_devices`] returns `Ok(Vec)` without panicking, whether or not devices exist.
    /// Every returned device is `Mic`, is not loopback, and has the stable key `id == name`.
    #[test]
    fn list_devices_never_panics_and_is_consistent() {
        let devices = list_devices().expect("list_devices is designed not to return Err");
        for d in &devices {
            assert_eq!(d.source_kind, SourceKind::Mic);
            assert!(!d.is_loopback, "microphones are not loopback devices");
            // Stable key: cpal uses the device name as the ID.
            assert_eq!(d.id, d.name);
            assert!(!d.id.is_empty(), "id (= name) is nonempty");
            assert!(d.sample_rate > 0);
            assert!(d.channels > 0);
        }
        // At most one default input device.
        assert!(devices.iter().filter(|d| d.is_default).count() <= 1);
    }

    /// `start` may return `Err(DeviceNotFound)` where no input device exists (servers/CI).
    /// Both Ok and Err(DeviceNotFound) are acceptable; panicking is not. Where an input device
    /// exists, capture starts and stops on stop.
    #[test]
    fn start_then_stop_tolerates_missing_device() {
        let mut backend = CpalMicBackend::new(None);
        let (rate, channels) = backend.native_format();
        let cap = (rate as usize * channels as usize).max(1); // about one second
        let (prod, _cons) = raw_ring(cap);
        let sink = RawSink::new(prod, rate, channels);

        match backend.start(sink) {
            Ok(()) => {
                // In environments where start succeeds, stop must be safe.
                backend.stop();
                // Repeated stop calls are safe too.
                backend.stop();
            }
            Err(Error::DeviceNotFound) => {
                // Accept this when no input device is present (CI/server).
            }
            Err(other) => panic!("unexpected error from start(): {other:?}"),
        }
    }

    /// End-to-end test that records from a real microphone. Run with
    /// `cargo test -p flexaudio-mic -- --ignored` on a laptop or other machine with an input
    /// device. Ignored by default because servers/CI usually have no input device.
    #[test]
    #[ignore = "requires a real microphone; run `cargo test -p flexaudio-mic -- --ignored` on a laptop"]
    fn end_to_end_captures_real_audio() {
        use std::time::Duration;

        let mut backend = CpalMicBackend::new(None);
        let (rate, channels) = backend.native_format();
        let cap = rate as usize * channels as usize * 2; // about two seconds
        let (prod, mut cons) = raw_ring(cap);
        let sink = RawSink::new(prod, rate, channels);

        backend
            .start(sink)
            .expect("start() should succeed with a real input device");

        // Capture for a few hundred milliseconds and verify that samples arrive.
        thread::sleep(Duration::from_millis(500));
        backend.stop();

        let mut buf = vec![0.0f32; cap];
        let got = cons.pop_slice(&mut buf);
        assert!(got > 0, "expected captured samples, got none");
        // Samples stay within [-1, 1] (conversion is valid).
        assert!(buf[..got].iter().all(|&s| (-1.5..=1.5).contains(&s)));
    }
}
