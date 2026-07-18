//! 1 ソースのキャプチャパイプラインを所有する [`Stream`]。
//!
//! コア部品（[`RawRing`](mod@flexaudio_core::raw_ring) / [`Normalizer`] /
//! [`ChunkRing`](mod@flexaudio_core::chunk_ring) / [`ClockNormalizer`] /
//! [`CaptureBackend`]）を配線し、プル型 API（[`poll_chunk`](Stream::poll_chunk) /
//! [`poll_event`](Stream::poll_event)）で消費側へ供給する。
//!
//! # スレッド構成
//! - backend の RT スレッド: [`RawSink`] 経由で生フレームを
//!   [`RawRing`](mod@flexaudio_core::raw_ring) へ push するだけ（非ブロッキング）。
//! - 取り込み/加工スレッド（1 本・通常優先度）: RawRing を pop → [`Normalizer`] で
//!   48k/stereo/20ms 化 → 単調増加 `seq` を付与 →
//!   [`ChunkRing`](mod@flexaudio_core::chunk_ring)（DROP_OLDEST）へ push。
//!   最後にサンプルを処理した時刻を `AtomicI64` で更新する。
//! - ウォッチドッグスレッド（1 本・~250ms tick）: 一定時間サンプル更新が止まったら
//!   無音死と判定し、backend を指数バックオフ（250ms→5s・ジッタ）で再オープンする。
//!   失速で [`Event::StreamStalled`]、復帰で [`Event::StreamRecovered`] を発火し、復帰後の
//!   最初のチャンクに [`ChunkFlags::RECOVERED`] | [`ChunkFlags::DISCONTINUITY`] を立てる。

use std::collections::VecDeque;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use flexaudio_core::backend::{CaptureBackend, RawSink};
use flexaudio_core::chunk_ring::{chunk_ring, ChunkConsumer, ChunkProducer};
use flexaudio_core::clock::{monotonic_now_ns, ClockNormalizer};
use flexaudio_core::normalizer::{InnerProcessor, Normalizer};
use flexaudio_core::raw_ring::{raw_ring, RawConsumer};
use flexaudio_core::secondary_ring::{
    secondary_chunk_ring, SecondaryChunkConsumer, SecondaryChunkProducer,
};
use flexaudio_core::types::{
    AudioChunk, ChunkFlags, Error, Event, OutputFormat, Result, SecondaryChunk, StreamConfig,
};

/// Wraps [`flexaudio_denoise::Denoiser`] as a core [`InnerProcessor`] so the
/// core stays independent of the concrete noise-suppression implementation.
///
/// The processor runs on the internal normalized form (48kHz / stereo), so the
/// denoiser is always constructed for two channels regardless of the primary
/// output channel count. `process`/`flush` forward to the denoiser (any error
/// is swallowed: the normalized form always has an even length, so
/// [`Denoiser::process`](flexaudio_denoise::Denoiser::process) never rejects it).
struct DenoiseInnerProcessor {
    denoiser: flexaudio_denoise::Denoiser,
}

impl DenoiseInnerProcessor {
    /// Build a fresh stereo denoiser. Called once per intake generation so a
    /// source switch / reopen starts with clean RNNoise state (no bleed across
    /// the discontinuity).
    fn new() -> Self {
        // Denoiser::new(2) only fails for an out-of-range channel count; 2 is
        // always valid, so unwrap is safe here.
        Self {
            denoiser: flexaudio_denoise::Denoiser::new(2)
                .expect("stereo denoiser construction is infallible"),
        }
    }
}

impl InnerProcessor for DenoiseInnerProcessor {
    fn process(&mut self, samples: &mut [f32]) {
        let _ = self.denoiser.process(samples);
    }
    fn flush(&mut self) -> Vec<f32> {
        self.denoiser.flush()
    }
}

/// Build the optional inner processor for a Normalizer, honoring the current
/// `denoise_enabled` flag. Returns `None` when denoise is off.
fn build_inner_processor(shared: &SharedState) -> Option<Box<dyn InnerProcessor>> {
    if shared.denoise_enabled.load(Ordering::SeqCst) {
        Some(Box::new(DenoiseInnerProcessor::new()))
    } else {
        None
    }
}

/// Build the [`Normalizer`] for the current generation: primary output, the
/// optional secondary tap, and the optional denoise inner processor. Kept in one
/// place so `start` and the intake generation-change path stay in sync.
fn build_normalizer(
    shared: &SharedState,
    rate: u32,
    channels: u16,
    output: OutputFormat,
    secondary_output: Option<OutputFormat>,
) -> Result<Normalizer> {
    let mut n = Normalizer::new(rate, channels, output)?;
    if let Some(sec) = secondary_output {
        n = n.with_secondary(sec)?;
    }
    if let Some(proc) = build_inner_processor(shared) {
        n = n.with_inner_processor(proc);
    }
    Ok(n)
}

/// RawRing の容量（f32 サンプル単位）。ネイティブ SR×ch に依存させず、
/// 多めに確保して RT 経路のドロップを避ける（約 0.5 秒 @ 48k stereo 相当の余裕）。
/// mix ソースの合成バックエンド（`mix.rs`）も子リングに同じ容量を使う。
pub(crate) const RAW_RING_SAMPLES: usize = 48_000;

/// ウォッチドッグの tick 間隔。
const WATCHDOG_TICK: Duration = Duration::from_millis(250);

/// この時間サンプル到着が途絶したら「無音死」と判定する既定閾値。
const STALL_THRESHOLD: Duration = Duration::from_secs(2);

/// 再オープン指数バックオフの下限。
const BACKOFF_MIN: Duration = Duration::from_millis(250);
/// 再オープン指数バックオフの上限。
const BACKOFF_MAX: Duration = Duration::from_secs(5);

/// 1 ソースのキャプチャパイプライン。
///
/// [`open`](Self::open) で構成し、[`start`](Self::start) でキャプチャを開始する。
/// 消費側は [`poll_chunk`](Self::poll_chunk) / [`poll_event`](Self::poll_event) を
/// 非ブロッキングに呼ぶ。[`stop`](Self::stop) は全スレッドを join する。
pub struct Stream {
    config: StreamConfig,

    /// backend を共有し、取り込みスレッド/ウォッチドッグスレッドが (再)オープンする。
    shared: Arc<SharedState>,

    /// 消費側が取り出すチャンクリングの consumer。
    chunk_consumer: ChunkConsumer,

    /// 副タップの consumer（`config.secondary_output` が `Some` のときのみ）。
    secondary_consumer: Option<SecondaryChunkConsumer>,

    /// イベントキューの consumer 側（共有）。
    events: Arc<Mutex<VecDeque<Event>>>,

    /// 取り込み/加工スレッド。
    worker: Option<JoinHandle<()>>,
    /// ウォッチドッグスレッド。
    watchdog: Option<JoinHandle<()>>,

    /// 開始済みか（二重 start 防止）。
    started: bool,
}

/// 取り込みスレッド・ウォッチドッグスレッド・main で共有する状態。
struct SharedState {
    /// backend 本体（再オープンのためロックで保護）。
    backend: Mutex<Box<dyn CaptureBackend>>,

    /// 現在有効な RawConsumer。再オープン時にウォッチドッグが差し替える。
    /// `None` の間（再オープン中）は取り込みスレッドは pop しない。
    raw_consumer: Mutex<Option<RawConsumer>>,

    /// `raw_consumer` の世代。再オープンのたびに増える。取り込みスレッドは
    /// 世代変化を検知して内部状態（Normalizer 等）をリセットする。
    raw_generation: AtomicU64,

    /// 最後にサンプルを処理（pop して Normalizer へ投入）した単調時刻（ns）。
    last_sample_ns: AtomicI64,

    /// 全スレッドへの停止指示。
    stopping: AtomicBool,

    /// 復帰直後フラグ。ウォッチドッグが復帰時に true にし、取り込みスレッドが
    /// 次チャンクへ RECOVERED|DISCONTINUITY を立てて false に戻す。
    recovered_pending: AtomicBool,

    /// イベントキュー（producer/consumer 共有）。
    events: Arc<Mutex<VecDeque<Event>>>,

    /// ChunkRing の producer（取り込みスレッドが使用）。
    chunk_producer: Mutex<Option<ChunkProducer>>,

    /// 現在の `backend` のネイティブフォーマット `(sample_rate, channels)`。
    ///
    /// ウォッチドッグ復帰（同一 backend 再オープン）では不変だが、
    /// [`Stream::switch_source`] でソースを差し替えると新 backend の値へ更新される
    /// （mic↔system/process でネイティブ SR/ch が変わるのが普通）。
    /// 取り込みスレッドは世代変化を検知してここを読み直し、第 1 段
    /// （native 依存）の [`Normalizer`] を作り直す。
    native_format: Mutex<(u32, u16)>,

    /// ソース切替中フラグ。[`Stream::switch_backend`] が切替中 true にする。
    /// 切替中はウォッチドッグが並行再オープンしないよう失速処理をスキップする
    /// （切替の旧 backend stop で一時的に idle になっても誤って再オープンしない）。
    switching: AtomicBool,

    /// 意図的な不連続フラグ。[`Stream::switch_backend`] がソース切替成功時に
    /// true にし、取り込みスレッドが次チャンクへ DISCONTINUITY（RECOVERED は
    /// 付けない＝自動復帰ではなく意図的切替）を立てて false に戻す。
    discontinuity_pending: AtomicBool,

    /// ポーズ中フラグ。pause() で true。取り込みスレッドは完成チャンクを破棄して
    /// 配信しない（RawRing の取り込みは続けるのでデバイスは止まらず、ウォッチドッグの
    /// 失速判定もぶれない）。resume() で false に戻し、次チャンクへ DISCONTINUITY を立てる。
    paused: AtomicBool,

    /// 入力ゲイン（線形倍率）の f32 ビット表現（`f32::to_bits`/`from_bits` で保持）。
    /// open() で config.gain から初期化し、set_gain() が録音中いつでも書き換える。
    /// 取り込みスレッドが完成チャンクごとに読み、1.0 以外なら各サンプルへ乗算する。
    gain_bits: AtomicU32,

    /// 録音エポック（ns）。「最初に配信された主チャンクの算出 pts」を 1 度だけ確定し、
    /// 以後全チャンク（主・副）から引いて録音開始 0 起点にする。番兵 `i64::MIN` = 未確定。
    /// reopen/switch をまたいでもリセットしない（録音全体で 1 本の時計）。
    recording_epoch_ns: AtomicI64,

    /// 内部正規形へのノイズ抑制を有効にするか。取り込みスレッドが Normalizer を（再）構築
    /// するときに読み、有効なら core の InnerProcessor へ denoise を注入する。
    denoise_enabled: AtomicBool,

    /// 副 ChunkRing の producer（副タップ設定時のみ Some）。取り込みスレッドが使う。
    secondary_producer: Mutex<Option<SecondaryChunkProducer>>,
}

impl SharedState {
    fn push_event(&self, ev: Event) {
        // poison でもイベントは torn しない（VecDeque を回収して継続）。
        let mut q = self.events.lock().unwrap_or_else(|e| e.into_inner());
        q.push_back(ev);
    }
}

