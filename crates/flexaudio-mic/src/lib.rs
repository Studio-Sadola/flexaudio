//! flexaudio-mic — cpal によるマイク入力バックエンド（全 OS）。
//!
//! [`CpalMicBackend`] は cpal で入力デバイスから生 interleaved `f32` フレームを取り、
//! [`RawSink`] へ非ブロッキングに push する [`CaptureBackend`] 実装。検証は主に
//! Linux/ALSA だが、cpal が対応する OS なら動く。
//!
//! # `cpal::Stream` が `!Send` なので所有スレッドへ閉じ込める
//! [`CaptureBackend`] は `Send` を要求するが [`cpal::Stream`] は `!Send` なので、
//! backend 構造体に直接持てない。[`start`](CpalMicBackend::start) でスレッドを
//! spawn し、その中で stream を build + `play()` して停止シグナルまで `park` する。
//! 停止時にそのスレッドが Stream を drop してキャプチャが止まる。構造体自身が持つのは
//! `Send` なもの（停止フラグ・[`JoinHandle`]・キャッシュ済みフォーマット）だけ。
//!
//! ```no_run
//! use flexaudio_mic::CpalMicBackend;
//! use flexaudio_core::{CaptureBackend, RawSink, raw_ring};
//!
//! // 既定入力デバイス（device_id = None）。特定デバイスを選ぶなら
//! // `CpalMicBackend::new(Some("デバイス名".into()))`（id = デバイス名）。
//! let mut backend = CpalMicBackend::new(None);
//! let (rate, channels) = backend.native_format();
//! let (prod, _cons) = raw_ring(rate as usize * channels as usize); // 1 秒ぶん
//! let sink = RawSink::new(prod, rate, channels);
//! backend.start(sink).unwrap();
//! // ... _cons から生フレームを pop ...
//! backend.stop();
//! ```

#![warn(missing_docs)]

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, SampleFormat};

use flexaudio_core::backend::{CaptureBackend, RawSink};
use flexaudio_core::clock::monotonic_now_ns;
use flexaudio_core::types::{DeviceInfo, Error, Result, SourceKind};

/// 入力デバイスが取れないとき [`native_format`](CpalMicBackend::native_format) が
/// 返す既定フォーマット `(48000 Hz, mono)`。`start` 時にデバイスが無ければ
/// [`Error::DeviceNotFound`] になる。
const FALLBACK_FORMAT: (u32, u16) = (48_000, 1);

/// cpal によるマイク入力キャプチャバックエンド。
///
/// 既定入力デバイス（`device_id = None`）か、デバイス名で選んだ入力デバイス
/// （`device_id = Some(id)`）から生 interleaved `f32` フレームを取り [`RawSink`] へ
/// 流す。詳細はモジュールドキュメント参照。
///
/// `Send`。持つのは停止フラグ・[`JoinHandle`]・キャッシュ済みフォーマット・
/// device_id だけで、`!Send` な [`cpal::Stream`] は所有スレッド内に閉じ込める。
pub struct CpalMicBackend {
    /// 所有スレッドへの停止指示。`true` で stream を drop して終了する。
    stop_flag: Arc<AtomicBool>,
    /// cpal stream を所有するスレッドのハンドル（start 後に `Some`）。
    handle: Option<JoinHandle<()>>,
    /// `new` 時に問い合わせてキャッシュしたネイティブフォーマット。
    native: (u32, u16),
    /// 選択する入力デバイスの ID（デバイス名）。`None` で既定入力デバイス。
    device_id: Option<String>,
}

impl CpalMicBackend {
    /// マイクバックエンドを構築する。
    ///
    /// `device_id`:
    /// - `None` → 既定入力デバイス（`host.default_input_device()`）。
    /// - `Some(id)` → `host.input_devices()` を走査し `device.name()? == id` の最初の
    ///   デバイス（id は [`list_devices`] が返すデバイス名）。
    ///
    /// 選んだデバイスのネイティブフォーマットを問い合わせてキャッシュする。デバイスが
    /// 無い／一致しない／問い合わせ失敗なら `FALLBACK_FORMAT`（`(48000, 1)`）を
    /// キャッシュする。new 自体は panic もエラーもせず必ず成功し、device_id が一致
    /// しなければ [`start`](Self::start) で [`Error::DeviceNotFound`] になる。
    pub fn new(device_id: Option<String>) -> Self {
        let native = query_native_format(device_id.as_deref()).unwrap_or(FALLBACK_FORMAT);
        Self {
            stop_flag: Arc::new(AtomicBool::new(false)),
            handle: None,
            native,
            device_id,
        }
    }
}

