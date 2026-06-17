//! flexaudio-core — OS 非依存コア。
//!
//! デスクトップ音声キャプチャ抽象化ライブラリ `flexaudio` の OS 非依存中核。
//! リングバッファ / SR 変換 / チャンネル mix / 20ms チャンク化 / クロック正規化 /
//! イベント・型定義を提供する。OS 固有のキャプチャは [`backend::CaptureBackend`] を
//! 実装する別 crate（`flexaudio-os-*`）が担い、facade 層が両者を配線する。
//!
//! # 固定契約（逸脱不可）
//! - 出力 [`AudioChunk`]: **interleaved `f32` / 48000 Hz / ステレオ 2ch /
//!   20ms = 960 frames/chunk**。
//! - **プル型**: 公開 API にコールバックを置かない。RT スレッドは push のみ、
//!   消費側は poll。
//! - RT 経路は非ブロッキング（DROP_OLDEST / overflow ドロップ）、デバイス由来 PTS +
//!   ギャップ検知。
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
