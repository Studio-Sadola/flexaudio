# flexaudio

[English](README.md) | **日本語**

**汎用クロスプラットフォーム音声キャプチャライブラリ（Rust 製）。**

`flexaudio` は **マイク**、**システム出力（loopback）**、**個別プロセス** からの音声キャプチャを、**Linux**・**Windows**・**macOS** にわたって単一の統一 API で提供します。すべてのソースを interleaved な `f32` ストリームとして選択した出力フォーマットに正規化し、シンプルなポーリングループでチャンクとデバイス／ストリームイベントを受け取れます。

```rust
use flexaudio::{open, StreamConfig, SourceKind};

let mut stream = open(StreamConfig {
    kind: SourceKind::Mic,
    ..Default::default()
})?;
stream.start()?;
while let Some(chunk) = stream.poll_chunk() {
    // chunk.data は選択した OutputFormat の interleaved f32
    let _ = (chunk.frames, chunk.peak, chunk.rms);
}
stream.stop();
# Ok::<(), flexaudio::Error>(())
```

---

## ケイパビリティマトリクス（「9マス」）

3 種のキャプチャソース × 3 つの OS。✅ = 実装済み・検証済み、— = そのプラットフォームでは利用不可。

| ソース              | Linux            | Windows           | macOS                       |
|---------------------|------------------|-------------------|-----------------------------|
| **マイク**          | ✅ (cpal/ALSA)   | ✅ (cpal/WASAPI)  | ✅ (cpal/CoreAudio)         |
| **システム出力**    | ✅ (PipeWire)    | ✅ (WASAPI loopback) | ✅ (CoreAudio process taps) |
| **プロセス単位**    | ✅ (PipeWire)    | ✅ (WASAPI process loopback) | ✅ (CoreAudio process taps) |

- **マイク** はすべてのプラットフォームで [`cpal`] を通じて動作します。
- **システム / プロセス単位** のキャプチャは、コンパイル時に選択されたネイティブ OS バックエンドを使用します。特定の OS でサポートされていないソースを呼び出した場合は `Error::Unsupported` が返ります。
- プロセス単位のキャプチャには `StreamConfig` の `target_pid` が必要です。

---

## インストール

```toml
[dependencies]
flexaudio = "0.3"
```

または:

```sh
cargo add flexaudio
```

Voice Activity Detection アドオンは別クレートです:

```sh
cargo add flexaudio-vad
```

---

## 最小サンプル

