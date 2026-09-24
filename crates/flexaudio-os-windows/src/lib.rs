//! flexaudio-os-windows — Windows backend: WASAPI loopback / process loopback
//! (windows-rs 0.54, Windows build 20348 or later).
//!
//! Provides two [`CaptureBackend`](flexaudio_core::backend::CaptureBackend)s:
//!
//! - [`WasapiSystemBackend`] — captures the system audio output (the mix flowing into that
//!   endpoint) via classic loopback on a render endpoint (`AUDCLNT_STREAMFLAGS_LOOPBACK`).
//!   `device_id` selects the output endpoint (`None` for the default). The counterpart of
//!   Linux's [`PwSystemBackend`](../flexaudio_os_linux). The list of output endpoints is
//!   available from [`list_output_devices`].
//! - [`WasapiProcessBackend`] — captures the audio of a specific PID (its process tree) via
//!   `ActivateAudioInterfaceAsync` + process loopback (`AUDIOCLIENT_ACTIVATION_PARAMS`).
//!   `exclude_self` inverts this into "all system audio except the target tree".
//!
//! Candidate processes to capture (processes that have an audio session) are available from
//! [`list_processes`].
//!
//! # Working around `!Send`
//!
//! WASAPI COM interfaces such as `IAudioClient` are `!Send`, but the core contract
//! [`CaptureBackend`] requires `Send`. Everything from COM initialization through capture to
//! teardown happens on one dedicated thread, and the backend struct holds only `Send` things
//! (the stop flag [`AtomicBool`] / [`JoinHandle`] / the cached format). COM interfaces never
//! cross a thread boundary. Same design as the cpal / PipeWire backends.
//!
//! # Non-Windows
//!
//! The backend itself compiles to nothing on non-Windows targets via
//! `#[cfg(target_os = "windows")]`, and the `windows` dependency is pulled in only by the
//! `target.'cfg(...windows)'` section of `Cargo.toml`. Only the build-number check (a pure
//! function) also compiles and is unit-tested on non-Windows targets.

#![warn(missing_docs)]

/// Build number → whether process loopback is available. Decoupled from OS calls so it
/// can be unit-tested on non-Windows targets.
mod version;

#[cfg(target_os = "windows")]
mod common;
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
