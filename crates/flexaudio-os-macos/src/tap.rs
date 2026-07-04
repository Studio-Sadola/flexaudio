//! Process Tap チェーンの共通実装。`CATapDescription` → process tap →
//! private aggregate device → IOProc(block) → start を作り、破棄は [`TapChain`] の
//! `Drop` が逆順で行う。
//!
//! system / process 両バックエンドはこの [`build_tap_chain`] を `TapKind` で INCLUDE/EXCLUDE
//! を切り替えて呼ぶだけ。チェーン本体は共通。
//!
//! # 破棄順
//! `AudioDeviceStop` → `AudioDeviceDestroyIOProcID` →
//! `AudioHardwareDestroyAggregateDevice` → `AudioHardwareDestroyProcessTap` の順で片付け、
//! 最後に block（`RcBlock`）と `CATapDescription`（`Retained`）が drop される。この順序は
//! [`TapChain`] のフィールド宣言順と `Drop` 実装で守る。

use std::cell::RefCell;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::AnyThread;
use objc2_core_audio::{
    kAudioAggregateDeviceIsPrivateKey, kAudioAggregateDeviceIsStackedKey,
    kAudioAggregateDeviceNameKey, kAudioAggregateDeviceTapAutoStartKey,
    kAudioAggregateDeviceTapListKey, kAudioAggregateDeviceUIDKey, kAudioSubTapDriftCompensationKey,
    kAudioSubTapUIDKey, AudioDeviceCreateIOProcIDWithBlock, AudioDeviceDestroyIOProcID,
    AudioDeviceIOProcID, AudioDeviceStart, AudioDeviceStop, AudioHardwareCreateAggregateDevice,
    AudioHardwareCreateProcessTap, AudioHardwareDestroyAggregateDevice,
    AudioHardwareDestroyProcessTap, AudioObjectID, CATapDescription,
};
use objc2_core_audio_types::{AudioBufferList, AudioTimeStamp};
use objc2_core_foundation::CFDictionary;
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSObject, NSString};

use flexaudio_core::backend::RawSink;
use flexaudio_core::types::Error;

use crate::common::{
    map_os_status, now_ns, tap_format_is_float, tap_native_format, FALLBACK_FORMAT, NO_ERR,
};

/// tap の種別。INCLUDE = 指定プロセス群の mixdown / EXCLUDE = 指定プロセス群を除く全体。
pub(crate) enum TapKind {
    /// 指定オブジェクト群を含むステレオ mixdown（プロセスループバック INCLUDE）。
    /// 空 vec は不正（呼び出し側が DeviceNotFound を返す）。
    IncludeProcesses(Vec<AudioObjectID>),
    /// 指定オブジェクト群を除く全システム音（既定出力）。空 vec ならシステム全体。
    ExcludeProcesses(Vec<AudioObjectID>),
    /// 指定オブジェクト群を除く、特定出力デバイス宛の音。`device_uid` はそのデバイスの UID。
    /// `ExcludeProcesses` の既定出力版に対し、こちらは出力先を 1 デバイスへ絞る。
    ExcludeProcessesOnDevice {
        /// 除外するプロセスオブジェクト群（空ならそのデバイス宛の全システム音）。
        ids: Vec<AudioObjectID>,
        /// 対象出力デバイスの UID（`kAudioDevicePropertyDeviceUID`）。
        device_uid: String,
    },
}

