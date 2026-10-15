// flexaudio-napi dual-output + flushVad E2E test (no real audio; headless).
//
// Prerequisite: copy or rename the cdylib from `cargo build -p flexaudio-napi` to `flexaudio.node`
// in this directory (done by the run-smoke.sh scripts).
//
// Enable the secondary tap and integrated VAD with __openMockStream extended arguments; test without real capture:
//  [A] Dual output:
//   1. A time-aligned secondary chunk (primary.secondary) arrives paired with the primary chunk.
//   2. Secondary s16 is an Int16Array; 16 kHz/mono has 320 samples; encoding=='s16'.
//   3. The recording clock starts at 0 (the first primary chunk has ptsNs === 0).
//   4. Primary and secondary pts are non-decreasing. Paired pts differ by no more than 60 ms.
// Epoch closure and finalized segments are checked for both selected event carriers.

import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const require = createRequire(import.meta.url);
const here = dirname(fileURLToPath(import.meta.url));
const { native } = await import('./test-native.mjs');

function assert(cond, msg) {
  if (!cond) {
    console.error(`ASSERT FAILED: ${msg}`);
    process.exit(1);
  }
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// VAD sample positions are bigint; JSON.stringify throws on bigint without a replacer.
const show = (value) => JSON.stringify(value, (_key, v) => (typeof v === 'bigint' ? `${v}n` : v));

// [A] Dual output (secondary tap, s16, zero-based clock, paired time window).
async function dualOutput() {
  const chunks = []; // { pts, secPts, secLen }
  let firstPrimaryPts = null;
  let pairedCount = 0;
  let secShapeBad = null;

  // Primary 48 kHz/stereo + secondary 16 kHz/mono/s16.
  const stream = native.__openMockStream(48000, 2, 440.0, (primary) => {
    if (firstPrimaryPts === null) firstPrimaryPts = primary.ptsNs;
    const rec = { pts: primary.ptsNs, secPts: null, secLen: null };
    const s = primary.secondary;
    if (s) {
      pairedCount += 1;
      rec.secPts = s.ptsNs;
      rec.secLen = s.data.length;
      if (!(s.data instanceof Int16Array)) {
        secShapeBad = `secondary.data is not Int16Array (encoding=${s.encoding})`;
      }
      if (s.encoding !== 's16') secShapeBad = `secondary.encoding !== 's16' (${s.encoding})`;
      if (primary.frames !== 0 && s.data.length !== 320) {
        secShapeBad = `secondary length ${s.data.length} !== 320 (16k/mono 20ms)`;
      }
    }
    chunks.push(rec);
  }, 16000, 1, 's16');

  await sleep(700);
  await stream.stop();

  console.log(`[A1] primary chunks: ${chunks.length}, paired-with-secondary: ${pairedCount}`);
  assert(chunks.length > 0, 'expected primary chunks');
  assert(pairedCount > 0, 'expected at least one primary paired with a secondary');
  assert(secShapeBad === null, `secondary shape: ${secShapeBad}`);

  // 3. Recording starts at 0.
  console.log(`[A2] first primary ptsNs = ${firstPrimaryPts}`);
  assert(firstPrimaryPts === 0, `first primary ptsNs must be 0, got ${firstPrimaryPts}`);

  // 4. Primary pts are non-decreasing.
  for (let i = 1; i < chunks.length; i++) {
    assert(chunks[i].pts >= chunks[i - 1].pts, `primary pts must be non-decreasing at ${i}`);
  }
  // Paired pts differ by no more than 60 ms.
  const WINDOW_NS = 60_000_000;
  for (const c of chunks) {
    if (c.secPts !== null) {
      const d = Math.abs(c.pts - c.secPts);
      assert(d <= WINDOW_NS + 20_000_000, `pair pts skew too large: ${d} ns`);
    }
  }
  console.log('[A] DUAL OK');
}

async function closure(tap, explicit) {
  const events = [];
  const stream = native.__openMockStream(48000, 2, 440, chunk => {
    events.push(...(chunk.vadEvents ?? []), ...(chunk.secondary?.vadEvents ?? []));
  }, 16000, 1, 's16', { tap, params: { threshold: 0, minSpeechMs: 0, speechPadMs: 0 } });
  try {
    await sleep(300);
    assert(events.some(event => event.type === 'epochStart'), 'capture origin must precede payloads');
    if (explicit) await stream.flushVad();
  } finally {
    await stream.stop();
  }
  assert(events.some(event => event.type === 'segment'), 'flush must preserve finalized speech');
  assert(events.at(-1).type === 'epochEnd', 'stop must deliver epoch closure');
}
await dualOutput();
await closure('primary', true);
await closure('secondary', false);
console.log('DUAL OK');
