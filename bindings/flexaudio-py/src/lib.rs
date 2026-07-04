//! flexaudio-py — Python バインディング (PyO3 + maturin)。flexaudio を直接リンクする。
//!
//! Python アプリが flexaudio をインプロセスで使うためのバインディング。flexaudio-napi
//! （Node 向け）と同じ作法を PyO3 へ翻訳したもの。
//!
//! 設計:
//! - `open(...)` で `flexaudio::open` し、そのまま `start()` まで済ませて [`Stream`] を返す
//!   （napi の `open_stream` と同じく open で start まで行う）。
//! - poll 系（`poll_chunk` / `poll_event`）は非ブロッキングで速いので GIL は解放しない。
//!   pyclass のメソッドは GIL 保持下で呼ばれるので、内部 `flexaudio::Stream` への同時
//!   アクセスは起きない（napi のような bridge スレッドは持たない）。VAD / denoise の統合
//!   加工も poll_chunk が呼ばれたその場（GIL 下）で行う。
//! - チャンクの `data` は interleaved `f32` をリトルエンディアン生バイト（`bytes`）で渡す。
//!   numpy 利用者は `np.frombuffer(chunk.data, dtype=np.float32)` で読む。
//!
//! # モジュール構成
//! 神クラス化を避けて責務ごとにファイルを分ける:
//! - このファイル（`lib.rs`）: 共有ヘルパ（エラー変換・列挙変換）・`devices()`・pymodule 登録。
//! - `marshal`: Python へ渡すデータ型（AudioChunk / StreamEvent / DeviceInfo / VadEvent /
//!   DeviceEvent）とその変換。
//! - `config`: Python 引数 → コアの各種 config（StreamConfig / VadConfig）への変換と検証。
//! - `stream`: 録音ストリーム [`Stream`] と `open()`、VAD / denoise の統合。
//! - `vad` / `denoise` / `encode`: 独立アドオン（[`Vad`] / [`Denoiser`] / [`FlacEncoder`]）。
//! - `watcher`: デバイス着脱監視（[`DeviceWatcher`] と `watch_devices()`）。
//!
//! 実行時にネットワーク通信はしない（PyO3 は Python 拡張ブリッジのみ。埋め込みの VAD
//! モデル・FLAC エンコーダ・RNNoise モデルもファイル／通信を要らない）。

use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;

// 依存クレート `flexaudio` を `fa` で参照する。この cdylib の `[lib] name` と
// `#[pymodule] fn flexaudio` が同名 `flexaudio` を作るため、素の `flexaudio::` は
// クレートとモジュールで衝突しうる。別名にして曖昧さを断つ。
use ::flexaudio as fa;
use fa::{ProcessMode, SourceKind};

mod config;
mod denoise;
mod encode;
mod marshal;
mod stream;
mod vad;
mod watcher;

use marshal::device_info_to_py;

// ---------------------------------------------------------------------------
// エラー変換
// ---------------------------------------------------------------------------

/// flexaudio::Error → Python 例外。引数系は `ValueError`、それ以外は `RuntimeError`。
/// いずれもメッセージ（Display）を保持する。
pub(crate) fn to_py_err(err: fa::Error) -> PyErr {
    let msg = err.to_string();
    match err {
        fa::Error::InvalidArg(_) | fa::Error::UnsupportedFormat(_) => PyValueError::new_err(msg),
        _ => PyRuntimeError::new_err(msg),
    }
}

/// VadError → Python 例外。設定不正は `ValueError`、モデルロード／推論の失敗は
/// `RuntimeError`。
pub(crate) fn vad_err_to_py(err: flexaudio_vad::VadError) -> PyErr {
    let msg = err.to_string();
    match err {
        flexaudio_vad::VadError::InvalidConfig(_) => PyValueError::new_err(msg),
        flexaudio_vad::VadError::ModelLoad(_) | flexaudio_vad::VadError::Inference(_) => {
            PyRuntimeError::new_err(msg)
        }
    }
}

