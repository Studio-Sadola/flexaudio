// N-API 0.3.0 契約: processes() と FlexStream.stop() は Promise。
// 実音不要。前提は smoke.mjs と同じ（同じディレクトリの flexaudio.node）。
//
// (a) processes() は Promise を返し、配列か型のあるエラーで終わる
// (b) stop() の resolve より前に frames:0 の締めが届く。stop() の後に frames>0
//     が来たならそれも resolve より前（来なければこの確かめは飛ばす）。
//     resolve の後に onChunk は来ない。
// (c) onChunk の中から stop() を呼んでも resolve まで行く（固まらない）
// (d) stop() を 2 回呼んでも両方 resolve
// (e) 既に止まった後の stop() は JS スレッドで即 resolve（P1 の経路 a）
//     TSFN の Closing は mock では作れない（napi TSFN の Closing は
//     環境＝Node 終了中の状態）。Stopped 後の即 resolve で代わる。

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
  // P1 経路 (a): phase が既に Stopped なら、TSFN 往復を待たず JS スレッドで
  // resolve_undefined する。TSFN の Closing は mock では作れない（napi の
  // Closing は Node 終了中の環境状態）。
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
