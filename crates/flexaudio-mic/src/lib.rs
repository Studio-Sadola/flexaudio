//! flexaudio-mic — マイク入力バックエンド (cpal, 全 OS)。
//!
//! [`CpalMicBackend`] は cpal を介して既定入力デバイスから生 interleaved `f32`
//! フレームをキャプチャし、[`RawSink`](flexaudio_core::RawSink) へ非ブロッキングに
//! push する [`CaptureBackend`](flexaudio_core::CaptureBackend) 実装である。主な検証
//! 対象は Linux/ALSA だが、cpal が対応する全 OS で動作する。
//!
//! # `cpal::Stream` は `!Send` という制約
//! [`CaptureBackend`](flexaudio_core::CaptureBackend) は `Send` を要求するため、
//! `!Send` な [`cpal::Stream`] を backend 構造体へ直接保持できない。これを避けるため、
//! [`start`](CpalMicBackend::start) では **専用所有スレッド**を spawn し、その内部で
//! input stream を build + `play()` してから停止シグナルまで `park` する。停止時に
//! 所有スレッドが Stream を drop してキャプチャを終了する。`CpalMicBackend` 自身が
//! 保持するのは `Send` なもの（停止フラグ・[`JoinHandle`]・キャッシュ済みフォーマット）
//! だけである。
//!
//! ```no_run
//! use flexaudio_mic::CpalMicBackend;
//! use flexaudio_core::{CaptureBackend, RawSink, raw_ring};
//!
//! let mut backend = CpalMicBackend::new();
//! let (rate, channels) = backend.native_format();
//! let (prod, _cons) = raw_ring(rate as usize * channels as usize); // 1 秒ぶん
//! let sink = RawSink::new(prod, rate, channels);
//! backend.start(sink).unwrap();
//! // ... _cons から生フレームを pop ...
//! backend.stop();
//! ```

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SampleFormat;

use flexaudio_core::backend::{CaptureBackend, RawSink};
use flexaudio_core::clock::monotonic_now_ns;
use flexaudio_core::types::{Error, Result};

/// 入力デバイスが取得できない場合に [`native_format`](CpalMicBackend::native_format)
/// が返す無難な既定フォーマット `(48000 Hz, mono)`。実際の `start` 時にデバイスが
/// 無ければ [`Error::DeviceNotFound`] になる。
const FALLBACK_FORMAT: (u32, u16) = (48_000, 1);

/// cpal を用いたマイク入力キャプチャバックエンド。
///
/// 既定入力デバイスから生 interleaved `f32` フレームをキャプチャし
/// [`RawSink`](flexaudio_core::RawSink) へ流す。詳細はモジュールドキュメント参照。
///
/// この型は `Send`（保持するのは停止フラグ・[`JoinHandle`]・キャッシュ済み
/// フォーマットのみ。`!Send` な [`cpal::Stream`] は所有スレッド内に閉じ込める）。
pub struct CpalMicBackend {
    /// 所有スレッドへの停止指示。`true` で stream を drop して終了する。
    stop_flag: Arc<AtomicBool>,
    /// cpal stream を所有するスレッドのハンドル（start 後に `Some`）。
    handle: Option<JoinHandle<()>>,
    /// `new` 時に問い合わせてキャッシュしたネイティブフォーマット。
    native: (u32, u16),
}

impl CpalMicBackend {
    /// 新しいマイクバックエンドを構築する。
    ///
    /// 構築時に既定入力デバイスのネイティブフォーマットを問い合わせてキャッシュする。
    /// デバイスが無い／問い合わせに失敗した場合は [`FALLBACK_FORMAT`]（`(48000, 1)`）を
    /// キャッシュし、実際の [`start`](Self::start) でデバイスが無ければ
    /// [`Error::DeviceNotFound`] を返す。この関数は panic しない。
    pub fn new() -> Self {
        let native = query_native_format().unwrap_or(FALLBACK_FORMAT);
        Self {
            stop_flag: Arc::new(AtomicBool::new(false)),
            handle: None,
            native,
        }
    }
}

impl Default for CpalMicBackend {
    fn default() -> Self {
        Self::new()
    }
}

/// 既定入力デバイスのネイティブフォーマット `(sample_rate, channels)` を取得する。
/// デバイス／設定が取れなければ `None`。
fn query_native_format() -> Option<(u32, u16)> {
    let host = cpal::default_host();
    let device = host.default_input_device()?;
    let config = device.default_input_config().ok()?;
    Some((config.sample_rate().0, config.channels()))
}

impl CaptureBackend for CpalMicBackend {
    fn native_format(&self) -> (u32, u16) {
        self.native
    }

