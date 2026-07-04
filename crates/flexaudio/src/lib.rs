//! flexaudio — 汎用クロスプラットフォーム音声キャプチャライブラリ（mic / system loopback / per-process、Linux・Windows・macOS）。
//!
//! コア + OS バックエンド + mic を cfg で束ねる facade。
//!
//! [`Stream`] が 1 ソースのキャプチャパイプライン（backend → RawRing → 加工スレッド
//! → Normalizer → ChunkRing → poll + ウォッチドッグ復帰）を駆動する。

#![warn(missing_docs)]

pub use flexaudio_core as core;

pub mod device_watcher;
mod mix;
pub mod mock;
pub mod stream;

pub use device_watcher::DeviceWatcher;
pub use mock::MockBackend;
pub use stream::Stream;

// `open()` と一緒に使う型を facade トップから直接出す。利用側や napi バインディングが
// `flexaudio::core` を経由せず `flexaudio::{StreamConfig, SourceKind, ...}` で揃えられる。
pub use flexaudio_core::backend::CaptureBackend;
pub use flexaudio_core::types::{
    AudioChunk, ChunkFlags, DeviceEvent, DeviceInfo, Error, Event, OutputFormat, ProcessMode,
    Result, SourceKind, StreamConfig,
};

/// 全ソースのオーディオデバイスを 1 つのリストで返す。
///
/// - マイク入力（[`core::SourceKind::Mic`], `is_loopback = false`）—
///   [`flexaudio_mic::list_devices`] 経由（cpal, 全 OS）。
/// - システム音声出力（[`core::SourceKind::SystemLoopback`],
///   `is_loopback = true`）— OS 別バックエンド経由（Linux: PipeWire の Audio/Sink。
///   Linux では PipeWire が Audio/Source（マイク）も列挙するので、cpal 分と重複し得る。
///   Windows/macOS: 出力エンドポイント列挙）。返った `id` は
///   `--source system --device-id <ID>` でその出力を選ぶのに使える。
///
/// 各 [`DeviceInfo`] の `id` は取得できる範囲で最も安定なキー（cpal=デバイス名 /
/// PipeWire=`node.name`）。`is_default` は OS の既定デバイスに付く。
///
/// # OS 分岐
/// - Linux: cpal（マイク）+ PipeWire（sink + source）を結合。PipeWire セッションが
///   無ければ PipeWire 分は空になり、cpal 分のみ返る。
/// - Windows / macOS: cpal（マイク）+ OS の出力エンドポイントを結合。
///
/// デバイスが無い／列挙に失敗した環境でも panic せず、取得できた範囲のリスト
/// （しばしば空）を返す。
pub fn devices() -> Result<Vec<DeviceInfo>> {
    // マイク入力（cpal）は全 OS 共通。Linux はこの後 PipeWire 分を extend するので
    // mut が要るが、他 OS では extend しないので mut が不要。その差を allow で吸収する。
    #[allow(unused_mut)]
    let mut all = flexaudio_mic::list_devices()?;

    // システム出力エンドポイントは OS 別。
    #[cfg(target_os = "linux")]
    {
        let linux = flexaudio_os_linux::list_devices()?;
        all.extend(linux);
    }
    #[cfg(target_os = "windows")]
    {
        let win = flexaudio_os_windows::list_output_devices()?;
        all.extend(win);
    }
    #[cfg(target_os = "macos")]
    {
        let mac = flexaudio_os_macos::list_output_devices()?;
        all.extend(mac);
    }

    Ok(all)
}

/// デバイスの着脱・既定変更（ホットプラグ）を監視する [`DeviceWatcher`] を開始する。
///
/// 返ったウォッチャの [`DeviceWatcher::poll_event`] を周期的に呼ぶと、デバイスの
/// 接続・切断・既定変更が [`DeviceEvent`] として pull 型で取り出せる。capture
/// stream 単位の [`core::Event`] とは別系統で、デバイス単位の事象を扱う。
///
/// # OS 分岐 / 縮退
/// - Linux: PipeWire レジストリを永続監視する（`flexaudio-os-linux`）。PipeWire
///   デーモン不在・接続失敗時は [`NoopWatcher`](device_watcher) へ縮退して `Ok` を返す
///   （着脱が来ないだけ。`devices()` がデーモン不在を空リストに握るのと同じ扱い）。
/// - その他 OS: 常に no-op（着脱は配信されない）。
///
/// PipeWire 不在でも panic せず縮退するので、実用上は `Ok` を返す。
pub fn watch_devices() -> Result<DeviceWatcher> {
    device_watcher::watch_devices()
}

