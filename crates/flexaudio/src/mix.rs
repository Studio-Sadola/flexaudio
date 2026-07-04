//! mic + system を 1 本に合成する合成バックエンド [`CompositeBackend`]。
//!
//! mic と system の 2 つの子バックエンドを内部に持ち、各子の音声を内部正規形
//! （48kHz/stereo）へ揃えてから側別ゲインで加算合成し、[`Stream`](crate::Stream)
//! からはただの 1 バックエンドに見せる。Stream 本体には手を入れないので、
//! seq/PTS・ウォッチドッグ・pause・グローバル gain・switch_source がそのまま効く。
//!
//! # スレッド構成
//! - 子バックエンドの RT スレッド: それぞれ専用の子 RawRing へ push するだけ（既存
//!   backend のまま・触らない）。
//! - 合成スレッド（1 本・`flexaudio-mix`）: 子リングを pop → 子ごとの [`Normalizer`]
//!   で 48k/stereo 化 → 両側の揃ったフレームを側別ゲインで加算合成（±1.0 クランプ）
//!   → 実 sink へ push。mic と system は別々の水晶で動きレートが数〜数百 ppm ずれる
//!   ので、system 側だけを微リサンプルするドリフト補正（[`LinearStitcher`] +
//!   [`DriftController`]）で吸収する。RT ではないのでヒープ確保可（ただしループ内の
//!   定常確保はスクラッチ再利用で避ける）。

use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use flexaudio_core::backend::{CaptureBackend, RawSink};
use flexaudio_core::clock::monotonic_now_ns;
use flexaudio_core::normalizer::Normalizer;
use flexaudio_core::raw_ring::{raw_ring, RawConsumer};
use flexaudio_core::types::{Error, OutputFormat, Result, CHANNELS, SAMPLE_RATE};

use crate::stream::RAW_RING_SAMPLES;

/// 片側が供給ゼロのままこの時間を超えたら、不足分を無音（0.0）として合成を続行する。
/// 根拠: 正規化は 20ms チャンク単位でしか出てこないので、2〜3 チャンク分の到着ゆらぎ
/// までは正常とみなし、それを超えた途絶（例: システム側が何も再生していない時間帯）
/// でも録音全体は流れ続けるようにする。
const STARVATION_FILL_THRESHOLD: Duration = Duration::from_millis(60);

/// 片側の正規化済み FIFO の上限（f32 サンプル数）。約 500ms 分
/// （48kHz × 2ch × 0.5s = 48_000）を超えたら古い方から捨てる安全弁。
/// 子クロック間のレート差はドリフト補正（[`DriftController`]）が吸収するので、
/// 補正が効いている限り通常ここには到達しない。補正の範囲（±500ppm）で追い
/// つかない異常（片側の暴走供給など）に対する最後の砦として残す。
const FIFO_MAX_SAMPLES: usize = 48_000;

/// 両側とも合成する材料が無いときの待ち時間（stream.rs の取り込みスレッドと同じ流儀）。
const IDLE_SLEEP: Duration = Duration::from_millis(2);

/// ドリフト補正で system 側の読み出し比率 r を動かせる幅（1.0 ± 500ppm）。
/// 民生機の水晶の実ドリフトは通常数十〜百数十 ppm なのでこれで十分覆え、
/// r がこの範囲なら線形補間による歪みは無視できる水準に収まる（補間点が常に
/// 原サンプルのごく近傍に留まるため）。
const DRIFT_RATIO_LIMIT: f64 = 500e-6;

/// 比率を見直す間隔（合成出力の f32 サンプル数）。合成 100ms 分ごとに 1 回。
/// 正規化チャンク（20ms）5 個分で、到着粒度より粗く、ドリフトの時間スケール
/// （分オーダー）より十分細かい。
const DRIFT_UPDATE_INTERVAL_SAMPLES: usize = (SAMPLE_RATE as usize / 10) * CHANNELS as usize;

/// 残量差 EMA の係数。更新間隔 100ms と合わせて時定数はおよそ 1 秒。チャンク
/// 到着のゆらぎ（20ms 粒度のノコギリ状の残量変動）を均しつつ、ドリフトの変化
/// には十分追従できる。
const DRIFT_EMA_ALPHA: f64 = 0.1;

/// P 制御のゲイン。残量差 200ms 分（19_200 サンプル）でクランプ上限の 500ppm を
/// 使い切る傾き。この穏やかさなら、実ドリフト（数十〜数百 ppm）に対する定常
/// 残量差は数十〜百数十 ms 分（= ドリフト ÷ ゲイン）に落ち着き、500ms の安全弁
/// には遠く及ばない。
const DRIFT_GAIN: f64 = DRIFT_RATIO_LIMIT / 19_200.0;

/// 1 回の見直しで比率を動かせる上限（slew）。100ms あたり 20ppm＝クランプ幅の
/// 端から端まででも 5 秒かける。残量差の測定ノイズで比率が急変してピッチが
/// 揺れるのを防ぐ。
const DRIFT_SLEW_PER_UPDATE: f64 = 20e-6;

/// mic + system の 2 子バックエンドを内部正規形で加算合成する合成バックエンド。
///
/// [`native_format`](CaptureBackend::native_format) は常に内部正規形 `(48000, 2)` を
/// 返すので、Stream 側の第 1 段リサンプラは実質パススルーになる。子はコンストラクタ
/// 注入（テストでは mock を渡せる）。実子の構築は facade の `build_backend` が担う。
pub(crate) struct CompositeBackend {
    mic: Box<dyn CaptureBackend>,
    system: Box<dyn CaptureBackend>,
    mic_gain: f32,
    system_gain: f32,
    /// 合成スレッドへの停止指示。start のたびに新しい Arc に差し替える
    /// （旧スレッドの残骸と混線しない）。
    stopping: Arc<AtomicBool>,
    /// 合成スレッドのハンドル。`Some` なら動作中。
    mixer: Option<JoinHandle<()>>,
}

impl CompositeBackend {
    /// 子 2 つと側別ゲインを注入して作る。ゲインの検証（有限・0 以上）は呼び出し側
    /// （facade の `build_backend`）が済ませていること。
    pub(crate) fn new(
        mic: Box<dyn CaptureBackend>,
        system: Box<dyn CaptureBackend>,
        mic_gain: f32,
        system_gain: f32,
    ) -> Self {
        Self {
            mic,
            system,
            mic_gain,
            system_gain,
            stopping: Arc::new(AtomicBool::new(false)),
            mixer: None,
        }
    }
}

impl CaptureBackend for CompositeBackend {
    fn native_format(&self) -> (u32, u16) {
        // 合成は常に内部正規形で行う。Stream の第 1 段は実質パススルーになる。
        (SAMPLE_RATE, CHANNELS)
    }

    fn start(&mut self, sink: RawSink) -> Result<()> {
        // 動作中の二重 start は no-op（CaptureBackend 契約）。
        if self.mixer.is_some() {
            return Ok(());
        }

        // mic の起動に失敗したら即 Err、system の起動に失敗したら mic を stop してから
        // Err（片肺で起動成功にしない）。
        let mic_lane = start_child(&mut self.mic)?;
        let system_lane = match start_child(&mut self.system) {
            Ok(lane) => lane,
            Err(e) => {
                stop_child(&mut self.mic);
                return Err(e);
            }
        };

        // 合成スレッドを起動する。停止フラグは start ごとに新調する（前回 stop の
        // フラグを引きずらない）。
        self.stopping = Arc::new(AtomicBool::new(false));
        let stopping = self.stopping.clone();
        let mic_gain = self.mic_gain;
        let system_gain = self.system_gain;
        let mixer = thread::Builder::new()
            .name("flexaudio-mix".into())
            .spawn(move || {
                run_mixer(mic_lane, system_lane, mic_gain, system_gain, sink, stopping);
            })
            .map_err(|e| Error::Backend(format!("spawn mix thread: {e}")));
        match mixer {
            Ok(handle) => {
                self.mixer = Some(handle);
                Ok(())
            }
            Err(e) => {
                // スレッドが立たなければ子を止めて失敗を返す（片肺にしない）。
                stop_child(&mut self.mic);
                stop_child(&mut self.system);
                Err(e)
            }
        }
    }

