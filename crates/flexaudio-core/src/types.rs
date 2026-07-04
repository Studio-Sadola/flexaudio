//! 共有型: 内部正規形の定数 / [`AudioChunk`] / [`ChunkFlags`] / [`SourceKind`] /
//! [`OutputFormat`] / [`StreamConfig`] / [`Event`] / [`Error`]。
//!
//! 内部正規形は interleaved `f32` / 48000 Hz / ステレオ 2ch / 20ms = 960
//! frames per chunk。
//!
//! 出力フォーマットは [`OutputFormat`] で指定する（既定 `{48000, 2}`）。
//! Normalizer 第 2 段が内部正規形からそのレート/チャンネルへ再変換する。出力
//! チャンクは時間ベースで 20ms なので、レートに応じて [`AudioChunk::frames`] が
//! 変わる（48k=960 / 16k=320 / 8k=160）。既定 `{48000, 2}` のときは第 2 段が
//! パススルーになり、内部正規形がそのまま出力される。

use bitflags::bitflags;

/// 内部正規形のサンプルレート（Hz）。全ストリームは一旦このレートへ正規化される。
pub const SAMPLE_RATE: u32 = 48_000;

/// 内部正規形のチャンネル数。常にステレオ（2ch interleaved）。
pub const CHANNELS: u16 = 2;

bitflags! {
    /// 1 つの [`AudioChunk`] に付随する状態フラグ。
    ///
    /// FFI 越しに安定したビット幅で渡すため `u32` 背景表現。
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
    pub struct ChunkFlags: u32 {
        /// このチャンク直前にストリームの不連続（ドロップ / ギャップ）があった。
        const DISCONTINUITY = 0b0000_0001;
        /// デバイス喪失などからの自動復帰後の最初のチャンク。
        const RECOVERED = 0b0000_0010;
        /// 無音生成チャンク（ギャップ補填等で合成された無音）。
        const SILENCE = 0b0000_0100;
    }
}

/// 正規化済み 20ms オーディオチャンク。
///
/// `data` は interleaved `f32`（出力チャンネル順）で、長さは
/// `frames * output.channels`。チャンクは時間ベースで 20ms なので、出力レートに
/// 応じて `frames` が変わる（48k=960 / 16k=320 / 8k=160）。既定の出力 `{48000, 2}`
/// では `frames == 960`（1920 サンプル）。
#[derive(Debug, Clone, PartialEq)]
pub struct AudioChunk {
    /// interleaved `f32` サンプル。長さ = `frames * output.channels`。
    pub data: Vec<f32>,
    /// チャンク内のフレーム数（1 フレーム = 全出力チャンネル 1 サンプル組）。
    pub frames: usize,
    /// 先頭サンプルの正規化済み単調プレゼンテーションタイムスタンプ（ns）。
    pub pts_ns: i64,
    /// ストリーム層が単調増加で付与するシーケンス番号。
    pub seq: u64,
    /// このチャンクの状態フラグ。
    pub flags: ChunkFlags,
    /// このチャンクが届くまでに（直前に）ドロップされたチャンク数。
    pub dropped_before: u32,
    /// このチャンクの最終 `data`（出力フォーマット）における全サンプル絶対値の最大。
    /// 線形振幅（通常 `0.0..=1.0`）。
    pub peak: f32,
    /// このチャンクの最終 `data`（出力フォーマット）における二乗平均平方根（線形）。
    pub rms: f32,
}

