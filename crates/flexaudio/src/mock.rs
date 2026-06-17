//! ハード不要のテスト/検証用キャプチャバックエンド。
//!
//! [`MockBackend`] は実 OS デバイスの代わりにサイン波を生成し、概ねリアルタイムの
//! ペースで [`RawSink`] へ push する。これにより
//! [`Stream`](crate::Stream) を実機なしで end-to-end 駆動できる。

use std::f32::consts::PI;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use flexaudio_core::backend::{CaptureBackend, RawSink};
use flexaudio_core::types::Result;

/// 1 回の push でまとめて生成する時間長（ミリ秒）。
const BLOCK_MS: u32 = 10;

/// サイン波を生成してネイティブフォーマットで [`RawSink`] へ流す擬似バックエンド。
///
/// `native_format` は `new` で渡した `(sample_rate, channels)` をそのまま返す。
/// [`start`](Self::start) で生成スレッドを起動し、`BLOCK_MS`（既定 10ms）分の
/// interleaved `f32` サンプルを実時間ペースで push し続ける。[`stop`](Self::stop)
/// で生成スレッドを停止・join する。
///
/// ```no_run
/// use flexaudio::mock::MockBackend;
/// use flexaudio::Stream;
/// use flexaudio_core::types::StreamConfig;
///
/// // 44.1kHz / mono のマイクを模した擬似ソース。
/// let backend = Box::new(MockBackend::new(44_100, 1, 440.0));
/// let mut stream = Stream::open(StreamConfig::default(), backend).unwrap();
/// stream.start().unwrap();
/// // ... stream.poll_chunk() でチャンクを取り出す ...
/// stream.stop();
/// ```
pub struct MockBackend {
    sample_rate: u32,
    channels: u16,
    freq_hz: f32,
    /// 生成スレッドへの停止指示。
    running: Arc<AtomicBool>,
    /// 生成スレッドのハンドル（start 後に Some）。
    handle: Option<JoinHandle<()>>,
}

impl MockBackend {
    /// ネイティブ `(sample_rate, channels)` と生成するサイン波の周波数を指定して作る。
    ///
    /// 周波数 `0.0` 以下を渡した場合は実質無音（DC 0）を生成する。
    pub fn new(sample_rate: u32, channels: u16, freq_hz: f32) -> Self {
        Self {
            sample_rate: sample_rate.max(1),
            channels: channels.max(1),
            freq_hz,
            running: Arc::new(AtomicBool::new(false)),
            handle: None,
        }
    }
}

impl CaptureBackend for MockBackend {
    fn native_format(&self) -> (u32, u16) {
        (self.sample_rate, self.channels)
    }

    fn start(&mut self, mut sink: RawSink) -> Result<()> {
        // 既に動作中なら何もしない（二重 start に安全）。
        if self.running.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.running.store(true, Ordering::SeqCst);

        let running = self.running.clone();
        let sample_rate = self.sample_rate;
        let channels = self.channels as usize;
        let freq = self.freq_hz;

        let handle = thread::Builder::new()
            .name("flexaudio-mock-gen".into())
            .spawn(move || {
                let frames_per_block =
                    ((sample_rate as u64 * BLOCK_MS as u64) / 1000).max(1) as usize;
                let block_dur = Duration::from_millis(BLOCK_MS as u64);
                let two_pi_f_over_sr = 2.0 * PI * freq / sample_rate as f32;

                // 連続位相のためのグローバルフレーム索引（位相の不連続を避ける）。
                let mut phase_frame: u64 = 0;
                // device PTS（ns）— ネイティブ SR を基準にした単調なタイムスタンプ。
                let start = Instant::now();

                let mut scratch: Vec<f32> = Vec::with_capacity(frames_per_block * channels);

                while running.load(Ordering::SeqCst) {
                    scratch.clear();
                    for _ in 0..frames_per_block {
                        let s = if freq > 0.0 {
                            (two_pi_f_over_sr * phase_frame as f32).sin() * 0.5
                        } else {
                            0.0
                        };
                        // interleaved: 全チャンネルに同じサンプル（モノラル相当の中身）。
                        for _ in 0..channels {
                            scratch.push(s);
                        }
                        phase_frame = phase_frame.wrapping_add(1);
                    }

                    // この block 先頭フレームの device PTS（ns）= 経過時間ベース近似。
                    let pts_ns = start.elapsed().as_nanos() as i64;
                    sink.push(&scratch, pts_ns);

                    // 概ねリアルタイムのペースで眠る。
                    thread::sleep(block_dur);
                }
            })
            .map_err(|e| {
                flexaudio_core::types::Error::Backend(format!("spawn mock thread: {e}"))
            })?;

        self.handle = Some(handle);
        Ok(())
    }

    fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            // 生成スレッドの join。sleep 中でも次ループ頭で停止する。
            let _ = h.join();
        }
    }
}

impl Drop for MockBackend {
    fn drop(&mut self) {
        self.stop();
    }
}
