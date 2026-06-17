//! 任意のデバイスフレーム（任意 SR / 任意 ch / interleaved f32）を 2 段で
//! 正規化・再変換する。
//!
//! ```text
//! 入力(任意 SR/ch)
//!   │  第 1 段（内部正規化・不変）
//!   │   ・チャンネル mix（→stereo）
//!   │   ・SR 変換（rubato, →48000）
//!   ▼
//! 内部正規形: f32 / 48000 Hz / stereo / 20ms = 960 frame
//!   │  第 2 段（出口・新規）
//!   │   ・チャンネル変換（stereo→mono 平均 / mono→stereo 複製 / そのまま）
//!   │   ・SR 変換（rubato, 48000→output.sample_rate。等しければパススルー）
//!   ▼
//! 出力: f32 / output.sample_rate / output.channels / 時間ベース 20ms 固定
//!        （48k=960 / 16k=320 / 8k=160 frame）
//! ```
//!
//! - **出力 `{48000, 2}`（既定）なら第 2 段は完全パススルー**（内部正規形が
//!   そのまま出る＝回帰ゼロ）。
//! - 第 1 段の SR 変換は `in_sample_rate == 48000` ならパススルー。
//! - 第 2 段の SR 変換は `output.sample_rate == 48000` ならパススルー。
//!   どちらの rubato リサンプラも `FixedAsync::Input`（固定入力チャンク）で、
//!   生成された可変長出力を内部 accumulator に集約し、**きっかり 20ms 相当**の
//!   境界で切り出す。端数はリサンプラ内部 + accumulator が持ち越す。
//! - PTS: 出力チャンクの先頭サンプルに対応する device_pts を割り当てる。
//!   入力サンプルオフセット → 出力サンプルオフセットを比で追跡する。
//! - seq は付与しない（ストリーム層が付与）。

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{
    Async, FixedAsync, Indexing, Resampler, SincInterpolationParameters, SincInterpolationType,
    WindowFunction,
};

use crate::types::{Error, OutputFormat, Result, CHANNELS, SAMPLE_RATE};

/// 内部正規形 1 チャンクのフレーム数（20ms @ 48kHz）。第 1 段の切り出し境界。
pub const CHUNK_FRAMES: usize = 960;

/// 内部正規形のチャンネル数（stereo）。
const INNER_CH: usize = CHANNELS as usize; // 2

/// 入力デバイスフレームを内部正規形（48k/stereo/960frame）へ正規化し、さらに
/// 出力フォーマット（`output.sample_rate` / `output.channels` / 時間ベース 20ms）
/// へ再変換するステートフルな 2 段変換器。
///
/// `push` で interleaved サンプルを蓄積し、`pop_chunk` で完成済みの出力チャンクを
/// 1 つずつ取り出す。
pub struct Normalizer {
    in_sample_rate: u32,
    in_channels: usize,
    output: OutputFormat,

    // --- 第 1 段（内部正規化: → 48k/stereo） ---
    /// 48000 入力ならパススルー（リサンプラ無し）。
    stage1_resampler: Option<ResamplerState>,
    /// 第 1 段出力（内部正規形 stereo interleaved）。960 frame 境界で第 2 段へ渡す。
    inner_buf: Vec<f32>,

    // --- 第 2 段（出口: 48k/stereo → output） ---
    /// 出力段。`output == {48000, 2}` なら `None`（完全パススルー）。
    stage2: Option<OutputStage>,
    /// 完成待ちの出力（output.channels の interleaved）。`pop_chunk` がここから切る。
    out_buf: Vec<f32>,
    /// 出力 1 チャンクのフレーム数（`output.chunk_frames()`）。
    out_chunk_frames: usize,
    /// 出力チャンネル数。
    out_channels: usize,

    /// `out_buf` 先頭（まだ pop していない最古サンプル）に対応する出力フレーム索引。
    out_frame_origin: u64,

    /// PTS アンカー: ある出力フレーム索引に device_pts(ns) を結び付ける。
    pts_anchor: Option<PtsAnchor>,

    /// これまでに第 2 段から生成した累計出力フレーム数（アンカー計算用）。
    total_out_frames: u64,
    /// これまでに第 1 段が生成した累計内部 48k フレーム数（アンカー計算用）。
    total_inner_frames: u64,
}