/// `devices()` が 1 デバイスにつき返す情報。
///
/// 全 OS バックエンド共通の形。マイク入力（[`SourceKind::Mic`]）とシステム音声出力
/// （[`SourceKind::SystemLoopback`]）を 1 つのリストにまとめて返す。
///
/// `id` は再接続で index が変わらないよう、取得できる範囲で安定なキーを使う。
/// cpal（マイク, 全 OS）は永続 ID を持たないのでデバイス名を id にし、PipeWire
/// （Linux）は `node.name` を id にする（表示名 `name` には `node.description` を使う）。
///
/// 同じマシン・同じ構成で列挙し直せば同じ `id` が返る。別マシンや OS をまたいだ
/// 一意性は保証しない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    /// 安定 ID。[`StreamConfig::device_id`] に渡せるキー（cpal=デバイス名 /
    /// PipeWire=`node.name`）。
    pub id: String,
    /// 人間向け表示名（PipeWire は `node.description` 優先、無ければ `node.name`）。
    pub name: String,
    /// このデバイスをキャプチャするときのソース種別。
    pub source_kind: SourceKind,
    /// デバイスのネイティブ（既定）サンプルレート（Hz）。不明時は妥当な既定値。
    pub sample_rate: u32,
    /// デバイスのネイティブ（既定）チャンネル数。不明時は妥当な既定値。
    pub channels: u16,
    /// ループバック（システム出力の monitor）なら `true`、録音デバイス（マイク）なら
    /// `false`。
    pub is_loopback: bool,
    /// OS の既定デバイス（既定入力 / 既定出力 sink）なら `true`。
    pub is_default: bool,
}

/// デバイスの着脱・既定変更を表すホットプラグイベント。
///
/// capture stream 単位の [`Event`] とは別系統で、`DeviceWatcher`（facade 層）が
/// デバイス単位の事象として配信する。`poll_event` で取る。着脱は低頻度だが
/// 取りこぼせないので、配信キューは上限を設けない。
///
/// 将来バリアントを足せるよう `#[non_exhaustive]`（外部の match は `_ =>` が要る）。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DeviceEvent {
    /// デバイスが追加された（接続・新規ノード出現）。
    Added(DeviceInfo),
    /// デバイスが取り外された（切断・ノード消滅）。
    /// PipeWire の `global_remove` は数値 id しか渡さないため、安定 ID（`node.name`）のみ返す。
    Removed {
        /// 取り外されたデバイスの安定 ID（= [`DeviceInfo::id`] = PipeWire の `node.name`）。
        id: String,
    },
    /// OS 既定デバイスが変わった（既定 sink / source の切替）。
    DefaultChanged {
        /// 既定が切り替わったソース種別（`Mic` = 既定 source / `SystemLoopback` = 既定 sink）。
        kind: SourceKind,
        /// 新しい既定デバイスの安定 ID（= `node.name`）。
        id: String,
    },
}

/// キャプチャするオーディオソースの種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SourceKind {
    /// マイク入力（録音デバイス）。
    Mic,
    /// システム出力全体のループバック（既定スピーカーのミックス）。
    SystemLoopback,
    /// 特定プロセスの出力ループバック。
    ProcessLoopback,
    /// マイクとシステム音声を 1 本に合成して録る（mic + system のミックス）。
    Mix,
}

/// [`SourceKind::ProcessLoopback`] で対象 PID をどう扱うか（process ソース専用）。
///
/// - [`Include`](ProcessMode::Include)（既定）: 対象 `target_pid`（そのプロセス
///   ツリー）だけを録る。
/// - [`Exclude`](ProcessMode::Exclude): 対象 `target_pid`（そのプロセスツリー）以外
///   の全システム音を録る（`target_pid` が必須）。
///
/// process ソースはこの `mode` だけを見て [`StreamConfig::exclude_self`] を無視し、
/// system ソースは `exclude_self` だけを見て `mode` を無視する（mic は両方無関係）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProcessMode {
    /// 対象 `target_pid`（そのプロセスツリー）だけを録る（既定）。
    #[default]
    Include,
    /// 対象 `target_pid`（そのプロセスツリー）以外の全システム音を録る。
    /// `target_pid` が必須（無ければ facade が [`Error::InvalidArg`]）。
    Exclude,
}

/// 出力チャンクのフォーマット（サンプルレートとチャンネル数）。
///
/// Normalizer 第 2 段が内部正規形 48k/stereo からこのフォーマットへ再変換する。
/// 既定は内部正規形と同じ `{sample_rate: 48000, channels: 2}` で、このとき第 2 段は
/// パススルーになる。
///
/// `sample_rate` はダウン/アップサンプル先（rubato でアンチエイリアス込み）。
/// `channels` は 1（mono）または 2（stereo）で、stereo→mono は L/R 平均、
/// mono→stereo は複製。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputFormat {
    /// 出力サンプルレート（Hz）。
    pub sample_rate: u32,
    /// 出力チャンネル数（1 = mono / 2 = stereo）。
    pub channels: u16,
}

