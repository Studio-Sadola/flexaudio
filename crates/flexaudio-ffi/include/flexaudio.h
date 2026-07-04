/*
 * flexaudio C API — pull-based audio capture bindings.
 *
 * This header is generated from crates/flexaudio-ffi by cbindgen. Do not edit by hand;
 * regenerate it after changing the Rust ABI.
 */


#ifndef FLEXAUDIO_H
#define FLEXAUDIO_H

#pragma once

#include <stdarg.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>

// 成功。
#define FLEX_OK 0

// 引数が無効（NULL ポインタ・不正な UTF-8・未知の列挙値など）。
#define FLEX_INVALID_ARG -1

// flexaudio の操作が失敗した（メッセージは last_error に入る）。
#define FLEX_FAILURE -2

// FFI 境界で panic を捕捉した（メッセージは last_error に入る）。
#define FLEX_PANIC -3

// ハンドルの状態が操作に合わない（finalize 済みの FLAC への write など）。
#define FLEX_INVALID_STATE -4

// 録音するオーディオソースの種別（[`flexaudio::SourceKind`] に対応）。
typedef enum FlexSourceKind {
    // マイク入力。
    FLEX_SOURCE_KIND_MIC = 0,
    // システム出力全体のループバック。
    FLEX_SOURCE_KIND_SYSTEM = 1,
    // 特定プロセスの出力ループバック。
    FLEX_SOURCE_KIND_PROCESS = 2,
    // マイクとシステム音声を 1 本に合成して録る。
    FLEX_SOURCE_KIND_MIX = 3,
} FlexSourceKind;

// process ソースで対象 PID を含めるか除くか（[`flexaudio::ProcessMode`] に対応）。
typedef enum FlexProcessMode {
    // 対象 PID（そのプロセスツリー）だけを録る。
    FLEX_PROCESS_MODE_INCLUDE = 0,
    // 対象 PID 以外の全システム音を録る。
    FLEX_PROCESS_MODE_EXCLUDE = 1,
} FlexProcessMode;

// ストリームイベントの種別（[`flexaudio::Event`] に対応）。
typedef enum FlexEventKind {
    // チャンクリング満杯によりチャンクがドロップされた（個数は `FlexEvent::count`）。
    FLEX_EVENT_KIND_CHUNK_DROPPED = 0,
    // データ到着が途絶し、ストリームが失速した。
    FLEX_EVENT_KIND_STALLED = 1,
    // 失速後にデータ到着が復帰した。
    FLEX_EVENT_KIND_RECOVERED = 2,
    // 必要な権限が拒否された。
    FLEX_EVENT_KIND_PERMISSION_DENIED = 3,
    // キャプチャデバイスが失われた。
    FLEX_EVENT_KIND_DEVICE_LOST = 4,
    // その他のバックエンドエラー（メッセージは `flexaudio_last_error` で取る）。
    FLEX_EVENT_KIND_ERROR = 5,
    // 既知のどれにも当たらないイベント（将来のバリアント追加に備える）。
    FLEX_EVENT_KIND_UNKNOWN = 6,
} FlexEventKind;

// デバイス着脱イベントの種別（[`flexaudio::DeviceEvent`] に対応）。
typedef enum FlexDeviceEventKind {
    // デバイスが追加された（`device`/`name` 等が埋まる）。
    FLEX_DEVICE_EVENT_KIND_ADDED = 0,
    // デバイスが取り外された（`id` のみ）。
    FLEX_DEVICE_EVENT_KIND_REMOVED = 1,
    // OS 既定デバイスが変わった（`id` と `source_kind`）。
    FLEX_DEVICE_EVENT_KIND_DEFAULT_CHANGED = 2,
    // 既知のどれにも当たらないイベント（将来のバリアント追加に備える）。
    FLEX_DEVICE_EVENT_KIND_UNKNOWN = 3,
} FlexDeviceEventKind;

// ノイズ抑制の不透明ハンドル。中身は [`flexaudio_denoise::Denoiser`]。
// `flexaudio_denoise_new` で作り `flexaudio_denoise_free` で解放する。
typedef struct FlexDenoiser FlexDenoiser;

// FLAC 書き出しの不透明ハンドル。`flexaudio_flac_create` で作り、`flexaudio_flac_write` で
// チャンクを追記し、`flexaudio_flac_finalize` で確定、`flexaudio_flac_free` で解放する。
typedef struct FlexFlac FlexFlac;

