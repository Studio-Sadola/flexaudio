//! Check that the checked-in header is exactly the repository cbindgen projection.
#[test]
fn generated_header_matches_cbindgen() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let output = std::env::temp_dir().join(format!("flexaudio-header-{}.h", std::process::id()));
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _cleanup = Cleanup(output.clone());
    let status = std::process::Command::new("cbindgen")
        .args([
            "--config",
            "cbindgen.toml",
            "--crate",
            "flexaudio-ffi",
            "--output",
        ])
        .arg(&output)
        .current_dir(root)
        .env("CARGO_NET_OFFLINE", "true")
        .output()
        .expect("cbindgen must be installed to verify the generated C ABI");
    assert!(
        status.status.success(),
        "cbindgen generation failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    assert_eq!(
        std::fs::read(root.join("include/flexaudio.h")).unwrap(),
        std::fs::read(output).unwrap(),
        "regenerate include/flexaudio.h using cbindgen.toml"
    );
}

// Frozen v0.4.0 header values (release commits 07de0ca and 7c17704).
// This test requires neither git nor a checkout containing the old release.
#[test]
fn legacy_v040_codes_keep_their_names_and_values() {
    let header = include_str!("../../include/flexaudio.h");
    let expected = [
        ("FLEX_SOURCE_KIND_MIC", 0),
        ("FLEX_SOURCE_KIND_SYSTEM", 1),
        ("FLEX_SOURCE_KIND_PROCESS", 2),
        ("FLEX_SOURCE_KIND_MIX", 3),
        ("FLEX_PROCESS_MODE_INCLUDE", 0),
        ("FLEX_PROCESS_MODE_EXCLUDE", 1),
        ("FLEX_EVENT_KIND_CHUNK_DROPPED", 0),
        ("FLEX_EVENT_KIND_STALLED", 1),
        ("FLEX_EVENT_KIND_RECOVERED", 2),
        ("FLEX_EVENT_KIND_PERMISSION_DENIED", 3),
        ("FLEX_EVENT_KIND_DEVICE_LOST", 4),
        ("FLEX_EVENT_KIND_ERROR", 5),
        ("FLEX_EVENT_KIND_UNKNOWN", 6),
        ("FLEX_EVENT_KIND_SILENCE_WHILE_SOURCE_ACTIVE", 7),
        ("FLEX_EVENT_KIND_PERMISSION_PENDING", 8),
        ("FLEX_OUTPUT_ACTIVITY_UNKNOWN", 0),
        ("FLEX_OUTPUT_ACTIVITY_INACTIVE", 1),
        ("FLEX_OUTPUT_ACTIVITY_ACTIVE", 2),
        ("FLEX_DEVICE_EVENT_KIND_ADDED", 0),
        ("FLEX_DEVICE_EVENT_KIND_REMOVED", 1),
        ("FLEX_DEVICE_EVENT_KIND_DEFAULT_CHANGED", 2),
        ("FLEX_DEVICE_EVENT_KIND_UNKNOWN", 3),
    ];
    let actual: std::collections::BTreeMap<_, _> = header
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            (fields.next()? == "#define").then_some(())?;
            let name = fields.next()?;
            let value = fields.next()?.parse::<i32>().ok()?;
            Some((name, value))
        })
        .collect();
    for (name, value) in expected {
        assert_eq!(actual.get(name), Some(&value), "v0.4.0 code {name}");
    }
}

#[test]
fn readme_basic_capture_compiles_against_header() {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let readme = include_str!("../../README.md");
    let section = readme.split("## Basic Capture").nth(1).unwrap();
    let example = section
        .split("```c\n")
        .nth(1)
        .unwrap()
        .split("```")
        .next()
        .unwrap();
    let mut compiler = Command::new("cc")
        .args([
            "-std=c11",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-fsyntax-only",
            "-x",
            "c",
            "-I",
        ])
        .arg(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("include"))
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("a C compiler is required to verify the README example");
    compiler
        .stdin
        .take()
        .unwrap()
        .write_all(example.as_bytes())
        .unwrap();
    let output = compiler.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "README capture example failed to compile: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
