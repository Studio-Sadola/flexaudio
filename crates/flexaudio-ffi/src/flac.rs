//! FLAC 逐次書き出しアドオンの独立ハンドルと C ABI。
//!
//! [`FlexFlac`] は録音チャンク（interleaved f32）を可逆圧縮しながらファイルへ流す
//! 不透明ハンドルで、内部で [`flexaudio_encode::FlacWriter`] を駆動する。`split_seconds` を
//! 与えると、書き込んだフレーム数がしきい値に達するたびに連番ファイル（`name-001.flac`,
//! `name-002.flac`, …）へローテーションする（CLI の `--split-seconds` と同じ流儀）。
//!
//! 流儀はクレート全体と同じ（guard で panic を吸収・NULL 検査・失敗は last_error）。

use std::ffi::CStr;
use std::os::raw::c_char;
use std::path::{Path, PathBuf};
use std::slice;

use flexaudio_encode::{EncodeError, FlacWriter};

use crate::error::{clear_last_error, code, set_last_error};
use crate::{guard_i32, guard_ptr};

/// FLAC ライターの対応サンプルレート上限（Hz）。[`flexaudio_encode::FlacWriter`] が
/// flacenc の検証に合わせて 96kHz までに制限しているのと揃える（create で先に弾く）。
const MAX_SAMPLE_RATE: u32 = 96_000;

/// 分割録音の `index` 番目（1 始まり）のファイルパスを作る（純関数）。
///
/// `rec.flac` なら `rec-001.flac, rec-002.flac, …` のように拡張子の前へ 3 桁ゼロ詰め連番を
/// 挟む。1000 以降は桁が自然に増える。拡張子が無いパスは末尾に連番を足す。親ディレクトリは
/// 保たれる（CLI の `split_file_path` と同じ規則）。
fn split_file_path(base: &Path, index: u64) -> PathBuf {
    let stem = base
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = match base.extension() {
        Some(ext) => format!("{stem}-{index:03}.{}", ext.to_string_lossy()),
        None => format!("{stem}-{index:03}"),
    };
    base.with_file_name(name)
}

/// ローテーション付き FLAC 書き出し器。`split_seconds = 0` なら単一ファイル。
///
/// ファイルは最初のチャンクが来るまで開かない（遅延生成）ので、ちょうど境界で終わっても
/// 空の末尾ファイルは残らない（CLI の `RotatingWavWriter` と同じ）。境界はチャンク粒度の
/// 「以上になったら次へ」で、各ファイルは指定秒より最大 1 チャンク長くなりうる。
struct RotatingFlac {
    /// ベースパス（分割時は連番の元・分割なしはこのまま使う）。
    base: PathBuf,
    sample_rate: u32,
    channels: u16,
    /// 1 ファイルあたりのフレーム数しきい値（split_seconds × rate）。0 = 分割なし。
    frames_per_file: u64,
    /// 現在書き込み中のライター（遅延生成。ローテ直後や書き込み前は None）。
    writer: Option<FlacWriter>,
    /// 現在のファイルへ書き込んだフレーム数（ローテで 0 に戻る）。
    frames_in_current: u64,
    /// 次に開く分割ファイルの連番（1 始まり）。
    file_index: u64,
    /// finalize 済みなら以後の write を弾く。
    finalized: bool,
}

impl RotatingFlac {
    fn new(base: PathBuf, sample_rate: u32, channels: u16, split_seconds: u32) -> RotatingFlac {
        RotatingFlac {
            base,
            sample_rate,
            channels,
            frames_per_file: u64::from(split_seconds) * u64::from(sample_rate),
            writer: None,
            frames_in_current: 0,
            file_index: 1,
            finalized: false,
        }
    }

    /// 現在書くべきファイルパス（分割なしは base、分割ありは連番）。
    fn current_path(&self) -> PathBuf {
        if self.frames_per_file == 0 {
            self.base.clone()
        } else {
            split_file_path(&self.base, self.file_index)
        }
    }

