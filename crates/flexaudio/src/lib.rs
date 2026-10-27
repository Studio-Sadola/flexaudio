//! flexaudio -- a general-purpose cross-platform audio capture library (mic / system loopback /
//! per-process on Linux, Windows, and macOS).
//!
//! Facade that combines the core, OS backends, and mic through cfg.
//!
//! [`Stream`] drives the capture pipeline for one source (backend -> RawRing -> processing
//! thread -> Normalizer -> ChunkRing -> poll, with watchdog recovery).

#![warn(missing_docs)]
#![deny(clippy::let_underscore_must_use)]

pub use flexaudio_core as core;
pub use flexaudio_core::{
    set_drop_failure_handler, DropFailure, DropFailureHandler, DropFailureOwner,
};

mod capabilities;
#[cfg(test)]
mod capture_cell_tests;
pub use capabilities::capabilities;
pub use flexaudio_core::capabilities::{
    Availability, BackendKind, Capabilities, DeviceSelectionBehavior, DeviceWatcherCapabilities,
    OsVersion, PidScope, ProcessCapabilities, SideEffectCapabilities, SideEffectCapability,
    SideEffectCondition, SourceCapabilities, SourceCapability, SystemExclusionCapabilities,
    UnavailableReason, WatchScope, WatcherMode,
};

pub mod device_watcher;
mod mix;
pub mod mock;
mod open_options;
mod processes;
mod settings;
#[cfg(test)]
mod settings_guard_tests;
mod settings_request;
#[cfg(test)]
mod settings_request_tests;
mod settings_resolver;
#[cfg(test)]
mod settings_resolver_tests;
#[cfg(test)]
mod settings_tests;
mod stop_contract;

pub use settings_request::{SettingsAttachments, SettingsRequest};
pub use settings_resolver::resolve_settings;
pub mod stream;
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub mod test_support;
mod validation;
pub use validation::validate_open;

pub use device_watcher::DeviceWatcher;
pub use mock::MockBackend;
pub use open_options::OpenOptions;
pub use processes::processes;
pub use stream::Stream;

// Re-export types used with `open()` from the facade root so consumers and napi bindings can
// use `flexaudio::{StreamConfig, SourceKind, ...}` without going through `flexaudio::core`.
pub use flexaudio_core::backend::CaptureBackend;
pub use flexaudio_core::types::{
    AudioChunk, AudioLoss, AudioPath, ChunkFlags, DefaultDeviceKind, DeviceEvent, DeviceInfo,
    Error, ErrorContext, ErrorGroup, ErrorKind, Event, LossReason, MixLane, NativeStatus,
    Operation, OutputFormat, OutputTap, Permission, ProcessInfo, ProcessMode, Result,
    SecondaryChunk, ShutdownReport, SourceKind, StreamConfig, VadErrorCode,
};
pub use flexaudio_core::{AddonErrorKind, HelperStopReason, StartupDropReason};
pub use flexaudio_core::{
    AdjustmentReason, DeviceIdNamespace, IgnoreReason, NamespacedDevice, SettingKey,
    SettingOutcome, SettingResolution, SettingValue, SettingsReport,
};