/// [`StreamConfig`] からソース種別と OS に応じて backend を選び、[`Stream`] を構築して
/// 返す高レベル入口（まだ start しない）。
///
/// 利用側（CLI・napi バインディング等）は backend を自前で構築せず、`StreamConfig` を
/// 渡すだけでよい。低レベル入口 [`Stream::open`]（呼び元が `Box<dyn CaptureBackend>` を
/// 渡す）は mock テスト・上級用途のために残してある。
///
/// 戻った [`Stream`] はまだキャプチャしていない。消費側が [`Stream::start`] を呼んでから
/// [`Stream::poll_chunk`] / [`Stream::poll_event`] を周期的に呼ぶ。
///
/// # ソース → バックエンドの分岐
/// - [`SourceKind::Mic`] → [`flexaudio_mic::CpalMicBackend`]（cpal, 全 OS）。
/// - [`SourceKind::SystemLoopback`] → Linux / Windows / macOS
///   （Linux: [`flexaudio_os_linux::PwSystemBackend`]＝出力の monitor / PipeWire。
///   Windows: `flexaudio_os_windows::WasapiSystemBackend`＝render endpoint の
///   WASAPI loopback）。`config.exclude_self`（自ホスト除外）と `config.device_id`
///   （出力エンドポイント選択・`None` で既定出力）をそのまま渡す。
///   その他 OS では [`Error::Unsupported`]。
/// - [`SourceKind::ProcessLoopback`] → Linux / Windows / macOS
///   （Linux: [`flexaudio_os_linux::PwProcessBackend`]。Windows:
///   `flexaudio_os_windows::WasapiProcessBackend`）。`config.target_pid` が必須で、
///   無ければ [`Error::InvalidArg`]。`config.mode` をそのまま渡す。
///   その他 OS では [`Error::Unsupported`]。
/// - [`SourceKind::Mix`] → mic 子（cpal）と system 子（OS 別ループバック）を内部に
///   持つ合成バックエンド。各子を 48k/stereo へ揃えて側別ゲイン
///   （`config.mix_mic_gain` / `config.mix_system_gain`）で加算合成し、1 本の
///   ストリームとして届ける。デバイスは `config.mix_mic_device_id` /
///   `config.mix_system_device_id` で選ぶ（`None` で既定）。`config.exclude_self` は
///   system 側に適用される。system 側が非対応の OS では [`Error::Unsupported`]。
///
/// process ソースは `config.mode` だけを見て `config.exclude_self` を無視し、system
/// ソースは `config.exclude_self` だけを見て `config.mode` を無視する。両者を合成せず、
/// それぞれ OS の単一 PID 除外プリミティブへ 1 対 1 で写す。
///
/// # エラー
/// - 出力フォーマットが非対応 → [`Error::UnsupportedFormat`]（早期に弾く）。
/// - ProcessLoopback で `target_pid` 欠落 → [`Error::InvalidArg`]
///   （[`ProcessMode::Exclude`] でも `target_pid` 必須）。
/// - Mix で `mix_mic_gain` / `mix_system_gain` が有限でない・負 → [`Error::InvalidArg`]。
/// - 当該 OS で非対応のソース（Linux/Windows/macOS 以外の system/process/mix）→ [`Error::Unsupported`]。
/// - その他は [`Stream::open`] 由来（`ring_capacity_chunks == 0` 等）。
///
/// # 除外（Exclude / exclude_self）
/// process [`ProcessMode::Exclude`]（対象 PID 以外の全システム音）/ system
/// `exclude_self=true`（自プロセスを除外）は Linux / Windows / macOS の 3 OS とも対応。
/// Linux は PipeWire の対象外ノード fan-in、Windows/macOS は各 OS のネイティブ PID 除外で
/// 実現する。Include / `exclude_self=false` は除外せず対象そのものを録る。
///
/// # 例
/// ```no_run
/// use flexaudio::{open, StreamConfig, SourceKind};
///
/// let config = StreamConfig {
///     kind: SourceKind::Mic,
///     ..Default::default()
/// };
/// let mut stream = open(config)?;
/// stream.start()?;
/// while let Some(chunk) = stream.poll_chunk() {
///     // chunk.data は出力フォーマットの interleaved f32
///     let _ = chunk;
/// }
/// stream.stop();
/// # Ok::<(), flexaudio::Error>(())
/// ```
pub fn open(config: StreamConfig) -> Result<Stream> {
    // 出力フォーマットを先に弾く（Stream::open でも再検証されるが、backend を構築する
    // 前にエラーを返したい）。
    config.output.validate()?;

    let backend = build_backend(&config)?;

    // 低レベル入口へ委譲（Normalizer 構成・スレッド配線はここが担う）。
    Stream::open(config, backend)
}

