# @aratech/flexaudio

Native **N-API** bindings that let Node.js / TypeScript / Electron capture audio
through the [flexaudio](https://github.com/tsubome/flexaudio) Rust library:
microphone, system output (loopback), and per-process capture on **Linux**,
**Windows**, and **macOS**.

> This is the **npm** package for flexaudio (the Rust crate `flexaudio-napi`).
> It is **not** published to crates.io; consume the core library from Rust via
> the `flexaudio` crate instead.

## Install

```sh
npm install @aratech/flexaudio
```

The correct prebuilt native binary for your platform is pulled in automatically
via the platform-specific `optionalDependencies` (`@aratech/flexaudio-<triple>`).

## Usage

```js
const { devices, openStream } = require('@aratech/flexaudio');

console.log(devices());

const stream = openStream(
  { kind: 'mic', outputRate: 48000, outputChannels: 2, chunkMs: 20 },
  (chunk) => {
    // chunk.data: Float32Array (interleaved), chunk.frames, chunk.peak, chunk.rms,
    // chunk.seq: BigInt, chunk.flags, chunk.droppedBefore
  },
  (event) => {
    // event.type: 'chunkDropped' | 'stalled' | 'recovered' | 'permissionDenied'
    //           | 'deviceLost' | 'error'
  },
);

// later:
stream.stop();
```

`stream.switchSource(options)` hot-swaps the input source without stopping.
`watchDevices(cb)` reports hotplug (added / removed / defaultChanged) events.

## Building the loader (`index.js` / `index.d.ts`)

The JavaScript loader (`index.js`) and TypeScript declarations (`index.d.ts`)
follow the **napi-rs** convention and are **generated** by the napi CLI from the
`#[napi]` exports in `src/lib.rs`:

```sh
npm install
npx napi build --platform --release   # also produces the .node binary
```

`napi build` writes `index.js`, `index.d.ts`, and the platform `.node` artifact.
These generated files are git-ignored and produced at build/publish time
(`prepublishOnly` runs `napi prepublish`). Do not hand-edit them.

## Permissions

Audio capture requires OS-level consent: macOS TCC
(`kTCCServiceAudioCapture`; add `NSAudioCaptureUsageDescription` to your app's
`Info.plist`), the Windows microphone privacy setting, and a running PipeWire
session on Linux for system / per-process capture. See the
[workspace README](https://github.com/tsubome/flexaudio#os-specific-permission-requirements).

On macOS, system / per-process loopback (Core Audio process taps) requires
macOS 14.4 or later.

## License

[MIT](LICENSE) © 2026 tsubome / Aratech. This package redistributes native code
(including statically linked ONNX Runtime where the VAD add-on is built in); see
[`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md).
