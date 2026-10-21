//! Exercise process exit status and preflight before capture starts.
use std::process::Command;

#[test]
fn strict_rejection_exits_nonzero_and_emits_candidate_json() {
    let output = Command::new(env!("CARGO_BIN_EXE_flexaudio-cli"))
        .args([
            "--source",
            "mic",
            "--mode",
            "exclude",
            "--settings-json",
            "--out",
            "-",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    let reports: Vec<serde_json::Value> = stderr
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0]["revision"], 0);
    assert!(reports[0]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| entry["setting"] == "mode" && entry["outcome"]["status"] == "ignored"));
}

#[test]
fn no_strict_allows_ignored_settings_to_reach_device_selection() {
    let output = Command::new(env!("CARGO_BIN_EXE_flexaudio-cli"))
        .args([
            "--source",
            "mic",
            "--device-id",
            "nonexistent-input-for-settings-test",
            "--mode",
            "exclude",
            "--no-strict",
            "--out",
            "-",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("device/endpoint was not found"), "{stderr}");
    assert!(!stderr.contains("IgnoredSettings"), "{stderr}");
}

#[test]
fn contradictory_strict_flags_are_a_usage_error() {
    let output = Command::new(env!("CARGO_BIN_EXE_flexaudio-cli"))
        .args(["--strict", "--no-strict"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("--strict") && stderr.contains("--no-strict"));
    assert!(stderr.contains("cannot be used with"), "{stderr}");
}
