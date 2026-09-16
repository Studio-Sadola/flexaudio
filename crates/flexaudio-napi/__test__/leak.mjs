// After await stop() + dropping the stream + GC, onChunk closures (and the
// ballast they capture) must be released. Compare against allocating the same
// ballast without opening a stream. Requires `node --expose-gc`.
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const require = createRequire(import.meta.url);
const here = dirname(fileURLToPath(import.meta.url));
const native = require(join(here, 'flexaudio.node'));

if (typeof global.gc !== 'function') {
  console.error('leak.mjs requires node --expose-gc (global.gc is missing)');
  process.exit(1);
}

const N = 20;
const MB = 4;
const BALLAST_TOTAL_MB = N * MB;
const MAX_DELTA_OVER_CONTROL_MB = 40;

const rss = () => Math.round(process.memoryUsage().rss / 1048576);
const settle = async () => {
  global.gc();
  global.gc();
  await new Promise((r) => setTimeout(r, 300));
  global.gc();
  global.gc();
};

function assert(cond, msg) {
  if (!cond) {
    console.error(`ASSERT FAILED: ${msg}`);
    process.exit(1);
  }
}

async function main() {
  await settle();

  let base = rss();
  for (let i = 0; i < N; i++) {
    const b = new Uint8Array(MB * 1048576).fill(i & 255);
    if (b[0] === 999) console.log('x');
  }
  await settle();
  const controlDelta = rss() - base;
  console.log('CONTROL delta-MB', controlDelta);

  base = rss();
  for (let i = 0; i < N; i++) {
    const b = new Uint8Array(MB * 1048576).fill(i & 255);
    let s = native.__openMockStream(48000, 2, 440.0, () => {
      if (b[0] === 999) console.log('x');
    });
    await new Promise((r) => setTimeout(r, 20));
    await s.stop();
    s = null;
  }
  await settle();
  const streamDelta = rss() - base;
  console.log('STREAM delta-MB', streamDelta, '(ballast total', BALLAST_TOTAL_MB, 'MB)');

  const overControl = streamDelta - controlDelta;
  if (overControl >= MAX_DELTA_OVER_CONTROL_MB) {
    console.error(
      `onChunk ballast leaked after stop()+GC: STREAM-CONTROL=${overControl}MB ` +
        `(STREAM ${streamDelta}MB, CONTROL ${controlDelta}MB, threshold ${MAX_DELTA_OVER_CONTROL_MB}MB, ` +
        `ballast ${BALLAST_TOTAL_MB}MB). onChunk closures or chunk TSFN refs were not released.`,
    );
    process.exit(1);
  }
  assert(overControl < MAX_DELTA_OVER_CONTROL_MB, 'delta gate');
  console.log('LEAK OK', { controlDelta, streamDelta, overControl });
}

main().then(
  () => process.exit(0),
  (e) => {
    console.error('LEAK ERROR:', e);
    process.exit(1);
  },
);