    /// interleaved f32 を書く。長さはチャンネル数の倍数であること。境界に達したら
    /// 現在のファイルを finalize して連番を進める。
    fn write(&mut self, samples: &[f32]) -> Result<(), EncodeError> {
        let ch = self.channels as usize;
        if samples.is_empty() {
            // 空は no-op（ファイルを開かない＝空ファイルを作らない）。
            return Ok(());
        }
        if !samples.len().is_multiple_of(ch) {
            return Err(EncodeError::Unsupported(format!(
                "chunk length {} is not a multiple of channels {ch}",
                samples.len()
            )));
        }

        // 遅延生成: このチャンクで初めて現在ファイルを開く。
        if self.writer.is_none() {
            let path = self.current_path();
            self.writer = Some(FlacWriter::create(&path, self.sample_rate, self.channels)?);
        }
        // writer は直前に必ず用意済み。
        self.writer
            .as_mut()
            .expect("writer は直前に生成済み")
            .write_chunk(samples)?;

        self.frames_in_current += (samples.len() / ch) as u64;

        // しきい値に達したら現在ファイルを閉じ、次チャンクから次ファイルへ。
        if self.frames_per_file > 0 && self.frames_in_current >= self.frames_per_file {
            if let Some(w) = self.writer.take() {
                w.finalize()?;
            }
            self.file_index += 1;
            self.frames_in_current = 0;
        }
        Ok(())
    }

    /// 端数を書き切り、現在のファイルを確定して閉じる。以後 write は不可。
    fn finalize(&mut self) -> Result<(), EncodeError> {
        let result = match self.writer.take() {
            Some(w) => w.finalize(),
            None => Ok(()),
        };
        self.finalized = true;
        result
    }
}

/// FLAC 書き出しの不透明ハンドル。`flexaudio_flac_create` で作り、`flexaudio_flac_write` で
/// チャンクを追記し、`flexaudio_flac_finalize` で確定、`flexaudio_flac_free` で解放する。
pub struct FlexFlac {
    inner: RotatingFlac,
}

/// EncodeError をエラーコードへ写す（引数由来は InvalidArg・それ以外は Failure）。
fn flac_err(e: EncodeError) -> i32 {
    let is_arg = matches!(e, EncodeError::Unsupported(_));
    set_last_error(e.to_string());
    if is_arg {
        code::FLEX_INVALID_ARG
    } else {
        code::FLEX_FAILURE
    }
}

/// `path` に FLAC 書き出しを開く。`split_seconds = 0` で単一ファイル、1 以上で
/// `split_seconds` 秒ごとに `name-001.flac` 連番へローテーションする。
///
/// 失敗（NULL / 不正な UTF-8 パス / 非対応の `sr`・`ch`）で NULL を返し last_error を
/// セットする。`ch` は 1..=2、`sr` は 1..=96000 Hz。返ったハンドルは
/// `flexaudio_flac_free` で解放する（`flexaudio_flac_finalize` を呼ばずに free しても
/// ベストエフォートで閉じる）。
///
/// # Safety
/// `path` は有効な NUL 終端 C 文字列（UTF-8）を指していなければならない（NULL は失敗扱い）。
#[no_mangle]
pub unsafe extern "C" fn flexaudio_flac_create(
    path: *const c_char,
    sr: u32,
    ch: u16,
    split_seconds: u32,
) -> *mut FlexFlac {
    guard_ptr(|| {
        clear_last_error();
        if path.is_null() {
            set_last_error("flexaudio_flac_create: path pointer is null");
            return std::ptr::null_mut();
        }
        let path = match CStr::from_ptr(path).to_str() {
            Ok(s) => PathBuf::from(s),
            Err(_) => {
                set_last_error("flexaudio_flac_create: path is not valid UTF-8");
                return std::ptr::null_mut();
            }
        };
        // create 時に早めに弾く（FlacWriter::create と同じ範囲。ファイルは作らない）。
        if !(1..=2).contains(&ch) {
            set_last_error(format!(
                "flexaudio_flac_create: channels must be 1 or 2, got {ch}"
            ));
            return std::ptr::null_mut();
        }
        if !(1..=MAX_SAMPLE_RATE).contains(&sr) {
            set_last_error(format!(
                "flexaudio_flac_create: sample rate must be 1..={MAX_SAMPLE_RATE} Hz, got {sr}"
            ));
            return std::ptr::null_mut();
        }
        let inner = RotatingFlac::new(path, sr, ch, split_seconds);
        Box::into_raw(Box::new(FlexFlac { inner }))
    })
}