/// 構築済みの tap チェーン。`Drop` で逆順に破棄する。
///
/// フィールド宣言順を Rust の drop 順（宣言順）に合わせ、`Drop` 実装で明示的に
/// Stop→IOProc→aggregate→tap の順で OS リソースを片付けてから
/// `RcBlock` / `Retained<CATapDescription>` を drop させる。
// `_block` の `RcBlock<dyn Fn(...)>` は CoreAudio の IOProc block シグネチャ（5 引数）を
// そのまま写すので複雑になる。型エイリアスにしても読みやすくならないため、Linux backend と
// 同じく lint をここだけ許可する。
#[allow(clippy::type_complexity)]
pub(crate) struct TapChain {
    /// IOProc が回っている aggregate device ID。
    aggregate_id: AudioObjectID,
    /// 登録済み IOProc ID（block 駆動）。
    io_proc_id: AudioDeviceIOProcID,
    /// process tap ID。
    tap_id: AudioObjectID,
    /// IOProc 停止ゲート。`AudioDeviceStop` を呼ぶ前に `stopped=true`（Release）にし、IOProc
    /// block 先頭で `Acquire` ロードして立っていれば即 return させる。`AudioDeviceStop` 戻り後に
    /// in-flight だった late callback が `RefCell<RawSink>` を触る窓を塞ぐためのフェイルセーフ。
    /// CoreAudio が IOProc を単一スレッド・非再入で呼ぶことは Apple が文書化していないので念を
    /// 入れる。`Arc` で block と共有する。
    stopped: Arc<AtomicBool>,
    /// IOProc に渡した block（`DestroyIOProcID` まで生存必須）。最後に drop。
    _block: RcBlock<
        dyn Fn(
            NonNull<AudioTimeStamp>,
            NonNull<AudioBufferList>,
            NonNull<AudioTimeStamp>,
            NonNull<AudioBufferList>,
            NonNull<AudioTimeStamp>,
        ),
    >,
    /// tap description（aggregate 生存中は保持しておく）。block の後に drop。
    _desc: Retained<CATapDescription>,
}

// SAFETY: TapChain が保持する id 群は u32 で Send。`RcBlock` / `Retained<CATapDescription>` は
// 所有スレッド（バックエンドの専用スレッド）の中で生成・drop され、スレッド境界を跨いで共有され
// ない。バックエンド本体（`MacSystemBackend`/`MacProcessBackend`）が `Send` なので TapChain 自体は
// スレッドを跨がない設計。そのため TapChain への Send/Sync は宣言しない。

impl Drop for TapChain {
    fn drop(&mut self) {
        // late-callback ガード。`AudioDeviceStop` を呼ぶ前に停止フラグを立てる（Release）。
        // `AudioDeviceStop` 戻り後に in-flight だった IOProc が走っても、block 先頭の `Acquire`
        // ロードでこの store が見え、`RefCell<RawSink>` を触らず即 return する。
        self.stopped.store(true, Ordering::Release);
        // 破棄順: Stop → DestroyIOProcID → DestroyAggregateDevice → DestroyProcessTap。
        // 失敗は無視（best-effort クリーンアップ）。
        unsafe {
            if self.io_proc_id.is_some() {
                let _ = AudioDeviceStop(self.aggregate_id, self.io_proc_id);
                let _ = AudioDeviceDestroyIOProcID(self.aggregate_id, self.io_proc_id);
            }
            if self.aggregate_id != 0 {
                let _ = AudioHardwareDestroyAggregateDevice(self.aggregate_id);
            }
            if self.tap_id != 0 {
                let _ = AudioHardwareDestroyProcessTap(self.tap_id);
            }
        }
        // ここを抜けると宣言順で _block → _desc の順に drop される。
    }
}

/// `AudioObjectID` 群を `NSArray<NSNumber>`（u32 値）へ。
fn object_ids_to_nsarray(ids: &[AudioObjectID]) -> Retained<NSArray<NSNumber>> {
    let numbers: Vec<Retained<NSNumber>> = ids
        .iter()
        .map(|&id| NSNumber::numberWithUnsignedInt(id))
        .collect();
    NSArray::from_retained_slice(&numbers)
}

/// `&CStr` 鍵（objc2-core-audio が export する `kAudio…Key`）を `NSString` 鍵へ。
fn cstr_key(key: &std::ffi::CStr) -> Retained<NSString> {
    NSString::from_str(key.to_str().unwrap_or(""))
}

