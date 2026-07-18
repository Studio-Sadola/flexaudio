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
//! 既定の出力 `{48000, 2}` なら第 2 段は丸ごとパススルー（内部正規形がそのまま出る）。
//! 第 1 段の SR 変換は `in_sample_rate == 48000` で、第 2 段の SR 変換は
//! `output.sample_rate == 48000` でそれぞれパススルーになる。
//!
//! どちらの rubato リサンプラも `FixedAsync::Input`（固定入力チャンク）で、生成された
//! 可変長出力を内部 accumulator に集約し、20ms 相当の境界で切り出す。端数はリサンプラ
//! 内部と accumulator が次へ持ち越す。
//!
//! PTS は出力チャンク先頭サンプルに対応する device_pts を、入力→出力サンプルオフセット
//! の比で追跡して割り当てる。seq はストリーム層が付与する。

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

/// A processor applied to the internal normalized form (48kHz / stereo /
/// interleaved f32) *before* it is split into the output taps.
///
/// Kept behind a trait so `flexaudio-core` stays independent of any concrete
/// DSP implementation (e.g. noise suppression). The facade injects an
/// implementation; the core only sees this contract. Because the same
/// processed samples feed every output tap, a single processor affects both the
/// primary and secondary outputs.
///
/// [`process`](Self::process) runs on each batch of normalized samples in
/// place. [`flush`](Self::flush) is called once at stop to recover any tail the
/// processor is holding back (e.g. a fixed delay line); it returns the trailing
/// 48kHz / stereo interleaved samples (empty if none).
pub trait InnerProcessor: Send {
    /// Process one batch of 48kHz / stereo interleaved samples in place. The
    /// slice length is always a multiple of two (stereo).
    fn process(&mut self, samples: &mut [f32]);
    /// Flush and return any buffered tail (48kHz / stereo interleaved). Called
    /// once when the stream stops. Returns an empty vector when nothing is held.
    fn flush(&mut self) -> Vec<f32>;
}

/// 入力デバイスフレームを内部正規形（48k/stereo/960frame）へ正規化し、さらに
/// 1 つ以上の出力タップ（主 + 任意で副）へ再変換するステートフルな 2 段変換器。
///
/// 第 1 段（内部正規化）は 1 度だけ実行し、生成した内部正規形を主・副の各第 2 段へ
/// 供給する。任意の [`InnerProcessor`] を注入すると、第 2 段への分岐前に内部正規形へ
/// 1 度だけ適用される（両タップが同じ加工済み音を受ける）。
///
/// `push` で interleaved サンプルを蓄積し、`pop_chunk`（主）/ `pop_secondary`（副）で
/// 完成済みの出力チャンクを 1 つずつ取り出す。
pub struct Normalizer {
    in_sample_rate: u32,
    in_channels: usize,

    // --- 第 1 段（内部正規化: → 48k/stereo・全出力タップで共有） ---
    /// 48000 入力ならパススルー（リサンプラ無し）。
    stage1_resampler: Option<ResamplerState>,
    /// この push で第 1 段が生成した内部正規形（48k/stereo interleaved）の一時バッファ。
    /// 加工（[`InnerProcessor`]）してから各出力タップへ配る。容量は再利用する。
    inner_scratch: Vec<f32>,
    /// これまでに第 1 段が生成した累計内部 48k フレーム数（PTS アンカー計算用）。
    total_inner_frames: u64,

    /// 内部正規形へ第 2 段分岐前に 1 度だけ適用する任意プロセッサ（例: ノイズ抑制）。
    inner_processor: Option<Box<dyn InnerProcessor>>,

    // --- 出力タップ（各自が第 2 段・出力バッファ・PTS 状態を持つ） ---
    /// 主出力タップ（[`OutputFormat`] は `output`）。
    primary: OutputTap,
    /// 副出力タップ（設定時のみ）。主とは独立の第 2 段・PTS 状態を持つ。
    secondary: Option<OutputTap>,
}

/// 1 つの出力タップ。共有の内部正規形（48k/stereo）を受け、チャンネル変換 + SR 変換で
/// 自身の [`OutputFormat`] へ再変換し、20ms 固定チャンクを切り出す。各タップは独立の
/// PTS アンカー・出力バッファを持つ（主副で PTS が数十msズレる理由）。
struct OutputTap {
    output: OutputFormat,
    /// 出口段。`output == {48000, 2}`（内部正規形と同一）なら `None`（完全パススルー）。
    stage2: Option<OutputStage>,
    /// 完成待ちの出力（output.channels の interleaved）。`pop` がここから切る。
    out_buf: Vec<f32>,
    /// 出力 1 チャンクのフレーム数（`output.chunk_frames()`）。
    out_chunk_frames: usize,
    /// 出力チャンネル数。
    out_channels: usize,
    /// `out_buf` 先頭（まだ pop していない最古サンプル）に対応する出力フレーム索引。
    out_frame_origin: u64,
    /// PTS アンカー: ある出力フレーム索引に device_pts(ns) を結び付ける。
    pts_anchor: Option<PtsAnchor>,
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
    /// 第 1 段は入力を 48k/stereo へ正規化する（`in_sample_rate == 48000` なら SR
    /// パススルー、`in_channels` が 1 なら mono→stereo 複製、2 はそのまま、3 以上は
    /// フロント 2ch を採る）。第 2 段は内部正規形を `output` へ再変換する
    /// （`output == {48000, 2}` ならパススルー）。
    ///
    /// `output` は呼び出し側で [`OutputFormat::validate`] 済みであることを期待する
    /// （ここでは妥当域へ丸めない）。
    ///
    /// rubato リサンプラの構築は極端なレート比などで失敗し得る。panic させると非 RT
    /// の取り込みスレッドが無言で止まるため、失敗時は [`Error::Backend`] を返して
    /// 呼び出し側に伝播させる。
    pub fn new(in_sample_rate: u32, in_channels: u16, output: OutputFormat) -> Result<Self> {
        let in_channels = in_channels.max(1) as usize;

        // 第 1 段リサンプラ（→48000）。全出力タップで 1 度だけ実行する。
        let stage1_resampler = if in_sample_rate == SAMPLE_RATE {
            None
        } else {
            Some(ResamplerState::new(in_sample_rate, SAMPLE_RATE, INNER_CH)?)
        };

        Ok(Self {
            in_sample_rate,
            in_channels,
            stage1_resampler,
            inner_scratch: Vec::with_capacity(CHUNK_FRAMES * INNER_CH * 4),
            total_inner_frames: 0,
            inner_processor: None,
            primary: OutputTap::new(output)?,
            secondary: None,
        })
    }

