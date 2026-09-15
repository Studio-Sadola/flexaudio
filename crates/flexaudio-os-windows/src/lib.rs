//! flexaudio-os-windows — Windows バックエンド: WASAPI ループバック / プロセス
//! ループバック（windows-rs 0.54, Windows build 20348 or later）。
//!
//! 2 つの [`CaptureBackend`](flexaudio_core::backend::CaptureBackend) を提供する:
//!
//! - [`WasapiSystemBackend`] — render endpoint の古典 loopback
//!   （`AUDCLNT_STREAMFLAGS_LOOPBACK`）でシステム音声出力（そのエンドポイントへ流れている
//!   ミックス）を録る。`device_id` で出力エンドポイントを選べる（`None` で既定）。Linux の
//!   [`PwSystemBackend`](../flexaudio_os_linux) 相当。出力エンドポイントの一覧は
//!   [`list_output_devices`] で取れる。
//! - [`WasapiProcessBackend`] — `ActivateAudioInterfaceAsync` + プロセスループバック
//!   （`AUDIOCLIENT_ACTIVATION_PARAMS`）で特定 PID（そのプロセスツリー）の音声を録る。
//!   `exclude_self` で「対象ツリーを除く全システム音」へ反転する。
//!
//! 録れるプロセスの候補（音声セッションを持つプロセス）は [`list_processes`] で取れる。
//!
//! # `!Send` 回避
//!
//! WASAPI の `IAudioClient` 等の COM インターフェイスは `!Send` だが、コア契約
//! [`CaptureBackend`] は `Send` を要求する。COM の初期化からキャプチャ、破棄までを専用
//! スレッド 1 本の上で完結させ、バックエンド構造体が持つのは `Send` なものだけ（停止フラグ
//! [`AtomicBool`] / [`JoinHandle`] / キャッシュ済みフォーマット）にする。COM インター
//! フェイスはスレッド境界を跨がない。cpal / PipeWire backend と同じ作り。
//!
//! # 非 Windows
//!
//! バックエンド本体は `#[cfg(target_os = "windows")]` で非 Windows では空コンパイルに
//! なり、`windows` 依存も `Cargo.toml` の `target.'cfg(...windows)'` セクションでしか
//! 引かれない。ビルド番号の判定（純粋関数）だけは非 Windows でもコンパイルし、単体
//! テストする。

#![warn(missing_docs)]

/// ビルド番号 → プロセスループバック可否。OS 呼び出しから切り離してあるので
/// 非 Windows でも単体テストできる。
mod version;

#[cfg(target_os = "windows")]
mod common;
#[cfg(target_os = "windows")]
mod process;
#[cfg(target_os = "windows")]
mod processes;
#[cfg(target_os = "windows")]
mod system;

#[cfg(target_os = "windows")]
pub use process::WasapiProcessBackend;
#[cfg(target_os = "windows")]
pub use processes::list_processes;
#[cfg(target_os = "windows")]
pub use system::{list_output_devices, WasapiSystemBackend};
