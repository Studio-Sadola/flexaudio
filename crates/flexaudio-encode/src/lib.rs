//! flexaudio-encode — 録音チャンクを逐次 FLAC ファイルへ圧縮保存するアドオン。
//!
//! `flexaudio-core` には依存せず、interleaved `&[f32]` のサンプル列だけを受け取る。
//! エンコードは純 Rust の flacenc で行うので、システムライブラリも実行時ネットワークも
//! 要らない。長時間録音を WAV のまま置くとギガバイト級になるところを、可逆のまま
//! おおむね半分前後まで圧縮できる（例: 3 時間の会議録音 WAV 約 2GB → FLAC 数百 MB）。
//!
//! チャンクを受け取るたびにブロック単位でエンコードしてファイルへ流すため、録音の
//! 長さに関係なくメモリ使用量は一定。ストリーム情報（総サンプル数・MD5 など）は
//! [`FlacWriter::finalize`] でヘッダに書き戻して確定する。
//!
//! # 例
//! ```no_run
//! use flexaudio_encode::FlacWriter;
//!
//! // flexaudio の正規形 (48kHz / stereo) をそのまま渡す想定。
//! let mut writer = FlacWriter::create("meeting.flac", 48_000, 2).unwrap();
//! for chunk in some_audio_chunks() {
//!     writer.write_chunk(chunk).unwrap();
//! }
//! writer.finalize().unwrap();
//! # fn some_audio_chunks() -> Vec<&'static [f32]> { vec![] }
//! ```

#![warn(missing_docs)]

mod error;
mod writer;

pub use error::{EncodeError, Result};
pub use writer::FlacWriter;
