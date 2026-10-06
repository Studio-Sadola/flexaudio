// Manual real-audio end-to-end check for seamless source switching in flexaudio-napi (requires real hardware and PipeWire).
//
// Call switchSource on a single stream opened with openStream. Verify that hot-swapping sources
// from mic → system → process still records through one continuous callback stream.
// This cannot run in CI (requires real audio and a playback process); run it manually.
//
// Usage:
//   # Start a process that plays a sine wave at a known amplitude and note its PID (for example, pipe a sine wave to pw-cat).
//   TARGET_PID=<pid> node realswitch.mjs
//
// Expected: at t=0–1s (mic interval), peak is higher from microphone input. At t>=2s
// (system → process interval), it matches the playing sine amplitude (for example, 0.3). Switching is seamless within the single stream.
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