impl Default for CpalMicBackend {
    fn default() -> Self {
        Self::new(None)
    }
}

/// `device_id` の入力デバイスを cpal ホストから解決する。
///
/// - `None` → `host.default_input_device()`（取れなければ [`Error::DeviceNotFound`]）。
/// - `Some(id)` → `host.input_devices()` を走査し `device.name()? == id` の最初の
///   一致を返す。無ければ [`Error::DeviceNotFound`]。
///
/// 名前が取れないデバイスは比較できないのでスキップ。`input_devices()` 自体が失敗
/// する環境（ALSA 不在等）も [`Error::DeviceNotFound`] に写す。
fn resolve_input_device(host: &cpal::Host, device_id: Option<&str>) -> Result<Device> {
    match device_id {
        None => host.default_input_device().ok_or(Error::DeviceNotFound),
        // デバイス名で一致する最初のデバイス。
        Some(id) => {
            let devices = host.input_devices().map_err(|_| Error::DeviceNotFound)?;
            for device in devices {
                // 名前が取れないデバイスは比較できないのでスキップ。
                if let Ok(name) = device.name() {
                    if name == id {
                        return Ok(device);
                    }
                }
            }
            Err(Error::DeviceNotFound)
        }
    }
}

/// `device_id` で選択した入力デバイスのネイティブフォーマット
/// `(sample_rate, channels)` を取得する。デバイス解決／設定取得に失敗すれば `None`
/// （呼び元が [`FALLBACK_FORMAT`] へ落とす）。
fn query_native_format(device_id: Option<&str>) -> Option<(u32, u16)> {
    let host = cpal::default_host();
    let device = resolve_input_device(&host, device_id).ok()?;
    let config = device.default_input_config().ok()?;
    Some((config.sample_rate().0, config.channels()))
}

/// 入力（マイク）デバイスを列挙する。`devices()` のマイク分。
///
/// `host.input_devices()` を走査し各デバイスを [`DeviceInfo`] へ写す:
/// - `id` / `name`: cpal は永続 ID を持たないので device name を ID 代わりに両方へ
///   入れる（再接続で index が変わるため。同一構成なら同じ name）。
/// - `sample_rate` / `channels`: `default_input_config()` から。取れない（実際には
///   開けない等）デバイスはスキップ。
/// - `source_kind = Mic` / `is_loopback = false`。
/// - `is_default`: `host.default_input_device()` の name と一致すれば `true`。
///
/// デバイスが無い／ホスト初期化失敗の環境では空 `Vec`（panic しない）。同名デバイス
/// が複数あると id が重複し得るが、cpal でこれ以上安定なキーは取れないので許容する。
pub fn list_devices() -> Result<Vec<DeviceInfo>> {
    let host = cpal::default_host();

    // 既定入力デバイス名（is_default 判定用）。取れなければ既定一致は付かない。
    let default_name = host.default_input_device().and_then(|d| d.name().ok());

    // input_devices() 自体が失敗する環境（ALSA 不在等）は空リスト扱い。
    let devices = match host.input_devices() {
        Ok(it) => it,
        Err(_) => return Ok(Vec::new()),
    };

    let mut out = Vec::new();
    for device in devices {
        // name が取れないデバイスは ID を作れないのでスキップ。
        let Ok(name) = device.name() else {
            continue;
        };
        // 既定入力 config が取れない＝広告されていても実際には開けない。スキップ。
        let Ok(config) = device.default_input_config() else {
            continue;
        };
        let is_default = default_name.as_deref() == Some(name.as_str());
        out.push(DeviceInfo {
            id: name.clone(),
            name,
            source_kind: SourceKind::Mic,
            sample_rate: config.sample_rate().0,
            channels: config.channels(),
            is_loopback: false,
            is_default,
        });
    }
    Ok(out)
}

impl CaptureBackend for CpalMicBackend {
    fn native_format(&self) -> (u32, u16) {
        self.native
    }