/// backend の `start(sink)` を [`catch_unwind`](std::panic::catch_unwind) で包んで
/// 呼ぶ。backend が panic しても mutex を poison させる前に [`Error::Backend`] へ
/// 変換して返す（呼び出し側はこれを `Event::Error`/`Err` として表に出せる）。
///
/// `&mut Box<dyn CaptureBackend>` は `UnwindSafe` ではないため [`AssertUnwindSafe`]
/// で包む。安全なのは、panic を捕捉したらこの関数は `Err` を返すだけで、論理的に
/// 壊れたかもしれない backend を以降使い続けないため（呼び出し側は失敗として扱い、
/// stop/再オープン/drop へ進む）。ロックガードは正常に保持・drop され、poison しない。
fn start_backend_catching(be: &mut Box<dyn CaptureBackend>, sink: RawSink) -> Result<()> {
    match std::panic::catch_unwind(AssertUnwindSafe(|| be.start(sink))) {
        Ok(res) => res,
        Err(_) => Err(Error::Backend("backend panicked during start()".into())),
    }
}

/// backend の `stop()` を [`catch_unwind`](std::panic::catch_unwind) で包んで呼ぶ。
/// stop は `()` を返すため、panic は握りつぶして継続する（停止経路で再度 panic を
/// 伝播させても得は無く、mutex poison と連鎖 panic を防ぐのが目的）。`true` を返すと
/// 正常停止、`false` は panic を捕捉したこと（観測・診断用）を表す。
///
/// `AssertUnwindSafe` の安全性は [`start_backend_catching`] と同じ（捕捉後は backend を
/// 使い続けず、ガードは正常 drop される）。
#[must_use]
fn stop_backend_catching(be: &mut Box<dyn CaptureBackend>) -> bool {
    std::panic::catch_unwind(AssertUnwindSafe(|| be.stop())).is_ok()
}

impl Stream {
    /// 構成と backend からストリームを開く（まだキャプチャは始めない）。
    ///
    /// `config.chunk_ms` は固定契約上 20ms 前提。`ring_capacity_chunks` が
    /// チャンクリング容量になる。backend の [`native_format`](CaptureBackend::native_format)
    /// から [`Normalizer`] を構成する。
    pub fn open(config: StreamConfig, backend: Box<dyn CaptureBackend>) -> Result<Stream> {
        if config.ring_capacity_chunks == 0 {
            return Err(Error::InvalidArg("ring_capacity_chunks must be > 0".into()));
        }
        // 入力ゲインは有限かつ 0.0 以上（NaN・無限大・負は InvalidArg）。
        if !config.gain.is_finite() || config.gain < 0.0 {
            return Err(Error::InvalidArg(format!(
                "gain must be finite and >= 0.0, got {}",
                config.gain
            )));
        }
        // 出力フォーマットが対応域か検証（非対応は UnsupportedFormat）。
        config.output.validate()?;
        // 副出力フォーマットも同様に検証する（設定時のみ）。
        if let Some(sec) = config.secondary_output {
            sec.validate()?;
        }
        let native_format = backend.native_format();
        if native_format.0 == 0 || native_format.1 == 0 {
            return Err(Error::InvalidArg(
                "backend native_format must have non-zero rate and channels".into(),
            ));
        }

        let (chunk_producer, chunk_consumer) = chunk_ring(config.ring_capacity_chunks);
        // 副タップ設定時のみ専用リングを作る（公開 chunk_ring<AudioChunk> は不変）。
        let (secondary_producer, secondary_consumer) = if config.secondary_output.is_some() {
            let (p, c) = secondary_chunk_ring(config.ring_capacity_chunks);
            (Some(p), Some(c))
        } else {
            (None, None)
        };
        let events = Arc::new(Mutex::new(VecDeque::new()));

        let shared = Arc::new(SharedState {
            backend: Mutex::new(backend),
            raw_consumer: Mutex::new(None),
            raw_generation: AtomicU64::new(0),
            last_sample_ns: AtomicI64::new(0),
            stopping: AtomicBool::new(false),
            recovered_pending: AtomicBool::new(false),
            events: events.clone(),
            chunk_producer: Mutex::new(Some(chunk_producer)),
            native_format: Mutex::new(native_format),
            switching: AtomicBool::new(false),
            discontinuity_pending: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            gain_bits: AtomicU32::new(config.gain.to_bits()),
            recording_epoch_ns: AtomicI64::new(i64::MIN),
            denoise_enabled: AtomicBool::new(false),
            secondary_producer: Mutex::new(secondary_producer),
        });

        Ok(Stream {
            config,
            shared,
            chunk_consumer,
            secondary_consumer,
            events,
            worker: None,
            watchdog: None,
            started: false,
        })
    }

    /// キャプチャを開始する。
    ///
    /// RawRing を作って backend を起動し、取り込み/加工スレッドとウォッチドッグ
    /// スレッドを起動する。既に開始済みなら何もしない。
    pub fn start(&mut self) -> Result<()> {
        if self.started {
            return Ok(());
        }
        self.shared.stopping.store(false, Ordering::SeqCst);

        // 新しい start はポーズ状態を引き継がない（前回 pause したまま stop していても、
        // 再 start は通常状態から始める）。
        self.shared.paused.store(false, Ordering::SeqCst);

        // 新しい録音は 0 起点の時計を張り直す（前回録音のエポックを持ち越さない）。
        self.shared
            .recording_epoch_ns
            .store(i64::MIN, Ordering::SeqCst);

        // 初回 backend 起動: RawRing を作り sink を backend へ渡す。
        Self::open_backend_once(&self.shared)?;

        // 取り込み/加工スレッドへ移すため chunk_producer を取り出す。
        // poison でも回収して継続する（中の Option を take するだけ）。
        let chunk_producer = self
            .shared
            .chunk_producer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .ok_or_else(|| Error::InvalidState("chunk producer already taken".into()))?;

        // 副タップ設定時はその producer も取り出して取り込みスレッドへ移す。
        let secondary_producer = self
            .shared
            .secondary_producer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();

        // 取り込み/加工スレッド。初期 native_format は shared から読む
        // （以降は世代変化のたびに run_intake が shared を読み直して追従する）。
        let worker_shared = self.shared.clone();
        // poison でも回収して継続（中の (u32, u16) を読むだけ）。
        let initial_native = *self
            .shared
            .native_format
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let output = self.config.output;
        let secondary_output = self.config.secondary_output;
        let worker = thread::Builder::new()
            .name("flexaudio-intake".into())
            .spawn(move || {
                run_intake(
                    worker_shared,
                    chunk_producer,
                    secondary_producer,
                    initial_native,
                    output,
                    secondary_output,
                );
            })
            .map_err(|e| Error::Backend(format!("spawn intake thread: {e}")))?;
        self.worker = Some(worker);

        // ウォッチドッグスレッド。
        let wd_shared = self.shared.clone();
        let watchdog = thread::Builder::new()
            .name("flexaudio-watchdog".into())
            .spawn(move || {
                run_watchdog(wd_shared);
            })
            .map_err(|e| Error::Backend(format!("spawn watchdog thread: {e}")))?;
        self.watchdog = Some(watchdog);

        self.started = true;
        Ok(())
    }

    /// キャプチャを停止し、全スレッドを join する。
    ///
    /// 再入・二重 stop に安全。stop 後は [`poll_chunk`](Self::poll_chunk) で
    /// 既にリングへ溜まったチャンクを取り切れる。
    pub fn stop(&mut self) {
        // 停止フラグ → 全スレッドが次ループ頭で抜ける。
        self.shared.stopping.store(true, Ordering::SeqCst);

        // backend を止めて生成スレッドを終わらせる（RT push を止める）。
        // poison でも回収して stop を試みる。stop が panic しても catch_unwind で
        // 握りつぶし、mutex を再 poison させず join へ進む（無言死させない）。
        {
            let mut be = self
                .shared
                .backend
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let _ = stop_backend_catching(&mut be);
        }

        // スレッド join。
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
        if let Some(h) = self.watchdog.take() {
            let _ = h.join();
        }

        self.started = false;
    }

    /// キャプチャを一時停止する。
    ///
    /// OS 側のキャプチャは動かしたまま、完成チャンクの配信だけを止める。停止中は
    /// [`poll_chunk`](Self::poll_chunk) が新しいチャンクを返さない。内部では取り込みを続けて
    /// デバイスを生かしておくので、再開は素早く、ウォッチドッグの失速判定も誤発火しない。
    /// 既に停止中なら何もしない（多重呼び出し安全）。
    ///
    /// [`start`](Self::start) する前に呼んでもフラグは立つが、効くのは取り込みが回り始めてから。
    pub fn pause(&self) {
        self.shared.paused.store(true, Ordering::SeqCst);
    }

    /// [`pause`](Self::pause) を解除して配信を再開する。
    ///
    /// 再開後に最初に届くチャンクへ [`ChunkFlags::DISCONTINUITY`] を立てる（ポーズで音が時間的に
    /// 飛んだことを消費側へ伝える）。チャンクの `seq` はポーズ前後で連続し、ポーズ区間ぶんの
    /// 無音は挿入しない。停止していなければ何もしない（多重呼び出し安全）。
    pub fn resume(&self) {
        // 実際にポーズ中だったときだけ不連続を立てる（ポーズしていないのに resume を
        // 呼んでも余計な DISCONTINUITY を出さない）。
        if self.shared.paused.swap(false, Ordering::SeqCst) {
            self.shared
                .discontinuity_pending
                .store(true, Ordering::SeqCst);
        }
    }

    /// 現在ポーズ中かどうか。
    pub fn is_paused(&self) -> bool {
        self.shared.paused.load(Ordering::SeqCst)
    }

    /// 入力ゲイン（線形倍率）を変更する。1.0=そのまま、2.0=約+6dB、0.0=無音。
    ///
    /// 録音中いつでも呼べて、次の完成チャンクから効く（チャンクは 20ms 粒度）。
    /// 乗算後のサンプルは `-1.0..=1.0` にクランプされる。1.0 のときはサンプルに
    /// 一切触れない（バイト完全パススルー）。有限かつ 0.0 以上でなければ
    /// [`Error::InvalidArg`]（現在値は変わらない）。
    pub fn set_gain(&self, gain: f32) -> Result<()> {
        if !gain.is_finite() || gain < 0.0 {
            return Err(Error::InvalidArg(format!(
                "gain must be finite and >= 0.0, got {gain}"
            )));
        }
        self.shared
            .gain_bits
            .store(gain.to_bits(), Ordering::Relaxed);
        Ok(())
    }

    /// 現在の入力ゲイン（線形倍率）。
    pub fn gain(&self) -> f32 {
        f32::from_bits(self.shared.gain_bits.load(Ordering::Relaxed))
    }

    /// 完成済みチャンクを 1 つ取り出す（非ブロッキング）。無ければ `None`。
    ///
    /// 返るチャンクは出力フォーマット（`config.output`）の interleaved `f32`。
    /// チャンクは時間ベース 20ms 固定で `data.len() == frames * output.channels`。
    /// 既定 `{48000, 2}` なら `frames == 960`（`data.len() == 1920`）。
    /// `peak`/`rms` は最終 data に対して算出済み。`seq` は単調増加。
    pub fn poll_chunk(&mut self) -> Option<AudioChunk> {
        self.chunk_consumer.try_pop()
    }

