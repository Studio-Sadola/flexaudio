# flexaudio

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

---

## インストール

```toml
[dependencies]
flexaudio = "0.4"
```

または、次のコマンドを使います。

```sh
cargo add flexaudio@0.4
```

音声区間検出（VAD）のアドオンは、別のクレートです。

```sh
cargo add flexaudio-vad
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
  停止したストリームは再利用できません。再びキャプチャするには、新しいストリームを開いてください。
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
  `DeviceEvent`, `Event`, `Permission`, `Error`, `Result`。

音声区間検出（`flexaudio-vad`）では、`Vad::new` / `Vad::process` でストリーミングの
`SpeechStart` / `SpeechEnd` イベントを取得し、`get_speech_timestamps` でバッチ処理による区間分割を行います。
Silero VAD モデルはバイナリに埋め込まれているため、実行時のモデルファイルやネットワークアクセスを必要とせず、
完全にオフラインで動作します。

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
メッセージは対象の権限と原因、変更するプライバシー設定、アプリを再起動して再試行する手順を示します。
キャプチャ中に拒否を検出すると `Event::PermissionDenied { permission, detail }` を発行し、
キャプチャ（Mix では両方の入力）を終了して、それ以降の音声の配信と自動的な再オープンを抑止します。
権限を修正した後は新しいストリームを作成してください。終端エラーのあるストリームの
`start`、`resume`、`switch_source` は保持されたエラーを返します。

macOS の許可監視で認可状態を照会できない場合は、
`Event::TerminalError { error }` によりキャプチャを安全側に停止し、元のバックエンドエラーを保持します。
この場合、権限が拒否されたと推測することはありません。各バインディングはこれを `error`
イベントとして通知し、同じ終端エラーを参照できます。マイクの構成が事前に通知したネイティブ形式と
異なる場合は、キャプチャを構築する前に `Error::NativeFormatChanged { advertised, actual }`
で拒否します。誤った形式でサンプルを解釈して配信せず、現在のデバイス形式を使ってストリームを作り直してください。

Rust では `Stream::terminal_error()`、N-API では `terminalError()` を提供し、N-API の
`stop()` は終端エラーがあると拒否されます。即時の通知には `onEvent` を渡してください。
Python の `poll_chunk()` は `RuntimeError` を送出し、`terminal_error()` はイベントを消費せずに
エラーを確認できます。C の `flexaudio_poll_chunk()` と `flexaudio_terminal_error()` は
`FLEX_FAILURE` (-2) を返し、説明は `flexaudio_last_error()` で取得できます。
C の権限イベント種別は引き続き 3 です。N-API/Python の権限イベントの type は
`permissionDenied` のままで、`permission`（`microphone` または `systemAudio`）と `message` が追加されます。

### macOS

- **マイク**（Mix のマイク入力も含みます）: flexaudio は公開されている AVFoundation の認可状態を確認します。
  拒否または制限されている場合はキャプチャを開始する前に失敗します。状態が未決定で、メインのアプリバンドルに
  空でない `NSMicrophoneUsageDescription` があれば、同意を要求して最大 30 秒待ちます。
  拒否は権限エラーになります。要求が未回答の場合はキャプチャを進め、認可状態の監視を継続します。
  GUI アプリではイベントループを止めないよう、ワーカースレッドでオープンしてください。
- Terminal 内の単体 CLI では、メインバンドルにマイクの用途説明がない場合があります。
  この場合、flexaudio は直接同意を要求せず、macOS が責任を持つアプリのためにダイアログを表示できるよう
  キャプチャのオープンを進めます。同意が未決定の間は、キャプチャの全期間にわたり認可状態を確認します。
  最初の 60 秒間は 500 ms ごと、その後は 2 秒ごとに確認します。
  遅れて拒否／制限が確認されると終端の権限イベントを発行し、許可を確認するとポーリングを終了します。
  ダイアログが未回答であることだけでは、拒否と判断できません。
- キャプチャ開始から 5 秒後もマイクへの同意が未決定の場合、Mix のマイク入力も含め、
  `Event::PermissionPending { permission, detail }` をキャプチャ世代ごとに 1 回発行します。
  これは助言であり、キャプチャは継続します。権限が許可されるまで無音になる場合があります。
  **システム設定 > プライバシーとセキュリティ > マイク**を確認してください。
  SSH、launchd、その他のバックグラウンド環境ではダイアログが表示されない場合があります。
  Terminal から実行するか、空でない `NSMicrophoneUsageDescription` を持つアプリバンドルを使ってください。
  N-API/Python では type が `permissionPending` となり、`permission` と `message` を持ちます。
  C ではイベント種別 8 となり、`flexaudio_last_error()` で手順を取得できます。
  CLI は警告を表示して継続します。助言を受け取るには N-API の `onEvent` を渡すか、
  Rust/Python/C のイベントをポーリングしてください。終端エラーの参照 API は終端の失敗のみを報告します。
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
  PipeWire がない場合も、`devices()` は cpal が検出したマイクを返し、PipeWire のデバイスだけが含まれなくなります。
  `watch_devices()` はエラーにせず、何も行わないウォッチャーとして動作します。
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
互換性のある更新だけを受け取るには、バージョンを `0.4` に固定してください。
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