    fn start(&mut self, sink: RawSink) -> Result<()> {
        // 既に所有スレッドが生きていれば何もしない（二重 start に安全）。
        if self.handle.is_some() {
            return Ok(());
        }
        // 前回の stop 後でも再 start できるようフラグをリセット。
        self.stop_flag.store(false, Ordering::SeqCst);

        let stop_flag = self.stop_flag.clone();
        // cpal::Device は !Send なので、device_id 文字列だけ渡してスレッド内で解決する。
        let device_id = self.device_id.clone();
        // build/play の成否を所有スレッドから start() へ返す ready channel。
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();

        let handle = thread::Builder::new()
            .name("flexaudio-mic-cpal".into())
            .spawn(move || {
                run_capture_thread(sink, device_id, stop_flag, ready_tx);
            })
            .map_err(|e| Error::Backend(format!("spawn cpal mic thread: {e}")))?;

        // 所有スレッドが stream を build + play できたか待つ。
        match ready_rx.recv() {
            Ok(Ok(())) => {
                self.handle = Some(handle);
                Ok(())
            }
            Ok(Err(e)) => {
                // build/play 失敗。所有スレッドは ready 送信後に即終了するので join。
                let _ = handle.join();
                Err(e)
            }
            // ready 送信前に所有スレッドが死んだ（通常ありえない）。
            Err(_) => {
                let _ = handle.join();
                Err(Error::Backend(
                    "cpal mic thread exited before reporting readiness".into(),
                ))
            }
        }
    }

    fn stop(&mut self) {
        // handle が無ければ何もしない（再入・二重 stop に安全）。
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            // 所有スレッドは park 中。unpark で起こし、Stream を drop させて終了。
            h.thread().unpark();
            let _ = h.join();
        }
    }
}

impl Drop for CpalMicBackend {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 所有スレッド本体。cpal input stream を build + play し、停止まで park する。
///
/// build/play の成否を `ready_tx` で [`CpalMicBackend::start`] へ報告する。成功後は
/// `stop_flag` が立つまで park して `stream` を生かし、立ったら関数を抜けて
/// `stream` を drop することでキャプチャを止める。
fn run_capture_thread(
    sink: RawSink,
    device_id: Option<String>,
    stop_flag: Arc<AtomicBool>,
    ready_tx: mpsc::Sender<Result<()>>,
) {
    let stream = match build_stream(sink, device_id.as_deref()) {
        Ok(s) => s,
        Err(e) => {
            // 失敗を報告して即終了。
            let _ = ready_tx.send(Err(e));
            return;
        }
    };

    if let Err(e) = stream.play() {
        let _ = ready_tx.send(Err(Error::Backend(format!("cpal play: {e}"))));
        return;
    }

    // ここまで来れば起動成功。
    let _ = ready_tx.send(Ok(()));

    // stop シグナルまで stream を生かしたまま park する。
    // 偽の wakeup に備え stop_flag を毎回確認する。
    while !stop_flag.load(Ordering::SeqCst) {
        thread::park();
    }
    // ここを抜けると stream が drop されキャプチャが停止する。
    drop(stream);
}

/// RT 変換コールバックのスクラッチを事前確保するときに見込む最大ブロック長（秒）。
///
/// 1 コールバックの最大フレーム数を、ネイティブ SR×ch × この秒数で見積もって
/// stream セットアップ時に確保する。実機のブロックは通常 数 ms〜数十 ms なので
/// 1 秒ぶんあれば定常状態で容量拡大（= RT 内アロケート）は起きない。想定を超える
/// 巨大ブロックが来ても、[`fill_scratch`] が一度だけ `reserve` で広げて以後その容量
/// を保つ（panic しない）。
const MAX_SCRATCH_SECONDS: usize = 1;

/// 変換経路（I16/U16/I32）の RT コールバックで、interleaved 入力を変換しながら
/// 事前確保済みスクラッチへ詰める。
///
/// `scratch` は stream セットアップ時に最大ブロック長で確保済み。定常状態では容量内
/// なので `clear` + `push` は再確保を起こさない。`n` が容量を超えたときだけ一度
/// `reserve` で広げ、以後その容量を保つ。`convert` を各サンプルへ適用する。
#[inline]
fn fill_scratch<T: Copy>(scratch: &mut Vec<f32>, data: &[T], convert: impl Fn(T) -> f32) {
    let n = data.len();
    // 容量内なら reserve は何もしない。容量超のときだけ一度広げる。
    if n > scratch.capacity() {
        scratch.reserve(n - scratch.capacity());
    }
    scratch.clear();
    for &s in data {
        scratch.push(convert(s));
    }
}

/// プライミング過渡バッファと判定する f32 ピーク振幅の閾値。
///
/// flexaudio の f32 サンプルは契約上 `[-1.0, 1.0]`。Linux の PipeWire ALSA 互換
/// ブリッジ（`default` PCM）は、冷えた状態で stream を開くと開始直後の数百 ms ぶん
/// 範囲を大きく超えたフルスケール矩形のプライミング用ダミーバッファを吐く（実測ピーク
/// ≈ 3.3、左右ほぼ逆相なので source 段では DC≈0 だが、下流のレート変換で巨大 DC と
/// クリップに化ける）。正常音声は契約上 1.0 が上限なので、ピークが 1.0 を明確に超えた
/// バッファを過渡とみなせる。±1.0 ちょうどの正常音声を巻き込まないよう 1.0 直上に置く。
const PRIMING_PEAK_LIMIT: f32 = 1.001;

/// キャプチャ開始直後のプライミング過渡バッファを破棄するガード。
///
/// 過渡バッファは `[-1.0, 1.0]` を超えるフルスケール矩形なので、ピークが
/// [`PRIMING_PEAK_LIMIT`] を超えるバッファだけ捨てる。
///
/// 固定秒数で頭を捨てる方式は、過渡の無い環境（Mac / Windows / warm な Linux）でも
/// 頭出し無音を作ってしまう。ピークだけ見て過渡か判定するので、過渡が無い環境では
/// 1 バッファも捨てず、過渡の実長にも自動追従する。
///
/// 先頭側が過渡（範囲外）に見える間だけ捨て、レンジ内バッファが 1 つ来たら以後は
/// 永久に通す（latch-open）。過渡は起動直後だけ現れて単調減衰するので途中で再発
/// しない。RT コールバック内専用なので、判定はバッファ 1 走査の `abs` 比較だけ。
struct TransientGuard {
    /// 既に正常（レンジ内）バッファを通したか。`true` 以降は常に通す。
    latched: bool,
}

impl TransientGuard {
    fn new() -> Self {
        Self { latched: false }
    }