    fn stop(&mut self) {
        // 停止フラグ → 合成スレッド join → 子 2 つを stop。冪等（未起動なら子の stop
        // だけが走るが、子側も冪等契約なので無害）。
        self.stopping.store(true, Ordering::SeqCst);
        if let Some(h) = self.mixer.take() {
            let _ = h.join();
        }
        stop_child(&mut self.mic);
        stop_child(&mut self.system);
    }
}

impl Drop for CompositeBackend {
    fn drop(&mut self) {
        // stop されずに捨てられても合成スレッドと子を残さない。
        self.stop();
    }
}

/// 片側の子の取り込み状態一式（子リング consumer + 正規化器 + 正規化済み FIFO）。
struct ChildLane {
    consumer: RawConsumer,
    /// 子ネイティブ → 内部正規形（48k/stereo）。出力を内部正規形に固定するので
    /// 第 2 段はパススルー。
    normalizer: Normalizer,
    /// 正規化済みサンプル（48k/stereo interleaved）の FIFO。
    fifo: Vec<f32>,
    /// 最後にこの側が正規化済みサンプルを供給した時刻（飢餓判定用）。
    last_supply: Instant,
}

impl ChildLane {
    /// 子リングから pop して正規化し、完成分を FIFO へ積む。
    ///
    /// FIFO が [`FIFO_MAX_SAMPLES`] を超えたら古い方から捨てる（無限成長の安全弁）。
    /// rubato の処理失敗は `Err` で返す（呼び出し側が合成スレッドを終える）。
    fn ingest(&mut self, scratch: &mut [f32]) -> Result<()> {
        let got = self.consumer.pop_slice(scratch);
        if got == 0 {
            return Ok(());
        }
        // pts は sink 側では使われない（配線層が別途取り回す契約）が、正規化器の
        // アンカー用に単調 now を渡しておく。
        self.normalizer.push(&scratch[..got], monotonic_now_ns())?;
        let mut supplied = false;
        while let Some((chunk, _pts)) = self.normalizer.pop_chunk() {
            self.fifo.extend_from_slice(&chunk);
            supplied = true;
        }
        if supplied {
            self.last_supply = Instant::now();
            if self.fifo.len() > FIFO_MAX_SAMPLES {
                let excess = self.fifo.len() - FIFO_MAX_SAMPLES;
                self.fifo.drain(..excess);
            }
        }
        Ok(())
    }

    /// この側が [`STARVATION_FILL_THRESHOLD`] 以上供給ゼロのままか。
    fn is_starved(&self, now: Instant) -> bool {
        now.duration_since(self.last_supply) >= STARVATION_FILL_THRESHOLD
    }
}

/// system レーン用の微リサンプラ（線形補間ステッチャ）。
///
/// mic を基準クロックとし（録音の時間軸は人の声＝マイク側に合わせるのが自然）、
/// system の FIFO だけを比率 r 倍の速度で線形補間しながら読み出して、子クロック間
/// のレート差を吸収する。読み出し位置の小数部だけを状態として持つ。
struct LinearStitcher {
    /// FIFO 先頭フレームから見た読み出し位置の小数部（フレーム単位、[0, 1)）。
    frac: f64,
}

impl LinearStitcher {
    fn new() -> Self {
        Self { frac: 0.0 }
    }

    /// FIFO に `fifo_frames` フレームあるとき、比率 `ratio` で補間生成できる出力
    /// フレーム数。k 番目（0 始まり）の出力は位置 `frac + k×ratio` の左右 2 フレーム
    /// から作るので、位置が最終フレーム F-1 を超えない k までしか作れない
    /// （最終フレームちょうどに乗る分は重み 0 なので作れる）。
    fn producible(&self, fifo_frames: usize, ratio: f64) -> usize {
        if fifo_frames == 0 {
            return 0;
        }
        let span = (fifo_frames - 1) as f64 - self.frac;
        if span < 0.0 {
            return 0;
        }
        (span / ratio) as usize + 1
    }

    /// system FIFO から `out_frames` フレームを `ratio` 倍速の線形補間で読み出し、
    /// interleaved のまま `out` へ書き足す。読み終えたフレームは FIFO から捨て、
    /// 端数位置は次回へ持ち越す（ブロック境界をまたいでも位相が連続する）。
    /// 呼び出し側は `out_frames <= producible(...)` を保証すること。
    fn pull(&mut self, fifo: &mut Vec<f32>, ratio: f64, out_frames: usize, out: &mut Vec<f32>) {
        let ch = CHANNELS as usize;
        let frames = fifo.len() / ch;
        debug_assert!(out_frames <= self.producible(frames, ratio));
        for k in 0..out_frames {
            let pos = self.frac + k as f64 * ratio;
            let left = pos as usize;
            // 位置がちょうど最終フレームに乗ったときだけ右端をクランプ
            // （そのとき重みは 0 なので補間結果は変わらない）。
            let right = (left + 1).min(frames - 1);
            let w = (pos - left as f64) as f32;
            for c in 0..ch {
                let a = fifo[left * ch + c];
                let b = fifo[right * ch + c];
                out.push(a + w * (b - a));
            }
        }
        // 次の読み出し位置。整数部のフレームは読み終わったので捨て、小数部だけ残す。
        let end = self.frac + out_frames as f64 * ratio;
        let consumed = (end as usize).min(frames);
        self.frac = end - consumed as f64;
        fifo.drain(..consumed * ch);
    }

    /// 飢餓経路で system FIFO を丸ごと吐き出したときの位相の仕切り直し。
    fn reset(&mut self) {
        self.frac = 0.0;
    }
}

/// 残量差から読み出し比率 r を決めるフィードバック制御（EMA + P 制御 + slew）。
///
/// 合成が [`DRIFT_UPDATE_INTERVAL_SAMPLES`] 進むごとに、消費後の FIFO 残量差
/// （mic - system）の指数移動平均を取り、差が負（system 側が溜まる）なら r を
/// 上げて速く消費し、正なら下げる。P 制御で足りるのは、残量差自体がレート差の
/// 積分だから（比例で押し返せば残量差は有限のところで釣り合う）。
struct DriftController {
    /// 現在の読み出し比率 r（1.0 中心に ±[`DRIFT_RATIO_LIMIT`] でクランプ）。
    ratio: f64,
    /// 残量差 (mic_len - system_len) の指数移動平均（f32 サンプル数）。
    ema_diff: f64,
    /// 前回の見直しからの合成出力サンプル数。
    pending_samples: usize,
}

impl DriftController {
    fn new() -> Self {
        Self {
            ratio: 1.0,
            ema_diff: 0.0,
            pending_samples: 0,
        }
    }

    /// 合成出力が進んだ量を記録し、[`DRIFT_UPDATE_INTERVAL_SAMPLES`] に達したら
    /// 比率を 1 回見直す。残量は消費後の値を渡すこと（消費前だと今回消費する分
    /// まで差に見えてしまう）。
    fn on_output(&mut self, samples: usize, mic_len: usize, system_len: usize) {
        self.pending_samples += samples;
        if self.pending_samples >= DRIFT_UPDATE_INTERVAL_SAMPLES {
            self.pending_samples = 0;
            self.update(mic_len, system_len);
        }
    }

