//! flexaudio-os-macos — macOS バックエンド。Core Audio Process Taps
//! (objc2-core-audio, macOS 14.4+) を使う。
//!
//! システム音声出力全体（[`MacSystemBackend`]）と特定プロセス（[`MacProcessBackend`]）を
//! Process Tap で録る。Windows の WASAPI loopback / Linux の PipeWire monitor に相当する。
//!
//! # アーキテクチャ
//! tap チェーン（`CATapDescription` → process tap → private aggregate device →
//! IOProc block → start）は [`tap`] モジュールにまとめてあり、両バックエンドは
//! INCLUDE/EXCLUDE の `TapKind` を切り替えて同じチェーンを回す。`!Send` な ObjC オブジェクト
//! （`Retained<CATapDescription>` / `RcBlock` / `TapChain`）はバックエンドの専用スレッド内に
//! 閉じ込め、`Send` な本体（停止フラグ・`JoinHandle`・フォーマット）だけがスレッドを跨ぐ
//! （cpal / Windows / Linux バックエンドと同じ作り）。
//!
//! # 権限（TCC）
//! システム/プロセス音声キャプチャは TCC の `kTCCServiceAudioCapture` を要求する
//! （Info.plist の `NSAudioCaptureUsageDescription`）。private TCC SPI は使わず、権限の可否は
//! 初回キャプチャ時の OS プロンプトに委ねる。tap 作成が未承認で弾かれた場合は
//! [`map_os_status`](common::map_os_status) が権限拒否系 OSStatus を
//! [`Error::PermissionDenied`](flexaudio_core::types::Error) へ寄せる。
//!
//! # 非 macOS
//! macOS 専用。`#![cfg(target_os = "macos")]` で非 macOS では空コンパイルになり、objc2 系依存も
//! `Cargo.toml` の `target.'cfg(...macos)'` セクションでのみ引かれる（Linux/Windows ビルドは無傷）。

#![cfg(target_os = "macos")]
#![warn(missing_docs)]

mod common;
mod devices;
mod process;
mod system;
mod tap;
mod version;

pub use devices::list_output_devices;
pub use process::MacProcessBackend;
pub use system::MacSystemBackend;