#[derive(Clone, Copy)]
struct PtsAnchor {
    /// 出力フレーム索引（出力レート基準）。
    out_frame: u64,
    /// その出力フレームに対応する device_pts(ns)。
    pts_ns: i64,
}

/// rubato `Async`（`FixedAsync::Input`）を 1 段ぶん束ねた SR 変換器。
///
/// 固定入力チャンク `chunk_in_frames` ごとに `process` し、可変長出力を
/// `out_buf`（呼び出し側 accumulator）へ追記する。`channels` は段によって
/// 異なる（第 1 段は常に stereo=2、第 2 段は出力チャンネル数）。
struct ResamplerState {
    inner: Async<f32>,
    channels: usize,
    /// rubato が要求する 1 回分の入力フレーム数（`FixedAsync::Input` で固定）。
    chunk_in_frames: usize,
    /// 1 回の `process` が生成しうる最大出力フレーム数。
    max_out_frames: usize,
    /// 未処理の入力（interleaved・`channels` ch）。
    in_accum: Vec<f32>,
    /// rubato への出力スクラッチ（再利用してアロケートを避ける）。
    out_scratch: Vec<f32>,
}

/// 第 2 段（出口）。内部正規形 48k/stereo の 960frame チャンクを受け、
/// チャンネル変換 → SR 変換して出力フォーマットの interleaved を生成する。
struct OutputStage {
    out_channels: usize,
    /// 48000 → output.sample_rate のリサンプラ。`output.sample_rate == 48000`
    /// なら `None`（SR パススルー）。チャンネル変換後のサンプルに適用する。
    resampler: Option<ResamplerState>,
    /// チャンネル変換後・SR 変換前のスクラッチ（48k / out_channels interleaved）。
    ch_scratch: Vec<f32>,
}

impl Normalizer {
    /// 入力 SR / 入力チャンネル数 / 出力フォーマットを指定して正規化器を作る。
    ///
    /// 第 1 段は入力を 48k/stereo へ正規化（`in_sample_rate == 48000` なら SR
    /// パススルー、`in_channels` が 1 なら mono→stereo 複製、2 はそのまま、
    /// 3 以上はフロント 2ch を採用）。第 2 段は内部正規形を `output` へ再変換
    /// （`output == {48000, 2}` なら完全パススルー）。
    ///
    /// `output` は呼び出し側が事前に [`OutputFormat::validate`] 済みであることを
    /// 期待する（ここでは防御的に妥当域へ丸めはしない）。
    ///
    /// rubato リサンプラの構築は内部的に失敗し得る（極端なレート比等）。従来は
    /// `expect` で panic させていたが、これは非 RT の取り込みスレッドを無言で
    /// 殺す（=ストリームの無言停止）ため、[`Error::Backend`] として返すよう
    /// 変更した。呼び出し側（facade の `Stream::open` / 取り込みスレッド）が
    /// `Error` を伝播・記録する。
    pub fn new(in_sample_rate: u32, in_channels: u16, output: OutputFormat) -> Result<Self> {
        let in_channels = in_channels.max(1) as usize;

        // 第 1 段リサンプラ（→48000）。
        let stage1_resampler = if in_sample_rate == SAMPLE_RATE {
            None
        } else {
            Some(ResamplerState::new(in_sample_rate, SAMPLE_RATE, INNER_CH)?)
        };

        let out_channels = (output.channels.max(1)) as usize;
        let out_chunk_frames = output.chunk_frames().max(1);

        // 第 2 段。出力が内部正規形と完全一致なら不要（パススルー）。
        let stage2 = if output.sample_rate == SAMPLE_RATE && out_channels == INNER_CH {
            None
        } else {
            Some(OutputStage::new(output.sample_rate, out_channels)?)
        };

        Ok(Self {
            in_sample_rate,
            in_channels,
            output,
            stage1_resampler,
            inner_buf: Vec::with_capacity(CHUNK_FRAMES * INNER_CH * 4),
            stage2,
            out_buf: Vec::with_capacity(out_chunk_frames * out_channels * 4),
            out_chunk_frames,
            out_channels,
            out_frame_origin: 0,
            pts_anchor: None,
            total_out_frames: 0,
            total_inner_frames: 0,
        })
    }

    /// 入力サンプルレート（Hz）。
    pub fn in_sample_rate(&self) -> u32 {
        self.in_sample_rate
    }

    /// 出力フォーマット。
    pub fn output(&self) -> OutputFormat {
        self.output
    }

