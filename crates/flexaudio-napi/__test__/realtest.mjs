// flexaudio-napi manual end-to-end verification test with real audio (real hardware /
// PipeWire session required).
//
// Whereas smoke.mjs verifies the marshaling path with MockBackend, this one checks that
// capture works from a real audio device through napi. Not runnable in CI
// (needs real audio, a playing process, and a supported backend) = for manual runs.
//
// Usage (examples; set XDG_RUNTIME_DIR in environments that need it):
//   # Prepare a process playing a sine of known amplitude (e.g. pipe a sine into pw-cat) and note its PID
//   TARGET_PID=<pid> KIND=process node realtest.mjs   # capture a specific process's output
//   KIND=system            node realtest.mjs           # system loopback
//   KIND=mic               node realtest.mjs           # microphone
//
// Expected: chunks>0, firstLen === firstFrames*channels, maxPeak matching the amplitude for a
// source of known amplitude (e.g. 0.3), clean exit (no hang/zombie).
import { createRequire } from 'module';
const require = createRequire(import.meta.url);
const flex = require('./flexaudio.node');

const pid = parseInt(process.env.TARGET_PID, 10);
const kind = process.env.KIND || 'process';
if (kind === 'process' && !pid) {
  console.error('process capture requires TARGET_PID=<pid>');
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
