//! macOS バージョンゲート。Process Tap は macOS 14.4 以上が必須。
//!
//! `MacSystemBackend` / `MacProcessBackend` の `start` が tap 生成へ進む前にこのチェックを
//! 通し、14.4 未満なら [`Error::UnsupportedOsVersion`] を返す。これが無いと古い OS では
//! `AudioHardwareCreateProcessTap` が raw な `OSStatus` を返し、
//! [`map_os_status`](crate::common::map_os_status) 経由で `Error::Backend` に化けてしまい、
//! 他 OS の `UnsupportedOsVersion` 経路と非対称になる。型でゲートして error 種別を揃える。
//!
//! # 取得方法
//! `objc2-foundation` の `NSProcessInfo` フィーチャを増やさずに済ませるため、Foundation の
//! `[[NSProcessInfo processInfo] operatingSystemVersion]` を `objc2` の `class!` /
//! `msg_send!` で直接呼ぶ。返る `NSOperatingSystemVersion`（major/minor/patch の
//! `NSInteger` 3 連）はレイアウト一致のローカルミラー [`NSOperatingSystemVersion`] で受ける
//! （`Encode`/`RefEncode` を実装して構造体戻り値に対応する）。

use objc2::encode::{Encode, Encoding, RefEncode};
use objc2::ffi::NSInteger;
use objc2::runtime::AnyObject;
use objc2::{class, msg_send};

use flexaudio_core::types::{Error, Result};

/// Process Tap に必要な最小 macOS バージョンの major。
const MIN_MAJOR: i64 = 14;
/// Process Tap に必要な最小 macOS バージョンの minor（14.4）。
const MIN_MINOR: i64 = 4;

/// Foundation の `NSOperatingSystemVersion` とレイアウトを合わせたローカルミラー。
///
/// 3 つの `NSInteger`（64bit ターゲットでは `i64`）から成る `#[repr(C)]` 構造体。`msg_send!`
/// で `operatingSystemVersion` の構造体戻り値を受けるため `Encode`/`RefEncode` を実装する
/// （Foundation 側の `NSOperatingSystemVersion` と同じく無名 struct としてエンコードされる）。
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct NSOperatingSystemVersion {
    major: NSInteger,
    minor: NSInteger,
    patch: NSInteger,
}

// SAFETY: 3 つの NSInteger から成る `#[repr(C)]` 構造体で、Foundation の
// `NSOperatingSystemVersion` と同一レイアウト。エンコードも同じく無名 struct（"?"）。
unsafe impl Encode for NSOperatingSystemVersion {
    const ENCODING: Encoding = Encoding::Struct(
        "?",
        &[
            <NSInteger>::ENCODING,
            <NSInteger>::ENCODING,
            <NSInteger>::ENCODING,
        ],
    );
}

// SAFETY: 上記 `Encode` を持つ型への参照エンコード。
unsafe impl RefEncode for NSOperatingSystemVersion {
    const ENCODING_REF: Encoding = Encoding::Pointer(&Self::ENCODING);
}

/// `(major, minor)` が Process Tap の最小要件（14.4）を満たすか。
///
/// patch は見ない（Apple は 14.4 で導入と告知しており 14.4.x はすべて OK）。テストできるよう
/// OS 呼び出しから切り離した純関数にしてある。
fn meets_min_version(major: i64, minor: i64) -> bool {
    major > MIN_MAJOR || (major == MIN_MAJOR && minor >= MIN_MINOR)
}

/// 実行中の macOS バージョンを `(major, minor)` で取得する。
///
/// `[[NSProcessInfo processInfo] operatingSystemVersion]` を直接送って読む。Foundation は常に
/// リンクされており `NSProcessInfo` クラスは実行時に必ず存在する。
fn current_os_version() -> (i64, i64) {
    // SAFETY: `NSProcessInfo` クラスは Foundation に常在する。`processInfo` は autoreleased な
    // シングルトンを返すが、ここでは即座に operatingSystemVersion を送るだけなので保持は要らない。
    // `operatingSystemVersion` は NSOperatingSystemVersion を返すゼロ引数セレクタで、レイアウトを
    // 合わせたローカルミラーで受ける。
    unsafe {
        let cls = class!(NSProcessInfo);
        let process_info: *mut AnyObject = msg_send![cls, processInfo];
        let version: NSOperatingSystemVersion = msg_send![process_info, operatingSystemVersion];
        (version.major as i64, version.minor as i64)
    }
}

/// Process Tap がこの OS で使えるか確認する。14.4 未満なら
/// [`Error::UnsupportedOsVersion`]、満たせば `Ok(())`。
///
/// 各バックエンドの `start` が tap 生成（CoreAudio 呼び出し）へ進む前に呼ぶ。これで古い OS の
/// 失敗が raw `OSStatus`→`Error::Backend` ではなく型付きの `UnsupportedOsVersion` になる。
pub(crate) fn ensure_process_tap_supported() -> Result<()> {
    let (major, minor) = current_os_version();
    if meets_min_version(major, minor) {
        Ok(())
    } else {
        Err(Error::UnsupportedOsVersion)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 14.3 は要件未満（Unsupported 相当）。
    #[test]
    fn version_14_3_is_unsupported() {
        assert!(!meets_min_version(14, 3));
        assert!(!meets_min_version(14, 0));
        assert!(!meets_min_version(13, 9));
    }

    /// 14.4 ちょうどは要件を満たす（境界）。
    #[test]
    fn version_14_4_is_supported() {
        assert!(meets_min_version(14, 4));
    }

    /// 14.5 / 15.x / 26.x など以降は満たす。
    #[test]
    fn newer_versions_are_supported() {
        assert!(meets_min_version(14, 5));
        assert!(meets_min_version(15, 0));
        assert!(meets_min_version(26, 6));
    }

    /// メジャーが上なら minor が小さくても満たす（15.0 > 14.4）。
    #[test]
    fn higher_major_with_low_minor_is_supported() {
        assert!(meets_min_version(15, 0));
        assert!(meets_min_version(99, 0));
    }

    /// 実行中の OS バージョン取得が panic せず妥当な値（major >= 10）を返すこと。
    /// CI/実機いずれでも macOS なら macOS 10 以降なので major は 2 桁台に入る。
    #[test]
    fn current_os_version_is_sane() {
        let (major, minor) = current_os_version();
        assert!(major >= 10, "unexpected macOS major version: {major}");
        assert!(minor >= 0, "unexpected macOS minor version: {minor}");
    }
}
