// flexaudio-napi デュアル出力 + flushVad E2E（実音不要・ヘッドレス）。
//
// 前提: `cargo build -p flexaudio-napi` の cdylib を同じディレクトリに `flexaudio.node`
// としてコピー/リネームしてあること（run-smoke.sh 系が行う）。
//
// __openMockStream の拡張引数で副タップ / 統合 VAD を有効化し、実キャプチャ無しで検証する:
//  [A] デュアル出力:
//   1. 主チャンクに時刻対応する副チャンク（primary.secondary）がペアで届く。
//   2. 副 s16 は Int16Array・16k/mono は 320 sample・encoding=='s16'。
//   3. 録音クロックは 0 起点（最初に届く主チャンクの ptsNs === 0）。
//   4. 主・副の pts は非減少。ペアの pts 差は 60ms 窓内。
//  [B] flushVad（追補2-2）:
//   5. 開いた発話 → flushVad → 次チャンクの vadEvents に speechEnd が載る。
//   6. vadEvents の atNs は録音 0 起点（>= 0）で単調非減少。
//   7. stop() が音の stop-flush の後に flushVad を自動実行し、最終 speechEnd が届く。

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

// [A] デュアル出力（副タップ・s16・0 起点クロック・ペア窓）。
async function dualOutput() {
  const chunks = []; // { pts, secPts, secLen }
  let firstPrimaryPts = null;
  let pairedCount = 0;
  let secShapeBad = null;

  // 主 48k/stereo + 副 16k/mono/s16。
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
      if (s.data.length !== 320) secShapeBad = `secondary length ${s.data.length} !== 320 (16k/mono 20ms)`;
    }
    chunks.push(rec);
  }, 16000, 1, 's16');

  await sleep(700);
  stream.stop();
  await sleep(150);

  console.log(`[A1] primary chunks: ${chunks.length}, paired-with-secondary: ${pairedCount}`);
  assert(chunks.length > 0, 'expected primary chunks');
  assert(pairedCount > 0, 'expected at least one primary paired with a secondary');
  assert(secShapeBad === null, `secondary shape: ${secShapeBad}`);

  // 3. 録音 0 起点。
  console.log(`[A2] first primary ptsNs = ${firstPrimaryPts}`);
  assert(firstPrimaryPts === 0, `first primary ptsNs must be 0, got ${firstPrimaryPts}`);

  // 4. 主 pts 非減少。
  for (let i = 1; i < chunks.length; i++) {
    assert(chunks[i].pts >= chunks[i - 1].pts, `primary pts must be non-decreasing at ${i}`);
  }
  // ペアの pts 差は 60ms 窓内。
  const WINDOW_NS = 60_000_000;
  for (const c of chunks) {
    if (c.secPts !== null) {
      const d = Math.abs(c.pts - c.secPts);
      assert(d <= WINDOW_NS + 20_000_000, `pair pts skew too large: ${d} ns`);
    }
  }
  console.log('[A] DUAL OK');
}

// vadEvents 配列の atNs 検証（録音 0 起点・単調非減少）を共通化する。
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

// 主・副どちらのタップに VAD が乗っていても vadEvents を拾えるよう、両方から収集する。
function collectEvents(primary, sink) {
  if (primary.vadEvents) for (const ev of primary.vadEvents) sink.push(ev);
  const s = primary.secondary;
  if (s && s.vadEvents) for (const ev of s.vadEvents) sink.push(ev);
}

// [B] flushVad: 実行中の強制確定で最終 speechEnd が次チャンクに載る（vadTap='primary'）。
async function flushVadMidStream() {
  const events = [];
  // vadTap primary・threshold 0 → 全フレームが発話扱い＝無音が来ないのでセグメントは
  // 開いたまま。process では確定しない。
  const stream = native.__openMockStream(
    48000, 2, 440.0,
    (primary) => collectEvents(primary, events),
    undefined, undefined, undefined, // 副タップ無し
    0.0, 'primary',                  // vadThreshold=0, vadTap='primary'
  );

  await sleep(300);
  // 無音が来ないので flushVad 前は発話イベントが確定しない。
  assert(events.length === 0, `expected no vad events before flushVad, got ${events.length}`);

  stream.flushVad();
  await sleep(150); // 次チャンクが flush イベントを運ぶ。

  const ends = events.filter((e) => e.type === 'speechEnd');
  const starts = events.filter((e) => e.type === 'speechStart');
  console.log(`[B1] after flushVad: ${starts.length} speechStart, ${ends.length} speechEnd`);
  assert(ends.length >= 1, `expected a speechEnd after flushVad, got ${JSON.stringify(events)}`);
  assert(starts.length >= 1, `flushVad should also emit the paired speechStart`);
  checkVadEvents(events);
  console.log('[B2] atNs recording-0-based & monotonic OK');

  stream.stop();
  await sleep(150);
  checkVadEvents(events); // stop 自動 flush 後も単調非減少。
  console.log('[B] FLUSHVAD MID-STREAM OK');
}

// [C] stop() の自動 flushVad: flushVad を明示的に呼ばず、開いた発話のまま stop する。
// 無音が来ない合成波では発話は silence では閉じないので、speechEnd が届くのは stop() が
// 音の stop-flush の後に flushVad を自動実行したことの証明になる。標準運用（追補2-1）の
// vadTap='secondary' で検証する（副 16k リサンプラの残余で stop-flush テールが必ず出る）。
async function flushVadOnStop() {
  const events = [];
  const stream = native.__openMockStream(
    48000, 2, 440.0,
    (primary) => collectEvents(primary, events),
    16000, 1, 's16',   // 副タップ 16k/mono/s16（標準運用）
    0.0, 'secondary',  // vadThreshold=0, vadTap='secondary'
  );

  await sleep(300);
  assert(events.length === 0, `expected no vad events before stop, got ${events.length}`);

  stream.stop();      // flushVad は呼ばない。stop が自動実行するはず。
  await sleep(200);

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