    /// 副出力タップを追加する（[`OutputFormat::validate`] 済みであることを期待する）。
    ///
    /// 内部正規形（48k/stereo）は 1 度だけ生成して主・副の両第 2 段へ供給する。副タップは
    /// 独立の第 2 段・PTS 状態を持ち、主とは別に 20ms 固定チャンクを生成する。rubato 構築
    /// 失敗時は [`Error::Backend`]。
    pub fn with_secondary(mut self, secondary: OutputFormat) -> Result<Self> {
        self.secondary = Some(OutputTap::new(secondary)?);
        Ok(self)
    }

    /// 内部正規形（48k/stereo）へ第 2 段分岐前に 1 度だけ適用するプロセッサを注入する。
    pub fn with_inner_processor(mut self, processor: Box<dyn InnerProcessor>) -> Self {
        self.inner_processor = Some(processor);
        self
    }

    /// 入力サンプルレート（Hz）。
    pub fn in_sample_rate(&self) -> u32 {
        self.in_sample_rate
    }

    /// 主出力フォーマット。
    pub fn output(&self) -> OutputFormat {
        self.primary.output
    }

    /// 副出力フォーマット（副タップ設定時のみ）。
    pub fn secondary_output(&self) -> Option<OutputFormat> {
        self.secondary.as_ref().map(|t| t.output)
    }

    /// 副タップが有効か。
    pub fn has_secondary(&self) -> bool {
        self.secondary.is_some()
    }

    /// 第 1 段 SR 変換がパススルー（in == 48000）か。
    pub fn is_passthrough(&self) -> bool {
        self.stage1_resampler.is_none()
    }

    /// 主出力の第 2 段が完全パススルー（output == {48000, 2}）か。
    pub fn is_output_passthrough(&self) -> bool {
        self.primary.stage2.is_none()
    }

    /// interleaved 入力サンプルを蓄積する。
    ///
    /// `interleaved` の長さは `in_channels` の倍数であること。`device_pts_ns` は
    /// この push の先頭フレームに対応するデバイス由来 PTS。
    ///
    /// rubato の `process` が失敗したら [`Error::Backend`] を返す（panic させて取り込み
    /// スレッドを無言で止めない。呼び出し側がストリームを明示停止できる）。
    pub fn push(&mut self, interleaved: &[f32], device_pts_ns: i64) -> Result<()> {
        if interleaved.is_empty() {
            return Ok(());
        }
        let in_frames = interleaved.len() / self.in_channels;
        if in_frames == 0 {
            return Ok(());
        }

        // この push 先頭が将来現れる出力フレーム位置を比で近似して各タップの PTS アンカーを
        // 更新する（リサンプラ内部の保持端数があるため近似）。主副はそれぞれ独立に張る。
        self.primary
            .update_pts_anchor(self.total_inner_frames, device_pts_ns);
        if let Some(sec) = self.secondary.as_mut() {
            sec.update_pts_anchor(self.total_inner_frames, device_pts_ns);
        }

        // 第 1 段: チャンネル mix → stereo interleaved → 48k 正規化。この push の生成分を
        // inner_scratch に集める。
        self.inner_scratch.clear();
        let mut stereo = Vec::with_capacity(in_frames * INNER_CH);
        Self::mix_to_stereo(interleaved, self.in_channels, in_frames, &mut stereo);
        match &mut self.stage1_resampler {
            None => {
                // SR パススルー。そのまま内部正規形へ。
                self.total_inner_frames += in_frames as u64;
                self.inner_scratch.extend_from_slice(&stereo);
            }
            Some(rs) => {
                rs.in_accum.extend_from_slice(&stereo);
                let produced = rs.drain_into(&mut self.inner_scratch)?;
                self.total_inner_frames += produced;
            }
        }

        // 第 2 段分岐前に内部正規形へプロセッサ（例: denoise）を 1 度だけ適用する。
        if let Some(proc) = self.inner_processor.as_mut() {
            proc.process(&mut self.inner_scratch);
        }

        // 加工済み内部正規形を主・副の各第 2 段へ配る。
        self.distribute_inner()
    }

    /// 完成済みの主出力チャンクを 1 つ取り出す。
    ///
    /// 返り値は `(output.channels interleaved の `out_chunk_frames` frame, 先頭サンプル
    /// の device_pts(ns))`。1 チャンク分溜まっていなければ `None`。
    pub fn pop_chunk(&mut self) -> Option<(Vec<f32>, i64)> {
        self.primary.pop()
    }

