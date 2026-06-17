//! flexaudio-core — OS 非依存コア。
//!
//! デスクトップ音声キャプチャ抽象化ライブラリ `flexaudio` の OS 非依存部分。
//! リングバッファ / SR 変換 / チャンネル mix / 20ms チャンク化 / クロック正規化 /
//! イベント・型定義を提供する。OS 固有のキャプチャは [`backend::CaptureBackend`] を
//! 実装する別 crate（`flexaudio-os-*`）が担い、facade 層が両者を配線する。
//!
//! # 固定契約
//! 内部処理はすべて interleaved `f32` / 48000 Hz / ステレオ 2ch / 20ms = 960
//! frames/chunk で行う。外部へ出すレート/チャンネルは [`OutputFormat`] で変えられ
//! （Normalizer 第 2 段が再変換。例 16k/1ch は 320 frames/chunk）、出力チャンクは
//! レートに依らず時間ベースで 20ms。
//!
//! 公開 API にコールバックは無い。RT スレッドは push のみ、消費側は poll する。
//! RT 経路は非ブロッキング（満杯時は DROP_OLDEST / overflow ドロップ）。PTS は
//! デバイス由来で、ギャップを検知する。
//!
//! # 2 段リングバッファ構成
//! ```text
//! [RT cb] --push--> RawRing (rtrb, RT安全) --pop--> [取り込み/加工スレッド]
//!                                                       |
//!                                          Normalizer (mix + rubato SRC + 960切出)
//!                                                       |
//!                                                       v
//!                                       ChunkRing (ringbuf, DROP_OLDEST) --try_pop--> [poll]
//! ```

#![warn(missing_docs)]

pub mod backend;
pub mod chunk_ring;
pub mod clock;
pub mod normalizer;
pub mod raw_ring;
pub mod types;

// 主要型をクレート直下へ再エクスポート。
pub use backend::{CaptureBackend, RawSink};
pub use chunk_ring::{chunk_ring, ChunkConsumer, ChunkProducer};
pub use clock::{monotonic_now_ns, ClockNormalizer};
pub use normalizer::{Normalizer, CHUNK_FRAMES};
pub use raw_ring::{raw_ring, RawConsumer, RawProducer};
pub use types::{
    AudioChunk, ChunkFlags, DeviceEvent, DeviceInfo, Error, Event, OutputFormat, Result,
    SourceKind, StreamConfig, CHANNELS, SAMPLE_RATE,
};
