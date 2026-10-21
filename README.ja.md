# flexaudio

各リリースの変更と移行方法は、[変更履歴](CHANGELOG.md)を参照してください。
過去の VAD 移行: [0.6.0 の移行表](CHANGELOG.md#060---2026-10-15)には、
非推奨の 0.5.x の VAD API を 1 つの汎用 `Vad` に統合した変更を記載しています。

過去の Rust キャプチャの 0.4 から 0.5 への移行: `AudioChunk` と `SecondaryChunk` の構造体リテラルには、
新しい公開フィールド `frame_index: u64` が必要です。各タップの基準となる 48 kHz
タイムライン上で、チャンクの最初のフレームを指します。再オープン後も連続し、
キューで音声が失われると飛びが現れます。生成元のタイムラインから設定してください。
時点 0 から始まる合成チャンクを 1 個作る場合は `frame_index: 0` を使います。
C の `FlexChunk` v1 のレイアウトは変わりません。
過去の変更として、0.5.4 では Rust の `DeviceInfo` リテラルにも `format_available` が加わりました。

0.9 では `AudioChunk`・`DeviceInfo`・`StreamConfig` が `#[non_exhaustive]` になります。
`AudioChunk::new(data, frames)`・`DeviceInfo::new(id, name, source_kind)`・
`StreamConfig::new(kind)` で作成してから、必要な公開フィールドを設定してください。
`StreamConfig::default()` も引き続き使えます。
例えば `let mut config = StreamConfig::new(SourceKind::Mic); config.gain = 0.5;` と書けます。
分解するパターンには `..` を含めてください。チャンクのメタデータは初期状態では
ゼロと空のフラグなので、音声を生成するときはタイムラインの値とメーターを設定してください。

[English](README.md) | **日本語**

**Rust 向けの、汎用的で柔軟なクロスプラットフォーム音声キャプチャライブラリです。**

保守者が目指す完成像と約束は、[設計原則](docs/PRINCIPLES.md)を参照してください。

`flexaudio` は、**Linux**・**Windows**・**macOS** で、**マイク**、
**システム出力（ループバック）**、**個別プロセス**、**マイクとシステム出力のミックス**から音声をキャプチャする統一 API を提供します。
すべての音声ソースを、指定した出力形式のインターリーブされた `f32` ストリームに正規化し、
シンプルなポーリングループでチャンクとデバイス／ストリームのイベントを受け取れます。

```rust
use flexaudio::{open, StreamConfig, SourceKind};

let mut stream = open(StreamConfig::new(SourceKind::Mic))?;
stream.start()?;
while let Some(chunk) = stream.poll_chunk() {
    // chunk.data is interleaved f32 in your chosen OutputFormat
    let _ = (chunk.frames, chunk.peak, chunk.rms);
}
stream.stop();
# Ok::<(), flexaudio::Error>(())
```

---

## 対応機能一覧（「9 マス」）

3 種類のキャプチャソース × 3 つの OS です。✅ は実装・検証済み、
— はそのプラットフォームでは利用できないことを示します。

| ソース              | Linux            | Windows           | macOS                       |
|---------------------|------------------|-------------------|-----------------------------|
| **マイク**          | ✅ (cpal/ALSA)   | ✅ (cpal/WASAPI)  | ✅ (cpal/CoreAudio)         |
| **システム出力**    | ✅ (PipeWire)    | ✅ (WASAPI loopback) | ✅ (CoreAudio process taps) |
| **プロセス単位**    | ✅ (PipeWire)    | ✅ (WASAPI process loopback) | ✅ (CoreAudio process taps) |

- **マイク**は、すべてのプラットフォームで [`cpal`] を通じて動作します。
- **システム出力／プロセス単位**のキャプチャは、コンパイル時に選択される OS 固有のバックエンドを使います。
  その OS でサポートされていないソースを指定すると、`Error::Unsupported` が返ります。
- プロセス単位のキャプチャには、`StreamConfig` の `target_pid` が必要です。
- `SourceKind::Mix` は、3 つのプラットフォームすべてでマイクとシステム出力を組み合わせます。
- **VAD:** 1 つの汎用 `Vad` が区間分割、フレーム確率、暫定通知、キャプチャへの
  接続を Rust、Node.js、Python、C に提供します。埋め込んだ Silero モデルでオフライン動作します。
- **型付き診断:** 音声損失、クリッピング、権限、一時的／終端のエラーのイベントと、
  保持されたキャプチャ／後片付けの失敗を確認できます。

---

## バインディング機能一覧

以下はキャプチャのインターフェースです。単体アドオンは別です。接続した VAD は常に
共有の内部標準形式のキャプチャ分岐を処理し、主／副の指定はイベントを載せる出力を選びます。
— は公開されていない機能です。

| 機能 | Rust | Node | Python | C | CLI |
|---|---|---|---|---|---|
| 配信方式 | ポーリング | 内部ポーリングからコールバック | ポーリング | ポーリング | 内部ポーリング → stdout/WAV |
| 副出力タップ | あり | あり（`chunk.secondary`） | あり（`poll_secondary`、f32/s16） | あり（v3、f32/s16） | — |
| 接続 VAD（主／副） | `VadTap` で内部標準形式のキャプチャを処理 | 主または副のチャンクにイベントを付与 | 主または副のチャンクにイベントを付与 | 主（v2/v3）または副（v3）のチャンクにイベントを付与 | — |
| ノイズ抑制 | 内部の 48 kHz で処理、出力レートは任意 | 48 kHz 出力が必要 | 48 kHz 出力が必要 | v1/v2: 48 kHz 出力、v3/v4: 出力レートは任意 | `--denoise` |
| リング容量（既定は 50 チャンク） | `ring_capacity_chunks` | `ringCapacityChunks` | `ring_capacity_chunks` | v4 の `ring_capacity_chunks` | `--ring-capacity-chunks` |
| ソース切替 | `switch_source` | `switchSource`、アドオン設定を維持 | `switch_source` | `flexaudio_switch_source` | `--sources` のスケジュール |
| イベントの詳細 | 型付きイベント／エラー／損失 | 型付きイベント／エラー／損失 | 型付きイベント／エラー／損失 | v1 は平坦化、v2 は詳細情報 | stderr に診断／損失の要約 |
| 無音診断プローブの制御 | `silence_probe` | `silenceProbe` | `silence_probe` | `silence_probe`: 0/1 で有効、2 で無効 | `--no-silence-probe` |
| 機能対応状況の取得 | `capabilities()` | `await capabilities()` | `capabilities()` | `flexaudio_capabilities_v1` | `--capabilities`（JSON） |
| Windows 従来型ループバックのキープアライブ | `OpenOptions` | `windowsClassicLoopbackKeepalive` | `windows_classic_loopback_keepalive` | v3 の `windows_classic_loopback_keepalive` | `--no-classic-loopback-keepalive` |
| ゲイン 0 | ミュート | ミュート | ミュート | 設定のゲイン: 等倍（1.0）、実行時の `flexaudio_set_gain(s, 0)`: ミュート | ミュート |

任意の無音診断プローブは macOS 専用で、既定では有効です。

---

## strict モードと実効設定

キャプチャは既定では strict モードです。明示した値でもソース切替時でも、**デコード後の値が
文書化された既定値と異なる場合だけ、要求された設定とみなします**。例えばマイクの
`silenceProbe: false` は無視され、strict モードでは拒否されます。
`silenceProbe: true` は `notRequested` です。切替時の `gain: 1.0` は
`notRequested` となり現在のゲインを維持します。`gain: 2.0` は現在のゲインが
既に 2.0 でも無視されます。C の特別な既定値指定は先にデコードします。

strict モードは、バックエンドの構築、権限の要求、デバイス／形式の取得、モデルの読み込み、
旧ソースの停止より前に、要求されても無視される設定をまとめて拒否します。
既存の不正入力チェックでは引き続き型付きエラーを返します。
調整は許可します。PID の重複除去／自プロセスの暗黙追加は `exclusionSetNormalized`、
従来の特別な値のデコードは `legacySentinel` です。

レポートには `revision` と `entries` があり、各項目は `setting`、`requested`、
`effective`、`outcome`（`notRequested`、`applied`、`ignored`、`adjusted`）を持ちます。
無視／調整には理由も付きます。リビジョンは 0 から始まり、同一ソースを含め成功した
切替ごとに増えます。復旧では増えません。切替に失敗すると前のレポートとオプションを維持します。
実効値は設定した経路を表し、ハードウェアの稼働を証明しません。
未指定／既定デバイスは Rust の `Absent` または各言語の null 相当で表し、
VAD の値はモデルのパスではなく接続の有無です。Rust の独自バックエンドによる
オープンではレポートを返しません。

既定値はマイク、デバイス／PID 未指定、include モード、20 ms チャンク、リング容量
50 チャンク、ゲインと Mix ゲインは等倍、除外なし、Mix デバイス未指定、主出力
48000 Hz ステレオ、副出力なし、副出力エンコーディング `f32`、プローブ／キープアライブ
有効、ノイズ抑制無効、VAD なしです。strict とコールバック登録は制御項目で、レポートには
含みません。strict はオープン時は既定で true、切替時は明示的に置き換えない限り維持します。

無視される設定を許容するには、Rust は `OpenOptions::new().strict(false)`、Node は
`strict: false`、Python は `strict=False`、C v4 は `strict = 2`、CLI は `--no-strict`
を指定します。C v1-v3 のオープンは常に strict なので、解除するには v4 へ移行してください。
C v4 の `strict = 0` はオープン時は strict、切替時は現在の値の維持を意味します。
CLI の `--strict` は引き続き利用できますが、`--no-strict` との併用は使用方法のエラーです。
リング容量と初期ゲインはオープン時に固定します。ゲイン変更には実行時の setter を使います。

Rust（返されたストリームは `start()` まで開始しません）:

```rust
let mut stream = flexaudio::open_with_options(
    flexaudio::StreamConfig::default(),
    flexaudio::OpenOptions::new().strict(true),
)?;
println!("{:?}", stream.effective_settings());
stream.start()?;
stream.stop();
```

Node:

```js
const { openStream } = require('@studio-sadola/flexaudio');
const stream = openStream({ kind: 'mic', strict: true, ringCapacityChunks: 100 }, () => {});
console.log(stream.effectiveSettings());
await stream.stop();
```

Python:

```python
import flexaudio
with flexaudio.open('mic', strict=True, ring_capacity_chunks=100) as stream:
    print(stream.effective_settings())
```

C（`flexaudio.h` を include した関数内）:

```c
FlexConfig config = {0}; /* ソース種別 0 はマイク */
FlexStreamConfigV3 base = {0};
base.size = sizeof base;
base.version = FLEX_STREAM_VERSION_3;
base.config = &config;
uint64_t capacity = 100;
FlexOptionsV4 options = {sizeof options, FLEX_STREAM_VERSION_4, 1, &capacity};
FlexStreamConfigV4 request = {sizeof request, FLEX_STREAM_VERSION_4, &base, &options};
FlexStream *stream = NULL;
if (flexaudio_open_v4(&request, &stream) == FLEX_OK) {
    FlexSettingsV4 *report = NULL;
    if (flexaudio_settings_v4(stream, &report) == FLEX_OK) {
        uint64_t revision = 0;
        size_t count = 0;
        flexaudio_settings_count_v4(report, &revision, &count);
        /* flexaudio_settings_item_v4 で各項目を確認します。 */
        flexaudio_settings_free_v4(report);
    }
    flexaudio_free(stream);
}
```

CLI（レポートは stderr へ出力）:

```sh
flexaudio-cli --source mic --strict --settings-json --ring-capacity-chunks 100 --seconds 1 --out capture.wav
```

strict の拒否時は、Rust の `Error::ignored_settings()`、Node の
`FLEX_IGNORED_SETTINGS` 例外の `.report`、Python の `IgnoredSettingsError.report`、
C の `FLEX_IGNORED_SETTINGS`（-13）と `flexaudio_error_settings_v4`、
CLI の `--settings-json` で候補レポートを取得できます。
Rust の `resolve_settings(&SettingsRequest::new(config))` はデバイスなしで設定を解決し、
`SettingsAttachments` は接続の有無と各フロントエンドの切替方針を記述します。
この解決処理はキャプチャ引数の検証や接続自体の設定は行いません。

以下は共有レポートのタグを使った、無視される設定の正確な規則です。Node の `processId`
は `targetPid` に、`micDeviceId`、`systemDeviceId`、`micGain`、`systemGain` は
対応する 4 つの `mix*` タグに対応します。各行は既定値と異なる要求だけに適用し、
切替時のみと記した行を除き、オープン時と切替時の両方に適用します。

| 設定 | 条件 | 理由 |
|---|---|---|
| `deviceId` | Process または Mix | `wrongSourceKind` |
| `targetPid`, `mode` | Process 以外 | `wrongSourceKind` |
| `excludeSelf`, `excludePids` | マイクまたは Process | `wrongSourceKind` |
| `mixMicDeviceId`, `mixSystemDeviceId`, `mixMicGain`, `mixSystemGain` | Mix 以外 | `wrongSourceKind` |
| `silenceProbe` | マイク | `wrongSourceKind` |
| `silenceProbe` | macOS 以外のその他の種別 | `unsupportedPlatform` |
| `windowsClassicLoopbackKeepalive` | マイクまたは Process | `wrongSourceKind` |
| `windowsClassicLoopbackKeepalive` | Windows 以外の System/Mix | `unsupportedPlatform` |
| `windowsClassicLoopbackKeepalive` | 除外ありの Windows System/Mix | `processLoopbackPath` |
| `deviceId` / `mixSystemDeviceId` | 除外ありの Linux/Windows System/Mix のシステム側 | `exclusionDeviceUnscoped` |
| `gain`, `ringCapacityChunks` | 切替時のみ | `fixedAtOpen` |
| Node の `denoise`, `vad` | 切替時のみ | `fixedAtOpen` |
| C の `config.denoise` | 切替時のみ | `fixedAtOpen` |

`excludeSelf` が true、または `excludePids` が空でなければ除外は有効です。
macOS は除外とシステムデバイスの明示選択を併用できます。
Python の切替時のノイズ抑制と Rust の明示的なノイズ抑制切替メソッドは引き続き適用します。

## 任意の再生ヘルパーとライフサイクル

どちらも該当する環境では既定で有効です。目的は異なり、同じ OS で両方を使いません。

| OS | 無音診断プローブ | 従来型ループバックのキープアライブ |
|---|---|---|
| macOS | Mix のシステム側を含む System/Process キャプチャで、適格な外部出力が動作中にネイティブサンプルが 5 秒間連続して厳密なゼロなら、キャプチャ世代ごとに最大 1 回実行します。既定出力へ約 300 ms、1 kHz、振幅 `1e-5` の符号化信号を再生し、別の非公開セルフタップで確認します。小さな音ですが、聞こえない保証はありません。このキャプチャの設定にかかわらず、同じ出力をキャプチャする他アプリに信号が入る可能性があります。キャプチャ元プロセスを含めるかどうかが影響するのは、このキャプチャ自身のタップが信号を受け取るかどうかだけです。Started は診断試行への移行を表し、準備中に失敗する試行も含みます。 | 使用しません。 |
| Windows | 使用しません。 | **除外なし**の従来型 System ループバックと Mix のシステム側で使用します。キャプチャ対象の出力デバイスへ共有モードでデジタル無音を再生し、アイドル時もデバイスの時計でキャプチャを進めます。可聴信号を加えず、音量／ミュートも変更しませんが、他アプリは稼働中の再生セッションを検出でき、Windows の音量ミキサーに表示される場合があります。Started は再生開始の成功後に発生し、それ以前の準備失敗ではペアを出しません。Process／除外ループバックでは使用しません。 |
| Linux | 使用しません。 | 使用しません。 |

Mix はシステム側のライフサイクルイベントを転送します。ミックス後のマイク音声の有無は、
macOS 検出器が見るネイティブのシステム側サンプルを置き換えません。
自プロセスを含めると診断信号がシステム側へ入り、マイクが音響的に拾う場合もあります。
Windows の Mix は System キャプチャと同じ無音キープアライブを使い、診断音を混ぜません。

出力された Started ごとに、理由付きの Stopped が必ず 1 回だけ続きます。
ソース切替、復旧、キャンセル、失敗時も同様です。Stopped は後片付けを試みた後に
ヘルパーの所有が終了したことを表し、ネイティブの後片付け失敗は引き続き型付きエラーで報告します。
Windows で再生開始後にキャプチャ開始に失敗した場合、理由 `startupFailed` で停止します。
ライフサイクル同士の順序は保証しますが、PCM や通常イベントとの相対順序は保証しません。

| バインディング | ライフサイクルの配信 | macOS プローブを無効化 | Windows キープアライブを無効化 |
|---|---|---|---|
| Rust | 既存の `Stream::poll_event()`（`Event` のバリアント） | `StreamConfig.silence_probe = false` | `OpenOptions::new().windows_classic_loopback_keepalive(false)` |
| Node | 独立した `OpenOptions.onLifecycleEvent` コールバック | `silenceProbe: false` | `windowsClassicLoopbackKeepalive: false` |
| Python | 独立した `Stream.poll_lifecycle_event()` FIFO | `silence_probe=False` | `windows_classic_loopback_keepalive=False` |
| C | 独立した `flexaudio_poll_lifecycle_event_v4`。`flexaudio_lifecycle_event_free_v4` で解放 | `FlexConfig.silence_probe = 2` | v3 基底エンベロープの `windows_classic_loopback_keepalive = 2` |
| CLI | `--lifecycle-events` で stderr に JSON 行 | `--no-silence-probe` | `--no-classic-loopback-keepalive` |

Node/Python/C の通常イベント経路は、unknown としてもライフサイクルイベントを返しません。
Node は切替時にハンドラーを省略すると維持し、指定した場合は切替の成功時だけ置き換えます。
`stop()` は待機中のライフサイクルコールバック完了後に確定します。Python/C のキューは
停止後も独立して読み出せます。C のイベントは記憶領域を所有し、ストリーム破棄後も有効です。

## 到着時刻ラベルとデバイスの名前空間

主／副チャンクの到着時刻ラベルには、Rust/Python の `arrival_ns`、Node の `arrivalNs`、
C の `flexaudio_chunk_arrival_ns_v4` / `flexaudio_secondary_arrival_ns_v4` を使います。
値は既存の `pts_ns` / `ptsNs` と完全に同じです。チャンクの旧名は 0.8 から文書上のみ
非推奨で、0.9 の期間中もサポートします。固定された C フィールドは恒久的に残します。
音声の連続性にはフレーム番号を使います。VAD のエポック起点の PTS 名は変わりません。

Rust/Python の `devices_with_namespaces()`、Node の `await devicesWithNamespaces()`、
C の `flexaudio_devices_with_namespaces_v4`、CLI の `--list-devices-json` は、
選択用 ID と検出エラーを変えずに提供元の情報を加えます。名前空間は選択用 ID の領域を表し、
永続的なハードウェア識別情報ではありません。リング容量は全バインディングで設定でき、
既定は 50 チャンクです。CLI の `--denoise` は録音前に内部標準形式でのノイズ抑制を有効にし、
出力レートは任意です。

## 機能対応状況の取得

`capabilities()` は、ソースのバックエンドと利用可否、取得できた OS バージョン、
PID／除外の対象範囲と件数制限、出力デバイス選択の扱い、ウォッチャーの対応状況、
任意の音声再生による副作用を返します。キャプチャや再生のストリームを開かず、
音声を再生せず、録音の許可も要求しません。

```rust
use flexaudio::capabilities;

let support = capabilities();
println!("{:?}", support.sources.system.availability);
println!("{:?}", support.system_exclusions);
println!("{:?}", support.side_effects);
```

`Supported` はプラットフォームの前提条件を満たすことを示し、権限や利用可能なデバイスを保証しません。
`Unknown { error }` は、前提条件の問い合わせが失敗したか、完了できなかったことを示します。
対応済みや利用不可と決めつけず、型付きエラーを確認してください。
Linux では PipeWire のソケットがなければ `Unavailable`、ソケットの調査、接続、
サーバー同期の失敗は `Unknown` になります。時間制限付きの確認でも音声サービスを
待つ場合があるため、GUI アプリではワーカースレッドを使ってください。Node と CLI の JSON は
状態を `supported`、`unavailable`、`unknown` と表し、Python も同じタグを使います。

---

## インストール

```toml
[dependencies]
flexaudio = "0.9"
```

または、次のコマンドを使います。

```sh
cargo add flexaudio@0.9
```

音声区間検出（VAD）のアドオンは、別のクレートです。

```sh
cargo add flexaudio-vad@0.9
```

---

## 最小構成の例

```rust
use flexaudio::{open, StreamConfig, SourceKind, OutputFormat};

let mut config = StreamConfig::new(SourceKind::Mic);
config.output = OutputFormat { sample_rate: 16_000, channels: 1 };
let mut stream = open(config)?;
stream.start()?;

// Pull chunks (interleaved f32) and stream-level events.
while let Some(chunk) = stream.poll_chunk() {
    let _ = chunk; // chunk.data, chunk.frames, chunk.peak, chunk.rms, chunk.seq, ...
}
while let Some(event) = stream.poll_event() {
    let _ = event; // ChunkDropped / StreamStalled / PermissionDenied / DeviceLost / Error / ...
}
stream.stop();
# Ok::<(), flexaudio::Error>(())
```

---

## 公開 API の概要

窓口となるクレート `flexaudio` は、必要な API と型をすべて再エクスポートします。

- `flexaudio::open(StreamConfig) -> Result<Stream>` — ソースと OS に応じてバックエンドを選び、
  まだ開始されていないキャプチャストリームを作成します。
- `Stream::start` / `Stream::stop` — キャプチャを制御します。
  `stop_checked()` はキャプチャと後片付けの失敗を返し、`shutdown_report()` は完了結果を保持します。
  停止したストリームは再利用できず、`start()` は `InvalidState` を返します。
  再びキャプチャするには、新しいストリームを開いてください。この規則は 0.4 でも適用されていました。
  繰り返し停止しても、後片付けやイベントの通知は重複しません。
- `Stream::poll_chunk` / `Stream::poll_event` — `AudioChunk` と `Event` を取り出します。
- `Stream::terminal_error() -> Option<Error>` — 停止後も保持される終端エラーを確認します。
  `Stream::resume()` は `Result<()>` を返します。
- `Stream::switch_source` — ストリームを停止せずに入力ソースを切り替えます。
  チャンクの `seq` は連続性を保ち、切り替え後の最初のチャンクには不連続を示すフラグが付きます。
- `flexaudio::devices() -> Result<Vec<DeviceInfo>>` — マイク（CPAL、全プラットフォーム）と
  システム出力エンドポイント（Linux: PipeWire の sink、Windows: 有効な再生エンドポイント、
  macOS: 出力デバイス）を 1 つのリストに列挙します。
  Linux のマイク ID は CPAL の ID です。PipeWire の Audio/Source ノードはマイクとして列挙しません。
  コレクションや件数の取得失敗はエラーを返します。Linux で PipeWire デーモンへ接続できない場合も同様です。
  macOS の列挙は同意ダイアログを表示しません。マイクの同意が未決定なら、HAL／デバイス／形式を
  問い合わせる前に型付き `Backend` エラーを返します。責任を持つホストのアクセスを許可するか、
  待機時間に上限のあるキャプチャのオープンで同意を確定し、再度列挙してください。
  拒否／制限は `PermissionDenied` になります。読めない必須形式は 0/0 と `format_available=false` で
  示し、他のデバイスは利用可能なまま残します。Linux の PipeWire 出力形式には、
  [失敗通知](#054-の失敗通知)に記載した互換性のための既定値があります。
  Windows の列挙では AppCapability によるマイクの権限の事前確認を行わず、
  ネイティブの一覧取得を進めます。キャプチャには従来の別の権限事前確認を適用します。
- `flexaudio::processes() -> Result<Vec<ProcessInfo>>` — Linux/Windows では音声出力のセッション／ストリームを持つプロセスを、
  macOS では入力のみのプロセスを含む Core Audio のプロセスを列挙します
  （[キャプチャ可能なプロセスの列挙](#listing-capturable-processes)を参照）。
  待機中・停止中のプロセスも含まれます。現在再生中かどうかは `is_output_active` で確認し、
  プロセス単位のキャプチャには `pid` を `target_pid` として渡してください。
- `flexaudio::watch_devices() -> Result<DeviceWatcher>` — デバイスの接続・取り外しに関する通知
  （追加／削除／デフォルトの変更）をポーリングで受け取れます。
  Linux では PipeWire のシステム出力の追加・削除・デフォルト変更を、`devices()` と同じ出力 ID で通知します。
  PipeWire ノードから CPAL のマイク ID を解決できないため、マイクの変更は通知しません。
  Linux で開始に失敗するとエラーを返します。その後のデーモン、コールバック、停止処理の
  失敗は `DeviceEvent::TerminalError { error }` を通知します。後の停止／join も失敗した場合、
  蓄積した原因を含む通知が再度届くことがあります。
  Node.js/Python では型付き `error` を持つ `terminalError`、C v2 ではデバイスイベントの
  種別 6 と `flexaudio_device_event_error_v2` で参照できます。C v1 のポーリングは根本原因の
  負のエラーコードを返し、last_error を設定します。Windows/macOS では、意図的に
  何も行わないウォッチャーを返します。
- 再エクスポートされる型: `StreamConfig`, `SourceKind`, `ProcessMode`, `OutputFormat`,
  `AudioChunk`, `SecondaryChunk`, `ChunkFlags`, `DeviceInfo`, `ProcessInfo`,
  `DeviceEvent`, `DefaultDeviceKind`, `Event`, `Permission`, `Error`, `ErrorKind`,
  `ErrorContext`, `AddonErrorKind`, `AudioLoss`, `ShutdownReport`, `Result`。

音声区間検出（`flexaudio-vad`）は、単独処理とキャプチャへの接続で同じ `Vad` エンジンを使います。
`VadPostProcessor` と `speech_segments` でキャプチャに依存しない区間分割も利用できます。
Silero v6 モデルを埋め込み、完全にオフラインで動作します。区間分割は whisper.cpp `85a69493` からの
移植で、確定区間の端点は 10 ms 刻みの整数ミリ秒です。同じフレーム確率に対する後処理を互換対象とし、
異なるモデルの推論確率が一致することは保証しません。

```rust
use flexaudio_vad::{Vad, VadOptions, VadRuntimeOptions};

let mut vad = Vad::new(VadOptions::default(), VadRuntimeOptions::default())?;
let events = vad.process(&vec![0.0; 16_000])?;
let final_events = vad.finish()?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

`VadOptions` の既定値は `threshold: 0.5`、`min_speech_ms: 250`、`min_silence_ms: 100`、
`speech_pad_ms: 30`、`max_speech_ms: 0`（無制限）です。`VadRuntimeOptions.provisional` は
既定で false です。単独処理と接続処理の既定値は同じで、製品ごとの上限は呼び出し側から明示します。

`maxSpeechMs`（`max_speech_ms`）は整数のミリ秒です。省略、0、100,000,000 ms を超える値は、
実質無制限の従来の `INT_MAX/2` サンプルの番兵値（1,073,741,823 サンプル）を使います。
その他の 0 以外の値には、1 秒未満も含め、16 kHz での正確な分割予算
`16 * maxSpeechMs - 512 - 32 * speechPadMs` を使います。負の予算は `InvalidParameter` で
拒否し、0 は有効です（例: 1000 ms と 484 ms のパディング）。分割後の区間が結合される場合が
あるため、最終区間の長さや配信の遅延の上限にはなりません。

0.5.x では、小数の `maxSpeechDurationS` は整数の秒に切り捨てられていました。
同じ区間分割を維持するには `maxSpeechMs = trunc(seconds) * 1000` で移行してください
（例: 30.9 秒は 30000 ms になります）。

Node.js の単独 `process(samples, inputSampleRate?, inputChannels?)` は既定で
16000 Hz、1 チャンネルです。単体での変換は 8000–192000 Hz と正のチャンネル数を受け付けます。
Python は同じ既定値で `input_sample_rate` と `input_channels` を
使います。Rust は変換入力に `process_pcm(samples, input_sample_rate,
input_channels)`、16 kHz モノラルには `process(samples)` を使います。内部で形式を変換し、
イベント時刻は変換器の群遅延を補償した入力時間軸上のミリ秒です。リサンプリングで生じる有限な
振幅の超過だけを内部で [-1, 1] に制限します。生の入力は有限で [-1, 1] の範囲である必要があります。
厳密な golden 一致は
16 kHz モノラルを対象とします。

`VadTap` は基準キャプチャへ接続し、`captureSample` と `ptsNs` を持つ `epochStart`、確定した
`segment`、任意の `provisionalSpeechStart`／`provisionalSpeechEnd`／`provisionalCut`、および
`epochEnd` を届けます。接続設定は `StreamVadOptions`、Node.js は `OpenOptions.vad` です。
flush／停止時にも終了を通知します。不連続やフレーム番号の欠落ではそこまでの区間を閉じ、次の入力で
新しいエポックを開始します。欠落音声をパディングで埋めません。VAD の失敗はその接続だけに保持し、
不連続または成功した flush で復帰します。音声キャプチャは継続し、影響したエポックを型付きエラーで
一度だけ閉じます。960 フレームの基準ブロックと変換位相の整合を保つため、キャプチャのチャンク長は
20 ms に固定します。

---

## 0.5.4 の失敗通知

コレクションの取得や件数の問い合わせ自体が失敗すると、一覧全体を型付きエラーとして返します。
個々の任意メタデータが読めない場合は不明のまま残します。プロセス名には既存の `pid N` を使い、
実行ファイル名／バンドル ID は欠落させます。選択に使える識別子を読めない項目は省略できます。
Windows/macOS は名前を読める場合に既存の名前ベース ID を維持します。名前を読めない場合は
選択可能なネイティブエンドポイント ID／UID と `name=""`（明示的な不明）を使います。
ID は検出ごとに変わることがあり、名前が再び読めるとネイティブ ID から名前に置き換わります。
名前ベースの ID は名前が読めない間はオープンできません。ネイティブ ID は名前が読める場合も
読めない場合もオープンできます。
macOS では出力方向を読めないデバイスを検出結果から省きます。指定されたデバイスの出力方向の
問い合わせに失敗した場合は、その型付きの原因を返します。出力デバイスを明示的に指定した場合の
走査では、消滅したデバイスや、指定とは無関係で情報を読めないデバイスを省きます。
指定された macOS デバイスの出力方向の問い合わせに失敗し、名前も UID も読めない場合は、
指定した ID と照合できないため、オープンは `DeviceNotFound` を返します。
既定デバイスの問い合わせに失敗しても
検出は失敗せず、`is_default=false` になります。Windows/macOS の既定オープンは解決した
デバイスの形式を検査します。Linux は `device_id=None` の場合に `target.object` を設定せず、
一覧の事前検査をせずに PipeWire がオープン時に既定 sink を解決して形式を交渉します。
Linux の除外 fan-in もデバイス単位ではなく、交渉した正規形式を使います。
必須キャプチャ形式を読めないデバイスは `format_available: false`（JS は `formatAvailable`）で残し、
**サンプルレート = 0、チャンネル数 = 0** にします。Windows/macOS の出力とマイクはオープン時に
新しく形式を問い合わせます。Linux の明示的な出力 ID は新しい一覧と形式の可用性を検査します。
これらの検査で形式がまだ読めなければ原因と再試行方法を持つ型付きエラーで失敗します。
Linux の既定出力は PipeWire の解決と形式交渉に任せます。C は v1 構造体のレイアウトを維持し、
`flexaudio_device_format_available_v2` で可用性を公開します。Linux の PipeWire 出力検出と監視通知は、
任意プロパティの欠落時に独立した 48000 Hz／2 チャンネルの互換既定値を維持します。
これらは利用可能な既定値であり、測定値ではありません。
Windows では ERROR_INVALID_PARAMETER（87／HRESULT 0x80070057）を消滅と分類し、
ERROR_ACCESS_DENIED（5／0x80070005）やその他の名前／画像問い合わせエラーは任意メタデータ不明とします。
Windows のプロセス検出は、無効化または消滅した出力エンドポイント
（AUDCLNT_E_DEVICE_INVALIDATED、E_INVALIDARG、ERROR_NOT_FOUND、ERROR_FILE_NOT_FOUND）を省略します。
アクセス拒否とその他のエンドポイント問い合わせエラーは型付き原因とともに一覧全体を失敗させます。
Windows のデバイス検出でも、`Item()` の失敗に同じ省略／終端エラーの規則を適用します。
Linux のプロセス検出は、registry の削除通知で消滅を確認した場合だけ bind 失敗を省略し、
その他の bind 失敗では最初の原因を返します。
macOS では kAudioHardwareBadObjectError（`!obj`、0x216f626a）、ESRCH（3）、ENOENT（2）を消滅とし、
その他のプロパティ／パス問い合わせエラーは任意メタデータ不明とします。Windows の待機は
タイムアウトと待機自体の失敗を区別し、COM 初期化は想定外の HRESULT を報告します。

主／副タップの `arrivalNs`（Rust／Python は `arrival_ns`、C は v4 アクセサー）は、サンプルを取り込む際に付ける
到着時刻のラベルで、取り込みごとに基点を取り直します。どの OS でもマイクとループバックは
同じ時刻の基準を使い、デバイスのタイムスタンプは使いません。ラベルは前の値を下回らないよう
補正しますが、チャンクの時間範囲が重なったり同じ時刻が繰り返されたりします。
メディアクロックではありません。位置の正本は `frameIndex`／`frame_index` で、
番号の欠落は損失を示します。VAD の正確なサンプル時計は
`epochStart.ptsNs + tMs * 1_000_000`、または `epochStart.captureSample + tMs * 48` から求めます。
`captureSample`／`capture_sample` は専用キャプチャ時間軸の 48 kHz フレーム数で、主／副タップの形式や
`tap` に依存しません。主タップが 48 kHz なら、世代の置換、回復、損失後の再構築、起動時 PCM の解放を
またいでも同じサンプルの `frameIndex` と一致します。他のレートでは単位だけが共通で、リサンプラーの
群遅延を補償しないため基点は厳密に一致しません。起動時 PCM 解放のパディングも調整されず、
境界ごとに主タップの最大 1 チャンク分ずつ差が増えることがあります。どちらの番号も、配信 PCM を連結したバッファの
オフセットではありません。一時停止やキューの破棄でも、PCM を配信せずに番号は進みます。

再オープンを伴わない到着間隔の空きでは、イベントや `DISCONTINUITY` は発生しません。
`frameIndex` は連続し、主／副タップの到着時刻ラベルは飛ぶ場合があり、接続された Vad のエポックは開いたままです。
消費サンプルが 2 秒以上途絶えた停止は 250 ms ごとに検知し、`stalled`、再オープン、
再オープンの各失敗時に `recoverableError` と 250 ms〜5 秒の再試行間隔が発生します。
成功後、新しい PCM が来ると `recovered` を通知します。
新世代の最初の主タップデータには `RECOVERED|DISCONTINUITY` が付き、新しい PCM が来ると
Vad は `epochEnd(finish)` で閉じ、新しい `epochStart` を始めます。停止時間はフレームに換算しません。
この 3 種の助言と `onChunk`／エポックイベントの間に順序保証はありません。
接続された Vad のエポック番号はストリーム内で厳密に増加し、再構築では検査付きで 1 を足します。
新しい `openStream` は 0 から始まり、`(epoch, seq)` は各ストリーム内で一意です。
Node.js では `epochEnd(error)` は `vadFailed` より先に届きます。
成功した `switchSource` は必ず `DISCONTINUITY` を持つ新世代を作り、エポック遷移は新しい PCM まで延期します。
同期呼び出し中に `onChunk` が再入することはありません。戻り値が成功しても、意味するのはバックエンドの
置換であり、データの配信ではありません。古い `epochEnd` が新世代の最初の主チャンクより先に届く保証は
ないので、利用者は主タップの `DISCONTINUITY` を境界として使います。

Linux のルーティング変更は次に配信するチャンクに `DISCONTINUITY` を付けます。
bind の失敗、不正・欠落バッファ、拒否した形式再交渉、コールバックの panic は、
既存の準備完了・損失・終端通知の経路で報告します。監視の失敗は監視を終了し、
`DeviceEvent::TerminalError { error }` を通知し、後の停止／join 失敗で蓄積した原因を再通知することがあります。Node.js/Python は型付きの
`error` を持つ `terminalError`、C v2 はデバイスイベント種別 6 と
`flexaudio_device_event_error_v2` を公開します。C v1 の poll は負の原因コードを返し、
last_error を設定します。`RescanRequired` は一覧の無効化を表し、監視失敗とは区別します。
停止後もワーカーの join 失敗と未配信の終端通知を保持します。

`Error::Diagnostic { source, detail }` はネイティブ詳細を保持し、安全な `Display` には
型付きの原因だけを表示します。Node.js の `diagnosticDetails`、Python の
`diagnostic_details`、C v2 の詳細件数・詳細アクセサーで明示的に取得できます。
詳細には非公開のホスト情報が含まれる場合があるため、自動でログに出さないでください。
`Error::Addon { kind, message }` と `AddonErrorKind` は VAD のモデル読み込み・推論・
不正な設定／形式・再サンプリング・reset、および FLAC の I/O・未対応入力・エンコーダー失敗の
型付きの原因を保持します。バインディングは `addonKind`/`addonMessage`（Python は
`addon_kind`/`addon_message`、C v2 は種別・メッセージアクセサー）を公開します。
後から発生した終端エラー、flush と reset の同時失敗、VAD の末尾処理、VAD の
モデル・変換原因は、主原因を置き換えず順序付きの関連エラーとして残します。
Rust に追加した `InferenceError::Multiple`、`VadError::Cause`、
`VadTapError::Cause` が型付きの原因を保持します。

VAD の失敗では、モデル・変換器・セッションの型付き原因を保持します。VAD の接続はソースの
切り替え後も維持され、次の不連続なキャプチャチャンクが共有のギャップ処理で古いエポックを閉じ、
新しいエポックを始めます。イベント専用のチャンクで PCM を作らずに終了を届けます。

macOS の自己プローブが判定不能になった場合、ネイティブの原因を保持し、無音の注意通知と
既存の `RecoverableError` を配信します。キャプチャは継続できます。
ワーカー生成の失敗は I/O の種別・OS コードと再試行の案内を保持します。Node.js の
ローダーは試した全候補と元の失敗を `attempts`/`errors` に残し、停止エラーの生成に
失敗しても Promise を reject します。FFI の bool panic ガードは型付き last_error を
設定します。Python の不正 PID は値の表現を含み、十進表記できない巨大整数には
十六進表記を使います。時計の変換は `i64` の境界で飽和し、CLI は失敗した操作を示し、
実行中の失敗後も WAV の終了処理を検査して両方の失敗を保持します。

## 0.5 への移行

完全な移行手順は [0.5.0 の変更履歴](CHANGELOG.md#050---2026-10-11) を参照してください。
チャンクは 20 ms、ネイティブ入力はモノラル／ステレオを使ってください。他のチャンク長は
`InvalidArg`、2 チャンネルを超える入力は `UnsupportedFormat` になります。
20 ms 以外のチャンク長を拒否するメッセージは、基準となる 20 ms フレーム契約が理由であることと、
設定を 20 ms に変更する手順をすべてのバインディングで共通に示します。
Mix の片側で致命的なエラーが起きると両側を停止し、どちら側の失敗かを保持します。

`Error::kind()` / `root()` / `permission()` で、メッセージを解析せずにラップされたエラーを
分類できます。`Display` はライブラリが記述した説明を保持し、デバイス名、権限の詳細、内部の
呼び出し名は伏せます。必要な診断は構造化された情報から明示的に取得してください。
第三者バックエンドの従来のエラー文字列は、安全な説明に置き換えます。
`AudioLoss`、`Clipped`、`RecoverableError`、`ShutdownError` と、
デバイスの `DefaultCleared`／`RescanRequired` に対応してください。
`DefaultChanged.kind` は `DefaultDeviceKind` になりました。損失後の出力には
`DISCONTINUITY` が付き、`PADDED=8` と `CLIPPED=16` が出力の状態を示します。
すべてのバインディングで peak／RMS はノイズ除去とゲイン適用後の配信サンプルから計測します。

Node.js ではチャンクの `seq`、VAD の `captureSample`、チャンク損失数、音声損失数を
bigint で計算してください。VAD イベントの `seq` と `epoch`、PTS は number です。チャンクの型には `frameIndex` が
必須となり、イベントの型は判別可能な閉じた union になりました。
`terminalError()`／`shutdownReport()` は値がない場合 `undefined` を返します。
コールバックの例外は `uncaughtException` として通知されます。

Python のコアの失敗は `.audio_error` を持つ型付き例外になります。
`InvalidArgumentError`／`UnsupportedFormatError` は `ValueError` のサブクラス、
その他は `RuntimeError` のサブクラスです。`terminal_error()` は `AudioError` を返します。
列挙／ウォッチャーの失敗は例外になり、`stop()` と通常のコンテキスト終了も後片付けの失敗を
送出する場合があります。本体で既に例外が起きている場合は、それを主因として保持し、
後片付けの情報を添えます。停止後のノイズ除去の末尾も受け取るには、キューの音声を取り出してください。

C の v1 の物理レイアウトと従来の定数は維持しますが、enum のフィールドは `int32_t`、
入力の真偽値は `uint8_t` になります。未知の識別値は不正な引数です。
新しい -5..-12 を含む、負の根本原因の結果コードすべてをエラーとして扱ってください。
詳細は v2 の不透明なエラー／イベント／ウォッチャー／停止結果の参照 API を使います。

CLI は一時的な失敗、音声損失、クリッピングでは録音を継続し、終端エラーや後片付けの
失敗では失敗を返します。安全な案内の後に、構造化された権限の詳細を表示します。

---

## マイクとシステム出力のミックス

`SourceKind::Mix` は、マイク入力とシステム出力を 1 つのストリームに組み合わせます。
`devices()` が返す ID を使い、`mix_mic_device_id` と `mix_system_device_id` で
それぞれのデバイスを選択します。`None` はデフォルトの入力／出力を選択します。
Mix では `device_id` は無視されます。ミックス前の線形ゲイン `mix_mic_gain` と
`mix_system_gain` のデフォルトは `1.0` です。`gain` はミックス後に適用する全体の倍率です。

```rust
use flexaudio::{open, SourceKind, StreamConfig};

let mut config = StreamConfig::new(SourceKind::Mix);
config.mix_mic_gain = 1.0;
config.mix_system_gain = 0.5;
let mut stream = open(config)?;
stream.start()?;
stream.stop();
# Ok::<(), flexaudio::Error>(())
```

---

## マイクの起動時サンプル

0.5.3 では、オープン、復旧による再オープン、ソース切り替えで始まる各キャプチャ世代で、
マイク入力（Mix のマイク側も含みます）の先頭の不正サンプルを抑止します。
対象は有限でない値、または [-1, 1] の範囲を超える値です。すべてのチャンネルが
ネイティブフレームで 10 ms の間、有限かつ範囲内であることを確認し、その確認区間の
先頭から全サンプルを届けます。最初から有効な入力はゼロも含めて確認用に保持し、
変更せずに届けます。停止または世代の置き換えまでの有効入力が 10 ms 未満でも届けます。
確認後のサンプルは変更しません。システム／プロセスのループバックには適用しません。

抑止後の最初のチャンクには `DISCONTINUITY` が付きます。破棄フレームはキャプチャの時計と
フレーム番号を進め、録音時間を詰めません。破棄があったマイク世代ごとに一度、
ネイティブフレーム数を持つ `StartupSamplesDropped { frames, reason: OutOfRange }` を発行します。
Mix にも適用します。Node.js は `startupSamplesDropped` と bigint の `frames`、
`reason: 'outOfRange'`、Python は同じタグと整数のフレーム数を使います。
C v2 はイベント種別 15 と型付きフレーム／理由の参照 API、CLI は安全な警告を提供します。

Node.js では `onEvent` を渡すと、該当世代の最初の `onChunk` と接続した VAD の
`epochStart` より先に起動時通知を届けます。`Recovered` などの別の助言は独立した
コールバック経路を使い、起動時通知の前後どちらにも届く可能性があります。
Rust/Python/C の取得型 API では、世代の最初のチャンクを公開する前に通知を時系列順で
通常の FIFO イベントキューへ追加します。先にイベントを取り出す呼び出し側は、
PCM の取得前に通知を確認できます。PCM のポーリングはイベントのポーリングを待ちません。
停止後も PCM だけを取り出せます。C v1 はこの助言を `Unknown`、count 0 として返します。

不正サンプルの後、2 秒以内に有効入力を確認できなければ、原因と再試行の手順を説明する
型付きバックエンドエラーでキャプチャを終了します。有効な音声を固定時間で破棄しません。
有効なネイティブ起動時サンプルを保持している間にキャプチャが終端エラーで失敗した場合、
`AudioLoss` の理由 `generationEnded` が、破棄されたインターリーブ済みスカラーサンプル数と
そのネイティブのサンプルレート／チャンネル数を報告します。C v2 の理由コードは 6 です。

---

## Windows の無音時のシステムキャプチャ

Windows のシステムキャプチャでは、`exclude_self` / `exclude_pids` を指定しなければ、
どのアプリも音声を再生していないときも、デバイスのクロックに基づく無音を連続して取得します。
キャプチャ開始直後の無音時間も含まれます。従来の WASAPI ループバックでは、
同じ出力エンドポイント上で、エンドポイントのミックス形式を使う共有モードの再生ストリームを
無音だけで動作させ続けます。エンドポイントの音量やミュート状態は変更しません。
この無音ストリームの作成や開始に失敗すると、キャプチャの開始も失敗します。
実行中に失敗すると、型付きの `Event::TerminalError` でキャプチャを終了し、両方のストリームを停止します。
停止検知・再オープンによる復旧は行いません。再試行するには新しいストリームを開いてください。
除外を指定したシステムキャプチャを含むプロセスループバックでは、この仕組みを使いません。

キープアライブは既定で有効です。Rust では
`open_with_options(config, OpenOptions::new().windows_classic_loopback_keepalive(false))`
で無効にできます。`Stream::switch_source_with_options(config, options)` はソースとともに
設定を置き換えます。通常の `switch_source` は設定を維持し、切替に失敗した場合は以前の設定を維持します。
Node は `windowsClassicLoopbackKeepalive: false`、Python は
`windows_classic_loopback_keepalive=False`、C は v3 エンベロープの
`windows_classic_loopback_keepalive = 2`（0 は既定／維持、1 は有効）、CLI は
`--no-classic-loopback-keepalive` を使います。Node／Python の切替では省略時に維持し、
C の切替では 0 で維持します。Mix のシステム側にも適用し、
他のプラットフォームやプロセス／除外キャプチャでは無視します。

キープアライブを無効にすると、無音時の従来型ループバックはパケットを配信しません。
既存の 2 秒の監視機構が `Event::StreamStalled` を報告し、音声が再生されるまで再オープンを
繰り返します。この経路では無音の再生も、それに伴う Windows ミキサーのセッションも作りません。

キャプチャしているプロセスは、キャプチャ中や終了後の短い間、Windows の音量ミキサーに
オーディオセッションとして表示される場合があります。無音の再生ストリームがアクティブな
再生セッションを作成するためです。Microsoft の [Audio Sessions](https://learn.microsoft.com/en-us/windows/win32/coreaudio/audio-sessions)
には、アクティブな再生セッションと最近までアクティブだった再生セッションにミキサーの
操作項目を表示することが記載されています。実際の表示と削除のタイミングは、Windows のミキサー UI によって異なります。

---

## PID による再生音声の除外

`StreamConfig::exclude_pids` は、`exclude_self` と組み合わせて、
**システムキャプチャおよび Mix のシステム側**から再生音声を除外します。
マイクとプロセス単位のソースは、有効な除外指定を無視します。
各バインディングでも同じ制御を利用できます。

| インターフェース | PID による除外 |
|---|---|
| Rust | `let mut config = StreamConfig::new(SourceKind::SystemLoopback); config.exclude_pids = vec![1234];` |
| N-API | `openStream({ kind: 'system', excludePids: [1234] }, onChunk)` |
| C | `flexaudio_open_with_exclude_pids(&config, pids, count)`。ソースの切り替えには `flexaudio_switch_source_with_exclude_pids` を使います。 |
| Python | `flexaudio.open("system", exclude_pids=[1234])`。`Stream.switch_source()` でも指定できます。 |
| CLI | `flexaudio-cli --source system --exclude-pid 1234`。複数の PID は `--exclude-pid` を繰り返して指定します。 |

- **Windows:** 1 つのルートを持つプロセスツリーを除外します。`exclude_self` を
  指定すると呼び出し元のプロセスがルートになり、それ以外はリストの最初の PID がルートになります。
  すべての PID はそのルートと同じ値である必要があります。子孫の PID であっても、
  異なる PID は `Error::InvalidArg` で拒否されます。ルートを 1 回指定してください。
  WASAPI のプロセスループバックは出力エンドポイントを指定できないため、
  除外が有効な間は、指定されたシステムデバイスを使用しません。
- **macOS:** キャプチャ開始時に、PID を Core Audio のプロセスオブジェクトへ
  1 回だけ解決します（スナップショット）。音声オブジェクトを持たない実行中のプロセスを含め、
  除外対象を解決できない場合は、安全のためキャプチャを失敗させます。
  終了が確認されたプロセス（`ESRCH`）だけを除外リストから省きます。
  新しい音声ヘルパーが現れた場合は、キャプチャを開き直してください。
  PID は `1..=2147483647` に収まる必要があります。
  指定されたシステムデバイスは、除外と併せて使用されます。
- **Linux:** 子孫を含めず、PID の完全一致で除外します。Pulse 経由のストリームでは
  `application.process.id`、ネイティブクライアントでは `pipewire.sec.pid` を使います。
  除外中、Pulse 経由のストリームは PID が判明するまでキャプチャされません。
  アプリケーションのストリームを集約する処理はデバイス単位ではないため、
  除外が有効な間は、指定されたシステムデバイスを使用しません。
  プロセス／除外キャプチャに適格な対象がある場合は、通常のモニターと共通の 2 秒以内に
  形式のネゴシエーションを完了しなければ、原因と対処方法を持つ型付きエラーでオープンが失敗します。
  完全な対象一覧に適格なソースがなければ、オープンは成功し、その後の再生を監視し続けます。

デバイスの規則は `mix_system_device_id` にも適用されます。マイクの選択には影響しません。

---

<a id="listing-capturable-processes"></a>

## キャプチャ可能なプロセスの列挙

`flexaudio::processes()` は、Linux/Windows では音声出力のセッション／ストリームを持つプロセスを列挙します。
macOS では、入力のみのプロセスを含む、Core Audio が認識しているすべてのプロセスを列挙します。
現在再生中かどうかは `is_output_active` で確認してください。プロセス単位のキャプチャ
（`SourceKind::ProcessLoopback` と `target_pid`）には PID を渡します。
呼び出し元のプロセスは除外され、同じ PID の項目は 1 件にまとめられます。
再生中のプロセスを先頭に、その後は名前、PID の順で並べます。

```rust
use flexaudio::{open, processes, SourceKind, StreamConfig};

for p in processes()? {
    println!("{:>7} {} {:?} active={:?}", p.pid, p.name, p.executable, p.is_output_active);
}
let target = processes()?.into_iter().next();
if let Some(p) = target {
    let mut config = StreamConfig::new(SourceKind::ProcessLoopback);
    config.target_pid = Some(p.pid);
    let mut stream = open(config)?;
    stream.start()?;
    stream.stop();
}
# Ok::<(), flexaudio::Error>(())
```

| 項目／プラットフォーム | Linux (PipeWire) | Windows (WASAPI) | macOS (Core Audio) |
|---|---|---|---|
| 列挙される対象 | `Stream/Output/Audio` ノードを所有するクライアント | すべての有効な再生エンドポイントの音声セッション（システム音と期限切れセッションは除外） | Core Audio が認識しているプロセスオブジェクト（`kAudioHardwarePropertyProcessObjectList`。入力のみのプロセスも含みます） |
| `pid` | Pulse 経由のストリームでは `application.process.id`、ネイティブクライアントでは `pipewire.sec.pid`（キャプチャバックエンドと同じ方法で解決します） | `IAudioSessionControl2::GetProcessId` | `kAudioProcessPropertyPID` |
| `name` | ノードの `application.name`、なければクライアントの値 | イメージファイル名から `.exe` を除いたもの | 実行ファイル名 |
| `executable` | `/proc/<pid>/exe` のベース名。`exe` を読めない場合は `/proc/<pid>/comm` の値 | プロセスイメージのベース名 | `proc_pidpath` から取得したベース名 |
| `bundle_id` | — | — | `kAudioProcessPropertyBundleID` |
| `is_output_active` | ノードの状態が `Running` | セッションの状態が `Active` | `kAudioProcessPropertyIsRunningOutput` |
| 必要条件 | 実行中の PipeWire セッション | Windows ビルド 20348 以降（Windows 11 / Windows Server 2022） | macOS 14.4 以降。それより前では `Error::UnsupportedOsVersion` |

`name` は必ず空でない値になります。名前が取得できない場合は、実行ファイル名、バンドル ID、
`pid <N>` の順で補います。`executable`、`bundle_id`、`is_output_active` は、OS が情報を公開しない場合は
`None` になります。名前はアプリケーションが自己申告した表示用の情報であり、識別には PID を使います。

戻り値は次のように解釈します。

- `Ok(non-empty)` — プロセス単位のキャプチャが利用でき、音声出力のセッション／ストリームを持つプロセス
  （Linux/Windows）、または Core Audio のプロセス（macOS、入力のみのプロセスを含みます）があります。
  待機中・停止中のプロセスも列挙されます。現在再生中かどうかは `is_output_active` で確認してください。
- `Ok(empty)` — プロセス単位のキャプチャは利用できますが、該当するプロセスが現在ありません。
  これは「何も再生されていない」という意味では**ありません**。
- `Err(..)` — この環境でプロセス単位のキャプチャを利用できない
  （Linux: PipeWire に接続できない場合は `Error::Backend`、macOS 14.4 より前または Windows ビルド 20348 より前では
  `Error::UnsupportedOsVersion`、その他の OS では `Error::Unsupported`）、
  権限が拒否された（`Error::PermissionDenied`）、OS が時間内に応答しなかった、
  または前の列挙処理がまだ実行中（`Error::Backend`）のいずれかです。

この呼び出しは読み取り専用で、権限を求めるダイアログを表示しません。
OS の音声サービスが応答しなくても、3 秒以内に戻ります。
N-API バインディングは `await processes()` と `await stream.stop()` を提供し、
JS のイベントループをブロックしないようにしています。

---

## OS ごとの権限要件

アプリケーションは、ホスト OS が必要とする用途説明や機能宣言を行う必要があります。
録音の許可が拒否されたと確認できた場合は、`Error::PermissionDenied { permission, detail }` を返します。
安全な `Display` は対象の権限と、その OS のプライバシー設定／再起動の手順を示します。
元の詳細は構造化された情報から取得でき、CLI はそれを明示的に表示します。
キャプチャ中に拒否を検出すると `Event::PermissionDenied { permission, detail }` を発行し、
キャプチャ（Mix では両方の入力）を終了して、それ以降の音声の配信と自動的な再オープンを抑止します。
権限を修正した後は新しいストリームを作成してください。終端エラーのあるストリームの
`start`、`resume`、`switch_source` は保持されたエラーを返します。

組み込みバックエンドは `PermissionPending` を発行しません。このイベントはカスタム Rust
バックエンド用に残します。`PermissionGranted` は 0.6.0 で削除し、C のイベント番号 9 は予約します。
macOS の認可状態を照会できない場合は、キャプチャを開始する前に
型付きバックエンドエラーを返し、権限が拒否されたと推測しません。
マイクの構成が事前に通知したネイティブ形式と
異なる場合は、キャプチャを構築する前に `Error::NativeFormatChanged { advertised, actual }`
で拒否します。誤った形式でサンプルを解釈して配信せず、現在のデバイス形式を使ってストリームを作り直してください。

Rust では `Stream::terminal_error()`、N-API では `terminalError()` を提供し、N-API の
`stop()` はキャプチャまたは後片付けの失敗で拒否されます。即時の通知には `onEvent` を渡し、
`shutdownReport()` で停止結果を確認してください。Python の `poll_chunk()` は終端の失敗で
`RuntimeError` の型付きサブクラスを送出し、`terminal_error()` はイベントを消費せずに
保持された `AudioError` を返します。C v1 の `flexaudio_poll_chunk()` は、キャプチャの終端エラーで
`FLEX_FAILURE` (-2) を返します。`flexaudio_terminal_error()` と C v2 の `flexaudio_poll_chunk_v2()` も
同じ値を返します。安全な説明は `flexaudio_last_error()` で取得できます。型付きの根本コードを調べるには、
`flexaudio_terminal_error_info_v2()` で所有権を持つエラーを取得し、`flexaudio_error_kind_v2()` で読み、
`flexaudio_error_info_free_v2()` で解放してください。
C の権限イベント種別は引き続き 3 です。N-API/Python の権限イベントの type は
`permissionDenied` のままで、`permission`（`microphone` または `systemAudio`）と `message` が追加されます。

### macOS

- **マイク**（Mix のマイク入力も含みます）: flexaudio は HAL／デバイス形式を問い合わせる前に
  AVFoundation の認可状態を確認します。拒否または制限されている場合は権限エラーになります。
  状態が未決定で、メインのアプリバンドルに空でない `NSMicrophoneUsageDescription` があれば、
  同意を要求して最大 30 秒待ちます。未回答の場合はキャプチャを開始せず、型付き `Backend`
  エラーを返します。ダイアログに回答して再試行してください。オープンは同期呼び出しなので、
  GUI アプリではワーカースレッドで実行してください。
  30 秒の上限は未決定の同意待ちに対するものであり、認可後の任意のネイティブ問い合わせには適用しません。
- 認可が未決定でメインバンドルにマイクの用途説明がなければ、直ちに失敗します。
  責任を持つホストの HAL ダイアログで無制限に待ったり、安全でない直接の同意要求を行ったりしません。
  用途説明を持つアプリバンドルを使うか、責任を持つホストのマイクアクセスを許可して再試行してください。
  未回答だけでは拒否とは判断しません。0.5.3 からは、マイクの開始ごとに同意の確定が必要です。
  認可状態の変更や権限の取り消しを継続監視する仕組みは提供しません。
  0.5.2 では、事前確認が `Ok(true)` を返す未決定の `NotDetermined` 状態に対して、
  `ConsentPoll` が接続されていました。ただし、それより前の `query_native_format` が
  ブロックし、キャプチャ所有者と保留中の同意監視に到達できない場合がありました。
  最初から `Authorized` だったキャプチャには、継続的な権限取り消し監視はありませんでした。
  ネイティブのキャプチャ失敗は、通常のバックエンドエラー経路で報告します。
- **システム音声とプロセス単位の音声**には Core Audio の process tap（macOS 14.4 以降）を使います。
  アプリの `Info.plist` に用途説明を追加してください。
  ```xml
  <key>NSAudioCaptureUsageDescription</key>
  <string>This app records system and application audio.</string>
  ```
  同意ダイアログは OS が管理します。システム音声の権限状態を取得する公開 API はなく、
  flexaudio は非公開の TCC API を使いません。ネイティブの illegal operation は最善努力による診断として
  `SystemAudio` の権限エラーに変換しますが、このネイティブの結果は同意の失敗に限定されません。
- tap はネイティブの権限エラーを返さずにゼロを配信する場合があります。ビット単位で完全なゼロのネイティブサンプルが
  5 秒間連続し、Core Audio がキャプチャ対象の別の適格なプロセスについて `IsRunningOutput` が true と報告すると、
  flexaudio はキャプチャ世代ごとに 1 回、能動的な自己検査を行います。除外とデバイスの選択条件を反映します。
  ゼロのサンプルと出力の動作状態だけでは、録音権限の不足と実際のデジタル無音を区別できません。
- 自己検査は、位相符号化した 1 kHz、振幅 `1e-5` の信号を約 300 ms、デフォルトの出力へ送ります。
  この信号は低レベルですが、聞こえない保証はありません。独立した非公開の tap が自分自身のプロセスだけをキャプチャし、
  この診断信号を確認します。信号を識別できた場合は無音の警告を抑止します。ネイティブの出力コールバックが
  信号の送信を証明している間に、診断用 tap が完全なゼロだけを連続してキャプチャした場合は、
  `SystemAudio` の `Event::PermissionDenied` を発行し、Mix の両方の入力も含めてキャプチャを終了します。
  既存のバインディングの拒否イベントと終端エラーの参照 API は、原因と変更するプライバシー設定を保持します。
  自分自身のプロセスをキャプチャに含めている場合（例えば `excludeSelf: false` のシステムキャプチャ）、
  この診断信号が録音に含まれる場合があります。
- 自己検査でどちらの結果も確定できない場合（例えば構築の失敗、出力／キャプチャのコールバックの欠落、時間切れ）は、
  `Event::SilenceWhileSourceActive { detail }` をその世代で 1 回発行し、キャプチャを継続します
  （N-API/Python の type は `silenceWhileSourceActive`、C のイベント種別は 7）。
  この助言は、権限が不足している可能性と音源が実際に無音である可能性を示します。問い合わせの失敗、
  不明なデバイスの経路、非動作中の音源、サンプルの欠落、ゼロ以外または負のゼロのサンプルでは、
  最初の検査条件を満たしません。キャプチャを停止すると自己検査を取り消し、遅れた通知を抑止します。
  拒否や助言がないことは、権限が許可された証拠にはなりません。
- マイクのアクセスは **システム設定 > プライバシーとセキュリティ > マイク**、システム音声は
  **画面収録とシステムオーディオ録音**の **システムオーディオ録音**を確認してください。
  責任を持つホストアプリ（例えば Terminal）のアクセスを有効にし、そのアプリを再起動して新しいストリームで再試行してください。

プローブは既定で有効です。Rust の `StreamConfig.silence_probe: false`、Node.js の
`silenceProbe: false`、Python の `silence_probe=False`、または C のオープンオプションで
診断信号の再生を無効化できます。システム音声・プロセス音声・Mix のシステム側に適用し、
マイクの同意確認は変更しません。

### Windows

- マイクのキャプチャ（Mix のマイク入力も含みます）は公開されている `AppCapability` の同意 API を確認します。
  ユーザーまたはシステムによる拒否は `Microphone` の権限エラーになります。ネイティブストリームの構築／開始に失敗した後も
  同意を再確認します。未対応／曖昧な状態または問い合わせの失敗ではネイティブのキャプチャを試みますが、
  アクセスが認可された証拠としては扱いません。
- **設定 > プライバシーとセキュリティ > マイク**で **デスクトップ アプリにマイクへのアクセスを許可する**も
  有効にし、ホストアプリを再起動して新しいストリームで再試行してください。
  管理者が設定した制限は、管理者によるポリシー変更が必要な場合があります。
- システム出力（WASAPI ループバック）とプロセス単位のループバックは、標準の WASAPI 再生エンドポイントの
  ループバック／プロセスループバック API（Windows 10/11）を使います。
  デスクトップのループバックキャプチャにはマイクの同意の事前確認を適用しません。

### Linux

- PipeWire の ALSA プラグインでは、`snd_pcm_open`/`snd_pcm_close` の 1 往復ごとに
  約 4 KB がプラグイン内に残る場合があります。CPAL のマイクオープンはこの往復を数回行います。
  長時間動作するアプリでは、ループで開き直すより開いたマイクストリームを再利用してください。
  ネイティブ PipeWire のシステム専用キャプチャはこのマイク経路を通りませんが、
  Mix はマイク側を開きます。
- マイクのキャプチャは、`cpal` を介して ALSA/PipeWire で行います。
  ユーザーには音声デバイスへのアクセス権が必要です。通常は、`audio` グループへの所属、
  または実行中の PipeWire や PulseAudio セッションによって提供されます。
- システム出力とプロセス単位のキャプチャには、実行中の **PipeWire** セッションが必要です。
  プロセス／除外キャプチャに適格な対象がある場合は、通常のモニターと共通の 2 秒以内に
  形式のネゴシエーションを完了しなければ、型付きの原因と対処方法を持つエラーでオープンが失敗します。
  完全な対象一覧に適格なソースがなければ、オープンは成功し、その後の再生を監視し続けます。
  PipeWire がない場合やウォッチャーの開始に失敗した場合、`watch_devices()` はエラーを返します。
  デバイス列挙はすべての提供元から一覧を取得し、一覧自体の取得や件数の問い合わせに失敗するとエラーになります。
  ポータルを利用するデスクトップ環境では、ユーザーにキャプチャの許可を求める場合があります。

---

## 対応 Rust バージョン（MSRV）

- **コア／ファサード／OS バックエンド／マイク:** Rust **1.85** です。
- **`flexaudio-vad`、`flexaudio-napi`、`flexaudio-ffi`、`flexaudio-py`:**
  Rust **1.91** です（`tract-onnx` 0.23.7 が必要とするバージョンです）。

ワークスペースでは、各クレートの `rust-version` で MSRV を固定しています。

---

## バージョン管理方針（SemVer / 0.x）

flexaudio は [Semantic Versioning](https://semver.org/) に従います。
クレートが **0.x** 系である間は、公開 API は**まだ安定していません**。
SemVer に従い、**マイナー**バージョンの更新（`0.2 → 0.3`）には互換性を壊す変更が含まれる場合がありますが、
**パッチ**バージョンの更新（`0.2.0 → 0.2.1`）は後方互換性を保ちます。
互換性のある更新だけを受け取るには、バージョンを `0.9` に固定してください。
[`CHANGELOG.md`](CHANGELOG.md) を参照してください。

---

## ワークスペース構成

| クレート | crates.io | 説明 |
|-------|-----------|-------------|
| `flexaudio` | ✅ | ファサードです。統一された `open()` / `devices()` / `processes()` / `watch_devices()` を提供します。 |
| `flexaudio-core` | ✅ | ソースに依存しないストリームエンジン、型、リサンプリング／正規化処理を提供します。 |
| `flexaudio-mic` | ✅ | 全プラットフォーム対応のマイクバックエンド（cpal）です。 |
| `flexaudio-os-linux` | ✅ | PipeWire によるシステム出力／プロセス単位のバックエンド（Linux）です。 |
| `flexaudio-os-windows` | ✅ | WASAPI によるループバック／プロセス単位のバックエンド（Windows）です。 |
| `flexaudio-os-macos` | ✅ | Core Audio の process tap を使うバックエンド（macOS）です。 |
| `flexaudio-vad` | ✅ | Silero VAD のアドオンです（オフライン動作、モデル埋め込み済み）。 |
| `flexaudio-encode` | ✅ | ストリーミング FLAC エンコード（flacenc）です。 |
| `flexaudio-denoise` | ✅ | RNNoise によるノイズ除去（nnnoiseless）です。 |
| `flexaudio-cli` | — | 参考実装の CLI／ストリーミングキャプチャツールです。 |
| `flexaudio-napi` | — (npm) | Node.js N-API アドオンです（npm で `@studio-sadola/flexaudio` として公開されています）。 |
| `flexaudio-ffi` | — | C ABI です（ポーリングによるキャプチャ、VAD / FLAC / ノイズ除去、`flexaudio_processes`）。 |
| `bindings/flexaudio-py` | — | PyO3 による Python バインディングです（`open` / `devices` / `processes` / アドオン）。 |

✅ の付いた 9 つのクレートは crates.io に公開されています。ワークスペースには 13 のメンバーがあります。

| ワークスペースのメンバー |
|---|
| `crates/flexaudio-core` |
| `crates/flexaudio-os-windows` |
| `crates/flexaudio-os-macos` |
| `crates/flexaudio-os-linux` |
| `crates/flexaudio-mic` |
| `crates/flexaudio` |
| `crates/flexaudio-cli` |
| `crates/flexaudio-ffi` |
| `crates/flexaudio-napi` |
| `crates/flexaudio-vad` |
| `crates/flexaudio-encode` |
| `crates/flexaudio-denoise` |
| `bindings/flexaudio-py` |

---

## ライセンス

[MIT](LICENSE) © 2026 tubome / Studio Sadola.

このプロジェクトは、サードパーティのソフトウェア（Silero VAD モデルと VAD 用の
純粋な Rust による tract 推論、ノイズ除去用の nnnoiseless を介した RNNoise、
PipeWire、およびその他の Rust クレート）を同梱、またはリンクしています。
必要なライセンス表示については、[`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md) を参照してください。