// 録音ストリームの不透明ハンドル。中身は [`flexaudio::Stream`] と、有効時に同居する
// アドオン（denoise / VAD）で、C 側はポインタだけを持つ。`flexaudio_open` で作り
// `flexaudio_free` で解放する。
//
// アドオンはストリームの状態としてここに閉じ込める（薄いラッパ）。`poll_chunk` が
// 返す前に denoise → VAD の順で通す。`flexaudio_switch_source` はソースだけを差し替え、
// アドオンは open 時の構成のまま保つ（gain と同じ扱い）。
typedef struct FlexStream FlexStream;

// VAD の不透明ハンドル。中身は [`flexaudio_vad::Vad`]（ONNX セッションを 1 つ持つ）。
// `flexaudio_vad_new` で作り `flexaudio_vad_free` で解放する。
typedef struct FlexVad FlexVad;

// デバイスの不透明ウォッチャハンドル。中身は [`flexaudio::DeviceWatcher`]。
// `flexaudio_watch_devices` で作り `flexaudio_watcher_free` で解放する。
typedef struct FlexWatcher FlexWatcher;

// VAD（発話区間検出）の設定。`FlexConfig::vad` と `flexaudio_vad_new` に渡す。
//
// 各値は番兵 0 で既定を表す（[`flexaudio_vad::VadConfig`] の既定値に写す）。
// `threshold` 0 → 0.5、`neg_threshold` 0 → silero 式 `max(threshold-0.15, 0.01)`、
// `min_speech_ms` 0 → 250、`min_silence_ms` 0 → 100、`speech_pad_ms` 0 → 30、
// `sample_rate` 0 → 16000。`max_speech_ms` は 0 がそのまま「無制限」（既定）を意味する。
//
// [`flexaudio_vad_new`]: crate::flexaudio_vad_new
typedef struct FlexVadConfig {
    // 発話開始とみなす確率しきい値（>=）。0 なら 0.5。
    float threshold;
    // 無音開始とみなす負側しきい値（<）。0 なら silero 式で自動決定。
    float neg_threshold;
    // 採用する発話の最小長（ms）。これ未満のセグメントは破棄。0 なら 250。
    uint32_t min_speech_ms;
    // 発話終了の確定に必要な無音長（ms）。0 なら 100。
    uint32_t min_silence_ms;
    // セグメント境界を前後に広げるパディング（ms）。0 なら 30。
    uint32_t speech_pad_ms;
    // 1 セグメントの最大長（ms）。0 は無制限（既定）。超過時は強制分割。
    uint32_t max_speech_ms;
    // サンプルレート（8000 または 16000）。0 なら 16000。
    uint32_t sample_rate;
} FlexVadConfig;

// ストリームを開くための構成。`flexaudio_open` / `flexaudio_switch_source` に渡す。
//
// 文字列・任意値は番兵で「未指定」を表す（`device_id` が NULL なら既定デバイス、
// `process_id` が 0 ならなし、`output_rate`/`output_channels`/`chunk_ms` が 0 なら既定）。
typedef struct FlexConfig {
    // ソース種別。
    enum FlexSourceKind kind;
    // 選ぶデバイスの ID（UTF-8, NUL 終端）。NULL なら既定デバイス。
    const char *device_id;
    // process ソースの対象 PID。0 ならなし（process では start 時にエラーになりうる）。
    uint32_t process_id;
    // 対象 PID を含めるか除くか（process ソースのみ）。
    enum FlexProcessMode mode;
    // 自ホストの再生音をシステム音から除くか（system ソースのみ。mix では system 側
    // に適用）。
    bool exclude_self;
    // 出力サンプルレート（Hz）。0 なら 48000。
    uint32_t output_rate;
    // 出力チャンネル数。0 なら 2。
    uint16_t output_channels;
    // チャンク長（ミリ秒）。0 なら 20。
    uint32_t chunk_ms;
    // 開始時の入力ゲイン（線形倍率）。0 なら 1.0（既定）。実行時のミュートは
    // `flexaudio_set_gain(s, 0.0)` を使う。
    float gain;
    // mix の mic 側で選ぶ入力デバイスの ID（UTF-8, NUL 終端・mix 専用）。
    // NULL なら既定入力。
    const char *mix_mic_device_id;
    // mix の system 側で選ぶ出力エンドポイントの ID（UTF-8, NUL 終端・mix 専用）。
    // NULL なら既定出力。
    const char *mix_system_device_id;
    // mix の mic 側の合成前倍率（線形・mix 専用）。0 なら 1.0（既定）。
    // 合成後にグローバル `gain` が掛かる。
    float mix_mic_gain;
    // mix の system 側の合成前倍率（線形・mix 専用）。0 なら 1.0（既定）。
    float mix_system_gain;
    // ノイズ抑制（RNNoise）をストリームに挟むか。`true` で有効。有効時は出力レートが
    // 48000 でなければ `flexaudio_open` が失敗する（NULL + last_error）。denoise は
    // `poll_chunk` が返す直前に data をインプレース処理する（VAD より前段）。
    bool denoise;
    // VAD（発話区間検出）をストリームに挟むか。`true` で `vad` の設定に従い、poll した
    // 各チャンクを VAD に通して `FlexChunk::vad_events` を埋める。
    bool has_vad;
    // VAD の設定（`has_vad` が `true` のときだけ使う。`false` なら無視）。
    struct FlexVadConfig vad;
} FlexConfig;

