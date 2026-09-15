//! プロセス列挙結果（[`ProcessInfo`]）の OS 非依存な後処理。
//!
//! 各 OS バックエンドは「見つけたまま」の生リスト（同じ PID が複数セッション／ノードで
//! 重複し得る・名前が空のこともある）を返す。facade はそれをここへ通して、全 OS で同じ
//! 形に揃える:
//!
//! 1. `pid == 0` と除外 PID（既定は呼び出し元プロセス自身）を落とす。
//! 2. 同じ PID を 1 件へ統合する（出力中フラグは「どれか 1 つでも出力中なら出力中」）。
//! 3. 表示名を必ず非空にする（名乗り名 → 実行ファイル名 → bundle ID → `pid <N>`）。
//! 4. 出力中を先頭に、表示名（大小無視）→ PID の順で並べる（呼ぶたびに同じ順序）。

use std::collections::BTreeMap;

use crate::types::ProcessInfo;

/// パス文字列から実行ファイルのベース名を取り出す（`/` と `\` の両方を区切りとみなす）。
///
/// 末尾の区切りは無視する。ベース名が空になるときは `None`。
///
/// ```
/// use flexaudio_core::process_list::executable_basename;
/// assert_eq!(executable_basename("/usr/bin/firefox").as_deref(), Some("firefox"));
/// assert_eq!(executable_basename(r"C:\Program Files\App\app.exe").as_deref(), Some("app.exe"));
/// assert_eq!(executable_basename(""), None);
/// ```
pub fn executable_basename(path: &str) -> Option<String> {
    path.split(['/', '\\'])
        .rev()
        .map(str::trim)
        .find(|segment| !segment.is_empty())
        .map(str::to_string)
}

/// 出力中フラグを統合する。どれかが `Some(true)` なら `Some(true)`、どちらかが
/// `Some(false)` なら `Some(false)`、両方 `None` なら `None`（不明のまま）。
fn merge_activity(a: Option<bool>, b: Option<bool>) -> Option<bool> {
    match (a, b) {
        (Some(true), _) | (_, Some(true)) => Some(true),
        (Some(false), _) | (_, Some(false)) => Some(false),
        (None, None) => None,
    }
}