/// interleaved f32（長さ = フレーム数 × チャンネル数）を追記する。
///
/// `len` はチャンネル数の倍数であること（倍数でなければ InvalidArg）。`len=0` は no-op。
/// finalize 済みのハンドルへの write は [`FLEX_INVALID_STATE`](code::FLEX_INVALID_STATE)。
/// 戻り 0 = 成功 / 負 = エラー。
///
/// # Safety
/// `f` は有効なハンドル、`samples` は `len` 要素の有効な配列（`len=0` なら NULL 可）で
/// なければならない。
#[no_mangle]
pub unsafe extern "C" fn flexaudio_flac_write(
    f: *mut FlexFlac,
    samples: *const f32,
    len: usize,
) -> i32 {
    guard_i32(|| {
        clear_last_error();
        let Some(flac) = f.as_mut() else {
            set_last_error("flexaudio_flac_write: flac pointer is null");
            return code::FLEX_INVALID_ARG;
        };
        if flac.inner.finalized {
            set_last_error("flexaudio_flac_write: writer is already finalized");
            return code::FLEX_INVALID_STATE;
        }
        let input: &[f32] = if len == 0 {
            &[]
        } else if samples.is_null() {
            set_last_error("flexaudio_flac_write: samples pointer is null");
            return code::FLEX_INVALID_ARG;
        } else {
            slice::from_raw_parts(samples, len)
        };
        match flac.inner.write(input) {
            Ok(()) => code::FLEX_OK,
            Err(e) => flac_err(e),
        }
    })
}

/// 端数を書き切り、現在のファイルを確定して閉じる。以後の write は InvalidState。
///
/// 二重 finalize は安全（no-op で 0 を返す）。戻り 0 = 成功 / 負 = エラー。
///
/// # Safety
/// `f` は有効なハンドルでなければならない（NULL は InvalidArg）。
#[no_mangle]
pub unsafe extern "C" fn flexaudio_flac_finalize(f: *mut FlexFlac) -> i32 {
    guard_i32(|| {
        clear_last_error();
        let Some(flac) = f.as_mut() else {
            set_last_error("flexaudio_flac_finalize: flac pointer is null");
            return code::FLEX_INVALID_ARG;
        };
        if flac.inner.finalized {
            // 既に確定済みなら何もしない（冪等）。
            return code::FLEX_OK;
        }
        match flac.inner.finalize() {
            Ok(()) => code::FLEX_OK,
            Err(e) => flac_err(e),
        }
    })
}