// VAD が確定した 1 イベント。`flexaudio_vad_process` の出力配列と `FlexChunk::vad_events`
// に入る。
//
// `at_sample` は VAD 内部レート（`sample_rate`＝8000/16000）のサンプル基準で、入力
// サンプル基準ではない（[`flexaudio_vad::VadEvent`] と同じ）。
typedef struct FlexVadEvent {
    // 種別。0 = 発話開始（SpeechStart）、1 = 発話終了（SpeechEnd）。
    int32_t kind;
    // イベントのサンプル位置（VAD 内部レート基準・開始は含む/終了は排他）。
    int64_t at_sample;
} FlexVadEvent;

// 取得した 1 チャンクのオーディオデータ。`flexaudio_poll_chunk` が埋める。
//
// `data` は flexaudio 所有の interleaved f32 で、長さは `len`（= `frames * channels`）。
// 使い終わったら必ず `flexaudio_chunk_free` で解放する（C の free は使わない）。
typedef struct FlexChunk {
    // interleaved f32 サンプルへのポインタ。`flexaudio_chunk_free` で解放する。
    float *data;
    // `data` の要素数（= `frames * channels`）。
    uintptr_t len;
    // チャンク内のフレーム数。
    uint32_t frames;
    // 先頭サンプルの単調プレゼンテーションタイムスタンプ（ns）。
    int64_t pts_ns;
    // ストリーム層が付与する単調増加のシーケンス番号。
    uint64_t seq;
    // チャンクの状態フラグ（ChunkFlags のビット）。
    uint32_t flags;
    // このチャンクが届くまでにドロップされたチャンク数。
    uint32_t dropped_before;
    // 全サンプル絶対値の最大（線形振幅）。
    float peak;
    // 全サンプルの二乗平均平方根（線形）。
    float rms;
    // このチャンクで VAD が確定したイベント配列。VAD 無効時・イベント無しのときは
    // NULL（`vad_events_len = 0`）。非 NULL のときは `flexaudio_chunk_free` が
    // `data` と一緒に解放する。
    struct FlexVadEvent *vad_events;
    // `vad_events` の要素数。VAD 無効時・イベント無しでは 0。
    uintptr_t vad_events_len;
} FlexChunk;

// 取得した 1 イベント。`flexaudio_poll_event` が埋める。
//
// `Error` のときはメッセージが `flexaudio_last_error` に入る。
typedef struct FlexEvent {
    // イベント種別。
    enum FlexEventKind kind;
    // `ChunkDropped` のドロップ数。それ以外では 0。
    int64_t count;
} FlexEvent;

// 列挙された 1 デバイスの情報（[`flexaudio::DeviceInfo`] に対応）。
//
// `id` / `name` は flexaudio 所有の UTF-8 NUL 終端文字列。配列ごと
// `flexaudio_devices_free` で解放する（C の free は使わない）。
typedef struct FlexDeviceInfo {
    // 安定 ID（`flexaudio_devices_free` で解放）。
    char *id;
    // 人間向け表示名（`flexaudio_devices_free` で解放）。
    char *name;
    // このデバイスをキャプチャするときのソース種別。
    enum FlexSourceKind source_kind;
    // ネイティブ（既定）サンプルレート（Hz）。
    uint32_t sample_rate;
    // ネイティブ（既定）チャンネル数。
    uint16_t channels;
    // ループバック（システム出力の monitor）なら true。
    bool is_loopback;
    // OS の既定デバイスなら true。
    bool is_default;
} FlexDeviceInfo;

