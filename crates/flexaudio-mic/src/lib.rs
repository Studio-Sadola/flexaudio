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

use cpal::traits::{DeviceTrait, HostTrait};
use cpal::{Device, SampleFormat};

use flexaudio_core::backend::{CaptureBackend, RawSink};
use flexaudio_core::clock::monotonic_now_ns;
use flexaudio_core::types::{DeviceInfo, Error, Event, Result, SourceKind};

mod capture_owner;
mod input_config;
#[cfg(target_os = "macos")]
mod mac_permission;
#[cfg(any(target_os = "macos", test))]
mod mac_policy;
mod permission;
#[cfg(target_os = "windows")]
mod windows_permission;
#[cfg(any(target_os = "windows", test))]
mod windows_policy;

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
    /// Notifications from the capture owner; callbacks never query consent.
    events: mpsc::Receiver<Event>,
    event_tx: mpsc::Sender<Event>,
    /// A confirmed runtime denial cannot be restarted on this backend instance.
    terminal_error: Option<Error>,
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
    /// [`start`](Self::start) returns [`Error::DeviceNotFound`]. If consent or a device
    /// change reveals a different format, `start` returns [`Error::NativeFormatChanged`]
    /// before building capture, and refreshes [`native_format`](Self::native_format).
    /// Prefer [`try_new`](Self::try_new) to discover the format after consent.
    pub fn new(device_id: Option<String>) -> Self {
        let native = if permission::can_query_format() {
            query_native_format(device_id.as_deref()).unwrap_or(FALLBACK_FORMAT)
        } else {
            FALLBACK_FORMAT
        };
        Self::with_format(device_id, native)
    }

    /// Check OS microphone permission before discovering the device format.
    ///
    /// macOS prompts only when the main bundle declares a nonempty microphone
    /// usage description. Otherwise the responsible application may prompt during
    /// startup, and the capture owner watches for a later denial. Windows checks
    /// the public microphone capability; unsupported queries defer to capture.
    /// A refused explicit prompt returns [`Error::PermissionDenied`]. An unanswered
    /// prompt proceeds after 30 seconds; capture emits [`Event::PermissionPending`]
    /// after five seconds of undecided consent and continues checking until a decision.
    pub fn try_new(device_id: Option<String>) -> Result<Self> {
        permission::preflight()?;
        let native = query_native_format(device_id.as_deref()).unwrap_or(FALLBACK_FORMAT);
        Ok(Self::with_format(device_id, native))
    }

    fn with_format(device_id: Option<String>, native: (u32, u16)) -> Self {
        let (event_tx, events) = mpsc::channel();
        Self {
            stop_flag: Arc::new(AtomicBool::new(false)),
            handle: None,
            native,
            device_id,
            events,
            event_tx,
            terminal_error: None,
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
        if let Some(error) = self.terminal_error.as_ref() {
            return Err(error.clone());
        }
        // Do nothing if the owner thread is already alive (safe for repeated start calls).
        if self.handle.is_some() {
            return Ok(());
        }
        // Reset the flag so start can be called again after stop.
        self.stop_flag.store(false, Ordering::SeqCst);

        let stop_flag = self.stop_flag.clone();
        // cpal::Device is !Send, so pass only the device_id string and resolve it on the thread.
        let device_id = self.device_id.clone();
        let event_tx = self.event_tx.clone();
        // Ready channel reports build/play success or failure from the owner thread to start().
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();

        let handle = thread::Builder::new()
            .name("flexaudio-mic-cpal".into())
            .spawn(move || {
                capture_owner::run(sink, device_id, stop_flag, ready_tx, event_tx);
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
                if let Error::NativeFormatChanged { actual, .. } = &e {
                    self.native = *actual;
                }
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

    fn poll_event(&mut self) -> Option<Event> {
        let event = self.events.try_recv().ok()?;
        let terminal = match &event {
            Event::PermissionDenied { permission, detail } => Some(Error::PermissionDenied {
                permission: *permission,
                detail: detail.clone(),
            }),
            Event::TerminalError { error } => Some(error.clone()),
            _ => None,
        };
        if self.terminal_error.is_none() {
            self.terminal_error = terminal;
        }
        Some(event)
    }
}

impl Drop for CpalMicBackend {
    fn drop(&mut self) {
        self.stop();
    }
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

/// Build (but do not yet play) an input stream for the device selected by `device_id`.
/// `device_id = None` selects the default input device. Return [`Error::DeviceNotFound`] if
/// no device matches.
///
/// Select a callback for each sample format: pass F32 through directly and convert I16/U16/I32
/// to `f32` in `[-1.0, 1.0]` before sending to [`RawSink::push`]. Every buffer is delivered;
/// discarding leading buffers by amplitude was unproved and dropped real audio.
fn build_stream(
    sink: RawSink,
    device_id: Option<&str>,
    stop_flag: Arc<AtomicBool>,
) -> Result<cpal::Stream> {
    let host = cpal_default_host()?;
    // None selects default; Some selects the first name match. A mismatch is DeviceNotFound.
    let device = resolve_input_device(&host, device_id)?;

    // If the default input config is unavailable, the advertised device cannot actually be
    // opened (including ALSA "default" PCM on a server without a sound card). Treat this as
    // having no usable input device and map it to DeviceNotFound.
    // The same checked configuration is passed to CPAL below. Never feed a sink
    // configured from a fallback or stale device format with different samples.
    let supported = input_config::checked(
        || {
            device
                .default_input_config()
                .map_err(|_| Error::DeviceNotFound)
        },
        |config| (config.sample_rate().0, config.channels()),
        (sink.native_rate(), sink.native_channels()),
    )?;
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

    // Move the sink into the callback. Non-F32 formats capture the converter.
    //
    // cpal data callbacks cross an FFI (C ABI) boundary, where a panic could cause undefined
    // behavior. No current path is known to panic, but wrap each callback body in catch_unwind
    // so an unexpected panic only discards that block.
    let stream = match sample_format {
        SampleFormat::F32 => {
            let mut sink = sink;
            device.build_input_stream(
                &config,
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    let _ = catch_unwind(AssertUnwindSafe(|| {
                        if stop_flag.load(Ordering::SeqCst) {
                            return;
                        }
                        // Already interleaved f32.
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
            device.build_input_stream(
                &config,
                move |data: &[i16], _: &cpal::InputCallbackInfo| {
                    let _ = catch_unwind(AssertUnwindSafe(|| {
                        if stop_flag.load(Ordering::SeqCst) {
                            return;
                        }
                        fill_scratch(&mut scratch, data, |s| s as f32 / -(i16::MIN as f32));
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
            device.build_input_stream(
                &config,
                move |data: &[u16], _: &cpal::InputCallbackInfo| {
                    let _ = catch_unwind(AssertUnwindSafe(|| {
                        if stop_flag.load(Ordering::SeqCst) {
                            return;
                        }
                        // Map u16 [0, 65535] to [-1, 1) around midpoint 32768.
                        fill_scratch(&mut scratch, data, |s| (s as f32 - 32_768.0) / 32_768.0);
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
            device.build_input_stream(
                &config,
                move |data: &[i32], _: &cpal::InputCallbackInfo| {
                    let _ = catch_unwind(AssertUnwindSafe(|| {
                        if stop_flag.load(Ordering::SeqCst) {
                            return;
                        }
                        fill_scratch(&mut scratch, data, |s| s as f32 / -(i32::MIN as f32));
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

    #[test]
    fn backend_terminal_query_failure_preserves_cause_and_rejects_restart() {
        let mut backend = CpalMicBackend::with_format(None, FALLBACK_FORMAT);
        let error = Error::Backend("injected authorization query failure".into());
        let event = Event::TerminalError {
            error: error.clone(),
        };
        backend.event_tx.send(event.clone()).unwrap();
        assert_eq!(backend.poll_event(), Some(event));
        backend.stop();
        let (prod, _cons) = raw_ring(16);
        assert_eq!(
            backend.start(RawSink::new(prod, FALLBACK_FORMAT.0, FALLBACK_FORMAT.1)),
            Err(error)
        );
    }

    #[test]
    fn backend_permission_event_retains_terminal_cause_and_rejects_restart() {
        let mut backend = CpalMicBackend::with_format(None, FALLBACK_FORMAT);
        let event = Event::PermissionDenied {
            permission: flexaudio_core::types::Permission::Microphone,
            detail: "injected late denial".into(),
        };
        backend.event_tx.send(event.clone()).unwrap();
        assert_eq!(backend.poll_event(), Some(event));
        backend.stop();
        let (prod, _cons) = raw_ring(16);
        let result = backend.start(RawSink::new(prod, FALLBACK_FORMAT.0, FALLBACK_FORMAT.1));
        assert!(
            matches!(result, Err(Error::PermissionDenied { permission: flexaudio_core::types::Permission::Microphone, detail }) if detail == "injected late denial")
        );
        assert_eq!(backend.poll_event(), None);
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
    /// panicking. Host microphone denial can precede device resolution on macOS/Windows
    /// and skips this hardware assertion.
    #[test]
    fn start_with_unknown_device_id_yields_device_not_found() {
        let mut backend = CpalMicBackend::new(Some("__no_such_device__".into()));
        let (rate, channels) = backend.native_format();
        let cap = (rate as usize * channels as usize).max(1);
        let (prod, _cons) = raw_ring(cap);
        let sink = RawSink::new(prod, rate, channels);

        match backend.start(sink) {
            Err(Error::DeviceNotFound) => {}
            #[cfg(any(target_os = "windows", target_os = "macos"))]
            Err(
                error @ Error::PermissionDenied {
                    permission: flexaudio_core::types::Permission::Microphone,
                    ..
                },
            ) => {
                eprintln!("Skipping start_with_unknown_device_id_yields_device_not_found: host microphone permission is denied: {error}");
            }
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
    /// Host microphone denial is also an environment outcome on macOS/Windows. Where
    /// an input device is accessible, capture starts and stops on stop.
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
            #[cfg(any(target_os = "windows", target_os = "macos"))]
            Err(
                error @ Error::PermissionDenied {
                    permission: flexaudio_core::types::Permission::Microphone,
                    ..
                },
            ) => {
                eprintln!("Skipping start_then_stop_tolerates_missing_device: host microphone permission is denied: {error}");
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
