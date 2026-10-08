# flexaudio-os-macos

macOS audio capture backend for the `flexaudio` workspace, built on Core Audio
Process Taps via [objc2-core-audio](https://crates.io/crates/objc2-core-audio).

Provides two `CaptureBackend` implementations:

- **`MacSystemBackend`** — records the full system audio output mix via a Core
  Audio process tap with an EXCLUDE-self configuration (captures everything playing
  on the system). The macOS equivalent of WASAPI loopback on Windows.
- **`MacProcessBackend`** — records audio from a specific process (INCLUDE tap),
  or everything except that process (EXCLUDE tap), using `CATapDescription` →
  process tap → private aggregate device → `IOProc` callback.

Because the ObjC objects involved (`Retained<CATapDescription>`, `RcBlock`,
`TapChain`) are `!Send`, the entire tap chain is created and torn down on a
dedicated thread; only `Send`-safe handles (stop flag, `JoinHandle`, cached format)
cross thread boundaries.

**Platform requirement:** Core Audio Process Taps require **macOS 14.4 or later**.
Attempting to start a backend on an older OS returns `Error::UnsupportedOsVersion`.
Native backends are unavailable on non-macOS targets; the private capture-health
policy still compiles there for device-free tests.

Tap creation failures with Core Audio's illegal-operation status produce
`Error::PermissionDenied { permission: Permission::SystemAudio, detail }` with
privacy-setting guidance. Successful creation does not prove recording consent:
macOS can deliver zero samples while permission is missing.

There is no public system-audio permission-status API, and no private TCC APIs
are used. After five continuous seconds of bit-exact zero native samples while
another eligible process has `IsRunningOutput` true, the backend runs one active
self-probe per capture generation. Process exclusions and selected-device routing
restrict eligibility; query failures, missing delivery, dropped samples, negative
zero, nonzero samples, and unknown routing reset the observation window.

The probe renders a roughly 300 ms, phase-coded 1 kHz signal at amplitude `1e-5`
on the default output, designed to be inaudible, and captures only our own process
through a separate private tap. Recognizing the signal suppresses the warning.
Proven rendering with continuously exact-zero diagnostic capture produces terminal
`Event::PermissionDenied { permission: Permission::SystemAudio, detail }` with
System Audio Recording privacy-setting and restart/retry guidance. Setup failures,
missing render/capture evidence, or timeout produce the existing
`Event::SilenceWhileSourceActive { detail }` advisory and continue capture because
missing permission and genuine digital silence remain possible. The signal may
appear in user capture when our own process is included. Stop cancels the probe
and suppresses late notifications. Call `CaptureBackend::poll_event` to receive
backend notifications when using a backend directly.

> **Most users should use the [`flexaudio`](https://crates.io/crates/flexaudio)
> facade crate instead of this one directly.** Depend on `flexaudio-os-macos`
> only if you are building a custom macOS audio pipeline that needs direct access
> to `MacSystemBackend` or `MacProcessBackend`.

## License

[MIT](LICENSE) © 2026 tubome / Studio Sadola.
