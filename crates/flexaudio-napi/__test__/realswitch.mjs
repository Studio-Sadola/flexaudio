// flexaudio-napi seamless source switching: manual end-to-end verification with real audio
// (real hardware / PipeWire required).
//
// Calls switchSource on a single stream opened with openStream and checks that hot-swapping
// the source mic → system → process still captures as one continuous callback stream. Not
// runnable in CI (needs real audio and a playing process) = for manual runs.
//
// Usage:
//   # Prepare a process playing a sine of known amplitude and note its PID (e.g. pipe a sine into pw-cat)
//   TARGET_PID=<pid> node realswitch.mjs
//
// Expected: at t=0,1s (mic section) the peak is higher from the microphone input; at t>=2s
// (system→process section) it matches the amplitude of the playing sine (e.g. 0.3). Switching
// is transparent within the single stream.
import { createRequire } from 'module';
const require = createRequire(import.meta.url);
const flex = require('./flexaudio.node');

const pid = parseInt(process.env.TARGET_PID, 10);
const t0 = Date.now();
const buckets = {};
const rec = (p) => {
  const s = Math.floor((Date.now() - t0) / 1000);
  buckets[s] = Math.max(buckets[s] || 0, p);
};

const stream = flex.openStream(
  { kind: 'mic', outputRate: 48000, outputChannels: 2 },
  (c) => rec(c.peak),
  null,
);
console.log('start mic');
setTimeout(() => { stream.switchSource({ kind: 'system', outputRate: 48000, outputChannels: 2 }); console.log('switch -> system'); }, 2000);
setTimeout(() => { stream.switchSource({ kind: 'process', processId: pid, outputRate: 48000, outputChannels: 2 }); console.log('switch -> process'); }, 4000);
setTimeout(() => {
  stream.stop();
  console.log(JSON.stringify(Object.entries(buckets).map(([s, p]) => `t=${s}s maxPeak=${p.toFixed(3)}`)));
  process.exit(0);
}, 6500);
