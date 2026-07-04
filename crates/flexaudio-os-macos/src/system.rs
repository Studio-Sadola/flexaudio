//! [`MacSystemBackend`] — システム音声出力全体の Process Tap loopback。
//!
//! `CATapDescription::initStereoGlobalTapButExcludeProcesses([...])` で tap を作り、
//! private aggregate device + IOProc で録る。Windows の
//! [`WasapiSystemBackend`](../flexaudio_os_windows) / Linux の
//! [`PwSystemBackend`](../flexaudio_os_linux) 相当。
//!
//! # `exclude_self`
//! [`MacSystemBackend::new`] の `exclude_self` で除外集合を切り替える。
//! - `exclude_self == false`（既定）→ 除外なし `excludeProcesses([])`、全システム音。
//! - `exclude_self == true` → 自ホストプロセス（[`std::process::id`]）を除外
//!   `excludeProcesses([self_object])`。自分の出力を取り込まない（フィードバック防止）。
//!
//! `exclude_self` は system ソース専用で、process ソースの
//! [`ProcessMode`](flexaudio_core::types::ProcessMode) とは合成しない（system ソースは
//! `mode` を見ない）。
//!
//! # 出力デバイス選択（`device_id`）
//! [`MacSystemBackend::new`] の `device_id` で対象出力デバイスを選ぶ。
//! - `None`（既定）→ 既定出力の global tap。
//! - `Some(name)` → その名前の出力デバイスを対象に
//!   `initExcludingProcesses:andDeviceUID:withStream:` で tap を作る（名前→UID は
//!   [`uid_for_device_name`](crate::devices::uid_for_device_name) で解決）。一致デバイスが
//!   無ければ `start` が [`Error::DeviceNotFound`] を返す。列挙は
//!   [`list_output_devices`](crate::list_output_devices)。
//!
//! `exclude_self == true` のときは `device_id` を無視し、既定出力の自プロセス除外 tap にする
//! （自分の音の除外を優先）。
//!
//! # スレッド / Send
//! tap/aggregate/ioproc 周りの `!Send` な ObjC オブジェクト（[`TapChain`]）は専用スレッド内に
//! 閉じ込め、[`MacSystemBackend`] が保持するのは `Send` なものだけ（停止フラグ・[`JoinHandle`]・
//! キャッシュ済みフォーマット）にする（Windows と同じ作り）。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use flexaudio_core::backend::{CaptureBackend, RawSink};
use flexaudio_core::types::{Error, Result};

use crate::common::{translate_pid_to_object, FALLBACK_FORMAT};
use crate::tap::{build_tap_chain, TapChain, TapKind};

/// システム音声出力全体を Process Tap でキャプチャする [`CaptureBackend`]。
///
/// 専用スレッド上で tap チェーン（global tap → aggregate → IOProc）を構築し、IOProc の RT block
/// から interleaved f32 を [`RawSink::push`] へ流す。tap 作成が TCC 未承認等で失敗したときは
/// panic せず [`start`](CaptureBackend::start) が [`Error`] を返す。
///
/// `exclude_self` で自ホストプロセスの出力を除外するか切り替える（フィードバック防止）。
/// `device_id` で対象出力デバイスを選ぶ（`None` = 既定出力）。
///
/// `Send`。保持するのは `exclude_self`・`device_id`・停止フラグ・[`JoinHandle`]・キャッシュ済み
/// フォーマットだけで、`!Send` な ObjC は専用スレッド内に閉じ込める。
pub struct MacSystemBackend {
    /// 自ホストプロセス除外フラグ。`true` で自分（[`std::process::id`]）の出力を除外集合に
    /// 加える（`excludeProcesses([self])`）、`false` で除外なしの全システム音。
    exclude_self: bool,
    /// 対象出力デバイス名（= [`DeviceInfo::id`](flexaudio_core::types::DeviceInfo)）。`None` で
    /// 既定出力の global tap、`Some(name)` でその出力デバイスを対象にする。`exclude_self == true`
    /// のときは無視される。
    device_id: Option<String>,
    /// 起動中フラグ（二重 start ガード / 停止指示 / drop 判定）。`Send`。
    stop_flag: Arc<AtomicBool>,
    /// tap チェーンを所有するスレッドのハンドル（start 後に `Some`）。
    handle: Option<JoinHandle<()>>,
    /// ネイティブフォーマット `(rate, channels)`。実際の値は tap 作成後に `start` 経由で
    /// 確定するが、`native_format` では事前キャッシュ（フォールバック）を返す。
    native: (u32, u16),
}

