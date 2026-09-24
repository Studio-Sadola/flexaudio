//! flexaudio — general-purpose cross-platform audio capture library (mic / system loopback /
//! per-process, Linux, Windows, and macOS).
//!
//! A facade that bundles the core + OS backends + mic via cfg.
//!
//! [`Stream`] drives the capture pipeline for one source (backend -> RawRing -> processing
//! thread -> Normalizer -> ChunkRing -> poll + watchdog recovery).

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

// Expose the types used together with `open()` directly from the facade top level. Callers
// and the napi binding can use `flexaudio::{StreamConfig, SourceKind, ...}` without going
// through `flexaudio::core`.
pub use flexaudio_core::backend::CaptureBackend;
pub use flexaudio_core::types::{
    AudioChunk, ChunkFlags, DeviceEvent, DeviceInfo, Error, Event, OutputFormat, ProcessInfo,
    ProcessMode, Result, SecondaryChunk, SourceKind, StreamConfig,
};

/// Returns the audio devices of all sources as a single list.
///
/// - Microphone input ([`core::SourceKind::Mic`], `is_loopback = false`) — via
///   [`flexaudio_mic::list_devices`] (cpal, all OSes).
/// - System audio output ([`core::SourceKind::SystemLoopback`],
///   `is_loopback = true`) — via the per-OS backend (Linux: PipeWire's Audio/Sink.
///   On Linux PipeWire also enumerates Audio/Source (microphones), so these may duplicate
///   the cpal entries. Windows/macOS: output endpoint enumeration). The returned `id` can be
///   used to select that output with `--source system --device-id <ID>`.
///
/// The `id` of each [`DeviceInfo`] is the most stable key obtainable (cpal = device name /
/// PipeWire = `node.name`). `is_default` is set on the OS default device.
///
/// # OS branches
/// - Linux: combines cpal (microphones) + PipeWire (sink + source). If there is no PipeWire
///   session, the PipeWire part is empty and only the cpal part is returned.
/// - Windows / macOS: combines cpal (microphones) + the OS output endpoints.
///
/// Even in an environment with no devices / where enumeration fails, it does not panic and
/// returns the list for whatever could be obtained (often empty).
pub fn devices() -> Result<Vec<DeviceInfo>> {
    // Microphone input (cpal) is common to all OSes. Linux extends it with the PipeWire part
    // afterwards and so needs mut, while other OSes do not extend it and do not need mut. The
    // difference is absorbed with allow.
    #[allow(unused_mut)]
    let mut all = flexaudio_mic::list_devices()?;

    // System output endpoints are per OS.
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

/// Starts a [`DeviceWatcher`] that watches device hotplug and default-device changes.
///
/// Calling [`DeviceWatcher::poll_event`] on the returned watcher periodically lets you pull
/// device connect / disconnect / default changes as [`DeviceEvent`]s. It is a separate channel
/// from the per-capture-stream [`core::Event`] and handles per-device occurrences.
///
/// # OS branches / degradation
/// - Linux: persistently watches the PipeWire registry (`flexaudio-os-linux`). When the
///   PipeWire daemon is absent or the connection fails, degrades to
///   [`NoopWatcher`](device_watcher) and returns `Ok` (hotplug just never arrives; the same
///   treatment as `devices()` swallowing an absent daemon into an empty list).
/// - Other OSes: always no-op (no hotplug is delivered).
///
/// It degrades without panicking even when PipeWire is absent, so in practice it returns `Ok`.
pub fn watch_devices() -> Result<DeviceWatcher> {
    device_watcher::watch_devices()
}

/// High-level entry point that picks a backend from a [`StreamConfig`] according to the source
/// kind and OS, and builds and returns a [`Stream`] (not started yet).
///
/// Callers (CLI, napi binding, etc.) do not build a backend themselves; they only pass a
/// `StreamConfig`. The low-level entry point [`Stream::open`] (where the caller passes a
/// `Box<dyn CaptureBackend>`) is kept for mock tests and advanced use.
///
/// The returned [`Stream`] is not capturing yet. The consumer calls [`Stream::start`] and then
/// periodically calls [`Stream::poll_chunk`] / [`Stream::poll_event`].
///
/// # Source -> backend branches
/// - [`SourceKind::Mic`] -> [`flexaudio_mic::CpalMicBackend`] (cpal, all OSes).
/// - [`SourceKind::SystemLoopback`] -> Linux / Windows / macOS
///   (Linux: [`flexaudio_os_linux::PwSystemBackend`] = the output's monitor / PipeWire.
///   Windows: `flexaudio_os_windows::WasapiSystemBackend` = WASAPI loopback of the render
///   endpoint). `config.exclude_self` (exclude own host) and `config.device_id`
///   (output endpoint selection; `None` for the default output) are passed through as-is.
///   On other OSes, [`Error::Unsupported`].
/// - [`SourceKind::ProcessLoopback`] -> Linux / Windows / macOS
///   (Linux: [`flexaudio_os_linux::PwProcessBackend`]. Windows:
///   `flexaudio_os_windows::WasapiProcessBackend`). `config.target_pid` is required;
///   without it, [`Error::InvalidArg`]. `config.mode` is passed through as-is.
///   On other OSes, [`Error::Unsupported`].
/// - [`SourceKind::Mix`] -> a composite backend that internally holds a mic child (cpal) and a
///   system child (per-OS loopback). Each child is brought to 48k/stereo, summed with per-side
///   gains (`config.mix_mic_gain` / `config.mix_system_gain`), and delivered as a single
///   stream. Devices are selected with `config.mix_mic_device_id` /
///   `config.mix_system_device_id` (`None` for the default). `config.exclude_self` is
///   applied to the system side. On OSes where the system side is unsupported,
///   [`Error::Unsupported`].
///
/// A process source looks only at `config.mode` and ignores `config.exclude_self`; a system
/// source looks only at `config.exclude_self` and ignores `config.mode`. The two are not
/// combined; each maps 1:1 onto the OS's single-PID exclusion primitive.
///
/// # Errors
/// - Unsupported output format -> [`Error::UnsupportedFormat`] (rejected early).
/// - ProcessLoopback with `target_pid` missing -> [`Error::InvalidArg`]
///   (`target_pid` is required even with [`ProcessMode::Exclude`]).
/// - Mix with `mix_mic_gain` / `mix_system_gain` non-finite or negative ->
///   [`Error::InvalidArg`].
/// - A source unsupported on the current OS (system/process/mix outside Linux/Windows/macOS)
///   -> [`Error::Unsupported`].
/// - Everything else comes from [`Stream::open`] (`ring_capacity_chunks == 0`, etc.).
///
/// # Exclusion (Exclude / exclude_self)
/// process [`ProcessMode::Exclude`] (all system audio except the target PID) / system
/// `exclude_self=true` (exclude own process) are supported on all 3 OSes: Linux, Windows, and
/// macOS. Linux implements it with a fan-in of non-target PipeWire nodes, and Windows/macOS
/// with each OS's native PID exclusion. Include / `exclude_self=false` captures the target
/// itself without exclusion.
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
    // Reject the output format first (Stream::open re-validates it too, but we want to return
    // the error before building the backend).
    config.output.validate()?;

    let backend = build_backend(&config)?;

    // Delegate to the low-level entry point (it handles the Normalizer setup and thread wiring).
    Stream::open(config, backend)
}

