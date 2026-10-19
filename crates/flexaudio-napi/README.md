# @studio-sadola/flexaudio

Native **N-API** bindings that let Node.js / TypeScript / Electron capture audio
through the [flexaudio](https://github.com/Studio-Sadola/flexaudio/blob/v0.8.1/README.md) Rust library:
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
npm install @studio-sadola/flexaudio@0.8
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
It applies only the stream configuration: `options.denoise` and `options.vad`
are ignored, and the existing denoise and VAD settings remain in effect.
`options.gain` is also ignored; use `setGain()` to change gain.
`watchDevices(cb)` reports Linux PipeWire system output additions, removals and
default changes using the same output IDs as `devices()`. Microphone and
default-microphone changes are not reported: CPAL microphone IDs cannot be resolved
from PipeWire nodes. Windows/macOS use a no-op watcher.

## Capability discovery

`await capabilities()` returns a read-only platform support snapshot. Its bounded
prerequisite probe runs on the libuv thread pool. It does not enumerate microphones,
request recording permission, open streams, or play audio. Supported means platform
prerequisites are met; it does not promise permission or a usable device.

```js
const { capabilities } = require('@studio-sadola/flexaudio');
const support = await capabilities();
console.log(support.sources.system.backend, support.sources.system.availability);
console.log(support.systemExclusions, support.process, support.deviceWatcher);
console.log(support.sideEffects.classicLoopbackKeepalive);
```

The snapshot contains `osVersion`, the four `sources` (`mic`, `system`, `process`,
`mix`), `systemExclusions`, process PID scopes, watcher mode/scopes, and
`sideEffects.silenceProbe` / `sideEffects.classicLoopbackKeepalive`. Fields use
camelCase. Missing OS versions/components and an unlimited `maxDistinctPids` are
`null`. Availability is a discriminated union: `{ status: 'supported' }`,
`{ status: 'unavailable', reason }`, or `{ status: 'unknown', error }`.
Probe failures retain the existing structured `AudioError` in `error`, including
contexts and related errors. Unknown future enum values use `other`; an unknown
unavailable reason also carries a `code` string. See the exported `Capabilities`
type for every field and tag.

On Linux, a missing PipeWire socket means unavailable. Failed socket inspection,
connection or server sync means unknown, with the structured query error retained.

## Windows classic loopback keepalive

`openStream({ kind: 'system', windowsClassicLoopbackKeepalive: false }, onChunk)`
disables silent rendering used to keep classic loopback delivering idle packets.
Omitting the option on open defaults to `true`. It also applies to the system lane
of `mix`, without PID exclusions. Other platforms and process/exclusion capture
paths ignore it.

`stream.switchSource({ kind: 'system', windowsClassicLoopbackKeepalive: true })`
updates the option with the source only after a successful switch. Omitting it
from `switchSource` keeps the current value. When disabled, an idle endpoint sends
no packets; the existing 2 s watchdog reports `stalled` and reopens repeatedly until
audio plays.

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
capture starts. An unresolved live process, including a helper that has not yet
rendered audio, fails startup. Only a non-host process confirmed gone is omitted;
unresolved host exclusion always fails startup. This is a start-time snapshot. Open the capture while the
app is already playing, or reopen it when a new helper appears.

macOS honors `deviceId` (or `systemDeviceId` for mix) together with exclusion.
Linux and Windows do not use the requested system device while exclusion is
active: Linux fan-in is not device-scoped, and WASAPI process loopback cannot
target an output endpoint. Microphone selection is unaffected.

## Settings reports, strict mode, and lifecycle

`openStream({ kind: 'mic', strict: true, ringCapacityChunks: 100 }, onChunk)`
opts into applicability checks and a 100-chunk output ring (default 50).
`stream.effectiveSettings()` returns the last successful open/switch snapshot,
with a `bigint` revision and requested/effective values and outcomes for every
recognized capture setting. Effective values describe configuration, not proof
of running hardware. Omitted `strict` retains strictness on source switches.

Only decoded values differing from documented defaults count as requests;
property presence makes no difference. Explicit defaults are `notRequested`.
Strict rejection throws an error with `code === 'FLEX_IGNORED_SETTINGS'` and a
complete candidate `.report`, before device access, model loading, or stopping
the old source. Failed switches keep the previous report and controls.
Gain, ring capacity, denoise and VAD cannot be replaced by `switchSource`:
non-default requests are `ignored` with `fixedAtOpen` (strict rejects them).
Use `setGain()` for runtime gain changes. See the root README's
[ignored-setting table](https://github.com/Studio-Sadola/flexaudio/blob/v0.8.1/README.md#strict-mode-and-effective-settings)
for all reasons and the default-value rule.

Register `onLifecycleEvent` inside `OpenOptions` for a separate ordered helper
callback channel. Ordinary `onEvent` never receives these events, even as unknown.
Omitting the handler on switch retains it; supplying one replaces it only after
successful commit. `stop()` settles after queued lifecycle callbacks complete.
The handler must not synchronously wait for `stop()` to settle (for example,
with a blocking loop or `Atomics.wait`): settlement requires the handler to
return so the JS thread can deliver the remaining callbacks. To request shutdown
inside the handler, use `void stream.stop().catch(handleError)` and return;
await completion outside the handler. Returned promises are not awaited by the
binding. `switchSource()` throws `stream already stopped` as soon as `stop()`
is requested, including while its promise is pending.
Every emitted helper Started has exactly one Stopped with a reason. On macOS,
Started means a diagnostic attempt; on Windows it means successful silent-render
start. Stopped ends ownership after teardown attempts; cleanup errors remain
authoritative. Ordering against PCM and ordinary events is not guaranteed.
See [helper behavior and controls](https://github.com/Studio-Sadola/flexaudio/blob/v0.8.1/README.md#optional-playback-helpers-and-lifecycle).

Library-created primary and secondary chunks always expose `arrivalNs`, identical
to `ptsNs`. The declaration is optional to preserve consumer-created objects.
Old chunk `ptsNs` naming is documentation-only deprecated since 0.8 and retained
throughout 0.9; `frameIndex` remains authoritative for audio continuity.
`await devicesWithNamespaces()` returns `{ device, idNamespace }` entries with
unchanged selectable IDs. Namespaces describe provider selector domains, not
persistent hardware identities.

## Chunk delivery shape (primary, secondary, VAD)

`onChunk` is called with **one** argument, the primary chunk. When
`secondaryOutput` is set, the paired secondary chunk travels **inside** it as
`chunk.secondary` — it is not a second callback argument. VAD events ride on the
chunk of the tap selected by `vad.tap`. Do not block synchronously inside
`onChunk` (defer heavy work); blocking stalls terminator delivery and delays
`stop()` resolving.

One possible use is saving the primary PCM while analyzing a smaller secondary format:

```js
const stream = openStream(
  {
    kind: 'system',
    outputRate: 48000, outputChannels: 2,
    secondaryOutput: { rate: 16000, channels: 1, encoding: 's16' },
    vad: { params: { threshold: 0.5 }, tap: 'primary' },
  },
  (chunk) => {
    save(chunk.data);
    if (chunk.secondary) analyze(chunk.secondary.data);
    for (const event of chunk.vadEvents ?? []) {
      // Segment startMs/endMs are relative to the epoch origin.
      consume(event);
    }
  },
);
```


Compare primary and secondary arrival labels with `arrivalNs`, never `seq`:
each tap counts its own sequence. Arrival labels can overlap or repeat and are
not an exact media clock or an exact pairing key. Delay depends on the secondary
format and buffering; a 48 kHz stereo tap bypasses secondary conversion and has
no conversion delay. Node expands the primary chunk's 20 ms interval by 30 ms on
each side (60 ms pairing tolerance). There is no fixed delay between taps.

`stream` also exposes `pause()` / `resume()`, `setGain(x)`, and the read-only
`isPaused()`, `gain()`, `nativeFormat()` (`{ sampleRate, channels }`) and
`droppedChunks()` (a `bigint` running total). After `stop()`, the stream is spent;
call `openStream` to capture again.

Use `bigint` arithmetic for u64 counters: primary/secondary `seq`,
`droppedChunks()`, `chunkDropped.count`, loss `samples` (when known),
`rescanRequired.droppedEvents`, and VAD `captureSample`. For example, increment a
sequence with `chunk.seq + 1n`. `ptsNs` and VAD milliseconds remain `number` timestamps;
`frames`, `flags`, and the u32 `droppedBefore` field also remain `number`.

## Voice activity detection, noise suppression, FLAC

The add-ons work standalone on any `Float32Array` of interleaved samples, and the
VAD / noise suppression can also be wired into a live `openStream`.

```js
const { Vad, Denoiser, FlacEncoder, openStream } = require('@studio-sadola/flexaudio');

// VAD: feed 8000–192000 Hz PCM; it resamples internally to the VAD rate (16 kHz).
const vad = new Vad({ threshold: 0.5, minSilenceMs: 100 });
for (const ev of [...vad.process(samples, 48000, 2), ...vad.finish()]) {
  if (ev.type === 'segment') console.log(ev.startMs, ev.endMs);
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
  { kind: 'mic', outputRate: 48000, denoise: true, vad: { params: { threshold: 0.5 }, tap: 'primary' } },
  (chunk) => {
    // chunk.data is already noise-suppressed; chunk.vadEvents holds VAD events
  },
);
```

`denoise` requires `outputRate: 48000` (RNNoise is 48 kHz only) — any other rate
makes `openStream` throw.

## Streaming VAD

`Vad` uses the embedded Silero v6 model and the segmentation rules from
whisper.cpp pin `85a69493a601d4ff5a834064f7b7bac250bd8739`. Feed normalized interleaved `Float32Array` PCM; the default input is 16000 Hz mono. Other input formats are converted internally with group-delay compensation so events stay on the input timeline. Finite resampling overshoot is clipped to [-1,1]; invalid source PCM and nonfinite converter output are rejected. Run its synchronous inference in a Node
Worker when used during recording.
The embedded model SHA-256 is
`7776b81ad1b0350c15d7f1555943b9232eb53e9ca5d989c6d0cea9ebc8664d87`;
using v6 does not promise the same probabilities as whisper.cpp's v5 model.

```js
const { Vad } = require('@studio-sadola/flexaudio');
const vad = new Vad({ provisional: true });
for (const mono16k of decodedChunks) consume(vad.process(mono16k));
consume(vad.finish()); // True EOF; infer one zero-padded partial frame if needed.
```

The immutable parameters are `threshold` (0.5), `minSpeechMs` (250),
`minSilenceMs` (100), `maxSpeechMs` (0, unbounded), and
`speechPadMs` (30). Minimum durations and padding accept integer 0..134217 ms; zero is literal. `maxSpeechMs` accepts unsigned integer milliseconds. Zero or values above 100000000 use the legacy `INT_MAX/2`-sample sentinel (1073741823 samples), which is effectively unbounded. Other positive values must be at least `32 + 2 * speechPadMs`; their split budget is exactly `maxSpeechMs * 16 - 512 - 2 * speechPadMs * 16` samples. Zero budgets are valid (for example, 1000 ms with 484 ms padding); negative budgets throw `InvalidParameter`.

In 0.5.x, fractional `maxSpeechDurationS` values were truncated to whole seconds.
To preserve identical segmentation, migrate with
`maxSpeechMs = trunc(seconds) * 1000` (for example, 30.9 seconds becomes 30000 ms).

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

`openStream` accepts `vad: { params: { ... }, provisional: true, tap }`.
A secondary event carrier requires an enabled secondary output. One VAD owner consumes the producer's shared 48 kHz
stereo branch before output conversion. The selected chunk carries
`vadEvents`, beginning each epoch with its exact `captureSample` bigint and
`ptsNs` origin. Every audio chunk exposes `frameIndex: bigint` in 48 kHz units;
output taps keep independent producer timelines across native restarts and gaps.
`flushVad()` delivers an empty closing carrier before its promise resolves;
`stop()` drains capture and closes its last epoch before settlement.

An attached VAD failure is a failure of the **VAD session only**. It is reported once
as `onEvent({ type: 'vadFailed', error, epoch })`, after the carrier whose
`vadEvents` end with `epochEnd(error)`; `error.vadCode` names the typed
cause. Capture, PCM delivery, `pause`/`resume`, `switchSource` and `stop` keep
working, and `terminalError()` stays undefined (it means capture primary only). The
shutdown report keeps the cause in `cleanupErrors` with its `vadCode` and leaves
`primary` for real capture failures. The next discontinuity (the flag, a capture
index gap, a PTS reanchor, or the first chunk after `resume`/`switchSource`) reinitializes converter
and model, so the following frame starts a new anchored epoch; a failed
reinitialization keeps the session failed without repeating the event, until
`flushVad()` succeeds. Missing capture audio never becomes padding, and a
capture index gap closes the intact prefix instead of failing it.

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
The declarations also maintain the strict attachment event unions and parameter
types used by the custom marshaller. Preserve those declarations and review the
generated diff when regenerating exports.

## Permissions

Audio capture requires OS-level consent: macOS TCC
(`kTCCServiceAudioCapture`; add `NSAudioCaptureUsageDescription` to your app's
`Info.plist`), the Windows microphone privacy setting, and a running PipeWire
session on Linux for system / per-process capture. See the
[workspace README](https://github.com/Studio-Sadola/flexaudio/blob/v0.8.1/README.md#os-specific-permission-requirements).

On macOS, system / per-process loopback (Core Audio process taps) requires
macOS 14.4 or later.

## License

[MIT](LICENSE) © 2026 tubome / Studio Sadola. This package redistributes native code
and bundled assets: the embedded Silero VAD model (built-in VAD add-on), the
pure-Rust tract inference crates, and RNNoise via nnnoiseless with embedded
weights (noise suppression).
See [`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md).

The macOS silence diagnostic probe is enabled by default for system/process capture
and the system lane of Mix. It runs once per generation after five continuous
seconds of bit-exact zero native samples with eligible external output activity,
honoring exclusions and device selection. Its low-level signal is not guaranteed
inaudible. Set `silenceProbe: false` in `openStream` to disable it. Other platforms ignore the option.