/// DenoiseError → Python 例外。チャンネル数・長さの不正はいずれも引数系なので `ValueError`。
pub(crate) fn denoise_err_to_py(err: flexaudio_denoise::DenoiseError) -> PyErr {
    PyValueError::new_err(err.to_string())
}

/// EncodeError → Python 例外。非対応パラメータは `ValueError`、I/O は `OSError`、
/// エンコーダ内部エラーは `RuntimeError`。将来のバリアント追加（`#[non_exhaustive]`）は
/// `RuntimeError` に倒す。
pub(crate) fn encode_err_to_py(err: flexaudio_encode::EncodeError) -> PyErr {
    use flexaudio_encode::EncodeError;
    let msg = err.to_string();
    match err {
        EncodeError::Unsupported(_) => PyValueError::new_err(msg),
        EncodeError::Io(_) => pyo3::exceptions::PyOSError::new_err(msg),
        _ => PyRuntimeError::new_err(msg),
    }
}

// ---------------------------------------------------------------------------
// 列挙体 ↔ 文字列の変換ヘルパ
// ---------------------------------------------------------------------------

/// [`SourceKind`] を Python 向け文字列（"mic"|"system"|"process"|"mix"）へ。
pub(crate) fn source_kind_str(k: SourceKind) -> &'static str {
    match k {
        SourceKind::Mic => "mic",
        SourceKind::SystemLoopback => "system",
        SourceKind::ProcessLoopback => "process",
        SourceKind::Mix => "mix",
    }
}

/// "mic"|"system"|"process"|"mix" を [`SourceKind`] へ。不正値は `ValueError`。
pub(crate) fn parse_source_kind(s: &str) -> PyResult<SourceKind> {
    match s {
        "mic" => Ok(SourceKind::Mic),
        "system" => Ok(SourceKind::SystemLoopback),
        "process" => Ok(SourceKind::ProcessLoopback),
        "mix" => Ok(SourceKind::Mix),
        other => Err(PyValueError::new_err(format!(
            "unknown kind: {other:?} (expected mic|system|process|mix)"
        ))),
    }
}

/// "include"|"exclude" を [`ProcessMode`] へ（process 専用）。既定は Include。
pub(crate) fn parse_process_mode(s: &str) -> PyResult<ProcessMode> {
    match s {
        "include" => Ok(ProcessMode::Include),
        "exclude" => Ok(ProcessMode::Exclude),
        other => Err(PyValueError::new_err(format!(
            "unknown mode: {other:?} (expected include|exclude)"
        ))),
    }
}

/// Python 風に bool を "True"/"False" で表す（__repr__ 用）。
pub(crate) fn bool_repr(b: bool) -> &'static str {
    if b {
        "True"
    } else {
        "False"
    }
}

// ---------------------------------------------------------------------------
// モジュール関数
// ---------------------------------------------------------------------------

/// 利用可能なデバイスを列挙する。ヘッドレス環境では空リストでも例外にしない。
#[pyfunction]
fn devices() -> PyResult<Vec<marshal::PyDeviceInfo>> {
    let list = fa::devices().map_err(to_py_err)?;
    Ok(list.into_iter().map(device_info_to_py).collect())
}

// ---------------------------------------------------------------------------
// モジュール定義
// ---------------------------------------------------------------------------