    /// 完成済みの副出力チャンクを 1 つ取り出す（副タップ未設定なら常に `None`）。
    pub fn pop_secondary(&mut self) -> Option<(Vec<f32>, i64)> {
        self.secondary.as_mut().and_then(OutputTap::pop)
    }

    /// 停止時のフラッシュ。プロセッサ（denoise 等）の末尾テールを流し込み、各タップの
    /// 第 2 段リサンプラ残余を吐き切り、末尾の端数チャンクは無音でパディングして 20ms
    /// 固定境界に揃える（`pop_chunk` / `pop_secondary` で取り切れるようにする）。
    ///
    /// リサンプラのフラッシュが失敗しても停止経路を止めないよう、ベストエフォートで
    /// 続ける（末尾数 ms の欠落に留まる）。
    pub fn flush(&mut self) {
        // 1. プロセッサの末尾テール（例: denoise の遅延線）を内部正規形として流し込む。
        if let Some(proc) = self.inner_processor.as_mut() {
            let tail = proc.flush();
            if !tail.is_empty() {
                self.inner_scratch.clear();
                self.inner_scratch.extend_from_slice(&tail);
                let _ = self.distribute_inner();
            }
        }
        // 2. 各タップの第 2 段リサンプラ残余を吐き出し、端数チャンクを無音パディング。
        self.primary.flush();
        if let Some(sec) = self.secondary.as_mut() {
            sec.flush();
        }
    }

    /// 現在 `out_buf`（主タップ）に溜まっている未取り出し出力フレーム数。
    pub fn buffered_out_frames(&self) -> usize {
        self.primary.buffered_out_frames()
    }

    // --- 内部ヘルパ ---

    /// `inner_scratch` の内部正規形を主・副の各第 2 段へ配る（借用衝突を避けるため一時的に
    /// バッファを取り出してから配り、容量を戻す）。
    fn distribute_inner(&mut self) -> Result<()> {
        if self.inner_scratch.is_empty() {
            return Ok(());
        }
        let inner = std::mem::take(&mut self.inner_scratch);
        let r_primary = self.primary.feed_inner(&inner);
        let r_secondary = self
            .secondary
            .as_mut()
            .map(|sec| sec.feed_inner(&inner))
            .unwrap_or(Ok(()));
        // 容量を再利用するためバッファを戻す。
        self.inner_scratch = inner;
        self.inner_scratch.clear();
        r_primary.and(r_secondary)
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
                // >2ch は当面フロント 2ch を採る。
                // TODO(BS.775): 5.1 等の正式なダウンミックス係数を適用する。
                for f in 0..in_frames {
                    let base = f * in_ch;
                    dst.push(src[base]);
                    dst.push(src[base + 1]);
                }
            }
        }
    }
}

impl OutputTap {
    /// 出力フォーマットから出力タップを作る（`output` は検証済みを期待する）。
    fn new(output: OutputFormat) -> Result<Self> {
        let out_channels = (output.channels.max(1)) as usize;
        let out_chunk_frames = output.chunk_frames().max(1);

        // 出力が内部正規形と完全一致なら第 2 段は不要（パススルー）。
        let stage2 = if output.sample_rate == SAMPLE_RATE && out_channels == INNER_CH {
            None
        } else {
            Some(OutputStage::new(output.sample_rate, out_channels)?)
        };

        Ok(Self {
            output,
            stage2,
            out_buf: Vec::with_capacity(out_chunk_frames * out_channels * 4),
            out_chunk_frames,
            out_channels,
            out_frame_origin: 0,
            pts_anchor: None,
        })
    }

    /// 加工済み内部正規形（48k/stereo interleaved・任意長）を第 2 段へ通し、生成された
    /// 出力フレームを `out_buf` へ追記する。
    fn feed_inner(&mut self, inner_stereo: &[f32]) -> Result<()> {
        if inner_stereo.is_empty() {
            return Ok(());
        }
        match &mut self.stage2 {
            None => {
                // 第 2 段パススルー（output == {48000, 2}）。そのまま追記。
                self.out_buf.extend_from_slice(inner_stereo);
            }
            Some(stage) => stage.process_inner(inner_stereo, &mut self.out_buf)?,
        }
        Ok(())
    }

    /// 完成済み出力チャンクを 1 つ取り出す。1 チャンク分溜まっていなければ `None`。
    fn pop(&mut self) -> Option<(Vec<f32>, i64)> {
        let need = self.out_chunk_frames * self.out_channels;
        if self.out_buf.len() < need {
            return None;
        }
        let pts = self.pts_for_out_frame(self.out_frame_origin);
        let chunk: Vec<f32> = self.out_buf.drain(..need).collect();
        self.out_frame_origin += self.out_chunk_frames as u64;
        Some((chunk, pts))
    }

    /// 停止時フラッシュ。第 2 段リサンプラの残余を吐き出し、末尾の端数チャンクを無音で
    /// パディングして 20ms 固定境界へ揃える（`pop` で取り切れるようにする）。
    fn flush(&mut self) {
        if let Some(stage) = self.stage2.as_mut() {
            // リサンプラのフラッシュ失敗はベストエフォートで無視（末尾数 ms の欠落のみ）。
            let _ = stage.flush_into(&mut self.out_buf);
        }
        let need = self.out_chunk_frames * self.out_channels;
        let rem = self.out_buf.len() % need;
        if rem != 0 {
            let pad = need - rem;
            self.out_buf.resize(self.out_buf.len() + pad, 0.0);
        }
    }