    /// 第 1 段 SR 変換がパススルー（in == 48000）か。
    pub fn is_passthrough(&self) -> bool {
        self.stage1_resampler.is_none()
    }

    /// 第 2 段（出口）が完全パススルー（output == {48000, 2}）か。
    pub fn is_output_passthrough(&self) -> bool {
        self.stage2.is_none()
    }

    /// interleaved 入力サンプルを蓄積する。
    ///
    /// `interleaved` の長さは `in_channels` の倍数であること。`device_pts_ns` は
    /// この push の先頭フレームに対応するデバイス由来 PTS。
    ///
    /// rubato の `process` は理論上失敗し得る（従来は `expect` で panic）。
    /// 非 RT の取り込みスレッドを無言で殺さないよう、失敗時は
    /// [`Error::Backend`] を返す（呼び出し側がストリームを明示停止できる）。
    pub fn push(&mut self, interleaved: &[f32], device_pts_ns: i64) -> Result<()> {
        if interleaved.is_empty() {
            return Ok(());
        }
        let in_frames = interleaved.len() / self.in_channels;
        if in_frames == 0 {
            return Ok(());
        }

        // この push の先頭フレームが将来現れる出力フレーム位置を比で近似し、
        // PTS アンカーを更新する。（リサンプラ内部の保持端数があるため近似。）
        self.update_pts_anchor(device_pts_ns);

        // 第 1 段: チャンネル mix → stereo interleaved。
        let mut stereo = Vec::with_capacity(in_frames * INNER_CH);
        Self::mix_to_stereo(interleaved, self.in_channels, in_frames, &mut stereo);

        match &mut self.stage1_resampler {
            None => {
                // SR パススルー: そのまま内部 accumulator へ。
                self.total_inner_frames += in_frames as u64;
                self.inner_buf.extend_from_slice(&stereo);
            }
            Some(rs) => {
                rs.in_accum.extend_from_slice(&stereo);
                rs.drain_into(&mut self.inner_buf, &mut self.total_inner_frames)?;
            }
        }

        // 内部正規形が 960frame 溜まるごとに第 2 段へ流し込む。
        self.pump_stage2()
    }

    /// 完成済みの出力チャンクを 1 つ取り出す。
    ///
    /// 返り値は `(output.channels interleaved・`out_chunk_frames` frame ぶん,
    /// 先頭サンプルの device_pts(ns))`。まだ 1 チャンク分溜まっていなければ `None`。
    pub fn pop_chunk(&mut self) -> Option<(Vec<f32>, i64)> {
        let need = self.out_chunk_frames * self.out_channels;
        if self.out_buf.len() < need {
            return None;
        }

        let pts = self.pts_for_out_frame(self.out_frame_origin);

        let chunk: Vec<f32> = self.out_buf.drain(..need).collect();
        self.out_frame_origin += self.out_chunk_frames as u64;

        Some((chunk, pts))
    }

    /// 現在 `out_buf` に溜まっている未取り出し出力フレーム数。
    pub fn buffered_out_frames(&self) -> usize {
        self.out_buf.len() / self.out_channels
    }

    // --- 内部ヘルパ ---

    /// `inner_buf` に溜まった内部正規形を 960frame 境界で第 2 段へ流し、
    /// 生成された出力フレームを `out_buf` へ追記する。
    fn pump_stage2(&mut self) -> Result<()> {
        let inner_chunk = CHUNK_FRAMES * INNER_CH;

        match &mut self.stage2 {
            None => {
                // 第 2 段パススルー: 内部正規形 = 出力。inner_buf をそのまま out_buf へ。
                // （960frame 境界に揃える必要はない。pop_chunk が 960 単位で切る。）
                if !self.inner_buf.is_empty() {
                    let n_frames = (self.inner_buf.len() / INNER_CH) as u64;
                    self.total_out_frames += n_frames;
                    self.out_buf.append(&mut self.inner_buf);
                }
            }
            Some(stage) => {
                // 960frame ちょうどの内部チャンク単位で第 2 段を回す。
                while self.inner_buf.len() >= inner_chunk {
                    // 借用衝突回避のため 1 チャンク分をローカルへ取り出す。
                    let chunk: Vec<f32> = self.inner_buf.drain(..inner_chunk).collect();
                    stage.process_inner_chunk(
                        &chunk,
                        &mut self.out_buf,
                        &mut self.total_out_frames,
                    )?;
                }
            }
        }
        Ok(())
    }