    /// interleaved f32 バッファを与え、プライミング過渡（捨てるべき）なら `true`。
    /// レンジ内バッファを 1 つでも通したら、以後は常に `false`（通す）。
    fn should_drop(&mut self, data: &[f32]) -> bool {
        if self.latched || data.is_empty() {
            // 既に正常区間。または空バッファ（捨てる意味がない）。
            self.latched = true;
            return false;
        }
        // バッファ 1 走査でピーク振幅を求める。
        let mut peak = 0.0f32;
        for &s in data {
            let a = s.abs();
            if a > peak {
                peak = a;
            }
        }
        // 契約レンジを明確に超える＝プライミング過渡。
        let is_transient = peak > PRIMING_PEAK_LIMIT;
        if !is_transient {
            self.latched = true;
        }
        is_transient
    }
}

/// `device_id` の入力デバイスへ input stream を build する（まだ `play` しない）。
/// `device_id = None` で既定入力デバイス。一致するデバイスが無ければ
/// [`Error::DeviceNotFound`]。
///
/// sample format ごとにコールバックを分岐し、F32 はそのまま、I16/U16/I32 は
/// `f32` `[-1.0, 1.0]` へ変換して [`RawSink::push`] へ渡す。開始直後のプライミング
/// 過渡バッファは [`TransientGuard`] が破棄する（PipeWire ALSA ブリッジ対策）。
fn build_stream(sink: RawSink, device_id: Option<&str>) -> Result<cpal::Stream> {
    let host = cpal::default_host();
    // None=既定 / Some=name 一致の最初。不一致は DeviceNotFound。
    let device = resolve_input_device(&host, device_id)?;

    // 既定入力 config が取れない＝広告されたデバイスが実際には開けない
    // （サウンドカード無しのサーバ等で ALSA "default" PCM が開けない場合を含む）。
    // 使える入力デバイスが無いのと等価なので DeviceNotFound に写す。
    let supported = device
        .default_input_config()
        .map_err(|_| Error::DeviceNotFound)?;
    let sample_format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();

    let err_fn = |e: cpal::StreamError| {
        // RT 経路外のエラーコールバック。ログ手段が未配線なので今は黙殺する
        // （TODO: 配線層で Event::DeviceLost 等へ写す）。
        let _ = e;
    };

    // 変換経路のスクラッチを最大ブロック長（ネイティブ SR×ch × MAX_SCRATCH_SECONDS）
    // で事前確保する。RT コールバック内の初回/拡大アロケート（xrun リスク）を定常状態
    // で避けるため。最低 1 は確保する。
    let scratch_cap = (config.sample_rate.0 as usize)
        .saturating_mul(config.channels as usize)
        .saturating_mul(MAX_SCRATCH_SECONDS)
        .max(1);

    // sink はコールバックへ move。F32 以外は変換用に閉じ込める。過渡判定は f32 値に
    // 対して行うので、変換フォーマットでは変換後に判定する。
    //
    // cpal の data コールバックは FFI（C ABI）境界を越えて呼ばれるので、ここで panic
    // すると未定義動作になり得る。今 live なパニック経路は無いが、念のため各コールバック
    // 本体を catch_unwind で包み、万一の panic はその回のブロックを捨てるだけにする。
    let stream = match sample_format {
        SampleFormat::F32 => {
            let mut sink = sink;
            let mut guard = TransientGuard::new();
            device.build_input_stream(
                &config,
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    let _ = catch_unwind(AssertUnwindSafe(|| {
                        // 既に interleaved f32。プライミング過渡なら捨てる。
                        if guard.should_drop(data) {
                            return;
                        }
                        sink.push(data, monotonic_now_ns());
                    }));
                },
                err_fn,
                None,
            )
        }
        SampleFormat::I16 => {
            let mut sink = sink;
            // 変換用スクラッチ。最大ブロック長で事前確保し、RT 内で容量拡大させない。
            let mut scratch: Vec<f32> = Vec::with_capacity(scratch_cap);
            let mut guard = TransientGuard::new();
            device.build_input_stream(
                &config,
                move |data: &[i16], _: &cpal::InputCallbackInfo| {
                    let _ = catch_unwind(AssertUnwindSafe(|| {
                        fill_scratch(&mut scratch, data, |s| s as f32 / -(i16::MIN as f32));
                        if guard.should_drop(&scratch) {
                            return;
                        }
                        sink.push(&scratch, monotonic_now_ns());
                    }));
                },
                err_fn,
                None,
            )
        }
        SampleFormat::U16 => {
            let mut sink = sink;
            let mut scratch: Vec<f32> = Vec::with_capacity(scratch_cap);
            let mut guard = TransientGuard::new();
            device.build_input_stream(
                &config,
                move |data: &[u16], _: &cpal::InputCallbackInfo| {
                    let _ = catch_unwind(AssertUnwindSafe(|| {
                        // u16 [0, 65535] を中点 32768 基準で [-1, 1) へ。
                        fill_scratch(&mut scratch, data, |s| (s as f32 - 32_768.0) / 32_768.0);
                        if guard.should_drop(&scratch) {
                            return;
                        }
                        sink.push(&scratch, monotonic_now_ns());
                    }));
                },
                err_fn,
                None,
            )
        }
        SampleFormat::I32 => {
            let mut sink = sink;
            let mut scratch: Vec<f32> = Vec::with_capacity(scratch_cap);
            let mut guard = TransientGuard::new();
            device.build_input_stream(
                &config,
                move |data: &[i32], _: &cpal::InputCallbackInfo| {
                    let _ = catch_unwind(AssertUnwindSafe(|| {
                        fill_scratch(&mut scratch, data, |s| s as f32 / -(i32::MIN as f32));
                        if guard.should_drop(&scratch) {
                            return;
                        }
                        sink.push(&scratch, monotonic_now_ns());
                    }));
                },
                err_fn,
                None,
            )
        }
        other => {
            return Err(Error::Backend(format!(
                "unsupported cpal sample format: {other:?}"
            )));
        }
    };

