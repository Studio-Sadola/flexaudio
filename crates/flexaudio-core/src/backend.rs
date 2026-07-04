//! OS バックエンドが実装する [`CaptureBackend`] トレイトと、バックエンドが
//! 生フレームをコアへ渡すための [`RawSink`] ハンドル。
//!
//! 配線（backend → [`RawRing`](mod@crate::raw_ring) → [`Normalizer`](crate::normalizer)
//! → [`ChunkRing`](mod@crate::chunk_ring)）は facade 層が後で行う。ここではバックエンド
//! 契約の型のみを定義する。

use crate::raw_ring::RawProducer;
use crate::types::Result;

/// バックエンドが生 interleaved f32 フレームをコアへ渡すためのシンク。
///
/// 内部に [`RawProducer`] を持ち、[`push`](Self::push) は RT 安全な非ブロッキング
/// 書き込み（満杯時 DROP）を行う。SPSC の producer 側で、バックエンドの RT
/// コールバックスレッドからのみ `push` する想定。
pub struct RawSink {
    producer: RawProducer,
    native_rate: u32,
    native_channels: u16,
}

impl RawSink {
    /// バックエンドのネイティブフォーマットと共に生フレームシンクを作る。
    pub fn new(producer: RawProducer, native_rate: u32, native_channels: u16) -> Self {
        Self {
            producer,
            native_rate,
            native_channels,
        }
    }

    /// 生 interleaved f32 フレームを非ブロッキングに渡す。
    ///
    /// `pts_ns` はデバイス由来のプレゼンテーションタイムスタンプ。決してブロックせず、
    /// 満杯時は内部 overflow カウンタを増やしてドロップする。
    ///
    /// PTS の正規化と 20ms チャンク化は取り込みスレッドの責務で、ここでは生フレーム
    /// だけを渡す。生リングはサンプルしか運ばないため `pts_ns` は配線層が別途取り回す
    /// （将来フレームへ対応付ける用途で、用意できる backend は渡しておく）。
    ///
    /// `push` は backend の RT コールバックから呼ばれる想定。独自 backend では、push
    /// を呼ぶ経路でヒープ確保・ロック・ブロッキング・システムコールをしないこと。
    pub fn push(&mut self, interleaved: &[f32], pts_ns: i64) -> usize {
        let _ = pts_ns;
        self.producer.push_slice(interleaved)
    }

    /// バックエンドのネイティブサンプルレート（Hz）。
    pub fn native_rate(&self) -> u32 {
        self.native_rate
    }

    /// バックエンドのネイティブチャンネル数。
    pub fn native_channels(&self) -> u16 {
        self.native_channels
    }

    /// これまでに（満杯で）ドロップした累計サンプル数。
    pub fn overflow_count(&self) -> u64 {
        self.producer.overflow_count()
    }
}

/// OS 固有キャプチャバックエンドが実装するトレイト。
///
/// facade は [`native_format`](Self::native_format) でネイティブフォーマットを
/// 取得して [`Normalizer`](crate::normalizer) を構成し、[`start`](Self::start) に
/// [`RawSink`] を渡してキャプチャを開始する。バックエンドは自身の RT コールバック
/// 内で `sink.push(...)` を呼ぶ。
///
/// `Stream::open` に `Box<dyn CaptureBackend>` を渡せば独自 backend を差し込める。
///
/// 独自 backend を実装するときに守ること:
/// - キャプチャスレッド / RT コールバックで panic させない（panic するとキャプチャが
///   静かに止まりうる）。
/// - RT コールバックからの [`RawSink::push`] は RT 安全に呼ぶ（ヒープ確保・ロック・
///   ブロッキング・システムコールなし。詳細は [`RawSink::push`] を参照）。
/// - `start` / `stop` を冪等にする（動作中の二重 `start` は no-op で `Ok`、未起動の
///   `stop` も no-op）。
pub trait CaptureBackend: Send {
    /// バックエンドのネイティブフォーマット `(sample_rate, channels)`。
    fn native_format(&self) -> (u32, u16);

    /// 指定シンクへ生フレームを流し始める。
    fn start(&mut self, sink: RawSink) -> Result<()>;

    /// キャプチャを停止する。
    fn stop(&mut self);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raw_ring::raw_ring;

    /// テスト用の極小バックエンド。`start` で 1 ブロック push する。
    struct DummyBackend {
        sink: Option<RawSink>,
    }

    impl CaptureBackend for DummyBackend {
        fn native_format(&self) -> (u32, u16) {
            (44_100, 2)
        }
        fn start(&mut self, mut sink: RawSink) -> Result<()> {
            assert_eq!(sink.native_rate(), 44_100);
            assert_eq!(sink.native_channels(), 2);
            sink.push(&[0.1, 0.2, 0.3, 0.4], 0);
            self.sink = Some(sink);
            Ok(())
        }
        fn stop(&mut self) {
            self.sink = None;
        }
    }

    #[test]
    fn backend_pushes_into_raw_ring() {
        let (prod, mut cons) = raw_ring(16);
        let sink = RawSink::new(prod, 44_100, 2);
        let mut be = DummyBackend { sink: None };
        assert_eq!(be.native_format(), (44_100, 2));
        be.start(sink).unwrap();

        let mut out = [0.0f32; 4];
        let got = cons.pop_slice(&mut out);
        assert_eq!(got, 4);
        assert_eq!(out, [0.1, 0.2, 0.3, 0.4]);
        be.stop();
    }
}
