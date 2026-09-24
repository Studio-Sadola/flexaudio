// flexaudio-napi dual output + flushVad E2E (no real audio needed; headless).
//
// Prerequisite: the cdylib from `cargo build -p flexaudio-napi` has been copied/renamed into
// the same directory as `flexaudio.node` (the run-smoke.sh family does this).
//
// Enables the secondary tap / integrated VAD via the extended arguments of __openMockStream
// and verifies without real capture:
//  [A] Dual output:
//   1. The time-matched secondary chunk (primary.secondary) arrives paired with the primary.
//   2. Secondary s16 is Int16Array, 16k/mono is 320 samples, encoding=='s16'.
//   3. The recording clock starts at zero (ptsNs === 0 for the first primary chunk).
//   4. Primary and secondary pts are non-decreasing. The pts difference within a pair is
//      within the 60ms window.
//  [B] flushVad (addendum 2-2):
//   5. Open speech segment → flushVad → speechEnd rides on the next chunk's vadEvents.
//   6. atNs of vadEvents is recording-start-based (>= 0) and monotonically non-decreasing.
//   7. stop() auto-runs flushVad after the audio stop-flush, and the final speechEnd
//      arrives.

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

// [A] Dual output (secondary tap, s16, zero-based clock, pair window).
async function dualOutput() {
  const chunks = []; // { pts, secPts, secLen }
  let firstPrimaryPts = null;
  let pairedCount = 0;
  let secShapeBad = null;

  // Primary 48k/stereo + secondary 16k/mono/s16.
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

  // 3. Recording starts at zero.
  console.log(`[A2] first primary ptsNs = ${firstPrimaryPts}`);
  assert(firstPrimaryPts === 0, `first primary ptsNs must be 0, got ${firstPrimaryPts}`);

  // 4. Primary pts non-decreasing.
  for (let i = 1; i < chunks.length; i++) {
    assert(chunks[i].pts >= chunks[i - 1].pts, `primary pts must be non-decreasing at ${i}`);
  }
  // The pts difference within a pair is within the 60ms window.
  const WINDOW_NS = 60_000_000;
  for (const c of chunks) {
    if (c.secPts !== null) {
      const d = Math.abs(c.pts - c.secPts);
      assert(d <= WINDOW_NS + 20_000_000, `pair pts skew too large: ${d} ns`);
    }
  }
  console.log('[A] DUAL OK');
}

// Shared check of atNs in a vadEvents array (recording-start-based, monotonically non-decreasing).
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

// Collect from both taps so vadEvents are picked up whichever tap (primary or secondary) the VAD is on.
function collectEvents(primary, sink) {
  if (primary.vadEvents) for (const ev of primary.vadEvents) sink.push(ev);
  const s = primary.secondary;
  if (s && s.vadEvents) for (const ev of s.vadEvents) sink.push(ev);
}

// [B] flushVad: a runtime force-finalize puts the final speechEnd on the next chunk (vadTap='primary').
async function flushVadMidStream() {
  const events = [];
  // vadTap primary, threshold 0 → every frame counts as speech = no silence ever comes, so the
  // segment stays open. process does not finalize it.
  const stream = native.__openMockStream(
    48000, 2, 440.0,
    (primary) => collectEvents(primary, events),
    undefined, undefined, undefined, // no secondary tap
    0.0, 'primary',                  // vadThreshold=0, vadTap='primary'
  );

  await sleep(300);
  // No silence comes, so no speech events are finalized before flushVad.
  assert(events.length === 0, `expected no vad events before flushVad, got ${events.length}`);

  stream.flushVad();
  await sleep(150); // The next chunk carries the flush events.

  const ends = events.filter((e) => e.type === 'speechEnd');
  const starts = events.filter((e) => e.type === 'speechStart');
  console.log(`[B1] after flushVad: ${starts.length} speechStart, ${ends.length} speechEnd`);
  assert(ends.length >= 1, `expected a speechEnd after flushVad, got ${JSON.stringify(events)}`);
  assert(starts.length >= 1, `flushVad should also emit the paired speechStart`);
  checkVadEvents(events);
  console.log('[B2] atNs recording-0-based & monotonic OK');

  await stream.stop();
  checkVadEvents(events); // Still monotonically non-decreasing after the stop auto-flush.
  console.log('[B] FLUSHVAD MID-STREAM OK');
}

// [C] Automatic flushVad in stop(): stop with the speech segment still open, without calling
// flushVad explicitly. With a synthetic wave that never goes silent, the segment is never
// closed by silence, so a speechEnd arriving proves that stop() auto-ran flushVad after the
// audio stop-flush. Verified with vadTap='secondary' as in standard operation (addendum 2-1)
// (the residue of the secondary 16k resampler always produces a stop-flush tail).
async function flushVadOnStop() {
  const events = [];
  const stream = native.__openMockStream(
    48000, 2, 440.0,
    (primary) => collectEvents(primary, events),
    16000, 1, 's16',   // secondary tap 16k/mono/s16 (standard operation)
    0.0, 'secondary',  // vadThreshold=0, vadTap='secondary'
  );

  await sleep(300);
  assert(events.length === 0, `expected no vad events before stop, got ${events.length}`);

  await stream.stop();      // Do not call flushVad. stop should run it automatically.

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