/// 空白だけの文字列を `None` に寄せる。
fn non_blank(value: Option<String>) -> Option<String> {
    value.and_then(|v| {
        let trimmed = v.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

/// 同じ PID の 2 件目以降を 1 件目へ畳み込む（空欄だけを埋め、既存の値は上書きしない）。
fn merge_into(kept: &mut ProcessInfo, other: ProcessInfo) {
    if kept.name.trim().is_empty() {
        kept.name = other.name;
    }
    if kept.executable.is_none() {
        kept.executable = other.executable;
    }
    if kept.bundle_id.is_none() {
        kept.bundle_id = other.bundle_id;
    }
    kept.is_output_active = merge_activity(kept.is_output_active, other.is_output_active);
}

/// 表示名を決める（名乗り名 → 実行ファイル名 → bundle ID → `pid <N>`）。
fn display_name(info: &ProcessInfo) -> String {
    let own = info.name.trim();
    if !own.is_empty() {
        return own.to_string();
    }
    info.executable
        .clone()
        .or_else(|| info.bundle_id.clone())
        .unwrap_or_else(|| format!("pid {}", info.pid))
}

/// 生のプロセスリストを正規化する（重複統合・除外・表示名補完・安定ソート）。
///
/// `exclude_pid` に `Some(pid)` を渡すとその PID を落とす（facade は呼び出し元プロセス
/// 自身＝`std::process::id()` を渡す）。`pid == 0` は常に落とす（どの OS でも
/// 「プロセスではない」を表す値で、`target_pid` にも使えない）。
pub fn normalize_process_list(raw: Vec<ProcessInfo>, exclude_pid: Option<u32>) -> Vec<ProcessInfo> {
    let mut by_pid: BTreeMap<u32, ProcessInfo> = BTreeMap::new();
    for mut info in raw {
        if info.pid == 0 || Some(info.pid) == exclude_pid {
            continue;
        }
        info.executable = non_blank(info.executable);
        info.bundle_id = non_blank(info.bundle_id);
        match by_pid.get_mut(&info.pid) {
            Some(kept) => merge_into(kept, info),
            None => {
                by_pid.insert(info.pid, info);
            }
        }
    }

    let mut out: Vec<ProcessInfo> = by_pid
        .into_values()
        .map(|mut info| {
            info.name = display_name(&info);
            info
        })
        .collect();
    // 出力中（Some(true)）を先頭へ。残りは表示名（大小無視）→ PID で決定的に並べる。
    out.sort_by(|a, b| {
        let a_active = a.is_output_active == Some(true);
        let b_active = b.is_output_active == Some(true);
        b_active
            .cmp(&a_active)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.pid.cmp(&b.pid))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(pid: u32, name: &str) -> ProcessInfo {
        ProcessInfo {
            pid,
            name: name.to_string(),
            executable: None,
            bundle_id: None,
            is_output_active: None,
        }
    }

    #[test]
    fn basename_handles_both_separators_and_edge_cases() {
        assert_eq!(
            executable_basename("/usr/lib/firefox/firefox").as_deref(),
            Some("firefox")
        );
        assert_eq!(
            executable_basename(r"C:\Windows\System32\svchost.exe").as_deref(),
            Some("svchost.exe")
        );
        assert_eq!(executable_basename("plain").as_deref(), Some("plain"));
        assert_eq!(
            executable_basename("/trailing/slash/").as_deref(),
            Some("slash")
        );
        assert_eq!(
            executable_basename("/Applications/Music.app/Contents/MacOS/Music").as_deref(),
            Some("Music")
        );
        assert_eq!(executable_basename(""), None);
        assert_eq!(executable_basename("///"), None);
    }

    #[test]
    fn drops_pid_zero_and_excluded_pid() {
        let out = normalize_process_list(
            vec![info(0, "idle"), info(42, "self"), info(7, "app")],
            Some(42),
        );
        assert_eq!(out.iter().map(|p| p.pid).collect::<Vec<_>>(), vec![7]);
    }

    #[test]
    fn without_exclusion_keeps_every_nonzero_pid() {
        let out = normalize_process_list(vec![info(42, "self"), info(7, "app")], None);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn duplicates_merge_activity_and_fill_blanks() {
        let a = ProcessInfo {
            is_output_active: Some(false),
            ..info(5, "")
        };
        let b = ProcessInfo {
            executable: Some("player".into()),
            is_output_active: Some(true),
            ..info(5, "Player")
        };
        let c = ProcessInfo {
            bundle_id: Some("com.example.player".into()),
            ..info(5, "Other name")
        };
        let out = normalize_process_list(vec![a, b, c], None);
        assert_eq!(out.len(), 1);
        let p = &out[0];
        assert_eq!(p.name, "Player", "the first non-empty name wins");
        assert_eq!(p.executable.as_deref(), Some("player"));
        assert_eq!(p.bundle_id.as_deref(), Some("com.example.player"));
        assert_eq!(
            p.is_output_active,
            Some(true),
            "any active session makes the process active"
        );
    }

    #[test]
    fn merge_activity_truth_table() {
        assert_eq!(merge_activity(None, None), None);
        assert_eq!(merge_activity(Some(false), None), Some(false));
        assert_eq!(merge_activity(None, Some(false)), Some(false));
        assert_eq!(merge_activity(Some(false), Some(true)), Some(true));
        assert_eq!(merge_activity(Some(true), None), Some(true));
    }

    #[test]
    fn display_name_falls_back_in_order() {
        let exe = ProcessInfo {
            executable: Some("chrome.exe".into()),
            bundle_id: Some("com.google.Chrome".into()),
            ..info(1, "  ")
        };
        let bundle = ProcessInfo {
            bundle_id: Some("com.apple.Music".into()),
            ..info(2, "")
        };
        let bare = info(3, "");
        let blank_fields = ProcessInfo {
            executable: Some("   ".into()),
            bundle_id: Some(String::new()),
            ..info(4, "")
        };
        let out = normalize_process_list(vec![exe, bundle, bare, blank_fields], None);
        let names: Vec<(u32, &str)> = out.iter().map(|p| (p.pid, p.name.as_str())).collect();
        assert!(names.contains(&(1, "chrome.exe")));
        assert!(names.contains(&(2, "com.apple.Music")));
        assert!(names.contains(&(3, "pid 3")));
        assert!(
            names.contains(&(4, "pid 4")),
            "blank executable/bundle are treated as absent"
        );
        let blank = out.iter().find(|p| p.pid == 4).unwrap();
        assert_eq!(blank.executable, None);
        assert_eq!(blank.bundle_id, None);
        assert!(out.iter().all(|p| !p.name.trim().is_empty()));
    }

    #[test]
    fn sorts_active_first_then_name_then_pid() {
        let out = normalize_process_list(
            vec![
                info(30, "beta"),
                ProcessInfo {
                    is_output_active: Some(true),
                    ..info(20, "zeta")
                },
                info(11, "Alpha"),
                info(10, "alpha"),
                ProcessInfo {
                    is_output_active: Some(false),
                    ..info(40, "gamma")
                },
            ],
            None,
        );
        let order: Vec<u32> = out.iter().map(|p| p.pid).collect();
        assert_eq!(order, vec![20, 10, 11, 30, 40]);
    }

    #[test]
    fn normalization_is_idempotent() {
        let raw = vec![
            info(3, ""),
            ProcessInfo {
                is_output_active: Some(true),
                ..info(9, "music")
            },
            info(3, "late name"),
        ];
        let once = normalize_process_list(raw, None);
        let twice = normalize_process_list(once.clone(), None);
        assert_eq!(once, twice);
    }
}
