//! ハード不要のテスト/検証用キャプチャバックエンド。
//!
//! [`MockBackend`] は実 OS デバイスの代わりにサイン波を生成し、概ねリアルタイムの
//! ペースで [`RawSink`] へ push する。これにより
//! [`Stream`](crate::Stream) を実機なしで end-to-end 駆動できる。

use std::f32::consts::PI;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
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

/// 失速（stall）を起こせるテスト専用バックエンド。
///
/// 通常の [`MockBackend`] は正弦を途切れず push し続けるため、ウォッチドッグの
/// 失速検知 → 再オープン → [`ChunkFlags::RECOVERED`](flexaudio_core::types::ChunkFlags::RECOVERED)
/// 経路をテストできない。これは最初の `start()` セッションだけを `stall_after` 経過後に
/// 給餌停止（=stall）し、ウォッチドッグが `stop()`→`start()` で再オープンした 2 回目
/// 以降のセッションは正常給餌に戻る。stall → 自動復帰 → 復帰チャンクに RECOVERED を
/// 実機なしで再現できる。
///
/// `start()` 呼び出し回数を共有 [`AtomicU32`] で数え、世代 0（初回）でのみ stall する。
/// テスト用途のみ（公開 API ではない）。
#[doc(hidden)]
pub struct StallableMockBackend {
    sample_rate: u32,
    channels: u16,
    freq_hz: f32,
    /// 初回セッションで給餌を止めるまでの経過時間。
    stall_after: Duration,
    /// 生成スレッドへの停止指示。
    running: Arc<AtomicBool>,
    /// これまでの `start()` 呼び出し回数（=セッション世代）。共有して生成スレッドが読む。
    start_count: Arc<AtomicU32>,
    /// 生成スレッドのハンドル。
    handle: Option<JoinHandle<()>>,
}

impl StallableMockBackend {
    /// ネイティブ `(sample_rate, channels)` と周波数、初回セッションで失速するまでの
    /// 経過時間を指定して作る。
    pub fn new(sample_rate: u32, channels: u16, freq_hz: f32, stall_after: Duration) -> Self {
        Self {
            sample_rate: sample_rate.max(1),
            channels: channels.max(1),
            freq_hz,
            stall_after,
            running: Arc::new(AtomicBool::new(false)),
            start_count: Arc::new(AtomicU32::new(0)),
            handle: None,
        }
    }

    /// これまでの `start()` 呼び出し回数（=再オープン回数 + 1）。テストの観測用。
    pub fn start_count(&self) -> u32 {
        self.start_count.load(Ordering::SeqCst)
    }
}

impl CaptureBackend for StallableMockBackend {
    fn native_format(&self) -> (u32, u16) {
        (self.sample_rate, self.channels)
    }

    fn start(&mut self, mut sink: RawSink) -> Result<()> {
        if self.running.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.running.store(true, Ordering::SeqCst);

        // このセッションの世代（0 始まり）。世代 0 でのみ stall する。
        let generation = self.start_count.fetch_add(1, Ordering::SeqCst);

        let running = self.running.clone();
        let sample_rate = self.sample_rate;
        let channels = self.channels as usize;
        let freq = self.freq_hz;
        let stall_after = self.stall_after;

        let handle = thread::Builder::new()
            .name("flexaudio-stallable-mock-gen".into())
            .spawn(move || {
                let frames_per_block =
                    ((sample_rate as u64 * BLOCK_MS as u64) / 1000).max(1) as usize;
                let block_dur = Duration::from_millis(BLOCK_MS as u64);
                let two_pi_f_over_sr = 2.0 * PI * freq / sample_rate as f32;

                let mut phase_frame: u64 = 0;
                let session_start = Instant::now();
                let mut scratch: Vec<f32> = Vec::with_capacity(frames_per_block * channels);

                while running.load(Ordering::SeqCst) {
                    // 世代 0 かつ stall_after を超えたら給餌停止（=失速）。
                    // スレッドは生かしたまま push だけ止めるので、ウォッチドッグが
                    // last_sample_ns の停滞を STALL_THRESHOLD 後に検知する。
                    let stalled = generation == 0 && session_start.elapsed() >= stall_after;

                    if !stalled {
                        scratch.clear();
                        for _ in 0..frames_per_block {
                            let s = if freq > 0.0 {
                                (two_pi_f_over_sr * phase_frame as f32).sin() * 0.5
                            } else {
                                0.0
                            };
                            for _ in 0..channels {
                                scratch.push(s);
                            }
                            phase_frame = phase_frame.wrapping_add(1);
                        }
                        let pts_ns = session_start.elapsed().as_nanos() as i64;
                        sink.push(&scratch, pts_ns);
                    }

                    thread::sleep(block_dur);
                }
            })
            .map_err(|e| {
                flexaudio_core::types::Error::Backend(format!("spawn stallable mock thread: {e}"))
            })?;

        self.handle = Some(handle);
        Ok(())
    }

    fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for StallableMockBackend {
    fn drop(&mut self) {
        self.stop();
    }
}

/// `start()` または `stop()` で panic するテスト専用バックエンド。
///
/// OS バックエンドの start/stop は flexaudio から見れば外部コードで、契約違反として
/// panic しうる。その panic が `SharedState.backend`（start/stop を跨いでロックされる
/// `Mutex`）を poison させ、取り込み/ウォッチドッグスレッドが次にそのロックを取った
/// 瞬間に連鎖 panic で無言死する——その回帰を突くためのバックエンド。
/// [`Stream`](crate::Stream) はこの panic を
/// [`Error::Backend`](flexaudio_core::types::Error::Backend) /
/// [`Event::Error`](flexaudio_core::types::Event::Error) として表に出し、他スレッドを
/// 連鎖 panic させない（`stream.rs` の panic 回帰テスト参照）。
///
/// - [`PanicMode::Start`]: 最初の `start()` で panic する。
/// - [`PanicMode::Stop`]: `stop()` で panic する（start は成功し、給餌も行う）。
///
/// テスト用途のみ（公開 API ではない）。
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanicMode {
    /// `start()` 呼び出しで panic する。
    Start,
    /// `stop()` 呼び出しで panic する。
    Stop,
}

/// 指定タイミングで panic する擬似バックエンド（[`PanicMode`] 参照）。
///
/// `PanicMode::Stop` の場合は [`MockBackend`] 同様にサイン波を給餌してから、`stop()`
/// で panic する。`PanicMode::Start` の場合は給餌スレッドを立てる前に `start()` 内で
/// panic する。テスト用途のみ（公開 API ではない）。
#[doc(hidden)]
pub struct PanickingMockBackend {
    sample_rate: u32,
    channels: u16,
    freq_hz: f32,
    mode: PanicMode,
    /// 給餌スレッド（`PanicMode::Stop` で start に成功した場合のみ Some）。
    running: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl PanickingMockBackend {
    /// ネイティブ `(sample_rate, channels)`・周波数・panic タイミングを指定して作る。
    pub fn new(sample_rate: u32, channels: u16, freq_hz: f32, mode: PanicMode) -> Self {
        Self {
            sample_rate: sample_rate.max(1),
            channels: channels.max(1),
            freq_hz,
            mode,
            running: Arc::new(AtomicBool::new(false)),
            handle: None,
        }
    }
}

impl CaptureBackend for PanickingMockBackend {
    fn native_format(&self) -> (u32, u16) {
        (self.sample_rate, self.channels)
    }

    fn start(&mut self, mut sink: RawSink) -> Result<()> {
        if self.mode == PanicMode::Start {
            // start で panic させる（任意 backend の契約違反を模す）。
            panic!("PanickingMockBackend: intentional panic in start()");
        }

        // PanicMode::Stop: 通常どおり給餌スレッドを立てる（stop で panic する）。
        if self.running.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.running.store(true, Ordering::SeqCst);

        let running = self.running.clone();
        let sample_rate = self.sample_rate;
        let channels = self.channels as usize;
        let freq = self.freq_hz;

        let handle = thread::Builder::new()
            .name("flexaudio-panicking-mock-gen".into())
            .spawn(move || {
                let frames_per_block =
                    ((sample_rate as u64 * BLOCK_MS as u64) / 1000).max(1) as usize;
                let block_dur = Duration::from_millis(BLOCK_MS as u64);
                let two_pi_f_over_sr = 2.0 * PI * freq / sample_rate as f32;

                let mut phase_frame: u64 = 0;
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
                        for _ in 0..channels {
                            scratch.push(s);
                        }
                        phase_frame = phase_frame.wrapping_add(1);
                    }
                    let pts_ns = start.elapsed().as_nanos() as i64;
                    sink.push(&scratch, pts_ns);
                    thread::sleep(block_dur);
                }
            })
            .map_err(|e| {
                flexaudio_core::types::Error::Backend(format!("spawn panicking mock thread: {e}"))
            })?;

        self.handle = Some(handle);
        Ok(())
    }