    fn start(&mut self, sink: RawSink) -> Result<()> {
        // 二重 start に安全: 既に所有スレッドが生きていれば何もしない。
        if self.handle.is_some() {
            return Ok(());
        }
        // 前回の stop 後でも再 start できるようフラグをリセット。
        self.stop_flag.store(false, Ordering::SeqCst);

        let stop_flag = self.stop_flag.clone();
        // build/play の成否を所有スレッドから start() へ返すための ready channel。
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();

        let handle = thread::Builder::new()
            .name("flexaudio-mic-cpal".into())
            .spawn(move || {
                run_capture_thread(sink, stop_flag, ready_tx);
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
        // 再入・二重 stop に安全: handle が無ければ何もしない。
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
/// `stop_flag` が立つまで park し続け（`stream` を生かす）、立ったら関数を抜けて
/// `stream` を drop することでキャプチャを停止する。
fn run_capture_thread(
    sink: RawSink,
    stop_flag: Arc<AtomicBool>,
    ready_tx: mpsc::Sender<Result<()>>,
) {
    let stream = match build_stream(sink) {
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

/// 既定入力デバイスへ input stream を build する（まだ `play` はしない）。
///
/// sample format に応じてコールバックを分岐し、F32 はそのまま、I16/U16/I32 は
/// `f32` `[-1.0, 1.0]` へ変換して [`RawSink::push`] へ渡す。
fn build_stream(sink: RawSink) -> Result<cpal::Stream> {
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or(Error::DeviceNotFound)?;

    // 既定入力 config が取れない＝広告されたデバイスが実際には開けない
    // （サウンドカード無しのサーバ等で ALSA "default" PCM が開けない場合を含む）。
    // 使える入力デバイスが無いのと等価なので DeviceNotFound に写す。
    let supported = device
        .default_input_config()
        .map_err(|_| Error::DeviceNotFound)?;
    let sample_format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();

    let err_fn = |e: cpal::StreamError| {
        // RT 経路外のエラーコールバック。ログ手段が未配線のため現状は黙殺する
        // （配線層で Event::DeviceLost 等へ写すのが TODO）。
        let _ = e;
    };

    // sink はコールバックへ move する。F32 以外は変換用に閉じ込める。
    let stream = match sample_format {
        SampleFormat::F32 => {
            let mut sink = sink;
            device.build_input_stream(
                &config,
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    // 既に interleaved f32。そのまま非ブロッキング push。
                    sink.push(data, monotonic_now_ns());
                },
                err_fn,
                None,
            )
        }
        SampleFormat::I16 => {
            let mut sink = sink;
            // 変換用スクラッチ。コールバック内に閉じ込めて再利用（アロケート回避）。
            let mut scratch: Vec<f32> = Vec::new();
            device.build_input_stream(
                &config,
                move |data: &[i16], _: &cpal::InputCallbackInfo| {
                    scratch.clear();
                    scratch.extend(data.iter().map(|&s| s as f32 / -(i16::MIN as f32)));
                    sink.push(&scratch, monotonic_now_ns());
                },
                err_fn,
                None,
            )
        }
        SampleFormat::U16 => {
            let mut sink = sink;
            let mut scratch: Vec<f32> = Vec::new();
            device.build_input_stream(
                &config,
                move |data: &[u16], _: &cpal::InputCallbackInfo| {
                    scratch.clear();
                    // u16 [0, 65535] を中点 32768 基準で [-1, 1) へ。
                    scratch.extend(
                        data.iter()
                            .map(|&s| (s as f32 - 32_768.0) / 32_768.0),
                    );
                    sink.push(&scratch, monotonic_now_ns());
                },
                err_fn,
                None,
            )
        }
        SampleFormat::I32 => {
            let mut sink = sink;
            let mut scratch: Vec<f32> = Vec::new();
            device.build_input_stream(
                &config,
                move |data: &[i32], _: &cpal::InputCallbackInfo| {
                    scratch.clear();
                    scratch.extend(data.iter().map(|&s| s as f32 / -(i32::MIN as f32)));
                    sink.push(&scratch, monotonic_now_ns());
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

    /// `new` + `native_format` が panic しないこと（入力デバイス有無を問わず）。
    #[test]
    fn new_and_native_format_do_not_panic() {
        let backend = CpalMicBackend::new();
        let (rate, channels) = backend.native_format();
        // フォーマットは常に正の値（デバイス無しなら FALLBACK_FORMAT）。
        assert!(rate > 0);
        assert!(channels > 0);
    }

    /// `start` は homelab（サーバ）に入力デバイスが無いと `Err(DeviceNotFound)` に
    /// なり得る。Ok と Err(DeviceNotFound) の両方を許容し、panic だけは不可。
    /// 入力デバイスがある環境では実際にキャプチャが起動し、stop で停止する。
    #[test]
    fn start_then_stop_tolerates_missing_device() {
        let mut backend = CpalMicBackend::new();
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

        let mut backend = CpalMicBackend::new();
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