    /// 任意 ch interleaved を stereo interleaved へ mix して `dst` に push する。
    fn mix_to_stereo(src: &[f32], in_ch: usize, in_frames: usize, dst: &mut Vec<f32>) {
        match in_ch {
            1 => {
                // mono → stereo（L=R 複製）
                for &s in &src[..in_frames] {
                    dst.push(s);
                    dst.push(s);
                }
            }
            2 => {
                // 2ch はそのまま（必要分のみ）
                dst.extend_from_slice(&src[..in_frames * 2]);
            }
            _ => {
                // >2ch: 当面フロント 2ch を採用。
                // TODO(BS.775): 5.1 等の正式ダウンミックス係数を適用する。
                for f in 0..in_frames {
                    let base = f * in_ch;
                    dst.push(src[base]);
                    dst.push(src[base + 1]);
                }
            }
        }
    }

    /// 入力サンプルオフセット → 出力サンプルオフセットを SR 比で換算し、
    /// この push 先頭に対応する出力フレーム位置へ PTS アンカーを張る。
    ///
    /// 出力フレーム位置はこれまでの累計入力フレーム数を「入力 SR → 出力 SR」比で
    /// 写像した近似値（第 1 段・第 2 段のリサンプラ内部保持端数があるため厳密で
    /// はない）。
    fn update_pts_anchor(&mut self, device_pts_ns: i64) {
        // 入力 → 出力レート比（第 1 段 in→48000、第 2 段 48000→output を合成）。
        let ratio = self.output.sample_rate as f64 / self.in_sample_rate as f64;
        let projected_out_frame = (self.total_in_frames_estimate() as f64 * ratio) as u64;
        self.pts_anchor = Some(PtsAnchor {
            out_frame: projected_out_frame,
            pts_ns: device_pts_ns,
        });
    }

    /// PTS アンカー用の累計入力フレーム数推定。
    ///
    /// 第 1 段は内部 48k フレームで累計を持つので、これを入力レート基準へ戻して
    /// 使う（`update_pts_anchor` 内で出力レートへ写像する）。push のたびに
    /// `total_inner_frames` は更新済みなので、入力レート換算へ逆写像する。
    fn total_in_frames_estimate(&self) -> u64 {
        // total_inner_frames は 48k 基準。入力レート基準へ戻す。
        let inv = self.in_sample_rate as f64 / SAMPLE_RATE as f64;
        (self.total_inner_frames as f64 * inv) as u64
    }

    /// 出力フレーム索引 `out_frame` に対応する device_pts(ns) を、
    /// アンカーから出力レート比で外挿して求める。
    fn pts_for_out_frame(&self, out_frame: u64) -> i64 {
        match self.pts_anchor {
            None => crate::clock::monotonic_now_ns(),
            Some(anchor) => {
                let frame_delta = out_frame as i64 - anchor.out_frame as i64;
                let ns_per_out_frame = 1_000_000_000_i64 / self.output.sample_rate as i64;
                anchor.pts_ns + frame_delta * ns_per_out_frame
            }
        }
    }
}

impl ResamplerState {
    /// `in_sr` → `out_sr` の固定比リサンプラを `channels` ch で作る。
    ///
    /// rubato の構築は理論上失敗し得る（極端なレート比等）。従来は `expect` で
    /// panic していたが、無言のスレッド死を避けるため [`Error::Backend`] を返す。
    fn new(in_sr: u32, out_sr: u32, channels: usize) -> Result<Self> {
        let ratio = out_sr as f64 / in_sr as f64;
        // 入力固定チャンク: 20ms 相当の入力フレーム（端数は rubato が内部保持）。
        let chunk_in_frames = (in_sr as usize / 50).max(64);

        let params = SincInterpolationParameters {
            sinc_len: 128,
            f_cutoff: 0.95,
            interpolation: SincInterpolationType::Linear,
            oversampling_factor: 128,
            window: WindowFunction::BlackmanHarris2,
        };

        let inner = Async::<f32>::new_sinc(
            ratio,
            1.0, // 比は固定（可変リサンプルは不要）
            &params,
            chunk_in_frames,
            channels,
            FixedAsync::Input,
        )
        .map_err(|e| Error::Backend(format!("rubato sinc resampler construction failed: {e}")))?;

        let max_out_frames = inner.output_frames_max();

        Ok(Self {
            inner,
            channels,
            chunk_in_frames,
            max_out_frames,
            in_accum: Vec::with_capacity(chunk_in_frames * channels * 4),
            out_scratch: vec![0.0; max_out_frames * channels],
        })
    }