// 取得した 1 つのデバイスイベント。`flexaudio_watcher_poll` が埋める。
//
// フィールドの有効範囲は `kind` による:
// - `Added`: `id`/`name` と `source_kind`/`sample_rate`/`channels`/`is_loopback`/`is_default`
//   がすべて埋まる（追加されたデバイスの完全な情報）。
// - `Removed`: `id` のみ（`name` は NULL・数値は 0）。
// - `DefaultChanged`: `id` と `source_kind`（既定が切り替わった側）のみ。
//
// `id`/`name` は flexaudio 所有の UTF-8 NUL 終端文字列で、[`flexaudio_device_event_free`]
// で解放する（C の free は使わない）。
typedef struct FlexDeviceEvent {
    // イベント種別。
    enum FlexDeviceEventKind kind;
    // 安定 ID（`Added`/`Removed`/`DefaultChanged` で有効・`flexaudio_device_event_free`
    // で解放）。`Unknown` では NULL。
    char *id;
    // 表示名（`Added` のみ・`flexaudio_device_event_free` で解放）。他では NULL。
    char *name;
    // `Added` では当該デバイスのソース種別、`DefaultChanged` では既定が切り替わった側
    // （`Mic` = 既定 source / `System` = 既定 sink）。他では未使用（`Mic`）。
    enum FlexSourceKind source_kind;
    // ネイティブサンプルレート（`Added` のみ・他では 0）。
    uint32_t sample_rate;
    // ネイティブチャンネル数（`Added` のみ・他では 0）。
    uint16_t channels;
    // ループバック（`Added` のみ）。
    bool is_loopback;
    // OS の既定デバイス（`Added` のみ）。
    bool is_default;
} FlexDeviceEvent;

// 構成からストリームを開く（まだ start しない）。失敗で NULL を返し last_error をセット。
//
// `config.denoise` / `config.has_vad` が有効なら、対応するアドオン（ノイズ抑制 / VAD）を
// ここで構築してストリームに同居させる（`poll_chunk` が denoise → VAD の順で通す）。
// denoise 有効時は出力レートが 48000 でなければ失敗する（NULL + last_error。RNNoise は
// 48kHz 固定）。返ったハンドルは `flexaudio_free` で解放する。
//
// # Safety
// `config` は有効な `FlexConfig` を指していなければならない（NULL は失敗扱い）。
struct FlexStream *flexaudio_open(const struct FlexConfig *config);

// ストリームを停止してから解放する。NULL 安全。
//
// # Safety
// `s` は `flexaudio_open` が返したハンドル（または NULL）でなければならない。
// 解放後の `s` を使ってはならない。
void flexaudio_free(struct FlexStream *s);

// キャプチャを開始する。
//
// # Safety
// `s` は有効なハンドルでなければならない（NULL は InvalidArg）。
int32_t flexaudio_start(struct FlexStream *s);

// キャプチャを停止する。
//
// # Safety
// `s` は有効なハンドルでなければならない（NULL は InvalidArg）。
int32_t flexaudio_stop(struct FlexStream *s);

// 配信を一時停止する（デバイスは動かしたまま）。
//
// # Safety
// `s` は有効なハンドルでなければならない（NULL は InvalidArg）。
int32_t flexaudio_pause(struct FlexStream *s);

// 一時停止を解除して配信を再開する。
//
// # Safety
// `s` は有効なハンドルでなければならない（NULL は InvalidArg）。
int32_t flexaudio_resume(struct FlexStream *s);

// 一時停止中なら true を返す。NULL や panic では false。
//
// # Safety
// `s` は有効なハンドル（または NULL）でなければならない。
bool flexaudio_is_paused(const struct FlexStream *s);

