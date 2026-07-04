# flexaudio-ffi

flexaudio を C から使うための **C ABI バインディング**（プル型）。C アプリが
flexaudio をインプロセスで叩く第三の経路（第一は CLI パイプ、第二は N-API addon）。
呼び出し側が `flexaudio_poll_chunk` / `flexaudio_poll_event` を周期的に呼んでチャンク・
イベントを取り出す。

キャプチャ本体に加えて、3 つのアドオンを C へ露出する:

- **VAD**（発話区間検出・silero VAD / ONNX）— `flexaudio_vad_*`
- **FLAC**（録音チャンクの逐次可逆圧縮・ローテーション対応）— `flexaudio_flac_*`
- **denoise**（RNNoise によるノイズ抑制）— `flexaudio_denoise_*`

VAD / denoise はストリームに組み込むこともできる（`FlexConfig` の `has_vad` / `denoise`）。
デバイス着脱の監視（ホットプラグ）は `flexaudio_watch_devices` 系で取れる。

## ビルドとヘッダ生成

`staticlib`（`.a`）と `cdylib`（`.so` / `.dll` / `.dylib`）の両方を生成する。

```sh
cargo build -p flexaudio-ffi --release
# ヘッダを再生成（ABI を変えたら必ず）
cbindgen --config cbindgen.toml --crate flexaudio-ffi --output include/flexaudio.h
```

生成物を `include/flexaudio.h` と一緒にリンクする。全関数は FFI 境界で panic を巻き上げず
（`catch_unwind`）、失敗時は負のコードを返して `flexaudio_last_error()` にメッセージを残す。
C へ渡した確保物（`FlexChunk::data` / VAD イベント配列 / デバイス文字列など）は、対応する
free 関数で必ず flexaudio 側に解放させる（C の `free` は使わない）。

## 基本のキャプチャ

```c
#include "flexaudio.h"
#include <stdio.h>

int main(void) {
    FlexConfig cfg = {0};              // すべて 0 = 既定（mic / 48k / stereo / 20ms）
    cfg.kind = FLEX_SOURCE_KIND_MIC;

    FlexStream *s = flexaudio_open(&cfg);
    if (!s) {
        fprintf(stderr, "open failed: %s\n", flexaudio_last_error());
        return 1;
    }
    if (flexaudio_start(s) != FLEX_OK) {
        fprintf(stderr, "start failed: %s\n", flexaudio_last_error());
        flexaudio_free(s);
        return 1;
    }

    FlexChunk chunk;
    for (int i = 0; i < 100; i++) {
        int r = flexaudio_poll_chunk(s, &chunk);
        if (r < 0) break;              // エラー（flexaudio_last_error）
        if (r == 0) continue;          // 今は無し（少し待って再試行）
        // chunk.data は interleaved f32（chunk.len 要素 = frames * channels）
        printf("frames=%u peak=%.3f\n", chunk.frames, chunk.peak);
        flexaudio_chunk_free(&chunk);  // data を解放
    }

    flexaudio_free(s);
    return 0;
}
```

## ストリームに denoise / VAD を組み込む

`denoise` / `has_vad` を立てると、`flexaudio_poll_chunk` が返す直前にチャンクを
**denoise → VAD** の順で通す。denoise は 48kHz 出力が前提（`output_rate` が 48000 以外だと
`flexaudio_open` が NULL を返す）。VAD が確定したイベントは `FlexChunk::vad_events` に入り、
`flexaudio_chunk_free` が `data` と一緒に解放する。

```c
FlexConfig cfg = {0};
cfg.kind = FLEX_SOURCE_KIND_MIC;
cfg.denoise = true;      // 48k 出力が前提（output_rate=0 は 48000）
cfg.has_vad = true;      // cfg.vad は全 0 = silero 既定（threshold 0.5 など）

FlexStream *s = flexaudio_open(&cfg);
/* ... start / poll ... */
if (flexaudio_poll_chunk(s, &chunk) == 1) {
    for (size_t i = 0; i < chunk.vad_events_len; i++) {
        FlexVadEvent ev = chunk.vad_events[i];   // kind: 0=開始 / 1=終了
        printf("%s @ %lld\n", ev.kind == 0 ? "speech-start" : "speech-end",
               (long long)ev.at_sample);
    }
    flexaudio_chunk_free(&chunk);    // data と vad_events を両方解放
}
```