    /// `in_accum` に溜まった分を chunk_in_frames 単位で可能な限りリサンプルし、
    /// 生成した interleaved を `out_buf` へ追記する。
    ///
    /// rubato の adapter 構築・`process_into_buffer` は理論上失敗し得る（従来は
    /// `expect` で panic）。取り込みスレッドの無言死を避けるため、失敗時は
    /// [`Error::Backend`] を返す。
    fn drain_into(&mut self, out_buf: &mut Vec<f32>, total_out_frames: &mut u64) -> Result<()> {
        let step = self.chunk_in_frames * self.channels;

        while self.in_accum.len() >= step {
            let in_adapter =
                InterleavedSlice::new(&self.in_accum[..step], self.channels, self.chunk_in_frames)
                    .map_err(|e| {
                        Error::Backend(format!("rubato interleaved input adapter failed: {e}"))
                    })?;

            let mut out_adapter = InterleavedSlice::new_mut(
                &mut self.out_scratch[..],
                self.channels,
                self.max_out_frames,
            )
            .map_err(|e| {
                Error::Backend(format!("rubato interleaved output adapter failed: {e}"))
            })?;

            let indexing = Indexing {
                input_offset: 0,
                output_offset: 0,
                partial_len: None,
                active_channels_mask: None,
            };

            let (_in_used, out_written) = self
                .inner
                .process_into_buffer(&in_adapter, &mut out_adapter, Some(&indexing))
                .map_err(|e| Error::Backend(format!("rubato process_into_buffer failed: {e}")))?;

            let n_samples = out_written * self.channels;
            out_buf.extend_from_slice(&self.out_scratch[..n_samples]);
            *total_out_frames += out_written as u64;

            // 消費した入力を取り除く（FixedAsync::Input なので chunk_in_frames 固定消費）。
            self.in_accum.drain(..step);
        }
        Ok(())
    }
}

impl OutputStage {
    /// 出力レート / 出力チャンネル数を指定して出口段を作る。
    ///
    /// `out_sample_rate == 48000` なら SR 変換はパススルー（チャンネル変換のみ）。
    ///
    /// rubato 構築失敗は [`Error::Backend`] として伝播する（panic しない）。
    fn new(out_sample_rate: u32, out_channels: usize) -> Result<Self> {
        let resampler = if out_sample_rate == SAMPLE_RATE {
            None
        } else {
            // 入力は内部正規形 48000、出力は out_sample_rate。チャンネルは out_channels。
            Some(ResamplerState::new(SAMPLE_RATE, out_sample_rate, out_channels)?)
        };
        Ok(Self {
            out_channels,
            resampler,
            ch_scratch: Vec::with_capacity(CHUNK_FRAMES * out_channels),
        })
    }

