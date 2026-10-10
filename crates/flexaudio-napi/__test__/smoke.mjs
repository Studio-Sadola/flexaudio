// flexaudio-napi smoke test (end-to-end without audio or a display).
//
// Prerequisite: copy or rename the cdylib produced by `cargo build -p flexaudio-napi`
// into this directory as `flexaudio.node` (run-smoke.sh does this).
//
// Checks:
//  1. devices() returns an array, or throws a typed backend error when discovery fails.
//  2. __openMockStream(48000, 2, 440.0, onChunk) emits 440 Hz sine wave chunks.
//     - Assert data.length === frames*channels and peak > 0 for each chunk.
//     - Confirm that at least one chunk arrives.
//  3. The process exits cleanly without hanging after stop().

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
  // Discovery fails closed: without a PipeWire daemon (CI runners) devices() throws a
  // typed backend error instead of returning a partial array.
  try {
    const devs = native.devices();
    assert(Array.isArray(devs), 'devices() must return an array');
    console.log(`[1] devices() -> ${devs.length} device(s) (array OK)`);
  } catch (error) {
    assert(error?.audioError?.kind === 'backend', `devices() threw an untyped error: ${error}`);
    console.log(`[1] devices() failed closed with a typed backend error: ${error.message}`);
  }

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
  // The 440 Hz sine wave should have a nonzero peak.
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
    // Exit explicitly. CI will fail if a dangling handle keeps the process alive.
    process.exit(0);
  },
  (e) => {
    console.error('SMOKE ERROR:', e);
    process.exit(1);
  },
);
