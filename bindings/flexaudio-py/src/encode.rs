//! FLAC 逐次書き出しクラス [`FlacEncoder`]。
//!
//! 録音チャンクを逐次 FLAC ファイルへ圧縮保存するアドオン（[`flexaudio_encode`]）の Python
//! 露出。`split_seconds>0` で連番ファイル（`name-001.flac`, `name-002.flac`, ...）へ分割
//! ローテーションする（境界はフレーム数ベースで、チャンクは分割せず取りこぼしも無い）。

use std::path::{Path, PathBuf};

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

use flexaudio_encode::FlacWriter;

use crate::encode_err_to_py;

/// FLAC 形式が flacenc の検証で許す最大サンプルレート（Hz）。[`FlacWriter::create`] と同値。
const MAX_SAMPLE_RATE: u32 = 96_000;

/// 分割録音の `index` 番目（1 始まり）のファイルパスを作る（純関数）。
///
/// `rec.flac` なら `rec-001.flac, rec-002.flac, ...` のように拡張子の前へ 3 桁ゼロ詰め
/// 連番を挟む。1000 番目以降は桁が自然に増える。拡張子が無いパス（`rec`）は末尾に連番を
/// 足す（`rec-001`）。親ディレクトリは保たれる。flexaudio-cli の `split_file_path` と同じ流儀。
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

/// 録音チャンクを逐次 FLAC へ書き出すエンコーダ。
///
/// [`write_chunk`](FlacEncoder::write_chunk) に interleaved f32（flexaudio の
/// `AudioChunk.data` と同じ形）を流し、終わったら [`finalize`](FlacEncoder::finalize) で
/// ヘッダを確定する。context manager（`with`）にも対応し、`with` を抜けるとき finalize する。
///
/// `split_seconds>0` を指定すると、書き込んだフレーム数が `split_seconds × sample_rate` に
/// 達するたびに現在のファイルを finalize して次の連番ファイルへ切り替える（各ファイルは
/// 指定秒より最大 1 チャンク長くなりうる。チャンクは分割せず取りこぼしも無い）。
///
/// ファイルは最初のチャンクが来るまで開かない（遅延生成）。分割なしで一度も書かずに
/// finalize した場合は空ファイルを作らない。finalize を呼ばずに破棄しても、下層の
/// `FlacWriter` の Drop がベストエフォートで閉じる（確実に検知したいなら finalize を呼ぶ）。
#[pyclass(module = "flexaudio", name = "FlacEncoder")]
pub struct FlacEncoder {
    // `--out` に相当するベースパス（分割時は連番の元、分割なしはこのまま使う）。
    base: PathBuf,
    sample_rate: u32,
    channels: u16,
    // 1 ファイルあたりのフレーム数しきい値（split_seconds × sample_rate）。0 = 分割なし。
    frames_per_file: u64,
    // 現在書き込み中のライター（遅延生成。ローテーション直後や書き込み前は None）。
    writer: Option<FlacWriter>,
    // 現在のファイルへ書き込んだフレーム数（ローテーションで 0 に戻る）。
    frames_in_current: u64,
    // これまでに開いたファイル数（次の連番を決めるのに使う）。
    files_opened: u64,
    // finalize 済みなら以後の write_chunk を拒否する。
    finalized: bool,
}

impl FlacEncoder {
    /// 次に開くファイルのパス。分割なしはベースパスそのまま、分割ありは 1 始まり連番。
    fn next_path(&self) -> PathBuf {
        if self.frames_per_file > 0 {
            split_file_path(&self.base, self.files_opened + 1)
        } else {
            self.base.clone()
        }
    }

    /// 書き込み先ファイルが未オープンなら開く（遅延生成）。
    fn ensure_writer(&mut self) -> PyResult<()> {
        if self.writer.is_none() {
            let path = self.next_path();
            let writer = FlacWriter::create(&path, self.sample_rate, self.channels)
                .map_err(encode_err_to_py)?;
            self.writer = Some(writer);
            self.files_opened += 1;
        }
        Ok(())
    }

    /// 現在のファイルを finalize して閉じる（開いていなければ何もしない）。
    fn finalize_current(&mut self) -> PyResult<()> {
        if let Some(writer) = self.writer.take() {
            writer.finalize().map_err(encode_err_to_py)?;
        }
        Ok(())
    }
}

#[pymethods]
impl FlacEncoder {
    /// `path` に 16bit FLAC を書くエンコーダを作る。`channels` は 1..=2、`sample_rate` は
    /// 1..=96000 Hz（範囲外は `ValueError`）。`split_seconds>0` で連番分割ローテーション。
    ///
    /// この時点ではファイルを開かない（最初の `write_chunk` で開く）。
    #[new]
    #[pyo3(signature = (path, sample_rate, channels, split_seconds = 0))]
    fn new(path: PathBuf, sample_rate: u32, channels: u16, split_seconds: u64) -> PyResult<Self> {
        // 下層 FlacWriter::create と同じ範囲を、ファイルを作る前にここで検証する
        // （遅延生成なので不正パラメータでも空ファイルを残さない）。
        if !(1..=2).contains(&channels) {
            return Err(pyo3::exceptions::PyValueError::new_err(format!(
                "channels must be 1 or 2, got {channels}"
            )));
        }
        if !(1..=MAX_SAMPLE_RATE).contains(&sample_rate) {
            return Err(pyo3::exceptions::PyValueError::new_err(format!(
                "sample rate must be 1..={MAX_SAMPLE_RATE} Hz, got {sample_rate}"
            )));
        }
        Ok(FlacEncoder {
            base: path,
            sample_rate,
            channels,
            // split_seconds × sample_rate。桁溢れは飽和で握る（現実的な値では起きない）。
            frames_per_file: split_seconds.saturating_mul(u64::from(sample_rate)),
            writer: None,
            frames_in_current: 0,
            files_opened: 0,
            finalized: false,
        })
    }

