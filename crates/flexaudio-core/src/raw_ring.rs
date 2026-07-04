//! 生 interleaved f32 デバイスフレーム用の SPSC・RT 安全リング（rtrb バック）。
//!
//! producer（RT コールバック）は slice を非ブロッキングに push する。満杯時は overflow
//! カウンタ（[`AtomicU64`]）を増やして該当分をドロップし、RT スレッドはブロックしない。
//! consumer は取り込みスレッド側で pop する。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use rtrb::{Consumer, Producer, RingBuffer};

/// 生フレームリングを作る。`capacity_samples` は f32 サンプル数での容量。
///
/// 返り値の producer は RT コールバックスレッドへ、consumer は取り込みスレッドへ渡す
/// （SPSC）。`overflow` カウンタは両者で共有し、ドロップ済みサンプル数を数える。
pub fn raw_ring(capacity_samples: usize) -> (RawProducer, RawConsumer) {
    let cap = capacity_samples.max(1);
    let (prod, cons) = RingBuffer::<f32>::new(cap);
    let overflow = Arc::new(AtomicU64::new(0));
    (
        RawProducer {
            inner: prod,
            overflow: overflow.clone(),
        },
        RawConsumer {
            inner: cons,
            overflow,
        },
    )
}

/// RT コールバック側のハンドル。非ブロッキング push のみを行う。
pub struct RawProducer {
    inner: Producer<f32>,
    overflow: Arc<AtomicU64>,
}

impl RawProducer {
    /// interleaved サンプル slice を非ブロッキングに push する。
    ///
    /// 書ける分だけ書き、入り切らなかった残りはドロップして overflow カウンタに加算
    /// する。返り値は実際に書き込めたサンプル数。決してブロックしない。
    pub fn push_slice(&mut self, samples: &[f32]) -> usize {
        if samples.is_empty() {
            return 0;
        }
        let free = self.inner.slots();
        let writable = free.min(samples.len());

        if writable > 0 {
            // write_chunk_uninit でアロケート無しにまとめて書く。
            if let Ok(mut chunk) = self.inner.write_chunk_uninit(writable) {
                let (a, b) = chunk.as_mut_slices();
                let (head, tail) = samples.split_at(a.len().min(samples.len()));
                for (dst, &src) in a.iter_mut().zip(head.iter()) {
                    dst.write(src);
                }
                let tail = &tail[..b.len().min(tail.len())];
                for (dst, &src) in b.iter_mut().zip(tail.iter()) {
                    dst.write(src);
                }
                // SAFETY: writable 個の MaybeUninit を確かに初期化した。
                unsafe { chunk.commit_all() };
            }
        }

        let dropped = samples.len() - writable;
        if dropped > 0 {
            self.overflow.fetch_add(dropped as u64, Ordering::Relaxed);
        }
        writable
    }

    /// これまでにドロップした累計サンプル数。
    pub fn overflow_count(&self) -> u64 {
        self.overflow.load(Ordering::Relaxed)
    }
}

/// 取り込みスレッド側のハンドル。pop する。
pub struct RawConsumer {
    inner: Consumer<f32>,
    overflow: Arc<AtomicU64>,
}

impl RawConsumer {
    /// 利用可能なサンプルを最大 `dst.len()` 個まで `dst` へ取り出す。返り値は取り出し数。
    pub fn pop_slice(&mut self, dst: &mut [f32]) -> usize {
        let avail = self.inner.slots();
        let n = avail.min(dst.len());
        if n == 0 {
            return 0;
        }
        if let Ok(chunk) = self.inner.read_chunk(n) {
            let (a, b) = chunk.as_slices();
            let (alen, blen) = (a.len(), b.len());
            dst[..alen].copy_from_slice(a);
            dst[alen..alen + blen].copy_from_slice(b);
            chunk.commit_all();
            alen + blen
        } else {
            0
        }
    }

    /// 1 サンプル取り出す（無ければ `None`）。
    pub fn pop(&mut self) -> Option<f32> {
        self.inner.pop().ok()
    }

    /// 取り出し可能なサンプル数。
    pub fn available(&self) -> usize {
        self.inner.slots()
    }