    /// 現在 `out_buf` に溜まっている未取り出し出力フレーム数。
    fn buffered_out_frames(&self) -> usize {
        self.out_buf.len() / self.out_channels
    }

    /// この push 先頭に対応する出力フレーム位置へ PTS アンカーを張る。
    ///
    /// 出力フレーム位置は累計内部フレーム数を出力レートへ写像した近似値（リサンプラ内部の
    /// 保持端数があるため厳密ではない）。`in_sample_rate` は約分で消えるので出力レートと
    /// 内部レートだけで求まる。
    fn update_pts_anchor(&mut self, total_inner_frames: u64, device_pts_ns: i64) {
        let projected_out_frame = (total_inner_frames as f64 * self.output.sample_rate as f64
            / SAMPLE_RATE as f64) as u64;
        self.pts_anchor = Some(PtsAnchor {
            out_frame: projected_out_frame,
            pts_ns: device_pts_ns,
        });
    }

    /// 出力フレーム索引 `out_frame` に対応する device_pts(ns) を、アンカーから出力レート比で
    /// 外挿して求める。
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
    /// rubato の構築失敗時は [`Error::Backend`] を返す（panic させてスレッドを無言で
    /// 止めない）。
    fn new(in_sr: u32, out_sr: u32, channels: usize) -> Result<Self> {
        let ratio = out_sr as f64 / in_sr as f64;
        // 固定入力チャンクは 20ms 相当の入力フレーム（端数は rubato が内部に保持する）。
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

    /// `in_accum` に溜まった分を chunk_in_frames 単位で可能な限りリサンプルし、生成した
    /// interleaved を `out_buf` へ追記する。生成した出力フレーム数を返す。
    ///
    /// rubato の adapter 構築・`process_into_buffer` が失敗したら [`Error::Backend`]
    /// を返す（panic させて取り込みスレッドを無言で止めない）。
    fn drain_into(&mut self, out_buf: &mut Vec<f32>) -> Result<u64> {
        let step = self.chunk_in_frames * self.channels;
        let mut produced = 0u64;

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
            produced += out_written as u64;

            // 消費した入力を取り除く（FixedAsync::Input なので消費は chunk_in_frames 固定）。
            self.in_accum.drain(..step);
        }
        Ok(produced)
    }

    /// 停止時、`in_accum` に残った 1 入力チャンク未満の端数を `partial_len` で最後に流し、
    /// 生成した interleaved を `out_buf` へ追記する。生成した出力フレーム数を返す。
    ///
    /// これで丸め残りの入力（最大 20ms 弱）を吐き切る。呼び出し後 `in_accum` は空になる。
    /// リサンプラ内部のフィルタ群遅延（数 ms）まではフラッシュしない。
    fn flush_into(&mut self, out_buf: &mut Vec<f32>) -> Result<u64> {
        let remaining = self.in_accum.len() / self.channels;
        if remaining == 0 {
            return Ok(0);
        }
        // 入力を 1 チャンク分まで無音でパディングし、有効長だけ `partial_len` で伝える。
        self.in_accum
            .resize(self.chunk_in_frames * self.channels, 0.0);

        let in_adapter = InterleavedSlice::new(
            &self.in_accum[..self.chunk_in_frames * self.channels],
            self.channels,
            self.chunk_in_frames,
        )
        .map_err(|e| Error::Backend(format!("rubato interleaved input adapter failed: {e}")))?;

        let mut out_adapter = InterleavedSlice::new_mut(
            &mut self.out_scratch[..],
            self.channels,
            self.max_out_frames,
        )
        .map_err(|e| Error::Backend(format!("rubato interleaved output adapter failed: {e}")))?;

        let indexing = Indexing {
            input_offset: 0,
            output_offset: 0,
            partial_len: Some(remaining),
            active_channels_mask: None,
        };

        let (_in_used, out_written) = self
            .inner
            .process_into_buffer(&in_adapter, &mut out_adapter, Some(&indexing))
            .map_err(|e| Error::Backend(format!("rubato flush process_into_buffer failed: {e}")))?;

        let n_samples = out_written * self.channels;
        out_buf.extend_from_slice(&self.out_scratch[..n_samples]);
        self.in_accum.clear();
        Ok(out_written as u64)
    }
}

impl OutputStage {
    /// 出力レート / 出力チャンネル数を指定して出口段を作る。
    ///
    /// `out_sample_rate == 48000` なら SR 変換はパススルー（チャンネル変換のみ）。
    /// rubato 構築失敗は [`Error::Backend`] として伝播する。
    fn new(out_sample_rate: u32, out_channels: usize) -> Result<Self> {
        let resampler = if out_sample_rate == SAMPLE_RATE {
            None
        } else {
            // 内部正規形 48000 から out_sample_rate へ、out_channels ch で変換する。
            Some(ResamplerState::new(
                SAMPLE_RATE,
                out_sample_rate,
                out_channels,
            )?)
        };
        Ok(Self {
            out_channels,
            resampler,
            ch_scratch: Vec::with_capacity(CHUNK_FRAMES * out_channels),
        })
    }