    /// interleaved f32 サンプルを追記する。長さは `channels` の倍数であること（そうでなければ
    /// `ValueError`）。`samples` は list / array.array / numpy 配列いずれも渡せる。空は no-op。
    ///
    /// 書き込み後にフレーム数が `split_seconds × sample_rate` 以上になったら、その場で
    /// 現在のファイルを finalize して次の連番ファイルへローテーションする。
    fn write_chunk(&mut self, samples: Vec<f32>) -> PyResult<()> {
        if self.finalized {
            return Err(PyRuntimeError::new_err(
                "FlacEncoder is already finalized; cannot write more chunks",
            ));
        }
        if samples.is_empty() {
            return Ok(());
        }
        self.ensure_writer()?;
        {
            let writer = self.writer.as_mut().expect("writer は直前で開いている");
            // 長さがチャンネル数の倍数でなければ FlacWriter が Unsupported を返す（ここで弾く）。
            writer.write_chunk(&samples).map_err(encode_err_to_py)?;
        }
        // 上の write_chunk が成功した＝長さは channels の倍数。フレーム数を積む。
        let frames = (samples.len() / self.channels as usize) as u64;
        self.frames_in_current += frames;

        if self.frames_per_file > 0 && self.frames_in_current >= self.frames_per_file {
            // 現在のファイルを確定して次の連番へ（次の write_chunk が新ファイルを開く）。
            self.finalize_current()?;
            self.frames_in_current = 0;
        }
        Ok(())
    }

    /// 端数フレームの書き出しとヘッダ確定を行い、現在のファイルを閉じる。二重呼び出し安全
    /// （2 回目以降は no-op）。
    fn finalize(&mut self) -> PyResult<()> {
        self.finalize_current()?;
        self.finalized = true;
        Ok(())
    }

    /// context manager 対応。`with flexaudio.FlacEncoder(...) as enc:` で使える。
    fn __enter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    /// `with` ブロックを抜けるとき finalize する。ブロック内で例外が起きていなければ
    /// finalize のエラーも伝播する（例外発生中は元の例外を隠さないよう best-effort）。
    fn __exit__(
        &mut self,
        exc_type: Option<Bound<'_, PyAny>>,
        _exc_value: Option<Bound<'_, PyAny>>,
        _traceback: Option<Bound<'_, PyAny>>,
    ) -> PyResult<bool> {
        let result = self.finalize_current();
        self.finalized = true;
        // ブロックが正常終了なら finalize のエラーを伝える。例外発生中は元の例外を優先し、
        // finalize の失敗は握る（下層 Drop はもう走らない＝ take 済み）。
        if exc_type.is_none() {
            result?;
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_file_path_inserts_zero_padded_index() {
        assert_eq!(
            split_file_path(Path::new("rec.flac"), 1),
            PathBuf::from("rec-001.flac")
        );
        assert_eq!(
            split_file_path(Path::new("rec.flac"), 2),
            PathBuf::from("rec-002.flac")
        );
        // 1000 番目以降は桁が自然に増える。
        assert_eq!(
            split_file_path(Path::new("rec.flac"), 1000),
            PathBuf::from("rec-1000.flac")
        );
        // 親ディレクトリは保たれる。
        assert_eq!(
            split_file_path(Path::new("/tmp/out/rec.flac"), 3),
            PathBuf::from("/tmp/out/rec-003.flac")
        );
        // 拡張子が無ければ末尾に連番を足す。
        assert_eq!(
            split_file_path(Path::new("rec"), 5),
            PathBuf::from("rec-005")
        );
    }

    #[test]
    fn frames_per_file_reflects_split_seconds() {
        // split_seconds × sample_rate がしきい値。0 は分割なし。
        let enc = FlacEncoder::new(PathBuf::from("x.flac"), 48_000, 2, 0).unwrap();
        assert_eq!(enc.frames_per_file, 0);

        let enc = FlacEncoder::new(PathBuf::from("x.flac"), 48_000, 2, 10).unwrap();
        assert_eq!(enc.frames_per_file, 480_000);

        let enc = FlacEncoder::new(PathBuf::from("x.flac"), 16_000, 1, 3).unwrap();
        assert_eq!(enc.frames_per_file, 48_000);
    }

    #[test]
    fn new_rejects_out_of_range_params() {
        assert!(FlacEncoder::new(PathBuf::from("x.flac"), 48_000, 0, 0).is_err());
        assert!(FlacEncoder::new(PathBuf::from("x.flac"), 48_000, 3, 0).is_err());
        assert!(FlacEncoder::new(PathBuf::from("x.flac"), 0, 2, 0).is_err());
        assert!(FlacEncoder::new(PathBuf::from("x.flac"), MAX_SAMPLE_RATE + 1, 2, 0).is_err());
        // 範囲内は通る。
        assert!(FlacEncoder::new(PathBuf::from("x.flac"), 48_000, 2, 0).is_ok());
    }
}