impl MacSystemBackend {
    /// システム loopback バックエンドを構築する（この時点では tap を作らない）。
    ///
    /// `exclude_self` が `true` のとき、`start` で自ホストプロセス（[`std::process::id`]）を
    /// 除外集合に加える（フィードバック防止）。`false` のときは除外なしの全システム tap。
    ///
    /// `device_id` が `Some(name)` のとき、その名前の出力デバイスを対象に tap を作る
    /// （`start` で名前→UID を解決し、無ければ [`Error::DeviceNotFound`]）。`None` で既定出力。
    /// `exclude_self == true` のときは `device_id` を無視する。
    ///
    /// ネイティブフォーマットはフォールバック `(48000, 2)` をキャッシュする。実フォーマットは
    /// tap 作成時（`start`）に tap の ASBD から決まるが、`native_format` は構築時に 1 つ返す必要が
    /// あるので、tap 無しで安全に得られるフォールバックを使う。Normalizer は出力 20ms 時間ベース
    /// なので、多少のネイティブ推定差は第 1 段リサンプルで吸収される。
    pub fn new(exclude_self: bool, device_id: Option<String>) -> Self {
        Self {
            exclude_self,
            device_id,
            stop_flag: Arc::new(AtomicBool::new(false)),
            handle: None,
            native: FALLBACK_FORMAT,
        }
    }
}

impl Default for MacSystemBackend {
    fn default() -> Self {
        Self::new(false, None)
    }
}

impl CaptureBackend for MacSystemBackend {
    fn native_format(&self) -> (u32, u16) {
        self.native
    }

    fn start(&mut self, sink: RawSink) -> Result<()> {
        if self.handle.is_some() {
            return Ok(());
        }

        // バージョンゲート。Process Tap は macOS 14.4 以上が必須。tap 生成へ進む前に OS バージョンを
        // 確認し、満たさなければ raw OSStatus→Backend に化けさせず型付きの
        // Error::UnsupportedOsVersion を返す。
        crate::version::ensure_process_tap_supported()?;

        self.stop_flag.store(false, Ordering::SeqCst);

        let stop_flag = self.stop_flag.clone();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
        // bool は Copy なのでそのままクロージャへ move できる。
        let exclude_self = self.exclude_self;
        // device_id（String）はクロージャへ move する。exclude_self のときは使わない。
        let device_id = self.device_id.clone();

        let handle = thread::Builder::new()
            .name("flexaudio-macos-system".into())
            .spawn(move || {
                let kind = if exclude_self {
                    // exclude_self は device_id より優先。既定出力で自プロセスを除外する。
                    // PID → AudioObjectID 変換は CoreAudio を叩くので所有スレッド内で行う
                    // （process.rs と同じ）。自ホストプロセスのオブジェクトを除外集合に入れる。
                    match translate_pid_to_object(std::process::id() as i32) {
                        // 自プロセスに対応するオーディオオブジェクトが無い（今は無音で出力していない
                        // 等）。除外すべき自分の音が無いので、エラーにせず除外なしの全システム tap へ
                        // 落とす（録り逃さない）。
                        Ok(0) => TapKind::ExcludeProcesses(Vec::new()),
                        // 自プロセスのオブジェクトを除外集合に加える（フィードバック防止）。
                        Ok(self_object_id) => TapKind::ExcludeProcesses(vec![self_object_id]),
                        // 変換自体が失敗（TCC 等）。readiness として Err を返して終了。
                        Err(e) => {
                            let _ = ready_tx.send(Err(e));
                            return;
                        }
                    }
                } else if let Some(name) = device_id {
                    // 指定出力デバイス。名前→UID を解決して、そのデバイス宛の全音（除外なし）を
                    // tap する。一致デバイスが無ければ DeviceNotFound。
                    match crate::devices::uid_for_device_name(&name) {
                        Some(uid) => TapKind::ExcludeProcessesOnDevice {
                            ids: Vec::new(),
                            device_uid: uid,
                        },
                        None => {
                            let _ = ready_tx.send(Err(Error::DeviceNotFound));
                            return;
                        }
                    }
                } else {
                    // 既定出力。除外なしの全システム音。PID 変換不要。
                    TapKind::ExcludeProcesses(Vec::new())
                };
                run_tap_thread(kind, sink, stop_flag, ready_tx);
            })
            .map_err(|e| Error::Backend(format!("spawn macos system thread: {e}")))?;

        match ready_rx.recv() {
            Ok(Ok(())) => {
                self.handle = Some(handle);
                Ok(())
            }
            Ok(Err(e)) => {
                self.stop_flag.store(false, Ordering::SeqCst);
                let _ = handle.join();
                Err(e)
            }
            Err(_) => {
                self.stop_flag.store(false, Ordering::SeqCst);
                let _ = handle.join();
                Err(Error::Backend(
                    "macos system thread exited before reporting readiness".into(),
                ))
            }
        }
    }

