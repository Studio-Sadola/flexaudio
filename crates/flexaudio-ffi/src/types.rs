//! C ABI で渡す `#[repr(C)]` 型と opaque ハンドル。
//!
//! cbindgen がこれらをそのまま `flexaudio.h` の struct / enum に写す。レイアウトは
//! C 側と一致させる必要があるので、フィールドの型・順序を勝手に変えないこと。

use std::os::raw::c_char;

use flexaudio_denoise::Denoiser;
use flexaudio_vad::Vad;

/// 録音するオーディオソースの種別（[`flexaudio::SourceKind`] に対応）。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlexSourceKind {
    /// マイク入力。
    Mic = 0,
    /// システム出力全体のループバック。
    System = 1,
    /// 特定プロセスの出力ループバック。
    Process = 2,
    /// マイクとシステム音声を 1 本に合成して録る。
    Mix = 3,
}

/// process ソースで対象 PID を含めるか除くか（[`flexaudio::ProcessMode`] に対応）。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlexProcessMode {
    /// 対象 PID（そのプロセスツリー）だけを録る。
    Include = 0,
    /// 対象 PID 以外の全システム音を録る。
    Exclude = 1,
}

/// VAD（発話区間検出）の設定。`FlexConfig::vad` と `flexaudio_vad_new` に渡す。
///
/// 各値は番兵 0 で既定を表す（[`flexaudio_vad::VadConfig`] の既定値に写す）。
/// `threshold` 0 → 0.5、`neg_threshold` 0 → silero 式 `max(threshold-0.15, 0.01)`、
/// `min_speech_ms` 0 → 250、`min_silence_ms` 0 → 100、`speech_pad_ms` 0 → 30、
/// `sample_rate` 0 → 16000。`max_speech_ms` は 0 がそのまま「無制限」（既定）を意味する。
///
/// [`flexaudio_vad_new`]: crate::flexaudio_vad_new
#[repr(C)]
pub struct FlexVadConfig {
    /// 発話開始とみなす確率しきい値（>=）。0 なら 0.5。
    pub threshold: f32,
    /// 無音開始とみなす負側しきい値（<）。0 なら silero 式で自動決定。
    pub neg_threshold: f32,
    /// 採用する発話の最小長（ms）。これ未満のセグメントは破棄。0 なら 250。
    pub min_speech_ms: u32,
    /// 発話終了の確定に必要な無音長（ms）。0 なら 100。
    pub min_silence_ms: u32,
    /// セグメント境界を前後に広げるパディング（ms）。0 なら 30。
    pub speech_pad_ms: u32,
    /// 1 セグメントの最大長（ms）。0 は無制限（既定）。超過時は強制分割。
    pub max_speech_ms: u32,
    /// サンプルレート（8000 または 16000）。0 なら 16000。
    pub sample_rate: u32,
}

/// VAD が確定した 1 イベント。`flexaudio_vad_process` の出力配列と `FlexChunk::vad_events`
/// に入る。
///
/// `at_sample` は VAD 内部レート（`sample_rate`＝8000/16000）のサンプル基準で、入力
/// サンプル基準ではない（[`flexaudio_vad::VadEvent`] と同じ）。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlexVadEvent {
    /// 種別。0 = 発話開始（SpeechStart）、1 = 発話終了（SpeechEnd）。
    pub kind: i32,
    /// イベントのサンプル位置（VAD 内部レート基準・開始は含む/終了は排他）。
    pub at_sample: i64,
}

/// ストリームを開くための構成。`flexaudio_open` / `flexaudio_switch_source` に渡す。
///
/// 文字列・任意値は番兵で「未指定」を表す（`device_id` が NULL なら既定デバイス、
/// `process_id` が 0 ならなし、`output_rate`/`output_channels`/`chunk_ms` が 0 なら既定）。
#[repr(C)]
pub struct FlexConfig {
    /// ソース種別。
    pub kind: FlexSourceKind,
    /// 選ぶデバイスの ID（UTF-8, NUL 終端）。NULL なら既定デバイス。
    pub device_id: *const c_char,
    /// process ソースの対象 PID。0 ならなし（process では start 時にエラーになりうる）。
    pub process_id: u32,
    /// 対象 PID を含めるか除くか（process ソースのみ）。
    pub mode: FlexProcessMode,
    /// 自ホストの再生音をシステム音から除くか（system ソースのみ。mix では system 側
    /// に適用）。
    pub exclude_self: bool,
    /// 出力サンプルレート（Hz）。0 なら 48000。
    pub output_rate: u32,
    /// 出力チャンネル数。0 なら 2。
    pub output_channels: u16,
    /// チャンク長（ミリ秒）。0 なら 20。
    pub chunk_ms: u32,
    /// 開始時の入力ゲイン（線形倍率）。0 なら 1.0（既定）。実行時のミュートは
    /// `flexaudio_set_gain(s, 0.0)` を使う。
    pub gain: f32,
    /// mix の mic 側で選ぶ入力デバイスの ID（UTF-8, NUL 終端・mix 専用）。
    /// NULL なら既定入力。
    pub mix_mic_device_id: *const c_char,
    /// mix の system 側で選ぶ出力エンドポイントの ID（UTF-8, NUL 終端・mix 専用）。
    /// NULL なら既定出力。
    pub mix_system_device_id: *const c_char,
    /// mix の mic 側の合成前倍率（線形・mix 専用）。0 なら 1.0（既定）。
    /// 合成後にグローバル `gain` が掛かる。
    pub mix_mic_gain: f32,
    /// mix の system 側の合成前倍率（線形・mix 専用）。0 なら 1.0（既定）。
    pub mix_system_gain: f32,
    /// ノイズ抑制（RNNoise）をストリームに挟むか。`true` で有効。有効時は出力レートが
    /// 48000 でなければ `flexaudio_open` が失敗する（NULL + last_error）。denoise は
    /// `poll_chunk` が返す直前に data をインプレース処理する（VAD より前段）。
    pub denoise: bool,
    /// VAD（発話区間検出）をストリームに挟むか。`true` で `vad` の設定に従い、poll した
    /// 各チャンクを VAD に通して `FlexChunk::vad_events` を埋める。
    pub has_vad: bool,
    /// VAD の設定（`has_vad` が `true` のときだけ使う。`false` なら無視）。
    pub vad: FlexVadConfig,
}