    /// 内部正規形 1 チャンク（48k/stereo・960frame interleaved）を処理して、
    /// 出力フォーマットの interleaved を `out_buf` へ追記する。
    fn process_inner_chunk(
        &mut self,
        inner_stereo: &[f32],
        out_buf: &mut Vec<f32>,
        total_out_frames: &mut u64,
    ) -> Result<()> {
        debug_assert_eq!(inner_stereo.len(), CHUNK_FRAMES * INNER_CH);

        // 1) チャンネル変換: stereo(2ch) → out_channels。
        self.ch_scratch.clear();
        match self.out_channels {
            1 => {
                // stereo → mono（L/R 平均）。
                for f in 0..CHUNK_FRAMES {
                    let l = inner_stereo[f * 2];
                    let r = inner_stereo[f * 2 + 1];
                    self.ch_scratch.push((l + r) * 0.5);
                }
            }
            2 => {
                // そのまま。
                self.ch_scratch.extend_from_slice(inner_stereo);
            }
            _ => {
                // MVP は 1/2 のみ。validate で弾かれているはずだが防御的に L 複製。
                for f in 0..CHUNK_FRAMES {
                    let l = inner_stereo[f * 2];
                    for _ in 0..self.out_channels {
                        self.ch_scratch.push(l);
                    }
                }
            }
        }

        // 2) SR 変換: 48000 → out_sample_rate（パススルーなら ch_scratch をそのまま）。
        match &mut self.resampler {
            None => {
                // SR パススルー。チャンネル変換後をそのまま出力へ。
                let n_frames = (self.ch_scratch.len() / self.out_channels) as u64;
                *total_out_frames += n_frames;
                out_buf.extend_from_slice(&self.ch_scratch);
            }
            Some(rs) => {
                rs.in_accum.extend_from_slice(&self.ch_scratch);
                rs.drain_into(out_buf, total_out_frames)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;

    /// 既定出力（{48000, 2}）のヘルパ。
    fn default_out() -> OutputFormat {
        OutputFormat::default()
    }

    #[test]
    fn mono_48k_to_stereo_duplicates_channels() {
        let mut n = Normalizer::new(48_000, 1, default_out()).expect("normalizer");
        assert!(n.is_passthrough());
        assert!(n.is_output_passthrough());
        // 960 フレーム分の mono 入力（パススルーなので 1 チャンクちょうど）。
        let mono: Vec<f32> = (0..CHUNK_FRAMES).map(|i| (i as f32) * 0.001).collect();
        n.push(&mono, 0).expect("push");
        let (chunk, _pts) = n.pop_chunk().expect("one chunk");
        assert_eq!(chunk.len(), CHUNK_FRAMES * 2);
        // L == R がフレーム毎に成立。
        for f in 0..CHUNK_FRAMES {
            assert_eq!(chunk[f * 2], chunk[f * 2 + 1], "L==R at frame {f}");
            assert_eq!(chunk[f * 2], mono[f]);
        }
    }

    #[test]
    fn passthrough_preserves_frame_count() {
        let mut n = Normalizer::new(48_000, 2, default_out()).expect("normalizer");
        assert!(n.is_passthrough());
        assert!(n.is_output_passthrough());
        // 2 チャンク分 + 端数。
        let frames = CHUNK_FRAMES * 2 + 100;
        let stereo: Vec<f32> = (0..frames * 2).map(|i| (i as f32) * 1e-4).collect();
        n.push(&stereo, 0).expect("push");

        let mut got_frames = 0usize;
        while let Some((c, _)) = n.pop_chunk() {
            assert_eq!(c.len(), CHUNK_FRAMES * 2);
            got_frames += CHUNK_FRAMES;
        }
        // ちょうど 2 チャンク取り出せ、端数 100 frame は残る。
        assert_eq!(got_frames, CHUNK_FRAMES * 2);
        assert_eq!(n.buffered_out_frames(), 100);
    }

    #[test]
    fn stereo_44100_to_48000_yields_about_50_chunks_per_second() {
        let mut n = Normalizer::new(44_100, 2, default_out()).expect("normalizer");
        assert!(!n.is_passthrough());

        // 1 秒分の 44100Hz ステレオ サイン波。
        let in_frames = 44_100;
        let freq = 440.0_f32;
        let mut interleaved = Vec::with_capacity(in_frames * 2);
        for i in 0..in_frames {
            let s = (2.0 * PI * freq * (i as f32) / 44_100.0).sin() * 0.5;
            interleaved.push(s); // L
            interleaved.push(s); // R
        }

        // 細切れ push（実機の小バッファ到着を模す）でも panic しないこと。
        let mut pts = 0i64;
        for block in interleaved.chunks(441 * 2) {
            n.push(block, pts).expect("push");
            pts += (block.len() as i64 / 2) * 1_000_000_000 / 44_100;
        }

        let mut chunks = 0usize;
        while let Some((c, _pts)) = n.pop_chunk() {
            assert_eq!(c.len(), CHUNK_FRAMES * 2);
            chunks += 1;
        }
        assert!(
            (47..=50).contains(&chunks),
            "expected ~50 chunks, got {chunks}"
        );
    }

    #[test]
    fn pts_increases_monotonically_across_chunks() {
        let mut n = Normalizer::new(48_000, 2, default_out()).expect("normalizer");
        let frames = CHUNK_FRAMES * 3;
        let stereo = vec![0.0f32; frames * 2];
        n.push(&stereo, 100_000_000).expect("push");

        let mut last = i64::MIN;
        let mut count = 0;
        while let Some((_, pts)) = n.pop_chunk() {
            assert!(pts >= last, "pts must be non-decreasing");
            last = pts;
            count += 1;
        }
        assert_eq!(count, 3);
    }

    // --- 第 2 段（出口）の検証 ---

    /// 48k/stereo 入力 + 出力 {16000, 1} → 320 frame の mono チャンク。
    #[test]
    fn output_16k_mono_yields_320_frame_mono_chunks() {
        let out = OutputFormat {
            sample_rate: 16_000,
            channels: 1,
        };
        let mut n = Normalizer::new(48_000, 2, out).expect("normalizer");
        assert!(n.is_passthrough()); // 第 1 段は SR パススルー（48k 入力）。
        assert!(!n.is_output_passthrough()); // 第 2 段は有効。

        // 1 秒分の 48k stereo サイン波（細切れ push）。
        let in_frames = 48_000;
        let freq = 440.0_f32;
        let mut pts = 0i64;
        for blk in 0..(in_frames / 480) {
            let mut block = Vec::with_capacity(480 * 2);
            for j in 0..480 {
                let i = blk * 480 + j;
                let s = (2.0 * PI * freq * (i as f32) / 48_000.0).sin() * 0.5;
                block.push(s);
                block.push(s);
            }
            n.push(&block, pts).expect("push");
            pts += 480 * 1_000_000_000 / 48_000;
        }

        let mut chunks = 0usize;
        while let Some((c, _)) = n.pop_chunk() {
            assert_eq!(c.len(), 320, "16k mono 20ms = 320 sample (mono)");
            chunks += 1;
        }
        // 16000/320 = 50 チャンク/秒。リサンプラ遅延で約 50。
        assert!((47..=50).contains(&chunks), "expected ~50 chunks, got {chunks}");
    }

    /// 出力 {16000, 2} → 320 frame・640 sample（stereo）。
    #[test]
    fn output_16k_stereo_yields_320_frame_640_sample_chunks() {
        let out = OutputFormat {
            sample_rate: 16_000,
            channels: 2,
        };
        let mut n = Normalizer::new(48_000, 2, out).expect("normalizer");
        let in_frames = 48_000;
        let stereo: Vec<f32> = (0..in_frames * 2)
            .map(|i| ((i / 2) as f32 * 0.0001).sin() * 0.3)
            .collect();
        for block in stereo.chunks(480 * 2) {
            n.push(block, 0).expect("push");
        }
        let mut chunks = 0usize;
        while let Some((c, _)) = n.pop_chunk() {
            assert_eq!(c.len(), 640, "16k stereo 20ms = 320 frame * 2 = 640 sample");
            chunks += 1;
        }
        assert!((47..=50).contains(&chunks), "expected ~50 chunks, got {chunks}");
    }

    /// 出力 {8000, 2} → 160 frame・320 sample。
    #[test]
    fn output_8k_stereo_yields_160_frame_chunks() {
        let out = OutputFormat {
            sample_rate: 8_000,
            channels: 2,
        };
        let mut n = Normalizer::new(48_000, 2, out).expect("normalizer");
        let stereo: Vec<f32> = (0..48_000 * 2).map(|i| (i as f32 * 1e-5).sin() * 0.2).collect();
        for block in stereo.chunks(480 * 2) {
            n.push(block, 0).expect("push");
        }
        let mut chunks = 0usize;
        while let Some((c, _)) = n.pop_chunk() {
            assert_eq!(c.len(), 320, "8k stereo 20ms = 160 frame * 2 = 320 sample");
            chunks += 1;
        }
        assert!((47..=50).contains(&chunks), "expected ~50 chunks, got {chunks}");
    }

    /// stereo→mono は L/R 平均（L=+a, R=-a の逆相は 0 に近づく）。
    #[test]
    fn stereo_to_mono_is_lr_average() {
        // 出力 48000/mono にして SR パススルー・チャンネル変換のみを検証する。
        let out = OutputFormat {
            sample_rate: 48_000,
            channels: 1,
        };
        let mut n = Normalizer::new(48_000, 2, out).expect("normalizer");
        // 完全逆相（L=+0.5, R=-0.5）→ 平均 0。
        let mut stereo = Vec::with_capacity(CHUNK_FRAMES * 2);
        for _ in 0..CHUNK_FRAMES {
            stereo.push(0.5);
            stereo.push(-0.5);
        }
        n.push(&stereo, 0).expect("push");
        let (chunk, _) = n.pop_chunk().expect("one mono chunk");
        assert_eq!(chunk.len(), CHUNK_FRAMES); // mono 960 sample。
        for &s in &chunk {
            assert!(s.abs() < 1e-6, "逆相の平均は 0 付近のはず: {s}");
        }
    }
}
