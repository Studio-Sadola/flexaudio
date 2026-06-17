# flexaudio-os-linux

Linux audio capture backend for the `flexaudio` workspace, built on
[PipeWire](https://pipewire.org/) (`pipewire` 0.10).

Provides `PwSystemBackend`, which captures the default sink monitor (system output
loopback — the audio mix flowing to the speakers) via a PipeWire
`Stream/Input/Audio` stream with `stream.capture.sink=true`. This is the Linux
equivalent of WASAPI loopback on Windows. Per-process capture is also routed
through PipeWire.

Because PipeWire's `MainLoop` / `Context` / `Core` / `Stream` are `!Send`, the
entire PipeWire session runs on a dedicated thread; only `Send`-safe handles
(stop sender, `JoinHandle`) cross thread boundaries.

**Platform requirement:** a running PipeWire session is required for system and
per-process audio capture. The crate compiles as an empty stub on non-Linux
targets (`#![cfg(target_os = "linux")]`).

> **Most users should use the [`flexaudio`](https://crates.io/crates/flexaudio)
> facade crate instead of this one directly.** Depend on `flexaudio-os-linux`
> only if you are building a custom Linux audio pipeline that needs direct access
> to `PwSystemBackend`.

## License

[MIT](LICENSE) © 2026 tsubome / Aratech.
