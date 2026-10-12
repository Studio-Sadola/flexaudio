# flexaudio

Rust の 0.5 への移行: `AudioChunk` と `SecondaryChunk` の構造体リテラルには、
新しい公開フィールド `frame_index: u64` が必要です。各タップの基準となる 48 kHz
タイムライン上で、チャンクの最初のフレームを指します。再オープン後も連続し、
キューで音声が失われると飛びが現れます。生成元のタイムラインから設定してください。
時点 0 から始まる合成チャンクを 1 個作る場合は `frame_index: 0` を使います。
C の `FlexChunk` v1 のレイアウトは変わりません。

[English](README.md) | **日本語**

**Rust 向けの、汎用的で柔軟なクロスプラットフォーム音声キャプチャライブラリです。**

`flexaudio` は、**Linux**・**Windows**・**macOS** で、**マイク**、
**システム出力（ループバック）**、**個別プロセス**、**マイクとシステム出力のミックス**から音声をキャプチャする統一 API を提供します。
すべての音声ソースを、指定した出力形式のインターリーブされた `f32` ストリームに正規化し、
シンプルなポーリングループでチャンクとデバイス／ストリームのイベントを受け取れます。

```rust
use flexaudio::{open, StreamConfig, SourceKind};

let mut stream = open(StreamConfig {
    kind: SourceKind::Mic,
    ..Default::default()
})?;
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
- **0.5 の VAD:** 従来の `Vad` に加え、`WhisperVad` が whisper.cpp 互換の後処理と
  キャプチャに接続するタップを提供します。Rust・Node.js・Python・C に対応し、
  どちらも埋め込みモデルでオフライン動作します。
- **型付き診断:** 音声損失、クリッピング、権限、一時的／終端のエラーのイベントと、
  保持されたキャプチャ／後片付けの失敗を確認できます。

---

## インストール

```toml
[dependencies]
flexaudio = "0.5"
```

または、次のコマンドを使います。

```sh
cargo add flexaudio@0.5
```

音声区間検出（VAD）のアドオンは、別のクレートです。

```sh
cargo add flexaudio-vad@0.5
```

---

## 最小構成の例

```rust
use flexaudio::{open, StreamConfig, SourceKind, OutputFormat};

let config = StreamConfig {
    kind: SourceKind::Mic,
    output: OutputFormat { sample_rate: 16_000, channels: 1 },
    ..Default::default()
};
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
  不完全な列挙や列挙の失敗はエラーを返します。Linux で PipeWire デーモンへ接続できない場合も同様です。
  macOS の列挙は同意ダイアログを表示しません。マイクの同意が未決定なら、HAL／デバイス／形式を
  問い合わせる前に型付き `Backend` エラーを返します。責任を持つホストのアクセスを許可するか、
  待機時間に上限のあるキャプチャのオープンで同意を確定し、再度列挙してください。
  拒否／制限は `PermissionDenied` になります。DeviceInfo のサンプルレートとチャンネル数は必須で、
  一部だけの一覧や不明な形式を補ったレコードは返しません。
  Windows の列挙では AppCapability によるマイクの権限の事前確認を行わず、
  ネイティブの一覧取得を進めます。キャプチャには従来の別の権限事前確認を適用します。
