//! FLAC ストリーミング書き出し ([`FlacWriter`])。
//!
//! flacenc の主入口 `encode_with_fixed_block_size` は全サンプルを [`Source`] から
//! 一括で読む設計で、長時間録音では全データをメモリに抱えることになる。ここでは
//! フレーム単位の入口 [`flacenc::encode_fixed_size_frame`] を使い、ブロック
//! （4096 サンプル/ch）が溜まるたびにエンコードしてファイルへ流す。
//!
//! STREAMINFO ヘッダは作成時に仮の値（総サンプル数 0・MD5 ゼロ）で書いておき、
//! [`FlacWriter::finalize`] で先頭へシークして確定値に書き換える。STREAMINFO は
//! 固定長 34 バイトなのでヘッダ全長は常に 42 バイトで変わらず、上書きで安全に
//! 差し替えられる。
//!
//! [`Source`]: flacenc::source::Source

use std::fs::File;
use std::io::{BufWriter, Seek, Write};
use std::path::Path;

use flacenc::bitsink::ByteSink;
use flacenc::component::{BitRepr, Stream, StreamInfo};
use flacenc::config::Encoder as EncoderConfig;
use flacenc::error::{Verified, Verify};
use flacenc::source::{Context, Fill, FrameBuf};

use crate::error::{EncodeError, Result};

/// 1 フレームあたりのブロックサイズ（サンプル/チャンネル）。flacenc の既定値と同じ。
const BLOCK_SIZE: usize = 4096;

/// 量子化ビット深度。現状 16bit 固定。
/// flacenc 自体は 24bit まで対応しているので、必要になれば [`quantize_i16`] の
/// スケールとここを広げれば 24bit FLAC も書ける（現時点では余地のみ）。
const BITS_PER_SAMPLE: usize = 16;

/// 対応するサンプルレート上限 (Hz)。FLAC 形式自体は 655,350 Hz まで表せるが、
/// flacenc の検証が 96 kHz までに制限しているのでそれに合わせる。
const MAX_SAMPLE_RATE: u32 = 96_000;

/// FLAC ヘッダ全長: "fLaC" マジック 4B + メタデータブロックヘッダ 4B + STREAMINFO 34B。
const HEADER_LEN: usize = 42;

/// f32 サンプル 1 個を 16bit 整数へ量子化する（ディザなしの単純量子化）。
///
/// 量子化の正典は [`flexaudio_core::quantize_i16`]（全層で同一実装を共有する）。flacenc の
/// API 都合で `i32` を要求するので、core の `i16` 版へ委譲して昇格する。スケールは 32768
/// （負側フルスケール = -1.0 基準）・`+1.0` は 32767 へクランプ・範囲外は飽和・NaN は 0。
#[inline]
fn quantize_i16(x: f32) -> i32 {
    flexaudio_core::quantize_i16(x) as i32
}

/// flacenc 系エラーを [`EncodeError::Encoder`] に写すヘルパ。
fn enc_err(e: impl std::fmt::Display) -> EncodeError {
    EncodeError::Encoder(e.to_string())
}

/// 録音チャンクを逐次 FLAC ファイルへ書き出すライター。
///
/// interleaved `f32`（flexaudio の `AudioChunk.data` と同じ形）を
/// [`write_chunk`](FlacWriter::write_chunk) に流し、終わったら
/// [`finalize`](FlacWriter::finalize) でヘッダを確定する。エンコードは呼び出し
/// スレッド上で同期的に行う。
///
/// finalize を呼ばずに drop した場合もベストエフォートで閉じる（端数の書き出しと
/// ヘッダ確定を試み、エラーは握りつぶす）。確実に完結させたいときは finalize を
/// 呼ぶこと。
pub struct FlacWriter {
    file: BufWriter<File>,
    config: Verified<EncoderConfig>,
    /// 確定値を積算するストリーム情報。finalize でヘッダに書き戻す。
    stream_info: StreamInfo,
    /// エンコード入力用のブロックバッファ（flacenc がチャンネル分離して保持）。
    frame_buf: FrameBuf,
    /// MD5 と総サンプル数の積算（エンコードとは独立した flacenc の仕組み）。
    context: Context,
    /// フレームのビット列化に使い回すシンク。
    sink: ByteSink,
    /// ブロックサイズに満たない量子化済みサンプルの持ち越し。
    pending: Vec<i32>,
    channels: usize,
    /// 次に書くフレーム番号（固定ブロックサイズなのでフレーム単位の連番）。
    frame_number: usize,
    /// finalize 済みなら Drop で何もしない。
    finalized: bool,
}