## 独立ハンドル（ストリームなしで使う）

### VAD

手元の任意フォーマットの f32 サンプルを流し込める（内部で mono 化・VAD レートへ
リサンプル）。イベント配列は `flexaudio_vad_events_free` で解放する。

```c
FlexVad *vad = flexaudio_vad_new(NULL);        // NULL = 既定設定
FlexVadEvent *events = NULL;
size_t n = 0;
// samples: 48k/stereo の interleaved f32（len 要素）
if (flexaudio_vad_process(vad, samples, len, 48000, 2, &events, &n) == FLEX_OK) {
    for (size_t i = 0; i < n; i++) { /* events[i].kind / at_sample */ }
    flexaudio_vad_events_free(events, n);
}
flexaudio_vad_free(vad);
```

### FLAC

interleaved f32（flexaudio の正規形 48k/stereo をそのまま渡せる）を逐次可逆圧縮する。
`split_seconds` に 1 以上を与えると、その秒数ごとに `rec-001.flac`, `rec-002.flac`, … と
連番でローテーションする（0 なら単一ファイル）。

```c
// 単一ファイル
FlexFlac *flac = flexaudio_flac_create("rec.flac", 48000, 2, 0);
flexaudio_flac_write(flac, samples, len);       // 何度でも追記
flexaudio_flac_finalize(flac);                  // ヘッダ確定（以後 write は不可）
flexaudio_flac_free(flac);

// 5 分ごとに分割 → rec-001.flac, rec-002.flac, ...
FlexFlac *split = flexaudio_flac_create("rec.flac", 48000, 2, 300);
```

### denoise

interleaved f32（48kHz・±1.0 正規化）をインプレースでノイズ抑制する。出力は入力を
480 サンプル/ch 遅らせた列で、先頭のその分は無音になる（ストリーミング遅延）。

```c
FlexDenoiser *dn = flexaudio_denoise_new(1);    // 1 = mono / 2 = stereo
flexaudio_denoise_process(dn, samples, len);    // インプレース（48kHz 前提）
flexaudio_denoise_free(dn);
```

## デバイス着脱の監視（ホットプラグ）

デバイスの接続・切断・既定変更を pull 型で取れる。`id` / `name` は
`flexaudio_device_event_free` で解放する。

```c
FlexWatcher *w = flexaudio_watch_devices();     // 非対応環境では no-op へ縮退
FlexDeviceEvent ev;
int r = flexaudio_watcher_poll(w, &ev);
if (r == 1) {
    switch (ev.kind) {
        case FLEX_DEVICE_EVENT_KIND_ADDED:          /* ev.id / ev.name / ... */ break;
        case FLEX_DEVICE_EVENT_KIND_REMOVED:        /* ev.id のみ */ break;
        case FLEX_DEVICE_EVENT_KIND_DEFAULT_CHANGED:/* ev.id / ev.source_kind */ break;
        default: break;
    }
    flexaudio_device_event_free(&ev);
}
flexaudio_watcher_free(w);
```

## ライセンスとサードパーティ

このクレート自体は MIT（ワークスペース全体と同じ・`LICENSE` を参照）。C ABI から露出する
アドオンは、以下のオフライン処理ライブラリ／モデルに依存する。いずれも実行時の
ネットワークもモデルファイル配布も要らない（重み・モデルはビルド時に埋め込む／取得する）。

| コンポーネント | 用途 | ライセンス |
| --- | --- | --- |
| [flacenc](https://crates.io/crates/flacenc) | FLAC エンコード（`flexaudio-encode`） | Apache-2.0 |
| [nnnoiseless](https://crates.io/crates/nnnoiseless)（RNNoise 移植） | ノイズ抑制（`flexaudio-denoise`） | BSD-3-Clause |
| [ort](https://crates.io/crates/ort) / ONNX Runtime | VAD の推論実行（`flexaudio-vad`） | MIT OR Apache-2.0 / ONNX Runtime は MIT |
| Silero VAD モデル | VAD のモデル重み（バイナリ埋め込み） | MIT |

再配布時は上記の著作権表示・ライセンス条項を同梱すること。
