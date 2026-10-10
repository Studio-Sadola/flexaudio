//! Provisional hysteresis and fixed-deadline pieces, independent of canonical segmentation.

use crate::whisper_params::DerivedParams;
use crate::{PreviewCloseReason, PreviewCutReason, WhisperVadEventKind as Kind};

/// One frame advances at most 32 ms; a closure needs at most three events.
/// Stack storage keeps fatal closure independent of further allocations.
#[derive(Default)]
pub(crate) struct PreviewEvents {
    values: [Option<Kind>; 3],
    len: usize,
}

impl PreviewEvents {
    fn push(&mut self, kind: Kind) {
        self.values[self.len] = Some(kind);
        self.len += 1;
    }
}

impl IntoIterator for PreviewEvents {
    type Item = Kind;
    type IntoIter = std::iter::Flatten<std::array::IntoIter<Option<Kind>, 3>>;

    fn into_iter(self) -> Self::IntoIter {
        self.values.into_iter().flatten()
    }
}

#[derive(Debug, Clone, Copy)]
enum State {
    Idle,
    Speaking { piece_start: u64 },
}

#[derive(Debug, Clone)]
pub(crate) struct Preview {
    params: DerivedParams,
    state: State,
    watermark: u64,
}

impl Preview {
    pub fn new(params: DerivedParams) -> Self {
        Self {
            params,
            state: State::Idle,
            watermark: 0,
        }
    }
    pub fn reset(&mut self) {
        self.state = State::Idle;
        self.watermark = 0;
    }

    pub fn feed(&mut self, p: f32, start_ms: u64, valid_end_ms: u64, out: &mut PreviewEvents) {
        // High precedence is observable when threshold is below the lower threshold.
        if p >= self.params.threshold {
            if matches!(self.state, State::Idle) {
                self.state = State::Speaking {
                    piece_start: start_ms,
                };
                out.push(Kind::ProvisionalSpeechStart { at_ms: start_ms });
            }
        } else if p < self.params.negative {
            self.close_at(start_ms, PreviewCloseReason::Hysteresis, out);
        }
        self.advance(valid_end_ms, out);
        self.watermark = valid_end_ms;
    }

    fn advance(&mut self, end_ms: u64, out: &mut PreviewEvents) {
        if let State::Speaking { piece_start } = &mut self.state {
            while end_ms >= *piece_start + 30_000 {
                out.push(Kind::ProvisionalCut {
                    start_ms: *piece_start,
                    end_ms: *piece_start + 30_000,
                    reason: PreviewCutReason::Limit,
                });
                *piece_start += 30_000;
            }
        }
    }

    fn close_at(&mut self, end_ms: u64, reason: PreviewCloseReason, out: &mut PreviewEvents) {
        self.advance(end_ms, out);
        if let State::Speaking { piece_start } = self.state {
            if end_ms > piece_start {
                let cut_reason = match reason {
                    PreviewCloseReason::Hysteresis => PreviewCutReason::Hysteresis,
                    PreviewCloseReason::Finish => PreviewCutReason::Finish,
                    PreviewCloseReason::Reset => PreviewCutReason::Reset,
                    PreviewCloseReason::Error => PreviewCutReason::Error,
                };
                out.push(Kind::ProvisionalCut {
                    start_ms: piece_start,
                    end_ms,
                    reason: cut_reason,
                });
            }
            out.push(Kind::ProvisionalSpeechEnd {
                at_ms: end_ms,
                reason,
            });
            self.state = State::Idle;
        }
    }

    pub fn close(&mut self, reason: PreviewCloseReason, out: &mut PreviewEvents) {
        self.close_at(self.watermark, reason, out);
    }
}
