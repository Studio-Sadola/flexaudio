//! flexaudio-os-windows — Windows backend: WASAPI system loopback / process
//! loopback (windows-rs 0.54, Windows build 20348 or later).
//!
//! Provides two [`CaptureBackend`](flexaudio_core::backend::CaptureBackend) implementations:
//!
//! - [`WasapiSystemBackend`] — classic render-endpoint loopback
//!   (`AUDCLNT_STREAMFLAGS_LOOPBACK`) to capture system audio output (the mix playing on that endpoint).
//!   Select an output endpoint with `device_id` (`None` uses the default). Equivalent to Linux’s
//!   [`PwSystemBackend`](../flexaudio_os_linux). List output endpoints with [`list_output_devices`].
//! - [`WasapiProcessBackend`] — `ActivateAudioInterfaceAsync` + process loopback
//!   (`AUDIOCLIENT_ACTIVATION_PARAMS`) to capture audio from a specific PID (its process tree).
//!   `exclude_self` reverses this to capture all system audio except that tree.
//!
//! Candidate processes that have audio sessions can be listed with [`list_processes`].
//!
//! # Handling `!Send`
//!
//! WASAPI COM interfaces such as `IAudioClient` are `!Send`, but the core contract
//! [`CaptureBackend`] requires `Send`. Keep COM initialization, capture, and destruction on one
//! dedicated thread. The backend struct stores only `Send` values (a stop flag [`AtomicBool`] /
//! [`JoinHandle`] / cached format). COM interfaces never cross thread boundaries. This follows the
//! same design as the cpal / PipeWire backends.
//!
//! # Non-Windows platforms
//!
//! The backend is gated by `#[cfg(target_os = "windows")]` and compiles to an empty crate elsewhere.
//! The `windows` dependency is only included in the Windows target section of `Cargo.toml`.
//! Build-number and capture lifecycle helpers are unit-tested without devices on non-Windows platforms.

#![warn(missing_docs)]

/// Build number → process-loopback availability. Decoupled from OS calls so it can be
/// unit-tested on non-Windows platforms.
mod version;

#[cfg(any(target_os = "windows", test))]
mod lifecycle;

#[cfg(any(target_os = "windows", test))]
mod format;

#[cfg(target_os = "windows")]
mod common;
#[cfg(target_os = "windows")]
mod keepalive;
#[cfg(target_os = "windows")]
mod owner;
#[cfg(target_os = "windows")]
mod process;
#[cfg(target_os = "windows")]
mod processes;
#[cfg(target_os = "windows")]
mod system;

#[cfg(target_os = "windows")]
pub use process::WasapiProcessBackend;
#[cfg(target_os = "windows")]
pub use processes::list_processes;
#[cfg(target_os = "windows")]
pub use system::{list_output_devices, WasapiSystemBackend};
