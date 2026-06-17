//! セグメント化状態機械（ONNX 非依存）。
//!
//! silero-VAD 原典 `utils_vad.py::get_speech_timestamps` の判定ロジックを再現する。
//! frame ごとの発話確率を逐次食わせると、min_speech / min_silence / max_speech / pad /
//! neg_threshold を適用したセグメント（開始/終了サンプル位置）を確定順に返す。
//!
//! streaming ([`crate::Vad::process`]) と batch ([`crate::get_speech_timestamps`]) が
//! この同じロジックを使う。サンプル位置は frame index でなく絶対サンプル位置（累積）。

use crate::config::VadConfig;

/// 確定した発話セグメント（パディング適用後・絶対サンプル位置）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    /// 発話開始サンプル位置（パディング適用後）。
    pub start_sample: u64,
    /// 発話終了サンプル位置（パディング適用後・排他的。この位置の手前まで）。
    pub end_sample: u64,
}

impl Segment {
    /// 開始位置を ms に変換（指定サンプルレート基準）。
    pub fn start_ms(&self, sample_rate: u32) -> u64 {
        (self.start_sample * 1000) / u64::from(sample_rate.max(1))
    }

    /// 終了位置を ms に変換（指定サンプルレート基準）。
    pub fn end_ms(&self, sample_rate: u32) -> u64 {
        (self.end_sample * 1000) / u64::from(sample_rate.max(1))
    }

    /// セグメント長（サンプル数）。
    pub fn len_samples(&self) -> u64 {
        self.end_sample.saturating_sub(self.start_sample)
    }
}

/// セグメント化状態機械。
///
/// frame index ではなくフレーム末尾の絶対サンプル位置で進む。1 frame が
/// `frame_size` サンプル (16k=512) で、`feed` ごとに位置が `frame_size` 進む。
#[derive(Debug, Clone)]
pub struct Segmenter {
    threshold: f32,
    neg_threshold: f32,
    min_speech_samples: u64,
    min_silence_samples: u64,
    speech_pad_samples: u64,
    max_speech_samples: u64, // 0 = 無制限
    frame_size: u64,

    /// 発話中フラグ。
    triggered: bool,
    /// 現在の発話の開始サンプル位置（生・未パディング）。
    current_start: u64,
    /// 無音が始まった位置。0 = 無音区間なし（silero と同じ番兵）。
    temp_end: u64,
    /// 次に feed するフレーム末尾の絶対サンプル位置（これまで feed したサンプル総数）。
    next_pos: u64,
    /// 直前に確定（pad 適用後）したセグメント終端。pad 重なりクランプ用。0 = 未確定。
    prev_end: u64,
}

impl Segmenter {
    /// 設定からセグメンタを構築する。
    pub fn new(config: &VadConfig) -> Self {
        Segmenter {
            threshold: config.threshold,
            neg_threshold: config.resolved_neg_threshold(),
            min_speech_samples: config.ms_to_samples(config.min_speech_ms),
            min_silence_samples: config.ms_to_samples(config.min_silence_ms),
            speech_pad_samples: config.ms_to_samples(config.speech_pad_ms),
            max_speech_samples: config.ms_to_samples(config.max_speech_ms),
            frame_size: config.frame_size() as u64,
            triggered: false,
            current_start: 0,
            temp_end: 0,
            next_pos: 0,
            prev_end: 0,
        }
    }

    /// 状態を初期化する。
    pub fn reset(&mut self) {
        self.triggered = false;
        self.current_start = 0;
        self.temp_end = 0;
        self.next_pos = 0;
        self.prev_end = 0;
    }