/// Return audio devices for all sources in one list.
///
/// - Microphone input ([`core::SourceKind::Mic`], `is_loopback = false`) via
///   [`flexaudio_mic::list_devices`] (cpal, all OSes).
/// - System audio output ([`core::SourceKind::SystemLoopback`], `is_loopback = true`) via
///   OS-specific backends (Linux: PipeWire Audio/Sink; Windows/macOS: output
///   endpoint enumeration). Use a returned `id` with `--source system --device-id <ID>` to
///   select that output.
///
/// Each [`DeviceInfo`] uses the most stable available key for `id` (cpal=device name /
/// PipeWire=`node.name`). The OS default device has `is_default` set.
/// Linux microphones use only cpal IDs, which the microphone backend can resolve.
/// PipeWire Audio/Source IDs are omitted because they cannot select a cpal input.
///
/// Collection/count failures return Err even if another provider returned devices.
/// Per-entry optional metadata stays unknown; unreadable identities may be omitted.
/// Unreadable formats keep entries with format_available=false and 0/0.
/// On macOS, enumeration never prompts. With undecided microphone consent it returns
/// a typed [`Error::Backend`] before any HAL/device/format query. Grant the responsible
/// host access or use the bounded capture-open consent flow, then retry enumeration.
/// Denied/restricted consent returns [`Error::PermissionDenied`]. Unreadable mandatory DeviceInfo formats
/// use format_available=false and the 0/0 sentinel; other entries remain usable.
/// Windows enumeration does not preflight microphone permission; native enumeration
/// proceeds. Microphone capture retains its separate existing permission preflight.
pub fn devices() -> Result<Vec<DeviceInfo>> {
    devices_with_namespaces().map(|devices| devices.into_iter().map(|entry| entry.device).collect())
}

/// Enumerate selectable devices with provenance supplied by each provider.
/// Device IDs and discovery errors are identical to [`devices`].
pub fn devices_with_namespaces() -> Result<Vec<NamespacedDevice>> {
    // Microphone input (cpal) is common to all OSes. Linux extends this with PipeWire devices,
    // so mut is needed there; other OSes do not extend it. The allow handles this difference.
    #[allow(unused_mut)]
    // Each provider owns its Enumerate context; preserve native context without wrapping twice.
    let mut all = flexaudio_mic::list_devices_with_namespaces()?;

    // System output endpoints are OS-specific.
    #[cfg(target_os = "linux")]
    {
        let linux = flexaudio_os_linux::list_devices_with_namespaces()?;
        all.extend(
            linux
                .into_iter()
                .filter(|entry| entry.device.source_kind == SourceKind::SystemLoopback),
        );
    }
    #[cfg(target_os = "windows")]
    {
        let win = flexaudio_os_windows::list_output_devices_with_namespaces()?;
        all.extend(win);
    }
    #[cfg(target_os = "macos")]
    {
        let mac = flexaudio_os_macos::list_output_devices_with_namespaces()?;
        all.extend(mac);
    }

    Ok(all)
}

/// Start a [`DeviceWatcher`] for device and default-device changes (hot-plug).
///
/// Call [`DeviceWatcher::poll_event`] periodically to pull device connection, disconnection,
/// and default-device changes as [`DeviceEvent`] values. These are device-level events,
/// separate from capture-stream-level [`core::Event`].
///
/// Linux watches the PipeWire registry and returns Err on startup failure.
/// Only system output changes are reported, using the same IDs as [`devices`].
/// PipeWire microphone IDs cannot select CPAL inputs, so microphone changes are omitted.
/// Unsupported platforms intentionally provide a no-op watcher.
pub fn watch_devices() -> Result<DeviceWatcher> {
    device_watcher::watch_devices()
}

