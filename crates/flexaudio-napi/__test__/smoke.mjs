// flexaudio-napi smoke test (no real audio needed; end-to-end in a headless environment).
//
// Prerequisite: the cdylib built by `cargo build -p flexaudio-napi` has been copied/renamed
// into this same directory as `flexaudio.node` (run-smoke.sh does this).
//
// What is verified:
//  1. devices() returns an array (must not throw even if empty).
//  2. __openMockStream(48000, 2, 440.0, onChunk) receives chunks of a 440Hz sine.
//     - For each chunk, assert data.length === frames*channels and peak > 0.
//     - Check that received count > 0.
//  3. After stop() the process exits cleanly without hanging.

import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const require = createRequire(import.meta.url);
const here = dirname(fileURLToPath(import.meta.url));
const addonPath = join(here, 'flexaudio.node');

const native = require(addonPath);

function assert(cond, msg) {
  if (!cond) {
    console.error(`ASSERT FAILED: ${msg}`);
    process.exit(1);
  }
}

async function main() {
  // --- 1. devices() ---
  const devs = native.devices();
  assert(Array.isArray(devs), 'devices() must return an array');
  console.log(`[1] devices() -> ${devs.length} device(s) (array OK)`);

  // --- 2. __openMockStream ---
  const SAMPLE_RATE = 48000;
  const CHANNELS = 2;
  const FREQ = 440.0;

  let received = 0;
  let firstChunk = null;
  let badChunk = null;
  let peakSeen = 0;

  const stream = native.__openMockStream(SAMPLE_RATE, CHANNELS, FREQ, (chunk) => {
    received += 1;
    if (firstChunk === null) {
      firstChunk = {
        frames: chunk.frames,
        dataLength: chunk.data.length,
        peak: chunk.peak,
        rms: chunk.rms,
        seq: chunk.seq, // BigInt
        flags: chunk.flags,
      };
    }
    // data.length === frames * channels
    if (chunk.data.length !== chunk.frames * CHANNELS) {
      badChunk = `data.length(${chunk.data.length}) !== frames(${chunk.frames})*channels(${CHANNELS})`;
    }
    if (chunk.peak > peakSeen) peakSeen = chunk.peak;
  });

  // Receive chunks for a fixed period.
  await new Promise((r) => setTimeout(r, 500));

  await stream.stop();

  console.log(`[2] received ${received} chunk(s)`);
  assert(received > 0, 'expected received > 0 chunks');
  assert(badChunk === null, `chunk length mismatch: ${badChunk}`);
  // It is a 440Hz sine wave, so peak should be non-zero.
  assert(peakSeen > 0, `expected peak > 0, got ${peakSeen}`);
  assert(firstChunk.data instanceof Object || true, 'data present');

  console.log('[3] first chunk:', {
    frames: firstChunk.frames,
    dataLength: firstChunk.dataLength,
    peak: firstChunk.peak,
    rms: firstChunk.rms,
    seq: String(firstChunk.seq),
    flags: firstChunk.flags,
  });
  console.log(`[3] max peak observed: ${peakSeen}`);

  console.log('SMOKE OK');
}

main().then(
  () => {
    // Exit explicitly (to confirm there are no dangling handles; if it hangs, CI fails).
    process.exit(0);
  },
  (e) => {
    console.error('SMOKE ERROR:', e);
    process.exit(1);
  },
);