/// Builds one backend from a [`StreamConfig`] according to the source kind and OS.
///
/// [`open`] calls this after validating the output format.
/// [`Stream::switch_source`](crate::stream::Stream::switch_source) also uses the same function
/// to build the backend to switch to.
///
/// Branches and errors are the same as in the [`open`] documentation:
/// - [`SourceKind::Mic`] -> [`flexaudio_mic::CpalMicBackend`] (all OSes).
///   A specific input device can be selected by passing `config.device_id` (`None` for the
///   default input; the id is the stable ID returned by [`devices`] = device name. A mismatch
///   yields [`Error::DeviceNotFound`] at `start`). `config.device_id` applies to both mic
///   (input device) and system (output endpoint) (`None` for the default). process does not
///   look at it (the target is decided by target_pid).
/// - [`SourceKind::SystemLoopback`] -> supported on Linux/Windows/macOS
///   (Linux = [`flexaudio_os_linux::PwSystemBackend`] / Windows = WASAPI loopback /
///   macOS = CoreAudio Process Tap). `config.device_id` selects the output endpoint
///   (`None` for the default output). Unsupported OSes get [`Error::Unsupported`].
/// - [`SourceKind::ProcessLoopback`] -> supported on Linux/Windows/macOS
///   (Linux = [`flexaudio_os_linux::PwProcessBackend`] / Windows = WASAPI process loopback /
///   macOS = CoreAudio Process Tap). `target_pid` is required; missing yields
///   [`Error::InvalidArg`]. Unsupported OSes get [`Error::Unsupported`].
/// - [`SourceKind::Mix`] -> a composite backend (`mix::CompositeBackend`) holding a mic child
///   ([`flexaudio_mic::CpalMicBackend`], `config.mix_mic_device_id`) and a system child
///   ([`build_system_backend`], `config.exclude_self` + `config.mix_system_device_id`).
///   `mix_mic_gain` / `mix_system_gain` must be finite and >= 0, otherwise
///   [`Error::InvalidArg`]. OSes where the system side is unsupported get
///   [`Error::Unsupported`].
pub(crate) fn build_backend(config: &StreamConfig) -> Result<Box<dyn CaptureBackend>> {
    // Error is used in several branches (missing PID is InvalidArg, unsupported OS is
    // Unsupported), so it is imported at the top of the function.
    use flexaudio_core::types::Error;

    let backend: Box<dyn CaptureBackend> = match config.kind {
        // Microphone input is common to all OSes (cpal). device_id selects a specific input
        // device (None = default input device; the id is the stable ID returned by
        // devices() = device name). The same device_id also selects the output endpoint for
        // system.
        SourceKind::Mic => Box::new(flexaudio_mic::CpalMicBackend::new(config.device_id.clone())),

        // System output loopback is supported on Linux / Windows / macOS.
        // exclude_self (exclude own host) and device_id (output endpoint selection) are passed
        // to the backend. mode is not looked at. device_id=None means the default output.
        SourceKind::SystemLoopback => {
            build_system_backend(config.exclude_self, config.device_id.clone())?
        }

        // Process output loopback is supported on Linux / Windows / macOS; target_pid is
        // required. mode (Include/Exclude) is passed to the backend. exclude_self is not
        // looked at. target_pid is required even with mode:Exclude (InvalidArg if missing).
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

        // mic + system mix. Injects the two children into the composite backend. Per-side
        // gains are validated by the same criteria as the global gain (validated by
        // Stream::open). device_id is not looked at (mix_mic_device_id / mix_system_device_id
        // select each side).
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
            let mic = Box::new(flexaudio_mic::CpalMicBackend::new(
                config.mix_mic_device_id.clone(),
            ));
            // In Mix, exclude_self is applied to the system side (the feedback-prevention
            // intent is the same as for system alone). Unsupported OSes become Unsupported here.
            let system =
                build_system_backend(config.exclude_self, config.mix_system_device_id.clone())?;
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

/// Builds one system output loopback backend according to the OS.
///
/// A shared helper used by both [`SourceKind::SystemLoopback`] itself and the system side of
/// [`SourceKind::Mix`] (so the OS branching is not duplicated). `exclude_self` excludes the
/// own host, and `device_id` selects the output endpoint (`None` for the default output).
/// Anything other than Linux / Windows / macOS gets [`Error::Unsupported`].
fn build_system_backend(
    exclude_self: bool,
    device_id: Option<String>,
) -> Result<Box<dyn CaptureBackend>> {
    #[cfg(target_os = "linux")]
    {
        Ok(Box::new(flexaudio_os_linux::PwSystemBackend::new(
            exclude_self,
            device_id,
        )))
    }
    #[cfg(target_os = "windows")]
    {
        Ok(Box::new(flexaudio_os_windows::WasapiSystemBackend::new(
            exclude_self,
            device_id,
        )))
    }
    #[cfg(target_os = "macos")]
    {
        Ok(Box::new(flexaudio_os_macos::MacSystemBackend::new(
            exclude_self,
            device_id,
        )))
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        let _ = (exclude_self, device_id);
        Err(flexaudio_core::types::Error::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mix per-side gain validation: negative values and NaN are rejected with InvalidArg
    /// before the backend is built (no device is touched at all).
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
                    panic!(
                        "mix gains ({mic_gain}, {system_gain}) should be rejected with InvalidArg"
                    )
                }
                Err(Error::InvalidArg(msg)) => {
                    assert!(
                        msg.contains("mix_mic_gain") || msg.contains("mix_system_gain"),
                        "the message should say which gain is invalid: {msg}"
                    );
                }
                Err(other) => panic!("expected InvalidArg but got a different error: {other:?}"),
            }
        }
    }

    /// A valid Mix config gets through backend construction (open) (it is not started yet, so
    /// no real device is touched; holds in a headless environment too).
    #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
    #[test]
    fn mix_config_with_valid_gains_opens() {
        let config = StreamConfig {
            kind: SourceKind::Mix,
            mix_mic_gain: 0.5,
            mix_system_gain: 2.0,
            ..Default::default()
        };
        let stream = open(config).expect("a valid Mix config should get through open");
        // The composite backend advertises the internal canonical form (Stream stage 1 is a
        // pass-through).
        assert_eq!(stream.native_format(), (48_000, 2));
    }
}