impl OutputFormat {
    /// 扱える出力レートの下限/上限（0 や極端な値を弾く）。
    const MIN_RATE: u32 = 4_000;
    const MAX_RATE: u32 = 384_000;

    /// 構成が妥当か検証する。`channels` は 1 か 2、`sample_rate` は
    /// `MIN_RATE..=MAX_RATE`。外れていれば [`Error::UnsupportedFormat`]。
    pub fn validate(&self) -> Result<()> {
        if self.channels != 1 && self.channels != 2 {
            return Err(Error::UnsupportedFormat(format!(
                "output channels must be 1 or 2, got {}",
                self.channels
            )));
        }
        if self.sample_rate < Self::MIN_RATE || self.sample_rate > Self::MAX_RATE {
            return Err(Error::UnsupportedFormat(format!(
                "output sample_rate {} Hz out of supported range {}..={}",
                self.sample_rate,
                Self::MIN_RATE,
                Self::MAX_RATE
            )));
        }
        Ok(())
    }

    /// 出力レートでの 20ms チャンクのフレーム数（48k=960 / 16k=320 / 8k=160）。
    pub fn chunk_frames(&self) -> usize {
        (self.sample_rate as usize * 20) / 1000
    }
}

impl Default for OutputFormat {
    fn default() -> Self {
        // 内部正規形と同一にして第 2 段をパススルーにする。
        Self {
            sample_rate: SAMPLE_RATE,
            channels: CHANNELS,
        }
    }
}

/// 1 ストリームを開くための構成。
///
/// [`Default`] は `chunk_ms = 20`, `ring_capacity_chunks = 50`, `mode = Include`,
/// `exclude_self = false`, `kind = Mic`, `output = {48000, 2}`, `gain = 1.0`,
/// `mix_mic_device_id = None`, `mix_system_device_id = None`, `mix_mic_gain = 1.0`,
/// `mix_system_gain = 1.0` を返す。
///
/// process ソースの対象 PID 扱いは [`mode`](Self::mode) だけ、system ソースの自ホスト
/// 除外は [`exclude_self`](Self::exclude_self) だけが決める。process ソースは
/// `exclude_self` を、system ソースは `mode` を無視する（mic は両方無関係）。
/// `mix_*` の 4 フィールドは [`SourceKind::Mix`] 専用で、それ以外のソースでは無視される。
#[derive(Debug, Clone, PartialEq)]
pub struct StreamConfig {
    /// 選ぶデバイス。mic（入力デバイス）と system（出力エンドポイント）の両方に効く。
    /// `None` なら既定（mic=既定入力 / system=既定出力）、`Some(id)` なら `devices()`
    /// が返す安定 ID に一致するデバイス。不一致なら `start` 時に
    /// [`Error::DeviceNotFound`]。[`SourceKind::ProcessLoopback`] では無視される
    /// （`target_pid` で対象を決める）。[`SourceKind::Mix`] でも無視される
    /// （代わりに `mix_mic_device_id` / `mix_system_device_id` で各側を選ぶ）。
    pub device_id: Option<String>,
    /// ソース種別。
    pub kind: SourceKind,
    /// チャンク長（ミリ秒）。20 固定。
    pub chunk_ms: u32,
    /// チャンクリングの容量（チャンク数）。満杯時は DROP_OLDEST。
    pub ring_capacity_chunks: usize,
    /// [`SourceKind::ProcessLoopback`] の対象 PID。
    pub target_pid: Option<u32>,
    /// 対象 PID を含めるか除くか（process ソースのみ）。[`ProcessMode::Include`]
    /// が既定。[`SourceKind::ProcessLoopback`] 以外では無視される。`Exclude` は
    /// `target_pid` 必須（無ければ facade が [`Error::InvalidArg`]）。
    pub mode: ProcessMode,
    /// 自ホスト（自プロセス）の再生音をシステム音から除外するか（system ソースのみ。
    /// フィードバックループ防止）。`true` で self PID（`std::process::id()`）を除外
    /// する。[`SourceKind::Mix`] では system 側の子キャプチャに適用される。
    /// それ以外のソースでは無視される。
    pub exclude_self: bool,
    /// 出力チャンクのフォーマット。既定 `{48000, 2}`（パススルー）。
    pub output: OutputFormat,
    /// 開始時の入力ゲイン（線形倍率）。1.0=そのまま、2.0=約+6dB、0.0=無音。既定 1.0。
    /// 有限かつ 0.0 以上であること（外れていれば open が [`Error::InvalidArg`]）。
    /// 実行時変更は `Stream::set_gain`。
    pub gain: f32,
    /// [`SourceKind::Mix`] の mic 側で選ぶ入力デバイス。`None` なら既定入力。
    /// id は `devices()` が返す mic の安定 ID。`Mix` 以外では無視される。
    pub mix_mic_device_id: Option<String>,
    /// [`SourceKind::Mix`] の system 側で選ぶ出力エンドポイント。`None` なら既定出力。
    /// id は `devices()` が返す system の安定 ID。`Mix` 以外では無視される。
    pub mix_system_device_id: Option<String>,
    /// [`SourceKind::Mix`] の mic 側の合成前倍率（線形）。既定 1.0。`Mix` 以外では
    /// 無視される。既存の [`gain`](Self::gain) はグローバルで合成後に適用される
    /// （最終値 ≒ clamp(clamp(mic×mix_mic_gain + sys×mix_system_gain) × gain)）。
    pub mix_mic_gain: f32,
    /// [`SourceKind::Mix`] の system 側の合成前倍率（線形）。既定 1.0。`Mix` 以外では
    /// 無視される。合成後のグローバル倍率は [`gain`](Self::gain) を参照。
    pub mix_system_gain: f32,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            device_id: None,
            kind: SourceKind::Mic,
            chunk_ms: 20,
            ring_capacity_chunks: 50,
            target_pid: None,
            mode: ProcessMode::Include,
            exclude_self: false,
            output: OutputFormat::default(),
            gain: 1.0,
            mix_mic_device_id: None,
            mix_system_device_id: None,
            mix_mic_gain: 1.0,
            mix_system_gain: 1.0,
        }
    }
}