```rust
use flexaudio::{open, StreamConfig, SourceKind, OutputFormat};

let config = StreamConfig {
    kind: SourceKind::Mic,
    output: OutputFormat { sample_rate: 16_000, channels: 1 },
    ..Default::default()
};
let mut stream = open(config)?;
stream.start()?;

// チャンク（interleaved f32）とストリームレベルイベントを取り出す。
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

## 公開 API 概要

ファサードクレート `flexaudio` は必要なものをすべて re-export します:

- `flexaudio::open(StreamConfig) -> Result<Stream>` — ソースと OS からバックエンドを選択し、（まだ開始されていない）キャプチャストリームを生成します。
- `Stream::start` / `Stream::stop` — キャプチャの制御。
- `Stream::poll_chunk` / `Stream::poll_event` — `AudioChunk` と `Event` を取り出します。
- `Stream::switch_source` — ストリームを停止せずに入力ソースをホットスワップします（チャンクの `seq` は連続したまま維持され、切替後の最初のチャンクに非連続フラグが付きます）。
- `flexaudio::devices() -> Result<Vec<DeviceInfo>>` — マイク（cpal・全プラットフォーム）とシステム出力エンドポイント（Linux: PipeWire の sink と source、Windows: 有効な render エンドポイント、macOS: 出力デバイス）を 1 つのリストで取得します。
- `flexaudio::processes() -> Result<Vec<ProcessInfo>>` — 音声出力のセッション（ストリーム）を持ち、プロセス単位でキャプチャできるプロセスを一覧で取得します（[録れるプロセスの列挙](#録れるプロセスの列挙) を参照）。停止中・Idle も載り、今鳴っているかは `is_output_active` で見ます。
- `flexaudio::watch_devices() -> Result<DeviceWatcher>` — プル型のホットプラグ通知（追加 / 削除 / デフォルト変更）を取得します（Linux のみ、Windows/macOS はノーオップのウォッチャーを返します）。
- Re-export された型: `StreamConfig`, `SourceKind`, `ProcessMode`, `OutputFormat`,
  `AudioChunk`, `SecondaryChunk`, `ChunkFlags`, `DeviceInfo`, `ProcessInfo`,
  `DeviceEvent`, `Event`, `Error`, `Result`。

Voice activity detection (`flexaudio-vad`): ストリーミング `SpeechStart` / `SpeechEnd` イベントには `Vad::new` / `Vad::process`、バッチ分割には `get_speech_timestamps` を使用します。Silero VAD モデルはバイナリに埋め込まれており、VAD はランタイムモデルファイルやネットワークアクセスなしで完全オフライン動作します。

---

## 録れるプロセスの列挙

`flexaudio::processes()` は、プロセス単位キャプチャ（`SourceKind::ProcessLoopback` + `target_pid`）に渡せるプロセスを返します。呼び出し元プロセス自身は除外され、同じ PID は 1 件にまとめられ、出力中のプロセスが先頭 → 名前 → PID の順に並びます。

| 項目 / OS | Linux（PipeWire） | Windows（WASAPI） | macOS（Core Audio） |
|---|---|---|---|
| 列挙の元 | `Stream/Output/Audio` ノードを持つ Client | 有効な全 render エンドポイントの音声セッション（システム音・期限切れセッションは除外） | Core Audio が把握しているプロセスオブジェクト（`kAudioHardwarePropertyProcessObjectList`。入力だけのプロセスも含む） |
| `pid` | Client の `pipewire.sec.pid`（キャプチャ側と同じ解決経路） | `IAudioSessionControl2::GetProcessId` | `kAudioProcessPropertyPID` |
| `name` | ノード（無ければ Client）の `application.name` | イメージ名（`.exe` を除く） | 実行ファイル名 |
| `executable` | `/proc/<pid>/exe` のベース名。読めなければ `/proc/<pid>/comm` | プロセスイメージのベース名 | `proc_pidpath` のベース名 |
| `bundle_id` | — | — | `kAudioProcessPropertyBundleID` |
| `is_output_active` | ノード状態が `Running` | セッション状態が `Active` | `kAudioProcessPropertyIsRunningOutput` |
| 必要条件 | PipeWire セッションが動いていること | Windows build 20348 以上（Windows 11・Windows Server 2022） | macOS 14.4 以降（未満は `Error::UnsupportedOsVersion`） |

`name` は常に非空です（実行ファイル名 → bundle ID → `pid <N>` の順で補完）。`executable` / `bundle_id` / `is_output_active` は OS が公開しない場合 `None` になります。名前はアプリ自身が名乗る値を含むので表示専用で、キーは PID です。

戻り値の読み方:

- `Ok(空でないリスト)` — プロセス単位キャプチャが使え、音声出力のセッション（ストリーム）を持つプロセスがある。停止中・Idle も載る。今鳴っているかは `is_output_active` で見る。
- `Ok(空)` — プロセス単位キャプチャは使えるが、そういうプロセスが今は無い（「何も鳴っていない」ではない）。
- `Err(..)` — この環境ではプロセス単位キャプチャができない（Linux: PipeWire に届かない → `Error::Backend`、macOS 14.4 未満 / Windows build 20348 未満 → `Error::UnsupportedOsVersion`、その他の OS → `Error::Unsupported`）、権限が無い（`Error::PermissionDenied`）、OS が時間内に応答しなかった、または前の問い合わせがまだ終わっていない（`Error::Backend`）。

読み取り専用で権限プロンプトは出さず、OS の音声サービスが応答しなくても 3 秒以内に必ず戻ります。N-API では `await processes()` / `await stream.stop()` です（JS のイベントループを塞がない）。

---

## OS 別パーミッション要件

flexaudio は音声をキャプチャするため、各プラットフォームはユーザーの許可を要求します。アプリケーションは関連するプロンプトのトリガーまたは必要なエンタイトルメントの宣言に責任を持ちます。

### macOS

システムおよびプロセス単位の音声キャプチャは Core Audio process taps を使用し、`kTCCServiceAudioCapture` 配下の **TCC** プライバシーサブシステムによって管理されます。

- アプリの `Info.plist` に usage description 文字列を追加してください:
  ```xml
  <key>NSAudioCaptureUsageDescription</key>
  <string>This app records system and application audio.</string>
  ```
  （マイクのみのキャプチャにはさらに `NSMicrophoneUsageDescription` が必要です。）
- OS は一度限りの同意プロンプトを表示します。ユーザーが承認するまでは `PermissionDenied` イベント／エラーとして返ります。
- process taps には macOS 14.4 以降が必要です。

### Windows

- マイクキャプチャは **マイク** プライバシー設定（設定 → プライバシーとセキュリティ → マイク）によって管理されます。拒否されたアプリには `PermissionDenied` が返ります。
- システム（WASAPI loopback）およびプロセス単位の loopback キャプチャは標準の WASAPI レンダーエンドポイント loopback / process-loopback API（Windows 10/11）を使用します。デスクトップアプリには特別なマニフェスト capability は不要ですが、マイクキャプチャにはマイクプライバシーゲートが引き続き適用されます。

### Linux

- マイクキャプチャは `cpal` を介して ALSA/PipeWire で行われます。ユーザーはオーディオデバイスへのアクセス権（通常は `audio` グループへの所属、または PipeWire もしくは PulseAudio セッションの実行）が必要です。
- システムおよびプロセス単位のキャプチャには **PipeWire** セッションが実行中である必要があります。PipeWire が存在しない場合、`devices()` は空のリストを返し、`watch_devices()` は失敗するのではなくノーオップにデグレードします。ポータルベースのデスクトップ環境では、ユーザーがキャプチャアクセスの許可を求められる場合があります。

---

## 対応 Rust バージョン（MSRV）

- **コア / ファサード / OS バックエンド / マイク:** Rust **1.85**。
- **`flexaudio-vad` および `flexaudio-napi`:** Rust **1.88**（`ort` / `napi-build` ツールチェーン依存に必要）。

ワークスペースは各クレートの `rust-version` で MSRV を固定しています。

---

## バージョニングポリシー（SemVer / 0.x）

flexaudio は [Semantic Versioning](https://semver.org/) に従います。クレートが **0.x** シリーズにある間は、公開 API は**まだ安定していません**: SemVer に従い、**マイナー** バージョンのバンプ（`0.2 → 0.3`）には破壊的変更が含まれる場合があります。一方、**パッチ** バンプ（`0.2.0 → 0.2.1`）は後方互換です。互換アップデートのみを受け取るには `0.3` にピン留めしてください。[`CHANGELOG.md`](CHANGELOG.md) を参照してください。

---

## ワークスペース構成

| クレート | crates.io | 説明 |
|----------|-----------|------|
| `flexaudio` | ✅ | ファサード: 統一 `open()` / `devices()` / `processes()` / `watch_devices()`。 |
| `flexaudio-core` | ✅ | ソース非依存のストリームエンジン、型定義、リサンプリング／ノーマライザー。 |
| `flexaudio-mic` | ✅ | マイクバックエンド（cpal）、全プラットフォーム対応。 |
| `flexaudio-os-linux` | ✅ | PipeWire システム / プロセス単位バックエンド（Linux）。 |
| `flexaudio-os-windows` | ✅ | WASAPI loopback / プロセスバックエンド（Windows）。 |
| `flexaudio-os-macos` | ✅ | Core Audio process-tap バックエンド（macOS）。 |
| `flexaudio-vad` | ✅ | Silero VAD アドオン（オフライン、モデル埋め込み済み）。 |
| `flexaudio-cli` | — | リファレンス CLI / ストリーミングキャプチャツール。 |
| `flexaudio-napi` | — (npm) | Node.js N-API アドオン（npm に公開、crates.io には非公開）。 |
| `flexaudio-ffi` | — | C ABI（プル型キャプチャ、VAD / FLAC / denoise、`flexaudio_processes`）。 |
| `bindings/flexaudio-py` | — | PyO3 Python バインディング（`open` / `devices` / `processes` / アドオン）。 |

---

## ライセンス

[MIT](LICENSE) © 2026 tubome / Studio Sadola.

このプロジェクトはサードパーティのソフトウェア（Silero VAD モデル、ONNX Runtime、PipeWire、およびパーミッシブライセンスの Rust クレート）をバンドル / リンクしています。必要な表示については [`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md) を参照してください。
