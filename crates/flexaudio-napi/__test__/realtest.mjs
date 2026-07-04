// flexaudio-napi 実音 end-to-end 手動検証テスト（実機 / PipeWire セッション必須）。
//
// smoke.mjs が MockBackend で marshaling 経路を検証するのに対し、こちらは
// 実際の音声デバイスから napi 経由でキャプチャできることを確かめる。CI 不可
// （実音・再生プロセス・対応バックエンドが要る）＝手動実行用。
//
// 使い方（例。XDG_RUNTIME_DIR が要る環境では設定すること）:
//   # 既知振幅のサインを鳴らすプロセスを用意（例: pw-cat にサインをパイプ）し PID を控える
//   TARGET_PID=<pid> KIND=process node realtest.mjs   # 特定プロセス出力をキャプチャ
//   KIND=system            node realtest.mjs           # システムループバック
//   KIND=mic               node realtest.mjs           # マイク
//
// 期待: chunks>0・firstLen === firstFrames*channels・既知振幅の音源なら maxPeak が
// その振幅に一致（例 0.3）・クリーン終了（ハング/ゾンビなし）。
import { createRequire } from 'module';
const require = createRequire(import.meta.url);
const flex = require('./flexaudio.node');

const pid = parseInt(process.env.TARGET_PID, 10);
const kind = process.env.KIND || 'process';
if (kind === 'process' && !pid) {
  console.error('process キャプチャには TARGET_PID=<pid> が必要');
  process.exit(2);
}

let count = 0, maxPeak = 0, firstFrames = 0, firstLen = 0;
const events = [];
const opts = { kind, outputRate: 48000, outputChannels: 2 };
if (kind === 'process') opts.processId = pid;

console.log('openStream', JSON.stringify(opts));
const stream = flex.openStream(
  opts,
  (chunk) => {
    count++;
    if (chunk.peak > maxPeak) maxPeak = chunk.peak;
    if (count === 1) { firstFrames = chunk.frames; firstLen = chunk.data.length; }
  },
  (ev) => { events.push(ev.type); },
);

setTimeout(() => {
  stream.stop();
  console.log(JSON.stringify({
    chunks: count,
    maxPeak: Math.round(maxPeak * 1000) / 1000,
    firstFrames, firstLen,
    events: [...new Set(events)],
  }));
  process.exit(0);
}, 6000);
