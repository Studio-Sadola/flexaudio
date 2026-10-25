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
#![deny(clippy::let_underscore_must_use)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use cpal::traits::{DeviceTrait, HostTrait};
use cpal::{Device, SampleFormat};

use flexaudio_core::backend::{CaptureBackend, RawSink};
use flexaudio_core::clock::monotonic_now_ns;
use flexaudio_core::types::{DeviceInfo, Error, Event, Result, SourceKind};
use flexaudio_core::{ErrorContext, Operation, ShutdownReport};

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

mod capture_owner;

mod callback_mailbox;
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

enum OwnerStopPhase {
    Startup,
    Runtime,
}

/// The only entry point that returns cpal's default host.
///
/// On Windows, cpal 0.16's process-wide WASAPI enumerator depends on the COM lifetime of the
/// STA thread that first creates it. Initialize the enumerator through a keeper before
/// returning the host, preventing a short-lived caller thread from becoming the first
/// creator. Other OSes skip the keeper and return `cpal::default_host()` as before.
fn cpal_default_host(purpose: permission::Purpose) -> Result<cpal::Host> {
    permission::query_after_preflight(
        || permission::preflight(purpose),
        || {
            #[cfg(windows)]
            windows_keeper::ensure()?;
            Ok(cpal::default_host())
        },
    )
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
    /// Control-thread teardown notifications; callbacks never query consent.
    events: mpsc::Receiver<Event>,
    #[cfg(test)]
    event_tx: mpsc::Sender<Event>,
    /// A retained terminal failure prevents restarting this backend instance.
    terminal_error: Option<Error>,
    /// Legacy construction cannot return discovery failure; only start returns it.
    query_error: Option<Error>,
    /// Runtime failures are materialized on the control thread, never the callback.
    callback_errors: Arc<callback_mailbox::CallbackMailbox>,
    shutdown: Option<ShutdownReport>,
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
    /// Query and cache the selected device's native format without requesting consent.
    /// This legacy infallible constructor caches any query failure for `start` to
    /// return before opening capture, without a terminal event or shutdown failure. A failed
    /// query leaves a compatibility placeholder in `native_format`, never a format
    /// used for capture. Prefer [`try_new`](Self::try_new) for checked discovery
    /// after consent. macOS undecided or denied access fails before native queries.
    pub fn new(device_id: Option<String>) -> Self {
        // Preserve the existing nonprompting Windows capability check and fallback.
        #[cfg(target_os = "windows")]
        let purpose = permission::Purpose::Capture;
        #[cfg(not(target_os = "windows"))]
        let purpose = permission::Purpose::Enumeration;
        let queried = query_native_format(device_id.as_deref(), purpose);
        Self::from_query_result(device_id, queried)
    }

    /// Check OS microphone permission before discovering the device format.
    ///
    /// macOS prompts only when the main bundle declares a nonempty microphone
    /// usage description. Undecided consent without that metadata fails before
    /// querying the input format; grant access to the responsible host app or
    /// launch from an app bundle with that usage description, then retry. Windows checks
    /// the public microphone capability; unsupported queries defer to capture.
    /// A refused explicit prompt returns [`Error::PermissionDenied`]. An unanswered
    /// prompt returns an actionable [`Error::Backend`] after at most 30 seconds,
    /// without querying HAL or treating the unanswered prompt as a denial.
    pub fn try_new(device_id: Option<String>) -> Result<Self> {
        let native = query_native_format(device_id.as_deref(), permission::Purpose::Capture)?;
        Ok(Self::with_format(device_id, native))
    }

    fn from_query_result(device_id: Option<String>, queried: Result<(u32, u16)>) -> Self {
        match queried {
            Ok(native) => Self::with_format(device_id, native),
            Err(error) => {
                // The legacy constructor cannot return Result. Its placeholder
                // format is never used for capture: retain the cause before start.
                let mut backend = Self::with_format(device_id, FALLBACK_FORMAT);
                backend.query_error = Some(error);
                backend
            }
        }
    }

    fn with_format(device_id: Option<String>, native: (u32, u16)) -> Self {
        let (_event_tx, events) = mpsc::channel();
        let stop_flag = Arc::new(AtomicBool::new(false));
        let callback_errors = Arc::new(callback_mailbox::CallbackMailbox::new(stop_flag.clone()));
        Self {
            stop_flag,
            handle: None,
            native,
            device_id,
            events,
            #[cfg(test)]
            event_tx: _event_tx,
            terminal_error: None,
            query_error: None,
            callback_errors,
            shutdown: None,
            pending_events: VecDeque::new(),
        }
    }

    /// Completed stop outcome, retained independently of event polling.
    pub fn shutdown_report(&self) -> Option<&ShutdownReport> {
        self.shutdown.as_ref()
    }

    fn retain_shutdown_outcome(&mut self) {
        if self.shutdown.is_some() {
            return;
        }
        let cleanup = self.stop_owner(OwnerStopPhase::Runtime);
        self.shutdown = Some(ShutdownReport::new(
            self.terminal_error.clone(),
            cleanup.into_iter().collect(),
        ));
    }

    /// Quiesce callbacks and publish runtime observations before owner cleanup.
    fn stop_owner(&mut self, phase: OwnerStopPhase) -> Option<Error> {
        self.stop_flag.store(true, Ordering::SeqCst);
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
        match phase {
            OwnerStopPhase::Startup => {}
            OwnerStopPhase::Runtime => self.reconcile_events(),
        }
        if let Err(error) = &result {
            self.pending_events.push_back(Event::ShutdownError {
                error: error.clone(),
            });
        }
        result.err()
    }

    fn complete_start(
        &mut self,
        readiness: Result<()>,
        previous_shutdown: Option<ShutdownReport>,
    ) -> Result<()> {
        match readiness {
            Ok(()) => Ok(()),
            Err(error) => {
                let current_cleanup = self.stop_owner(OwnerStopPhase::Startup);
                if let Error::NativeFormatChanged { actual, .. } = &error {
                    self.native = *actual;
                }
                // A callback can fail while build/play is still rejecting startup.
                // Return those observations with this attempt, without publishing
                // runtime terminal state for a capture that never started.
                let mut startup = Some(error);
                while let Some(error) = self.callback_errors.take() {
                    flexaudio_core::terminal_causes::retain_terminal_cause(&mut startup, error);
                }
                let mut cleanup = previous_shutdown
                    .map(|previous| previous.cleanup().to_vec())
                    .unwrap_or_default();
                cleanup.extend(current_cleanup);
                let report = ShutdownReport::new(startup, cleanup);
                let result = report.result();
                self.shutdown = Some(ShutdownReport::new(None, report.cleanup().to_vec()));
                result
            }
        }
    }

    fn retain_terminal(&mut self, event: &Event) {
        let error = match event {
            Event::PermissionDenied { permission, detail } => Error::PermissionDenied {
                permission: *permission,
                detail: detail.clone(),
            },
            Event::TerminalError { error } => error.clone(),
            _ => return,
        };
        flexaudio_core::terminal_causes::retain_terminal_cause(&mut self.terminal_error, error);
    }

    fn reconcile_events(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            self.retain_terminal(&event);
            self.pending_events.push_back(event);
        }
        while let Some(error) = self.callback_errors.take() {
            if flexaudio_core::terminal_causes::retain_terminal_cause(
                &mut self.terminal_error,
                error.clone(),
            ) {
                self.pending_events
                    .push_back(Event::TerminalError { error });
            }
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
                input_config::query_failure("enumerate microphone devices", error)
            })?;
            let mut selected = None;
            for device in devices {
                let name = device.name().map_err(|error| {
                    input_config::query_failure("query microphone device name", error)
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

/// Query the selected input device's native format without fabricating a fallback.
/// Consent, host, selection, and configuration failures retain their typed cause.
fn query_native_format(
    device_id: Option<&str>,
    purpose: permission::Purpose,
) -> Result<(u32, u16)> {
    let host = cpal_default_host(purpose)?;
    let device = resolve_input_device(&host, device_id)?;
    flexaudio_core::inventory::require_device_format(input_config::discover(
        || {
            device.default_input_config().map_err(|error| {
                input_config::query_failure("query microphone input configuration", error)
            })
        },
        |config| (config.sample_rate().0, config.channels()),
    ))
}

/// Enumerate input (microphone) devices, for the microphone portion of `devices()`.
///
/// Walk `host.input_devices()` and convert each device to [`DeviceInfo`]:
/// - `id` / `name`: cpal has no persistent device ID, so use the device name for both. The
///   index can change after reconnecting, while the name remains the same for the same setup.
/// - `sample_rate` / `channels`: from `default_input_config()`. For devices whose config
///   cannot be read: retain the entry with format_available=false and 0/0.
///   Apply the same positive-rate and one/two-channel validation as capture setup.
/// - `source_kind = Mic` / `is_loopback = false`.
/// - `is_default`: `true` when the name matches `host.default_input_device()`.
///
/// Return an empty `Vec` only after a successful query found no devices.
/// IDs may collide when multiple devices share a name, but cpal provides no more stable key,
/// so this is accepted.
/// macOS enumeration never prompts: undecided consent returns an actionable Backend
/// error before CPAL/HAL queries; denied/restricted access returns PermissionDenied.
/// Windows enumeration does not consult microphone capture capability; discovery
/// keeps its native query behavior independently of capture consent.
pub fn list_devices() -> Result<Vec<DeviceInfo>> {
    list_devices_with_namespaces()
        .map(|devices| devices.into_iter().map(|entry| entry.device).collect())
}

/// Enumerate CPAL microphone name selectors with their provider provenance.
/// IDs and discovery failures follow [`list_devices`]; the namespace describes
/// the selector domain and does not guarantee unique or persistent IDs.
pub fn list_devices_with_namespaces() -> Result<Vec<flexaudio_core::NamespacedDevice>> {
    let host = cpal_default_host(permission::Purpose::Enumeration)?;

    let default_name = host
        .default_input_device()
        .map(|device| device.name())
        .and_then(std::result::Result::ok);
    let devices = host
        .input_devices()
        .map_err(|error| input_config::query_failure("enumerate microphone devices", error))?;

    let mut out = Vec::new();
    for device in devices {
        // CPAL exposes no separate ID; an unreadable identity cannot be selected.
        let Ok(name) = device.name() else { continue };
        let format = input_config::discover(
            || {
                device.default_input_config().map_err(|error| {
                    input_config::query_failure("query microphone input configuration", error)
                })
            },
            |config| (config.sample_rate().0, config.channels()),
        );
        out.push(microphone_device(name, format, default_name.as_deref()));
    }
    Ok(out)
}

fn microphone_device(
    name: String,
    format: Result<(u32, u16)>,
    default_name: Option<&str>,
) -> flexaudio_core::NamespacedDevice {
    let (sample_rate, channels, format_available) =
        flexaudio_core::inventory::device_format(format);
    let is_default = default_name == Some(name.as_str());
    flexaudio_core::NamespacedDevice::new(
        {
            let mut device_value = DeviceInfo::new(name.clone(), name, SourceKind::Mic);
            device_value.sample_rate = sample_rate;
            device_value.channels = channels;
            device_value.format_available = format_available;
            device_value.is_loopback = false;
            device_value.is_default = is_default;
            device_value
        },
        flexaudio_core::DeviceIdNamespace::CpalDeviceName,
    )
}

#[cfg(test)]
mod namespace_tests {
    use super::*;

    #[test]
    fn microphone_enumeration_assigns_cpal_namespace_to_duplicate_names() {
        let entries = [Ok((44_100, 1)), Err(Error::DeviceLost)].map(|format| {
            microphone_device("duplicate input".into(), format, Some("duplicate input"))
        });
        for entry in &entries {
            assert_eq!(
                entry.id_namespace,
                flexaudio_core::DeviceIdNamespace::CpalDeviceName
            );
            assert_eq!(entry.device.id, "duplicate input");
            assert_eq!(entry.device.name, "duplicate input");
            assert_eq!(entry.device.source_kind, SourceKind::Mic);
            assert!(entry.device.is_default);
        }
        assert!(entries[0].device.format_available);
        assert!(!entries[1].device.format_available);
    }
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
        if let Some(error) = self.query_error.as_ref() {
            return Err(error.clone());
        }
        // Do nothing if the owner thread is already alive (safe for repeated start calls).
        if self.handle.is_some() {
            return Ok(());
        }
        // Each owner receives a fresh stop flag and callback mailbox, so old
        // callback failures cannot close PCM on a later generation.
        self.stop_flag = Arc::new(AtomicBool::new(false));
        self.callback_errors = Arc::new(callback_mailbox::CallbackMailbox::new(
            self.stop_flag.clone(),
        ));

        let stop_flag = self.stop_flag.clone();
        // cpal::Device is !Send, so pass only the device_id string and resolve it on the thread.
        let device_id = self.device_id.clone();
        let callback_errors = self.callback_errors.clone();
        // Ready channel reports build/play success or failure from the owner thread to start().
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();

        let handle = thread::Builder::new()
            .name("flexaudio-mic-cpal".into())
            .spawn(move || {
                capture_owner::run(sink, device_id, stop_flag, ready_tx, callback_errors);
            })
            .map_err(|e| {
                self.stop_flag.store(true, Ordering::SeqCst);
                flexaudio_core::spawn_error("CPAL microphone capture owner", e)
            })?;
        self.handle = Some(handle);
        let previous_shutdown = self.shutdown.take();

        // Wait for the owner thread to build and play the stream.
        let readiness = ready_rx.recv().unwrap_or_else(|_| {
            Err(
                Error::Backend("cpal mic thread exited before reporting readiness".into())
                    .with_context(ErrorContext::new(Operation::Start)),
            )
        });
        self.complete_start(readiness, previous_shutdown)
    }

    fn stop(&mut self) {
        self.retain_shutdown_outcome();
    }

    fn stop_checked(&mut self) -> Result<()> {
        self.retain_shutdown_outcome();
        self.shutdown
            .as_ref()
            .expect("stop retains its report")
            .result()
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
    let host = cpal_default_host(permission::Purpose::Capture)?;
    // None selects default; Some must uniquely match an advertised CPAL name.
    let device = resolve_input_device(&host, device_id)?;

    // The same checked configuration is passed to CPAL below. Never feed a sink
    // configured from a fallback or stale device format with different samples.
    let supported = input_config::checked(
        || {
            device.default_input_config().map_err(|error| {
                input_config::query_failure("query microphone input configuration", error)
            })
        },
        |config| (config.sample_rate().0, config.channels()),
        (sink.native_rate(), sink.native_channels()),
    )?;
    let sample_format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();

    let native_errors = callback_errors.clone();
    let err_fn = move |error: cpal::StreamError| native_errors.record(error);

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
    // behavior. The shared guard records unexpected panics in the lock-free mailbox,
    // closes PCM and lets the owner publish a typed terminal error outside the callback.
    let stream = match sample_format {
        SampleFormat::F32 => {
            let mut sink = sink;
            device.build_input_stream(
                &config,
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    callback_errors.guard(|| {
                        if stop_flag.load(Ordering::SeqCst) {
                            return;
                        }
                        // Already interleaved f32.
                        sink.push(data, monotonic_now_ns());
                    });
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
                    callback_errors.guard(|| {
                        if stop_flag.load(Ordering::SeqCst) {
                            return;
                        }
                        fill_scratch(&mut scratch, data, |s| s as f32 / -(i16::MIN as f32));
                        sink.push(&scratch, monotonic_now_ns());
                    });
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
                    callback_errors.guard(|| {
                        if stop_flag.load(Ordering::SeqCst) {
                            return;
                        }
                        // Map u16 [0, 65535] to [-1, 1) around midpoint 32768.
                        fill_scratch(&mut scratch, data, |s| (s as f32 - 32_768.0) / 32_768.0);
                        sink.push(&scratch, monotonic_now_ns());
                    });
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
                    callback_errors.guard(|| {
                        if stop_flag.load(Ordering::SeqCst) {
                            return;
                        }
                        fill_scratch(&mut scratch, data, |s| s as f32 / -(i32::MIN as f32));
                        sink.push(&scratch, monotonic_now_ns());
                    });
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