- `flexaudio::processes() -> Result<Vec<ProcessInfo>>` — Linux/Windows では音声出力のセッション／ストリームを持つプロセスを、
  macOS では入力のみのプロセスを含む Core Audio のプロセスを列挙します
  （[キャプチャ可能なプロセスの列挙](#listing-capturable-processes)を参照）。
  待機中・停止中のプロセスも含まれます。現在再生中かどうかは `is_output_active` で確認し、
  プロセス単位のキャプチャには `pid` を `target_pid` として渡してください。
- `flexaudio::watch_devices() -> Result<DeviceWatcher>` — デバイスの接続・取り外しに関する通知
  （追加／削除／デフォルトの変更）をポーリングで受け取れます。
  Linux で開始に失敗するとエラーを返します。Windows/macOS では、現在は何も行わないウォッチャーを返します。
- 再エクスポートされる型: `StreamConfig`, `SourceKind`, `ProcessMode`, `OutputFormat`,
  `AudioChunk`, `SecondaryChunk`, `ChunkFlags`, `DeviceInfo`, `ProcessInfo`,
  `DeviceEvent`, `DefaultDeviceKind`, `Event`, `Permission`, `Error`, `ErrorKind`,
  `ErrorContext`, `AudioLoss`, `ShutdownReport`, `Result`。

音声区間検出（`flexaudio-vad`）では、`Vad::new` / `Vad::process` でストリーミングの
`SpeechStart` / `SpeechEnd` イベントを取得し、`get_speech_timestamps` でバッチ処理による区間分割を行います。
Silero VAD モデルはバイナリに埋め込まれているため、実行時のモデルファイルやネットワークアクセスを必要とせず、
完全にオフラインで動作します。0.5 では `WhisperVad` / `WhisperVadPostProcessor` /
`whisper_speech_segments` が whisper.cpp から移植した後処理を提供します。確定区間は
10 ms 刻みの整数ミリ秒で返し、任意の暫定発話／分割イベントと、末尾の不完全フレームを
パディングして推論する機能があります。互換性の対象は固定された後処理アルゴリズムです。
異なるモデルバージョンで推論確率が一致するとは限りません。`WhisperVadTap` は
基準となるキャプチャへ接続し、エポックの時計の基点と flush／停止時の終了イベントを届けます。
キャプチャのフレーム番号が飛んだ場合や不連続が通知された場合は、そこまでの区間を確定して閉じ、
次の受け付けたフレームから新しいエポックとして基点を取り直します。欠落した音声をパディングで
埋めることはありません。VAD セッション自体が失敗した場合は、次の不連続か成功した flush まで
そのセッションを失敗状態のまま保持し、キャプチャ側は動き続けます（影響を受けたエポックは
型付きエラー終端で一度だけ閉じます）。
同じストリームで従来の VAD と WhisperVad を同時に有効にすることはできません。
0.5.3 では、従来の VAD で不連続時の flush が成功した後に reset や処理が失敗しても、
確定済みの終了イベントを元の時刻で一度だけ届けます。元の失敗も報告し、Python/C では
次のポーリング用にゼロフレームのチャンクへ保持します。

---

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

Node.js では u64 カウンター（VAD の `atSample`、チャンク損失数、音声損失数を含む）を
bigint で計算してください。PTS は number のままです。チャンクの型には `frameIndex` が
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

let mut stream = open(StreamConfig {
    kind: SourceKind::Mix,
    mix_mic_gain: 1.0,
    mix_system_gain: 0.5,
    ..Default::default()
})?;
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

Node.js では `onEvent` を渡すと、該当世代の最初の `onChunk` と接続した Whisper VAD の
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
実行中に失敗するとキャプチャのデータ供給を停止し、既存の停止検知・再オープンのエラー経路に従います。
除外を指定したシステムキャプチャを含むプロセスループバックでは、この仕組みを使いません。

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
| Rust | `StreamConfig { exclude_pids: vec![1234], ..Default::default() }` |
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
    let mut stream = open(StreamConfig {
        kind: SourceKind::ProcessLoopback,
        target_pid: Some(p.pid),
        ..Default::default()
    })?;
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

組み込みバックエンドは、すべての OS とキャプチャソースで `PermissionPending` も
`PermissionGranted` も発行しません。これらのイベントの選択肢は、カスタム Rust バックエンドと
互換性のために保持します。macOS の認可状態を照会できない場合は、キャプチャを開始する前に
型付きバックエンドエラーを返し、権限が拒否されたと推測しません。
マイクの構成が事前に通知したネイティブ形式と
異なる場合は、キャプチャを構築する前に `Error::NativeFormatChanged { advertised, actual }`
で拒否します。誤った形式でサンプルを解釈して配信せず、現在のデバイス形式を使ってストリームを作り直してください。

Rust では `Stream::terminal_error()`、N-API では `terminalError()` を提供し、N-API の
`stop()` はキャプチャまたは後片付けの失敗で拒否されます。即時の通知には `onEvent` を渡し、
`shutdownReport()` で停止結果を確認してください。Python の `poll_chunk()` は終端の失敗で
`RuntimeError` の型付きサブクラスを送出し、`terminal_error()` はイベントを消費せずに
保持された `AudioError` を返します。C v1 の `flexaudio_poll_chunk()` は、キャプチャの終端エラーで
`FLEX_FAILURE` (-2) を返します。C v2 の `flexaudio_poll_chunk_v2()` と `flexaudio_terminal_error()` は
主因の結果コード（例: `FLEX_PERMISSION_DENIED` (-7)、バックエンドの失敗は `FLEX_FAILURE` (-2)）を
返し、安全な説明は `flexaudio_last_error()` で取得できます。
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
  この信号は聞こえないように設計されています。独立した非公開の tap が自分自身のプロセスだけをキャプチャし、
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

- マイクのキャプチャは、`cpal` を介して ALSA/PipeWire で行います。
  ユーザーには音声デバイスへのアクセス権が必要です。通常は、`audio` グループへの所属、
  または実行中の PipeWire や PulseAudio セッションによって提供されます。
- システム出力とプロセス単位のキャプチャには、実行中の **PipeWire** セッションが必要です。
  プロセス／除外キャプチャに適格な対象がある場合は、通常のモニターと共通の 2 秒以内に
  形式のネゴシエーションを完了しなければ、型付きの原因と対処方法を持つエラーでオープンが失敗します。
  完全な対象一覧に適格なソースがなければ、オープンは成功し、その後の再生を監視し続けます。
  PipeWire がない場合やウォッチャーの開始に失敗した場合、`watch_devices()` はエラーを返します。
  デバイス列挙はすべての提供元の完全な一覧を必要とし、不完全な照会や照会の失敗はエラーになります。
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
互換性のある更新だけを受け取るには、バージョンを `0.5` に固定してください。
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