/// High-level entry point that selects a backend based on the source kind and OS, then builds
/// and returns a [`Stream`] (not started yet).
///
/// Consumers (CLI, napi bindings, etc.) only need to pass `StreamConfig`; they do not construct
/// a backend themselves. The low-level entry point [`Stream::open`] (which accepts a
/// `Box<dyn CaptureBackend>`) remains available for mock tests and advanced use.
///
/// The returned [`Stream`] has not started capturing. The consumer calls [`Stream::start`],
/// then periodically calls [`Stream::poll_chunk`] / [`Stream::poll_event`].
///
/// # Source-to-backend mapping
/// - [`SourceKind::Mic`] -> [`flexaudio_mic::CpalMicBackend`] (cpal, all OSes).
/// - [`SourceKind::SystemLoopback`] -> Linux / Windows / macOS
///   (Linux: [`flexaudio_os_linux::PwSystemBackend`] = output monitor / PipeWire.
///   Windows: `flexaudio_os_windows::WasapiSystemBackend` = WASAPI loopback on the render
///   endpoint). Pass through `config.exclude_self` (exclude this process) and
///   `config.device_id` (select the output endpoint; `None` uses the default output).
///   Other OSes return [`Error::Unsupported`].
/// - [`SourceKind::ProcessLoopback`] -> Linux / Windows / macOS
///   (Linux: [`flexaudio_os_linux::PwProcessBackend`]; Windows:
///   `flexaudio_os_windows::WasapiProcessBackend`). `config.target_pid` is required; if it is
///   missing, [`Error::InvalidArg`] is returned. Pass through `config.mode`. Other OSes return
///   [`Error::Unsupported`].
/// - [`SourceKind::Mix`] -> Composite backend containing a mic child (cpal) and system child
///   (OS-specific loopback). Each child is normalized to 48 kHz stereo, scaled by its own gain
///   (`config.mix_mic_gain` / `config.mix_system_gain`), and mixed into one stream. Select
///   devices with `config.mix_mic_device_id` / `config.mix_system_device_id` (`None` uses the
///   default). `config.exclude_self` applies to the system child. If system capture is not
///   supported on the OS, return [`Error::Unsupported`].
///
/// Process sources use only `config.mode` and ignore `config.exclude_self`; system sources
/// use only `config.exclude_self` and ignore `config.mode`. They are not combined: each maps
/// one-to-one to the OS's single-PID exclusion primitive.
///
/// # Errors
/// - Unsupported output format -> [`Error::UnsupportedFormat`] (rejected early).
/// - Missing `target_pid` for ProcessLoopback -> [`Error::InvalidArg`] (required even for
///   [`ProcessMode::Exclude`]).
/// - Non-finite or negative `mix_mic_gain` / `mix_system_gain` for Mix -> [`Error::InvalidArg`].
/// - Source unsupported on this OS (system/process/mix on OSes other than Linux/Windows/macOS)
///   -> [`Error::Unsupported`].
/// - Other errors come from [`Stream::open`] (such as `ring_capacity_chunks == 0`).
///
/// # Exclusion (Exclude / exclude_self)
/// Process [`ProcessMode::Exclude`] (all system audio except the target PID) and system
/// `exclude_self=true` (exclude this process) are supported on Linux, Windows, and macOS.
/// Linux uses PipeWire fan-in of all nodes except the target; Windows/macOS use native
/// per-process exclusion. Include / `exclude_self=false` captures the target without exclusion.
///
/// # Example
/// ```no_run
/// use flexaudio::{open, StreamConfig, SourceKind};
///
/// let config = StreamConfig::new(SourceKind::Mic);
/// let mut stream = open(config)?;
/// stream.start()?;
/// while let Some(chunk) = stream.poll_chunk() {
///     // chunk.data is interleaved f32 in the output format
///     let _ = chunk;
/// }
/// stream.stop();
/// # Ok::<(), flexaudio::Error>(())
/// ```
/// For macOS microphone/Mix, consent is checked before any HAL/device-format query.
/// An undecided bundled prompt waits at most 30 seconds; an unanswered prompt returns
/// a typed [`Error::Backend`] without querying HAL or starting capture. Answer the
/// prompt and retry. Undecided consent without a nonempty microphone usage description
/// fails immediately; configure the bundle or grant the responsible host access first.
/// This bound covers the undecided consent wait, not arbitrary native queries after
/// authorization. Opening is synchronous; GUI hosts should use a worker thread.
/// Pull PCM does not require an event poll. Startup notices enter the ordinary FIFO
/// event queue before the affected generation's first chunk is published.
pub fn open(config: StreamConfig) -> Result<Stream> {
    open_with_options(config, OpenOptions::default())
}

/// Open a not-yet-started capture stream with explicit backend options.
///
/// Uses the same validation and source selection as [`open`]. Options that do
/// not apply to the selected operating system or source are rejected when non-default.
/// [`OpenOptions::strict`] defaults to true; set false to allow ignored settings.
/// Non-default ignored settings in strict mode return
/// [`Error::IgnoredSettings`] before backend construction or native access.
pub fn open_with_options(config: StreamConfig, options: OpenOptions) -> Result<Stream> {
    open_using(config, options, build_backend)
}