impl FlacWriter {
    /// `path` に 16bit FLAC ファイルを新規作成する（既存ファイルは上書き）。
    ///
    /// 対応範囲は `channels` 1..=2、`sample_rate` 1..=96,000 Hz。範囲外は
    /// [`EncodeError::Unsupported`]（ファイルは作られない）。
    pub fn create<P: AsRef<Path>>(path: P, sample_rate: u32, channels: u16) -> Result<FlacWriter> {
        if !(1..=2).contains(&channels) {
            return Err(EncodeError::Unsupported(format!(
                "channels must be 1 or 2, got {channels}"
            )));
        }
        if !(1..=MAX_SAMPLE_RATE).contains(&sample_rate) {
            return Err(EncodeError::Unsupported(format!(
                "sample rate must be 1..={MAX_SAMPLE_RATE} Hz, got {sample_rate}"
            )));
        }

        let config = EncoderConfig::default()
            .into_verified()
            .map_err(|(_, e)| enc_err(e))?;
        let mut stream_info =
            StreamInfo::new(sample_rate as usize, channels as usize, BITS_PER_SAMPLE)
                .map_err(enc_err)?;
        // 固定ブロックサイズのストリームとして宣言する（flacenc の一括入口と同じ流儀）。
        stream_info
            .set_block_sizes(BLOCK_SIZE, BLOCK_SIZE)
            .map_err(enc_err)?;
        let frame_buf = FrameBuf::with_size(channels as usize, BLOCK_SIZE).map_err(enc_err)?;
        let context = Context::new(BITS_PER_SAMPLE, channels as usize);

        let mut writer = FlacWriter {
            file: BufWriter::new(File::create(path)?),
            config,
            stream_info,
            frame_buf,
            context,
            sink: ByteSink::new(),
            pending: Vec::new(),
            channels: channels as usize,
            frame_number: 0,
            finalized: false,
        };
        // 仮ヘッダ。finalize で同じ長さのまま確定値に上書きする。
        let header = writer.header_bytes()?;
        writer.file.write_all(&header)?;
        Ok(writer)
    }

    /// interleaved `f32` サンプルを追記する。
    ///
    /// 長さは `channels` の倍数であること（flexaudio の `AudioChunk.data` は
    /// そのまま渡せる）。倍数でなければ [`EncodeError::Unsupported`] を返し、
    /// 何も書かない。ブロックに満たない端数は内部に持ち越し、次の呼び出しか
    /// finalize で書かれる。
    pub fn write_chunk(&mut self, interleaved: &[f32]) -> Result<()> {
        if interleaved.len() % self.channels != 0 {
            return Err(EncodeError::Unsupported(format!(
                "chunk length {} is not a multiple of channels {}",
                interleaved.len(),
                self.channels
            )));
        }
        self.pending.reserve(interleaved.len());
        self.pending
            .extend(interleaved.iter().map(|&x| quantize_i16(x)));
        self.drain_full_blocks()
    }

    /// 端数フレームの書き出しとヘッダ確定を行い、ファイルを閉じる。
    ///
    /// self を消費するので、以後の [`write_chunk`](FlacWriter::write_chunk) は型で
    /// 不可能。Drop でも同じ処理をベストエフォートで行うが、書き込みエラーを
    /// 検知できるのはこちらだけなので finalize 推奨。
    pub fn finalize(mut self) -> Result<()> {
        let result = self.finish_inner();
        // Drop での二重実行を防ぐ（失敗していても再試行はしない）。
        self.finalized = true;
        result
    }

    /// pending から満杯ブロックをすべてエンコードして書き出す。
    fn drain_full_blocks(&mut self) -> Result<()> {
        let block_len = BLOCK_SIZE * self.channels;
        // self.pending を借用したまま &mut self の encode_block は呼べないので、
        // 一旦取り出して処理し、戻すときに消費済み分を落とす。
        let pending = std::mem::take(&mut self.pending);
        let mut consumed = 0;
        let mut result = Ok(());
        while consumed + block_len <= pending.len() {
            if let Err(e) = self.encode_block(&pending[consumed..consumed + block_len]) {
                result = Err(e);
                break;
            }
            consumed += block_len;
        }
        self.pending = pending;
        self.pending.drain(..consumed);
        result
    }