// 入力ゲイン（線形倍率）を変更する。1.0 でそのまま、2.0 で約 +6dB、0.0 で無音。
// 録音中いつでも呼べて、次のチャンクから効く（20ms 粒度）。乗算後のサンプルは
// ±1.0 にクランプされる。有限かつ 0 以上でなければ FLEX_INVALID_ARG。
//
// # Safety
// `s` は有効なハンドルでなければならない（NULL は InvalidArg）。
int32_t flexaudio_set_gain(struct FlexStream *s,
                           float gain);

// 現在の入力ゲイン（線形倍率）を返す。NULL や panic では 1.0。
//
// # Safety
// `s` は有効なハンドル（または NULL）でなければならない。
float flexaudio_gain(const struct FlexStream *s);

// 現在の backend のネイティブフォーマット `(sample_rate, channels)` を `sr`/`ch` に書く。
//
// open 時に backend から取得した値で、`flexaudio_switch_source` で更新される。表示・診断用
// （出力フォーマットは `config` で指定した値）。戻り 0 = 成功 / 負 = エラー。
//
// # Safety
// `s` は有効なハンドル、`sr`/`ch` は有効な書き込み先でなければならない（NULL は InvalidArg）。
int32_t flexaudio_native_format(const struct FlexStream *s,
                                uint32_t *sr,
                                uint16_t *ch);

// これまでにチャンクリングが DROP_OLDEST で捨てた累計チャンク数を返す。NULL や panic では 0。
//
// # Safety
// `s` は有効なハンドル（または NULL）でなければならない。
uint64_t flexaudio_dropped_chunks(const struct FlexStream *s);

// チャンクを 1 つ取り出して `out` を埋める。
//
// 戻り 1 = 取得して `out` を埋めた / 0 = 今は無し / 負 = エラー。`out.data` は
// flexaudio 所有で、使い終わったら `flexaudio_chunk_free` で解放する。
//
// アドオンが有効なら、返す前にチャンクを denoise → VAD の順で通す。VAD 有効時は
// 確定したイベントが `out.vad_events`（要素数 `out.vad_events_len`）に入り、これも
// `flexaudio_chunk_free` が `data` と一緒に解放する（無効時・イベント無しは NULL/0）。
//
// # Safety
// `s` は有効なハンドル、`out` は有効な `FlexChunk` の書き込み先でなければならない。
int32_t flexaudio_poll_chunk(struct FlexStream *s,
                             struct FlexChunk *out);

// `flexaudio_poll_chunk` が埋めた `data` を解放し、`data=NULL` / `len=0` にする。
// NULL・二重解放とも安全。
//
// # Safety
// `chunk` は `flexaudio_poll_chunk` が埋めた `FlexChunk`（または NULL）を指して
// いなければならない。
void flexaudio_chunk_free(struct FlexChunk *chunk);

// イベントを 1 つ取り出して `out` を埋める。
//
// 戻り 1 = 取得 / 0 = 今は無し / 負 = エラー。`Error` イベントのときは
// `out.kind = Error` にし、メッセージを last_error に入れる。
//
// # Safety
// `s` は有効なハンドル、`out` は有効な `FlexEvent` の書き込み先でなければならない。
int32_t flexaudio_poll_event(struct FlexStream *s,
                             struct FlexEvent *out);

// 録音を止めずに入力ソースをホットスワップする。`config.gain` は無視される
// （ゲインはストリームの状態。変更は `flexaudio_set_gain`）。同様に `config.denoise` /
// `config.has_vad` / `config.vad` も無視される（アドオンは open 時に確定したものを保つ。
// 出力フォーマットは switch_source で変えられないので、48k 制約や VAD 設定は不変）。
//
// # Safety
// `s` は有効なハンドル、`config` は有効な `FlexConfig` を指していなければならない。
int32_t flexaudio_switch_source(struct FlexStream *s,
                                const struct FlexConfig *config);

// 利用可能なデバイスを列挙し、配列を確保して `out_array` / `out_count` にセットする。
//
// 成功で 0。確保した配列は `flexaudio_devices_free` で解放する。ヘッドレス環境では
// 0 件（`out_array=NULL` / `out_count=0`）でも成功扱い。
//
// # Safety
// `out_array` / `out_count` は有効な書き込み先でなければならない（NULL は InvalidArg）。
int32_t flexaudio_devices(struct FlexDeviceInfo **out_array,
                          uintptr_t *out_count);