    /// 残量差の EMA を更新し、P 制御 + slew で比率を 1 段階動かす。
    fn update(&mut self, mic_len: usize, system_len: usize) {
        let diff = mic_len as f64 - system_len as f64;
        self.ema_diff += DRIFT_EMA_ALPHA * (diff - self.ema_diff);
        let target = (1.0 - DRIFT_GAIN * self.ema_diff)
            .clamp(1.0 - DRIFT_RATIO_LIMIT, 1.0 + DRIFT_RATIO_LIMIT);
        let step = (target - self.ratio).clamp(-DRIFT_SLEW_PER_UPDATE, DRIFT_SLEW_PER_UPDATE);
        self.ratio += step;
    }
}

/// ドリフト補正一式（ステッチャ + 比率コントローラ）。合成スレッドごとに 1 つ持つ。
struct DriftCorrection {
    stitcher: LinearStitcher,
    controller: DriftController,
}

impl DriftCorrection {
    fn new() -> Self {
        Self {
            stitcher: LinearStitcher::new(),
            controller: DriftController::new(),
        }
    }
}

/// 子を 1 つ起動する: 専用の子 RawRing（stream.rs と同じ容量）を作り、子ネイティブ
/// フォーマットの [`RawSink`] で `start` する。成功で [`ChildLane`] を返す。
///
/// 子の `start` の panic は catch_unwind で [`Error::Backend`] へ変換する
/// （stream.rs の start_backend_catching と同じ趣旨。合成スレッドや呼び出し側を
/// 連鎖 panic させない）。
fn start_child(child: &mut Box<dyn CaptureBackend>) -> Result<ChildLane> {
    let (rate, channels) = child.native_format();
    if rate == 0 || channels == 0 {
        return Err(Error::InvalidArg(
            "mix child native_format must have non-zero rate and channels".into(),
        ));
    }
    let (producer, consumer) = raw_ring(RAW_RING_SAMPLES);
    let sink = RawSink::new(producer, rate, channels);
    match std::panic::catch_unwind(AssertUnwindSafe(|| child.start(sink))) {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(e),
        Err(_) => return Err(Error::Backend("mix child panicked during start()".into())),
    }
    // 子ネイティブ → 内部正規形（48k/stereo）。この Normalizer の出力を内部正規形に
    // 固定するので、第 2 段は常にパススルー。
    let normalizer = Normalizer::new(
        rate,
        channels,
        OutputFormat {
            sample_rate: SAMPLE_RATE,
            channels: CHANNELS,
        },
    )
    .inspect_err(|_| {
        // 正規化器が作れないなら子を止めてから失敗を返す（起動済みの子を残さない）。
        stop_child(child);
    })?;
    Ok(ChildLane {
        consumer,
        normalizer,
        fifo: Vec::with_capacity(FIFO_MAX_SAMPLES),
        last_supply: Instant::now(),
    })
}

/// 子の `stop` を catch_unwind で包んで呼ぶ（panic を巻き上げない。stream.rs の
/// stop_backend_catching と同じ趣旨）。
fn stop_child(child: &mut Box<dyn CaptureBackend>) {
    let _ = std::panic::catch_unwind(AssertUnwindSafe(|| child.stop()));
}

/// 合成スレッド本体。
///
/// まず [`prime_lanes`] で両側の最初の供給が揃うのを待ってから（上限は飢餓閾値）、
/// 各子を取り込み（pop → 正規化 → FIFO）、両側の揃ったフレームを側別ゲインで加算
/// 合成して実 sink へ push する。片側が [`STARVATION_FILL_THRESHOLD`] 以上供給ゼロ
/// なら不足分を無音として続行する（システム側が無音の時間帯も録音は流れ続ける）。
/// 両側とも材料が無ければ [`IDLE_SLEEP`] 眠る。
///
/// 正規化の失敗（理論上の rubato 失敗）はループを終える。以降サンプルが流れなく
/// なるので、Stream のウォッチドッグが失速を検知して backend を再オープンする。
fn run_mixer(
    mut mic: ChildLane,
    mut system: ChildLane,
    mic_gain: f32,
    system_gain: f32,
    mut sink: RawSink,
    stopping: Arc<AtomicBool>,
) {
    // pop 用スクラッチ（子リング容量ぶん）と合成出力スクラッチ。ループ内で再利用する。
    let mut scratch = vec![0.0f32; RAW_RING_SAMPLES];
    let mut mixed: Vec<f32> = Vec::with_capacity(FIFO_MAX_SAMPLES);
    // 子クロック間ドリフト補正の状態（start ごとに新調＝前回録音の比率を引きずらない）。
    let mut drift = DriftCorrection::new();

    // 起動直後は子スレッドの立ち上がりがバラつくため、両側が流れ始めるまで
    // （上限は飢餓閾値）待ってから合成を始める＝録音の頭が片側だけになるのを防ぐ。
    if !prime_lanes(&mut mic, &mut system, &mut scratch, &stopping) {
        return;
    }

    loop {
        if stopping.load(Ordering::SeqCst) {
            break;
        }

        if mic.ingest(&mut scratch).is_err() || system.ingest(&mut scratch).is_err() {
            // 正規化が壊れたら合成を終える（ウォッチドッグの再オープンに委ねる）。
            return;
        }

        let pushed = mix_and_push(
            &mut mic,
            &mut system,
            mic_gain,
            system_gain,
            &mut drift,
            &mut sink,
            &mut mixed,
        );

        if !pushed {
            thread::sleep(IDLE_SLEEP);
        }
    }
}

/// 合成開始前のプライミング。起動直後は子バックエンドのスレッド立ち上がりが
/// バラつくため、両側の FIFO に最初の正規化済みサンプルが届くまで [`IDLE_SLEEP`]
/// でポーリングして待ってから合成を始める。ここを飛ばすと、遅れた側のリングが
/// 空のまま飢餓埋めが発動して録音の頭が片側だけの音になり得る。
///
/// 待ちの上限は [`STARVATION_FILL_THRESHOLD`]。片側が最初から供給ゼロ
/// （例: システム側が何も再生していない）という正当なケースは、既存の飢餓と
/// 同じ時間感覚で見切って合成を始める。停止指示が来たら待ちを打ち切る。
/// 正規化の失敗は `false` を返し、呼び出し側が合成スレッドを終える。
fn prime_lanes(
    mic: &mut ChildLane,
    system: &mut ChildLane,
    scratch: &mut [f32],
    stopping: &AtomicBool,
) -> bool {
    let start = Instant::now();
    while !stopping.load(Ordering::SeqCst) {
        if mic.ingest(scratch).is_err() || system.ingest(scratch).is_err() {
            return false;
        }
        if (!mic.fifo.is_empty() && !system.fifo.is_empty())
            || start.elapsed() >= STARVATION_FILL_THRESHOLD
        {
            break;
        }
        thread::sleep(IDLE_SLEEP);
    }
    true
}

