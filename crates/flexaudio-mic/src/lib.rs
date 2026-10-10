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

use std::collections::VecDeque;
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
use flexaudio_core::{ErrorContext, ErrorGroup, Operation};

mod capture_owner;

mod callback_mailbox;
mod generation;
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
    /// Runtime failures are materialized on the control thread, never the callback.
    callback_errors: Arc<callback_mailbox::CallbackMailbox>,
    generation: Arc<generation::Generation>,
    shutdown_result: Option<Result<()>>,
    pending_events: VecDeque<Event>,
}

impl CpalMicBackend {
    /// Create a microphone backend.
    ///
    /// `device_id`:
    /// - `None` → default input device (`host.default_input_device()`).
    /// - `Some(id)` → unique device in `host.input_devices()` where `device.name()? == id`
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
        let stop_flag = Arc::new(AtomicBool::new(false));
        let callback_errors = Arc::new(callback_mailbox::CallbackMailbox::new(stop_flag.clone()));
        let generation = Arc::new(generation::Generation::new(
            stop_flag.clone(),
            event_tx.clone(),
        ));
        Self {
            stop_flag,
            handle: None,
            native,
            device_id,
            events,
            event_tx,
            terminal_error: None,
            callback_errors,
            generation,
            shutdown_result: None,
            pending_events: VecDeque::new(),
        }
    }

    fn teardown(&mut self) -> Result<()> {
        if let Some(result) = &self.shutdown_result {
            return result.clone();
        }
        self.generation.cancel();
        let result = match self.handle.take() {
            Some(handle) => {
                handle.thread().unpark();
                handle.join().map_err(|_| {
                    Error::Backend("microphone capture owner panicked".into())
                        .with_context(ErrorContext::new(Operation::Join))
                })
            }
            None => Ok(()),
        };
        if let Err(error) = &result {
            let _ = self.event_tx.send(Event::ShutdownError {
                error: error.clone(),
            });
        }
        self.reconcile_events();
        self.shutdown_result = Some(result.clone());
        result
    }

    fn retain_terminal(&mut self, event: &Event) {
        if self.terminal_error.is_none() {
            self.terminal_error = match event {
                Event::PermissionDenied { permission, detail } => Some(Error::PermissionDenied {
                    permission: *permission,
                    detail: detail.clone(),
                }),
                Event::TerminalError { error } => Some(error.clone()),
                _ => None,
            };
        }
    }

    fn reconcile_events(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            self.retain_terminal(&event);
            self.pending_events.push_back(event);
        }
        if let Some(error) = self.callback_errors.take() {
            let error = match error {
                cpal::StreamError::DeviceNotAvailable => Error::DeviceLost,
                cpal::StreamError::BackendSpecific { err } => {
                    Error::Backend(format!("microphone capture failed: {err}"))
                }
            };
            let event = Event::TerminalError { error };
            self.retain_terminal(&event);
            self.pending_events.push_back(event);
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
/// - `Some(id)` → Return the unique name match, or reject ambiguous selection.
///
/// Query failures return Backend; DeviceNotFound means a complete lookup had no match.
fn resolve_input_device(host: &cpal::Host, device_id: Option<&str>) -> Result<Device> {
    match device_id {
        None => host.default_input_device().ok_or(Error::DeviceNotFound),
        Some(id) => {
            let devices = host.input_devices().map_err(|error| {
                Error::Backend(format!("enumerate microphone devices: {error}"))
                    .with_context(ErrorContext::new(Operation::Enumerate))
            })?;
            let mut selected = None;
            for device in devices {
                let name = device.name().map_err(|error| {
                    Error::Backend(format!("query microphone device name: {error}"))
                        .with_context(ErrorContext::new(Operation::Enumerate))
                })?;
                if name == id {
                    if selected.is_some() {
                        return Err(Error::AmbiguousDeviceName);
                    }
                    selected = Some(device);
                }
            }
            selected.ok_or(Error::DeviceNotFound)
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
/// - `sample_rate` / `channels`: from `default_input_config()`. For devices whose config
///   cannot be read: return Backend rather than an incomplete inventory.
/// - `source_kind = Mic` / `is_loopback = false`.
/// - `is_default`: `true` when the name matches `host.default_input_device()`.
///
/// Return an empty `Vec` only after a successful query found no devices.
/// IDs may collide when multiple devices share a name, but cpal provides no more stable key,
/// so this is accepted.
pub fn list_devices() -> Result<Vec<DeviceInfo>> {
    let host = cpal_default_host()?;

    let default_name = host
        .default_input_device()
        .map(|device| device.name())
        .transpose()
        .map_err(|error| {
            Error::Backend(format!("query default microphone name: {error}"))
                .with_context(ErrorContext::new(Operation::Enumerate))
        })?;
    let devices = host.input_devices().map_err(|error| {
        Error::Backend(format!("enumerate microphone devices: {error}"))
            .with_context(ErrorContext::new(Operation::Enumerate))
    })?;

    let mut out = Vec::new();
    for device in devices {
        let name = device.name().map_err(|error| {
            Error::Backend(format!("query microphone device name: {error}"))
                .with_context(ErrorContext::new(Operation::Enumerate))
        })?;
        let config = device.default_input_config().map_err(|error| {
            Error::Backend(format!("query microphone input configuration: {error}"))
                .with_context(ErrorContext::new(Operation::Enumerate))
        })?;
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
        self.reconcile_events();
        if let Some(error) = self.terminal_error.as_ref() {
            return Err(error.clone());
        }
        // Do nothing if the owner thread is already alive (safe for repeated start calls).
        if self.handle.is_some() {
            return Ok(());
        }
        // Each owner receives fresh state; an old generation can never publish
        // grants or close PCM on a later generation.
        self.stop_flag = Arc::new(AtomicBool::new(false));
        self.callback_errors = Arc::new(callback_mailbox::CallbackMailbox::new(
            self.stop_flag.clone(),
        ));
        self.generation = Arc::new(generation::Generation::new(
            self.stop_flag.clone(),
            self.event_tx.clone(),
        ));

        let stop_flag = self.stop_flag.clone();
        // cpal::Device is !Send, so pass only the device_id string and resolve it on the thread.
        let device_id = self.device_id.clone();
        let generation = self.generation.clone();
        let callback_errors = self.callback_errors.clone();
        // Ready channel reports build/play success or failure from the owner thread to start().
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();

        let handle = thread::Builder::new()
            .name("flexaudio-mic-cpal".into())
            .spawn(move || {
                capture_owner::run(
                    sink,
                    device_id,
                    stop_flag,
                    ready_tx,
                    generation,
                    callback_errors,
                );
            })
            .map_err(|e| {
                self.generation.cancel();
                Error::Backend(format!("spawn cpal mic thread: {e}"))
                    .with_context(ErrorContext::new(Operation::Start))
            })?;
        self.handle = Some(handle);
        let previous_shutdown = self.shutdown_result.take();

        // Wait for the owner thread to build and play the stream.
        let readiness = ready_rx.recv().unwrap_or_else(|_| {
            Err(
                Error::Backend("cpal mic thread exited before reporting readiness".into())
                    .with_context(ErrorContext::new(Operation::Start)),
            )
        });
        match readiness {
            Ok(()) => Ok(()),
            Err(e) => {
                let cleanup = self.teardown();
                if let Error::NativeFormatChanged { actual, .. } = &e {
                    self.native = *actual;
                }
                if previous_shutdown.is_some() {
                    self.shutdown_result = previous_shutdown;
                }
                match cleanup {
                    Ok(()) => Err(e),
                    Err(cleanup) => Err(Error::Multiple(ErrorGroup::new(e, cleanup, Vec::new()))),
                }
            }
        }
    }

    fn stop(&mut self) {
        let _ = self.teardown();
    }

    fn stop_checked(&mut self) -> Result<()> {
        self.teardown()
    }

    fn poll_event(&mut self) -> Option<Event> {
        self.reconcile_events();
        self.pending_events.pop_front()
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
    callback_errors: Arc<callback_mailbox::CallbackMailbox>,
) -> Result<cpal::Stream> {
    let host = cpal_default_host()?;
    // None selects default; Some must uniquely match an advertised CPAL name.
    let device = resolve_input_device(&host, device_id)?;

    // The same checked configuration is passed to CPAL below. Never feed a sink
    // configured from a fallback or stale device format with different samples.
    let supported = input_config::checked(
        || {
            device.default_input_config().map_err(|error| {
                Error::Backend(format!("query microphone input configuration: {error}"))
            })
        },
        |config| (config.sample_rate().0, config.channels()),
        (sink.native_rate(), sink.native_channels()),
    )?;
    let sample_format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();

    let err_fn = move |error: cpal::StreamError| callback_errors.record(error);

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
        _ => {
            return Err(Error::UnsupportedFormat(
                "microphone sample encoding is unsupported".into(),
            ));
        }
    };

    stream.map_err(|e| Error::Backend(format!("build_input_stream: {e}")))
}

#[cfg(test)]
mod tests {
    include!("tests.rs");
}
