//! Exercise the actual process exit status without opening audio hardware.
use std::process::Command;

#[test]
fn strict_rejection_exits_nonzero_and_emits_candidate_json() {
    let output = Command::new(env!("CARGO_BIN_EXE_flexaudio-cli"))
        .args([
            "--source",
            "mic",
            "--mode",
            "exclude",
            "--strict",
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