    /// 内部正規形（48k/stereo interleaved・任意長）を処理して、出力フォーマットの
    /// interleaved を `out_buf` へ追記する。長さは `INNER_CH`（stereo）の倍数であること。
    fn process_inner(&mut self, inner_stereo: &[f32], out_buf: &mut Vec<f32>) -> Result<()> {
        let frames = inner_stereo.len() / INNER_CH;
        if frames == 0 {
            return Ok(());
        }

        // チャンネル変換: stereo → out_channels。
        self.ch_scratch.clear();
        match self.out_channels {
            1 => {
                // stereo → mono（L/R 平均）。
                for f in 0..frames {
                    let l = inner_stereo[f * 2];
                    let r = inner_stereo[f * 2 + 1];
                    self.ch_scratch.push((l + r) * 0.5);
                }
            }
            2 => {
                self.ch_scratch
                    .extend_from_slice(&inner_stereo[..frames * 2]);
            }
            _ => {
                // validate で 1/2 に絞られているはず。届いても L 複製で凌ぐ。
                for f in 0..frames {
                    let l = inner_stereo[f * 2];
                    for _ in 0..self.out_channels {
                        self.ch_scratch.push(l);
                    }
                }
            }
        }

        // SR 変換: 48000 → out_sample_rate。パススルーなら ch_scratch をそのまま出力へ。
        match &mut self.resampler {
            None => {
                out_buf.extend_from_slice(&self.ch_scratch);
            }
            Some(rs) => {
                rs.in_accum.extend_from_slice(&self.ch_scratch);
                rs.drain_into(out_buf)?;
            }
        }
        Ok(())
    }

