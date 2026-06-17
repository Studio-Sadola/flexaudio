//! flexaudio-vad — silero-VAD を ONNX でオフライン実行する VAD アドオン。
//!
//! `flexaudio-core` には依存せず、`&[f32]` のサンプル列だけを受け取る。silero-VAD
//! モデル (MIT) をバイナリに埋め込むので、実行時のモデルファイルもネットワークも要らない。
//!
//! # 例 (ストリーミング)
//! ```no_run
//! use flexaudio_vad::{Vad, VadConfig, VadEvent};
//! let mut vad = Vad::new(VadConfig::default()).unwrap();
//! for chunk in some_audio_chunks() {
//!     for ev in vad.process(chunk) {
//!         match ev {
//!             VadEvent::SpeechStart { at_sample } => println!("start @ {at_sample}"),
//!             VadEvent::SpeechEnd { at_sample } => println!("end @ {at_sample}"),
//!         }
//!     }
//! }
//! # fn some_audio_chunks() -> Vec<&'static [f32]> { vec![] }
//! ```
//!
//! # 例 (バッチ)
//! ```no_run
//! use flexaudio_vad::{get_speech_timestamps, VadConfig};
//! let samples: Vec<f32> = vec![0.0; 16000];
//! let segments = get_speech_timestamps(&samples, &VadConfig::default()).unwrap();
//! for s in segments {
//!     println!("{}..{}", s.start_sample, s.end_sample);
//! }
//! ```

#![warn(missing_docs)]

mod config;
mod segmenter;

pub use config::VadConfig;
pub use segmenter::Segment;

use ort::session::Session;
use ort::value::Tensor;
use segmenter::Segmenter;

/// silero-VAD モデル (v6, MIT)。実行時ファイルを要らなくするためバイナリへ埋め込む。
static MODEL_BYTES: &[u8] = include_bytes!("../assets/silero_vad.onnx");

/// state テンソルの要素数 (2 * 1 * 128)。
const STATE_LEN: usize = 2 * 128;

/// VAD が確定したイベント。サンプル位置はパディング適用後。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VadEvent {
    /// 発話開始 (パディング適用後の絶対サンプル位置)。
    SpeechStart {
        /// 発話が始まった絶対サンプル位置 (パディング適用後・含む)。
        at_sample: u64,
    },
    /// 発話終了 (パディング適用後の絶対サンプル位置・排他的)。
    SpeechEnd {
        /// 発話が終わった絶対サンプル位置 (パディング適用後・排他的)。
        at_sample: u64,
    },
}

/// VAD のエラー型。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VadError {
    /// モデルのロードに失敗。
    ModelLoad(String),
    /// 推論実行中のエラー。
    Inference(String),
    /// 設定値が不正。
    InvalidConfig(String),
}

impl std::fmt::Display for VadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VadError::ModelLoad(m) => write!(f, "model load error: {m}"),
            VadError::Inference(m) => write!(f, "inference error: {m}"),
            VadError::InvalidConfig(m) => write!(f, "invalid config: {m}"),
        }
    }
}

impl std::error::Error for VadError {}

/// ストリーミング VAD。1 インスタンスが ONNX `Session` を 1 つ持つ（共有しない）。
///
/// 任意長の `&[f32]` を [`Vad::process`] に流すと、内部で frame (16k=512) 単位に束ねて
/// silero 推論し、セグメント状態機械を回して確定したイベントを返す。
pub struct Vad {
    session: Session,
    config: VadConfig,
    segmenter: Segmenter,

    /// silero state テンソル `[2,1,128]`（フレーム間で継承）。
    state: Vec<f32>,
    /// 前回フレーム末尾の context (16k=64)。次フレームの前置に使う。
    context: Vec<f32>,
    /// frame_size に満たない端数サンプルの残り。
    pending: Vec<f32>,

    /// 直近 [`Vad::process`] で計算した各フレームの生発話確率。
    last_probs: Vec<f32>,
}

impl Vad {
    /// 埋め込みモデルをロードして VAD を構築する。
    pub fn new(config: VadConfig) -> Result<Vad, VadError> {
        config.validate().map_err(VadError::InvalidConfig)?;

        let session = Session::builder()
            .map_err(|e| VadError::ModelLoad(e.to_string()))?
            .commit_from_memory(MODEL_BYTES)
            .map_err(|e| VadError::ModelLoad(e.to_string()))?;

        let segmenter = Segmenter::new(&config);
        let ctx_size = config.context_size();

        Ok(Vad {
            session,
            config,
            segmenter,
            state: vec![0.0f32; STATE_LEN],
            context: vec![0.0f32; ctx_size],
            pending: Vec::new(),
            last_probs: Vec::new(),
        })
    }

