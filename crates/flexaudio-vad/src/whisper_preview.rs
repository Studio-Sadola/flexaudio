//! Provisional hysteresis and fixed-deadline pieces, independent of canonical segmentation.

use crate::whisper_params::DerivedParams;
use crate::{PreviewCloseReason, PreviewCutReason, WhisperVadEventKind as Kind};

pub(crate) const MAX_PREVIEW_EVENTS: u64 = 3;
const PIECE_CAP_MS: u64 = 30_000;

/// Events are constructed as ordered slots, with no indexed append operation.
/// Session feeds advance at most 32 ms, so advance crosses at most one 30 s
/// deadline. A feed yields start + limit cut, or limit/residual cut + end;
/// closure yields at most limit cut + residual cut + end. Stack storage keeps
/// fatal closure independent of further allocations.
#[derive(Default)]
pub(crate) struct PreviewEvents {
    values: [Option<Kind>; 3],
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

    pub fn feed(&mut self, p: f32, start_ms: u64, valid_end_ms: u64) -> PreviewEvents {
        // High precedence is observable when threshold is below the lower threshold.
        let start = if p >= self.params.threshold {
            if matches!(self.state, State::Idle) {
                self.state = State::Speaking {
                    piece_start: start_ms,
                };
                Some(Kind::ProvisionalSpeechStart { at_ms: start_ms })
            } else {
                None
            }
        } else if p < self.params.negative {
            let events = self.close_at(start_ms, PreviewCloseReason::Hysteresis);
            // Closure leaves the state idle, so advancing valid_end_ms emits nothing.
            self.watermark = valid_end_ms;
            return events;
        } else {
            None
        };
        let limit = self.advance(valid_end_ms);
        self.watermark = valid_end_ms;
        PreviewEvents {
            values: [start, limit, None],
        }
    }

    fn advance(&mut self, end_ms: u64) -> Option<Kind> {
        if let State::Speaking { piece_start } = &mut self.state {
            // Every previous feed already advanced to its valid end. The next
            // frame adds <=32 ms, and close uses that watermark or the next
            // frame's start, so at most one PIECE_CAP_MS deadline is due.
            if end_ms >= *piece_start + PIECE_CAP_MS {
                let cut = Kind::ProvisionalCut {
                    start_ms: *piece_start,
                    end_ms: *piece_start + PIECE_CAP_MS,
                    reason: PreviewCutReason::Limit,
                };
                *piece_start += PIECE_CAP_MS;
                return Some(cut);
            }
        }
        None
    }

    fn close_at(&mut self, end_ms: u64, reason: PreviewCloseReason) -> PreviewEvents {
        let limit = self.advance(end_ms);
        if let State::Speaking { piece_start } = self.state {
            let cut = (end_ms > piece_start).then(|| {
                let cut_reason = match reason {
                    PreviewCloseReason::Hysteresis => PreviewCutReason::Hysteresis,
                    PreviewCloseReason::Finish => PreviewCutReason::Finish,
                    PreviewCloseReason::Reset => PreviewCutReason::Reset,
                    PreviewCloseReason::Error => PreviewCutReason::Error,
                };
                Kind::ProvisionalCut {
                    start_ms: piece_start,
                    end_ms,
                    reason: cut_reason,
                }
            });
            self.state = State::Idle;
            PreviewEvents {
                values: [
                    limit,
                    cut,
                    Some(Kind::ProvisionalSpeechEnd {
                        at_ms: end_ms,
                        reason,
                    }),
                ],
            }
        } else {
            PreviewEvents::default()
        }
    }

    pub fn close(&mut self, reason: PreviewCloseReason) -> PreviewEvents {
        self.close_at(self.watermark, reason)
    }
}