    /// 完成済みの副タップチャンクを 1 つ取り出す（非ブロッキング）。無ければ `None`。
    ///
    /// 副タップは `config.secondary_output` が `Some` のときだけ生成される。未設定なら
    /// 常に `None`。副チャンクは主 [`AudioChunk`] と同じ録音時計（0 起点）に乗る `pts_ns`
    /// を持つが、値は主とは独立で、副 Stage2 のリサンプラ群遅延ぶん主より 20〜60ms 遅れる。
    /// 主↔副の対応は `pts_ns`（時刻）で取ること（`seq` は各タップ独立カウンタ）。
    pub fn poll_secondary(&mut self) -> Option<SecondaryChunk> {
        self.secondary_consumer.as_mut().and_then(|c| c.try_pop())
    }

    /// 内部正規形へのノイズ抑制（RNNoise）を有効/無効にする。
    ///
    /// [`start`](Self::start) の前に呼ぶこと（取り込みスレッドが Normalizer を構築する
    /// ときに反映される。録音中の変更は次の世代交代＝ソース切替/自動復帰で反映される）。
    /// 有効時は 48kHz/stereo の内部正規形へ 1 度だけ適用され、主・副の両タップが除去済み
    /// 音声を受ける（+10ms の固定遅延）。core は denoise 非依存で、この facade が実装を
    /// 差し込む。
    pub fn set_denoise(&self, enabled: bool) {
        self.shared.denoise_enabled.store(enabled, Ordering::SeqCst);
    }

    /// 未配信イベントを 1 つ取り出す（非ブロッキング）。無ければ `None`。
    pub fn poll_event(&mut self) -> Option<Event> {
        self.events.lock().ok().and_then(|mut q| q.pop_front())
    }

    /// これまでにチャンクリングが DROP_OLDEST で捨てた累計チャンク数。
    pub fn dropped_chunks(&self) -> u64 {
        self.chunk_consumer.dropped_count()
    }

    /// 現在の構成への参照。
    pub fn config(&self) -> &StreamConfig {
        &self.config
    }

