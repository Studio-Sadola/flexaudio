// Self-exclusion smoke test against a REAL PipeWire session (not CI-mock).
//
// Two children play tones into a null sink: A = 1 kHz via libpulse (paplay,
// the path Electron/Chromium uses), B = 3 kHz via native PipeWire (pw-play).
// The addon must (1) list A under A's real pid, (2) capture B but not A when
// A's pid is excluded from a `system` capture, (3) capture both when nothing
// is excluded, reading the test sink's own monitor (deviceId) so the check
// doesn't depend on this sink being the PipeWire default. Requires: pw-cli,
// pw-play, paplay on PATH; XDG_RUNTIME_DIR set.
//
// Detector: Goertzel at exact FFT bins. The bin index is round(N*f/rate) —
// the +0.5 variant lands one bin off and reads a pure tone as silence.
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';
import { spawn, execSync } from 'node:child_process';
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
  const mono = [];
  const events = [];
  const stream = flex.openStream(
    { outputRate: RATE, outputChannels: 1, chunkMs: 20, ...opts },
    (chunk) => { for (let i = 0; i < chunk.data.length; i++) mono.push(chunk.data[i]); },
    (ev) => events.push(ev.type),
  );
  await new Promise((r) => setTimeout(r, CAPTURE_SECONDS * 1000));
  await stream.stop();
  const s = Float32Array.from(mono.slice(RATE)); // drop the first second (link-up)
  return { amp1k: goertzel(s, 1000), amp3k: goertzel(s, 3000), events, chunks: mono.length / 960 };
}

function expect(label, r, want1k, want3k) {
  const ok1 = want1k ? r.amp1k >= PRESENT_MIN : r.amp1k <= ABSENT_MAX;
  const ok3 = want3k ? r.amp3k >= PRESENT_MIN : r.amp3k <= ABSENT_MAX;
  console.log(`[${label}] amp1k=${r.amp1k.toFixed(4)} amp3k=${r.amp3k.toFixed(4)} chunks=${r.chunks} events=${[...new Set(r.events)]}`);
  if (!ok1 || !ok3) fail(`${label}: expected 1k ${want1k ? 'present' : 'absent'}, 3k ${want3k ? 'present' : 'absent'}`);
}

const sinkName = process.env.FLEX_SMOKE_SINK || `flexaudio-smoke-${process.pid}`;
const ownSink = !process.env.FLEX_SMOKE_SINK;
if (ownSink) {
  execSync(`pw-cli create-node adapter '{ factory.name=support.null-audio-sink node.name=${sinkName} media.class=Audio/Sink object.linger=true audio.position=[FL FR] }'`, { stdio: 'ignore' });
}
const dir = mkdtempSync(join(tmpdir(), 'flexaudio-smoke-'));
writeToneWav(join(dir, '1k.wav'), { freqHz: 1000, seconds: 30 });
writeToneWav(join(dir, '3k.wav'), { freqHz: 3000, seconds: 30 });

const a = spawn('paplay', ['--volume=65536', join(dir, '1k.wav')], { env: { ...process.env, PULSE_SINK: sinkName }, stdio: 'ignore' });
const b = spawn('pw-play', ['--target', sinkName, '--volume', '1.0', join(dir, '3k.wav')], { stdio: 'ignore' });
await new Promise((r) => setTimeout(r, 2000));
console.log(`children: paplay(1k, libpulse) pid=${a.pid}  pw-play(3k, native) pid=${b.pid}  sink=${sinkName}`);

try {
  // (1) processes() lists the libpulse child under its real pid.
  const list = await flex.processes();
  const rowA = list.find((p) => p.pid === a.pid);
  console.log('[processes]', JSON.stringify(list.map((p) => ({ pid: p.pid, name: p.name, executable: p.executable }))));
  if (!rowA) fail(`processes() has no entry with pid ${a.pid} (paplay); libpulse clients resolve to pipewire-pulse's pid`);
  else if (!['paplay', 'pacat'].includes(rowA.executable)) fail(`pid ${a.pid} listed with executable ${rowA.executable}`);

  // (2) exclusion by the libpulse child's real pid.
  expect('exclude-A', await capture({ kind: 'system', excludePids: [a.pid] }), false, true);
  // (3) control: nothing excluded, both present.
  expect('control', await capture({ kind: 'system', deviceId: sinkName }), true, true);
} finally {
  a.kill(); b.kill();
  rmSync(dir, { recursive: true, force: true });
  if (ownSink && !process.env.FLEX_SMOKE_KEEP_SINK) {
    try { execSync(`pw-cli destroy ${sinkName}`, { stdio: 'ignore' }); } catch {}
  }
}
if (process.exitCode) process.exit(process.exitCode);
console.log('SMOKE OK');
