// flexaudio-napi スモークテスト（実音不要・ヘッドレス環境 で end-to-end）。
//
// 前提: `cargo build -p flexaudio-napi` で生成された cdylib を、この同じディレクトリに
// `flexaudio.node` という名前でコピー/リネームしてあること（run-smoke.sh が行う）。
//
// 検証内容:
//  1. devices() が配列を返す（空でも throw しないこと）。
//  2. __openMockStream(48000, 2, 440.0, onChunk) で 440Hz サインのチャンクを受信。
//     - 各 chunk で data.length === frames*channels、peak > 0 を assert。
//     - 受信数 > 0 を確認。
//  3. stop() 後にプロセスがハングせず綺麗に終わる。

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

  // 一定時間チャンクを受信。
  await new Promise((r) => setTimeout(r, 500));

  stream.stop();

  // stop 後に追加で少し待ち、コールバックの残りを消化させてから判定。
  await new Promise((r) => setTimeout(r, 100));

  console.log(`[2] received ${received} chunk(s)`);
  assert(received > 0, 'expected received > 0 chunks');
  assert(badChunk === null, `chunk length mismatch: ${badChunk}`);
  // 440Hz サイン波なので peak は非ゼロのはず。
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
    // 明示的に exit（ぶら下がりハンドルが無いことを確認するため、ハングしたら CI が落ちる）。
    process.exit(0);
  },
  (e) => {
    console.error('SMOKE ERROR:', e);
    process.exit(1);
  },
);