/// process tap → aggregate device → IOProc → start までを構築する。
///
/// `sink` は IOProc block へ move され、RT コールバックから [`RawSink::push`] される。成功時は
/// [`TapChain`] を返す（drop で全リソースを逆順破棄）。失敗時は途中まで作ったリソースをその場で
/// 破棄してから [`Error`] を返す。
///
/// # Safety
/// CoreAudio を呼ぶ。`sink` の所有権を block へ渡す。block は単一 RT スレッドからのみ呼ばれる
/// 前提で `RefCell` による内部可変を使う。
pub(crate) unsafe fn build_tap_chain(
    kind: TapKind,
    name: &str,
    sink: RawSink,
) -> Result<TapChain, Error> {
    // 1) CATapDescription（INCLUDE = mixdown / EXCLUDE = global-but-exclude /
    //    ExcludeOnDevice = 特定出力デバイス宛を exclude）。
    let desc: Retained<CATapDescription> = match &kind {
        TapKind::IncludeProcesses(ids) => {
            let arr = object_ids_to_nsarray(ids);
            CATapDescription::initStereoMixdownOfProcesses(CATapDescription::alloc(), &arr)
        }
        TapKind::ExcludeProcesses(ids) => {
            let arr = object_ids_to_nsarray(ids);
            CATapDescription::initStereoGlobalTapButExcludeProcesses(
                CATapDescription::alloc(),
                &arr,
            )
        }
        TapKind::ExcludeProcessesOnDevice { ids, device_uid } => {
            let arr = object_ids_to_nsarray(ids);
            let uid = NSString::from_str(device_uid);
            // stream 0 = デバイスの最初の出力ストリーム。tap のフォーマットはこのストリームに従う。
            // 出力先を device_uid のデバイスへ絞り、ids を除いたそのデバイス宛の音を mixdown する。
            CATapDescription::initExcludingProcesses_andDeviceUID_withStream(
                CATapDescription::alloc(),
                &arr,
                &uid,
                0,
            )
        }
    };
    desc.setName(&NSString::from_str(name));
    desc.setPrivate(true);
    // aggregate の sub-tap UID に使う tap の UUID 文字列。
    let uuid_str: Retained<NSString> = desc.UUID().UUIDString();

    // 2) process tap を作る。
    let mut tap_id: AudioObjectID = 0;
    let status = AudioHardwareCreateProcessTap(Some(&desc), &mut tap_id as *mut AudioObjectID);
    if status != NO_ERR {
        return Err(map_os_status("AudioHardwareCreateProcessTap", status));
    }
    if tap_id == 0 {
        return Err(Error::Backend(
            "AudioHardwareCreateProcessTap returned null tap id".into(),
        ));
    }

    // tap の native フォーマット（rate/channels）をデバッグ出力する。Stream の native_format は
    // 構築時に backend のフォールバック値を使うので、ここでの読みは情報目的だけ。
    if std::env::var_os("FLEXAUDIO_DEBUG").is_some() {
        match tap_native_format(tap_id) {
            Some((rate, ch)) => eprintln!(
                "[flexaudio-os-macos] tap ASBD: rate={rate} channels={ch} (fallback would be {FALLBACK_FORMAT:?})"
            ),
            None => eprintln!(
                "[flexaudio-os-macos] tap ASBD unavailable; using fallback {FALLBACK_FORMAT:?}"
            ),
        }
    }

    // ASBD の float ビットを確認する。IOProc は mData を *const f32 として読むので、非 float
    // サンプルだと UB になり得る。float ビットが無いと確定したときだけ Backend エラーで弾く。
    // ASBD 取得不能（None）は判定できないので float 決め打ちで続行する（実機 tap は常に float）。
    if let Some(false) = tap_format_is_float(tap_id) {
        let _ = unsafe { AudioHardwareDestroyProcessTap(tap_id) };
        return Err(Error::Backend(
            "tap format is not float (kAudioFormatFlagIsFloat unset); IOProc reads f32".into(),
        ));
    }

    // 3) private aggregate device を作る。失敗時は tap を破棄してから返す。
    let aggregate_id = match create_aggregate_device(name, &uuid_str) {
        Ok(id) => id,
        Err(e) => {
            let _ = AudioHardwareDestroyProcessTap(tap_id);
            return Err(e);
        }
    };

    // 4) IOProc block を作る。sink を block へ move（RefCell で内部可変）。
    //    block は単一 RT スレッドからのみ呼ばれる前提。
    let sink_cell = RefCell::new(sink);

    // planar→interleaved 用の scratch を、セットアップ時（非 RT）に最大想定長で確保しておく。
    // これで IOProc 内での初回/拡大ヒープアロケートを避ける。容量は tap の native フォーマット
    // （取れなければ FALLBACK）から ~100ms ぶんのフレーム × ch を見込む。IOProc バッファは通常
    // 10–20ms ぶんなので定常状態では resize が容量内 no-op になる（超えても拡大されるだけで安全）。
    // block が単独所有する `RefCell<Vec<f32>>` として move し、thread_local は使わない。所有
    // スレッドと RT スレッドが別でも block と一緒に正しく生存させるため。
    let (native_rate, native_ch) = tap_native_format(tap_id).unwrap_or(FALLBACK_FORMAT);
    let max_scratch = ((native_rate as usize / 10).max(1)) * (native_ch as usize).max(1);
    let scratch_cell = RefCell::new({
        let mut v: Vec<f32> = Vec::new();
        v.reserve_exact(max_scratch);
        v
    });

    // late-callback ガード用の停止フラグ（block と TapChain で共有）。stop/Drop が
    // `AudioDeviceStop` 前に true（Release）にし、block 先頭で Acquire ロードする。
    let stopped = Arc::new(AtomicBool::new(false));
    let stopped_for_block = stopped.clone();

    let block = RcBlock::new(
        move |_in_now: NonNull<AudioTimeStamp>,
              in_input: NonNull<AudioBufferList>,
              _in_input_time: NonNull<AudioTimeStamp>,
              _out: NonNull<AudioBufferList>,
              _out_time: NonNull<AudioTimeStamp>| {
            // この block は CoreAudio が呼ぶ FFI 境界コールバック。境界を越える panic は UB
            // なので本体全体を catch_unwind で包み、RawSink::push 等が万一 panic しても unwind を
            // CoreAudio へ伝播させない。
            let _ = catch_unwind(AssertUnwindSafe(|| {
                // late-callback ガード。stop/Drop が `AudioDeviceStop` 前に立てた停止フラグを
                // Acquire ロードする。立っていれば `RefCell<RawSink>` を触らず即 return し、
                // AudioDeviceStop 戻り後の in-flight callback が sink を触る窓を塞ぐ。
                if stopped_for_block.load(Ordering::Acquire) {
                    return;
                }
                // RT コールバック。借用に失敗（再入）したら何もしない。
                if let Ok(mut sink) = sink_cell.try_borrow_mut() {
                    if let Ok(mut scratch) = scratch_cell.try_borrow_mut() {
                        // SAFETY: in_input は有効な AudioBufferList（CoreAudio が供給）。
                        unsafe { push_buffer_list(&mut sink, &mut scratch, in_input.as_ptr()) };
                    }
                }
            }));
        },
    );

    // 5) IOProc を登録する（queue=None で device 既定の RT スレッド）。
    let mut io_proc_id: AudioDeviceIOProcID = None;
    let status = AudioDeviceCreateIOProcIDWithBlock(
        NonNull::from(&mut io_proc_id),
        aggregate_id,
        None,
        // AudioDeviceIOBlock = *mut DynBlock<...>。RcBlock を生 DynBlock ポインタとして渡す。
        RcBlock::as_ptr(&block),
    );
    if status != NO_ERR || io_proc_id.is_none() {
        let _ = AudioHardwareDestroyAggregateDevice(aggregate_id);
        let _ = AudioHardwareDestroyProcessTap(tap_id);
        return Err(map_os_status("AudioDeviceCreateIOProcIDWithBlock", status));
    }

    // 6) start。
    let status = AudioDeviceStart(aggregate_id, io_proc_id);
    if status != NO_ERR {
        let _ = AudioDeviceDestroyIOProcID(aggregate_id, io_proc_id);
        let _ = AudioHardwareDestroyAggregateDevice(aggregate_id);
        let _ = AudioHardwareDestroyProcessTap(tap_id);
        return Err(map_os_status("AudioDeviceStart", status));
    }

    Ok(TapChain {
        aggregate_id,
        io_proc_id,
        tap_id,
        stopped,
        _block: block,
        _desc: desc,
    })
}