/// [`StreamConfig`] からソース種別と OS に応じて backend を 1 つ構築する。
///
/// [`open`] は出力フォーマット検証後にこれを呼ぶ。
/// [`Stream::switch_source`](crate::stream::Stream::switch_source) も切替先の
/// backend を作るのに同じ関数を使う。
///
/// 分岐・エラーは [`open`] のドキュメントと同じ:
/// - [`SourceKind::Mic`] → [`flexaudio_mic::CpalMicBackend`]（全 OS）。
///   `config.device_id` を渡して特定入力デバイスを選べる（`None` で既定入力。
///   id は [`devices`] が返す安定 ID = デバイス名。不一致は `start` 時に
///   [`Error::DeviceNotFound`]）。`config.device_id` は mic（入力デバイス）と system
///   （出力エンドポイント）の両方に効く（`None` で既定）。process では見ない
///   （target_pid で対象を決める）。
/// - [`SourceKind::SystemLoopback`] → Linux/Windows/macOS 対応
///   （Linux=[`flexaudio_os_linux::PwSystemBackend`] / Windows=WASAPI loopback /
///   macOS=CoreAudio Process Tap）。`config.device_id` で出力エンドポイントを選べる
///   （`None` で既定出力）。非対応 OS は [`Error::Unsupported`]。
/// - [`SourceKind::ProcessLoopback`] → Linux/Windows/macOS 対応
///   （Linux=[`flexaudio_os_linux::PwProcessBackend`] / Windows=WASAPI process loopback /
///   macOS=CoreAudio Process Tap）。`target_pid` 必須・欠落で [`Error::InvalidArg`]。
///   非対応 OS は [`Error::Unsupported`]。
/// - [`SourceKind::Mix`] → mic 子（[`flexaudio_mic::CpalMicBackend`]、
///   `config.mix_mic_device_id`）と system 子（[`build_system_backend`]、
///   `config.exclude_self` + `config.mix_system_device_id`）を持つ合成バックエンド
///   （`mix::CompositeBackend`）。`mix_mic_gain` / `mix_system_gain` は有限かつ
///   0 以上でなければ [`Error::InvalidArg`]。system 側が非対応の OS は
///   [`Error::Unsupported`]。
pub(crate) fn build_backend(config: &StreamConfig) -> Result<Box<dyn CaptureBackend>> {
    // Error は複数の分岐で使う（PID 欠落は InvalidArg、非対応 OS は Unsupported）ので
    // 関数頭で use する。
    use flexaudio_core::types::Error;

    let backend: Box<dyn CaptureBackend> = match config.kind {
        // マイク入力は全 OS 共通（cpal）。device_id で特定入力デバイスを選べる
        // （None=既定入力デバイス。id は devices() が返す安定 ID = デバイス名）。
        // 同じ device_id は system でも出力エンドポイント選択に効く。
        SourceKind::Mic => Box::new(flexaudio_mic::CpalMicBackend::new(config.device_id.clone())),

        // システム出力ループバックは Linux / Windows / macOS 対応。
        // exclude_self（自ホスト除外）と device_id（出力エンドポイント選択）を backend へ
        // 渡す。mode は見ない。device_id=None で既定出力。
        SourceKind::SystemLoopback => {
            build_system_backend(config.exclude_self, config.device_id.clone())?
        }

        // プロセス出力ループバックは Linux / Windows / macOS 対応・target_pid 必須。
        // mode（Include/Exclude）を backend へ渡す。exclude_self は見ない。
        // mode:Exclude でも target_pid は必須（無ければ InvalidArg）。
        SourceKind::ProcessLoopback => {
            #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
            {
                let pid = config.target_pid.ok_or_else(|| {
                    Error::InvalidArg("ProcessLoopback には target_pid が必要".into())
                })?;
                #[cfg(target_os = "linux")]
                {
                    Box::new(flexaudio_os_linux::PwProcessBackend::new(pid, config.mode))
                }
                #[cfg(target_os = "windows")]
                {
                    Box::new(flexaudio_os_windows::WasapiProcessBackend::new(
                        pid,
                        config.mode,
                    ))
                }
                #[cfg(target_os = "macos")]
                {
                    Box::new(flexaudio_os_macos::MacProcessBackend::new(pid, config.mode))
                }
            }
            #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
            {
                return Err(Error::Unsupported);
            }
        }

        // mic + system のミックス。子 2 つを合成バックエンドへ注入する。側別ゲインは
        // グローバル gain（Stream::open が検証）と同じ基準で検証する。device_id は
        // 見ない（mix_mic_device_id / mix_system_device_id で各側を選ぶ）。
        SourceKind::Mix => {
            for (name, gain) in [
                ("mix_mic_gain", config.mix_mic_gain),
                ("mix_system_gain", config.mix_system_gain),
            ] {
                if !gain.is_finite() || gain < 0.0 {
                    return Err(Error::InvalidArg(format!(
                        "{name} must be finite and >= 0.0, got {gain}"
                    )));
                }
            }
            let mic = Box::new(flexaudio_mic::CpalMicBackend::new(
                config.mix_mic_device_id.clone(),
            ));
            // exclude_self は Mix では system 側に適用する（フィードバック防止の意図は
            // system 単独と同じ）。非対応 OS はここで Unsupported になる。
            let system =
                build_system_backend(config.exclude_self, config.mix_system_device_id.clone())?;
            Box::new(mix::CompositeBackend::new(
                mic,
                system,
                config.mix_mic_gain,
                config.mix_system_gain,
            ))
        }
    };

    Ok(backend)
}