fn open_using(
    config: StreamConfig,
    options: OpenOptions,
    build: impl FnOnce(&StreamConfig, &OpenOptions) -> Result<Box<dyn CaptureBackend>>,
) -> Result<Stream> {
    // Share custom-open validation, but run it before any native construction.
    let report = validate_open(&config, &options)?;
    let backend = build(&config, &options)?;

    // Delegate to the low-level entry point (which configures the Normalizer and threads).
    let mut stream = Stream::open_with_backend_options(config, backend, options)?;
    stream.settings_report = Some(report);
    Ok(stream)
}

/// Shared preflight used by the Mix branch of open's backend construction.
fn validate_mix_gains(config: &StreamConfig) -> Result<()> {
    for (name, gain) in [
        ("mix_mic_gain", config.mix_mic_gain),
        ("mix_system_gain", config.mix_system_gain),
    ] {
        if !gain.is_finite() || gain < 0.0 {
            return Err(Error::InvalidArg(format!(
                "{name} must be finite and >= 0.0, got {gain}"
            )));
        }
    }
    Ok(())
}

/// Validate exclusion PIDs only for sources that use system capture.
pub(crate) fn validate_exclude_pids(config: &StreamConfig) -> Result<()> {
    if matches!(config.kind, SourceKind::SystemLoopback | SourceKind::Mix)
        && config.exclude_pids.contains(&0)
    {
        return Err(Error::InvalidArg(
            "exclude_pids: pid 0 is not a valid process id".into(),
        ));
    }
    Ok(())
}