/// private aggregate device を作り、その `AudioObjectID` を返す。
///
/// 渡す辞書:
/// `{ Name, UID(生成UUID), IsPrivate:true, IsStacked:false, TapAutoStart:true,
///    TapList:[{SubTapUID: tap UUID, SubTapDriftCompensation:true}] }`。
/// NSDictionary で組み、toll-free bridge で `&CFDictionary` として渡す。
fn create_aggregate_device(name: &str, sub_tap_uid: &NSString) -> Result<AudioObjectID, Error> {
    // sub-tap 辞書: { uid: <tap uuid>, drift: true }。
    let drift_true = NSNumber::numberWithBool(true);
    let sub_tap: Retained<NSDictionary<NSString, NSObject>> = NSDictionary::from_slices::<NSString>(
        &[
            &cstr_key(kAudioSubTapUIDKey),
            &cstr_key(kAudioSubTapDriftCompensationKey),
        ],
        &[sub_tap_uid.as_ref(), drift_true.as_ref()],
    );
    let tap_list: Retained<NSArray<NSObject>> =
        NSArray::from_retained_slice(&[Retained::into_super(sub_tap)]);

    // aggregate 自身の UID（一意な UUID 文字列）。
    let agg_uid = NSString::from_str(&new_uuid_string());
    let agg_name = NSString::from_str(name);
    let is_private = NSNumber::numberWithBool(true);
    let is_stacked = NSNumber::numberWithBool(false);
    let tap_auto_start = NSNumber::numberWithBool(true);

    let keys: [&NSString; 6] = [
        &cstr_key(kAudioAggregateDeviceNameKey),
        &cstr_key(kAudioAggregateDeviceUIDKey),
        &cstr_key(kAudioAggregateDeviceIsPrivateKey),
        &cstr_key(kAudioAggregateDeviceIsStackedKey),
        &cstr_key(kAudioAggregateDeviceTapAutoStartKey),
        &cstr_key(kAudioAggregateDeviceTapListKey),
    ];
    let values: [&NSObject; 6] = [
        agg_name.as_ref(),
        agg_uid.as_ref(),
        is_private.as_ref(),
        is_stacked.as_ref(),
        tap_auto_start.as_ref(),
        tap_list.as_ref(),
    ];
    let dict: Retained<NSDictionary<NSString, NSObject>> =
        NSDictionary::from_slices::<NSString>(&keys, &values);

    // SAFETY: NSDictionary と CFDictionary は toll-free bridged（同一 ObjC オブジェクト）なので
    // ポインタを &CFDictionary として読める。dict は本関数末尾まで生存し、その間ポインタは有効。
    let cf: &CFDictionary = unsafe { &*(Retained::as_ptr(&dict) as *const CFDictionary) };

    let mut device_id: AudioObjectID = 0;
    // SAFETY: cf は有効な CFDictionary、device_id は有効なローカル。
    let status = unsafe { AudioHardwareCreateAggregateDevice(cf, NonNull::from(&mut device_id)) };
    if status != NO_ERR {
        return Err(map_os_status("AudioHardwareCreateAggregateDevice", status));
    }
    if device_id == 0 {
        return Err(Error::Backend(
            "AudioHardwareCreateAggregateDevice returned null device id".into(),
        ));
    }
    Ok(device_id)
}

