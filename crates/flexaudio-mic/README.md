# flexaudio-mic

Cross-platform microphone capture backend for the `flexaudio` workspace, built on
[cpal](https://crates.io/crates/cpal).

`CpalMicBackend` implements the `CaptureBackend` trait from `flexaudio-core`. It
opens the default input device (or a named device by ID) and pushes raw interleaved
`f32` frames into a `RawSink` in a non-blocking, RT-safe way. Because `cpal::Stream`
is `!Send`, the stream is created and owned on a dedicated thread; only `Send`-safe
handles (stop flag, `JoinHandle`, cached format) cross thread boundaries.

Primary test target is Linux/ALSA, but the implementation works on any OS that cpal
supports (Linux, Windows, macOS).

> **Most users should use the [`flexaudio`](https://crates.io/crates/flexaudio)
> facade crate instead of this one directly.** Depend on `flexaudio-mic` only if
> you are wiring a custom microphone backend without the full facade.

## License

[MIT](LICENSE) © 2026 tsubome / Aratech.