/// Build one backend from [`StreamConfig`] based on the source kind and OS.
///
/// [`open`] calls this after validating the output format.
/// [`Stream::switch_source`](crate::stream::Stream::switch_source) uses the same function to
/// build the replacement backend.
///
/// The mappings and errors are the same as documented for [`open`]:
/// - [`SourceKind::Mic`] -> [`flexaudio_mic::CpalMicBackend`] (all OSes). Pass `config.device_id`
///   to select an input device (`None` uses the default). The ID is the stable device-name key
///   returned by [`devices`]; a mismatch returns [`Error::DeviceNotFound`] at `start`.
///   `config.device_id` applies to mic input and system output selection (`None` uses the
///   default); process capture does not use it (the target is selected by target_pid).
/// - [`SourceKind::SystemLoopback`] -> supported on Linux/Windows/macOS
///   (Linux=[`flexaudio_os_linux::PwSystemBackend`] / Windows=WASAPI loopback /
///   macOS=CoreAudio Process Tap). Select an output endpoint with `config.device_id`
///   (`None` uses the default output). Other OSes return [`Error::Unsupported`].
/// - [`SourceKind::ProcessLoopback`] -> supported on Linux/Windows/macOS
///   (Linux=[`flexaudio_os_linux::PwProcessBackend`] / Windows=WASAPI process loopback /
///   macOS=CoreAudio Process Tap). `target_pid` is required; if missing, return
///   [`Error::InvalidArg`]. Other OSes return [`Error::Unsupported`].
/// - [`SourceKind::Mix`] -> `mix::CompositeBackend` with a mic child
///   ([`flexaudio_mic::CpalMicBackend`], `config.mix_mic_device_id`) and a system child
///   ([`build_system_backend`], `config.exclude_self` + `config.mix_system_device_id`).
///   `mix_mic_gain` / `mix_system_gain` must be finite and nonnegative or return
///   [`Error::InvalidArg`]. Return [`Error::Unsupported`] if system capture is unavailable.
pub(crate) fn build_backend(
    config: &StreamConfig,
    options: &OpenOptions,
) -> Result<Box<dyn CaptureBackend>> {
    // Error is used by multiple branches (missing PID -> InvalidArg, unsupported OS -> Unsupported),
    // so import it at the top of the function.
    use flexaudio_core::types::Error;
    stream::validate_chunk_ms(config.chunk_ms)?;

    let backend: Box<dyn CaptureBackend> = match config.kind {
        // Microphone input is common to all OSes (cpal). device_id selects an input device
        // (None=default input; id is the stable device-name key returned by devices()). The
        // same device_id selects the output endpoint for system capture.
        SourceKind::Mic => stop_contract::builtin(flexaudio_mic::CpalMicBackend::try_new(
            config.device_id.clone(),
        )?),

        // System output loopback is supported on Linux / Windows / macOS.
        // Pass exclude_self (exclude this process) and device_id (select output endpoint)
        // to the backend. mode is ignored. device_id=None selects the default output.
        SourceKind::SystemLoopback => build_system_backend(
            config.exclude_self,
            config.exclude_pids.clone(),
            config.device_id.clone(),
            config.silence_probe,
            options,
        )?,

        // Per-process output loopback is supported on Linux / Windows / macOS and requires target_pid.
        // Pass mode (Include/Exclude) to the backend. exclude_self is ignored.
        // target_pid is required even for mode:Exclude (otherwise InvalidArg).
        SourceKind::ProcessLoopback => {
            #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
            {
                let pid = config.target_pid.ok_or_else(|| {
                    Error::InvalidArg("ProcessLoopback requires target_pid".into())
                })?;
                #[cfg(target_os = "linux")]
                {
                    stop_contract::builtin(flexaudio_os_linux::PwProcessBackend::new(
                        pid,
                        config.mode,
                    ))
                }
                #[cfg(target_os = "windows")]
                {
                    stop_contract::builtin(flexaudio_os_windows::WasapiProcessBackend::new(
                        pid,
                        config.mode,
                    ))
                }
                #[cfg(target_os = "macos")]
                {
                    stop_contract::builtin(
                        flexaudio_os_macos::MacProcessBackend::new(pid, config.mode)
                            .with_silence_probe(config.silence_probe),
                    )
                }
            }
            #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
            {
                return Err(Error::Unsupported);
            }
        }

        // Mix mic and system by injecting both children into the composite backend. Validate
        // per-side gains using the same rules as global gain (checked by Stream::open).
        // Ignore device_id; select each side with mix_mic_device_id / mix_system_device_id.
        SourceKind::Mix => {
            validate_mix_gains(config)?;
            let mic = stop_contract::builtin(flexaudio_mic::CpalMicBackend::try_new(
                config.mix_mic_device_id.clone(),
            )?);
            // For Mix, apply exclude_self to the system side (same feedback-prevention intent
            // as standalone system capture). Unsupported OSes return Unsupported here.
            let system = build_system_backend(
                config.exclude_self,
                config.exclude_pids.clone(),
                config.mix_system_device_id.clone(),
                config.silence_probe,
                options,
            )?;
            stop_contract::builtin(mix::CompositeBackend::new(
                mic,
                system,
                config.mix_mic_gain,
                config.mix_system_gain,
            ))
        }
    };

    Ok(backend)
}

