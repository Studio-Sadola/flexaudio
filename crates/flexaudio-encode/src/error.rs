//! flexaudio-encode のエラー型と結果型。

/// flexaudio-encode の操作で発生しうるエラー。
///
/// 将来バリアントを足せるよう `#[non_exhaustive]`（外部の match は `_ =>` が要る）。
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum EncodeError {
    /// ファイル入出力の失敗。
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// 対応していないパラメータ（チャンネル数・サンプルレート・チャンク長など）。
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// FLAC エンコーダ内部のエラー（説明文付き）。
    #[error("encoder error: {0}")]
    Encoder(String),
}

/// flexaudio-encode 全体で用いる結果型。
pub type Result<T> = std::result::Result<T, EncodeError>;
