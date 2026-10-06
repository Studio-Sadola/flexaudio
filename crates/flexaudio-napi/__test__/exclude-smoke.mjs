// Self-exclusion smoke test against a REAL PipeWire session (not CI-mock).
//
// Two children play tones into a null sink: A = 1 kHz via libpulse (paplay,
// the path Electron/Chromium uses), B = 3 kHz left / 4.5 kHz right via native
// PipeWire (pw-play).
// The addon must (1) list A under A's real pid, (2) capture B but not A when
// A's pid is excluded from a `system` capture, (3) capture both when nothing
// is excluded, and (4) capture the surviving tone at (near) the level the
// control reads, per channel. Distinct stereo tones detect missing, swapped,
// or duplicated source channels that a mono level check cannot distinguish.
// The control capture runs FIRST so (4) has its reference.
// Both (2) and (3) name the test sink via `deviceId` so neither
// depends on this sink being the PipeWire default. `excludePids` is now
// implemented, and a non-empty exclusion set takes the Linux fan-in path,
// which ignores `deviceId` — so a green (2) is genuine pid exclusion, not an
// artifact of sink scoping, and its failure mode is the real leak (1 kHz
// still present). `deviceId` still scopes (3), the control capture.
// Requires: pw-cli, pw-play, paplay on PATH; XDG_RUNTIME_DIR set.
//
// Detector: Goertzel at exact FFT bins. The bin index is round(N*f/rate) —
// the +0.5 variant lands one bin off and reads a pure tone as silence.
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';
import { spawn, execFileSync } from 'node:child_process';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { writeToneWav } from './tone-wav.mjs';

const require = createRequire(import.meta.url);
const here = dirname(fileURLToPath(import.meta.url));
const flex = require(join(here, 'flexaudio.node'));

const RATE = 48000;
const CAPTURE_SECONDS = 4;
const PRESENT_MIN = 0.05;   // amplitude of a tone that is being captured
const ABSENT_MAX = 0.005;   // amplitude of a tone that must not leak

function fail(msg) { console.error(`SMOKE FAILED: ${msg}`); process.exitCode = 1; }

function goertzel(samples, freq) {
  const n = samples.length;
  const k = Math.round((n * freq) / RATE);
  const w = (2 * Math.PI * k) / n;
  const c = 2 * Math.cos(w);
  let s1 = 0, s2 = 0;
  for (let i = 0; i < n; i++) { const s0 = samples[i] + c * s1 - s2; s2 = s1; s1 = s0; }
  return Math.sqrt(s1 * s1 + s2 * s2 - c * s1 * s2) / (n / 2);
}

async function capture(opts) {
  const left = [], right = [];
  const events = [];
  let invalidChunk = false;
  const stream = flex.openStream(
    { ...opts, outputRate: RATE, outputChannels: 2, chunkMs: 20 },
    (chunk) => {
      if (chunk.data.length !== chunk.frames * 2) { invalidChunk = true; return; }
      for (let i = 0; i < chunk.data.length; i += 2) {
        left.push(chunk.data[i]); right.push(chunk.data[i + 1]);
      }
    },
    (ev) => events.push(ev.type),
  );
  await new Promise((r) => setTimeout(r, CAPTURE_SECONDS * 1000));
  await stream.stop();
  if (invalidChunk) throw new Error('capture: expected interleaved stereo data (frames * 2 samples)');
  const measure = (samples) => {
    const s = Float32Array.from(samples.slice(RATE)); // drop the first second (link-up)
    return { amp1k: goertzel(s, 1000), amp3k: goertzel(s, 3000), amp4k5: goertzel(s, 4500) };
  };
  return { left: measure(left), right: measure(right), events, chunks: left.length / 960 };
}

function expect(label, r, want1k) {
  for (const [channel, tone, key, otherChannel] of [
    ['left', '3k', 'amp3k', 'right'],
    ['right', '4.5k', 'amp4k5', 'left'],
  ]) {
    const levels = r[channel];
    console.log(`[${label} ${channel}] amp1k=${levels.amp1k.toFixed(4)} amp3k=${levels.amp3k.toFixed(4)} amp4k5=${levels.amp4k5.toFixed(4)} chunks=${r.chunks} events=${[...new Set(r.events)]}`);
    if (!(want1k ? levels.amp1k >= PRESENT_MIN : levels.amp1k <= ABSENT_MAX)) {
      fail(`${label}: ${channel} channel expected 1k ${want1k ? 'present' : 'absent'}`);
    }
    if (!(levels[key] >= PRESENT_MIN)) fail(`${label}: ${channel} channel missing its ${tone} tone`);
    if (!(r[otherChannel][key] < levels[key] * 0.1)) {
      fail(`${label}: ${otherChannel} channel contains ${tone} from ${channel} (expected <10% of ${channel} level) — duplicated or misrouted channel`);
    }
  }
}