/// Python モジュール `flexaudio`。
#[pymodule]
fn flexaudio(m: &Bound<'_, PyModule>) -> PyResult<()> {
    // 関数。
    m.add_function(wrap_pyfunction!(devices, m)?)?;
    m.add_function(wrap_pyfunction!(stream::open, m)?)?;
    m.add_function(wrap_pyfunction!(watcher::watch_devices, m)?)?;

    // ストリームと、そこから返るデータ型。
    m.add_class::<stream::Stream>()?;
    m.add_class::<marshal::PyAudioChunk>()?;
    m.add_class::<marshal::PyStreamEvent>()?;
    m.add_class::<marshal::PyDeviceInfo>()?;
    m.add_class::<marshal::PyVadEvent>()?;
    m.add_class::<marshal::PyDeviceEvent>()?;

    // 独立アドオンと監視。
    m.add_class::<vad::Vad>()?;
    m.add_class::<denoise::Denoiser>()?;
    m.add_class::<encode::FlacEncoder>()?;
    m.add_class::<watcher::DeviceWatcher>()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    //! 共有ヘルパ（列挙変換・エラー変換）の純粋部分を Python ランタイム無しで検証する。
    //!
    //! pyclass を生成する経路（PyBytes 等）や PyDict を要する経路は Python ホストが要る
    //! ので、ここでは Python 非依存の純変換だけを見る。config / marshal / encode の各
    //! モジュールにもそれぞれの純ロジックのテストがある。
    //!
    //! 実 import のスモーク（Python ホスト側・CI 向け）:
    //! ```sh
    //! cargo build -p flexaudio-py --features extension-module
    //! cp target/debug/libflexaudio.so /tmp/fa/flexaudio.so
    //! python3 - <<'PY'
    //! import sys; sys.path.insert(0, "/tmp/fa")
    //! import flexaudio as fa
    //! assert fa.Vad().process([0.0]*16000, 16000, 1) == []      # 無音は発話なし
    //! d = fa.Denoiser(1); assert len(d.process([0.0]*1000)) == 1000; assert len(d.flush()) == 480
    //! fa.watch_devices().poll_event()                           # None か DeviceEvent
    //! try: fa.open("mic", denoise=True, output_rate=16000); assert False
    //! except ValueError: pass                                   # denoise は 48k 専用
    //! PY
    //! ```
    //! （maturin があれば `maturin develop` でも同じことができる。）

    use super::*;

    #[test]
    fn source_kind_roundtrips() {
        for (s, k) in [
            ("mic", SourceKind::Mic),
            ("system", SourceKind::SystemLoopback),
            ("process", SourceKind::ProcessLoopback),
            ("mix", SourceKind::Mix),
        ] {
            assert_eq!(parse_source_kind(s).unwrap(), k);
            assert_eq!(source_kind_str(k), s);
        }
    }

    #[test]
    fn parse_source_kind_rejects_unknown() {
        assert!(parse_source_kind("bogus").is_err());
    }

    #[test]
    fn parse_process_mode_defaults_and_explicit() {
        assert_eq!(parse_process_mode("include").unwrap(), ProcessMode::Include);
        assert_eq!(parse_process_mode("exclude").unwrap(), ProcessMode::Exclude);
        assert!(parse_process_mode("nope").is_err());
    }

    #[test]
    fn bool_repr_matches_python() {
        assert_eq!(bool_repr(true), "True");
        assert_eq!(bool_repr(false), "False");
    }

    #[test]
    fn to_py_err_maps_variants_without_panic() {
        // 種別判定は Python ランタイム不要（match のみ）。メッセージは Display 由来。
        let err = fa::Error::DeviceNotFound;
        assert!(err.to_string().contains("device not found"));
        // 変換自体が panic しないことだけ確認（PyErr の中身は Python 要）。
        let _ = to_py_err(fa::Error::DeviceNotFound);
        let _ = to_py_err(fa::Error::InvalidArg("x".to_string()));
    }

    #[test]
    fn addon_err_maps_without_panic() {
        // アドオン系のエラー変換も panic しないこと（種別分岐のみ）。
        let _ = vad_err_to_py(flexaudio_vad::VadError::InvalidConfig("x".into()));
        let _ = vad_err_to_py(flexaudio_vad::VadError::ModelLoad("x".into()));
        let _ = denoise_err_to_py(flexaudio_denoise::DenoiseError::InvalidChannels(3));
        let _ = encode_err_to_py(flexaudio_encode::EncodeError::Unsupported("x".into()));
    }
}
