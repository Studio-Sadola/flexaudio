//! VAD 設定。既定値は silero-VAD 原典 (`get_speech_timestamps`) のデフォルトに揃える。

/// VAD の挙動を制御する設定。
///
/// デフォルト値は silero-VAD の `get_speech_timestamps` に揃えてある。
#[derive(Debug, Clone, PartialEq)]
pub struct VadConfig {
    /// 発話開始とみなす確率しきい値 (>=)。既定 0.5。
    pub threshold: f32,
    /// 無音開始とみなす負側しきい値 (<)。`None` なら
    /// `max(threshold - 0.15, 0.01)`（silero 準拠）。
    pub neg_threshold: Option<f32>,
    /// 採用する発話の最小長 (ms)。これ未満のセグメントは破棄。既定 250。
    pub min_speech_ms: u32,
    /// 発話終了の確定に必要な無音長 (ms)。既定 100（silero）。
    pub min_silence_ms: u32,
    /// セグメント境界を前後に広げるパディング (ms)。既定 30（silero）。
    pub speech_pad_ms: u32,
    /// 1 セグメントの最大長 (ms)。0 = 無制限。超過時は強制分割。既定 0。
    pub max_speech_ms: u32,
    /// サンプルレート。8000 または 16000 のみ。既定 16000。
    pub sample_rate: u32,
}

impl Default for VadConfig {
    /// silero のデフォルト（[`VadConfig::balanced`] と同じ）。
    fn default() -> Self {
        VadConfig {
            threshold: 0.5,
            neg_threshold: None,
            min_speech_ms: 250,
            min_silence_ms: 100,
            speech_pad_ms: 30,
            max_speech_ms: 0,
            sample_rate: 16000,
        }
    }
}

impl VadConfig {
    /// 取りこぼしを減らす感度高めのプリセット。
    ///
    /// しきい値を下げ、短い無音でも発話を継続しやすくする。
    pub fn aggressive() -> Self {
        VadConfig {
            threshold: 0.35,
            neg_threshold: None,
            min_speech_ms: 200,
            min_silence_ms: 150,
            speech_pad_ms: 50,
            max_speech_ms: 0,
            sample_rate: 16000,
        }
    }

    /// バランス型プリセット。silero のデフォルト（[`VadConfig::default`]）と同じ。
    pub fn balanced() -> Self {
        VadConfig::default()
    }

    /// 誤検出を減らす保守的なプリセット。
    ///
    /// しきい値を上げ、より長い無音で発話を切る。
    pub fn conservative() -> Self {
        VadConfig {
            threshold: 0.6,
            neg_threshold: None,
            min_speech_ms: 300,
            min_silence_ms: 300,
            speech_pad_ms: 30,
            max_speech_ms: 0,
            sample_rate: 16000,
        }
    }

    /// 実効的な負側しきい値を返す。
    ///
    /// 明示指定があればそれを、なければ `max(threshold - 0.15, 0.01)`（silero 準拠）。
    pub fn resolved_neg_threshold(&self) -> f32 {
        match self.neg_threshold {
            Some(v) => v,
            None => (self.threshold - 0.15).max(0.01),
        }
    }

    /// 16k なら 512、8k なら 256。silero のフレーム長。
    pub(crate) fn frame_size(&self) -> usize {
        if self.sample_rate == 8000 {
            256
        } else {
            512
        }
    }

    /// 16k なら 64、8k なら 32。silero の前置コンテキスト長。
    pub(crate) fn context_size(&self) -> usize {
        if self.sample_rate == 8000 {
            32
        } else {
            64
        }
    }

    /// ms をサンプル数に変換 (現在の `sample_rate` 基準)。
    pub(crate) fn ms_to_samples(&self, ms: u32) -> u64 {
        (u64::from(ms) * u64::from(self.sample_rate)) / 1000
    }

    /// 設定の妥当性を検証する。
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.sample_rate != 8000 && self.sample_rate != 16000 {
            return Err(format!(
                "sample_rate must be 8000 or 16000, got {}",
                self.sample_rate
            ));
        }
        if !(0.0..=1.0).contains(&self.threshold) {
            return Err(format!(
                "threshold must be within [0.0, 1.0], got {}",
                self.threshold
            ));
        }
        if let Some(nt) = self.neg_threshold {
            if !(0.0..=1.0).contains(&nt) {
                return Err(format!("neg_threshold must be within [0.0, 1.0], got {nt}"));
            }
        }
        Ok(())
    }
}