/// FLAC ハンドルを解放する。NULL 安全。
///
/// finalize せずに free した場合も、内部の [`FlacWriter`] が drop 時にベストエフォートで
/// 端数書き出しとヘッダ確定を試みる（エラーは握り潰す。確実に検知したいなら先に
/// `flexaudio_flac_finalize` を呼ぶ）。
///
/// # Safety
/// `f` は `flexaudio_flac_create` が返したハンドル（または NULL）でなければならない。
/// 解放後の `f` を使ってはならない。
#[no_mangle]
pub unsafe extern "C" fn flexaudio_flac_free(f: *mut FlexFlac) {
    guard_i32(|| {
        if !f.is_null() {
            drop(Box::from_raw(f));
        }
        code::FLEX_OK
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;
    use std::fs;

    // split_file_path は CLI と同じ連番規則（回帰防止のため代表点を固定）。
    #[test]
    fn split_file_path_inserts_padded_index() {
        assert_eq!(
            split_file_path(Path::new("rec.flac"), 1),
            PathBuf::from("rec-001.flac")
        );
        assert_eq!(
            split_file_path(Path::new("rec.flac"), 42),
            PathBuf::from("rec-042.flac")
        );
        assert_eq!(
            split_file_path(Path::new("/tmp/dir/rec.flac"), 3),
            PathBuf::from("/tmp/dir/rec-003.flac")
        );
        // 拡張子なし。
        assert_eq!(
            split_file_path(Path::new("rec"), 2),
            PathBuf::from("rec-002")
        );
    }

    #[test]
    fn rotating_frames_per_file_reflects_split_seconds() {
        // split_seconds × rate = 1 ファイルのフレーム数しきい値。0 は分割なし。
        let r = RotatingFlac::new(PathBuf::from("x.flac"), 48_000, 2, 5);
        assert_eq!(r.frames_per_file, 5 * 48_000);
        let single = RotatingFlac::new(PathBuf::from("x.flac"), 48_000, 2, 0);
        assert_eq!(single.frames_per_file, 0);
        assert_eq!(single.current_path(), PathBuf::from("x.flac"));
    }

    /// 一意な一時パス（プロセス ID + ラベルで衝突回避。テスト後に必ず消す）。
    fn temp_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("flexffi_{}_{}.flac", std::process::id(), label))
    }

    #[test]
    fn create_write_finalize_and_write_after_finalize_is_invalid_state() {
        let path = temp_path("single");
        let _ = fs::remove_file(&path);
        let cpath = CString::new(path.to_str().unwrap()).unwrap();

        let f = unsafe { flexaudio_flac_create(cpath.as_ptr(), 48_000, 1, 0) };
        assert!(!f.is_null());

        // 1 ブロック分（4096 フレーム mono）を書く。
        let samples = vec![0.0f32; 4096];
        assert_eq!(
            unsafe { flexaudio_flac_write(f, samples.as_ptr(), samples.len()) },
            code::FLEX_OK
        );
        assert_eq!(unsafe { flexaudio_flac_finalize(f) }, code::FLEX_OK);
        // finalize 後の write は InvalidState。
        assert_eq!(
            unsafe { flexaudio_flac_write(f, samples.as_ptr(), samples.len()) },
            code::FLEX_INVALID_STATE
        );
        // 二重 finalize は冪等。
        assert_eq!(unsafe { flexaudio_flac_finalize(f) }, code::FLEX_OK);
        unsafe { flexaudio_flac_free(f) };

        assert!(path.exists(), "FLAC ファイルが作られているはず");
        assert!(fs::metadata(&path).unwrap().len() > 0, "空でないはず");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn split_rotates_to_numbered_files() {
        let base = temp_path("split");
        let f1 = split_file_path(&base, 1);
        let f2 = split_file_path(&base, 2);
        let _ = fs::remove_file(&f1);
        let _ = fs::remove_file(&f2);
        let cpath = CString::new(base.to_str().unwrap()).unwrap();

        // split_seconds=1 @48k mono → 48000 フレームで 1 ファイル。
        let f = unsafe { flexaudio_flac_create(cpath.as_ptr(), 48_000, 1, 1) };
        assert!(!f.is_null());
        // ちょうどしきい値に達する量を書く → file-001 が閉じて連番が進む。
        let block = vec![0.0f32; 48_000];
        assert_eq!(
            unsafe { flexaudio_flac_write(f, block.as_ptr(), block.len()) },
            code::FLEX_OK
        );
        // 次のチャンクで file-002 を開く。
        let block2 = vec![0.0f32; 4096];
        assert_eq!(
            unsafe { flexaudio_flac_write(f, block2.as_ptr(), block2.len()) },
            code::FLEX_OK
        );
        assert_eq!(unsafe { flexaudio_flac_finalize(f) }, code::FLEX_OK);
        unsafe { flexaudio_flac_free(f) };

        assert!(f1.exists(), "1 本目 {f1:?} が作られているはず");
        assert!(f2.exists(), "2 本目 {f2:?} が作られているはず");
        let _ = fs::remove_file(&f1);
        let _ = fs::remove_file(&f2);
    }

    #[test]
    fn create_rejects_bad_params_and_null() {
        // NULL パス。
        assert!(unsafe { flexaudio_flac_create(std::ptr::null(), 48_000, 1, 0) }.is_null());
        // 非対応チャンネル / サンプルレート。
        let p = CString::new("/tmp/does_not_matter.flac").unwrap();
        assert!(unsafe { flexaudio_flac_create(p.as_ptr(), 48_000, 3, 0) }.is_null());
        assert!(unsafe { flexaudio_flac_create(p.as_ptr(), 0, 1, 0) }.is_null());
        // NULL ハンドル操作は InvalidArg / free は安全。
        assert_eq!(
            unsafe { flexaudio_flac_write(std::ptr::null_mut(), std::ptr::null(), 0) },
            code::FLEX_INVALID_ARG
        );
        assert_eq!(
            unsafe { flexaudio_flac_finalize(std::ptr::null_mut()) },
            code::FLEX_INVALID_ARG
        );
        unsafe { flexaudio_flac_free(std::ptr::null_mut()) };
    }
}
