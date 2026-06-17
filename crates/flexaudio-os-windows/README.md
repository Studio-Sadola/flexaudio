# flexaudio-os-windows

Windows audio capture backend for the `flexaudio` workspace, built on WASAPI via
[windows-rs](https://crates.io/crates/windows) 0.54.

Provides two `CaptureBackend` implementations:

- **`WasapiSystemBackend`** — classic WASAPI loopback
  (`AUDCLNT_STREAMFLAGS_LOOPBACK`) that records the full system output mix (all
  audio flowing to the default render endpoint).
- **`WasapiProcessBackend`** — process-tree loopback via
  `ActivateAudioInterfaceAsync` + `AUDIOCLIENT_ACTIVATION_PARAMS`, which captures
  audio from a specific PID (and its child processes). Setting `exclude_self`
  inverts the filter to capture all system audio except that process tree.

Because WASAPI COM interfaces are `!Send`, all COM initialization, capture, and
teardown happen on a dedicated thread; only `Send`-safe handles (stop flag,
`JoinHandle`, cached format) cross thread boundaries.

**Platform requirement:** WASAPI loopback is available on Windows Vista and later;
process loopback (`WasapiProcessBackend`) requires Windows 10 build 20348 or later.
The crate compiles as an empty stub on non-Windows targets
(`#![cfg(target_os = "windows")]`).

> **Most users should use the [`flexaudio`](https://crates.io/crates/flexaudio)
> facade crate instead of this one directly.** Depend on `flexaudio-os-windows`
> only if you are building a custom Windows audio pipeline that needs direct access
> to `WasapiSystemBackend` or `WasapiProcessBackend`.

## License

[MIT](LICENSE) © 2026 tsubome / Aratech.