    stream.map_err(|e| Error::Backend(format!("build_input_stream: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use flexaudio_core::raw_ring;

    /// interleaved stereo の f32 バッファを `frames` フレームぶん生成する。
    /// 各サンプルは交互に `±peak`（ピーク振幅 `peak` の矩形）。
    fn make_buf(frames: usize, peak: f32) -> Vec<f32> {
        let mut v = Vec::with_capacity(frames * 2);
        for i in 0..frames {
            let s = if i % 2 == 0 { peak } else { -peak };
            v.push(s); // L
            v.push(s); // R
        }
        v
    }

    /// [`TransientGuard`] が「範囲外フルスケール（過渡）→ 減衰 → レンジ内」の列の
    /// 先頭側（ピークが [`PRIMING_PEAK_LIMIT`] 超のバッファ）だけを破棄し、レンジ内
    /// バッファが来たら latch して以後は全て通すこと。
    #[test]
    fn transient_guard_drops_priming_then_latches_open() {
        let frames = 1024;
        let mut g = TransientGuard::new();

        // 範囲外フルスケール矩形（実測のプライミング過渡相当 peak≈3.3）→ 破棄。
        assert!(g.should_drop(&make_buf(frames, 3.3)));
        // 減衰中だがまだ範囲外（peak=1.5 > LIMIT）→ 破棄。
        assert!(g.should_drop(&make_buf(frames, 1.5)));
        // レンジ内に戻った正常音声（peak=0.88）→ 通す＝ここで latch。
        assert!(!g.should_drop(&make_buf(frames, 0.88)));
        // latch 後は、たとえ範囲外バッファが来ても以後は必ず通す（途中再発を防ぐ）。
        assert!(!g.should_drop(&make_buf(frames, 3.3)));
    }

    /// 過渡が全く無い環境（Mac/Win/warm Linux）では先頭からレンジ内音声なので、
    /// [`TransientGuard`] は 1 バッファも破棄しない（頭出し無音ゼロ）。
    #[test]
    fn transient_guard_passes_clean_audio_from_the_start() {
        let frames = 1024;
        let mut g = TransientGuard::new();
        // デジタルフルスケール ±1.0 ちょうどでも誤検知しない（LIMIT が 1.0 直上）。
        assert!(!g.should_drop(&make_buf(frames, 1.0)));
        assert!(!g.should_drop(&make_buf(frames, 0.5)));
        // 無音（全ゼロ）も破棄しない。
        assert!(!g.should_drop(&vec![0.0f32; frames * 2]));
    }

    /// 空バッファは破棄せず latch する。
    #[test]
    fn transient_guard_handles_empty_buffer() {
        let mut g = TransientGuard::new();
        assert!(!g.should_drop(&[]));
        // 空で latch したので、以後の範囲外バッファも通す。
        assert!(!g.should_drop(&make_buf(1024, 3.3)));
    }

    /// [`fill_scratch`] が容量内では再確保を起こさず、変換も正しいこと。RT コール
    /// バックでの定常状態アロケート無しを担保する。
    #[test]
    fn fill_scratch_no_realloc_in_steady_state() {
        // 最大想定ブロック長で確保。
        let cap = 480 * 2; // 10ms @ 48k stereo 相当
        let mut scratch: Vec<f32> = Vec::with_capacity(cap);
        let before = scratch.capacity();

        // 容量内のブロックを何度詰めても容量は変わらない（= 再確保が起きない）。
        let data: Vec<i16> = (0..cap as i16).collect();
        for _ in 0..100 {
            fill_scratch(&mut scratch, &data, |s| s as f32 / -(i16::MIN as f32));
            assert_eq!(scratch.len(), data.len());
            assert_eq!(scratch.capacity(), before, "定常状態で容量拡大しない");
        }
        // 変換が正しい（i16::MIN は -1.0 にマップ）。
        let mut one = Vec::with_capacity(1);
        fill_scratch(&mut one, &[i16::MIN], |s| s as f32 / -(i16::MIN as f32));
        assert_eq!(one[0], -1.0);
    }

    /// `new` + `native_format` が panic しないこと（入力デバイス有無を問わず）。
    /// device_id = None（既定）でも Some（特定デバイス）でも new は必ず成功する。
    #[test]
    fn new_and_native_format_do_not_panic() {
        // 既定入力デバイス（device_id = None）。
        let backend = CpalMicBackend::new(None);
        let (rate, channels) = backend.native_format();
        // フォーマットは常に正の値（デバイス無しなら FALLBACK_FORMAT）。
        assert!(rate > 0);
        assert!(channels > 0);

        // 存在しない device_id でも new は panic せず成功し、FALLBACK_FORMAT を返す
        // （解決失敗の表面化は start/build_stream まで遅延する設計）。
        let backend = CpalMicBackend::new(Some("__no_such_device__".into()));
        let (rate, channels) = backend.native_format();
        assert_eq!((rate, channels), FALLBACK_FORMAT);
    }

    /// 存在しない device_id を指定した `start` は panic せず
    /// [`Error::DeviceNotFound`] になる（cold-start/TransientGuard と整合）。
    /// 既定入力デバイスの有無に依らず、不一致 id は必ず DeviceNotFound。
    #[test]
    fn start_with_unknown_device_id_yields_device_not_found() {
        let mut backend = CpalMicBackend::new(Some("__no_such_device__".into()));
        let (rate, channels) = backend.native_format();
        let cap = (rate as usize * channels as usize).max(1);
        let (prod, _cons) = raw_ring(cap);
        let sink = RawSink::new(prod, rate, channels);

        match backend.start(sink) {
            Err(Error::DeviceNotFound) => {}
            other => panic!("unknown device_id は DeviceNotFound であるべき: {other:?}"),
        }
    }

    /// [`list_devices`] はデバイス有無を問わず panic せず `Ok(Vec)` を返す。
    /// 返ったデバイスは全て `Mic` / 非ループバックで、`id == name` の安定キーを持つ。
    #[test]
    fn list_devices_never_panics_and_is_consistent() {
        let devices = list_devices().expect("list_devices は Err を返さない設計");
        for d in &devices {
            assert_eq!(d.source_kind, SourceKind::Mic);
            assert!(!d.is_loopback, "マイクはループバックではない");
            // 安定キー: cpal では id にデバイス名を使う。
            assert_eq!(d.id, d.name);
            assert!(!d.id.is_empty(), "id（=name）は空でない");
            assert!(d.sample_rate > 0);
            assert!(d.channels > 0);
        }
        // 既定入力は高々 1 つ。
        assert!(devices.iter().filter(|d| d.is_default).count() <= 1);
    }

    /// `start` は入力デバイスが無い環境（サーバー・CI 等）では `Err(DeviceNotFound)` に
    /// なり得る。Ok と Err(DeviceNotFound) の両方を許容し、panic だけは不可。
    /// 入力デバイスがある環境では実際にキャプチャが起動し、stop で停止する。
    #[test]
    fn start_then_stop_tolerates_missing_device() {
        let mut backend = CpalMicBackend::new(None);
        let (rate, channels) = backend.native_format();
        let cap = (rate as usize * channels as usize).max(1); // 約 1 秒
        let (prod, _cons) = raw_ring(cap);
        let sink = RawSink::new(prod, rate, channels);

        match backend.start(sink) {
            Ok(()) => {
                // 起動できた環境では停止が安全に行えること。
                backend.stop();
                // 二重 stop も安全。
                backend.stop();
            }
            Err(Error::DeviceNotFound) => {
                // 入力デバイス無し環境（CI/サーバ）では許容。
            }
            Err(other) => panic!("unexpected error from start(): {other:?}"),
        }
    }

    /// 実マイクから実際に録音する end-to-end テスト。入力デバイスのある
    /// ラップトップ等で `cargo test -p flexaudio-mic -- --ignored` で回す。
    /// サーバ/CI には入力デバイスが無いため既定では `#[ignore]`。
    #[test]
    #[ignore = "実マイク必須。ラップトップで `cargo test -p flexaudio-mic -- --ignored` で実行"]
    fn end_to_end_captures_real_audio() {
        use std::time::Duration;

        let mut backend = CpalMicBackend::new(None);
        let (rate, channels) = backend.native_format();
        let cap = rate as usize * channels as usize * 2; // 約 2 秒
        let (prod, mut cons) = raw_ring(cap);
        let sink = RawSink::new(prod, rate, channels);

        backend
            .start(sink)
            .expect("start() should succeed with a real input device");

        // 数百ミリ秒キャプチャしてサンプルが流れてくることを確認。
        thread::sleep(Duration::from_millis(500));
        backend.stop();

        let mut buf = vec![0.0f32; cap];
        let got = cons.pop_slice(&mut buf);
        assert!(got > 0, "expected captured samples, got none");
        // サンプルは [-1, 1] の範囲内に収まること（変換の健全性）。
        assert!(buf[..got].iter().all(|&s| (-1.5..=1.5).contains(&s)));
    }
}