/// ストリーム実行中に消費側へ通知される非同期イベント。
///
/// 将来バリアントを足せるよう `#[non_exhaustive]`（外部の match は `_ =>` が要る）。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Event {
    /// チャンクリング満杯により `count` 個のチャンクがドロップされた。
    ChunkDropped {
        /// 直近の通知以降にドロップされた累計（または増分）数。
        count: u64,
    },
    /// データ到着が途絶し、ストリームが失速したと判定された。
    StreamStalled,
    /// 失速後にデータ到着が復帰した。
    StreamRecovered,
    /// 必要な権限が拒否された。
    PermissionDenied,
    /// キャプチャデバイスが失われた（切断など）。
    DeviceLost,
    /// その他のバックエンドエラー（説明文付き）。
    Error(String),
}

/// flexaudio-core の操作で発生しうるエラー。
///
/// 将来バリアントを足せるよう `#[non_exhaustive]`（外部の match は `_ =>` が要る）。
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// 引数が無効。
    #[error("invalid argument: {0}")]
    InvalidArg(String),
    /// 現在の状態では実行できない操作。
    #[error("invalid state: {0}")]
    InvalidState(String),
    /// 指定デバイスが見つからない。
    #[error("device not found")]
    DeviceNotFound,
    /// 権限が拒否された。
    #[error("permission denied")]
    PermissionDenied,
    /// 実行中の OS バージョンが当該機能を満たさない。
    #[error("unsupported OS version")]
    UnsupportedOsVersion,
    /// デバイスが実行中に失われた。
    #[error("device lost")]
    DeviceLost,
    /// バックエンド固有のエラー（説明文付き）。
    #[error("backend error: {0}")]
    Backend(String),
    /// 要求された出力フォーマット（レート/チャンネル）が非対応。
    #[error("unsupported output format: {0}")]
    UnsupportedFormat(String),
    /// この環境ではサポートされない操作。
    #[error("unsupported")]
    Unsupported,
}

