# @studio-sadola/flexaudio

Native **N-API** bindings that let Node.js / TypeScript / Electron capture audio
through the [flexaudio](https://github.com/Studio-Sadola/flexaudio) Rust library:
microphone, system output (loopback), per-process capture, and mic + system mix on **Linux**,
**Windows**, and **macOS**.

Three offline audio add-ons are compiled into the same binary and exposed to
JavaScript: **voice activity detection** (Silero VAD on pure-Rust tract),
**noise suppression** (RNNoise via nnnoiseless), and streaming **FLAC** encoding. They run fully
offline — no model files to ship, no network at runtime.

> This package is published on npm as `@studio-sadola/flexaudio`.
> It is the **npm** package for flexaudio (the Rust crate `flexaudio-napi`).
> It is **not** published to crates.io; consume the core library from Rust via
> the `flexaudio` crate instead.

## Install

```sh
npm install @studio-sadola/flexaudio@0.5
```

The correct prebuilt native binary for your platform is pulled in automatically
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
`watchDevices(cb)` reports hotplug (added / removed / defaultChanged) events
on Linux via PipeWire. On Windows/macOS, it uses a no-op watcher and emits no
device-change events.

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

`processes()` lists audio output session/stream owners on Linux/Windows. On
macOS, it lists all processes known to Core Audio, including input-only
processes. Idle/stopped processes are included; use `isOutputActive` to check
current playback. Pass `pid` as `processId` for per-process capture:

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
never by `seq`: each tap counts its own sequence. Relative timing depends on
the output formats and buffering; there is no fixed delay between taps.

`stream` also exposes `pause()` / `resume()`, `setGain(x)`, and the read-only
`isPaused()`, `gain()`, `nativeFormat()` (`{ sampleRate, channels }`) and
`droppedChunks()` (a `bigint` running total). After `stop()`, the stream is spent;
call `openStream` to capture again.

Use `bigint` arithmetic for u64 counters: primary/secondary `seq`,
`droppedChunks()`, `chunkDropped.count`, loss `samples` (when known),
`rescanRequired.droppedEvents`, and VAD `atSample`. For example, increment a
sequence with `chunk.seq + 1n`. `ptsNs` and VAD `atNs` remain `number` timestamps;
`frames`, `flags`, and the u32 `droppedBefore` field also remain `number`.

## Voice activity detection, noise suppression, FLAC

The add-ons work standalone on any `Float32Array` of interleaved samples, and the
VAD / noise suppression can also be wired into a live `openStream`.

```js
const { Vad, Denoiser, FlacEncoder, openStream } = require('@studio-sadola/flexaudio');

// VAD: feed any format; it resamples internally to the VAD rate (16 kHz).
const vad = new Vad({ threshold: 0.5, minSilenceMs: 100 });
for (const ev of vad.process(samples, 48000, 2)) {
  // ev.type: 'speechStart' | 'speechEnd'
  // ev.atSample is a bigint on the VAD's internal 16 kHz sample clock.
  const wholeSeconds = ev.atSample / 16000n; // exact bigint quotient
  const seconds = Number(ev.atSample) / 16000; // approximate number for display
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

## Whisper-compatible standalone VAD

`WhisperVad` uses the embedded Silero v6 model and the segmentation rules from
whisper.cpp pin `85a69493a601d4ff5a834064f7b7bac250bd8739`. Feed normalized
mono `Float32Array` PCM at 16000 Hz. Run its synchronous inference in a Node
Worker when used during recording.
The embedded model SHA-256 is
`7776b81ad1b0350c15d7f1555943b9232eb53e9ca5d989c6d0cea9ebc8664d87`;
using v6 does not promise the same probabilities as whisper.cpp's v5 model.

```js
const { WhisperVad } = require('@studio-sadola/flexaudio');
const vad = new WhisperVad({ provisional: true });
for (const mono16k of decodedChunks) consume(vad.process(mono16k));
consume(vad.finish()); // True EOF; infer one zero-padded partial frame if needed.
```

The immutable parameters are `threshold` (0.5), `minSpeechDurationMs` (250),
`minSilenceDurationMs` (100), `maxSpeechDurationS` (finite f32::MAX), and
`speechPadMs` (30). Integer duration fields accept 0..134217; zero is literal.
Unknown keys, invalid types, out-of-range PCM and nonfinite values throw errors
with stable `code` values. Drain the exception's `terminalEvents` through the
same consumer before handling a fatal failure; validation errors carry `[]`.

`segment` contains epoch-relative integer `startMs`/`endMs` on the 10 ms grid.
Final endpoints can exceed physical EOF, and finalization has no latency bound:
the fixed 200 ms merge can undo maximum-duration splits. Optional provisional
speech hints and nonoverlapping `provisionalCut` pieces of at most 30000 ms use
separate event types. Every published hint closes before `epochEnd`.
`epoch` and `seq` identify ordered events. Standalone events have no capture origin.

`finish()` closes the epoch once; repeated finish returns `[]`. `reset()` closes
published hints, abandons pending final segments and starts the next epoch.
`lastFrameProbabilities()` returns an owned `{ firstFrameIndex, values }` copy
for the latest successful call, including an EOF tail. Processing an empty
array clears that batch without advancing the frame timeline. One model frame
consumes 512 samples (32 ms); that stride differs from the final 10 ms grid.

`openStream` accepts `whisperVad: { params: { ... }, provisional: true, tap }`.
Legacy `vad`/`vadTap` are mutually exclusive with attachment; secondary requires
an enabled secondary output. One VAD owner consumes the producer's shared 48 kHz
stereo branch before output conversion. The selected chunk carries
`whisperVadEvents`, beginning each epoch with its exact `captureSample` bigint and
`ptsNs` origin. Every audio chunk exposes `frameIndex: bigint` in 48 kHz units;
output taps keep independent producer timelines across native restarts and gaps.
`flushWhisperVad()` delivers an empty closing carrier before its promise resolves;
`stop()` drains capture and closes its last epoch before settlement.

Standalone tests do not open audio devices. After building/copying the addon as
in `__test__/run-smoke.sh`, run
`node __test__/whisper-vad.test.mjs` from this crate. The script uses `node:test`
and reports each of its twelve cases.

## Building the loader (`index.js` / `index.d.ts`)

The JavaScript loader (`index.js`) and TypeScript declarations (`index.d.ts`)
follow the **napi-rs** convention. The napi CLI generates exports from the
`#[napi]` definitions in `src/`:

```sh
npm install
npx napi build --platform --release   # also produces the .node binary
```

`napi build` writes `index.js`, `index.d.ts`, and the platform `.node` artifact.
`index.js` and `index.d.ts` are committed; the `.node` binaries are git-ignored.
The declarations also maintain the strict whisper event unions and parameter
types used by the custom marshaller. Preserve those declarations and review the
generated diff when regenerating exports.

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
