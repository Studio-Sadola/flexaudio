// flexaudio-napi シームレス・ソース切替の実音 end-to-end 手動検証（実機 / PipeWire 必須）。
//
// openStream で開いた単一ストリームに対し switchSource を呼び、mic → system →
// process とソースをホットスワップしても 1 本の連続コールバックストリームのまま
// 録れることを確かめる。CI 不可（実音・再生プロセス要）＝手動実行用。
//
// 使い方:
//   # 既知振幅のサインを鳴らすプロセスを用意し PID を控える（例 pw-cat にサインをパイプ）
//   TARGET_PID=<pid> node realswitch.mjs
//
// 期待: t=0,1s（mic 区間）はマイク入力で peak が高め、t>=2s（system→process 区間）は
// 再生中サインの振幅（例 0.3）に揃う。切替は単一ストリーム内で透過。
import { createRequire } from 'module';
const require = createRequire(import.meta.url);
const flex = require('./flexaudio.node');

const pid = parseInt(process.env.TARGET_PID, 10);
const t0 = Date.now();
const buckets = {};
const rec = (p) => {
  const s = Math.floor((Date.now() - t0) / 1000);
  buckets[s] = Math.max(buckets[s] || 0, p);
};

const stream = flex.openStream(
  { kind: 'mic', outputRate: 48000, outputChannels: 2 },
  (c) => rec(c.peak),
  null,
);
console.log('start mic');
setTimeout(() => { stream.switchSource({ kind: 'system', outputRate: 48000, outputChannels: 2 }); console.log('switch -> system'); }, 2000);
setTimeout(() => { stream.switchSource({ kind: 'process', processId: pid, outputRate: 48000, outputChannels: 2 }); console.log('switch -> process'); }, 4000);
setTimeout(() => {
  stream.stop();
  console.log(JSON.stringify(Object.entries(buckets).map(([s, p]) => `t=${s}s maxPeak=${p.toFixed(3)}`)));
  process.exit(0);
}, 6500);
