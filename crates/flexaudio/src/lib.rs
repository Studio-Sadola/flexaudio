//! flexaudio — a general-purpose cross-platform audio capture library (mic / system loopback /
//! per-process on Linux, Windows, and macOS).
//!
//! Facade that combines the core, OS backends, and mic through cfg.
//!
//! [`Stream`] drives the capture pipeline for one source (backend → RawRing → processing
//! thread → Normalizer → ChunkRing → poll, with watchdog recovery).

#![warn(missing_docs)]

pub use flexaudio_core as core;

pub mod device_watcher;
mod mix;
pub mod mock;
mod processes;
pub mod stream;

pub use device_watcher::DeviceWatcher;
pub use mock::MockBackend;
pub use processes::processes;
pub use stream::Stream;

// Re-export types used with `open()` from the facade root so consumers and napi bindings can
// use `flexaudio::{StreamConfig, SourceKind, ...}` without going through `flexaudio::core`.
pub use flexaudio_core::backend::CaptureBackend;
pub use flexaudio_core::types::{
    AudioChunk, ChunkFlags, DeviceEvent, DeviceInfo, Error, Event, OutputFormat, Permission,
    ProcessInfo, ProcessMode, Result, SecondaryChunk, SourceKind, StreamConfig,
};

/// Return audio devices for all sources in one list.
///
/// - Microphone input ([`core::SourceKind::Mic`], `is_loopback = false`) via
///   [`flexaudio_mic::list_devices`] (cpal, all OSes).
/// - System audio output ([`core::SourceKind::SystemLoopback`], `is_loopback = true`) via
///   OS-specific backends (Linux: PipeWire Audio/Sink. PipeWire also lists Audio/Source
///   (microphones) on Linux, so these may duplicate cpal entries. Windows/macOS: output
///   endpoint enumeration). Use a returned `id` with `--source system --device-id <ID>` to
///   select that output.
///
/// Each [`DeviceInfo`] uses the most stable available key for `id` (cpal=device name /
/// PipeWire=`node.name`). The OS default device has `is_default` set.
///
/// # OS-specific behavior
/// - Linux: Combine cpal microphones with PipeWire sinks and sources. If there is no PipeWire
///   session, the PipeWire portion is empty and only cpal devices are returned.
/// - Windows / macOS: Combine cpal microphones with OS output endpoints.
///
/// Does not panic when devices are absent or enumeration fails; returns the devices found
/// (often an empty list).
pub fn devices() -> Result<Vec<DeviceInfo>> {
    // Microphone input (cpal) is common to all OSes. Linux extends this with PipeWire devices,
    // so mut is needed there; other OSes do not extend it. The allow handles this difference.
    #[allow(unused_mut)]
    let mut all = flexaudio_mic::list_devices()?;

    // System output endpoints are OS-specific.
    #[cfg(target_os = "linux")]
    {
        let linux = flexaudio_os_linux::list_devices()?;
        all.extend(linux);
    }
    #[cfg(target_os = "windows")]
    {
        let win = flexaudio_os_windows::list_output_devices()?;
        all.extend(win);
    }
    #[cfg(target_os = "macos")]
    {
        let mac = flexaudio_os_macos::list_output_devices()?;
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
/// # OS-specific behavior and degraded mode
/// - Linux: Persistently monitor the PipeWire registry (`flexaudio-os-linux`). If the PipeWire
///   daemon is absent or a connection fails, degrade to [`NoopWatcher`](device_watcher) and
///   return `Ok` (no device-change events arrive, like `devices()` returning an empty list
///   when the daemon is absent).
/// - Other OSes: Always no-op (device changes are not reported).
///
/// Degrades without panicking when PipeWire is absent, so this normally returns `Ok`.
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
/// - [`SourceKind::Mic`] → [`flexaudio_mic::CpalMicBackend`] (cpal, all OSes).
/// - [`SourceKind::SystemLoopback`] → Linux / Windows / macOS
///   (Linux: [`flexaudio_os_linux::PwSystemBackend`] = output monitor / PipeWire.
///   Windows: `flexaudio_os_windows::WasapiSystemBackend` = WASAPI loopback on the render
///   endpoint). Pass through `config.exclude_self` (exclude this process) and
///   `config.device_id` (select the output endpoint; `None` uses the default output).
///   Other OSes return [`Error::Unsupported`].
/// - [`SourceKind::ProcessLoopback`] → Linux / Windows / macOS
///   (Linux: [`flexaudio_os_linux::PwProcessBackend`]; Windows:
///   `flexaudio_os_windows::WasapiProcessBackend`). `config.target_pid` is required; if it is
///   missing, [`Error::InvalidArg`] is returned. Pass through `config.mode`. Other OSes return
///   [`Error::Unsupported`].
/// - [`SourceKind::Mix`] → Composite backend containing a mic child (cpal) and system child
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
/// - Unsupported output format → [`Error::UnsupportedFormat`] (rejected early).
/// - Missing `target_pid` for ProcessLoopback → [`Error::InvalidArg`] (required even for
///   [`ProcessMode::Exclude`]).
/// - Non-finite or negative `mix_mic_gain` / `mix_system_gain` for Mix → [`Error::InvalidArg`].
/// - Source unsupported on this OS (system/process/mix on OSes other than Linux/Windows/macOS)
///   → [`Error::Unsupported`].
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
/// let config = StreamConfig {
///     kind: SourceKind::Mic,
///     ..Default::default()
/// };
/// let mut stream = open(config)?;
/// stream.start()?;
/// while let Some(chunk) = stream.poll_chunk() {
///     // chunk.data is interleaved f32 in the output format
///     let _ = chunk;
/// }
/// stream.stop();
/// # Ok::<(), flexaudio::Error>(())
/// ```
pub fn open(config: StreamConfig) -> Result<Stream> {
    // Validate the output format early. Stream::open also checks it, but errors should be
    // returned before constructing the backend.
    config.output.validate()?;
    validate_exclude_pids(&config)?;

    let backend = build_backend(&config)?;

    // Delegate to the low-level entry point (which configures the Normalizer and threads).
    Stream::open(config, backend)
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
/// - [`SourceKind::Mic`] → [`flexaudio_mic::CpalMicBackend`] (all OSes). Pass `config.device_id`
///   to select an input device (`None` uses the default). The ID is the stable device-name key
///   returned by [`devices`]; a mismatch returns [`Error::DeviceNotFound`] at `start`.
///   `config.device_id` applies to mic input and system output selection (`None` uses the
///   default); process capture does not use it (the target is selected by target_pid).
/// - [`SourceKind::SystemLoopback`] → supported on Linux/Windows/macOS
///   (Linux=[`flexaudio_os_linux::PwSystemBackend`] / Windows=WASAPI loopback /
///   macOS=CoreAudio Process Tap). Select an output endpoint with `config.device_id`
///   (`None` uses the default output). Other OSes return [`Error::Unsupported`].
/// - [`SourceKind::ProcessLoopback`] → supported on Linux/Windows/macOS
///   (Linux=[`flexaudio_os_linux::PwProcessBackend`] / Windows=WASAPI process loopback /
///   macOS=CoreAudio Process Tap). `target_pid` is required; if missing, return
///   [`Error::InvalidArg`]. Other OSes return [`Error::Unsupported`].
/// - [`SourceKind::Mix`] → `mix::CompositeBackend` with a mic child
///   ([`flexaudio_mic::CpalMicBackend`], `config.mix_mic_device_id`) and a system child
///   ([`build_system_backend`], `config.exclude_self` + `config.mix_system_device_id`).
///   `mix_mic_gain` / `mix_system_gain` must be finite and nonnegative or return
///   [`Error::InvalidArg`]. Return [`Error::Unsupported`] if system capture is unavailable.
pub(crate) fn build_backend(config: &StreamConfig) -> Result<Box<dyn CaptureBackend>> {
    // Error is used by multiple branches (missing PID -> InvalidArg, unsupported OS -> Unsupported),
    // so import it at the top of the function.
    use flexaudio_core::types::Error;

    let backend: Box<dyn CaptureBackend> = match config.kind {
        // Microphone input is common to all OSes (cpal). device_id selects an input device
        // (None=default input; id is the stable device-name key returned by devices()). The
        // same device_id selects the output endpoint for system capture.
        SourceKind::Mic => Box::new(flexaudio_mic::CpalMicBackend::try_new(
            config.device_id.clone(),
        )?),

        // System output loopback is supported on Linux / Windows / macOS.
        // Pass exclude_self (exclude this process) and device_id (select output endpoint)
        // to the backend. mode is ignored. device_id=None selects the default output.
        SourceKind::SystemLoopback => build_system_backend(
            config.exclude_self,
            config.exclude_pids.clone(),
            config.device_id.clone(),
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
                    Box::new(flexaudio_os_linux::PwProcessBackend::new(pid, config.mode))
                }
                #[cfg(target_os = "windows")]
                {
                    Box::new(flexaudio_os_windows::WasapiProcessBackend::new(
                        pid,
                        config.mode,
                    ))
                }
                #[cfg(target_os = "macos")]
                {
                    Box::new(flexaudio_os_macos::MacProcessBackend::new(pid, config.mode))
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
            let mic = Box::new(flexaudio_mic::CpalMicBackend::try_new(
                config.mix_mic_device_id.clone(),
            )?);
            // For Mix, apply exclude_self to the system side (same feedback-prevention intent
            // as standalone system capture). Unsupported OSes return Unsupported here.
            let system = build_system_backend(
                config.exclude_self,
                config.exclude_pids.clone(),
                config.mix_system_device_id.clone(),
            )?;
            Box::new(mix::CompositeBackend::new(
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
) -> Result<Box<dyn CaptureBackend>> {
    #[cfg(target_os = "linux")]
    {
        Ok(Box::new(
            flexaudio_os_linux::PwSystemBackend::new(exclude_self, device_id)
                .with_exclude_pids(exclude_pids),
        ))
    }
    #[cfg(target_os = "windows")]
    {
        Ok(Box::new(
            flexaudio_os_windows::WasapiSystemBackend::new(exclude_self, device_id)
                .with_exclude_pids(exclude_pids),
        ))
    }
    #[cfg(target_os = "macos")]
    {
        Ok(Box::new(
            flexaudio_os_macos::MacSystemBackend::new(exclude_self, device_id)
                .with_exclude_pids(exclude_pids),
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
            let config = StreamConfig {
                kind,
                exclude_pids: vec![0],
                ..Default::default()
            };
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
                let config = StreamConfig {
                    kind,
                    exclude_pids,
                    ..Default::default()
                };
                assert!(validate_exclude_pids(&config).is_ok());
            }
        }
        for kind in [SourceKind::Mic, SourceKind::ProcessLoopback] {
            let config = StreamConfig {
                kind,
                exclude_pids: vec![0],
                ..Default::default()
            };
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
            let config = StreamConfig {
                kind: SourceKind::Mix,
                mix_mic_gain: mic_gain,
                mix_system_gain: system_gain,
                ..Default::default()
            };
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

    /// A valid Mix config passes backend construction (open), including permission
    /// checks and native microphone format discovery on the host.
    #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
    #[test]
    fn mix_config_with_valid_gains_opens() {
        let config = StreamConfig {
            kind: SourceKind::Mix,
            mix_mic_gain: 0.5,
            mix_system_gain: 2.0,
            ..Default::default()
        };
        let stream = match open(config) {
            Ok(stream) => stream,
            #[cfg(any(target_os = "windows", target_os = "macos"))]
            Err(
                error @ Error::PermissionDenied {
                    permission: Permission::Microphone,
                    ..
                },
            ) => {
                eprintln!("Skipping mix_config_with_valid_gains_opens: host microphone permission is denied: {error}");
                return;
            }
            Err(error) => panic!("valid Mix config should open successfully: {error:?}"),
        };
        // The composite backend reports its internal canonical format (Stream's first stage is pass-through).
        assert_eq!(stream.native_format(), (48_000, 2));
    }
}