// `flexaudio_devices` が確保した配列と各 `id`/`name` を解放する。NULL 安全。
//
// # Safety
// `arr`/`count` は `flexaudio_devices` が返したもの（または NULL/0）でなければならない。
void flexaudio_devices_free(struct FlexDeviceInfo *arr,
                            uintptr_t count);

// 現在のスレッドの直近エラーメッセージを返す。
//
// 同一スレッドで次に last_error を更新する FFI 呼び出しまで有効。エラーが無ければ
// NULL。返るポインタは flexaudio 所有で、C 側で free してはならない。
const char *flexaudio_last_error(void);

// チャンネル数（1 = mono / 2 = stereo interleaved）を指定して denoiser を構築する。
//
// `channels` が 1..=2 以外なら NULL を返し last_error をセット。返ったハンドルは
// `flexaudio_denoise_free` で解放する。48kHz 前提はモジュールの説明を参照。
struct FlexDenoiser *flexaudio_denoise_new(uint16_t channels);

// interleaved f32（48kHz・±1.0 正規化）を **インプレース**でノイズ抑制する。
//
// `len` はチャンネル数の倍数であること（倍数でなければ InvalidArg）。`len=0` は no-op。
// 出力は入力を 480 サンプル/ch 遅らせた列で、ストリーム先頭のその分は無音になる。
// 戻り 0 = 成功 / 負 = エラー。
//
// # Safety
// `d` は有効なハンドル、`samples` は `len` 要素の有効な可変配列（`len=0` なら NULL 可）で
// なければならない。
int32_t flexaudio_denoise_process(struct FlexDenoiser *d,
                                  float *samples,
                                  uintptr_t len);

// RNN 状態・持ち越しバッファ・遅延線を初期化する（生成直後と同じ状態に戻す）。
//
// # Safety
// `d` は有効なハンドルでなければならない（NULL は InvalidArg）。
int32_t flexaudio_denoise_reset(struct FlexDenoiser *d);

// denoiser ハンドルを解放する。NULL 安全。
//
// # Safety
// `d` は `flexaudio_denoise_new` が返したハンドル（または NULL）でなければならない。
// 解放後の `d` を使ってはならない。
void flexaudio_denoise_free(struct FlexDenoiser *d);

// `path` に FLAC 書き出しを開く。`split_seconds = 0` で単一ファイル、1 以上で
// `split_seconds` 秒ごとに `name-001.flac` 連番へローテーションする。
//
// 失敗（NULL / 不正な UTF-8 パス / 非対応の `sr`・`ch`）で NULL を返し last_error を
// セットする。`ch` は 1..=2、`sr` は 1..=96000 Hz。返ったハンドルは
// `flexaudio_flac_free` で解放する（`flexaudio_flac_finalize` を呼ばずに free しても
// ベストエフォートで閉じる）。
//
// # Safety
// `path` は有効な NUL 終端 C 文字列（UTF-8）を指していなければならない（NULL は失敗扱い）。
struct FlexFlac *flexaudio_flac_create(const char *path,
                                       uint32_t sr,
                                       uint16_t ch,
                                       uint32_t split_seconds);

// interleaved f32（長さ = フレーム数 × チャンネル数）を追記する。
//
// `len` はチャンネル数の倍数であること（倍数でなければ InvalidArg）。`len=0` は no-op。
// finalize 済みのハンドルへの write は [`FLEX_INVALID_STATE`](code::FLEX_INVALID_STATE)。
// 戻り 0 = 成功 / 負 = エラー。
//
// # Safety
// `f` は有効なハンドル、`samples` は `len` 要素の有効な配列（`len=0` なら NULL 可）で
// なければならない。
int32_t flexaudio_flac_write(struct FlexFlac *f,
                             const float *samples,
                             uintptr_t len);

// 端数を書き切り、現在のファイルを確定して閉じる。以後の write は InvalidState。
//
// 二重 finalize は安全（no-op で 0 を返す）。戻り 0 = 成功 / 負 = エラー。
//
// # Safety
// `f` は有効なハンドルでなければならない（NULL は InvalidArg）。
int32_t flexaudio_flac_finalize(struct FlexFlac *f);

