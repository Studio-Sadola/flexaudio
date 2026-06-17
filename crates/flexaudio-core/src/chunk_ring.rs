//! 完成 [`AudioChunk`] の SPSC リング（ringbuf バック）。
//!
//! **DROP_OLDEST**: 満杯時は最古を pop して新規を push し、ドロップ数を
//! [`AtomicU64`] で数えて次チャンクの `dropped_before` に反映する。
//! consumer は `try_pop()`。
//!
//! ## 同期について
//! ringbuf の overwrite（`push_overwrite`）は producer が最古を pop する必要があり、
//! これは consumer 側のインデックスにも触れるため SPSC のロックフリー前提を満たさない
//! （ringbuf ドキュメント: overwrite を並行に行うにはロックが要る）。本リングの
//! producer は **RT スレッドではなく取り込み/加工スレッド**（通常優先度・§0.7）なので、
//! リング本体を短い [`Mutex`] で保護する。RT 経路（[`crate::raw_ring`]）はこのロックに
//! 一切触れない。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use ringbuf::traits::{Consumer, Observer, RingBuffer};
use ringbuf::HeapRb;

use crate::types::AudioChunk;

type Shared = Arc<Mutex<HeapRb<AudioChunk>>>;

/// 容量 `capacity_chunks` のチャンクリングを作る。
///
/// producer は加工スレッドへ、consumer は poll スレッドへ渡す。`dropped` カウンタは
/// DROP_OLDEST で捨てたチャンク数を数え、次に push されるチャンクの `dropped_before`
/// に反映される。
pub fn chunk_ring(capacity_chunks: usize) -> (ChunkProducer, ChunkConsumer) {
    let cap = capacity_chunks.max(1);
    let rb: Shared = Arc::new(Mutex::new(HeapRb::<AudioChunk>::new(cap)));
    let dropped = Arc::new(AtomicU64::new(0));
    (
        ChunkProducer {
            rb: rb.clone(),
            dropped: dropped.clone(),
        },
        ChunkConsumer { rb, dropped },
    )
}

/// 加工スレッド側のハンドル。DROP_OLDEST 方針で push する。
pub struct ChunkProducer {
    rb: Shared,
    dropped: Arc<AtomicU64>,
}

