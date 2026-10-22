// flexaudio-napi real-audio end-to-end manual verification (requires real hardware / a PipeWire session).
//
// While smoke.mjs checks the marshaling path with MockBackend, this test
// verifies capture through napi from an actual audio device. Not for CI:
// requires real audio, a playback process, and a supported backend; run manually.
//
// Usage (set XDG_RUNTIME_DIR if your environment requires it):
//   # Start a process playing a sine wave with known amplitude (e.g. pipe one from pw-cat) and note its PID
//   TARGET_PID=<pid> KIND=process node realtest.mjs   # Capture a specific process output
//   KIND=system            node realtest.mjs           # System loopback
//   KIND=mic               node realtest.mjs           # Microphone
//
// Expected: chunks>0; firstLen === firstFrames*channels; maxPeak matches the known source amplitude
// (e.g. 0.3); clean shutdown (no hangs or zombies).
import { createRequire } from 'module';
const require = createRequire(import.meta.url);
const flex = require('./flexaudio.node');

const pid = parseInt(process.env.TARGET_PID, 10);
const kind = process.env.KIND || 'process';
if (kind === 'process' && !pid) {
  console.error('TARGET_PID=<pid> is required for process capture');
  process.exit(2);
}

let count = 0, maxPeak = 0, firstFrames = 0, firstLen = 0;
const events = [];
const opts = { kind, outputRate: 48000, outputChannels: 2 };
if (kind === 'process') opts.processId = pid;

console.log('openStream', JSON.stringify(opts));
const stream = flex.openStream(
  opts,
  (chunk) => {
    count++;
    if (chunk.peak > maxPeak) maxPeak = chunk.peak;
    if (count === 1) { firstFrames = chunk.frames; firstLen = chunk.data.length; }
  },
  (ev) => { events.push(ev.type); },
);

setTimeout(() => {
  stream.stop();
  console.log(JSON.stringify({
    chunks: count,
    maxPeak: Math.round(maxPeak * 1000) / 1000,
    firstFrames, firstLen,
    events: [...new Set(events)],
  }));
  process.exit(0);
}, 6000);