    /// 1 フレーム分の発話確率を食わせ、確定したセグメントがあれば返す。
    ///
    /// `prob` はそのフレーム [next_pos, next_pos + frame_size) の発話確率。内部位置は
    /// `frame_size` 進む。silero `get_speech_timestamps` のループ本体に対応する。
    pub fn feed(&mut self, prob: f32, out: &mut Vec<Segment>) {
        // このフレームが占める区間 [frame_start, frame_end)。
        let frame_start = self.next_pos;
        let frame_end = frame_start + self.frame_size;
        self.next_pos = frame_end;

        // 発話あり (prob >= threshold)。
        if prob >= self.threshold {
            // 無音カウンタをリセット（silero: temp_end = 0）。
            if self.temp_end != 0 {
                self.temp_end = 0;
            }
            if !self.triggered {
                self.triggered = true;
                // silero は frame index * window。ここでは frame 先頭の絶対サンプル位置。
                self.current_start = frame_start;
            }
            // triggered 中は silero と同じく毎フレーム max_speech を見る。
            self.check_max_speech(frame_end, out);
            return;
        }

        // 無音側 (prob < neg_threshold) かつ発話中。
        if prob < self.neg_threshold && self.triggered {
            if self.temp_end == 0 {
                self.temp_end = frame_start;
            }
            // 無音が min_silence に達したら発話終了を確定。終端は無音開始位置 (temp_end)。
            if frame_start.saturating_sub(self.temp_end) >= self.min_silence_samples {
                self.finalize_segment(self.current_start, self.temp_end, out);
                self.triggered = false;
                self.temp_end = 0;
                return;
            }
            // まだ min_silence 未満。発話継続のまま max_speech 評価へ落ちる。
        }
        // threshold > prob >= neg_threshold のグレーゾーンは silero と同じく状態を
        // 変えず継続する（triggered のまま無音カウントも始めない）。

        // triggered 継続中の無音/グレーフレームでの max_speech 強制分割。
        self.check_max_speech(frame_end, out);
    }

    /// triggered 中に発話長が max_speech を超えていれば強制分割する。
    ///
    /// silero: 直近の無音 (temp_end) があればそこで切って続行、無ければ現フレーム末尾で切る。
    fn check_max_speech(&mut self, frame_end: u64, out: &mut Vec<Segment>) {
        if !self.triggered || self.max_speech_samples == 0 {
            return;
        }
        let speech_len = frame_end.saturating_sub(self.current_start);
        if speech_len <= self.max_speech_samples {
            return;
        }
        if self.temp_end != 0 {
            // 直近の無音位置で分割し、その位置から次発話を継続。
            let split = self.temp_end;
            self.finalize_segment(self.current_start, split, out);
            self.current_start = split;
            self.temp_end = 0;
        } else {
            // 無音が無いまま長すぎ → 現フレーム末尾で分割し継続。
            self.finalize_segment(self.current_start, frame_end, out);
            self.current_start = frame_end;
        }
    }

    /// 入力終端に達したときに呼ぶ。発話中なら現在位置までを最終セグメントとして確定する。
    /// silero は末尾の未確定発話を `audio_length` まで採用する。
    pub fn flush(&mut self, out: &mut Vec<Segment>) {
        if self.triggered {
            self.finalize_segment(self.current_start, self.next_pos, out);
            self.triggered = false;
            self.temp_end = 0;
        }
    }

    /// 生（未パディング）の発話区間を min_speech で篩い、pad を適用して `out` に積む。
    fn finalize_segment(&mut self, raw_start: u64, raw_end: u64, out: &mut Vec<Segment>) {
        // min_speech 未満は捨てる（silero: (end - start) < min_speech_samples）。
        if raw_end.saturating_sub(raw_start) < self.min_speech_samples {
            return;
        }

        // speech_pad: 開始を手前へ、終了を後ろへ広げる。
        let mut start = raw_start.saturating_sub(self.speech_pad_samples);
        let end = raw_end + self.speech_pad_samples;

        // 直前セグメントと pad が重ならないようクランプする。silero は隣接セグメント間の
        // 隙間を pad*2 と比べて中点で割るが、ここでは prev_end を侵さないよう開始を
        // クランプする（等価で保守的）。
        if self.prev_end != 0 && start < self.prev_end {
            start = self.prev_end;
        }

        self.prev_end = end;
        out.push(Segment {
            start_sample: start,
            end_sample: end,
        });
    }
}