/// 両側の FIFO から合成できる分を取り出し、側別ゲインで加算合成（±1.0 クランプ）して
/// sink へ push する。何か push したら `true`。
///
/// 取り出し量の決め方:
/// - 両側にデータがある（定常経路）→ mic の在庫と system から補間で作れる量の min。
///   mic は等速、system は [`LinearStitcher`] が比率 r 倍速の線形補間で読み出して
///   子クロック間のドリフトを吸収する。r は消費後の残量差から [`DriftController`]
///   が微調整する。
/// - 片側だけデータがあり、もう片側が飢餓（60ms 以上供給ゼロ）→ ある側の全量を
///   無音相手と合成（不足分 0.0 埋め）。補正はこの経路には掛けない（既存の飢餓
///   セマンティクスのまま）。
/// - それ以外（両側空・相手がまだ飢餓でない）→ 何もしない（揃うのを待つ）。
fn mix_and_push(
    mic: &mut ChildLane,
    system: &mut ChildLane,
    mic_gain: f32,
    system_gain: f32,
    drift: &mut DriftCorrection,
    sink: &mut RawSink,
    mixed: &mut Vec<f32>,
) -> bool {
    let ch = CHANNELS as usize;
    let ratio = drift.controller.ratio;
    let steady_frames =
        (mic.fifo.len() / ch).min(drift.stitcher.producible(system.fifo.len() / ch, ratio));
    if steady_frames > 0 {
        // 定常経路: system 側だけを r 倍速の線形補間で読み出してから加算合成する。
        mixed.clear();
        drift
            .stitcher
            .pull(&mut system.fifo, ratio, steady_frames, mixed);
        let count = steady_frames * ch;
        for (i, out) in mixed.iter_mut().enumerate() {
            *out = (mic.fifo[i] * mic_gain + *out * system_gain).clamp(-1.0, 1.0);
        }
        mic.fifo.drain(..count);
        drift
            .controller
            .on_output(count, mic.fifo.len(), system.fifo.len());
        // pts は stream.rs の取り込みと同様、単調 now でよい（sink 側では別途取り回す契約）。
        sink.push(mixed, monotonic_now_ns());
        return true;
    }

    // 飢餓経路（補正なし・既存セマンティクス）。
    let now = Instant::now();
    let (mic_take, system_take) = if !mic.fifo.is_empty() && system.is_starved(now) {
        // system 途絶: mic 全量を出す。補間の右端が来ないまま残った端数
        // （高々 1 フレーム）もここで吐き切り、位相は仕切り直す。
        drift.stitcher.reset();
        (mic.fifo.len(), system.fifo.len())
    } else if mic.fifo.is_empty() && !system.fifo.is_empty() && mic.is_starved(now) {
        drift.stitcher.reset();
        (0, system.fifo.len())
    } else {
        return false;
    };

    let count = mic_take.max(system_take);
    mixed.clear();
    for i in 0..count {
        let m = if i < mic_take { mic.fifo[i] } else { 0.0 };
        let s = if i < system_take { system.fifo[i] } else { 0.0 };
        mixed.push((m * mic_gain + s * system_gain).clamp(-1.0, 1.0));
    }
    mic.fifo.drain(..mic_take);
    system.fifo.drain(..system_take);

    // pts は stream.rs の取り込みと同様、単調 now でよい（sink 側では別途取り回す契約）。
    sink.push(mixed, monotonic_now_ns());
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use flexaudio_core::raw_ring::raw_ring;
    use std::sync::atomic::AtomicU32;

    /// 定振幅（直流）のサンプルを飽和供給するテスト専用の子バックエンド。
    ///
    /// 正弦波（MockBackend）だと 2 ソース合成時に位相の問題が出るため、合成結果を
    /// 決定論的に検証できる直流信号を使う。`feed_for` を指定すると、その時間だけ給餌
    /// してから push を止める（スレッドは生かしたまま）＝片側飢餓の再現用。
    ///
    /// 供給は実時間ペース（10ms sleep）ではなく飽和方式: 子リングが受け取れる限り
    /// 即座に push し続け、満杯で入り切らなければ 1ms だけ譲って再試行する。実時間
    /// ペースだと遅いテスト機の粗い sleep 粒度で供給が遅れ、ミキサーが起きた瞬間に
    /// 片側の FIFO だけ空 → 飢餓埋めで片側だけのチャンクが混ざり、合成値の検証が
    /// スケジューラ依存になってしまう。飽和供給なら両レーンの FIFO はミキサーが
    /// いつ起きても非空で、検証対象を「合成の数学」だけに絞れる（スケジューリング
    /// 耐性そのものは mix_survives_one_side_starvation が担う）。直流なので満杯時の
    /// ドロップや途中までの書き込みは値に影響しない。
    struct ConstBackend {
        sample_rate: u32,
        channels: u16,
        value: f32,
        feed_for: Option<Duration>,
        running: Arc<AtomicBool>,
        handle: Option<JoinHandle<()>>,
    }

    impl ConstBackend {
        fn new(value: f32, feed_for: Option<Duration>) -> Self {
            Self {
                sample_rate: 48_000,
                channels: 2,
                value,
                feed_for,
                running: Arc::new(AtomicBool::new(false)),
                handle: None,
            }
        }
    }

    impl CaptureBackend for ConstBackend {
        fn native_format(&self) -> (u32, u16) {
            (self.sample_rate, self.channels)
        }

        fn start(&mut self, mut sink: RawSink) -> Result<()> {
            if self.running.load(Ordering::SeqCst) {
                return Ok(());
            }
            self.running.store(true, Ordering::SeqCst);
            let running = self.running.clone();
            let sample_rate = self.sample_rate;
            let channels = self.channels as usize;
            let value = self.value;
            let feed_for = self.feed_for;
            let handle = thread::Builder::new()
                .name("flexaudio-const-gen".into())
                .spawn(move || {
                    let frames_per_block = (sample_rate as usize / 100).max(1); // 10ms 相当
                    let block = vec![value; frames_per_block * channels];
                    let start = Instant::now();
                    while running.load(Ordering::SeqCst) {
                        let feeding = feed_for.is_none_or(|d| start.elapsed() < d);
                        if !feeding {
                            // 給餌期間が終わったら以降は何も供給しない（片側飢餓の
                            // 再現）。停止指示だけ見張って眠る。
                            thread::sleep(Duration::from_millis(5));
                            continue;
                        }
                        // 飽和供給: 全量入ったら即座に次を push、満杯で入り切らな
                        // かったら 1ms だけ譲って再試行（push は非ブロッキングで
                        // 入り切らない分を落とす契約。直流なので欠けても無害）。
                        let accepted = sink.push(&block, start.elapsed().as_nanos() as i64);
                        if accepted < block.len() {
                            thread::sleep(Duration::from_millis(1));
                        }
                    }
                })
                .map_err(|e| Error::Backend(format!("spawn const gen thread: {e}")))?;
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

    impl Drop for ConstBackend {
        fn drop(&mut self) {
            self.stop();
        }
    }

    /// `start` が常に Err を返すテスト専用バックエンド。
    struct FailingStartBackend;

    impl CaptureBackend for FailingStartBackend {
        fn native_format(&self) -> (u32, u16) {
            (48_000, 2)
        }
        fn start(&mut self, _sink: RawSink) -> Result<()> {
            Err(Error::Backend("intentional start failure".into()))
        }
        fn stop(&mut self) {}
    }

    /// start / stop の呼び出し回数を共有カウンタへ記録するテスト専用バックエンド。
    struct TrackingBackend {
        starts: Arc<AtomicU32>,
        stops: Arc<AtomicU32>,
    }

    impl CaptureBackend for TrackingBackend {
        fn native_format(&self) -> (u32, u16) {
            (48_000, 2)
        }
        fn start(&mut self, _sink: RawSink) -> Result<()> {
            self.starts.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn stop(&mut self) {
            self.stops.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// composite を組んで start し、実 sink の consumer を返すヘルパ。
    fn start_composite(
        mic: Box<dyn CaptureBackend>,
        system: Box<dyn CaptureBackend>,
        mic_gain: f32,
        system_gain: f32,
    ) -> (CompositeBackend, RawConsumer) {
        let mut be = CompositeBackend::new(mic, system, mic_gain, system_gain);
        assert_eq!(be.native_format(), (48_000, 2), "内部正規形を名乗るはず");
        let (producer, consumer) = raw_ring(RAW_RING_SAMPLES);
        let sink = RawSink::new(producer, 48_000, 2);
        be.start(sink).expect("composite start");
        (be, consumer)
    }

    /// 値比較の許容誤差。直流 2 値の f32 加算の丸めを吸収できれば十分。
    const VALUE_TOL: f32 = 1e-4;

    /// 「実際に出現した」と認める最低サンプル数 = 内部正規形 1 チャンク分
    /// （960 frame × 2ch）。存在保証の床は全体比でなく絶対数で置く: 全体比（例:
    /// 25%）だと、片側飢餓埋めの大きなバースト（FIFO 上限の 48k サンプルが一括で
    /// 出得る）が混ざったときに分母だけ膨らんで割れる＝結局スケジューラ依存に
    /// なってしまう。
    const ONE_CHUNK_SAMPLES: usize = 1_920;

    /// 条件を満たすまで consumer からサンプルを集めるヘルパ。
    ///
    /// `done` は新しく pop できたぶんだけを毎回受け取る（呼び出し側が件数などを
    /// 加算して判定する）。true を返したら全収集分を返す。壁時計の固定窓（「500ms
    /// で N サンプル」）は、負荷でスレッド群がデスケジュールされると窓内の生産量を
    /// 保証できず原理的にフレークするため、「条件到達まで待つ」方式にする。
    /// `max_wait` は極端な負荷でも走り続けないためのハング保険で、超過時は集まった
    /// ぶんを返す（不足は呼び出し側のアサーションが検出する）。
    fn collect_until(
        consumer: &mut RawConsumer,
        max_wait: Duration,
        mut done: impl FnMut(&[f32]) -> bool,
    ) -> Vec<f32> {
        let mut out = Vec::new();
        let mut scratch = vec![0.0f32; RAW_RING_SAMPLES];
        let start = Instant::now();
        loop {
            let got = consumer.pop_slice(&mut scratch);
            out.extend_from_slice(&scratch[..got]);
            if done(&scratch[..got]) || start.elapsed() >= max_wait {
                return out;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// [`collect_until`] の待ち上限。通常環境では条件到達で即抜けるので、これは
    /// 「極端な負荷でスレッドがほとんど走れない」場合のハング防止でしかない。
    const COLLECT_MAX_WAIT: Duration = Duration::from_secs(30);

    /// `v` に一致（誤差 [`VALUE_TOL`]）するサンプル数を数えるヘルパ。
    fn count_near(samples: &[f32], v: f32) -> usize {
        samples
            .iter()
            .filter(|&&s| (s - v).abs() < VALUE_TOL)
            .count()
    }

    /// 全サンプルが「理論上現れうる値の集合」のいずれかに一致することを検証し、
    /// 合成値 `mixed` に一致した個数を返すヘルパ。
    ///
    /// なぜ比率でなく集合判定か: 「定常部の 98% が合成値」のような比率アサーションは、
    /// 負荷で producer / ミキサースレッドが飢餓閾値（60ms）超デスケジュールされると
    /// 飢餓ゼロ埋めや片側値がどの区間にも混ざり得て、閾値をどこに置いてもいつか割れる
    /// （比率は本質的に壁時計依存）。一方、直流ソース＋定数ゲインに対してミキサーが
    /// 出せる値は mixed（両側合成）/ mic 単独（system 飢餓埋め）/ system 単独
    /// （mic 飢餓埋め）/ 0.0（プライミング境界）の 4 つだけで、間違った和・ゲイン
    /// 誤適用・クランプ漏れは必ずこの集合の外の値になる。スケジューラは値の分布を
    /// 動かせても集合の外の値は作れないので、この判定はスケジューラ非依存。
    fn assert_only_allowed_values(
        samples: &[f32],
        mixed: f32,
        mic_only: f32,
        system_only: f32,
    ) -> usize {
        let allowed = [mixed, mic_only, system_only, 0.0];
        let mut mixed_count = 0usize;
        for (i, &s) in samples.iter().enumerate() {
            if (s - mixed).abs() < VALUE_TOL {
                mixed_count += 1;
            } else {
                assert!(
                    allowed.iter().any(|&a| (s - a).abs() < VALUE_TOL),
                    "許容集合 {allowed:?} の外の値（合成の数学の誤り）: samples[{i}] = {s}"
                );
            }
        }
        mixed_count
    }

    /// 既知振幅の直流 2 ソース（0.2 と 0.3）を mic_gain=1.0 / system_gain=2.0 で合成
    /// すると、出力は 0.2×1.0 + 0.3×2.0 = 0.8 になる（48k/stereo 子なので全段
    /// パススルー・値は決定論的）。
    #[test]
    fn mix_sums_two_sources_with_gains() {
        let mic = Box::new(ConstBackend::new(0.2, None));
        let system = Box::new(ConstBackend::new(0.3, None));
        let (mut be, mut consumer) = start_composite(mic, system, 1.0, 2.0);

        // 相応の量 + 合成値 1 チャンク分が出るまで集める（負荷で遅くても待つ）。
        let (mut total, mut mixed) = (0usize, 0usize);
        let samples = collect_until(&mut consumer, COLLECT_MAX_WAIT, |new| {
            total += new.len();
            mixed += count_near(new, 0.8);
            total >= 10_000 && mixed >= ONE_CHUNK_SAMPLES
        });
        be.stop();

        assert!(
            samples.len() >= 10_000,
            "相応のサンプルが出るはず: {}",
            samples.len()
        );
        // 全域・全サンプルで集合判定: 現れてよいのは合成値 0.2*1.0 + 0.3*2.0 = 0.8、
        // 飢餓ゼロ埋め時の片側値 0.2（mic 単独）/ 0.6（system 単独）、プライミング
        // 境界の 0.0 だけ。1 サンプルでも集合外なら合成の数学が壊れている。
        let mixed_count = assert_only_allowed_values(&samples, 0.8, 0.2, 0.6);
        // 存在保証: 合成が実際に起きていること（1 チャンク分の絶対数）。
        assert!(
            mixed_count >= ONE_CHUNK_SAMPLES,
            "合成値 0.8 が相応に出現するはず: {mixed_count}/{}",
            samples.len()
        );
    }

    /// 合成が ±1.0 を超える組合せ（0.8 + 0.8 = 1.6）はクランプされて 1.0 になる。
    #[test]
    fn mix_clamps_sum() {
        let mic = Box::new(ConstBackend::new(0.8, None));
        let system = Box::new(ConstBackend::new(0.8, None));
        let (mut be, mut consumer) = start_composite(mic, system, 1.0, 1.0);

        // クランプされた合成値が 1 チャンク分出るまで集める（負荷で遅くても待つ）。
        let mut clamped = 0usize;
        let samples = collect_until(&mut consumer, COLLECT_MAX_WAIT, |new| {
            clamped += count_near(new, 1.0);
            clamped >= ONE_CHUNK_SAMPLES
        });
        be.stop();

        // クランプの本質: どのサンプルも 1.0 を超えない。
        for (i, &s) in samples.iter().enumerate() {
            assert!(
                s <= 1.0,
                "クランプ後は 1.0 を超えないはず: samples[{i}] = {s}"
            );
        }
        // 全域・全サンプルで集合判定: 現れてよいのは clamp(0.8 + 0.8) = 1.0、
        // 飢餓ゼロ埋め時の片側値 0.8、プライミング境界の 0.0 だけ（クランプ漏れの
        // 1.6 などは集合外として即 FAIL）。
        let clamped_count = assert_only_allowed_values(&samples, 1.0, 0.8, 0.8);
        // 存在保証: クランプされた合成値が実際に出現していること（1 チャンク分）。
        assert!(
            clamped_count >= ONE_CHUNK_SAMPLES,
            "クランプ値 1.0 が相応に出現するはず: {clamped_count}/{}",
            samples.len()
        );
    }

    /// 片側（system）が途中で供給を止めても出力は止まらず、飢餓側を無音 0.0 として
    /// mic 側の音だけで合成が続く（mic 単独値 0.2 が流れ始める）。
    #[test]
    fn mix_survives_one_side_starvation() {
        let mic = Box::new(ConstBackend::new(0.2, None));
        // system は 150ms だけ給餌してから止まる（スレッドは生存）。
        let system = Box::new(ConstBackend::new(0.3, Some(Duration::from_millis(150))));
        let (mut be, mut consumer) = start_composite(mic, system, 1.0, 1.0);

        // system 停止（壁時計 150ms）→ バックログ消化 → 飢餓閾値経過の後、必ず
        // mic 単独値 0.2 が流れ始める。「400ms 後の窓の大半が 0.2」のような壁時計窓
        // の判定は、負荷でバックログ消化が遅れるだけで割れるので、「mic 単独値が
        // 1 チャンク分出るまで待つ」存在保証に置く。
        let mut mic_only = 0usize;
        let samples = collect_until(&mut consumer, COLLECT_MAX_WAIT, |new| {
            mic_only += count_near(new, 0.2);
            mic_only >= ONE_CHUNK_SAMPLES
        });
        be.stop();

        // 集合判定: 現れてよいのは合成値 0.5 / mic 単独 0.2 / system 単独 0.3 /
        // プライミング境界の 0.0 だけ。
        assert_only_allowed_values(&samples, 0.5, 0.2, 0.3);
        // 存在保証: system 停止後も出力は止まらず、飢餓側ゼロ埋めの mic 単独値が
        // 実際に流れること。
        let mic_only_count = count_near(&samples, 0.2);
        assert!(
            mic_only_count >= ONE_CHUNK_SAMPLES,
            "飢餓後は mic 単独の値 0.2 が流れ続けるはず: {mic_only_count}/{}",
            samples.len()
        );
    }

    /// system 子の start が Err を返したら、先に起動した mic 子が stop され、全体も
    /// Err になる（片肺で起動成功にしない）。
    #[test]
    fn mix_start_failure_cleans_up() {
        let starts = Arc::new(AtomicU32::new(0));
        let stops = Arc::new(AtomicU32::new(0));
        let mic = Box::new(TrackingBackend {
            starts: starts.clone(),
            stops: stops.clone(),
        });
        let system = Box::new(FailingStartBackend);

        let mut be = CompositeBackend::new(mic, system, 1.0, 1.0);
        let (producer, _consumer) = raw_ring(RAW_RING_SAMPLES);
        let sink = RawSink::new(producer, 48_000, 2);

        let err = be
            .start(sink)
            .expect_err("system 起動失敗で全体も Err のはず");
        assert!(
            matches!(err, Error::Backend(_)),
            "system 子の Err が伝播するはず: {err:?}"
        );
        assert_eq!(starts.load(Ordering::SeqCst), 1, "mic は一度起動される");
        assert_eq!(
            stops.load(Ordering::SeqCst),
            1,
            "system 失敗時に mic が stop されるはず"
        );
    }

    /// mic 子の start が Err なら system 子には触れず即 Err。stop は冪等で二重に
    /// 呼べる。
    #[test]
    fn mix_mic_start_failure_is_immediate() {
        let starts = Arc::new(AtomicU32::new(0));
        let stops = Arc::new(AtomicU32::new(0));
        let mic = Box::new(FailingStartBackend);
        let system = Box::new(TrackingBackend {
            starts: starts.clone(),
            stops: stops.clone(),
        });

        let mut be = CompositeBackend::new(mic, system, 1.0, 1.0);
        let (producer, _consumer) = raw_ring(RAW_RING_SAMPLES);
        let sink = RawSink::new(producer, 48_000, 2);
        assert!(be.start(sink).is_err(), "mic 起動失敗で即 Err のはず");
        assert_eq!(
            starts.load(Ordering::SeqCst),
            0,
            "mic 失敗なら system は起動されない"
        );

        // stop は未起動でも冪等（子の stop も冪等契約）。
        be.stop();
        be.stop();
    }

    /// 合成バックエンドを実際の [`Stream`](crate::Stream) に載せた end-to-end。
    /// 20ms/960frame のチャンクが流れ、data が合成値（0.2 + 0.3 = 0.5）になる。
    /// Stream 本体無変更で seq・チャンク契約がそのまま効くことの裏取り。
    #[test]
    fn stream_delivers_mixed_chunks_end_to_end() {
        use flexaudio_core::types::StreamConfig;

        let mic = Box::new(ConstBackend::new(0.2, None));
        let system = Box::new(ConstBackend::new(0.3, None));
        let backend = Box::new(CompositeBackend::new(mic, system, 1.0, 1.0));
        let mut stream = crate::Stream::open(StreamConfig::default(), backend).expect("open");
        stream.start().expect("start");

        // 合成値のサンプルが 1 チャンク分届くまでポーリングする（壁時計の固定窓は
        // 負荷でフレークするため、条件到達までの待ち方式。上限はハング保険）。
        let mut chunks = Vec::new();
        let mut mixed = 0usize;
        let deadline = Instant::now() + COLLECT_MAX_WAIT;
        while Instant::now() < deadline && mixed < ONE_CHUNK_SAMPLES {
            while let Some(c) = stream.poll_chunk() {
                mixed += count_near(&c.data, 0.5);
                chunks.push(c);
            }
            thread::sleep(Duration::from_millis(5));
        }
        stream.stop();

        assert!(!chunks.is_empty(), "チャンクが届くはず");
        for (i, c) in chunks.iter().enumerate() {
            assert_eq!(c.frames, 960, "20ms@48k = 960 frame");
            assert_eq!(c.data.len(), 960 * 2, "stereo interleaved");
            if i > 0 {
                assert!(c.seq > chunks[i - 1].seq, "seq は単調増加");
            }
        }
        // 全チャンク・全サンプルで集合判定: 現れてよいのは合成値 0.2 + 0.3 = 0.5、
        // 飢餓ゼロ埋め時の片側値 0.2（mic 単独）/ 0.3（system 単独）、プライミング
        // 境界の 0.0 だけ。Stream の第 1 段は 48k/stereo でパススルー（gain 1.0 は
        // バイト無変更）なので、ミキサー出力の値がそのまま届く。
        let all: Vec<f32> = chunks.iter().flat_map(|c| c.data.iter().copied()).collect();
        let mixed_count = assert_only_allowed_values(&all, 0.5, 0.2, 0.3);
        // 存在保証: 合成が実際に起きていること（1 チャンク分の絶対数）。
        assert!(
            mixed_count >= ONE_CHUNK_SAMPLES,
            "合成値 0.5 が相応に出現するはず: {mixed_count}/{}",
            all.len()
        );
    }

    // ---- ドリフト補正のコンポーネント単体（純粋・決定論） ----

    /// system 側が溜まる（mic - system が負）と r は 1.0 より上（速く消費する方向）
    /// へ、mic 側が溜まると 1.0 より下へ動く。
    #[test]
    fn drift_controller_moves_toward_lagging_side() {
        let mut c = DriftController::new();
        for _ in 0..50 {
            c.update(0, 9_600);
        }
        assert!(
            c.ratio > 1.0,
            "system 側が溜まると r > 1.0 のはず: {}",
            c.ratio
        );

        let mut c = DriftController::new();
        for _ in 0..50 {
            c.update(9_600, 0);
        }
        assert!(
            c.ratio < 1.0,
            "mic 側が溜まると r < 1.0 のはず: {}",
            c.ratio
        );
    }

    /// どれだけ大きな残量差を与え続けても r は 1.0 ± 500ppm で頭打ちになる。
    #[test]
    fn drift_controller_clamps_at_ratio_limit() {
        let mut c = DriftController::new();
        for _ in 0..1_000 {
            c.update(0, 10_000_000);
        }
        assert!(
            (c.ratio - (1.0 + DRIFT_RATIO_LIMIT)).abs() < 1e-12,
            "上側クランプちょうどで止まるはず: {}",
            c.ratio
        );

        let mut c = DriftController::new();
        for _ in 0..1_000 {
            c.update(10_000_000, 0);
        }
        assert!(
            (c.ratio - (1.0 - DRIFT_RATIO_LIMIT)).abs() < 1e-12,
            "下側クランプちょうどで止まるはず: {}",
            c.ratio
        );
    }

    /// 巨大な残量差を一撃で与えても、1 回の更新で動けるのは slew 幅まで。
    #[test]
    fn drift_controller_slew_limits_change_per_update() {
        let mut c = DriftController::new();
        c.update(0, 10_000_000);
        assert!(
            (c.ratio - (1.0 + DRIFT_SLEW_PER_UPDATE)).abs() < 1e-12,
            "1 回目の更新は slew 幅ちょうどで頭打ちのはず: {}",
            c.ratio
        );
        c.update(0, 10_000_000);
        assert!(
            (c.ratio - (1.0 + 2.0 * DRIFT_SLEW_PER_UPDATE)).abs() < 1e-12,
            "2 回目も 1 段階ずつ: {}",
            c.ratio
        );

        let mut c = DriftController::new();
        c.update(10_000_000, 0);
        assert!(
            (c.ratio - (1.0 - DRIFT_SLEW_PER_UPDATE)).abs() < 1e-12,
            "反対向きも slew 幅ちょうど: {}",
            c.ratio
        );
    }

    /// on_output は合成 100ms 分（更新間隔）たまるまでは比率を見直さない。
    #[test]
    fn drift_controller_updates_only_at_interval() {
        let mut c = DriftController::new();
        c.on_output(DRIFT_UPDATE_INTERVAL_SAMPLES - 1, 0, 10_000_000);
        assert!(
            (c.ratio - 1.0).abs() < 1e-15,
            "間隔未満では動かないはず: {}",
            c.ratio
        );
        c.on_output(1, 0, 10_000_000);
        assert!(c.ratio > 1.0, "間隔に達したら見直すはず: {}", c.ratio);
    }

    /// 等速（r = 1.0・位相 0）ならステッチャは完全なパススルー: 入力と同じ値が
    /// そのまま出て、FIFO は全量消費され、位相も 0 のまま。
    #[test]
    fn stitcher_unity_ratio_is_passthrough() {
        let mut st = LinearStitcher::new();
        let src: Vec<f32> = (0..10).flat_map(|f| [f as f32, -(f as f32)]).collect();
        let mut fifo = src.clone();
        assert_eq!(st.producible(10, 1.0), 10);
        let mut out = Vec::new();
        st.pull(&mut fifo, 1.0, 10, &mut out);
        assert_eq!(out, src, "r=1.0 はパススルーのはず");
        assert!(fifo.is_empty(), "全量消費されるはず: {} 残り", fifo.len());
        assert!(st.frac.abs() < 1e-12, "位相は 0 のまま: {}", st.frac);
    }

    /// ランプ（frame k の値 = k）を r = 1.25 で読むと、出力は位置 0 / 1.25 / 2.5 /
    /// 3.75 の線形補間値そのものになる（両チャンネルとも・決定論）。
    #[test]
    fn stitcher_interpolates_between_frames() {
        let mut st = LinearStitcher::new();
        let mut fifo: Vec<f32> = (0..5).flat_map(|f| [f as f32, f as f32 * 10.0]).collect();
        // span = 4, floor(4 / 1.25) = 3 → 3 + 1 = 4 フレーム作れる。
        assert_eq!(st.producible(5, 1.25), 4);
        let mut out = Vec::new();
        st.pull(&mut fifo, 1.25, 4, &mut out);
        let expect = [0.0f32, 1.25, 2.5, 3.75];
        for (k, &e) in expect.iter().enumerate() {
            assert!(
                (out[k * 2] - e).abs() < 1e-6,
                "位置 {e} の補間値: {}",
                out[k * 2]
            );
            assert!(
                (out[k * 2 + 1] - e * 10.0).abs() < 1e-5,
                "ch2 も同じ位置で補間: {}",
                out[k * 2 + 1]
            );
        }
        // floor(0 + 4×1.25) = 5 で全フレーム読み終わり、位相は 0 に戻る。
        assert!(fifo.is_empty(), "全量消費されるはず: {} 残り", fifo.len());
        assert!(st.frac.abs() < 1e-12, "位相: {}", st.frac);
    }

    // ---- スレッド無しの同期ドリフトシミュレーション（決定論） ----

    /// スレッドを立てない同期シミュレーション用の ChildLane。リング・正規化器は
    /// 形として持つだけで、供給はテストが [`sim_feed`] で FIFO へ直接行う。
    fn sim_lane() -> ChildLane {
        let (_producer, consumer) = raw_ring(16);
        let normalizer = Normalizer::new(
            SAMPLE_RATE,
            CHANNELS,
            OutputFormat {
                sample_rate: SAMPLE_RATE,
                channels: CHANNELS,
            },
        )
        .expect("パススルー正規化器");
        ChildLane {
            consumer,
            normalizer,
            fifo: Vec::new(),
            last_supply: Instant::now(),
        }
    }

    /// ingest と同じ流儀（FIFO へ積む + 上限の安全弁）で直流フレームを直接供給する。
    fn sim_feed(lane: &mut ChildLane, value: f32, frames: usize) {
        let new_len = lane.fifo.len() + frames * CHANNELS as usize;
        lane.fifo.resize(new_len, value);
        if lane.fifo.len() > FIFO_MAX_SAMPLES {
            let excess = lane.fifo.len() - FIFO_MAX_SAMPLES;
            lane.fifo.drain(..excess);
        }
        lane.last_supply = Instant::now();
    }

    /// 同期ドリフトシミュレーションの計測結果。
    struct DriftSimOutcome {
        /// 1 シミュレーション秒ごとの消費後 FIFO 残量（f32 サンプル数）。
        system_backlog: Vec<usize>,
        mic_backlog: Vec<usize>,
        final_ratio: f64,
    }

    /// スレッドを立てない同期シミュレーション。20ms を 1 tick として、mic に等速
    /// （960 frame/tick）、system に (1 + ppm×1e-6) 倍レートの直流を「時間の進みを
    /// サンプル数で模擬」しながら供給し、mix_and_push を直接ループで駆動する。
    /// スレッド・壁時計に依存しないので完全に決定論。
    ///
    /// `fixed_unity_ratio` はコントローラを毎 tick 1.0 に戻す「補正なし」相当のパス。
    /// ドリフト問題（残量の単調増大）がこのシミュレーションで実際に再現できている
    /// ことの自己検証に使う。
    ///
    /// 出力値は毎 tick 全数検証する: 両側とも常に供給があるので、現れてよいのは
    /// 合成値（0.2 + 0.3 = 0.5。直流の線形補間は同じ直流値）だけ。飢餓ゼロ埋めの
    /// 0.0 や片側値が 1 サンプルでも出たら即 FAIL（「ゼロ埋めが定常発生しない」
    /// ことの検証を兼ねる）。
    fn run_drift_sim(ppm: f64, seconds: usize, fixed_unity_ratio: bool) -> DriftSimOutcome {
        const TICK_FRAMES: usize = 960; // 20ms @48k
        const TICKS_PER_SEC: usize = 50;

        let mut mic = sim_lane();
        let mut system = sim_lane();
        let mut drift = DriftCorrection::new();
        let (producer, mut consumer) = raw_ring(RAW_RING_SAMPLES);
        let mut sink = RawSink::new(producer, SAMPLE_RATE, CHANNELS);
        let mut mixed: Vec<f32> = Vec::with_capacity(FIFO_MAX_SAMPLES);
        let mut scratch = vec![0.0f32; RAW_RING_SAMPLES];

        // system 供給フレーム数の端数繰り越し（レート差をサンプル数で正確に模擬）。
        let mut system_carry = 0.0f64;
        let mut outcome = DriftSimOutcome {
            system_backlog: Vec::new(),
            mic_backlog: Vec::new(),
            final_ratio: 1.0,
        };

        for tick in 0..seconds * TICKS_PER_SEC {
            sim_feed(&mut mic, 0.2, TICK_FRAMES);
            system_carry += TICK_FRAMES as f64 * (1.0 + ppm * 1e-6);
            let system_frames = system_carry as usize;
            system_carry -= system_frames as f64;
            sim_feed(&mut system, 0.3, system_frames);

            mix_and_push(
                &mut mic,
                &mut system,
                1.0,
                1.0,
                &mut drift,
                &mut sink,
                &mut mixed,
            );
            if fixed_unity_ratio {
                drift.controller.ratio = 1.0;
                drift.controller.ema_diff = 0.0;
            }

            let got = consumer.pop_slice(&mut scratch);
            for &s in &scratch[..got] {
                assert!(
                    (s - 0.5).abs() < VALUE_TOL,
                    "定常シミュレーションの出力は合成値 0.5 だけのはず（ゼロ埋め・片側値は \
                     出ない）: {s}"
                );
            }

            if (tick + 1) % TICKS_PER_SEC == 0 {
                outcome.system_backlog.push(system.fifo.len());
                outcome.mic_backlog.push(mic.fifo.len());
            }
        }
        outcome.final_ratio = drift.controller.ratio;
        outcome
    }

    /// +300ppm（system が速い）を 60 秒: 補正ありでは system FIFO 残量が安全弁に
    /// 届かず、伸びが減速し、補正なし相当より小さく収まる。補正なし相当（r = 1.0
    /// 固定）では同条件で残量が単調増大する＝テスト自体がドリフト問題を再現できて
    /// いることの自己検証。
    #[test]
    fn mix_drift_sim_plus_300ppm_stays_bounded() {
        let corrected = run_drift_sim(300.0, 60, false);
        let uncorrected = run_drift_sim(300.0, 60, true);

        // 自己検証: 補正なしでは system 残量が毎秒単調増大（+300ppm ≒ +28.8 サンプル/秒）。
        for (i, w) in uncorrected.system_backlog.windows(2).enumerate() {
            assert!(
                w[1] > w[0],
                "補正なしでは残量が単調増大するはず: {i} 秒目 {} → {}",
                w[0],
                w[1]
            );
        }

        // 補正あり: 残量は安全弁（FIFO_MAX_SAMPLES = 500ms 分）に到達しない。
        let max_corrected = corrected.system_backlog.iter().copied().max().unwrap();
        assert!(
            max_corrected < FIFO_MAX_SAMPLES,
            "補正ありなら安全弁に届かないはず: 最大 {max_corrected}"
        );

        // 補正あり: 補正なしより小さく収まる（補正が実際に効いている）。
        let last_c = *corrected.system_backlog.last().unwrap();
        let last_u = *uncorrected.system_backlog.last().unwrap();
        assert!(
            last_c < last_u,
            "補正ありの残量 {last_c} < 補正なし {last_u} のはず"
        );

        // 補正あり: 残量の伸びが減速している（最初の 9 秒間の増分 > 最後の 9 秒間の増分）。
        let sb = &corrected.system_backlog;
        let early = sb[9] - sb[0];
        let late = sb[59] - sb[50];
        assert!(
            late < early,
            "比率が追い付くにつれ伸びが減速するはず: 序盤 +{early} vs 終盤 +{late}"
        );

        // 比率は「system を速く消費する」方向へ動き、クランプ内に収まる。
        assert!(
            corrected.final_ratio > 1.0 + 2e-5,
            "r は 1.0 より上へ動くはず: {}",
            corrected.final_ratio
        );
        assert!(
            corrected.final_ratio <= 1.0 + DRIFT_RATIO_LIMIT + 1e-12,
            "r はクランプ内: {}",
            corrected.final_ratio
        );
    }

    /// -300ppm（system が遅い）を 60 秒: 飢餓ゼロ埋めは定常発生せず（出力値検証は
    /// run_drift_sim 内で毎 tick 全数実施）、補正ありでは mic 側の残量が安全弁に
    /// 届かず補正なし相当より小さく収まる。
    #[test]
    fn mix_drift_sim_minus_300ppm_no_steady_zero_fill() {
        let corrected = run_drift_sim(-300.0, 60, false);
        let uncorrected = run_drift_sim(-300.0, 60, true);

        // 自己検証: 補正なしでは遅い system に合わせるぶん mic 側の残量が単調増大。
        for (i, w) in uncorrected.mic_backlog.windows(2).enumerate() {
            assert!(
                w[1] > w[0],
                "補正なしでは mic 残量が単調増大するはず: {i} 秒目 {} → {}",
                w[0],
                w[1]
            );
        }

        // 補正あり: mic 残量は安全弁に到達せず、補正なしより小さく収まる。
        let max_corrected = corrected.mic_backlog.iter().copied().max().unwrap();
        assert!(
            max_corrected < FIFO_MAX_SAMPLES,
            "補正ありなら安全弁に届かないはず: 最大 {max_corrected}"
        );
        let last_c = *corrected.mic_backlog.last().unwrap();
        let last_u = *uncorrected.mic_backlog.last().unwrap();
        assert!(
            last_c < last_u,
            "補正ありの mic 残量 {last_c} < 補正なし {last_u} のはず"
        );

        // system 側は消費が供給に追随するので溜まらない（高々補間の端数 + 直近の
        // 端数チャンク程度）。
        let max_sys = corrected.system_backlog.iter().copied().max().unwrap();
        assert!(
            max_sys < ONE_CHUNK_SAMPLES,
            "system 側は溜まらないはず: 最大 {max_sys}"
        );

        // 比率は「system をゆっくり消費する」方向へ動き、クランプ内に収まる。
        assert!(
            corrected.final_ratio < 1.0 - 2e-5,
            "r は 1.0 より下へ動くはず: {}",
            corrected.final_ratio
        );
        assert!(
            corrected.final_ratio >= 1.0 - DRIFT_RATIO_LIMIT - 1e-12,
            "r はクランプ内: {}",
            corrected.final_ratio
        );
    }
}