    /// これまでに producer 側がドロップした累計サンプル数。
    pub fn overflow_count(&self) -> u64 {
        self.overflow.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_and_pop_roundtrip() {
        let (mut p, mut c) = raw_ring(16);
        let n = p.push_slice(&[1.0, 2.0, 3.0, 4.0]);
        assert_eq!(n, 4);
        let mut out = [0.0f32; 4];
        let got = c.pop_slice(&mut out);
        assert_eq!(got, 4);
        assert_eq!(out, [1.0, 2.0, 3.0, 4.0]);
        assert_eq!(p.overflow_count(), 0);
    }

    #[test]
    fn overflow_counts_dropped_and_never_blocks() {
        let (mut p, mut c) = raw_ring(4);
        // 容量 4 に 6 サンプル push → 4 書けて 2 ドロップ。
        let written = p.push_slice(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
        assert_eq!(written, 4);
        assert_eq!(p.overflow_count(), 2);
        assert_eq!(c.overflow_count(), 2);

        let mut out = [0.0f32; 8];
        let got = c.pop_slice(&mut out);
        assert_eq!(got, 4);
        assert_eq!(&out[..4], &[1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn wraps_around() {
        let (mut p, mut c) = raw_ring(4);
        p.push_slice(&[1.0, 2.0, 3.0]);
        let mut out = [0.0f32; 2];
        c.pop_slice(&mut out); // 2 消費 → read 索引前進
        assert_eq!(out, [1.0, 2.0]);
        // 残 1 + 新 3 = 4 で満杯。折り返し書き込みを検証。
        let w = p.push_slice(&[4.0, 5.0, 6.0]);
        assert_eq!(w, 3);
        let mut out2 = [0.0f32; 4];
        let got = c.pop_slice(&mut out2);
        assert_eq!(got, 4);
        assert_eq!(out2, [3.0, 4.0, 5.0, 6.0]);
    }

    /// 空 slice の push は 0 を返し overflow を増やさない（早期 return 経路）。
    #[test]
    fn push_empty_is_noop() {
        let (mut p, _c) = raw_ring(4);
        assert_eq!(p.push_slice(&[]), 0);
        assert_eq!(p.overflow_count(), 0);
    }

    /// 満杯のリングへさらに push すると全量ドロップ（writable=0）し、
    /// overflow がそのサンプル数ぶん増える。RT 経路がブロックしないことの裏取り。
    #[test]
    fn full_ring_drops_entire_push() {
        let (mut p, _c) = raw_ring(4);
        assert_eq!(p.push_slice(&[1.0, 2.0, 3.0, 4.0]), 4); // 満杯。
                                                            // もう入らない → 5 サンプル全ドロップ。
        let w = p.push_slice(&[5.0; 5]);
        assert_eq!(w, 0, "満杯なら 1 つも書けない");
        assert_eq!(
            p.overflow_count(),
            5,
            "全 5 サンプルが overflow に計上される"
        );
    }

    /// `pop_slice` は dst が available より大きくても available 個だけ取り出す（off-by-one 防止）。
    /// 残量より大きい dst・空リングからの pop=0 を確認。
    #[test]
    fn pop_slice_respects_available_and_dst_len() {
        let (mut p, mut c) = raw_ring(8);
        p.push_slice(&[1.0, 2.0, 3.0]);
        assert_eq!(c.available(), 3);
        // dst が大きくても available(3) だけ取れる。
        let mut big = [0.0f32; 16];
        assert_eq!(c.pop_slice(&mut big), 3);
        assert_eq!(&big[..3], &[1.0, 2.0, 3.0]);
        // 空になったので次は 0。
        assert_eq!(c.available(), 0);
        assert_eq!(c.pop_slice(&mut big), 0);
    }

    /// 連続ドロップで overflow カウンタが u32::MAX を超えても飽和せず u64 で増え続ける
    /// （overflow は AtomicU64・dropped_before の u32 飽和とは別経路）。
    #[test]
    fn overflow_counter_exceeds_u32_max() {
        let (mut p, _c) = raw_ring(1);
        // 1 サンプルだけ書いて満杯にし、以降は全ドロップにする。
        assert_eq!(p.push_slice(&[0.0]), 1);
        // u32::MAX を跨ぐ量をドロップさせる。大 slice を 1 回 push すれば一気に積める。
        let big = vec![0.0f32; 1000];
        let over_u32 = u64::from(u32::MAX) + 2_000;
        let mut total_dropped = 0u64;
        while total_dropped < over_u32 {
            let w = p.push_slice(&big);
            assert_eq!(w, 0, "満杯なので 1 つも書けない");
            total_dropped += big.len() as u64;
        }
        assert!(
            p.overflow_count() > u64::from(u32::MAX),
            "overflow は u32::MAX を超えて積み上がる: {}",
            p.overflow_count()
        );
    }

    /// 単発 `pop()` は 1 サンプルずつ FIFO で返し、空なら None。
    #[test]
    fn single_pop_is_fifo_then_none() {
        let (mut p, mut c) = raw_ring(4);
        p.push_slice(&[10.0, 20.0]);
        assert_eq!(c.pop(), Some(10.0));
        assert_eq!(c.pop(), Some(20.0));
        assert_eq!(c.pop(), None);
    }

    /// 容量 0 指定でも `max(1)` で最低 1 を確保し、push/pop が成立する（panic しない）。
    #[test]
    fn zero_capacity_is_clamped_to_one() {
        let (mut p, mut c) = raw_ring(0);
        assert_eq!(
            p.push_slice(&[7.0, 8.0]),
            1,
            "容量 1 に丸められ 1 サンプルだけ入る"
        );
        assert_eq!(p.overflow_count(), 1);
        assert_eq!(c.pop(), Some(7.0));
    }

    /// producer/consumer は overflow カウンタを共有する（同じ Arc）。
    #[test]
    fn overflow_count_is_shared_between_ends() {
        let (mut p, c) = raw_ring(2);
        p.push_slice(&[1.0, 2.0, 3.0, 4.0]); // 2 書けて 2 ドロップ。
        assert_eq!(p.overflow_count(), 2);
        assert_eq!(
            c.overflow_count(),
            2,
            "consumer 側も同じ overflow を観測する"
        );
    }
}
