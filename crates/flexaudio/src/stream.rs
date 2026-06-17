//! 1 ソースのキャプチャパイプラインを所有する [`Stream`]。
//!
//! コア部品（[`RawRing`](flexaudio_core::raw_ring) / [`Normalizer`] /
//! [`ChunkRing`](flexaudio_core::chunk_ring) / [`ClockNormalizer`] /
//! [`CaptureBackend`]）を配線し、プル型 API（[`poll_chunk`](Stream::poll_chunk) /
//! [`poll_event`](Stream::poll_event)）で消費側へ供給する。
//!
//! # スレッド構成
//! - **backend の RT スレッド**: [`RawSink`] 経由で生フレームを [`RawRing`] へ push のみ
//!   （非ブロッキング）。
//! - **取り込み/加工スレッド (1 本・通常優先度)**: RawRing を pop → [`Normalizer`] で
//!   48k/stereo/20ms 化 → 単調増加 `seq` を付与 → [`ChunkRing`]（DROP_OLDEST）へ push。
//!   最後にサンプルを処理した時刻を `AtomicI64` で更新する。
//! - **ウォッチドッグスレッド (1 本・~250ms tick)**: 一定時間サンプル更新が止まったら
//!   「無音死」と判定し、backend を指数バックオフ（250ms→5s・ジッタ）で再オープンする。
//!   失速で [`Event::StreamStalled`]、復帰で [`Event::StreamRecovered`] を発火し、復帰後の
//!   最初のチャンクに [`ChunkFlags::RECOVERED`] | [`ChunkFlags::DISCONTINUITY`] を立てる。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use flexaudio_core::backend::{CaptureBackend, RawSink};
use flexaudio_core::chunk_ring::{chunk_ring, ChunkConsumer, ChunkProducer};
use flexaudio_core::clock::{monotonic_now_ns, ClockNormalizer};
use flexaudio_core::normalizer::Normalizer;
use flexaudio_core::raw_ring::{raw_ring, RawConsumer};
use flexaudio_core::types::{
    AudioChunk, ChunkFlags, Error, Event, OutputFormat, Result, StreamConfig,
};

/// RawRing の容量（f32 サンプル単位）。ネイティブ SR×ch に依存させず、
/// 多めに確保して RT 経路のドロップを避ける（約 0.5 秒 @ 48k stereo 相当の余裕）。
const RAW_RING_SAMPLES: usize = 48_000;

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
/// 非ブロッキングに呼ぶ。[`stop`](Self::stop) は全スレッドを確実に join する。
pub struct Stream {
    config: StreamConfig,

    /// backend を共有し、取り込みスレッド/ウォッチドッグスレッドが (再)オープンする。
    shared: Arc<SharedState>,

    /// 消費側が取り出すチャンクリングの consumer。
    chunk_consumer: ChunkConsumer,

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
    /// [`Stream::switch_source`] でソースを差し替えると **新 backend の値へ更新**
    /// される（mic↔system/process でネイティブ SR/ch が変わるのが普通）。
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
}

impl SharedState {
    fn push_event(&self, ev: Event) {
        if let Ok(mut q) = self.events.lock() {
            q.push_back(ev);
        }
    }
}