const sinkName = process.env.FLEX_SMOKE_SINK || `flexaudio-smoke-${process.pid}`;
const ownSink = !process.env.FLEX_SMOKE_SINK;
let a, b, dir;
let spawnError;

try {
  if (ownSink) {
    execFileSync('pw-cli', ['create-node', 'adapter', `{ factory.name=support.null-audio-sink node.name=${sinkName} media.class=Audio/Sink object.linger=true audio.position=[FL FR] }`], { stdio: 'ignore' });
  }
  dir = mkdtempSync(join(tmpdir(), 'flexaudio-smoke-'));
  writeToneWav(join(dir, '1k.wav'), { freqHz: 1000, seconds: 30 });
  writeToneWav(join(dir, 'stereo.wav'), { leftHz: 3000, rightHz: 4500, seconds: 30 });

  a = spawn('paplay', ['--volume=65536', join(dir, '1k.wav')], { env: { ...process.env, PULSE_SINK: sinkName }, stdio: 'ignore' });
  a.on('error', (err) => { spawnError = new Error(`paplay spawn failed: ${err.message}`); });
  b = spawn('pw-play', ['--target', sinkName, '--volume', '1.0', join(dir, 'stereo.wav')], { stdio: 'ignore' });
  b.on('error', (err) => { spawnError = new Error(`pw-play spawn failed: ${err.message}`); });
  await new Promise((r) => setTimeout(r, 2000));
  if (spawnError) throw spawnError;
  console.log(`children: paplay(1k, libpulse) pid=${a.pid}  pw-play(3k L/4.5k R, native) pid=${b.pid}  sink=${sinkName}`);

  // (1) processes() lists the libpulse child under its real pid.
  const list = await flex.processes();
  const rowA = list.find((p) => p.pid === a.pid);
  console.log('[processes]', JSON.stringify(list.map((p) => ({ pid: p.pid, name: p.name, executable: p.executable }))));
  if (!rowA) fail(`processes() has no entry with pid ${a.pid} (paplay); libpulse clients resolve to pipewire-pulse's pid`);
  else if (!['paplay', 'pacat'].includes(rowA.executable)) fail(`pid ${a.pid} listed with executable ${rowA.executable}`);

  // (3) control: nothing excluded, both present. Captured FIRST so (4) has its
  // reference level for the surviving tone.
  const ctl = await capture({ kind: 'system', deviceId: sinkName });
  // (2) exclusion by the libpulse child's real pid. `deviceId` is ignored on
  // the fan-in path a non-empty exclusion set takes (see header); it is kept
  // here so that a failure is the real leak and not this sink losing scope.
  const excl = await capture({ kind: 'system', deviceId: sinkName, excludePids: [a.pid] });
  expect('exclude-A', excl, false);
  expect('control', ctl, true);

  // (4) Each surviving channel must match its control level; missing links
  // reduce it, while double-linking the same port increases it.
  for (const [channel, key] of [['left', 'amp3k'], ['right', 'amp4k5']]) {
    const ratio = excl[channel][key] / ctl[channel][key];
    console.log(`[fan-in level ${channel}] exclude/control = ${ratio.toFixed(3)}`);
    if (!Number.isFinite(ratio)) fail(`fan-in ${channel} channel has no valid control level`);
    else if (ratio < 0.9) fail(`fan-in ${channel} channel captured its tone at ${(ratio * 100).toFixed(0)}% of the control — a channel is missing`);
    else if (ratio > 1.15) fail(`fan-in ${channel} channel captured its tone at ${(ratio * 100).toFixed(0)}% of the control — the source is double-linked`);
  }
} catch (err) {
  fail(err instanceof Error ? err.message : String(err));
} finally {
  if (a) a.kill();
  if (b) b.kill();
  if (dir) rmSync(dir, { recursive: true, force: true });
  if (ownSink && !process.env.FLEX_SMOKE_KEEP_SINK) {
    try { execFileSync('pw-cli', ['destroy', sinkName], { stdio: 'ignore' }); } catch {}
  }
}
if (process.exitCode) process.exit(process.exitCode);
console.log('SMOKE OK');
