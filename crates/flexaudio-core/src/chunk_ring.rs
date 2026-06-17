//! 完成 [`AudioChunk`] の SPSC リング（ringbuf バック）。
//!
//! 満杯時は最古を pop して新規を push する（DROP_OLDEST）。ドロップ数を
//! [`AtomicU64`] で数えて次チャンクの `dropped_before` に反映する。consumer は
//! `try_pop()`。
//!
//! ringbuf の overwrite（`push_overwrite`）は producer が最古を pop する必要があり、
//! これは consumer 側のインデックスにも触れるので SPSC のロックフリー前提を満たさない
//! （overwrite を並行に行うにはロックが要る）。このリングの producer は RT スレッド
//! ではなく取り込み/加工スレッド（通常優先度）なので、リング本体を短い [`Mutex`]
//! で保護する。RT 経路（[`mod@crate::raw_ring`]）はこのロックに触れない。

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
    /// `dropped_before` には、このチャンクが入るまでに累計で捨てたチャンク数
    /// （このチャンクのための追い出し 1 件を含む）を入れる。消費側は連続チャンクの
    /// `dropped_before` の差分で直前の欠落数を、絶対値で累計欠落数を知れる。
    ///
    /// この push でドロップが起きたら `Some(累計ドロップ数)` を返す
    /// （[`crate::types::Event::ChunkDropped`] の発火判断に使える）。起きなければ `None`。
    pub fn push(&mut self, mut chunk: AudioChunk) -> Option<u64> {
        // poison（別スレッドがロック保持中に panic）でもリング本体は壊れないので、連鎖
        // panic させず内部値を回収して続ける。
        let mut rb = self.rb.lock().unwrap_or_else(|e| e.into_inner());

        // この push が最古を追い出す（満杯）か先に見ておく。
        let will_evict = rb.is_full();

        if will_evict {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        // この push の追い出し分を含めた累計ドロップ数を載せる。
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
        // poison でもリングは壊れないので回収して続ける。
        let mut rb = self.rb.lock().unwrap_or_else(|e| e.into_inner());
        rb.try_pop()
    }

    /// 現在リングに溜まっているチャンク数。
    pub fn len(&self) -> usize {
        // poison でもリングは壊れないので回収して続ける。
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

    /// 容量 0 は `max(1)` へ丸められ、容量 1 のリングとして機能する（panic しない）。
    #[test]
    fn zero_capacity_is_clamped_to_one() {
        let (mut p, mut c) = chunk_ring(0);
        assert!(c.is_empty());
        assert_eq!(p.push(chunk(0)), None); // 1 件入る。
        assert_eq!(c.len(), 1);
        // 満杯 → 次は最古を捨てる。
        assert_eq!(p.push(chunk(1)), Some(1));
        let got = c.try_pop().unwrap();
        assert_eq!(got.seq, 1);
    }

    /// `len` / `is_empty` が push/pop に追従し、容量を超えない（off-by-one 防止）。
    #[test]
    fn len_tracks_occupancy_and_is_capped() {
        let (mut p, mut c) = chunk_ring(3);
        assert!(c.is_empty());
        for s in 0..3 {
            p.push(chunk(s));
        }
        assert_eq!(c.len(), 3);
        assert!(!c.is_empty());
        // 満杯後にさらに 2 件 → 容量は 3 のまま（最古を捨てて入れ替え）。
        p.push(chunk(3));
        p.push(chunk(4));
        assert_eq!(c.len(), 3, "占有数は容量 3 を超えない");
        // 残るのは最新 3 件 seq2,3,4。
        assert_eq!(c.try_pop().unwrap().seq, 2);
        assert_eq!(c.try_pop().unwrap().seq, 3);
        assert_eq!(c.try_pop().unwrap().seq, 4);
        assert!(c.try_pop().is_none());
        assert!(c.is_empty());
    }

    /// 空リングからの `try_pop` は None（非ブロッキング）。producer/consumer は
    /// dropped カウンタを共有する。
    #[test]
    fn empty_pop_is_none_and_dropped_is_shared() {
        let (mut p, mut c) = chunk_ring(2);
        assert!(c.try_pop().is_none());
        // 容量 2 → 1 件ドロップさせる。
        p.push(chunk(0));
        p.push(chunk(1));
        p.push(chunk(2)); // seq0 を捨てる。
        assert_eq!(p.dropped_count(), 1);
        assert_eq!(c.dropped_count(), 1, "consumer 側も同じ dropped を観測する");
    }
}