impl ChunkProducer {
    /// チャンクを push する。満杯なら最古を捨て（DROP_OLDEST）、捨てた数を数える。
    ///
    /// このチャンクの `dropped_before` には、**このチャンクが入るまでに累計で
    /// 捨てられたチャンク数**（自分のための追い出し 1 件を含む）を反映する。
    /// 消費側は連続チャンクの `dropped_before` の差分で「直前の欠落数」を、
    /// 絶対値で「累計欠落数」を知れる。
    ///
    /// 返り値 `Some(total)` は、この push でドロップが発生した場合の累計ドロップ数
    /// （[`crate::types::Event::ChunkDropped`] 発火判断に使える）。ドロップが
    /// 無ければ `None`。
    pub fn push(&mut self, mut chunk: AudioChunk) -> Option<u64> {
        // poison（別スレッドがロック保持中に panic）でも本リングは torn しないので、
        // expect で連鎖 panic させず内部値を回収して処理を続行する（graceful degradation）。
        let mut rb = self.rb.lock().unwrap_or_else(|e| e.into_inner());

        // この push が最古を追い出すか（満杯か）を先に判定する。
        let will_evict = rb.is_full();

        if will_evict {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        // 累計ドロップ数（この push の追い出し分を含む）を反映。
        let total = self.dropped.load(Ordering::Relaxed);
        chunk.dropped_before = u32::try_from(total).unwrap_or(u32::MAX);

        let evicted = rb.push_overwrite(chunk);
        drop(rb);

        debug_assert_eq!(evicted.is_some(), will_evict);

        if will_evict {
            Some(total)
        } else {
            None
        }
    }

    /// これまでに DROP_OLDEST で捨てた累計チャンク数。
    pub fn dropped_count(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// poll スレッド側のハンドル。`try_pop` で消費する。
pub struct ChunkConsumer {
    rb: Shared,
    dropped: Arc<AtomicU64>,
}

impl ChunkConsumer {
    /// 最古のチャンクを 1 つ取り出す。無ければ `None`（非ブロッキング）。
    pub fn try_pop(&mut self) -> Option<AudioChunk> {
        // poison でも torn しないので連鎖 panic させず回収して続行する。
        let mut rb = self.rb.lock().unwrap_or_else(|e| e.into_inner());
        rb.try_pop()
    }

    /// 現在リングに溜まっているチャンク数。
    pub fn len(&self) -> usize {
        // poison でも torn しないので連鎖 panic させず回収して続行する。
        self.rb
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .occupied_len()
    }

    /// リングが空か。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// これまでに DROP_OLDEST で捨てた累計チャンク数。
    pub fn dropped_count(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ChunkFlags;

    fn chunk(seq: u64) -> AudioChunk {
        AudioChunk {
            data: vec![0.0; 1920],
            frames: 960,
            pts_ns: seq as i64 * 20_000_000,
            seq,
            flags: ChunkFlags::empty(),
            dropped_before: 0,
            peak: 0.0,
            rms: 0.0,
        }
    }

    #[test]
    fn fifo_order_when_not_full() {
        let (mut p, mut c) = chunk_ring(4);
        assert!(c.is_empty());
        for s in 0..3 {
            assert_eq!(p.push(chunk(s)), None);
        }
        assert_eq!(c.len(), 3);
        assert_eq!(c.try_pop().unwrap().seq, 0);
        assert_eq!(c.try_pop().unwrap().seq, 1);
        assert_eq!(c.try_pop().unwrap().seq, 2);
        assert!(c.try_pop().is_none());
    }

    #[test]
    fn drop_oldest_when_full_and_counts() {
        let (mut p, mut c) = chunk_ring(2);
        // 容量 2 を埋める。
        assert_eq!(p.push(chunk(0)), None);
        assert_eq!(p.push(chunk(1)), None);
        assert_eq!(p.dropped_count(), 0);

        // 満杯 → 最古(seq0)を捨てて seq2 を入れる。
        let dropped_total = p.push(chunk(2));
        assert_eq!(dropped_total, Some(1));
        assert_eq!(p.dropped_count(), 1);

        // 次の push でさらにもう 1 件ドロップ。
        let dropped_total = p.push(chunk(3));
        assert_eq!(dropped_total, Some(2));
        assert_eq!(p.dropped_count(), 2);

        // 残っているのは最新 2 件 seq2, seq3。
        let first = c.try_pop().unwrap();
        assert_eq!(first.seq, 2);
        // seq2 が入るまでの累計ドロップ = 1（seq0 を捨てた）。
        assert_eq!(first.dropped_before, 1);

        let second = c.try_pop().unwrap();
        assert_eq!(second.seq, 3);
        // seq3 が入るまでの累計ドロップ = 2（seq0, seq1 を捨てた）。
        assert_eq!(second.dropped_before, 2);

        assert!(c.try_pop().is_none());
    }

    #[test]
    fn dropped_before_is_cumulative() {
        let (mut p, mut c) = chunk_ring(1);
        p.push(chunk(0)); // 入る（dropped_before=0）
                          // 容量1で満杯 → 毎回ドロップ。
        p.push(chunk(1)); // seq0 捨て、累計ドロップ=1
        c.try_pop(); // seq1 取り出し → 空く
        let r = p.push(chunk(2)); // 空いているので入る、ドロップ無し
        assert_eq!(r, None);
        let got = c.try_pop().unwrap();
        assert_eq!(got.seq, 2);
        // ドロップは増えていないが累計は 1 のまま保持される。
        assert_eq!(got.dropped_before, 1);
        assert_eq!(p.dropped_count(), 1);
    }
}