/// 取得した 1 チャンクのオーディオデータ。`flexaudio_poll_chunk` が埋める。
///
/// `data` は flexaudio 所有の interleaved f32 で、長さは `len`（= `frames * channels`）。
/// 使い終わったら必ず `flexaudio_chunk_free` で解放する（C の free は使わない）。
#[repr(C)]
pub struct FlexChunk {
    /// interleaved f32 サンプルへのポインタ。`flexaudio_chunk_free` で解放する。
    pub data: *mut f32,
    /// `data` の要素数（= `frames * channels`）。
    pub len: usize,
    /// チャンク内のフレーム数。
    pub frames: u32,
    /// 先頭サンプルの単調プレゼンテーションタイムスタンプ（ns）。
    pub pts_ns: i64,
    /// ストリーム層が付与する単調増加のシーケンス番号。
    pub seq: u64,
    /// チャンクの状態フラグ（ChunkFlags のビット）。
    pub flags: u32,
    /// このチャンクが届くまでにドロップされたチャンク数。
    pub dropped_before: u32,
    /// 全サンプル絶対値の最大（線形振幅）。
    pub peak: f32,
    /// 全サンプルの二乗平均平方根（線形）。
    pub rms: f32,
    /// このチャンクで VAD が確定したイベント配列。VAD 無効時・イベント無しのときは
    /// NULL（`vad_events_len = 0`）。非 NULL のときは `flexaudio_chunk_free` が
    /// `data` と一緒に解放する。
    pub vad_events: *mut FlexVadEvent,
    /// `vad_events` の要素数。VAD 無効時・イベント無しでは 0。
    pub vad_events_len: usize,
}

/// ストリームイベントの種別（[`flexaudio::Event`] に対応）。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlexEventKind {
    /// チャンクリング満杯によりチャンクがドロップされた（個数は `FlexEvent::count`）。
    ChunkDropped = 0,
    /// データ到着が途絶し、ストリームが失速した。
    Stalled = 1,
    /// 失速後にデータ到着が復帰した。
    Recovered = 2,
    /// 必要な権限が拒否された。
    PermissionDenied = 3,
    /// キャプチャデバイスが失われた。
    DeviceLost = 4,
    /// その他のバックエンドエラー（メッセージは `flexaudio_last_error` で取る）。
    Error = 5,
    /// 既知のどれにも当たらないイベント（将来のバリアント追加に備える）。
    Unknown = 6,
}

/// 取得した 1 イベント。`flexaudio_poll_event` が埋める。
///
/// `Error` のときはメッセージが `flexaudio_last_error` に入る。
#[repr(C)]
pub struct FlexEvent {
    /// イベント種別。
    pub kind: FlexEventKind,
    /// `ChunkDropped` のドロップ数。それ以外では 0。
    pub count: i64,
}

/// 列挙された 1 デバイスの情報（[`flexaudio::DeviceInfo`] に対応）。
///
/// `id` / `name` は flexaudio 所有の UTF-8 NUL 終端文字列。配列ごと
/// `flexaudio_devices_free` で解放する（C の free は使わない）。
#[repr(C)]
pub struct FlexDeviceInfo {
    /// 安定 ID（`flexaudio_devices_free` で解放）。
    pub id: *mut c_char,
    /// 人間向け表示名（`flexaudio_devices_free` で解放）。
    pub name: *mut c_char,
    /// このデバイスをキャプチャするときのソース種別。
    pub source_kind: FlexSourceKind,
    /// ネイティブ（既定）サンプルレート（Hz）。
    pub sample_rate: u32,
    /// ネイティブ（既定）チャンネル数。
    pub channels: u16,
    /// ループバック（システム出力の monitor）なら true。
    pub is_loopback: bool,
    /// OS の既定デバイスなら true。
    pub is_default: bool,
}

/// 録音ストリームの不透明ハンドル。中身は [`flexaudio::Stream`] と、有効時に同居する
/// アドオン（denoise / VAD）で、C 側はポインタだけを持つ。`flexaudio_open` で作り
/// `flexaudio_free` で解放する。
///
/// アドオンはストリームの状態としてここに閉じ込める（薄いラッパ）。`poll_chunk` が
/// 返す前に denoise → VAD の順で通す。`flexaudio_switch_source` はソースだけを差し替え、
/// アドオンは open 時の構成のまま保つ（gain と同じ扱い）。
pub struct FlexStream {
    pub(crate) inner: flexaudio::Stream,
    /// 有効時のノイズ抑制器（48k 前提。open 時に構築）。無効なら `None`。
    pub(crate) denoiser: Option<Denoiser>,
    /// 有効時の VAD（open 時に構築）。無効なら `None`。
    pub(crate) vad: Option<Vad>,
}
