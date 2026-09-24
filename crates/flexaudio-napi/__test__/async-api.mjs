// N-API 0.3.0 contract: processes() and FlexStream.stop() are Promises.
// No real audio needed. Same prerequisites as smoke.mjs (flexaudio.node in the same directory).
//
// (a) processes() returns a Promise that ends with an array or a typed error
// (b) the frames:0 terminator arrives before stop() resolves. If frames>0 arrives after
//     stop(), it also comes before the resolve (this check is skipped if none arrives).
//     No onChunk comes after the resolve.
// (c) calling stop() from inside onChunk still reaches resolve (does not hang)
// (d) calling stop() twice resolves both
// (e) stop() after already stopped resolves immediately on the JS thread (P1 path a)
//     TSFN Closing cannot be produced with the mock (napi TSFN Closing is an
//     environment state = Node is exiting). The immediate resolve after Stopped stands in.

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

const withTimeout = (p, ms, label) =>
  Promise.race([
    p,
    new Promise((_, reject) => {
      setTimeout(() => reject(new Error(`${label} timed out after ${ms} ms`)), ms);
    }),
  ]);

async function testProcessesIsPromise() {
  const ret = native.processes();
  assert(ret && typeof ret.then === 'function', 'processes() must return a Promise');
  try {
    const list = await withTimeout(ret, 5000, 'processes()');
    assert(Array.isArray(list), `processes() must resolve to an array, got ${typeof list}`);
    console.log(`[a] processes() resolved to array length=${list.length}`);
  } catch (e) {
    assert(e && typeof e === 'object', 'processes() reject must be an Error object');
    assert(typeof e.message === 'string' && e.message.length > 0, 'rejected Error must have a message');
    console.log(`[a] processes() rejected with typed error: ${e.message}`);
  }
}

async function testStopFlushesLastPcmAndTerminatorBeforeResolve() {
  let sawLastPcm = false;
  let sawFramesZero = false;
  let resolved = false;
  let stopInvoked = false;
  let lastPcmAfterStop = false;
  let chunksAfterResolve = 0;
  const stream = native.__openMockStream(48000, 2, 440.0, (chunk) => {
    if (!chunk) {
      return;
    }
    if (resolved) {
      chunksAfterResolve += 1;
      return;
    }
    if (chunk.frames > 0) {
      assert(!resolved, 'last frames>0 PCM must arrive before stop() resolves');
      sawLastPcm = true;
      if (stopInvoked) {
        lastPcmAfterStop = true;
      }
    } else if (chunk.frames === 0) {
      assert(!resolved, 'frames:0 terminator must arrive before stop() resolves');
      sawFramesZero = true;
    }
  });
  await new Promise((r) => setTimeout(r, 120));
  stopInvoked = true;
  await withTimeout(stream.stop(), 5000, 'stop()');
  resolved = true;
  assert(sawLastPcm, 'expected a frames>0 PCM chunk before stop() resolved');
  if (lastPcmAfterStop) {
    console.log('[b] frames>0 after stop() arrived before resolve');
  } else {
    console.log('[b] no frames>0 after stop() was invoked (order check skipped)');
  }
  assert(sawFramesZero, 'expected a frames:0 terminator chunk before stop() resolved');
  await new Promise((r) => setTimeout(r, 50));
  assert(chunksAfterResolve === 0, `no onChunk after stop() resolves, got ${chunksAfterResolve}`);
  console.log('[b] frames:0 terminator arrived before resolve; no onChunk after resolve');
}

async function testStopFromOnChunk() {
  let stopPromise = null;
  const stream = native.__openMockStream(48000, 2, 440.0, (chunk) => {
    if (stopPromise === null && chunk && chunk.frames > 0) {
      stopPromise = stream.stop();
    }
  });
  const started = Date.now();
  while (stopPromise === null && Date.now() - started < 3000) {
    await new Promise((r) => setTimeout(r, 20));
  }
  assert(stopPromise !== null, 'onChunk should have called stop()');
  await withTimeout(stopPromise, 5000, 'stop() from onChunk');
  console.log('[c] stop() called from onChunk resolved (no deadlock)');
}

async function testStopTwice() {
  const stream = native.__openMockStream(48000, 2, 440.0, () => {});
  await new Promise((r) => setTimeout(r, 80));
  const a = stream.stop();
  const b = stream.stop();
  await withTimeout(Promise.all([a, b]), 5000, 'double stop()');
  console.log('[d] stop() twice (in-flight) both resolved');
}

async function testStopAfterAlreadyStoppedResolvesImmediately() {
  // P1 path (a): if the phase is already Stopped, resolve_undefined runs on the JS thread
  // without waiting for a TSFN round trip. TSFN Closing cannot be produced with the mock
  // (napi's Closing is an environment state while Node is exiting).
  const stream = native.__openMockStream(48000, 2, 440.0, () => {});
  await new Promise((r) => setTimeout(r, 50));
  await withTimeout(stream.stop(), 5000, 'first stop()');
  const started = Date.now();
  await withTimeout(stream.stop(), 2000, 'stop() after already stopped');
  const elapsed = Date.now() - started;
  assert(
    elapsed < 500,
    `already-stopped stop() should resolve immediately, took ${elapsed} ms`,
  );
  console.log(`[e] already-stopped stop() resolved immediately (${elapsed} ms)`);
}

async function main() {
  await testProcessesIsPromise();
  await testStopFlushesLastPcmAndTerminatorBeforeResolve();
  await testStopFromOnChunk();
  await testStopTwice();
  await testStopAfterAlreadyStoppedResolvesImmediately();
  console.log('ASYNC-API OK');
}

main().then(
  () => process.exit(0),
  (e) => {
    console.error('ASYNC-API ERROR:', e);
    process.exit(1);
  },
);
