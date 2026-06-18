//! C ABI で渡す `#[repr(C)]` 型と opaque ハンドル。
//!
//! cbindgen がこれらをそのまま `flexaudio.h` の struct / enum に写す。レイアウトは
//! C 側と一致させる必要があるので、フィールドの型・順序を勝手に変えないこと。

use std::os::raw::c_char;

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
    /// 自ホストの再生音をシステム音から除くか（system ソースのみ）。
    pub exclude_self: bool,
    /// 出力サンプルレート（Hz）。0 なら 48000。
    pub output_rate: u32,
    /// 出力チャンネル数。0 なら 2。
    pub output_channels: u16,
    /// チャンク長（ミリ秒）。0 なら 20。
    pub chunk_ms: u32,
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

/// 録音ストリームの不透明ハンドル。中身は [`flexaudio::Stream`] で、C 側はポインタ
/// だけを持つ。`flexaudio_open` で作り `flexaudio_free` で解放する。
pub struct FlexStream {
    pub(crate) inner: flexaudio::Stream,
}