    /// 任意長の f32 サンプルを処理し、確定した [`VadEvent`] を返す。
    ///
    /// 内部で frame_size 単位に束ね、満たないサンプルは次回まで保持する。サンプル位置は
    /// 呼び出しをまたいで連続する（累積）。
    pub fn process(&mut self, samples: &[f32]) -> Vec<VadEvent> {
        let frame_size = self.config.frame_size();
        self.last_probs.clear();

        let mut segments_out = Vec::new();
        let mut events = Vec::new();

        // pending + 新規サンプルを連結して frame_size 単位に消費。
        self.pending.extend_from_slice(samples);

        let mut offset = 0;
        // self.pending を借用したまま &mut self の infer_frame には渡せないので、
        // 1 フレーム分を局所バッファへコピーしてから推論する。
        let mut frame_buf = vec![0.0f32; frame_size];
        while offset + frame_size <= self.pending.len() {
            frame_buf.copy_from_slice(&self.pending[offset..offset + frame_size]);
            // 推論失敗時は無音 (0.0) に倒して継続。
            let prob = self.infer_frame(&frame_buf).unwrap_or(0.0);
            self.last_probs.push(prob);
            self.segmenter.feed(prob, &mut segments_out);
            offset += frame_size;
        }
        // 消費済み分を捨てる。
        self.pending.drain(0..offset);

        for seg in segments_out {
            events.push(VadEvent::SpeechStart {
                at_sample: seg.start_sample,
            });
            events.push(VadEvent::SpeechEnd {
                at_sample: seg.end_sample,
            });
        }
        events
    }

    /// 1 フレーム (frame_size サンプル) を silero に通し発話確率を返す。
    ///
    /// silero の入力は frame そのものではなく `concat(context, frame)` で、長さは
    /// `context_size + frame_size` (16k=64+512=576)。推論後、context を今回入力末尾の
    /// `context_size` サンプルで、state を出力 state で更新する。
    fn infer_frame(&mut self, frame: &[f32]) -> Result<f32, VadError> {
        let ctx_size = self.config.context_size();
        let frame_size = self.config.frame_size();
        debug_assert_eq!(frame.len(), frame_size);
        debug_assert_eq!(self.context.len(), ctx_size);

        // x = concat(context, frame)。長さ ctx_size + frame_size (16k=576)。
        let input_len = ctx_size + frame_size;
        let mut x = Vec::with_capacity(input_len);
        x.extend_from_slice(&self.context);
        x.extend_from_slice(frame);

        // 次回 context = 今回入力末尾の ctx_size サンプル。x は run へ move されるので
        // 先に控えておく。
        let next_context: Vec<f32> = x[input_len - ctx_size..].to_vec();

        // 入力テンソル: input f32 [1, input_len], state f32 [2,1,128], sr int64 scalar。
        let input_tensor = Tensor::from_array(([1_i64, input_len as i64], x))
            .map_err(|e| VadError::Inference(e.to_string()))?;
        let state_tensor = Tensor::from_array(([2_i64, 1, 128], self.state.clone()))
            .map_err(|e| VadError::Inference(e.to_string()))?;
        // sr はランク0スカラー int64（モデル入力 shape は []）。
        let sr_tensor =
            Tensor::from_array((vec![] as Vec<i64>, vec![self.config.sample_rate as i64]))
                .map_err(|e| VadError::Inference(e.to_string()))?;

        let outputs = self
            .session
            .run(ort::inputs![
                "input" => input_tensor,
                "state" => state_tensor,
                "sr" => sr_tensor,
            ])
            .map_err(|e| VadError::Inference(e.to_string()))?;

        // 出力名: 確率 = "output", 更新後 state = "stateN"（実モデルで確認済み）。
        let (_pshape, prob_slice) = outputs["output"]
            .try_extract_tensor::<f32>()
            .map_err(|e| VadError::Inference(e.to_string()))?;
        let prob = *prob_slice
            .first()
            .ok_or_else(|| VadError::Inference("empty output tensor".to_string()))?;

        let (_sshape, state_slice) = outputs["stateN"]
            .try_extract_tensor::<f32>()
            .map_err(|e| VadError::Inference(e.to_string()))?;
        if state_slice.len() != STATE_LEN {
            return Err(VadError::Inference(format!(
                "unexpected state length {} (expected {STATE_LEN})",
                state_slice.len()
            )));
        }
        let new_state = state_slice.to_vec();

        // 出力の借用が outputs/session を握っているので、コピーし終えてから更新する。
        drop(outputs);
        self.state = new_state;
        self.context = next_context;

        Ok(prob)
    }

