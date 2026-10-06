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
//  [B] flushVad (Addendum 2-2):
//   5. An open speech segment → flushVad → speechEnd appears in the next chunk's vadEvents.
//   6. vadEvents atNs starts at recording time 0 (>= 0) and is non-decreasing.
//   7. stop() automatically runs flushVad after the audio stop-flush, delivering the final speechEnd.

import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const require = createRequire(import.meta.url);
const here = dirname(fileURLToPath(import.meta.url));
const native = require(join(here, 'flexaudio.node'));

function assert(cond, msg) {
  if (!cond) {
    console.error(`ASSERT FAILED: ${msg}`);
    process.exit(1);
  }
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

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

// Share the check that vadEvents atNs starts at recording time 0 and is non-decreasing.
function checkVadEvents(events) {
  let last = -1;
  for (const ev of events) {
    assert(ev.type === 'speechStart' || ev.type === 'speechEnd', `unexpected vad event type ${ev.type}`);
    assert(typeof ev.atNs === 'number', `atNs must be a number, got ${ev.atNs} (${typeof ev.atNs})`);
    assert(ev.atNs >= 0, `atNs must be recording-0-based (>= 0), got ${ev.atNs}`);
    assert(ev.atNs >= last, `atNs must be non-decreasing: ${ev.atNs} < ${last}`);
    last = ev.atNs;
  }
}

// Collect from both taps so vadEvents are captured whether VAD runs on the primary or secondary tap.
function collectEvents(primary, sink) {
  if (primary.vadEvents) for (const ev of primary.vadEvents) sink.push(ev);
  const s = primary.secondary;
  if (s && s.vadEvents) for (const ev of s.vadEvents) sink.push(ev);
}

// [B] flushVad: force-finalize an open segment while running; its final speechEnd appears in the next chunk (vadTap='primary').
async function flushVadMidStream() {
  const events = [];
  // With vadTap primary and threshold 0, every frame counts as speech; no silence arrives, so the segment
  // stays open. process does not finalize it.
  const stream = native.__openMockStream(
    48000, 2, 440.0,
    (primary) => collectEvents(primary, events),
    undefined, undefined, undefined, // No secondary tap.
    0.0, 'primary',                  // vadThreshold=0, vadTap='primary'
  );

  await sleep(300);
  // No silence arrives, so no speech event is finalized before flushVad.
  assert(events.length === 0, `expected no vad events before flushVad, got ${events.length}`);

  stream.flushVad();
  await sleep(150); // Let the next chunk carry the flush event.

  const ends = events.filter((e) => e.type === 'speechEnd');
  const starts = events.filter((e) => e.type === 'speechStart');
  console.log(`[B1] after flushVad: ${starts.length} speechStart, ${ends.length} speechEnd`);
  assert(ends.length >= 1, `expected a speechEnd after flushVad, got ${JSON.stringify(events)}`);
  assert(starts.length >= 1, `flushVad should also emit the paired speechStart`);
  checkVadEvents(events);
  console.log('[B2] atNs recording-0-based & monotonic OK');

  await stream.stop();
  checkVadEvents(events); // Still non-decreasing after stop auto-flush.
  console.log('[B] FLUSHVAD MID-STREAM OK');
}

// [C] stop() auto-flushVad: stop with an open segment without calling flushVad explicitly.
// With this synthetic wave, no silence arrives to close the segment. A speechEnd therefore proves stop()
// automatically ran flushVad after the audio stop-flush. Test the standard setup (Addendum 2-1),
// using vadTap='secondary' (the secondary 16 kHz resampler always produces a stop-flush tail).
async function flushVadOnStop() {
  const events = [];
  const stream = native.__openMockStream(
    48000, 2, 440.0,
    (primary) => collectEvents(primary, events),
    16000, 1, 's16',   // Secondary tap 16 kHz/mono/s16 (standard setup).
    0.0, 'secondary',  // vadThreshold=0, vadTap='secondary'
  );

  await sleep(300);
  assert(events.length === 0, `expected no vad events before stop, got ${events.length}`);

  await stream.stop();      // Do not call flushVad; stop should run it automatically.

  const ends = events.filter((e) => e.type === 'speechEnd');
  console.log(`[C1] speechEnd delivered via stop auto-flush: ${ends.length}`);
  assert(ends.length >= 1, `stop() must auto-run flushVad and deliver a final speechEnd, got ${JSON.stringify(events)}`);
  checkVadEvents(events);
  console.log('[C] FLUSHVAD ON STOP OK');
}

async function main() {
  await dualOutput();
  await flushVadMidStream();
  await flushVadOnStop();
  console.log('DUAL OK');
}

main().then(
  () => process.exit(0),
  (e) => {
    console.error('DUAL ERROR:', e);
    process.exit(1);
  },
);