    fn stop(&mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for MacSystemBackend {
    fn drop(&mut self) {
        self.stop();
    }
}

/// tap チェーンを所有するスレッドの本体（system / process で共通）。
///
/// `kind` に応じて [`build_tap_chain`] でチェーンを作り、成否を `ready_tx` で報告する。成功後は
/// IOProc（CoreAudio の RT スレッド）が裏で block を回し続けるので、本スレッドは `stop_flag` が
/// 立つまで park して待つだけ。stop で [`TapChain`] を drop し、逆順に破棄する。
pub(crate) fn run_tap_thread(
    kind: TapKind,
    sink: RawSink,
    stop_flag: Arc<AtomicBool>,
    ready_tx: mpsc::Sender<Result<()>>,
) {
    // CATapDescription / aggregate の表示名（private なので衝突しても無害。デバッグ用）。
    let label = match &kind {
        TapKind::IncludeProcesses(_) => "flexaudio-process-tap",
        TapKind::ExcludeProcesses(_) => "flexaudio-system-tap",
        TapKind::ExcludeProcessesOnDevice { .. } => "flexaudio-system-device-tap",
    };
    // SAFETY: build_tap_chain は CoreAudio を叩く。sink を block へ move する。
    let chain: TapChain = match unsafe { build_tap_chain(kind, label, sink) } {
        Ok(c) => c,
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };

    if ready_tx.send(Ok(())).is_err() {
        // 呼び出し元が消えている。chain を drop して片付ける。
        drop(chain);
        return;
    }

    // IOProc は CoreAudio の RT スレッドで回る。本スレッドは stop まで待機する。
    while !stop_flag.load(Ordering::SeqCst) {
        thread::park_timeout(std::time::Duration::from_millis(100));
    }

    // stop。chain の drop で Stop→IOProc→aggregate→tap の順に破棄する。
    drop(chain);
}

#[cfg(test)]
mod tests {
    use super::*;
    use flexaudio_core::raw_ring;

    /// `new` + `native_format` は panic せず妥当な値を返す。
    #[test]
    fn new_and_native_format_do_not_panic() {
        let backend = MacSystemBackend::new(false, None);
        let (rate, channels) = backend.native_format();
        assert!(rate > 0);
        assert!(channels > 0);
    }

    /// `start` → `stop` が tap 作成可否を問わず panic しないこと。
    /// TCC 未承認 / tap 不可環境では `Err` を許容（panic だけ不可）。
    #[test]
    fn start_then_stop_tolerates_failure() {
        let mut backend = MacSystemBackend::new(false, None);
        let (rate, channels) = backend.native_format();
        let cap = (rate as usize * channels as usize).max(1);
        let (prod, _cons) = raw_ring(cap);
        let sink = RawSink::new(prod, rate, channels);

        match backend.start(sink) {
            Ok(()) => {
                backend.stop();
                backend.stop(); // 二重 stop も安全。
            }
            Err(_e) => { /* TCC 未承認 / tap 不可は許容 */ }
        }
    }

    /// `exclude_self == true` でも `native_format` が妥当で、`start` → `stop` が panic
    /// しないこと（headless/CI は TCC 無しなので `Err` を許容。自 PID 変換経路を踏ませる）。
    #[test]
    fn new_exclude_self_start_then_stop_tolerates_failure() {
        let mut backend = MacSystemBackend::new(true, None);
        let (rate, channels) = backend.native_format();
        assert!(rate > 0);
        assert!(channels > 0);
        let cap = (rate as usize * channels as usize).max(1);
        let (prod, _cons) = raw_ring(cap);
        let sink = RawSink::new(prod, rate, channels);

        match backend.start(sink) {
            Ok(()) => {
                backend.stop();
                backend.stop(); // 二重 stop も安全。
            }
            Err(_e) => { /* TCC 未承認 / tap 不可 / 自 PID 変換不可は許容 */ }
        }
    }

    /// 存在しない出力デバイス名を指定すると `start` が `DeviceNotFound` を返すこと。
    /// 名前解決で弾かれるので headless でも決定的に Err になる（14.4 未満は version gate が
    /// 先に `UnsupportedOsVersion` を返すのでそれも許容）。
    #[test]
    fn new_with_unknown_device_id_returns_device_not_found() {
        let mut backend =
            MacSystemBackend::new(false, Some("flexaudio-no-such-output-device-xyzzy".into()));
        let (rate, channels) = backend.native_format();
        let cap = (rate as usize * channels as usize).max(1);
        let (prod, _cons) = raw_ring(cap);
        let sink = RawSink::new(prod, rate, channels);

        match backend.start(sink) {
            Err(Error::DeviceNotFound) | Err(Error::UnsupportedOsVersion) => {}
            other => panic!("expected DeviceNotFound or UnsupportedOsVersion, got {other:?}"),
        }
    }
}
