//! flexaudio-os-windows — Windows バックエンド: WASAPI ループバック / プロセス
//! ループバック（windows-rs 0.54, Win11+）。
//!
//! 2 つの [`CaptureBackend`](flexaudio_core::backend::CaptureBackend) を提供する:
//!
//! - [`WasapiSystemBackend`] — 既定 render endpoint の古典 loopback
//!   （`AUDCLNT_STREAMFLAGS_LOOPBACK`）でシステム音声出力全体（スピーカーへ流れている
//!   ミックス）を録る。Linux の [`PwSystemBackend`](../flexaudio_os_linux) 相当。
//! - [`WasapiProcessBackend`] — `ActivateAudioInterfaceAsync` + プロセスループバック
//!   （`AUDIOCLIENT_ACTIVATION_PARAMS`）で特定 PID（そのプロセスツリー）の音声を録る。
//!   `exclude_self` で「対象ツリーを除く全システム音」へ反転する。
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
//! `#![cfg(target_os = "windows")]` で非 Windows では空コンパイルになり、`windows`
//! 依存も `Cargo.toml` の `target.'cfg(...windows)'` セクションでしか引かれない。

#![cfg(target_os = "windows")]
#![warn(missing_docs)]

mod common;
mod process;
mod system;

pub use process::WasapiProcessBackend;
pub use system::WasapiSystemBackend;