    fn stop(&mut self) {
        // 給餌スレッドはまず確実に止めて join する（リーク・hang 防止）。
        self.running.store(false, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        if self.mode == PanicMode::Stop {
            // stop で panic させる（任意 backend の契約違反を模す）。
            panic!("PanickingMockBackend: intentional panic in stop()");
        }
    }
}

impl Drop for PanickingMockBackend {
    fn drop(&mut self) {
        // Drop からは panic させない（二重 panic→abort を避ける）。給餌スレッドだけ
        // 確実に止める。PanicMode::Stop の panic は明示 `stop()` 呼び出しでのみ起きる。
        self.running.store(false, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// 初回 `start()` は成功して給餌し、`stall_after` 経過で給餌停止（=失速）し、
/// ウォッチドッグによる再オープン（2 回目以降の `start()`）で panic するテスト
/// 専用バックエンド。
///
/// ウォッチドッグスレッド内で backend が panic しても、`SharedState.backend` mutex を
/// poison させて取り込み/ウォッチドッグスレッドを連鎖無言死させず、
/// [`Event::Error`](flexaudio_core::types::Event::Error)（"reopen failed: ..."）として
/// 表に出すことを検証する回帰用。[`StallableMockBackend`] の失速機構と
/// [`PanickingMockBackend`] の panic を組み合わせたもの。
///
/// テスト用途のみ（公開 API ではない）。
#[doc(hidden)]
pub struct StallThenPanicOnReopenBackend {
    sample_rate: u32,
    channels: u16,
    freq_hz: f32,
    stall_after: Duration,
    running: Arc<AtomicBool>,
    /// `start()` 呼び出し回数。世代 0（初回）でのみ成功・給餌し、世代 1 以降で panic。
    start_count: Arc<AtomicU32>,
    handle: Option<JoinHandle<()>>,
}

impl StallThenPanicOnReopenBackend {
    /// ネイティブ `(sample_rate, channels)`・周波数・初回失速までの経過時間を指定して作る。
    pub fn new(sample_rate: u32, channels: u16, freq_hz: f32, stall_after: Duration) -> Self {
        Self {
            sample_rate: sample_rate.max(1),
            channels: channels.max(1),
            freq_hz,
            stall_after,
            running: Arc::new(AtomicBool::new(false)),
            start_count: Arc::new(AtomicU32::new(0)),
            handle: None,
        }
    }
}

impl CaptureBackend for StallThenPanicOnReopenBackend {
    fn native_format(&self) -> (u32, u16) {
        (self.sample_rate, self.channels)
    }

    fn start(&mut self, mut sink: RawSink) -> Result<()> {
        if self.running.load(Ordering::SeqCst) {
            return Ok(());
        }
        // 世代を確定（0 始まり）。世代 1 以降＝ウォッチドッグ再オープンで panic する。
        let generation = self.start_count.fetch_add(1, Ordering::SeqCst);
        if generation >= 1 {
            // ウォッチドッグスレッドからの再オープン。ここで panic させる。
            panic!("StallThenPanicOnReopenBackend: intentional panic on reopen start()");
        }

        // 世代 0: 通常給餌（stall_after で給餌停止する＝失速）。
        self.running.store(true, Ordering::SeqCst);
        let running = self.running.clone();
        let sample_rate = self.sample_rate;
        let channels = self.channels as usize;
        let freq = self.freq_hz;
        let stall_after = self.stall_after;

        let handle = thread::Builder::new()
            .name("flexaudio-stall-then-panic-gen".into())
            .spawn(move || {
                let frames_per_block =
                    ((sample_rate as u64 * BLOCK_MS as u64) / 1000).max(1) as usize;
                let block_dur = Duration::from_millis(BLOCK_MS as u64);
                let two_pi_f_over_sr = 2.0 * PI * freq / sample_rate as f32;

                let mut phase_frame: u64 = 0;
                let session_start = Instant::now();
                let mut scratch: Vec<f32> = Vec::with_capacity(frames_per_block * channels);

                while running.load(Ordering::SeqCst) {
                    let stalled = session_start.elapsed() >= stall_after;
                    if !stalled {
                        scratch.clear();
                        for _ in 0..frames_per_block {
                            let s = if freq > 0.0 {
                                (two_pi_f_over_sr * phase_frame as f32).sin() * 0.5
                            } else {
                                0.0
                            };
                            for _ in 0..channels {
                                scratch.push(s);
                            }
                            phase_frame = phase_frame.wrapping_add(1);
                        }
                        let pts_ns = session_start.elapsed().as_nanos() as i64;
                        sink.push(&scratch, pts_ns);
                    }
                    thread::sleep(block_dur);
                }
            })
            .map_err(|e| {
                flexaudio_core::types::Error::Backend(format!(
                    "spawn stall-then-panic mock thread: {e}"
                ))
            })?;

        self.handle = Some(handle);
        Ok(())
    }

    fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for StallThenPanicOnReopenBackend {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}