// FLAC ハンドルを解放する。NULL 安全。
//
// finalize せずに free した場合も、内部の [`FlacWriter`] が drop 時にベストエフォートで
// 端数書き出しとヘッダ確定を試みる（エラーは握り潰す。確実に検知したいなら先に
// `flexaudio_flac_finalize` を呼ぶ）。
//
// # Safety
// `f` は `flexaudio_flac_create` が返したハンドル（または NULL）でなければならない。
// 解放後の `f` を使ってはならない。
void flexaudio_flac_free(struct FlexFlac *f);

// 設定から VAD を構築する。`config` が NULL なら既定設定（silero 準拠）。
//
// 失敗（モデルのロード失敗・不正な sample_rate 等）で NULL を返し last_error をセット。
// 返ったハンドルは `flexaudio_vad_free` で解放する。
//
// # Safety
// `config` は NULL か、有効な `FlexVadConfig` を指していなければならない。
struct FlexVad *flexaudio_vad_new(const struct FlexVadConfig *config);

// 任意フォーマット（`in_rate` / `in_ch` の interleaved f32）のサンプルを VAD に通し、
// 確定したイベント配列を確保して `out` / `out_len` にセットする。
//
// 内部で mono 化・VAD レートへのリサンプルをしてから処理する（[`flexaudio_vad::Vad::process_pcm`]）。
// イベントが無ければ `out=NULL` / `out_len=0`。確保した配列は
// `flexaudio_vad_events_free` で解放する。戻り 0 = 成功 / 負 = エラー。
//
// # Safety
// `v` は有効なハンドル、`samples` は `len` 要素の有効な配列（`len=0` なら NULL 可）、
// `out` / `out_len` は有効な書き込み先でなければならない。
int32_t flexaudio_vad_process(struct FlexVad *v,
                              const float *samples,
                              uintptr_t len,
                              uint32_t in_rate,
                              uint16_t in_ch,
                              struct FlexVadEvent **out,
                              uintptr_t *out_len);

// `flexaudio_vad_process` が確保したイベント配列を解放する。NULL / 0 は安全。
//
// # Safety
// `events`/`len` は `flexaudio_vad_process` が返したもの（または NULL/0）でなければならない。
void flexaudio_vad_events_free(struct FlexVadEvent *events,
                               uintptr_t len);

// VAD の状態（内部 state / context / 端数バッファ / リサンプラ）を初期化する。
//
// # Safety
// `v` は有効なハンドルでなければならない（NULL は InvalidArg）。
int32_t flexaudio_vad_reset(struct FlexVad *v);

// VAD ハンドルを解放する。NULL 安全。
//
// # Safety
// `v` は `flexaudio_vad_new` が返したハンドル（または NULL）でなければならない。
// 解放後の `v` を使ってはならない。
void flexaudio_vad_free(struct FlexVad *v);

// デバイスの着脱・既定変更の監視を開始し、ウォッチャハンドルを返す。
//
// Linux は PipeWire レジストリを永続監視する。PipeWire 不在・非対応 OS では no-op へ
// 縮退して有効なハンドルを返す（着脱が来ないだけ・poll は常に 0）。失敗時のみ NULL +
// last_error。返ったハンドルは `flexaudio_watcher_free` で解放する。
struct FlexWatcher *flexaudio_watch_devices(void);

// デバイスイベントを 1 つ取り出して `out` を埋める（非ブロッキング）。
//
// 戻り 1 = 取得して `out` を埋めた / 0 = 今は無し / 負 = エラー。埋めた `out` は使い
// 終わったら `flexaudio_device_event_free` で解放する。
//
// # Safety
// `w` は有効なハンドル、`out` は有効な `FlexDeviceEvent` の書き込み先でなければならない。
int32_t flexaudio_watcher_poll(struct FlexWatcher *w,
                               struct FlexDeviceEvent *out);

// `flexaudio_watcher_poll` が埋めた `id`/`name` を解放し、NULL にする。NULL・二重解放
// とも安全。
//
// # Safety
// `ev` は `flexaudio_watcher_poll` が埋めた `FlexDeviceEvent`（または NULL）を指して
// いなければならない。
void flexaudio_device_event_free(struct FlexDeviceEvent *ev);

// ウォッチャを停止して解放する。NULL 安全。
//
// # Safety
// `w` は `flexaudio_watch_devices` が返したハンドル（または NULL）でなければならない。
// 解放後の `w` を使ってはならない。
void flexaudio_watcher_free(struct FlexWatcher *w);

#endif  /* FLEXAUDIO_H */
