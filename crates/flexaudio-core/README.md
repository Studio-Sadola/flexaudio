# flexaudio-core

OS-agnostic engine, types, and normalization pipeline for the `flexaudio` workspace.

This crate is the shared foundation that all `flexaudio` backends and the facade
depend on. It provides:

- **Shared types** — `AudioChunk`, `StreamConfig`, `SourceKind`, `OutputFormat`,
  `DeviceInfo`, `DeviceEvent`, `Error`, etc.
- **`CaptureBackend` trait** — the contract every OS backend implements.
- **Two-stage ring-buffer pipeline** — raw RT ring (`rtrb`) → `Normalizer`
  (mix + rubato SRC) → 20 ms chunk ring (`ringbuf`).
- **`Normalizer`** — converts arbitrary native formats to the fixed internal
  canonical form (48 kHz / stereo / interleaved `f32` / 20 ms chunks), then
  optionally re-converts to the caller-chosen `OutputFormat` in a second stage.
- **Clock utilities** — `ClockNormalizer`, `monotonic_now_ns`.

> **Most users should use the [`flexaudio`](https://crates.io/crates/flexaudio)
> facade crate instead of this one directly.** Depend on `flexaudio-core` only if
> you are implementing a custom capture backend that plugs into the `CaptureBackend`
> trait.

## License

[MIT](LICENSE) © 2026 tsubome / Aratech.