/// Build the system output loopback backend for the current OS.
///
/// Shared helper for [`SourceKind::SystemLoopback`] and the system side of [`SourceKind::Mix`]
/// to avoid duplicating OS branches. `exclude_self` excludes this process; `device_id`
/// selects an output endpoint (`None` uses the default). OSes other than Linux / Windows / macOS
/// return [`Error::Unsupported`].
/// `exclude_pids` are extra pids excluded alongside `exclude_self`
/// (see [`StreamConfig::exclude_pids`]); empty = exclusion is `exclude_self` alone.
fn build_system_backend(
    exclude_self: bool,
    exclude_pids: Vec<u32>,
    device_id: Option<String>,
    silence_probe: bool,
    options: &OpenOptions,
) -> Result<Box<dyn CaptureBackend>> {
    #[cfg(not(target_os = "macos"))]
    let _ = silence_probe;
    #[cfg(not(target_os = "windows"))]
    let _ = options;
    #[cfg(target_os = "linux")]
    {
        Ok(stop_contract::builtin(
            flexaudio_os_linux::PwSystemBackend::new(exclude_self, device_id)
                .with_exclude_pids(exclude_pids),
        ))
    }
    #[cfg(target_os = "windows")]
    {
        Ok(stop_contract::builtin(
            flexaudio_os_windows::WasapiSystemBackend::new(exclude_self, device_id)
                .with_exclude_pids(exclude_pids)
                .with_classic_loopback_keepalive(options.windows_classic_loopback_keepalive),
        ))
    }
    #[cfg(target_os = "macos")]
    {
        Ok(stop_contract::builtin(
            flexaudio_os_macos::MacSystemBackend::new(exclude_self, device_id)
                .with_exclude_pids(exclude_pids)
                .with_silence_probe(silence_probe),
        ))
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        let _ = (exclude_self, exclude_pids, device_id);
        Err(flexaudio_core::types::Error::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_rejects_zero_exclusion_pid_for_system_capture() {
        for kind in [SourceKind::SystemLoopback, SourceKind::Mix] {
            let mut config = StreamConfig::new(kind);
            config.exclude_pids = vec![0];
            assert!(matches!(
                open(config),
                Err(Error::InvalidArg(message))
                    if message == "exclude_pids: pid 0 is not a valid process id"
            ));
        }
    }

    #[test]
    fn exclusion_pid_validation_accepts_positive_and_ignored_pids() {
        for kind in [SourceKind::SystemLoopback, SourceKind::Mix] {
            for exclude_pids in [vec![], vec![42]] {
                let mut config = StreamConfig::new(kind);
                config.exclude_pids = exclude_pids;
                assert!(validate_exclude_pids(&config).is_ok());
            }
        }
        for kind in [SourceKind::Mic, SourceKind::ProcessLoopback] {
            let mut config = StreamConfig::new(kind);
            config.exclude_pids = vec![0];
            assert!(validate_exclude_pids(&config).is_ok());
        }
    }

    /// Mix per-side gain validation: negative or NaN values return InvalidArg before backend
    /// construction (no devices are touched).
    #[test]
    fn mix_config_rejects_invalid_side_gains() {
        for (mic_gain, system_gain) in [
            (-1.0f32, 1.0f32),
            (1.0, -0.5),
            (f32::NAN, 1.0),
            (1.0, f32::INFINITY),
        ] {
            let mut config = StreamConfig::new(SourceKind::Mix);
            config.mix_mic_gain = mic_gain;
            config.mix_system_gain = system_gain;
            match open(config) {
                Ok(_) => {
                    panic!("mix gains ({mic_gain}, {system_gain}) should be rejected as InvalidArg")
                }
                Err(Error::InvalidArg(msg)) => {
                    assert!(
                        msg.contains("mix_mic_gain") || msg.contains("mix_system_gain"),
                        "message should identify which gain is invalid: {msg}"
                    );
                }
                Err(other) => panic!("expected InvalidArg, got a different error: {other:?}"),
            }
        }
    }

    /// Valid Mix gains pass the same preflight as the public open path.
    #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
    #[test]
    fn valid_mix_gains_pass_preflight_and_open_mock_composite() {
        let mut config = StreamConfig::new(SourceKind::Mix);
        config.mix_mic_gain = 0.5;
        config.mix_system_gain = 2.0;
        // Public open also queries real microphone/system devices. Exercise its
        // shared gain validator here so this test stays hardware-free.
        validate_mix_gains(&config).expect("public open Mix gain validation");
        let backend = mix::CompositeBackend::new(
            Box::new(MockBackend::new(48_000, 2, 0.0)),
            Box::new(MockBackend::new(48_000, 2, 0.0)),
            config.mix_mic_gain,
            config.mix_system_gain,
        );
        let stream = Stream::open(config, Box::new(backend)).expect("valid Mix configuration");
        // The composite backend reports its internal canonical format (Stream's first stage is pass-through).
        assert_eq!(stream.native_format(), (48_000, 2));
    }
}