/// flexaudio-core 全体で用いる結果型。
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_stream_config_matches_contract() {
        let c = StreamConfig::default();
        assert_eq!(c.chunk_ms, 20);
        assert_eq!(c.ring_capacity_chunks, 50);
        assert_eq!(c.mode, ProcessMode::Include);
        assert!(!c.exclude_self);
        assert_eq!(c.kind, SourceKind::Mic);
        assert_eq!(c.device_id, None);
        assert_eq!(c.target_pid, None);
        assert_eq!(c.gain, 1.0);
        // Mix 専用フィールドの既定（デバイス未指定・合成前倍率 1.0）。
        assert_eq!(c.mix_mic_device_id, None);
        assert_eq!(c.mix_system_device_id, None);
        assert_eq!(c.mix_mic_gain, 1.0);
        assert_eq!(c.mix_system_gain, 1.0);
        // 既定の出力は内部正規形と同一（第 2 段パススルー）。
        assert_eq!(c.output.sample_rate, SAMPLE_RATE);
        assert_eq!(c.output.channels, CHANNELS);
        assert_eq!(c.output, OutputFormat::default());
    }

    #[test]
    fn output_format_chunk_frames_are_time_based() {
        assert_eq!(
            OutputFormat {
                sample_rate: 48_000,
                channels: 2
            }
            .chunk_frames(),
            960
        );
        assert_eq!(
            OutputFormat {
                sample_rate: 16_000,
                channels: 1
            }
            .chunk_frames(),
            320
        );
        assert_eq!(
            OutputFormat {
                sample_rate: 8_000,
                channels: 2
            }
            .chunk_frames(),
            160
        );
    }

    #[test]
    fn output_format_validation_rejects_bad_configs() {
        // ch=0 / ch=3 は非対応。
        assert!(OutputFormat {
            sample_rate: 48_000,
            channels: 0
        }
        .validate()
        .is_err());
        assert!(OutputFormat {
            sample_rate: 48_000,
            channels: 3
        }
        .validate()
        .is_err());
        // 極端なレートは非対応。
        assert!(OutputFormat {
            sample_rate: 100,
            channels: 1
        }
        .validate()
        .is_err());
        assert!(OutputFormat {
            sample_rate: 1_000_000,
            channels: 2
        }
        .validate()
        .is_err());
        // 妥当な構成は OK。
        assert!(OutputFormat {
            sample_rate: 16_000,
            channels: 1
        }
        .validate()
        .is_ok());
        assert!(OutputFormat::default().validate().is_ok());
    }

    #[test]
    fn device_info_builds_and_clones() {
        let mic = DeviceInfo {
            id: "alsa_input.pci-0000_00_1f.3".into(),
            name: "内蔵マイク".into(),
            source_kind: SourceKind::Mic,
            sample_rate: 48_000,
            channels: 2,
            is_loopback: false,
            is_default: true,
        };
        // Clone / PartialEq が機能すること（列挙結果の比較・複製に使う）。
        assert_eq!(mic, mic.clone());
        assert!(!mic.is_loopback);
        assert!(mic.is_default);
        assert_eq!(mic.source_kind, SourceKind::Mic);

        let sys = DeviceInfo {
            source_kind: SourceKind::SystemLoopback,
            is_loopback: true,
            is_default: false,
            ..mic.clone()
        };
        assert!(sys.is_loopback);
        assert_ne!(mic, sys);
    }

    #[test]
    fn process_mode_default_is_include() {
        // 既定は Include（対象 PID だけ録る）。Exclude は明示指定が要る。
        assert_eq!(ProcessMode::default(), ProcessMode::Include);
        assert_ne!(ProcessMode::Include, ProcessMode::Exclude);
    }

    #[test]
    fn chunk_flags_are_distinct_bits() {
        let all = ChunkFlags::DISCONTINUITY | ChunkFlags::RECOVERED | ChunkFlags::SILENCE;
        assert_eq!(all.bits(), 0b111);
        assert!(all.contains(ChunkFlags::SILENCE));
        assert_eq!(ChunkFlags::default(), ChunkFlags::empty());
    }
}
