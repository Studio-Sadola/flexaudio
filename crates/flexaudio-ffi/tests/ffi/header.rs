//! Check that the checked-in header is exactly the repository cbindgen projection.
#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "requires the cc C compiler; the Linux CI job enforces this check"
)]
fn c_header_layouts_and_v4_signatures_compile() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let output = std::process::Command::new("cc")
        .args([
            "-std=c11",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-fsyntax-only",
            "-I",
        ])
        .arg(root.join("include"))
        .arg(root.join("tests/ffi/abi.c"))
        .output()
        .expect("C compiler required for ABI verification");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "requires cbindgen; the Linux CI job enforces this check"
)]
fn generated_header_matches_cbindgen() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let output = std::env::temp_dir().join(format!("flexaudio-header-{}.h", std::process::id()));
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            #[allow(
                clippy::let_underscore_must_use,
                reason = "the temporary file is absent when the step that creates it failed"
            )]
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
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "requires the cc C compiler; the Linux CI job enforces this check"
)]
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

fn ascii_c_identifier(symbol: &str) -> bool {
    let mut bytes = symbol.bytes();
    match bytes.next() {
        Some(first) if first.is_ascii_alphabetic() || first == b'_' => {}
        _ => return false,
    }
    bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// Compile the checked-in header and print one validated symbol per line.
/// Symbols must already be ASCII identifiers. The program is built from Rust
/// strings and passed to cc as arguments, never through a shell.
fn c_header_symbol_values(symbols: &[String]) -> Vec<i32> {
    for symbol in symbols {
        assert!(
            ascii_c_identifier(symbol),
            "canonical code symbol is not an ASCII identifier: {symbol}"
        );
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let stamp = std::process::id();
    let source_path = std::env::temp_dir().join(format!("flexaudio-error-codes-{stamp}.c"));
    let binary_path = std::env::temp_dir().join(format!("flexaudio-error-codes-{stamp}"));
    let mut source =
        String::from("#include <stdio.h>\n#include \"flexaudio.h\"\nint main(void) {\n");
    for symbol in symbols {
        source.push_str("    (void)printf(\"%d\\n\", ");
        source.push_str(symbol);
        source.push_str(");\n");
    }
    source.push_str("    return 0;\n}\n");
    std::fs::write(&source_path, &source).expect("write error code program");
    struct Cleanup(std::path::PathBuf, std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            for path in [&self.0, &self.1] {
                #[allow(
                    clippy::let_underscore_must_use,
                    reason = "the temporary file is absent when the step that creates it failed"
                )]
                let _ = std::fs::remove_file(path);
            }
        }
    }
    let _cleanup = Cleanup(source_path.clone(), binary_path.clone());
    let compile = std::process::Command::new("cc")
        .args(["-std=c11", "-Wall", "-Wextra", "-Werror"])
        .arg("-I")
        .arg(root.join("include"))
        .arg(&source_path)
        .arg("-o")
        .arg(&binary_path)
        .output()
        .expect("C compiler required for error code verification");
    assert!(
        compile.status.success(),
        "error code program failed to compile: {}",
        String::from_utf8_lossy(&compile.stderr)
    );
    let run = std::process::Command::new(&binary_path)
        .output()
        .expect("run error code program");
    assert!(
        run.status.success(),
        "error code program failed: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    let text = String::from_utf8(run.stdout).expect("error code program output is utf-8");
    text.lines()
        .map(|line| line.parse::<i32>().expect("error code integer"))
        .collect()
}

/// Resolve a canonical symbol to the Rust ABI constant, independently of root_code.
/// This lists symbols, not a second ErrorKind-to-code mapping.
fn rust_error_code(symbol: &str) -> i32 {
    macro_rules! resolve {
        ($($constant:ident),+ $(,)?) => {
            match symbol {
                $(stringify!($constant) => crate::error::code::$constant,)+
                _ => panic!("canonical C error symbol has no Rust constant: {symbol}"),
            }
        };
    }
    resolve!(
        FLEX_INVALID_ARG,
        FLEX_INVALID_STATE,
        FLEX_DEVICE_NOT_FOUND,
        FLEX_DEVICE_LOST,
        FLEX_PERMISSION_DENIED,
        FLEX_UNSUPPORTED_OS_VERSION,
        FLEX_UNSUPPORTED,
        FLEX_UNSUPPORTED_FORMAT,
        FLEX_NATIVE_FORMAT_CHANGED,
        FLEX_AMBIGUOUS_DEVICE_NAME,
        FLEX_IGNORED_SETTINGS,
        FLEX_FAILURE,
    )
}

#[test]
fn every_core_error_kind_maps_to_canonical_distinct_c_code() {
    let cases = flexaudio_core::test_support::error_cases();
    let mut by_code = std::collections::BTreeMap::<i32, Vec<flexaudio::ErrorKind>>::new();
    for (kind, name, error) in &cases {
        assert_eq!(
            error.kind(),
            *kind,
            "core error fixture must match its kind"
        );
        let code = crate::error::root_code(error);
        by_code.entry(code).or_default().push(*kind);
        let symbol = flexaudio_core::test_support::canonical_error_code_symbol(name);
        assert!(
            ascii_c_identifier(&symbol),
            "canonical code symbol is not an ASCII identifier: {symbol}"
        );
        assert_eq!(
            code,
            rust_error_code(&symbol),
            "C error code must equal the canonical Rust constant for {kind:?}"
        );
    }
    for (code, kinds) in &by_code {
        assert!(
            kinds.len() == 1,
            "C error code collision or fallback for {kinds:?} sharing {code}"
        );
    }
}

#[test]
fn every_core_error_variant_maps_to_its_canonical_c_code() {
    for (kind, name, error) in flexaudio_core::test_support::error_variant_cases() {
        assert_eq!(error.kind(), kind, "core error fixture must match its kind");
        let symbol = flexaudio_core::test_support::canonical_error_code_symbol(name);
        assert_eq!(
            crate::error::root_code(&error),
            rust_error_code(&symbol),
            "C error code must be canonical for {kind:?}: {error:?}"
        );
    }
}

#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "requires the cc C compiler; the Linux CI job enforces this check"
)]
fn canonical_c_header_error_values_match_rust_constants() {
    let cases = flexaudio_core::test_support::error_cases();
    let symbols: Vec<_> = cases
        .iter()
        .map(|(_, name, _)| flexaudio_core::test_support::canonical_error_code_symbol(name))
        .collect();
    let values = c_header_symbol_values(&symbols);
    assert_eq!(
        values.len(),
        cases.len(),
        "header program must print one value per kind"
    );
    for (((kind, _, _), symbol), value) in cases.iter().zip(&symbols).zip(values) {
        assert_eq!(
            rust_error_code(symbol),
            value,
            "canonical Rust error constant must equal the header value for {kind:?}"
        );
    }
}