    /// 現在の backend のネイティブフォーマット `(sample_rate, channels)`。
    ///
    /// open 時に backend から取得した値。ウォッチドッグ復帰では不変だが、
    /// [`switch_source`](Self::switch_source) でソースを切り替えると新 backend の
    /// 値に更新される。表示・診断用（出力フォーマットは `config().output`）。
    pub fn native_format(&self) -> (u32, u16) {
        // poison でも回収して値を読む（連鎖 panic させない）。
        *self
            .shared
            .native_format
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    // --- 内部 ---

    /// 現 `shared.backend` を（再）start し、新しい RawRing/RawConsumer を共有へ
    /// 載せて世代を進める。初回起動・ウォッチドッグ再オープンの双方で使う。
    ///
    /// 手順:
    /// 1. 現 backend の [`native_format`](CaptureBackend::native_format) を取得し
    ///    `shared.native_format` を更新（同一 backend の再オープンでは不変、
    ///    将来ここを別 backend で呼んでも追従する）。
    /// 2. その rate/ch で新しい RawRing を作る（旧 RawRing の format 残骸を持ち込ま
    ///    ない＝位相破壊を避ける）。
    /// 3. backend を start。
    /// 4. 新 RawConsumer を共有へ載せ替え（旧 consumer は drop）、世代を ++。
    /// 5. `last_sample_ns` を now にして即失速判定を避ける。
    ///
    /// backend ロックは start 時のみ取る（呼び出し側がロックを保持していない
    /// 前提）。低レベルな切替（[`switch_backend`](Self::switch_backend)）は
    /// backend を直接差し替えるため本関数を経由しない（旧ソース復帰の局面でのみ
    /// 本関数を再利用する）。
    fn open_backend_once(shared: &Arc<SharedState>) -> Result<()> {
        // 現 backend のネイティブフォーマットを取得して shared へ反映する。
        // poison でも回収して継続（backend ロックは start を跨ぐため poison しうる）。
        let (rate, channels) = {
            let be = shared.backend.lock().unwrap_or_else(|e| e.into_inner());
            be.native_format()
        };
        {
            let mut nf = shared
                .native_format
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            *nf = (rate, channels);
        }

        // 新しい RawRing（旧 format の残骸を持ち込まない）。
        let (producer, consumer) = raw_ring(RAW_RING_SAMPLES);
        let sink = RawSink::new(producer, rate, channels);

        {
            // poison でも回収。backend が start() で panic しても catch_unwind が
            // mutex poison 前に Error::Backend へ変換して返すので、ここで `?` により
            // 呼び出し側（start()=呼び元へ Err / watchdog=Event::Error）へ伝わる。
            let mut be = shared.backend.lock().unwrap_or_else(|e| e.into_inner());
            start_backend_catching(&mut be, sink)?;
        }

        // 新しい consumer を共有へ載せ、世代を進める（旧 consumer は drop）。
        {
            // poison でも回収して載せ替える（中の Option を差し替えるだけ）。
            let mut rc = shared
                .raw_consumer
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            *rc = Some(consumer);
        }
        shared.raw_generation.fetch_add(1, Ordering::SeqCst);

        // 起動直後を「最後に到着した時刻」として扱い、即失速判定を避ける。
        shared
            .last_sample_ns
            .store(monotonic_now_ns(), Ordering::SeqCst);
        Ok(())
    }

    /// 低レベルなソース切替。現在の backend を新しい backend へ差し替え、チャンク
    /// ストリーム（seq・PTS）の連続性を保ったまま入力ソースを変える。
    ///
    /// `seq` は取り込みスレッドのローカル変数で backend にも `SharedState` にも無いので、
    /// ここで触らなければ差し替え前後で連続する。PTS は取り込みスレッドが世代変化を
    /// 検知して `Normalizer`/`ClockNormalizer` を作り直し、新ソース初回サンプルの実時刻で
    /// 再アンカーするので単調を保つ。
    ///
    /// 手順（generation++ は最後に 1 回だけ・全 Atomic は SeqCst）:
    /// - 未 start なら [`Error::InvalidState`]。
    /// - `switching = true`（ウォッチドッグの並行再オープンを止める）。
    /// - backend ロック下で旧 backend を `stop()` → 新 backend の native を取得して
    ///   `shared.native_format` 更新 → 新 RawRing → `new_backend.start(sink)`。
    ///   - 成功: backend を新へ差し替え、新 consumer を載せ替え（旧 drop）。
    ///   - 失敗: 旧 backend を [`open_backend_once`](Self::open_backend_once) で
    ///     再 start して旧ソースを継続（連続性を壊さない）。`discontinuity_pending`
    ///     を立て世代を ++、`switching=false` にして `Err` を返す。
    /// - 成功時: `discontinuity_pending = true`（意図的切替なので RECOVERED は付けない）
    ///   → `generation += 1`（最後に 1 回だけ）→ `last_sample_ns = now` →
    ///   `switching = false` → `Ok`。
    ///
    /// [`Box<dyn CaptureBackend>`] を直接受け取るので、mock backend で切替挙動を検証できる。
    /// 高レベル入口は [`switch_source`](Self::switch_source)。
    ///
    /// `#[doc(hidden)] pub`: 公開 API ではない（ドキュメントに出さない）が、別クレートの
    /// 統合テスト（`tests/integration.rs`）から MockBackend を渡して呼べるようにする。
    #[doc(hidden)]
    pub fn switch_backend(&mut self, new_backend: Box<dyn CaptureBackend>) -> Result<()> {
        if !self.started {
            return Err(Error::InvalidState(
                "switch_backend は start 済みのストリームでのみ可能".into(),
            ));
        }

        // 切替開始: ウォッチドッグの失速→再オープンと衝突しないよう先に止める。
        self.shared.switching.store(true, Ordering::SeqCst);

        // backend ロック下で旧 stop → 新 start を一気に行う。
        // poison でも回収して継続する（backend ロックは stop/start を跨ぐため poison
        // しうる。回収できれば差し替え処理はそのまま正しく行える）。
        {
            let mut be = self
                .shared
                .backend
                .lock()
                .unwrap_or_else(|e| e.into_inner());

            // 旧 backend を止める（RT push を止める）。panic しても catch_unwind で
            // 握りつぶし、mutex poison・連鎖 panic を避けて切替を続行する。
            let _ = stop_backend_catching(&mut be);

            // 新 backend のネイティブフォーマット。
            let (rate, channels) = new_backend.native_format();

            // 新 RawRing（旧 format 残骸を持ち込まない）。
            let (producer, consumer) = raw_ring(RAW_RING_SAMPLES);
            let sink = RawSink::new(producer, rate, channels);

            // 新 backend を start。panic は catch_unwind が Error::Backend へ変換する
            // ので、下の Err 分岐（旧ソース復帰 → Err 返却）に乗る。失敗時は旧ソースへ復帰。
            let mut new_backend = new_backend;
            match start_backend_catching(&mut new_backend, sink) {
                Ok(()) => {
                    // 順序が効く。取り込みスレッドは世代をロック外で load してから
                    // raw_consumer を lock して pop する。新 consumer を先に載せると、
                    // 世代を ++ する前に新ソースの native サンプルが旧 normalizer へ流れ込み、
                    // 位相が壊れる。そこで native_format 更新 → 世代 ++（+ DISCONTINUITY 等）
                    // → 最後に consumer/backend を差し替える順にする。こうすれば取り込み側が
                    // 新 consumer を観測する時には必ず新世代が見え、normalizer を作り直して
                    // から pop する。
                    //
                    // shared.native_format を新ソースの値へ更新。
                    {
                        // poison でも回収（中の (u32, u16) を更新するだけ）。
                        let mut nf = self
                            .shared
                            .native_format
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        *nf = (rate, channels);
                    }
                    // 意図的切替なので RECOVERED は付けず DISCONTINUITY のみ。
                    self.shared
                        .discontinuity_pending
                        .store(true, Ordering::SeqCst);
                    // 起動直後を最終到着時刻に（即失速判定を避ける）。
                    self.shared
                        .last_sample_ns
                        .store(monotonic_now_ns(), Ordering::SeqCst);
                    // 世代を進める（最後に 1 回だけ）。consumer 差し替えより前に行い、
                    // 新 consumer 観測時には必ず新世代が見えるようにする。
                    self.shared.raw_generation.fetch_add(1, Ordering::SeqCst);

                    // backend を新へ差し替え（旧 backend は drop）。
                    *be = new_backend;
                    // 新 consumer を共有へ載せ替え（旧 consumer は drop）。最後に行う。
                    {
                        // poison でも回収（Option を差し替えるだけ）。
                        let mut rc = self
                            .shared
                            .raw_consumer
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        *rc = Some(consumer);
                    }
                }
                Err(e) => {
                    // 新ソース起動失敗 → 旧 backend（`*be` のまま）を再 start して継続。
                    // backend ロックを保持したままだと open_backend_once が再ロックで
                    // デッドロックするため、ここで一旦解放してから復帰させる。
                    drop(be);
                    // 旧 backend を再オープン（native_format は旧 backend の値へ戻る）。
                    let _ = Self::open_backend_once(&self.shared);
                    // 旧ソース再開も「不連続」扱いにする（一瞬途切れたため）。
                    self.shared
                        .discontinuity_pending
                        .store(true, Ordering::SeqCst);
                    // open_backend_once が generation を ++ 済み。switching を戻して Err。
                    self.shared.switching.store(false, Ordering::SeqCst);
                    return Err(e);
                }
            }
        }

        // --- 切替成功 ---
        // generation++・native_format 更新・各フラグは backend ロック下で実施済み
        // （新 consumer 観測前に新世代が見えるよう順序付け）。ここでは switching を
        // 戻すだけ。
        self.shared.switching.store(false, Ordering::SeqCst);
        Ok(())
    }

    /// 録音を止めずに入力ソース（mic/system/process）を切り替える高レベル入口。
    ///
    /// `new_config` からソース別バックエンドを `build_backend`（facade 内 private）で
    /// 構築し（失敗時は旧ソース無傷のまま `Err`）、[`switch_backend`](Self::switch_backend)
    /// で差し替える。出力フォーマット（`output`）は切り替えられない（チャンクの
    /// frames/data.len が変わると連続ストリームが壊れるため）。変更要求は
    /// [`Error::InvalidArg`] で弾く。
    ///
    /// 成功時、`config` の可変項目（`kind` / `device_id` / `target_pid` / `mode`
    /// / `exclude_self`）だけを新しい値へ更新する。`output` / `chunk_ms`
    /// / `ring_capacity_chunks` は据え置く。`new_config.gain` も無視する（ゲインは
    /// ストリームの状態であり、切替では変わらない。変更は [`set_gain`](Self::set_gain)）。
    ///
    /// # エラー
    /// - 未 start → [`Error::InvalidState`]。
    /// - `output` 変更要求 → [`Error::InvalidArg`]。
    /// - 新ソースの backend 構築失敗（process の PID 欠落・非対応 OS 等）→
    ///   `build_backend`（facade 内 private）由来のエラー（旧ソースは無傷）。
    /// - 新 backend の start 失敗 → [`switch_backend`](Self::switch_backend) が
    ///   旧ソースへ復帰したうえで当該エラーを返す。
    pub fn switch_source(&mut self, new_config: StreamConfig) -> Result<()> {
        if !self.started {
            return Err(Error::InvalidState(
                "switch_source は start 済みのストリームでのみ可能".into(),
            ));
        }
        if new_config.output != self.config.output {
            return Err(Error::InvalidArg(
                "output format cannot change during switch_source".into(),
            ));
        }
        // 副タップのフォーマットも open 時固定（切替中に副 Normalizer/リングを再構成しない）。
        if new_config.secondary_output != self.config.secondary_output {
            return Err(Error::InvalidArg(
                "secondary output format cannot change during switch_source".into(),
            ));
        }
        // 新ソースの backend を構築（失敗時は旧ソース無傷のまま早期 return）。
        let backend = crate::build_backend(&new_config)?;
        // 差し替え（連続性は switch_backend が保証）。
        self.switch_backend(backend)?;
        // 成功時のみ config の可変項目を更新（output 等は据え置き）。
        self.config = StreamConfig {
            kind: new_config.kind,
            device_id: new_config.device_id,
            target_pid: new_config.target_pid,
            mode: new_config.mode,
            exclude_self: new_config.exclude_self,
            ..self.config.clone()
        };
        Ok(())
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        if self.started {
            self.stop();
        }
    }
}

/// 録音開始 0 起点の絶対時刻へ写す。最初に配信される（主）チャンクの算出 pts を録音
/// エポックとして 1 度だけ確定し（番兵 `i64::MIN` から書き換え）、以後全チャンクから引く。
/// 取り込みスレッドだけが書くので競合しない。以後 reopen/switch をまたいでも固定なので、
/// pts は 0 起点で録音全体を通して連続する。
fn apply_epoch(shared: &SharedState, raw_pts: i64) -> i64 {
    let epoch = shared.recording_epoch_ns.load(Ordering::SeqCst);
    if epoch == i64::MIN {
        shared.recording_epoch_ns.store(raw_pts, Ordering::SeqCst);
        0
    } else {
        raw_pts - epoch
    }
}

/// 完成チャンクの `data` へ入力ゲイン（線形倍率）を適用する。1.0 のときはサンプルに一切
/// 触れない（バイト完全パススルー）。1.0 以外なら各サンプルへ乗算し ±1.0 にクランプする。
fn apply_gain(data: &mut [f32], gain: f32) {
    if gain != 1.0 {
        for x in data.iter_mut() {
            *x = (*x * gain).clamp(-1.0, 1.0);
        }
    }
}

/// 取り込み/加工スレッド本体。
///
/// RawConsumer を pop → [`Normalizer`]（Stage1 共有 → 主/副の各 Stage2 へ分岐）へ投入 →
/// 完成チャンクへ `seq`・録音 0 起点 pts・peak/rms・不連続フラグを付与 → 主/副リングへ
/// push。世代変化（再オープン / ソース切替）を検知したら Normalizer/Clock を作り直し、
/// 次チャンクへ RECOVERED|DISCONTINUITY を立てる。RawRing オーバーフロー（キャプチャ側の
/// 欠落）も検知し、次の主・副チャンク両方へ DISCONTINUITY を立てる。停止時は Normalizer を
/// flush して末尾テールを吐き切る。
fn run_intake(
    shared: Arc<SharedState>,
    mut chunk_producer: ChunkProducer,
    mut secondary_producer: Option<SecondaryChunkProducer>,
    initial_native: (u32, u16),
    output: OutputFormat,
    secondary_output: Option<OutputFormat>,
) {
    let (mut rate, mut channels) = initial_native;
    // Normalizer 構築失敗（rubato 構築失敗等）は無言で死なせず Event::Error を出して終了。
    let mut normalizer = match build_normalizer(&shared, rate, channels, output, secondary_output) {
        Ok(n) => n,
        Err(e) => {
            shared.push_event(Event::Error(format!("normalizer init failed: {e}")));
            return;
        }
    };
    let mut clock = ClockNormalizer::new();
    let mut seq: u64 = 0; // 主タップ seq。
    let mut sec_seq: u64 = 0; // 副タップ seq（主とは別カウンタ）。
    let mut current_generation = shared.raw_generation.load(Ordering::SeqCst);
    // RawRing オーバーフロー累計の前回観測値（世代交代でリングが作り直され 0 に戻る）。
    let mut overflow_baseline: u64 = 0;

    // 不連続 pending をタップ毎に持つ（共有 AtomicBool を両タップのローカルへ扇状に配る）。
    // rec_* は RECOVERED|DISCONTINUITY、disc_* は DISCONTINUITY のみ。持ち越すことで
    // ポーズ中に破棄しても resume 後の最初のチャンクへ確実に載る。
    let mut rec_primary = false;
    let mut rec_secondary = false;
    let mut disc_primary = false;
    let mut disc_secondary = false;

    // タップ毎の最後に配信した pts。契約（録音 0 起点・非減少）を保証するため、各
    // チャンクの pts をこの値と 0 でクランプする。起動直後は各タップの PTS 外挿がわずかに
    // 負へ振れうる（主エポック基準・数 ms）ので 0 で下限を切る。世代交代をまたぐ後退も
    // ここで吸収する。
    let mut last_pts_primary: i64 = 0;
    let mut last_pts_secondary: i64 = 0;

    let out_channels = output.channels.max(1) as usize;
    let sec_channels = secondary_output
        .map(|f| f.channels.max(1) as usize)
        .unwrap_or(1);

    // pop 用スクラッチ（RawRing 容量ぶん確保。1 回で全量取り出せる）。
    let mut scratch = vec![0.0f32; RAW_RING_SAMPLES];

    loop {
        let stopping = shared.stopping.load(Ordering::SeqCst);

        // 世代変化（再オープン / ソース切替）検知 → 新しいソースへリセット。native_format が
        // 変わり得るので shared を読み直し、第 1 段（native 依存）の Normalizer を作り直す。
        // 録音エポックはここでリセットしない（0 起点で世代跨ぎ連続）。
        let gen = shared.raw_generation.load(Ordering::SeqCst);
        if gen != current_generation {
            current_generation = gen;
            let nf = *shared
                .native_format
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            rate = nf.0;
            channels = nf.1;
            normalizer = match build_normalizer(&shared, rate, channels, output, secondary_output) {
                Ok(n) => n,
                Err(e) => {
                    shared.push_event(Event::Error(format!(
                        "normalizer rebuild failed after source change: {e}"
                    )));
                    return;
                }
            };
            clock = ClockNormalizer::new();
            // 新しい RawConsumer は overflow を 0 起点から数える（偽の巨大差分を避ける）。
            overflow_baseline = 0;
        }

        // 共有 pending をタップ毎ローカルへ配る。切替中は switching でウォッチドッグを止める
        // ので両方同時には立たないが、立っても OR で合成される。
        if shared.recovered_pending.swap(false, Ordering::SeqCst) {
            rec_primary = true;
            rec_secondary = true;
        }
        if shared.discontinuity_pending.swap(false, Ordering::SeqCst) {
            disc_primary = true;
            disc_secondary = true;
        }

        // RawRing から取り出して Normalizer へ。overflow（キャプチャ側の欠落）も観測する。
        let mut produced_any = false;
        let mut push_err: Option<Error> = None;
        let mut overflow_now = overflow_baseline;
        {
            let mut rc_guard = shared
                .raw_consumer
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(rc) = rc_guard.as_mut() {
                let got = rc.pop_slice(&mut scratch);
                overflow_now = rc.overflow_count();
                if got > 0 {
                    let samples = &scratch[..got];
                    // device PTS: ネイティブ SR を基準にした単調近似（到着時刻）。
                    let device_pts = monotonic_now_ns();
                    let norm_pts = clock.normalize(device_pts);
                    if let Err(e) = normalizer.push(samples, norm_pts) {
                        push_err = Some(e);
                    } else {
                        shared
                            .last_sample_ns
                            .store(monotonic_now_ns(), Ordering::SeqCst);
                        produced_any = true;
                    }
                }
            }
        }

        // push が失敗していたら Event::Error を出して取り込みを終了（無言死しない）。
        if let Some(e) = push_err {
            shared.push_event(Event::Error(format!("normalizer push failed: {e}")));
            return;
        }

        // RawRing オーバーフロー（RT が intake を追い越しサンプルを捨てた）を検知したら、
        // 次の主・副チャンク両方へ DISCONTINUITY を立てる（正規形以前の欠落＝両タップが
        // 等しく影響を受ける）。pts は wall-clock 再アンカーで既にギャップを反映する。
        if overflow_now > overflow_baseline {
            disc_primary = true;
            disc_secondary = true;
        }
        overflow_baseline = overflow_now;

        // 停止時は末尾テール（denoise 遅延線 + リサンプラ残余）を flush して吐き切る。
        if stopping {
            normalizer.flush();
        }

        // --- 主タップ: 完成チャンクを全て取り出して ChunkRing へ。---
        let paused = shared.paused.load(Ordering::SeqCst);
        let gain = f32::from_bits(shared.gain_bits.load(Ordering::Relaxed));
        let mut emitted_any = false;
        while let Some((mut data, raw_pts)) = normalizer.pop_chunk() {
            // ポーズ中は破棄する（pop で out_frame_origin は進むので pts は前進を保つ）。
            // pending は消費せず持ち越し、resume 後の最初のチャンクへ載せる。
            if paused {
                continue;
            }
            // 0 起点化 → 非負・非減少へクランプ（契約: 録音 0 起点・非減少）。
            let pts_ns = apply_epoch(&shared, raw_pts).max(last_pts_primary);
            last_pts_primary = pts_ns;
            debug_assert_eq!(data.len() % out_channels, 0);
            let frames = data.len() / out_channels;
            apply_gain(&mut data, gain);
            let (peak, rms) = peak_rms(&data);

            let mut flags = ChunkFlags::empty();
            if rec_primary {
                flags |= ChunkFlags::RECOVERED | ChunkFlags::DISCONTINUITY;
                rec_primary = false;
            }
            if disc_primary {
                flags |= ChunkFlags::DISCONTINUITY;
                disc_primary = false;
            }

            let chunk = AudioChunk {
                data,
                frames,
                pts_ns,
                seq,
                flags,
                dropped_before: 0, // ChunkRing が push 時に上書きする。
                peak,
                rms,
            };
            seq += 1;

            if let Some(total) = chunk_producer.push(chunk) {
                shared.push_event(Event::ChunkDropped { count: total });
            }
            emitted_any = true;
        }

        // --- 副タップ: 設定時のみ。主と対称に pop・破棄・flag 付与する。---
        if let Some(sec_prod) = secondary_producer.as_mut() {
            while let Some((mut samples, raw_pts)) = normalizer.pop_secondary() {
                // ポーズ中は副も pop して破棄する（out_buf の無限成長を防ぐ）。
                if paused {
                    continue;
                }
                // 0 起点化 → 非負・非減少へクランプ（契約: 録音 0 起点・非減少）。
                let pts_ns = apply_epoch(&shared, raw_pts).max(last_pts_secondary);
                last_pts_secondary = pts_ns;
                debug_assert_eq!(samples.len() % sec_channels, 0);
                let frames = samples.len() / sec_channels;
                apply_gain(&mut samples, gain);
                let (peak, rms) = peak_rms(&samples);

                let mut flags = ChunkFlags::empty();
                if rec_secondary {
                    flags |= ChunkFlags::RECOVERED | ChunkFlags::DISCONTINUITY;
                    rec_secondary = false;
                }
                if disc_secondary {
                    flags |= ChunkFlags::DISCONTINUITY;
                    disc_secondary = false;
                }

                let chunk = SecondaryChunk {
                    samples,
                    frames,
                    pts_ns,
                    seq: sec_seq,
                    flags,
                    dropped_before: 0, // 副リングが push 時に上書きする。
                    peak,
                    rms,
                };
                sec_seq += 1;
                // 副タップのドロップは dropped_before で観測できる（専用イベントは出さない）。
                let _ = sec_prod.push(chunk);
                emitted_any = true;
            }
        }

        // 停止指示なら末尾を吐き切ったので終了。
        if stopping {
            break;
        }

        // データが無ければ少し眠って CPU を空転させない。
        if !produced_any && !emitted_any {
            thread::sleep(Duration::from_millis(2));
        }
    }
}

/// ウォッチドッグスレッド本体。
///
/// ~250ms tick で最終サンプル到着時刻を監視し、[`STALL_THRESHOLD`] を超えて
/// 途絶したら backend を指数バックオフで再オープンする。失速で
/// [`Event::StreamStalled`]、復帰で [`Event::StreamRecovered`] を発火する。
fn run_watchdog(shared: Arc<SharedState>) {
    let mut stalled = false;
    let mut backoff = BACKOFF_MIN;

    loop {
        if shared.stopping.load(Ordering::SeqCst) {
            break;
        }
        thread::sleep(WATCHDOG_TICK);
        if shared.stopping.load(Ordering::SeqCst) {
            break;
        }

        // ソース切替中は失速判定・再オープンをしない（switch_backend が旧 backend を
        // 一時的に stop して idle になるため、誤って並行再オープンするのを防ぐ）。
        // 切替は last_sample_ns を now に更新して終わるので、次 tick から通常監視へ戻る。
        if shared.switching.load(Ordering::SeqCst) {
            continue;
        }

        let now = monotonic_now_ns();
        let last = shared.last_sample_ns.load(Ordering::SeqCst);
        let idle_ns = now.saturating_sub(last);
        let idle = Duration::from_nanos(idle_ns.max(0) as u64);

        if !stalled {
            if idle >= STALL_THRESHOLD {
                // 失速判定。
                stalled = true;
                backoff = BACKOFF_MIN;
                shared.push_event(Event::StreamStalled);
            }
            continue;
        }

        // 失速中: backend を止めて再オープンを試みる。
        // poison でも回収して stop を試み、stop が panic しても catch_unwind で
        // 握りつぶす（ウォッチドッグを無言死させず再オープンへ進む）。
        {
            let mut be = shared.backend.lock().unwrap_or_else(|e| e.into_inner());
            let _ = stop_backend_catching(&mut be);
        }

        if shared.stopping.load(Ordering::SeqCst) {
            break;
        }

        let reopened = match Stream::open_backend_once(&shared) {
            Ok(()) => true,
            Err(e) => {
                shared.push_event(Event::Error(format!("reopen failed: {e}")));
                false
            }
        };

        if reopened {
            // open_backend_once が last_sample_ns を now に更新・世代を ++ 済み。
            // 復帰後の最初のチャンクへ RECOVERED|DISCONTINUITY を立てるよう
            // recovered_pending を倒す（取り込みスレッドが次チャンクで消費する）。
            // 復帰が本物かは次の tick で idle を見て確認する。
            shared.recovered_pending.store(true, Ordering::SeqCst);
            stalled = false;
            shared.push_event(Event::StreamRecovered);
            backoff = BACKOFF_MIN;
        } else {
            // 失敗 → 指数バックオフ（ジッタ付き）で待ってから再試行。
            let jittered = jittered_backoff(backoff);
            sleep_interruptible(&shared, jittered);
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }
    }
}

/// 出力フォーマットの最終 interleaved `data` から peak（全サンプル絶対値の最大）と
/// rms（二乗平均平方根・線形）を求める。
///
/// 20ms チャンク（最大 1920 サンプル）に対する 1 走査なので極小コスト。空 data は
/// `(0.0, 0.0)`。
fn peak_rms(data: &[f32]) -> (f32, f32) {
    if data.is_empty() {
        return (0.0, 0.0);
    }
    let mut peak = 0.0f32;
    let mut sum_sq = 0.0f64;
    for &x in data {
        let a = x.abs();
        if a > peak {
            peak = a;
        }
        sum_sq += (x as f64) * (x as f64);
    }
    let rms = (sum_sq / data.len() as f64).sqrt() as f32;
    (peak, rms)
}

/// バックオフへ時刻ベースの軽いジッタ（±約 12.5%）を加える（`rand` 不使用）。
fn jittered_backoff(base: Duration) -> Duration {
    let base_ns = base.as_nanos() as u64;
    // monotonic ns の下位ビットを擬似乱数源に使う。
    let entropy = monotonic_now_ns() as u64;
    // ±(base/8) の範囲。
    let span = (base_ns / 8).max(1);
    let delta = (entropy % (2 * span)) as i64 - span as i64;
    let result = base_ns as i64 + delta;
    Duration::from_nanos(result.max(0) as u64)
}

/// `stopping` を見ながら細かく刻んでスリープする（停止指示に素早く反応する）。
fn sleep_interruptible(shared: &Arc<SharedState>, dur: Duration) {
    let step = Duration::from_millis(50);
    let mut remaining = dur;
    while remaining > Duration::ZERO {
        if shared.stopping.load(Ordering::SeqCst) {
            return;
        }
        let s = step.min(remaining);
        thread::sleep(s);
        remaining = remaining.saturating_sub(s);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::{
        MockBackend, PanicMode, PanickingMockBackend, StallThenPanicOnReopenBackend,
        StallableMockBackend,
    };
    use std::time::Instant;

    /// 期限まで poll_chunk しながらチャンクを集めるヘルパ。
    fn collect_for(stream: &mut Stream, dur: Duration) -> Vec<AudioChunk> {
        let mut chunks = Vec::new();
        let start = Instant::now();
        while start.elapsed() < dur {
            while let Some(c) = stream.poll_chunk() {
                chunks.push(c);
            }
            thread::sleep(Duration::from_millis(5));
        }
        chunks
    }

    /// `条件 cond` が真になるまで（最大 `timeout`）待つ。真になれば true。
    fn wait_until<F: FnMut() -> bool>(mut cond: F, timeout: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if cond() {
                return true;
            }
            thread::sleep(Duration::from_millis(10));
        }
        cond()
    }

    /// `Stream::open` の結果から Err を取り出す（`Stream` は `Debug` 非実装のため
    /// `expect_err` が使えない）。Ok だった場合はメッセージ付きで panic する。
    fn open_err(result: Result<Stream>, ctx: &str) -> Error {
        match result {
            Ok(_) => panic!("{ctx}: エラーを期待したが Ok だった"),
            Err(e) => e,
        }
    }

    // --- 入力検証（Stream::open のエラー経路） ---

    /// `ring_capacity_chunks == 0` は InvalidArg で弾かれる（リング容量 0 は不正）。
    #[test]
    fn open_rejects_zero_ring_capacity() {
        let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
        let config = StreamConfig {
            ring_capacity_chunks: 0,
            ..Default::default()
        };
        let err = open_err(Stream::open(config, backend), "容量 0");
        assert!(
            matches!(err, Error::InvalidArg(_)),
            "InvalidArg のはず: {err:?}"
        );
    }

    /// 非対応の出力フォーマット（channels=3）は validate 失敗で UnsupportedFormat。
    #[test]
    fn open_rejects_invalid_output_channels() {
        let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
        let config = StreamConfig {
            output: OutputFormat {
                sample_rate: 48_000,
                channels: 3,
            },
            ..Default::default()
        };
        let err = open_err(Stream::open(config, backend), "ch=3");
        assert!(
            matches!(err, Error::UnsupportedFormat(_)),
            "UnsupportedFormat のはず: {err:?}"
        );
    }

    /// 極端な出力レート（範囲外）も UnsupportedFormat で弾かれる。
    #[test]
    fn open_rejects_out_of_range_output_rate() {
        let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
        let config = StreamConfig {
            output: OutputFormat {
                sample_rate: 1_000_000,
                channels: 2,
            },
            ..Default::default()
        };
        let err = open_err(Stream::open(config, backend), "極端なレート");
        assert!(
            matches!(err, Error::UnsupportedFormat(_)),
            "UnsupportedFormat のはず: {err:?}"
        );
    }

    /// backend の native_format が 0（rate=0 / ch=0）なら InvalidArg で弾かれる。
    #[test]
    fn open_rejects_zero_native_format() {
        // MockBackend::new は内部で max(1) するため 0 を作れない。テスト専用の
        // ゼロ native_format バックエンドを定義して検証する。
        struct ZeroFormatBackend;
        impl CaptureBackend for ZeroFormatBackend {
            fn native_format(&self) -> (u32, u16) {
                (0, 0)
            }
            fn start(&mut self, _sink: RawSink) -> Result<()> {
                Ok(())
            }
            fn stop(&mut self) {}
        }
        let backend = Box::new(ZeroFormatBackend);
        let err = open_err(
            Stream::open(StreamConfig::default(), backend),
            "native_format 0",
        );
        assert!(
            matches!(err, Error::InvalidArg(_)),
            "InvalidArg のはず: {err:?}"
        );
    }

    // --- poll_event（pull 型イベント取得） ---

    /// `poll_event` でイベントが pull 型で取れる。ChunkRing 容量を極小にして
    /// DROP_OLDEST を強制し、`Event::ChunkDropped` が poll_event で観測できることを確認。
    #[test]
    fn poll_event_yields_chunk_dropped() {
        // 容量 1 + ほとんど poll しない → 速やかに DROP_OLDEST が起きる。
        let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
        let config = StreamConfig {
            ring_capacity_chunks: 1,
            ..Default::default()
        };
        let mut stream = Stream::open(config, backend).expect("open");
        stream.start().expect("start");

        // poll_chunk せずに待つことでチャンクリングを溢れさせる。
        let got_drop = wait_until(
            || {
                // poll_event だけを回す（poll_chunk しない＝リングを詰まらせる）。
                while let Some(ev) = stream.poll_event() {
                    if matches!(ev, Event::ChunkDropped { .. }) {
                        return true;
                    }
                }
                false
            },
            Duration::from_secs(3),
        );
        stream.stop();
        assert!(got_drop, "poll_event で ChunkDropped を取得できるはず");
    }

    /// イベントが無ければ `poll_event` は None を返す（非ブロッキング・空キュー）。
    #[test]
    fn poll_event_is_none_when_empty() {
        let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
        let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");
        // start 前はイベントキューが空。
        assert!(stream.poll_event().is_none());
    }

    // --- ウォッチドッグ: stall 検知 → 自動復帰 → RECOVERED ---

    /// StallableMockBackend で「初回セッションが途中で給餌停止 → ウォッチドッグが
    /// STALL_THRESHOLD 後に失速検知 → backend を再オープン → 復帰後の最初のチャンクへ
    /// RECOVERED|DISCONTINUITY」を end-to-end で検証する。
    ///
    /// 観測:
    /// 1. `Event::StreamStalled` が発火する（失速判定）。
    /// 2. `Event::StreamRecovered` が発火する（再オープン成功）。
    /// 3. 復帰後の最初のチャンクに ChunkFlags::RECOVERED が立つ（DISCONTINUITY も伴う）。
    /// 4. seq は通して単調増加（復帰でリセットされない）。
    #[test]
    fn watchdog_detects_stall_and_flags_recovered() {
        // 300ms 給餌してから初回セッションを stall させる。
        let backend = Box::new(StallableMockBackend::new(
            48_000,
            2,
            440.0,
            Duration::from_millis(300),
        ));
        let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");
        stream.start().expect("start");

        let mut chunks: Vec<AudioChunk> = Vec::new();
        let mut saw_stalled = false;
        let mut saw_recovered = false;

        // 失速検知(>=2s) → 再オープン → 復帰チャンクまで十分待つ（最大 8 秒）。
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut recovered_chunk_seen = false;
        while Instant::now() < deadline && !recovered_chunk_seen {
            while let Some(c) = stream.poll_chunk() {
                if c.flags.contains(ChunkFlags::RECOVERED) {
                    recovered_chunk_seen = true;
                }
                chunks.push(c);
            }
            while let Some(ev) = stream.poll_event() {
                match ev {
                    Event::StreamStalled => saw_stalled = true,
                    Event::StreamRecovered => saw_recovered = true,
                    _ => {}
                }
            }
            thread::sleep(Duration::from_millis(20));
        }
        stream.stop();
        // stop 後の残りも取り切る。
        while let Some(c) = stream.poll_chunk() {
            if c.flags.contains(ChunkFlags::RECOVERED) {
                recovered_chunk_seen = true;
            }
            chunks.push(c);
        }

        assert!(saw_stalled, "Event::StreamStalled が発火するはず");
        assert!(saw_recovered, "Event::StreamRecovered が発火するはず");
        assert!(
            recovered_chunk_seen,
            "復帰後の最初のチャンクに RECOVERED が立つはず"
        );

        // RECOVERED が立ったチャンクには DISCONTINUITY も伴う（設計どおり）。
        let recovered: Vec<&AudioChunk> = chunks
            .iter()
            .filter(|c| c.flags.contains(ChunkFlags::RECOVERED))
            .collect();
        assert!(!recovered.is_empty());
        for c in &recovered {
            assert!(
                c.flags.contains(ChunkFlags::DISCONTINUITY),
                "RECOVERED には DISCONTINUITY が伴うはず: flags={:?}",
                c.flags
            );
        }

        // seq は通して単調増加（復帰でリセットされない）。
        for w in chunks.windows(2) {
            assert!(
                w[1].seq > w[0].seq,
                "seq は復帰をまたいでも単調増加: {} -> {}",
                w[0].seq,
                w[1].seq
            );
        }
    }

    /// 安定給餌（stall しない）なら RECOVERED は一切立たず、StreamStalled も来ない
    /// （回帰: ウォッチドッグが誤検知しない）。StallableMockBackend を十分小さい
    /// stall を起こさない値で使うのではなく、通常 MockBackend で短時間確認する。
    #[test]
    fn no_recovered_flag_under_steady_feed() {
        let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
        let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");
        stream.start().expect("start");

        // STALL_THRESHOLD 未満の短時間でフラグ・イベントを確認する。
        let chunks = collect_for(&mut stream, Duration::from_millis(500));
        let mut saw_stalled = false;
        while let Some(ev) = stream.poll_event() {
            if matches!(ev, Event::StreamStalled) {
                saw_stalled = true;
            }
        }
        stream.stop();

        assert!(!chunks.is_empty(), "安定給餌でチャンクが来るはず");
        assert!(!saw_stalled, "安定給餌では失速判定されないはず");
        for c in &chunks {
            assert!(
                !c.flags.contains(ChunkFlags::RECOVERED),
                "安定給餌では RECOVERED は立たない: flags={:?}",
                c.flags
            );
        }
    }

    // --- pause / resume（配信だけ止める） ---

    /// ポーズすると新しいチャンクが届かなくなる。ポーズ前に最低 1 個は取れて、ポーズ後の
    /// 一定窓では新規がゼロであることを確認する。
    #[test]
    fn pause_stops_delivering_chunks() {
        let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
        let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");
        stream.start().expect("start");

        // ポーズ前に少なくとも 1 個チャンクが届くまで待つ。
        let got_before = wait_until(|| stream.poll_chunk().is_some(), Duration::from_secs(2));
        assert!(got_before, "ポーズ前にチャンクが届くはず");

        // ポーズ。直後にリングへ残っていたぶんは取り切っておく。
        stream.pause();
        while stream.poll_chunk().is_some() {}

        // ポーズ後の窓では新規チャンクが来ないこと。
        let after = collect_for(&mut stream, Duration::from_millis(300));
        stream.stop();
        assert!(
            after.is_empty(),
            "ポーズ中は新しいチャンクが届かないはず: {} 個届いた",
            after.len()
        );
    }

    /// STALL_THRESHOLD を超える長いポーズでも失速判定されない。配信は止めても OS 側の
    /// 取り込み（last_sample_ns の更新）は続くので、ウォッチドッグは idle を検出しない。
    /// ポーズ窓は STALL_THRESHOLD + ウォッチドッグ tick より十分長く取る。
    #[test]
    fn long_pause_does_not_trigger_stall() {
        let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
        let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");
        stream.start().expect("start");

        // ポーズ前に少なくとも 1 個チャンクが届くまで待つ。
        let got_before = wait_until(|| stream.poll_chunk().is_some(), Duration::from_secs(2));
        assert!(got_before, "ポーズ前にチャンクが届くはず");

        // ポーズ。直後にリングへ残っていたぶんは取り切っておく。
        stream.pause();
        while stream.poll_chunk().is_some() {}

        // STALL_THRESHOLD（2s）を確実に超える時間ポーズを保ち、その間イベントを集める。
        let mut saw_stalled = false;
        let mut saw_recovered = false;
        let deadline = Instant::now() + Duration::from_millis(2800);
        while Instant::now() < deadline {
            while let Some(ev) = stream.poll_event() {
                match ev {
                    Event::StreamStalled => saw_stalled = true,
                    Event::StreamRecovered => saw_recovered = true,
                    _ => {}
                }
            }
            // ポーズ中はずっと paused のまま。
            assert!(
                stream.is_paused(),
                "ポーズ窓の間は is_paused が true のはず"
            );
            thread::sleep(Duration::from_millis(20));
        }

        // 長いポーズでも失速判定・復帰は一切起きないこと（これが主眼）。
        assert!(
            !saw_stalled,
            "長いポーズでも StreamStalled は発火しないはず"
        );
        assert!(
            !saw_recovered,
            "失速していないので StreamRecovered も発火しないはず"
        );

        // resume するとチャンク配信が再開する。
        stream.resume();
        let resumed = wait_until(|| stream.poll_chunk().is_some(), Duration::from_secs(2));
        stream.stop();
        assert!(resumed, "resume 後にチャンク配信が再開するはず");
    }

    /// resume 後の最初のチャンクに DISCONTINUITY が立ち、seq はポーズ前後で連続する
    /// （ポーズ前最後が N なら resume 後最初は N+1）。dropped_before も 0。
    #[test]
    fn resume_flags_discontinuity_and_keeps_seq_continuous() {
        let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
        let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");
        stream.start().expect("start");

        // ポーズ前のチャンクを集めて、最後の seq を控える。
        let before = collect_for(&mut stream, Duration::from_millis(200));
        assert!(!before.is_empty(), "ポーズ前にチャンクが届くはず");
        let last_seq = before.last().unwrap().seq;

        // ポーズして、リングに残ったぶんを取り切る。最後の seq を更新しておく。
        stream.pause();
        let mut last_seq = last_seq;
        while let Some(c) = stream.poll_chunk() {
            last_seq = c.seq;
        }

        // ポーズ中は新規が来ないことを軽く確認してから resume。
        assert!(collect_for(&mut stream, Duration::from_millis(150)).is_empty());
        stream.resume();

        // resume 後の最初のチャンクを待つ。
        let mut first_after: Option<AudioChunk> = None;
        let got = wait_until(
            || match stream.poll_chunk() {
                Some(c) => {
                    first_after = Some(c);
                    true
                }
                None => false,
            },
            Duration::from_secs(2),
        );
        stream.stop();
        assert!(got, "resume 後にチャンクが届くはず");

        let first = first_after.unwrap();
        assert!(
            first.flags.contains(ChunkFlags::DISCONTINUITY),
            "resume 後の最初のチャンクに DISCONTINUITY が立つはず: flags={:?}",
            first.flags
        );
        assert_eq!(
            first.seq,
            last_seq + 1,
            "seq はポーズ前後で連続するはず（{last_seq} -> {}）",
            first.seq
        );
        assert_eq!(first.dropped_before, 0, "ポーズで取りこぼしは出ないはず");
    }

    /// ポーズしていないのに resume を呼んでも、次のチャンクに DISCONTINUITY は立たない
    /// （no-op）。
    #[test]
    fn resume_without_pause_is_noop() {
        let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
        let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");
        stream.start().expect("start");

        // 最初のチャンク群を捨てて、起動直後の RECOVERED/DISCONTINUITY を流しておく。
        let _ = collect_for(&mut stream, Duration::from_millis(200));

        // ポーズしていない状態で resume。
        stream.resume();

        // 以降のチャンクに DISCONTINUITY が立たないこと。
        let after = collect_for(&mut stream, Duration::from_millis(200));
        stream.stop();
        assert!(!after.is_empty(), "チャンクが届くはず");
        for c in &after {
            assert!(
                !c.flags.contains(ChunkFlags::DISCONTINUITY),
                "ポーズなしの resume では DISCONTINUITY は立たない: flags={:?}",
                c.flags
            );
        }
    }

    /// pause を二重に呼んでも、resume 一回で正常に再開する（多重呼び出し安全）。
    #[test]
    fn double_pause_then_single_resume_recovers() {
        let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
        let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");
        stream.start().expect("start");

        let before = collect_for(&mut stream, Duration::from_millis(200));
        assert!(!before.is_empty(), "ポーズ前にチャンクが届くはず");

        // pause を二重に呼ぶ。
        stream.pause();
        stream.pause();
        assert!(stream.is_paused());
        while stream.poll_chunk().is_some() {}
        assert!(collect_for(&mut stream, Duration::from_millis(150)).is_empty());

        // resume は一回。
        stream.resume();
        assert!(!stream.is_paused());
        let got = wait_until(|| stream.poll_chunk().is_some(), Duration::from_secs(2));
        stream.stop();
        assert!(got, "resume 一回で配信が再開するはず");
    }

    // --- 入力ゲイン（config.gain / set_gain） ---

    /// config で指定したゲインが完成チャンクの data と peak/rms メーターに反映される。
    /// MockBackend のサイン波は振幅 0.5 なので、gain 2.0 でチャンクのピークは約 1.0、
    /// gain 0.5 で約 0.25 になる。peak はゲイン適用後の data から算出されること
    /// （メーターがゲイン後の実レベルを示すこと）も確認する。
    #[test]
    fn gain_scales_samples_and_meters() {
        // (gain, 期待ピークの範囲)。サイン振幅 0.5 × gain。
        for (gain, lo, hi) in [(2.0f32, 0.95f32, 1.0f32), (0.5, 0.2, 0.3)] {
            let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
            let config = StreamConfig {
                gain,
                ..Default::default()
            };
            let mut stream = Stream::open(config, backend).expect("open");
            stream.start().expect("start");
            let chunks = collect_for(&mut stream, Duration::from_millis(300));
            stream.stop();
            assert!(!chunks.is_empty(), "gain={gain} でチャンクが届くはず");

            // peak はゲイン適用後の data と一致する（メーターはゲイン後の実レベル）。
            let mut max_peak = 0.0f32;
            for c in &chunks {
                let recomputed = c.data.iter().fold(0.0f32, |m, &x| m.max(x.abs()));
                assert_eq!(
                    c.peak, recomputed,
                    "peak はゲイン適用後の data から算出されるはず"
                );
                max_peak = max_peak.max(c.peak);
            }
            assert!(
                (lo..=hi).contains(&max_peak),
                "gain={gain} のピークは {lo}..={hi} のはず: {max_peak}"
            );
        }
    }

    /// 録音中の set_gain が次のチャンクから効く。1.0 で開始してチャンクを受け取ったあと
    /// set_gain(0.0) すると、以降のチャンクが全サンプル 0・peak 0 になる。
    #[test]
    fn set_gain_takes_effect_mid_stream() {
        let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
        let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");
        stream.start().expect("start");
        assert_eq!(stream.gain(), 1.0, "既定ゲインは 1.0");

        // まず通常のチャンクが届くまで待つ。
        let got_before = wait_until(|| stream.poll_chunk().is_some(), Duration::from_secs(2));
        assert!(got_before, "set_gain 前にチャンクが届くはず");

        // ゲインを 0.0（無音）へ。次の完成チャンクから効く（20ms 粒度）。
        stream.set_gain(0.0).expect("set_gain(0.0)");
        assert_eq!(stream.gain(), 0.0);

        // 設定前に完成していたチャンクが流れてくる可能性があるので、無音チャンクの
        // 到着まで待つ。
        let got_silent = wait_until(
            || matches!(stream.poll_chunk(), Some(c) if c.peak == 0.0),
            Duration::from_secs(2),
        );
        assert!(got_silent, "set_gain(0.0) 後に無音チャンクが届くはず");

        // 以降のチャンクは全サンプル 0・peak 0・rms 0 のまま。
        let after = collect_for(&mut stream, Duration::from_millis(300));
        stream.stop();
        assert!(!after.is_empty(), "無音でもチャンクは流れ続けるはず");
        for c in &after {
            assert!(
                c.data.iter().all(|&x| x == 0.0),
                "gain 0.0 では全サンプル 0 のはず"
            );
            assert_eq!(c.peak, 0.0);
            assert_eq!(c.rms, 0.0);
        }
    }

    /// 大きなゲインでもサンプルは ±1.0 にクランプされる。サイン振幅 0.5 × gain 100 は
    /// クランプなしなら 50 に達するが、全サンプルが ±1.0 に収まり、ピークはちょうど 1.0。
    #[test]
    fn gain_clamps_to_unit_range() {
        let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
        let config = StreamConfig {
            gain: 100.0,
            ..Default::default()
        };
        let mut stream = Stream::open(config, backend).expect("open");
        stream.start().expect("start");
        let chunks = collect_for(&mut stream, Duration::from_millis(300));
        stream.stop();
        assert!(!chunks.is_empty(), "チャンクが届くはず");

        let mut max_peak = 0.0f32;
        for c in &chunks {
            assert!(
                c.data.iter().all(|&x| (-1.0..=1.0).contains(&x)),
                "サンプルは ±1.0 を超えないはず"
            );
            max_peak = max_peak.max(c.peak);
        }
        assert_eq!(max_peak, 1.0, "クランプによりピークはちょうど 1.0 のはず");
    }

    /// 不正なゲイン（負・NaN）は open / set_gain の双方で InvalidArg として弾かれる。
    #[test]
    fn invalid_gain_rejected() {
        // open: config.gain が負。
        let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
        let config = StreamConfig {
            gain: -1.0,
            ..Default::default()
        };
        let err = open_err(Stream::open(config, backend), "gain=-1.0");
        assert!(
            matches!(err, Error::InvalidArg(_)),
            "InvalidArg のはず: {err:?}"
        );

        // open: config.gain が NaN。
        let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
        let config = StreamConfig {
            gain: f32::NAN,
            ..Default::default()
        };
        let err = open_err(Stream::open(config, backend), "gain=NaN");
        assert!(
            matches!(err, Error::InvalidArg(_)),
            "InvalidArg のはず: {err:?}"
        );

        // set_gain: 負・NaN は InvalidArg で、現在値は変わらない。
        let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
        let stream = Stream::open(StreamConfig::default(), backend).expect("open");
        assert!(matches!(stream.set_gain(-1.0), Err(Error::InvalidArg(_))));
        assert!(matches!(
            stream.set_gain(f32::NAN),
            Err(Error::InvalidArg(_))
        ));
        assert_eq!(
            stream.gain(),
            1.0,
            "失敗した set_gain は現在値を変えないはず"
        );
    }

    // --- 堅牢性: backend の panic で無言死しない（poison 連鎖 panic 防止） ---
    //
    // これらのテストは「テストプロセス自体が panic で落ちない」こと自体が
    // 「無言死しない／連鎖 panic しない」ことの証明になる（落ちれば test result が
    // FAILED になる）。加えて、panic が Err / Event::Error として観測できることを
    // アサートし、握りつぶし（panic を黙って消すだけ）でないことを確かめる。

    /// backend の `start()` が panic してもプロセスは落ちず、`start()` が
    /// `Err(Error::Backend)` を返す（catch_unwind が mutex poison 前に変換するため、
    /// 取り込み/ウォッチドッグスレッドは起動すらされず連鎖 panic も起きない）。
    #[test]
    fn backend_panic_in_start_returns_err_not_silent_death() {
        let backend = Box::new(PanickingMockBackend::new(
            48_000,
            2,
            440.0,
            PanicMode::Start,
        ));
        let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");

        // start() は panic を伝播させず Err(Error::Backend) を返さねばならない。
        let result = stream.start();
        match result {
            Ok(()) => panic!("backend が start で panic したのに start() が Ok を返した"),
            Err(Error::Backend(msg)) => {
                assert!(
                    msg.contains("panicked"),
                    "Error::Backend は panic 由来と分かるメッセージのはず: {msg}"
                );
            }
            Err(other) => panic!("Error::Backend を期待したが別のエラー: {other:?}"),
        }

        // start 失敗後は未開始状態。stop しても（スレッド未起動でも）panic しない。
        stream.stop();
    }

    /// backend の `stop()` が panic してもプロセスは落ちず、`stop()` は正常に戻る
    /// （catch_unwind が握りつぶし、backend mutex を poison させないので、それまで
    /// 動いていた取り込み/ウォッチドッグスレッドが連鎖 panic しない）。
    #[test]
    fn backend_panic_in_stop_does_not_kill_process() {
        let backend = Box::new(PanickingMockBackend::new(48_000, 2, 440.0, PanicMode::Stop));
        let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");
        stream.start().expect("start");

        // 少し回して、取り込み/ウォッチドッグスレッドが実際に動いている状態を作る
        // （チャンクが流れることを確認＝happy path は不変）。
        let chunks = collect_for(&mut stream, Duration::from_millis(300));
        assert!(
            !chunks.is_empty(),
            "stop 前に通常どおりチャンクが流れるはず（happy path 不変）"
        );

        // stop() 内で backend.stop() が panic するが、catch_unwind で握りつぶされ
        // mutex を poison させない。このテストが panic で落ちないこと自体が証明。
        stream.stop();

        // stop 後も poll は連鎖 panic せず使える（poison していないことの追加確認）。
        let _ = stream.poll_chunk();
        let _ = stream.poll_event();
    }

    /// ウォッチドッグの再オープン時に backend が panic しても、ウォッチドッグ
    /// スレッドは連鎖無言死せず、`Event::Error`（"reopen failed: ..."）として表に
    /// 出る。プロセスは落ちない（catch_unwind が mutex poison を防ぎ、reopen の
    /// 失敗を `open_backend_once` の `Err` 経由で Event::Error 化する）。
    #[test]
    fn backend_panic_on_watchdog_reopen_surfaces_event_error() {
        // 300ms 給餌 → 失速 → ウォッチドッグ再オープンで panic。
        let backend = Box::new(StallThenPanicOnReopenBackend::new(
            48_000,
            2,
            440.0,
            Duration::from_millis(300),
        ));
        let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");
        stream.start().expect("start");

        // 失速検知(>=2s) → 再オープン試行(panic→Event::Error) まで十分待つ（最大 8 秒）。
        let mut saw_stalled = false;
        let mut saw_reopen_error = false;
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline && !saw_reopen_error {
            // poll_chunk も回す（リング詰まりで他経路が止まらないように）。
            while stream.poll_chunk().is_some() {}
            while let Some(ev) = stream.poll_event() {
                match ev {
                    Event::StreamStalled => saw_stalled = true,
                    Event::Error(msg) if msg.contains("reopen failed") => {
                        saw_reopen_error = true;
                    }
                    _ => {}
                }
            }
            thread::sleep(Duration::from_millis(20));
        }
        stream.stop();

        assert!(saw_stalled, "失速は検知されるはず（Event::StreamStalled）");
        assert!(
            saw_reopen_error,
            "再オープンでの backend panic は Event::Error(\"reopen failed: ...\") として\
             表に出るはず（無言死しない）"
        );
    }

    // --- 絶対クロック（録音 0 起点） ---

    /// 最初に配信されるチャンクの pts_ns は録音エポックそのものなので厳密に 0 で始まる。
    #[test]
    fn recording_clock_is_zero_based() {
        let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
        let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");
        stream.start().expect("start");

        let mut first: Option<AudioChunk> = None;
        let got = wait_until(
            || match stream.poll_chunk() {
                Some(c) => {
                    first = Some(c);
                    true
                }
                None => false,
            },
            Duration::from_secs(2),
        );
        stream.stop();
        assert!(got, "最初のチャンクが届くはず");
        let first = first.unwrap();
        assert_eq!(
            first.pts_ns, 0,
            "最初に配信されるチャンクは録音 0 起点（pts_ns == 0）のはず: {}",
            first.pts_ns
        );
    }

    /// pause 跨ぎで pts が pause 継続時間ぶん前進する（キャプチャ実時間の時計）。resume 後の
    /// 最初のチャンクに DISCONTINUITY・seq 連続・dropped_before 0 も確認する。
    #[test]
    fn pause_preserves_absolute_clock() {
        let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
        let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");
        stream.start().expect("start");

        // pause 前のチャンクを集め、最後の (pts_ns, seq) を控える。
        let before = collect_for(&mut stream, Duration::from_millis(250));
        assert!(!before.is_empty(), "pause 前にチャンクが届くはず");
        let mut last = before.last().cloned().unwrap();

        stream.pause();
        while let Some(c) = stream.poll_chunk() {
            last = c;
        }

        // 既知時間 D の pause を保つ（STALL_THRESHOLD 未満）。
        let d = Duration::from_millis(600);
        thread::sleep(d);
        stream.resume();

        let mut first_after: Option<AudioChunk> = None;
        let got = wait_until(
            || match stream.poll_chunk() {
                Some(c) => {
                    first_after = Some(c);
                    true
                }
                None => false,
            },
            Duration::from_secs(2),
        );
        stream.stop();
        assert!(got, "resume 後にチャンクが届くはず");
        let first = first_after.unwrap();

        assert!(
            first.flags.contains(ChunkFlags::DISCONTINUITY),
            "resume 後の最初のチャンクに DISCONTINUITY が立つはず: {:?}",
            first.flags
        );
        assert_eq!(first.seq, last.seq + 1, "seq は pause 前後で連続するはず");
        assert_eq!(first.dropped_before, 0, "pause で取りこぼしは出ない");

        // pts は pause 継続時間 D ぶん前進する（キャプチャ実時間）。CI 揺れを避けるため下限
        // D*0.8、上限 D + 余裕で挟む。
        let delta = first.pts_ns - last.pts_ns;
        let d_ns = d.as_nanos() as i64;
        assert!(
            delta >= d_ns * 4 / 5,
            "pts は pause ぶん（>= {} ns）前進するはず: delta={delta} ns",
            d_ns * 4 / 5
        );
        assert!(
            delta <= d_ns + 500_000_000,
            "pts の前進が過大でない（<= D + 500ms）: delta={delta} ns"
        );
    }

    // --- デュアル出力（主 + 副タップ） ---

    /// secondary_output を設定すると主（48k/stereo）と副（16k/mono）が同時に配信される。
    /// 副チャンクは 320 sample、pts は主と同じ 0 起点時計に乗る。
    #[test]
    fn dual_output_delivers_primary_and_secondary() {
        let config = StreamConfig {
            secondary_output: Some(OutputFormat {
                sample_rate: 16_000,
                channels: 1,
            }),
            // 収集窓の間に DROP_OLDEST で先頭（pts 0）が流れ去らないよう大きめに取る。
            ring_capacity_chunks: 200,
            ..Default::default()
        };
        let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
        let mut stream = Stream::open(config, backend).expect("open");
        stream.start().expect("start");

        let mut primary: Vec<AudioChunk> = Vec::new();
        let mut secondary: Vec<SecondaryChunk> = Vec::new();
        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline {
            while let Some(c) = stream.poll_chunk() {
                primary.push(c);
            }
            while let Some(c) = stream.poll_secondary() {
                secondary.push(c);
            }
            thread::sleep(Duration::from_millis(5));
        }
        stream.stop();
        while let Some(c) = stream.poll_chunk() {
            primary.push(c);
        }
        while let Some(c) = stream.poll_secondary() {
            secondary.push(c);
        }

        assert!(!primary.is_empty(), "主チャンクが届くはず");
        assert!(!secondary.is_empty(), "副チャンクが届くはず");
        for c in &primary {
            assert_eq!(c.data.len(), 960 * 2, "主は 48k/stereo = 1920 sample");
        }
        for c in &secondary {
            assert_eq!(c.samples.len(), 320, "副は 16k/mono = 320 sample");
        }
        // 両タップとも 0 起点で非減少。主の先頭は 0。
        assert_eq!(primary[0].pts_ns, 0, "主先頭は 0 起点");
        for w in secondary.windows(2) {
            assert!(w[1].pts_ns >= w[0].pts_ns, "副 pts は非減少");
        }
        assert!(secondary[0].pts_ns >= 0, "副 pts は非負（主エポック基準）");
        // 副 seq は独自カウンタで 0 始まり単調。
        assert_eq!(secondary[0].seq, 0);
        for w in secondary.windows(2) {
            assert_eq!(w[1].seq, w[0].seq + 1, "副 seq は単調連番");
        }
    }

    /// switch_source では secondary_output を変更できない（open 時固定）。
    #[test]
    fn secondary_output_cannot_change_on_switch() {
        let config = StreamConfig {
            secondary_output: Some(OutputFormat {
                sample_rate: 16_000,
                channels: 1,
            }),
            ..Default::default()
        };
        let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
        let mut stream = Stream::open(config, backend).expect("open");
        stream.start().expect("start");

        // 副フォーマットを変える切替要求は InvalidArg で弾かれる（backend 構築より前）。
        let new_config = StreamConfig {
            secondary_output: Some(OutputFormat {
                sample_rate: 8_000,
                channels: 1,
            }),
            ..Default::default()
        };
        let err = stream.switch_source(new_config);
        stream.stop();
        assert!(
            matches!(err, Err(Error::InvalidArg(_))),
            "副フォーマット変更は InvalidArg のはず: {err:?}"
        );
    }

    // --- RawRing オーバーフロー → DISCONTINUITY ---

    /// RawRing を溢れさせる擬似バックエンド。start で 1 バーストがリング容量の 2 倍を
    /// push し続け、必ず overflow を起こす。テスト専用。
    struct FloodingMockBackend {
        running: Arc<AtomicBool>,
        handle: Option<thread::JoinHandle<()>>,
    }

    impl FloodingMockBackend {
        fn new() -> Self {
            Self {
                running: Arc::new(AtomicBool::new(false)),
                handle: None,
            }
        }
    }

    impl CaptureBackend for FloodingMockBackend {
        fn native_format(&self) -> (u32, u16) {
            (48_000, 2)
        }
        fn start(&mut self, mut sink: RawSink) -> Result<()> {
            self.running.store(true, Ordering::SeqCst);
            let running = self.running.clone();
            let handle = thread::spawn(move || {
                // リング容量の 2 倍を毎バースト push（必ず溢れる）。
                let burst = vec![0.1f32; RAW_RING_SAMPLES * 2];
                while running.load(Ordering::SeqCst) {
                    sink.push(&burst, 0);
                    thread::sleep(Duration::from_millis(5));
                }
            });
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

    /// 持続的な RawRing オーバーフローが検知され、いずれかのチャンクに DISCONTINUITY が
    /// 立つ（フレッシュ start では overflow 以外の不連続源が無いので、DISCONTINUITY =
    /// overflow 由来）。
    #[test]
    fn ring_overflow_marks_discontinuity() {
        let backend = Box::new(FloodingMockBackend::new());
        // ChunkRing を大きめにして DROP_OLDEST で観測前に流れ去るのを避ける。
        let config = StreamConfig {
            ring_capacity_chunks: 200,
            ..Default::default()
        };
        let mut stream = Stream::open(config, backend).expect("open");
        stream.start().expect("start");

        let mut saw_discontinuity = false;
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline && !saw_discontinuity {
            while let Some(c) = stream.poll_chunk() {
                if c.flags.contains(ChunkFlags::DISCONTINUITY) {
                    saw_discontinuity = true;
                }
            }
            thread::sleep(Duration::from_millis(10));
        }
        stream.stop();
        while let Some(c) = stream.poll_chunk() {
            if c.flags.contains(ChunkFlags::DISCONTINUITY) {
                saw_discontinuity = true;
            }
        }
        assert!(
            saw_discontinuity,
            "RawRing オーバーフローで DISCONTINUITY が立つはず"
        );
    }

    // --- denoise を内部正規形へ注入（core InnerProcessor 経由） ---

    /// set_denoise(true) 後も主・副の両タップが配信され続ける（denoise 配線のスモーク）。
    /// 実際の除去効果は core のフラッシュ/処理テストで担保する。
    #[test]
    fn denoise_enabled_still_delivers_both_taps() {
        let config = StreamConfig {
            secondary_output: Some(OutputFormat {
                sample_rate: 16_000,
                channels: 1,
            }),
            ..Default::default()
        };
        let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
        let mut stream = Stream::open(config, backend).expect("open");
        stream.set_denoise(true);
        stream.start().expect("start");

        let mut primary = 0usize;
        let mut secondary = 0usize;
        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline {
            while let Some(_c) = stream.poll_chunk() {
                primary += 1;
            }
            while let Some(c) = stream.poll_secondary() {
                assert_eq!(c.samples.len(), 320, "副は 16k/mono = 320 sample");
                secondary += 1;
            }
            thread::sleep(Duration::from_millis(5));
        }
        stream.stop();
        assert!(primary > 0, "denoise 有効でも主チャンクが届くはず");
        assert!(secondary > 0, "denoise 有効でも副チャンクが届くはず");
    }
}
