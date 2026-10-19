//! Arrival-time aliases preserve the original timestamp representation exactly.

use crate::{AudioChunk, SecondaryChunk};

impl AudioChunk {
    /// Arrival-time label in nanoseconds, exactly equal to `pts_ns`.
    /// `frame_index` remains the authoritative media timeline.
    pub fn arrival_ns(&self) -> i64 {
        self.pts_ns
    }
}
impl SecondaryChunk {
    /// Arrival-time label in nanoseconds, exactly equal to `pts_ns`.
    /// `frame_index` remains the authoritative media timeline.
    pub fn arrival_ns(&self) -> i64 {
        self.pts_ns
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ChunkFlags;

    #[test]
    fn aliases_preserve_all_integer_labels_and_transport_states() {
        for pts_ns in [i64::MIN, -1, 0, 1, i64::MAX] {
            for flags in [
                ChunkFlags::empty(),
                ChunkFlags::PADDED,
                ChunkFlags::RECOVERED,
                ChunkFlags::DISCONTINUITY,
                ChunkFlags::all(),
            ] {
                let primary = AudioChunk {
                    data: Vec::new(),
                    frames: 0,
                    frame_index: 42,
                    pts_ns,
                    seq: 7,
                    flags,
                    dropped_before: 3,
                    peak: 0.0,
                    rms: 0.0,
                };
                let secondary = SecondaryChunk {
                    samples: Vec::new(),
                    frames: 0,
                    frame_index: 43,
                    pts_ns,
                    seq: 8,
                    flags,
                    dropped_before: 4,
                    peak: 0.0,
                    rms: 0.0,
                };
                assert_eq!(primary.arrival_ns(), primary.pts_ns);
                assert_eq!(secondary.arrival_ns(), secondary.pts_ns);
            }
        }
    }
}