/// 一意な UUID 文字列を生成する（aggregate の UID 用）。
fn new_uuid_string() -> String {
    use objc2_foundation::NSUUID;
    NSUUID::new().UUIDString().to_string()
}

/// IOProc に渡る `AudioBufferList` を interleaved f32 として [`RawSink::push`] へ流す。
///
/// - interleaved（`mNumberBuffers == 1`）: そのまま push。
/// - planar（`mNumberBuffers >= 2`）: フレーム毎に L,R,L,R… へインターリーブして push
///   （事前確保した `scratch` Vec を再利用してアロケートを避ける）。
/// - size0 / null は無音とみなし push しない。
///
/// `scratch` は block がセットアップ時（非 RT）に確保した Vec。定常状態では `resize` が容量内
/// no-op になり、RT 経路でヒープアロケートが起きない（容量を超えても拡大されるだけ）。
///
/// # Safety
/// `list` は有効な `AudioBufferList` を指すこと（CoreAudio が IOProc に供給する）。
unsafe fn push_buffer_list(
    sink: &mut RawSink,
    scratch: &mut Vec<f32>,
    list: *const AudioBufferList,
) {
    if list.is_null() {
        return;
    }
    let num_buffers = (*list).mNumberBuffers as usize;
    if num_buffers == 0 {
        return;
    }
    // interleaved/planar の別を一度だけデバッグ出力（FLEXAUDIO_DEBUG 時）。
    log_buffer_shape_once(num_buffers);
    // mBuffers は可変長配列の先頭。num_buffers 本ぶんをスライスとして読む。
    let buffers = std::slice::from_raw_parts((*list).mBuffers.as_ptr(), num_buffers);

    if num_buffers == 1 {
        // interleaved: そのまま f32 として push。
        let buf = &buffers[0];
        let n = buf.mDataByteSize as usize / core::mem::size_of::<f32>();
        if n == 0 || buf.mData.is_null() {
            return;
        }
        let slice = std::slice::from_raw_parts(buf.mData as *const f32, n);
        sink.push(slice, now_ns());
        return;
    }

    // planar: 各バッファ = 1ch ぶん。フレーム数は最小バッファに合わせる。
    let channels = num_buffers;
    let mut min_frames = usize::MAX;
    for b in buffers.iter() {
        if b.mData.is_null() {
            return;
        }
        let frames = b.mDataByteSize as usize / core::mem::size_of::<f32>();
        min_frames = min_frames.min(frames);
    }
    if min_frames == 0 || min_frames == usize::MAX {
        return;
    }

    // 事前確保済み scratch を再利用してインターリーブする（RT 経路でのアロケートを避ける）。
    // channels == num_buffers == buffers.len() なので buffers をそのまま enumerate する。
    let total = min_frames * channels;
    scratch.resize(total, 0.0);
    for (ch, buf) in buffers.iter().enumerate() {
        let src = std::slice::from_raw_parts(buf.mData as *const f32, min_frames);
        let mut idx = ch;
        for &s in src.iter() {
            scratch[idx] = s;
            idx += channels;
        }
    }
    sink.push(&scratch[..total], now_ns());
}