    /// 1 ブロック分（最終ブロックのみ端数可）の量子化済みサンプルを 1 フレームに
    /// エンコードしてファイルへ書く。
    fn encode_block(&mut self, block: &[i32]) -> Result<()> {
        self.frame_buf.fill_interleaved(block).map_err(enc_err)?;
        // MD5 と総サンプル数はここで積算される。
        self.context.fill_interleaved(block).map_err(enc_err)?;

        let frame = flacenc::encode_fixed_size_frame(
            &self.config,
            &self.frame_buf,
            self.frame_number,
            &self.stream_info,
        )
        .map_err(enc_err)?;
        // min/max フレームサイズ統計を積む（finalize でヘッダに反映される）。
        self.stream_info.update_frame_info(&frame);

        self.sink.clear();
        frame.write(&mut self.sink).map_err(enc_err)?;
        // FLAC フレームはバイト境界で終わるので as_slice で欠けなく取れる。
        self.file.write_all(self.sink.as_slice())?;
        self.frame_number += 1;
        Ok(())
    }

    /// finalize の実体。Drop からも呼ばれる。
    fn finish_inner(&mut self) -> Result<()> {
        // 端数を最終フレームとして吐く（FLAC は最終フレームだけ短くてよい）。
        if !self.pending.is_empty() {
            let tail = std::mem::take(&mut self.pending);
            self.encode_block(&tail)?;
        }

        // STREAMINFO を確定させる。
        // - min/max ブロックサイズ: RFC 9639 では最終（端数）ブロックを数えないが、
        //   update_frame_info は数えてしまい、端数が 16 サンプル未満だと仕様違反の
        //   ヘッダになる（claxon などが拒否する）。固定ブロックサイズのストリーム
        //   なので宣言値に戻す。
        // - 総サンプル数: update_frame_info も積算するが、Context の値を正とする。
        self.stream_info
            .set_block_sizes(BLOCK_SIZE, BLOCK_SIZE)
            .map_err(enc_err)?;
        self.stream_info.set_md5_digest(&self.context.md5_digest());
        self.stream_info
            .set_total_samples(self.context.total_samples());

        // 先頭の仮ヘッダを同じ長さの確定ヘッダで上書きする。
        let header = self.header_bytes()?;
        self.file.rewind()?;
        self.file.write_all(&header)?;
        self.file.flush()?;
        Ok(())
    }

    /// 現時点の StreamInfo から FLAC ヘッダ（"fLaC" + STREAMINFO ブロック）を作る。
    ///
    /// 常に [`HEADER_LEN`] バイト。作成時と finalize 時で長さが変わらないことに
    /// 依存して、finalize では先頭を上書きする。
    fn header_bytes(&self) -> Result<Vec<u8>> {
        let mut info = self.stream_info.clone();
        if self.frame_number == 0 {
            // フレームが 1 つも無い間は min/max フレームサイズが flacenc の番兵値
            // (u32::MAX / 0) のままなので、FLAC の「0 = 不明」に倒してから書く。
            info.set_frame_sizes(0, 0).map_err(enc_err)?;
        }
        // フレームを持たない Stream の直列化 = マジック + STREAMINFO ブロックのみ。
        let stream = Stream::with_stream_info(info);
        let mut sink = ByteSink::new();
        stream.write(&mut sink).map_err(enc_err)?;
        let bytes = sink.into_inner();
        debug_assert_eq!(bytes.len(), HEADER_LEN);
        Ok(bytes)
    }
}

impl Drop for FlacWriter {
    fn drop(&mut self) {
        if !self.finalized {
            // ベストエフォート。エラーは握りつぶす（検知したいなら finalize を呼ぶ）。
            let _ = self.finish_inner();
        }
    }
}

// flacenc 側の型（Verified 等）が Debug を持たないので derive できず、手書きする。
impl std::fmt::Debug for FlacWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FlacWriter")
            .field("sample_rate", &self.stream_info.sample_rate())
            .field("channels", &self.channels)
            .field("frame_number", &self.frame_number)
            .field("pending_samples", &self.pending.len())
            .field("finalized", &self.finalized)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::quantize_i16;

    #[test]
    fn quantize_reference_points() {
        assert_eq!(quantize_i16(0.0), 0);
        assert_eq!(quantize_i16(0.25), 8192);
        assert_eq!(quantize_i16(-1.0), -32768);
        // 正側フルスケールは 16bit に収まらないのでクランプ。
        assert_eq!(quantize_i16(1.0), 32767);
        // 範囲外は飽和。
        assert_eq!(quantize_i16(2.0), 32767);
        assert_eq!(quantize_i16(-2.0), -32768);
        // 非有限値。
        assert_eq!(quantize_i16(f32::NAN), 0);
        assert_eq!(quantize_i16(f32::INFINITY), 32767);
        assert_eq!(quantize_i16(f32::NEG_INFINITY), -32768);
        // 丸めは最近接（round half away from zero）。1.5/32768 は 2 進で正確。
        assert_eq!(quantize_i16(1.5 / 32768.0), 2);
        assert_eq!(quantize_i16(-1.5 / 32768.0), -2);
    }
}