impl Stream {
    /// 構成と backend からストリームを開く（まだキャプチャは始めない）。
    ///
    /// `config.chunk_ms` は固定契約上 20ms 前提。`ring_capacity_chunks` が
    /// チャンクリング容量になる。backend の [`native_format`](CaptureBackend::native_format)
    /// から [`Normalizer`] を構成する。
    pub fn open(config: StreamConfig, backend: Box<dyn CaptureBackend>) -> Result<Stream> {
        if config.ring_capacity_chunks == 0 {
            return Err(Error::InvalidArg(
                "ring_capacity_chunks must be > 0".into(),
            ));
        }
        // 出力フォーマットが MVP の対応域か検証（非対応は UnsupportedFormat）。
        config.output.validate()?;
        let native_format = backend.native_format();
        if native_format.0 == 0 || native_format.1 == 0 {
            return Err(Error::InvalidArg(
                "backend native_format must have non-zero rate and channels".into(),
            ));
        }

        let (chunk_producer, chunk_consumer) = chunk_ring(config.ring_capacity_chunks);
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
        });

        Ok(Stream {
            config,
            shared,
            chunk_consumer,
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

        // 初回 backend 起動: RawRing を作り sink を backend へ渡す。
        Self::open_backend_once(&self.shared)?;

        // 取り込み/加工スレッドへ移すため chunk_producer を取り出す。
        let chunk_producer = self
            .shared
            .chunk_producer
            .lock()
            .expect("chunk_producer mutex")
            .take()
            .ok_or_else(|| Error::InvalidState("chunk producer already taken".into()))?;

        // 取り込み/加工スレッド。初期 native_format は shared から読む
        // （以降は世代変化のたびに run_intake が shared を読み直して追従する）。
        let worker_shared = self.shared.clone();
        let initial_native = *self.shared.native_format.lock().expect("native_format mutex");
        let output = self.config.output;
        let worker = thread::Builder::new()
            .name("flexaudio-intake".into())
            .spawn(move || {
                run_intake(worker_shared, chunk_producer, initial_native, output);
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
        if let Ok(mut be) = self.shared.backend.lock() {
            be.stop();
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

    /// 完成済みチャンクを 1 つ取り出す（非ブロッキング）。無ければ `None`。
    ///
    /// 返るチャンクは出力フォーマット（`config.output`）の interleaved `f32`。
    /// チャンクは時間ベース 20ms 固定で `data.len() == frames * output.channels`。
    /// 既定 `{48000, 2}` なら `frames == 960`（`data.len() == 1920`）。
    /// `peak`/`rms` は最終 data に対して算出済み。`seq` は単調増加。
    pub fn poll_chunk(&mut self) -> Option<AudioChunk> {
        self.chunk_consumer.try_pop()
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
        *self.shared.native_format.lock().expect("native_format mutex")
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
        let (rate, channels) = {
            let be = shared.backend.lock().expect("backend mutex");
            be.native_format()
        };
        {
            let mut nf = shared.native_format.lock().expect("native_format mutex");
            *nf = (rate, channels);
        }

        // 新しい RawRing（旧 format の残骸を持ち込まない）。
        let (producer, consumer) = raw_ring(RAW_RING_SAMPLES);
        let sink = RawSink::new(producer, rate, channels);

        {
            let mut be = shared.backend.lock().expect("backend mutex");
            be.start(sink)?;
        }

        // 新しい consumer を共有へ載せ、世代を進める（旧 consumer は drop）。
        {
            let mut rc = shared.raw_consumer.lock().expect("raw_consumer mutex");
            *rc = Some(consumer);
        }
        shared.raw_generation.fetch_add(1, Ordering::SeqCst);

        // 起動直後を「最後に到着した時刻」として扱い、即失速判定を避ける。
        shared
            .last_sample_ns
            .store(monotonic_now_ns(), Ordering::SeqCst);
        Ok(())
    }

    /// 低レベルなソース切替: 現在の backend を **新しい backend へ差し替え**、
    /// チャンクストリーム（seq・PTS）の連続性を保ったまま入力ソースを変える。
    ///
    /// `seq` は取り込みスレッドのローカル変数で backend にも `SharedState` にも
    /// 無いため、**ここで触らなければ差し替え前後で連続**する。PTS は取り込み
    /// スレッドが世代変化を検知して `Normalizer`/`ClockNormalizer` を作り直し、
    /// 新ソース初回サンプルの実時刻で再アンカーするので単調を保つ。
    ///
    /// 手順（**generation++ は最後に 1 回だけ**・全 Atomic は SeqCst）:
    /// - 未 start なら [`Error::InvalidState`]。
    /// - `switching = true`（ウォッチドッグの並行再オープンを止める）。
    /// - backend ロック下で旧 backend を `stop()` → 新 backend の native を取得して
    ///   `shared.native_format` 更新 → 新 RawRing → `new_backend.start(sink)`。
    ///   - 成功: backend を新へ差し替え、新 consumer を載せ替え（旧 drop）。
    ///   - 失敗: 旧 backend を [`open_backend_once`](Self::open_backend_once) で
    ///     再 start して旧ソースを継続（連続性を壊さない）。`discontinuity_pending`
    ///     を立て世代を ++、`switching=false` にして `Err` を返す。
    /// - 成功時: `discontinuity_pending = true`（RECOVERED は付けない＝意図的切替）
    ///   → `generation += 1`（**最後に 1 回だけ**）→ `last_sample_ns = now` →
    ///   `switching = false` → `Ok`。
    ///
    /// テスト容易性のため [`Box<dyn CaptureBackend>`] を直接受け取る（mock backend
    /// で切替挙動を検証できる）。高レベル入口は [`switch_source`](Self::switch_source)。
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
        {
            let mut be = self.shared.backend.lock().expect("backend mutex");

            // 旧 backend を止める（RT push を止める）。
            be.stop();

            // 新 backend のネイティブフォーマット。
            let (rate, channels) = new_backend.native_format();

            // 新 RawRing（旧 format 残骸を持ち込まない）。
            let (producer, consumer) = raw_ring(RAW_RING_SAMPLES);
            let sink = RawSink::new(producer, rate, channels);

            // 新 backend を start。失敗時は旧ソースへ復帰する。
            let mut new_backend = new_backend;
            match new_backend.start(sink) {
                Ok(()) => {
                    // --- レース防止の順序が要 ---
                    // 取り込みスレッドは「世代をロック外で load → raw_consumer を
                    // lock して pop」する。新 consumer を先に載せると、世代を ++ する
                    // 前に新ソースの native サンプルを旧 normalizer へ流し込み得る
                    // （旧 native 形でない＝位相破壊）。これを避けるため、
                    //   native_format 更新 → 世代 ++（+ DISCONTINUITY 等）→ 最後に
                    //   consumer/backend を差し替える
                    // 順にする。こうすれば取り込み側が新 consumer を観測する時には
                    // 必ず新世代が見えており、normalizer を作り直してから pop する。
                    //
                    // shared.native_format を新ソースの値へ更新。
                    {
                        let mut nf =
                            self.shared.native_format.lock().expect("native_format mutex");
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
                        let mut rc =
                            self.shared.raw_consumer.lock().expect("raw_consumer mutex");
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
    /// `new_config` からソース別バックエンドを [`build_backend`](crate::build_backend)
    /// で構築し（失敗時は旧ソース無傷のまま `Err`）、[`switch_backend`](Self::switch_backend)
    /// で差し替える。出力フォーマット（`output`）は **切替不可**（チャンク
    /// frames/data.len が変わると連続ストリームが壊れるため）。変更要求は
    /// [`Error::InvalidArg`] で弾く。
    ///
    /// 成功時、`config` の **可変項目のみ**（`kind` / `device_id` / `target_pid`
    /// / `exclude_self`）を新しい値へ更新する。`output` / `chunk_ms`
    /// / `ring_capacity_chunks` は据え置く。
    ///
    /// # エラー
    /// - 未 start → [`Error::InvalidState`]。
    /// - `output` 変更要求 → [`Error::InvalidArg`]。
    /// - 新ソースの backend 構築失敗（process の PID 欠落・非対応 OS 等）→
    ///   [`build_backend`](crate::build_backend) 由来のエラー（旧ソースは無傷）。
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
        // 新ソースの backend を構築（失敗時は旧ソース無傷のまま早期 return）。
        let backend = crate::build_backend(&new_config)?;
        // 差し替え（連続性は switch_backend が保証）。
        self.switch_backend(backend)?;
        // 成功時のみ config の可変項目を更新（output 等は据え置き）。
        self.config = StreamConfig {
            kind: new_config.kind,
            device_id: new_config.device_id,
            target_pid: new_config.target_pid,
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

/// 取り込み/加工スレッド本体。
///
/// RawConsumer を pop → [`Normalizer`]（2 段: 内部正規化 → 出力フォーマット再変換）
/// へ投入 → 完成チャンクへ `seq`・peak/rms 付与 → ChunkRing へ push。世代変化
/// （再オープン）を検知したら Normalizer/Clock を作り直し、次チャンクへ
/// RECOVERED|DISCONTINUITY を立てる。
fn run_intake(
    shared: Arc<SharedState>,
    mut chunk_producer: ChunkProducer,
    initial_native: (u32, u16),
    output: OutputFormat,
) {
    let (mut rate, mut channels) = initial_native;
    // Normalizer 構築失敗（rubato 構築失敗等）は無言で死なせず Event::Error を出して終了。
    // 従来は Normalizer::new が panic していたため取り込みスレッドが無言死していた。
    let mut normalizer = match Normalizer::new(rate, channels, output) {
        Ok(n) => n,
        Err(e) => {
            shared.push_event(Event::Error(format!("normalizer init failed: {e}")));
            return;
        }
    };
    let mut clock = ClockNormalizer::new();
    let mut seq: u64 = 0;
    let mut current_generation = shared.raw_generation.load(Ordering::SeqCst);

    // pop 用スクラッチ（ネイティブ ch のフレーム×ある程度）。
    let mut scratch = vec![0.0f32; RAW_RING_SAMPLES];

    loop {
        if shared.stopping.load(Ordering::SeqCst) {
            // 停止前に Normalizer/RawRing に残ったものは捨てる（部分チャンクは出さない）。
            break;
        }

        // 世代変化（再オープン / ソース切替）検知 → 新しいソースへリセット。
        // ソース切替では native_format が変わり得るので shared を読み直し、
        // 第 1 段（native 依存）の Normalizer を作り直す（ウォッチドッグ復帰では
        // 同じ値が読めるため挙動は従来どおり）。
        let gen = shared.raw_generation.load(Ordering::SeqCst);
        if gen != current_generation {
            current_generation = gen;
            let nf = *shared.native_format.lock().expect("native_format mutex");
            rate = nf.0;
            channels = nf.1;
            // 新ソース向け Normalizer 再構築。失敗（rubato 構築失敗等）は無言死せず
            // Event::Error を出して取り込みを終了する。
            normalizer = match Normalizer::new(rate, channels, output) {
                Ok(n) => n,
                Err(e) => {
                    shared.push_event(Event::Error(format!(
                        "normalizer rebuild failed after source change: {e}"
                    )));
                    return;
                }
            };
            clock = ClockNormalizer::new();
        }

        // RawRing から取り出して Normalizer へ。
        let mut produced_any = false;
        // Normalizer::push の失敗（rubato process 失敗等）を持ち越す。ロックを保持した
        // まま return しないよう、ブロック内では結果だけ控えてブロック後に処理する。
        let mut push_err: Option<Error> = None;
        {
            let mut rc_guard = shared.raw_consumer.lock().expect("raw_consumer mutex");
            if let Some(rc) = rc_guard.as_mut() {
                let got = rc.pop_slice(&mut scratch);
                if got > 0 {
                    // pop_slice は consumer ロック内で完結。Normalizer 投入はロック外で
                    // 行いたいので、必要分をローカルへ移す。
                    // （ここでは小さな move コピーで十分。RT 経路には触れない。）
                    let samples = &scratch[..got];
                    // device PTS: ネイティブ SR を基準にした単調近似（到着時刻）。
                    let device_pts = monotonic_now_ns();
                    let norm_pts = clock.normalize(device_pts);
                    // push 失敗（理論上の rubato process 失敗）は無言で殺さず控える。
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

        // 完成チャンクを全て取り出して ChunkRing へ。
        let out_channels = output.channels.max(1) as usize;
        let mut emitted_any = false;
        while let Some((data, pts_ns)) = normalizer.pop_chunk() {
            // data は出力フォーマット（output.channels interleaved）。
            // frames は時間ベース 20ms 固定（48k=960 / 16k=320 / ...）。
            debug_assert_eq!(data.len() % out_channels, 0);
            let frames = data.len() / out_channels;

            // 最終 data に対して peak / rms（線形）を算出する（20ms なので極小コスト）。
            let (peak, rms) = peak_rms(&data);

            // フラグは二系統:
            //  - recovered_pending: ウォッチドッグ自動復帰 → RECOVERED|DISCONTINUITY。
            //  - discontinuity_pending: 意図的なソース切替 → DISCONTINUITY のみ。
            // 両方が同時に立つことは設計上無い（切替中は switching でウォッチドッグを
            // 止める）が、立っても OR で合成され安全（RECOVERED|DISCONTINUITY）。
            let mut flags = ChunkFlags::empty();
            if shared.recovered_pending.swap(false, Ordering::SeqCst) {
                flags |= ChunkFlags::RECOVERED | ChunkFlags::DISCONTINUITY;
            }
            if shared.discontinuity_pending.swap(false, Ordering::SeqCst) {
                flags |= ChunkFlags::DISCONTINUITY;
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

            // DROP_OLDEST。ドロップ発生なら ChunkDropped を通知。
            if let Some(total) = chunk_producer.push(chunk) {
                shared.push_event(Event::ChunkDropped { count: total });
            }
            emitted_any = true;
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
        {
            if let Ok(mut be) = shared.backend.lock() {
                be.stop();
            }
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