thread_local! {
    /// バッファ構成のデバッグ出力を一度だけにするためのフラグ（RT スレッドローカル）。
    static LOGGED_SHAPE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// 最初の IOProc コールバックで一度だけ、interleaved（mNumberBuffers==1）/ planar（>=2）の別を
/// `FLEXAUDIO_DEBUG` 時に stderr へ出す。
fn log_buffer_shape_once(num_buffers: usize) {
    if std::env::var_os("FLEXAUDIO_DEBUG").is_none() {
        return;
    }
    LOGGED_SHAPE.with(|c| {
        if !c.get() {
            c.set(true);
            let kind = if num_buffers == 1 {
                "interleaved"
            } else {
                "planar"
            };
            eprintln!(
                "[flexaudio-os-macos] IOProc buffer shape: mNumberBuffers={num_buffers} ({kind})"
            );
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `new_uuid_string` は 36 文字の UUID 形式（8-4-4-4-12）を返す。
    #[test]
    fn uuid_string_has_expected_shape() {
        let s = new_uuid_string();
        assert_eq!(s.len(), 36);
        assert_eq!(s.matches('-').count(), 4);
    }

    /// object_ids_to_nsarray が要素数を保つ。
    #[test]
    fn object_ids_array_preserves_count() {
        let arr = object_ids_to_nsarray(&[1, 2, 3]);
        assert_eq!(arr.count(), 3);
    }
}