/// システム出力ループバックの backend を OS に応じて 1 つ構築する。
///
/// [`SourceKind::SystemLoopback`] 本体と [`SourceKind::Mix`] の system 側の両方が
/// 使う共通ヘルパ（OS 分岐を二重化しない）。`exclude_self` は自ホスト除外、
/// `device_id` は出力エンドポイント選択（`None` で既定出力）。
/// Linux / Windows / macOS 以外は [`Error::Unsupported`]。
fn build_system_backend(
    exclude_self: bool,
    device_id: Option<String>,
) -> Result<Box<dyn CaptureBackend>> {
    #[cfg(target_os = "linux")]
    {
        Ok(Box::new(flexaudio_os_linux::PwSystemBackend::new(
            exclude_self,
            device_id,
        )))
    }
    #[cfg(target_os = "windows")]
    {
        Ok(Box::new(flexaudio_os_windows::WasapiSystemBackend::new(
            exclude_self,
            device_id,
        )))
    }
    #[cfg(target_os = "macos")]
    {
        Ok(Box::new(flexaudio_os_macos::MacSystemBackend::new(
            exclude_self,
            device_id,
        )))
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        let _ = (exclude_self, device_id);
        Err(flexaudio_core::types::Error::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mix の側別ゲイン検証: 負・NaN は backend 構築前に InvalidArg で弾かれる
    /// （デバイスには一切触れない）。
    #[test]
    fn mix_config_rejects_invalid_side_gains() {
        for (mic_gain, system_gain) in [
            (-1.0f32, 1.0f32),
            (1.0, -0.5),
            (f32::NAN, 1.0),
            (1.0, f32::INFINITY),
        ] {
            let config = StreamConfig {
                kind: SourceKind::Mix,
                mix_mic_gain: mic_gain,
                mix_system_gain: system_gain,
                ..Default::default()
            };
            match open(config) {
                Ok(_) => {
                    panic!("mix ゲイン ({mic_gain}, {system_gain}) は InvalidArg で弾かれるはず")
                }
                Err(Error::InvalidArg(msg)) => {
                    assert!(
                        msg.contains("mix_mic_gain") || msg.contains("mix_system_gain"),
                        "どちらのゲインが不正か分かるメッセージのはず: {msg}"
                    );
                }
                Err(other) => panic!("InvalidArg を期待したが別のエラー: {other:?}"),
            }
        }
    }

    /// 妥当な Mix config は backend 構築（open）まで通る（まだ start しないので
    /// 実デバイスには触れない。ヘッドレス環境でも成立）。
    #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
    #[test]
    fn mix_config_with_valid_gains_opens() {
        let config = StreamConfig {
            kind: SourceKind::Mix,
            mix_mic_gain: 0.5,
            mix_system_gain: 2.0,
            ..Default::default()
        };
        let stream = open(config).expect("妥当な Mix config は open まで通るはず");
        // 合成バックエンドは内部正規形を名乗る（Stream 第 1 段はパススルー）。
        assert_eq!(stream.native_format(), (48_000, 2));
    }
}
