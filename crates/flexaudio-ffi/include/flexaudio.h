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

// 録音するオーディオソースの種別（[`flexaudio::SourceKind`] に対応）。
typedef enum FlexSourceKind {
    // マイク入力。
    FLEX_SOURCE_KIND_MIC = 0,
    // システム出力全体のループバック。
    FLEX_SOURCE_KIND_SYSTEM = 1,
    // 特定プロセスの出力ループバック。
    FLEX_SOURCE_KIND_PROCESS = 2,
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

// 録音ストリームの不透明ハンドル。中身は [`flexaudio::Stream`] で、C 側はポインタ
// だけを持つ。`flexaudio_open` で作り `flexaudio_free` で解放する。
typedef struct FlexStream FlexStream;

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
    // 自ホストの再生音をシステム音から除くか（system ソースのみ）。
    bool exclude_self;
    // 出力サンプルレート（Hz）。0 なら 48000。
    uint32_t output_rate;
    // 出力チャンネル数。0 なら 2。
    uint16_t output_channels;
    // チャンク長（ミリ秒）。0 なら 20。
    uint32_t chunk_ms;
} FlexConfig;

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

// 構成からストリームを開く（まだ start しない）。失敗で NULL を返し last_error をセット。
//
// 返ったハンドルは `flexaudio_free` で解放する。
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

// チャンクを 1 つ取り出して `out` を埋める。
//
// 戻り 1 = 取得して `out` を埋めた / 0 = 今は無し / 負 = エラー。`out.data` は
// flexaudio 所有で、使い終わったら `flexaudio_chunk_free` で解放する。
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

// 録音を止めずに入力ソースをホットスワップする。
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

#endif  /* FLEXAUDIO_H */
