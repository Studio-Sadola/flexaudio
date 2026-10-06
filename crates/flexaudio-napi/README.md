# @studio-sadola/flexaudio

Native **N-API** bindings that let Node.js / TypeScript / Electron capture audio
through the [flexaudio](https://github.com/Studio-Sadola/flexaudio) Rust library:
microphone, system output (loopback), per-process capture, and mic + system mix on **Linux**,
**Windows**, and **macOS**.

Three offline audio add-ons are compiled into the same binary and exposed to
JavaScript: **voice activity detection** (Silero VAD on pure-Rust tract),
**noise suppression** (RNNoise via nnnoiseless), and streaming **FLAC** encoding. They run fully
offline — no model files to ship, no network at runtime.

> This package is distributed through npm; first publication pending.
> It is the **npm** package for flexaudio (the Rust crate `flexaudio-napi`).
> It is **not** published to crates.io; consume the core library from Rust via
> the `flexaudio` crate instead.

## Install

```sh
npm install @studio-sadola/flexaudio
```

After the first publication, the correct prebuilt native binary for your platform is pulled in automatically
via the platform-specific `optionalDependencies` (`@studio-sadola/flexaudio-<triple>`).

## Usage

```js
const { devices, openStream } = require('@studio-sadola/flexaudio');

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
await stream.stop();
```

`stream.switchSource(options)` hot-swaps the input source without stopping.
`watchDevices(cb)` reports hotplug (added / removed / defaultChanged) events.

## Microphone + system mix

Use `kind: 'mix'` to combine microphone and system audio in one stream.
`micDeviceId` and `systemDeviceId` select IDs from `devices()`; omitting them
uses the default input/output. `deviceId` is ignored for mix. `micGain` and
`systemGain` are linear pre-mix multipliers (default `1.0`); `gain` applies
ommediately after mixing.

```js
const stream = openStream(
  { kind: 'mix', micGain: 1.0, systemGain: 0.5 },
  onChunk,
);
// later:
await stream.stop();
```

## Picking a process to capture

`processes()` lists the processes that have an audio output session/stream and
can be captured per process. Idle/stopped processes are included; whether
something is playing now is `isOutputActive`. Pass `pid` as `processId`:

```js
const { processes, openStream } = require('@studio-sadola/flexaudio');

const list = await processes();
// [{ pid: 4242, name: 'Firefox', executable: 'firefox', isOutputActive: true }, …]
// bundleId is set on macOS only; executable / isOutputActive are undefined when
// the OS does not expose them. The calling process is never listed.

const target = list[0];
if (target) {
  const stream = openStream({ kind: 'process', processId: target.pid }, (chunk) => {});
  // …
  await stream.stop();
}
```

An empty array means per-process capture works but no such process exists right
now (not "nothing is playing"). A rejection means per-process capture is
unavailable here (Linux without a reachable PipeWire session, macOS before 14.4,
an unsupported OS, or a permission denial), the OS did not answer within 3
seconds, or a previous enumeration is still in progress. On Windows, listing
and capturing both need Windows build 20348 or later (Windows 11 / Windows
Server 2022).

## Excluding your own app (Electron hosts)

`excludePids` and `excludeSelf` apply to system capture and the system side of
mix; mic/process sources ignore valid exclusions. `excludeSelf: true` excludes
the process the addon runs in. Electron renders
audio from an audio utility process. On Linux and macOS, pass the audio service
PIDs from `app.getAppMetrics()` as well. On Windows, use `excludeSelf: true`
alone: the audio utility process is a direct child of the main process and
is covered by its excluded process tree.

```js
const pids = process.platform === 'win32'
  ? []
  : app.getAppMetrics()
      .filter((m) => m.serviceName === 'audio.mojom.AudioService' || m.type === 'Utility')
      .map((m) => m.pid);
const stream = openStream({ kind: 'system', excludeSelf: true, excludePids: pids }, onChunk, onEvent);
```

`excludePids` entries must be finite positive integers in `1..=4294967295`.
Invalid values cause an `InvalidArg` error; duplicates are allowed. `processId`
has the same validation rules.

Linux matches exact PIDs without descendants; native clients use
`pipewire.sec.pid`. macOS excludes listed PIDs resolved to audio objects.
Windows can exclude only **one process tree root**: self with `excludeSelf`,
otherwise the first listed PID. Every entry must equal the root; distinct PIDs
fail with an error, even if they are descendants. For Electron, keep `excludePids` empty and use `excludeSelf: true`.
On macOS, each PID must also fit a positive signed 32-bit integer
(`1..=2147483647`) or capture fails. On Linux, while exclusion is active, a
stream relayed through `pipewire-pulse` is not captured until its
`application.process.id` is known.

On **macOS** each pid is resolved to a Core Audio process object once, when the
capture starts. A helper that has not yet rendered any audio has no such object
and is therefore not excluded: this is a start-time snapshot. A failed PID
lookup fails the open unless the process is gone. Open the capture while the
app is already playing, or reopen it when a new helper appears.

macOS honors `deviceId` (or `systemDeviceId` for mix) together with exclusion.
Linux and Windows do not use the requested system device while exclusion is
active: Linux fan-in is not device-scoped, and WASAPI process loopback cannot
target an output endpoint. Microphone selection is unaffected.

## Chunk delivery shape (primary, secondary, VAD)

`onChunk` is called with **one** argument, the primary chunk. When
`secondaryOutput` is set, the paired secondary chunk travels **inside** it as
`chunk.secondary` — it is not a second callback argument. VAD events ride on the
chunk of the tap selected by `vadTap`. Do not block synchronously inside
`onChunk` (defer heavy work); blocking stalls terminator delivery and delays
`stop()` resolving.

```js
const stream = openStream(
  {
    kind: 'system',
    outputRate: 48000, outputChannels: 2,                       // primary: save
    secondaryOutput: { rate: 16000, channels: 1, encoding: 's16' }, // secondary: recognize
    vad: { threshold: 0.5 },
    vadTap: 'secondary',
  },
  (chunk) => {                    // ONE argument
    save(chunk.data);             // Float32Array, 48 kHz stereo
    const sec = chunk.secondary;  // undefined on rounds where it has not arrived yet
    if (sec) {
      recognize(sec.data);        // Int16Array because encoding is 's16'
      for (const ev of sec.vadEvents ?? []) {
        // ev.type: 'speechStart' | 'speechEnd'; ev.atNs: time since recording start
      }
    }
    // With vadTap: 'primary' (the default) the events are on chunk.vadEvents instead.
  },
);
```

Pair primary and secondary chunks by `ptsNs` (a zero-based recording clock),
never by `seq`: each tap counts its own sequence, and the secondary tap runs
about 20–60 ms behind the primary.

`stream` also exposes `pause()` / `resume()`, `setGain(x)`, and the read-only
`isPaused()`, `gain()`, `nativeFormat()` (`{ sampleRate, channels }`) and
`droppedChunks()` (a `bigint` running total).

## Voice activity detection, noise suppression, FLAC

The add-ons work standalone on any `Float32Array` of interleaved samples, and the
VAD / noise suppression can also be wired into a live `openStream`.

```js
const { Vad, Denoiser, FlacEncoder, openStream } = require('@studio-sadola/flexaudio');

// VAD: feed any format; it resamples internally to the VAD rate (16 kHz).
const vad = new Vad({ threshold: 0.5, minSilenceMs: 100 });
for (const ev of vad.process(samples, 48000, 2)) {
  // ev.type: 'speechStart' | 'speechEnd'
  // ev.atSample is on the VAD's internal rate — seconds = ev.atSample / 16000
}

// Noise suppression: 48 kHz only, returns the denoised copy (mono here).
const dn = new Denoiser(1);
const clean = dn.process(samples);   // same length as input (one-frame delay)
const tail = dn.flush();             // final 480 samples/ch when you're done

// FLAC: streaming encode. splitSeconds > 0 rotates into meeting-001.flac, -002…
const flac = FlacEncoder.create('meeting.flac', 48000, 2, /* splitSeconds */ 600);
flac.writeChunk(samples);
flac.finalize();
```

Integrated into a recording, `denoise` rewrites the delivered/stored audio and
`vad` attaches its events to each chunk (as `chunk.vadEvents`), applied in the
order denoise → VAD:

```js
const stream = openStream(
  { kind: 'mic', outputRate: 48000, denoise: true, vad: { threshold: 0.5 } },
  (chunk) => {
    // chunk.data is already noise-suppressed; chunk.vadEvents holds VAD events
  },
);
```

`denoise` requires `outputRate: 48000` (RNNoise is 48 kHz only) — any other rate
makes `openStream` throw.

## Building the loader (`index.js` / `index.d.ts`)

The JavaScript loader (`index.js`) and TypeScript declarations (`index.d.ts`)
follow the **napi-rs** convention and are **generated** by the napi CLI from the
`#[napi]` exports in `src/lib.rs`:

```sh
npm install
npx napi build --platform --release   # also produces the .node binary
```

`napi build` writes `index.js`, `index.d.ts`, and the platform `.node` artifact.
`index.js` and `index.d.ts` are committed (regenerate them after changing the
`#[napi]` exports); the `.node` binaries are git-ignored. Do not hand-edit the
generated files — change the doc comments in `src/lib.rs` and rebuild.

## Permissions

Audio capture requires OS-level consent: macOS TCC
(`kTCCServiceAudioCapture`; add `NSAudioCaptureUsageDescription` to your app's
`Info.plist`), the Windows microphone privacy setting, and a running PipeWire
session on Linux for system / per-process capture. See the
[workspace README](https://github.com/Studio-Sadola/flexaudio#os-specific-permission-requirements).

On macOS, system / per-process loopback (Core Audio process taps) requires
macOS 14.4 or later.

## License

[MIT](LICENSE) © 2026 tubome / Studio Sadola. This package redistributes native code
and bundled assets: the embedded Silero VAD model (built-in VAD add-on), the
pure-Rust tract inference crates, and RNNoise via nnnoiseless with embedded
weights (noise suppression).
See [`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md).
