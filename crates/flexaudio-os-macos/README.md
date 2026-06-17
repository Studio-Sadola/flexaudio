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
The crate compiles as an empty stub on non-macOS targets
(`#![cfg(target_os = "macos")]`).

> **Most users should use the [`flexaudio`](https://crates.io/crates/flexaudio)
> facade crate instead of this one directly.** Depend on `flexaudio-os-macos`
> only if you are building a custom macOS audio pipeline that needs direct access
> to `MacSystemBackend` or `MacProcessBackend`.

## License

[MIT](LICENSE) © 2026 tsubome / Aratech.