    /// 停止時、SR リサンプラの端数残余を吐き出して `out_buf` へ追記する（パススルー段は
    /// 残余を持たないので no-op）。
    fn flush_into(&mut self, out_buf: &mut Vec<f32>) -> Result<()> {
        if let Some(rs) = self.resampler.as_mut() {
            rs.flush_into(out_buf)?;
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
        assert!(
            (47..=50).contains(&chunks),
            "expected ~50 chunks, got {chunks}"
        );
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
        assert!(
            (47..=50).contains(&chunks),
            "expected ~50 chunks, got {chunks}"
        );
    }

    /// 出力 {8000, 2} → 160 frame・320 sample。
    #[test]
    fn output_8k_stereo_yields_160_frame_chunks() {
        let out = OutputFormat {
            sample_rate: 8_000,
            channels: 2,
        };
        let mut n = Normalizer::new(48_000, 2, out).expect("normalizer");
        let stereo: Vec<f32> = (0..48_000 * 2)
            .map(|i| (i as f32 * 1e-5).sin() * 0.2)
            .collect();
        for block in stereo.chunks(480 * 2) {
            n.push(block, 0).expect("push");
        }
        let mut chunks = 0usize;
        while let Some((c, _)) = n.pop_chunk() {
            assert_eq!(c.len(), 320, "8k stereo 20ms = 160 frame * 2 = 320 sample");
            chunks += 1;
        }
        assert!(
            (47..=50).contains(&chunks),
            "expected ~50 chunks, got {chunks}"
        );
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

    // --- 値検証ヘルパ（振幅・周波数の保存を確認する） ---

    /// サンプル列の RMS（線形）。正弦波なら振幅 A に対し A/√2 になる。
    fn rms(samples: &[f32]) -> f32 {
        if samples.is_empty() {
            return 0.0;
        }
        let sum_sq: f64 = samples.iter().map(|&x| (x as f64) * (x as f64)).sum();
        (sum_sq / samples.len() as f64).sqrt() as f32
    }

    /// 正→負 / 負→正 のゼロ交差回数を数える。1 周期で 2 回交差するので、
    /// 推定周波数 = (交差数 / 2) / 秒数。先頭/末尾の過渡を避けて中央を渡すこと。
    fn zero_crossings(samples: &[f32]) -> usize {
        let mut crossings = 0;
        for w in samples.windows(2) {
            // 厳密な符号反転のみ（0 ちょうどは無視）。
            if (w[0] < 0.0 && w[1] >= 0.0) || (w[0] >= 0.0 && w[1] < 0.0) {
                crossings += 1;
            }
        }
        crossings
    }

    /// 44.1kHz/mono 440Hz 正弦を 48kHz/stereo へリサンプルしても、振幅（RMS）と
    /// 周波数（ゼロ交差推定）が保存される。リサンプラのリンギングを避けるため
    /// 1 秒ぶん流して中央のチャンク群だけで測る。
    #[test]
    fn resample_44100_to_48000_preserves_amplitude_and_frequency() {
        let mut n = Normalizer::new(44_100, 1, default_out()).expect("normalizer");
        let freq = 440.0_f32;
        let amp = 0.5_f32;
        let in_rate = 44_100usize;
        // 2 秒ぶん流して十分なチャンクを得る（過渡を捨てる余裕を持つ）。
        let total_frames = in_rate * 2;
        let mut pts = 0i64;
        for blk in 0..(total_frames / 441) {
            let mut block = Vec::with_capacity(441);
            for j in 0..441 {
                let i = blk * 441 + j;
                block.push((2.0 * PI * freq * (i as f32) / in_rate as f32).sin() * amp);
            }
            n.push(&block, pts).expect("push");
            pts += 441 * 1_000_000_000 / in_rate as i64;
        }

        // 全チャンクを連結（出力は 48k/stereo/960frame）。
        let mut left: Vec<f32> = Vec::new();
        while let Some((c, _)) = n.pop_chunk() {
            assert_eq!(c.len(), CHUNK_FRAMES * 2);
            // L チャンネルだけ取り出す（mono→stereo 複製なので L==R）。
            for f in 0..CHUNK_FRAMES {
                assert_eq!(c[f * 2], c[f * 2 + 1], "mono 入力なので L==R");
                left.push(c[f * 2]);
            }
        }
        assert!(left.len() >= 48_000, "1 秒以上の出力が必要: {}", left.len());

        // 過渡（先頭・末尾各 0.25 秒 = 12000 sample）を捨てて中央 1 秒で測る。
        let start = 12_000;
        let mid = &left[start..start + 48_000];

        // 振幅: 正弦の RMS は amp/√2 ≈ 0.3536。リサンプラ通過で ±5% 以内。
        let got_rms = rms(mid);
        let expect_rms = amp / std::f32::consts::SQRT_2;
        let rms_err = ((got_rms - expect_rms) / expect_rms).abs();
        assert!(
            rms_err < 0.05,
            "RMS 保存誤差が大きい: got={got_rms} expect={expect_rms} err={rms_err}"
        );

        // 周波数: 中央 1 秒（48000 sample）のゼロ交差 ≈ 2*440 = 880。±2% 以内。
        let crossings = zero_crossings(mid);
        let est_freq = crossings as f32 / 2.0; // 1 秒なので交差数/2 = Hz。
        let freq_err = ((est_freq - freq) / freq).abs();
        assert!(
            freq_err < 0.02,
            "周波数 保存誤差が大きい: 交差={crossings} 推定={est_freq}Hz err={freq_err}"
        );
    }

    /// 16k/mono 出力の実チャンネル数とサンプル値: 48k/stereo 440Hz 入力を
    /// 16k/mono へ落としても 1ch・320sample で振幅/周波数が保存される。
    #[test]
    fn output_16k_mono_preserves_values() {
        let out = OutputFormat {
            sample_rate: 16_000,
            channels: 1,
        };
        let mut n = Normalizer::new(48_000, 2, out).expect("normalizer");
        let freq = 440.0_f32;
        let amp = 0.5_f32;
        let in_rate = 48_000usize;
        let total_frames = in_rate * 2;
        let mut pts = 0i64;
        for blk in 0..(total_frames / 480) {
            let mut block = Vec::with_capacity(480 * 2);
            for j in 0..480 {
                let i = blk * 480 + j;
                let s = (2.0 * PI * freq * (i as f32) / in_rate as f32).sin() * amp;
                block.push(s); // L
                block.push(s); // R
            }
            n.push(&block, pts).expect("push");
            pts += 480 * 1_000_000_000 / in_rate as i64;
        }

        let mut mono: Vec<f32> = Vec::new();
        while let Some((c, _)) = n.pop_chunk() {
            assert_eq!(c.len(), 320, "16k/mono 20ms = 320 sample（1ch）");
            mono.extend_from_slice(&c);
        }
        assert!(mono.len() >= 16_000, "1 秒以上必要: {}", mono.len());

        // 過渡を捨てて中央 1 秒（16000 sample）で測る。
        let start = 4_000;
        let mid = &mono[start..start + 16_000];

        // L==R の同相信号を平均してもレベル不変 → RMS ≈ amp/√2。
        let got_rms = rms(mid);
        let expect_rms = amp / std::f32::consts::SQRT_2;
        let rms_err = ((got_rms - expect_rms) / expect_rms).abs();
        assert!(
            rms_err < 0.05,
            "16k/mono RMS 保存誤差: got={got_rms} expect={expect_rms} err={rms_err}"
        );

        // 周波数: 16000 sample の中央 1 秒で交差 ≈ 880。±2% 以内。
        let est_freq = zero_crossings(mid) as f32 / 2.0;
        let freq_err = ((est_freq - freq) / freq).abs();
        assert!(
            freq_err < 0.02,
            "16k/mono 周波数 保存誤差: 推定={est_freq}Hz err={freq_err}"
        );
    }

    /// PTS は単調増加し、隣接チャンク間の delta が ~20ms（1e7 ns ±許容）になる。
    /// 48k パススルー経路で PTS アンカーが正しく外挿されることを値で確認する。
    #[test]
    fn pts_delta_is_about_20ms_between_chunks() {
        let mut n = Normalizer::new(48_000, 2, default_out()).expect("normalizer");
        // 480 frame（10ms）ずつ pts 付きで push（実機の小バッファ到着を模す）。
        let mut device_pts = 1_000_000_000i64; // 任意の原点。
        let block_frames = 480usize;
        for _ in 0..20 {
            let stereo = vec![0.1f32; block_frames * 2];
            n.push(&stereo, device_pts).expect("push");
            device_pts += block_frames as i64 * 1_000_000_000 / 48_000;
        }

        let mut pts_list = Vec::new();
        while let Some((_, pts)) = n.pop_chunk() {
            pts_list.push(pts);
        }
        assert!(
            pts_list.len() >= 5,
            "十分なチャンク数が必要: {}",
            pts_list.len()
        );

        // 20ms = 20_000_000 ns。許容 ±5%（1e6 ns）。
        for w in pts_list.windows(2) {
            let delta = w[1] - w[0];
            assert!(delta > 0, "PTS は厳密に増加: {} -> {}", w[0], w[1]);
            assert!(
                (delta - 20_000_000).abs() <= 1_000_000,
                "隣接 PTS delta が ~20ms でない: {delta} ns"
            );
        }
    }

    /// 入力サンプルが空 / 端数（in_channels の倍数未満）でも panic せず Ok を返し、
    /// チャンクは生成されない（境界・防御）。
    #[test]
    fn push_empty_and_subframe_are_noops() {
        let mut n = Normalizer::new(48_000, 2, default_out()).expect("normalizer");
        // 空。
        n.push(&[], 0).expect("empty push ok");
        // stereo(2ch) なのに 1 サンプルだけ → in_frames=0 で早期 return。
        n.push(&[0.5], 0).expect("subframe push ok");
        assert!(n.pop_chunk().is_none(), "端数だけでは 1 チャンクも出ない");
        assert_eq!(n.buffered_out_frames(), 0);
    }

    /// 周波数 0（無音 DC）入力は出力も全 0（peak/rms 0 経路の裏取り）。
    #[test]
    fn silence_input_yields_zero_output() {
        let mut n = Normalizer::new(48_000, 2, default_out()).expect("normalizer");
        let stereo = vec![0.0f32; CHUNK_FRAMES * 2];
        n.push(&stereo, 0).expect("push");
        let (chunk, _) = n.pop_chunk().expect("one chunk");
        assert!(chunk.iter().all(|&s| s == 0.0), "無音入力は無音出力");
    }

    // --- 副タップ（デュアル出力）の検証 ---

    /// 副タップ未設定なら `pop_secondary` は常に `None`・`has_secondary` は false。
    #[test]
    fn no_secondary_tap_by_default() {
        let mut n = Normalizer::new(48_000, 2, default_out()).expect("normalizer");
        assert!(!n.has_secondary());
        assert_eq!(n.secondary_output(), None);
        let stereo = vec![0.1f32; CHUNK_FRAMES * 2];
        n.push(&stereo, 0).expect("push");
        assert!(n.pop_secondary().is_none(), "副タップ無しなら None");
    }

    /// 主 48k/stereo + 副 16k/mono を 1 度の第 1 段から両立して生成する。主は 960frame/
    /// stereo、副は 320frame/mono を出す。両者はほぼ 50 チャンク/秒。
    #[test]
    fn dual_output_primary_and_secondary_shapes() {
        let secondary = OutputFormat {
            sample_rate: 16_000,
            channels: 1,
        };
        let mut n = Normalizer::new(48_000, 2, default_out())
            .expect("normalizer")
            .with_secondary(secondary)
            .expect("secondary");
        assert!(n.has_secondary());
        assert_eq!(n.secondary_output(), Some(secondary));

        // 1 秒分の 48k/stereo を 480 frame ずつ push。
        let mut pts = 0i64;
        for _ in 0..100 {
            let block = vec![0.2f32; 480 * 2];
            n.push(&block, pts).expect("push");
            pts += 480 * 1_000_000_000 / 48_000;
        }

        let mut primary_chunks = 0usize;
        while let Some((c, _)) = n.pop_chunk() {
            assert_eq!(c.len(), CHUNK_FRAMES * 2, "主は 48k/stereo = 1920 sample");
            primary_chunks += 1;
        }
        let mut secondary_chunks = 0usize;
        while let Some((c, _)) = n.pop_secondary() {
            assert_eq!(c.len(), 320, "副は 16k/mono = 320 sample");
            secondary_chunks += 1;
        }
        assert!(
            (47..=50).contains(&primary_chunks),
            "主 ~50 チャンク: {primary_chunks}"
        );
        assert!(
            (47..=50).contains(&secondary_chunks),
            "副 ~50 チャンク: {secondary_chunks}"
        );
    }

    /// 主副タップとも同じ push 由来の PTS 軸に乗り、それぞれ隣接 20ms で単調増加する
    /// （各タップは独立の PTS アンカーを持つ）。
    #[test]
    fn dual_output_taps_share_pts_axis() {
        let secondary = OutputFormat {
            sample_rate: 16_000,
            channels: 1,
        };
        let mut n = Normalizer::new(48_000, 2, default_out())
            .expect("normalizer")
            .with_secondary(secondary)
            .expect("secondary");

        let mut device_pts = 1_000_000_000i64;
        for _ in 0..40 {
            let block = vec![0.1f32; 480 * 2];
            n.push(&block, device_pts).expect("push");
            device_pts += 480 * 1_000_000_000 / 48_000;
        }

        let mut primary_pts = Vec::new();
        while let Some((_, p)) = n.pop_chunk() {
            primary_pts.push(p);
        }
        let mut secondary_pts = Vec::new();
        while let Some((_, p)) = n.pop_secondary() {
            secondary_pts.push(p);
        }
        assert!(primary_pts.len() >= 5 && secondary_pts.len() >= 5);
        for w in primary_pts.windows(2) {
            assert!((w[1] - w[0] - 20_000_000).abs() <= 1_000_000);
        }
        for w in secondary_pts.windows(2) {
            assert!((w[1] - w[0] - 20_000_000).abs() <= 1_000_000);
        }
        // 両タップの先頭 PTS は同じ push 原点近傍から始まる（数十ms 以内）。
        assert!(
            (primary_pts[0] - secondary_pts[0]).abs() < 100_000_000,
            "主副の開始 PTS は近接するはず: {} vs {}",
            primary_pts[0],
            secondary_pts[0]
        );
    }

    // --- InnerProcessor（denoise フック相当）と stop flush ---

    /// テスト用プロセッサ: 全サンプルを 2 倍する（末尾テールは持たない）。
    struct DoubleProcessor;
    impl InnerProcessor for DoubleProcessor {
        fn process(&mut self, s: &mut [f32]) {
            for x in s.iter_mut() {
                *x *= 2.0;
            }
        }
        fn flush(&mut self) -> Vec<f32> {
            Vec::new()
        }
    }

    /// テスト用プロセッサ: `hold` サンプルの固定遅延線（denoise の遅延線を模す）。
    /// 出力は入力を `hold` サンプル遅らせた列（先頭 `hold` は無音）。`flush` で末尾
    /// `hold` サンプルを返す。
    struct DelayProcessor {
        held: Vec<f32>,
    }
    impl DelayProcessor {
        fn new(hold: usize) -> Self {
            Self {
                held: vec![0.0; hold],
            }
        }
    }
    impl InnerProcessor for DelayProcessor {
        fn process(&mut self, s: &mut [f32]) {
            self.held.extend_from_slice(s);
            let n = s.len();
            s.copy_from_slice(&self.held[..n]);
            self.held.drain(..n);
        }
        fn flush(&mut self) -> Vec<f32> {
            std::mem::take(&mut self.held)
        }
    }

    /// InnerProcessor は内部正規形へ第 2 段分岐前に適用される（主 48k/stereo パススルー
    /// では出力がそのまま 2 倍になる）。
    #[test]
    fn inner_processor_applies_before_stage2() {
        let mut n = Normalizer::new(48_000, 2, default_out())
            .expect("normalizer")
            .with_inner_processor(Box::new(DoubleProcessor));
        let stereo: Vec<f32> = (0..CHUNK_FRAMES * 2).map(|i| (i as f32) * 1e-4).collect();
        n.push(&stereo, 0).expect("push");
        let (chunk, _) = n.pop_chunk().expect("one chunk");
        for (i, &s) in chunk.iter().enumerate() {
            assert!(
                (s - stereo[i] * 2.0).abs() < 1e-6,
                "sample {i} should be doubled"
            );
        }
    }

    /// InnerProcessor は主・副の両タップへ効く（副 16k/mono も 2 倍になる）。
    #[test]
    fn inner_processor_affects_both_taps() {
        let secondary = OutputFormat {
            sample_rate: 48_000,
            channels: 2,
        };
        // 副も 48k/stereo（パススルー）にして、2 倍が素通しで観測できるようにする。
        let mut n = Normalizer::new(48_000, 2, default_out())
            .expect("normalizer")
            .with_secondary(secondary)
            .expect("secondary")
            .with_inner_processor(Box::new(DoubleProcessor));
        let stereo = vec![0.25f32; CHUNK_FRAMES * 2];
        n.push(&stereo, 0).expect("push");
        let (p, _) = n.pop_chunk().expect("primary chunk");
        let (s, _) = n.pop_secondary().expect("secondary chunk");
        assert!(p.iter().all(|&x| (x - 0.5).abs() < 1e-6), "主が 2 倍");
        assert!(s.iter().all(|&x| (x - 0.5).abs() < 1e-6), "副も 2 倍");
    }

    /// stop flush はプロセッサの末尾テールを流し込み、末尾チャンクとして取り出せる。
    /// 遅延線プロセッサの held 分が flush 後の追加チャンクに現れる。
    #[test]
    fn stop_flush_emits_processor_tail() {
        // hold = 4 sample（2 stereo frame）の遅延線。
        let mut n = Normalizer::new(48_000, 2, default_out())
            .expect("normalizer")
            .with_inner_processor(Box::new(DelayProcessor::new(4)));
        // 非ゼロの識別可能な入力を 1 チャンク push。
        let stereo: Vec<f32> = (0..CHUNK_FRAMES * 2)
            .map(|i| (i as f32 + 1.0) * 1e-4)
            .collect();
        n.push(&stereo, 0).expect("push");

        // flush 前: 1 チャンク（先頭 4 sample は遅延の無音）。
        let (c0, _) = n.pop_chunk().expect("first chunk");
        assert_eq!(c0.len(), CHUNK_FRAMES * 2);
        assert!(
            c0[..4].iter().all(|&x| x == 0.0),
            "先頭 4 sample は遅延の無音"
        );
        assert!(n.pop_chunk().is_none(), "flush 前は 1 チャンクだけ");

        // flush: 遅延線の末尾 4 sample が追加チャンク（無音パディング付き）で出る。
        n.flush();
        let (c1, _) = n.pop_chunk().expect("flushed tail chunk");
        assert_eq!(
            c1.len(),
            CHUNK_FRAMES * 2,
            "末尾チャンクは 20ms へパディング"
        );
        // 末尾 4 sample = 入力の最後の 4 sample。
        let last4 = &stereo[stereo.len() - 4..];
        for (i, &x) in c1[..4].iter().enumerate() {
            assert!(
                (x - last4[i]).abs() < 1e-6,
                "flush テールが入力末尾に一致するはず"
            );
        }
    }

    /// stop flush は副タップの第 2 段リサンプラ残余も吐き出す（16k/mono でも末尾が届く）。
    #[test]
    fn stop_flush_drains_secondary_resampler() {
        let secondary = OutputFormat {
            sample_rate: 16_000,
            channels: 1,
        };
        let mut n = Normalizer::new(48_000, 2, default_out())
            .expect("normalizer")
            .with_secondary(secondary)
            .expect("secondary");
        // ちょうど 1 チャンク未満に近い量を push（リサンプラに端数が残る）。
        let stereo = vec![0.3f32; CHUNK_FRAMES * 2];
        n.push(&stereo, 0).expect("push");

        // flush 前に取れる副チャンク数を数える。
        let mut before = 0usize;
        while n.pop_secondary().is_some() {
            before += 1;
        }
        n.flush();
        // flush 後に末尾チャンクが 1 つ以上追加される（残余の吐き出し）。
        let mut after = 0usize;
        while let Some((c, _)) = n.pop_secondary() {
            assert_eq!(c.len(), 320, "副は 20ms 固定境界へ揃う");
            after += 1;
        }
        assert!(after >= 1, "flush で副タップの末尾が吐き出されるはず");
        let _ = before;
    }
}