    /// 直近 [`Vad::process`] で計算した各フレームの生発話確率を返す。
    ///
    /// セグメントイベントとは独立した第二の出力。
    pub fn last_frame_probabilities(&self) -> &[f32] {
        &self.last_probs
    }

    /// state / context / 状態機械 / サンプル位置 / 端数バッファをすべて初期化する。
    pub fn reset(&mut self) {
        self.state = vec![0.0f32; STATE_LEN];
        self.context = vec![0.0f32; self.config.context_size()];
        self.pending.clear();
        self.last_probs.clear();
        self.segmenter.reset();
    }

    /// 現在の設定への参照。
    pub fn config(&self) -> &VadConfig {
        &self.config
    }
}

/// バッチ処理用（silero `get_speech_timestamps` 相当）。
///
/// 全サンプルを一括処理して確定セグメント列を返す。末尾で発話中なら入力終端まで採る。
pub fn get_speech_timestamps(
    samples: &[f32],
    config: &VadConfig,
) -> Result<Vec<Segment>, VadError> {
    let mut vad = Vad::new(config.clone())?;
    let frame_size = config.frame_size();

    let mut out = Vec::new();
    let mut chunks = samples.chunks_exact(frame_size);
    for frame in chunks.by_ref() {
        let prob = vad.infer_frame(frame)?;
        vad.last_probs.push(prob);
        vad.segmenter.feed(prob, &mut out);
    }
    // 端数フレームは silero と同じく推論せず捨てる。入力終端で発話中なら確定。
    vad.segmenter.flush(&mut out);

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::VadConfig;
    use crate::segmenter::{Segment, Segmenter};

    /// frame_size=512 @16k 前提で、確率列をセグメンタに流しセグメント列を得るヘルパ。
    fn run_probs(config: &VadConfig, probs: &[f32]) -> Vec<Segment> {
        let mut seg = Segmenter::new(config);
        let mut out = Vec::new();
        for &p in probs {
            seg.feed(p, &mut out);
        }
        seg.flush(&mut out);
        out
    }

    fn base_config() -> VadConfig {
        // pad=0 にして純粋な境界ロジックを検証しやすくする。512 サンプル/フレーム前提。
        VadConfig {
            threshold: 0.5,
            neg_threshold: Some(0.35),
            min_speech_ms: 0, // 破棄を切る (個別テストで上書き)
            min_silence_ms: 0,
            speech_pad_ms: 0,
            max_speech_ms: 0,
            sample_rate: 16000,
        }
    }

    #[test]
    fn neg_threshold_default_formula() {
        let mut c = VadConfig::default();
        assert_eq!(c.resolved_neg_threshold(), (0.5 - 0.15_f32).max(0.01));
        c.threshold = 0.1;
        assert_eq!(c.resolved_neg_threshold(), 0.01); // クランプ下限
        c.neg_threshold = Some(0.2);
        assert_eq!(c.resolved_neg_threshold(), 0.2); // 明示優先
    }

    #[test]
    fn simple_speech_then_silence() {
        // 512 サンプル/フレーム。min_silence=512 (=1フレーム), min_speech=0。
        let mut c = base_config();
        c.min_silence_ms = 32; // 32ms @16k = 512 samples = 1 frame
                               // 20 フレーム発話 → 30 フレーム無音
        let mut probs = vec![0.9f32; 20];
        probs.extend(vec![0.1f32; 30]);
        let segs = run_probs(&c, &probs);
        assert_eq!(segs.len(), 1, "exactly one segment");
        let s = segs[0];
        // 開始 = フレーム0先頭 = 0。
        assert_eq!(s.start_sample, 0);
        // 終了 = 無音開始位置 (フレーム20先頭 = 20*512)。
        assert_eq!(s.end_sample, 20 * 512);
    }

    #[test]
    fn min_speech_discards_short_segment() {
        let mut c = base_config();
        c.min_silence_ms = 32; // 1 frame
        c.min_speech_ms = 250; // 250ms @16k = 4000 samples ≈ 7.8 frames
                               // 5 フレーム発話 (5*512=2560 < 4000) → 破棄されるべき
        let mut probs = vec![0.9f32; 5];
        probs.extend(vec![0.1f32; 10]);
        let segs = run_probs(&c, &probs);
        assert!(
            segs.is_empty(),
            "short segment must be discarded, got {segs:?}"
        );

        // 10 フレーム発話 (5120 >= 4000) → 採用
        let mut probs2 = vec![0.9f32; 10];
        probs2.extend(vec![0.1f32; 10]);
        let segs2 = run_probs(&c, &probs2);
        assert_eq!(segs2.len(), 1);
        assert_eq!(segs2[0].len_samples(), 10 * 512);
    }

    #[test]
    fn min_silence_boundary_keeps_segment_together() {
        // 短い無音 (< min_silence) はセグメントを切らない。
        let mut c = base_config();
        c.min_silence_ms = 192; // 192ms @16k = 3072 samples = 6 frames
                                // 10発話 → 3無音 (3*512=1536 < 3072 なので継続) → 10発話 → 長い無音
        let mut probs = vec![0.9f32; 10];
        probs.extend(vec![0.1f32; 3]);
        probs.extend(vec![0.9f32; 10]);
        probs.extend(vec![0.1f32; 10]); // 10*512=5120 >= 3072 で確定
        let segs = run_probs(&c, &probs);
        assert_eq!(segs.len(), 1, "short gap must NOT split, got {segs:?}");
        assert_eq!(segs[0].start_sample, 0);
        // 終端 = 2回目発話塊の後の無音開始 = フレーム23先頭。
        assert_eq!(segs[0].end_sample, 23 * 512);
    }

    #[test]
    fn min_silence_just_over_splits() {
        // min_silence をちょうど超える無音は切る。
        let mut c = base_config();
        c.min_silence_ms = 64; // 64ms = 1024 samples = 2 frames
                               // 5発話 → 3無音 (3*512=1536) → 5発話 → 無音
                               // 無音3フレーム目で (frame_start - temp_end) を評価:
                               //   temp_end は無音1フレーム目先頭。無音Nフレーム目先頭 - temp_end = (N-1)*512。
                               //   >= 1024 となるのは N-1 >= 2 → N>=3 → 3フレーム目の feed で確定。
        let mut probs = vec![0.9f32; 5];
        probs.extend(vec![0.1f32; 3]);
        probs.extend(vec![0.9f32; 5]);
        probs.extend(vec![0.1f32; 5]);
        let segs = run_probs(&c, &probs);
        assert_eq!(segs.len(), 2, "long gap must split into two, got {segs:?}");
        assert_eq!(segs[0].start_sample, 0);
        assert_eq!(segs[0].end_sample, 5 * 512); // 無音開始位置
    }

    #[test]
    fn speech_pad_extends_and_clamps() {
        let mut c = base_config();
        c.min_silence_ms = 32; // 1 frame
        c.speech_pad_ms = 32; // 32ms = 512 samples
                              // フレーム2..5 が発話 (先頭に無音2フレームを置き、開始 pad が 0 でクランプされない様に)
        let mut probs = vec![0.1f32; 2]; // フレーム0,1 無音
        probs.extend(vec![0.9f32; 4]); // フレーム2..5 発話 (開始=2*512=1024)
        probs.extend(vec![0.1f32; 5]); // 無音
        let segs = run_probs(&c, &probs);
        assert_eq!(segs.len(), 1);
        // 開始 = 1024 - 512 = 512。
        assert_eq!(segs[0].start_sample, 1024 - 512);
        // 終了 = 無音開始(6*512=3072) + 512 = 3584。
        assert_eq!(segs[0].end_sample, 6 * 512 + 512);
    }

    #[test]
    fn speech_pad_start_clamps_at_zero() {
        // フレーム0から発話 → 開始 pad が underflow せず 0 になる。
        let mut c = base_config();
        c.min_silence_ms = 32;
        c.speech_pad_ms = 64; // 1024 samples
        let mut probs = vec![0.9f32; 5];
        probs.extend(vec![0.1f32; 5]);
        let segs = run_probs(&c, &probs);
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].start_sample, 0, "start pad must clamp to 0");
    }

    #[test]
    fn pad_does_not_overlap_previous_segment() {
        // 2 セグメントで、後段の開始 pad が前段の終了 pad を侵さないようクランプ。
        let mut c = base_config();
        c.min_silence_ms = 64; // 2 frames
        c.speech_pad_ms = 192; // 3072 samples = 6 frames (大きめに)
                               // 5発話 → 3無音 (確定) → 5発話 → 無音
        let mut probs = vec![0.9f32; 5];
        probs.extend(vec![0.1f32; 3]);
        probs.extend(vec![0.9f32; 5]);
        probs.extend(vec![0.1f32; 5]);
        let segs = run_probs(&c, &probs);
        assert_eq!(segs.len(), 2);
        // seg0 終端 pad と seg1 開始 pad が重ならない (seg1.start >= seg0.end)。
        assert!(
            segs[1].start_sample >= segs[0].end_sample,
            "seg1.start ({}) must be >= seg0.end ({})",
            segs[1].start_sample,
            segs[0].end_sample
        );
    }

    #[test]
    fn max_speech_forces_split_when_no_silence() {
        // 無音が一切無いまま max_speech を超えたら強制分割。
        let mut c = base_config();
        c.max_speech_ms = 192; // 3072 samples = 6 frames
        c.min_silence_ms = 32;
        // 連続発話 20 フレーム (無音なし)。max_speech=6フレームごとに分割。
        let probs = vec![0.9f32; 20];
        let segs = run_probs(&c, &probs);
        assert!(
            segs.len() >= 2,
            "max_speech must force at least one split, got {segs:?}"
        );
        // 各セグメントが max_speech 程度で切られている。
        for s in &segs {
            assert!(
                s.len_samples() <= c.ms_to_samples(c.max_speech_ms) + 512,
                "segment {s:?} exceeds max_speech by more than one frame"
            );
        }
    }

    #[test]
    fn gray_zone_keeps_triggered() {
        // threshold > prob >= neg_threshold のグレーゾーンでは発話継続 (切れない)。
        let mut c = base_config(); // threshold 0.5, neg 0.35
        c.min_silence_ms = 32;
        let mut probs = vec![0.9f32; 5];
        probs.extend(vec![0.4f32; 5]); // グレー (0.35 <= 0.4 < 0.5)
        probs.extend(vec![0.9f32; 5]);
        probs.extend(vec![0.1f32; 5]); // 本当の無音
        let segs = run_probs(&c, &probs);
        assert_eq!(segs.len(), 1, "gray zone must not split, got {segs:?}");
        assert_eq!(segs[0].end_sample, 15 * 512);
    }

    // ---- ONNX 経路スモークテスト ----

    #[test]
    fn vad_loads_model() {
        let vad = Vad::new(VadConfig::default());
        assert!(vad.is_ok(), "model load failed: {:?}", vad.err());
    }

    #[test]
    fn zeros_produce_low_prob_no_speech() {
        let mut vad = Vad::new(VadConfig::default()).unwrap();
        let zeros = vec![0.0f32; 16000];
        let events = vad.process(&zeros);
        // 無音入力では SpeechStart は出ない。
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, VadEvent::SpeechStart { .. })),
            "silence must not trigger SpeechStart, got {events:?}"
        );
        let probs = vad.last_frame_probabilities();
        assert_eq!(probs.len(), 16000 / 512, "expected one prob per frame");
        for &p in probs {
            assert!((0.0..=1.0).contains(&p), "prob {p} out of [0,1]");
            assert!(p < 0.5, "silence prob {p} unexpectedly high");
        }
    }

    #[test]
    fn process_streams_across_calls() {
        // 端数サンプルがコール境界をまたいでも全フレームが処理される。
        let mut vad = Vad::new(VadConfig::default()).unwrap();
        // 300 + 300 + ... で 512 境界をまたぐ。合計 512*4 = 2048 サンプルを小分け。
        let total = 2048usize;
        let chunk = vec![0.0f32; 300];
        let mut fed = 0usize;
        let mut total_probs = 0usize;
        while fed < total {
            let take = chunk.len().min(total - fed);
            vad.process(&chunk[..take]);
            total_probs += vad.last_frame_probabilities().len();
            fed += take;
        }
        assert_eq!(
            total_probs,
            total / 512,
            "all complete frames must be inferred"
        );
    }

    #[test]
    fn reset_clears_state() {
        let mut vad = Vad::new(VadConfig::default()).unwrap();
        vad.process(&vec![0.0f32; 1000]); // pending に端数を残す
        vad.reset();
        assert_eq!(vad.last_frame_probabilities().len(), 0);
        assert_eq!(vad.config().sample_rate, 16000);
    }

    #[test]
    fn invalid_sample_rate_rejected() {
        let c = VadConfig {
            sample_rate: 44100,
            ..VadConfig::default()
        };
        let r = Vad::new(c);
        assert!(matches!(r, Err(VadError::InvalidConfig(_))));
    }

    #[test]
    fn batch_get_speech_timestamps_on_silence() {
        let zeros = vec![0.0f32; 16000];
        let segs = get_speech_timestamps(&zeros, &VadConfig::default()).unwrap();
        assert!(segs.is_empty(), "silence yields no segments, got {segs:?}");
    }
}
