//! Keep applicability reasons owned by the facade, including binding test fixtures.
//!
//! Mechanically checked rule: binding Rust source must never name either reason
//! enum. Convert inferred reasons with `as_str()` instead of importing, aliasing,
//! matching, or constructing variants. The check deliberately includes comments
//! and strings so an alias cannot hide a second implementation.

use std::path::Path;

fn reason_identifier(source: &str) -> Option<&str> {
    source
        .split(|ch: char| !ch.is_alphanumeric() && ch != '_')
        .find(|word| matches!(*word, "IgnoreReason" | "AdjustmentReason"))
}

fn check_directory(directory: &Path) {
    for entry in std::fs::read_dir(directory).expect("binding source directory") {
        let path = entry.expect("readable source entry").path();
        if path.is_dir() {
            check_directory(&path);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            let source = std::fs::read_to_string(&path).expect("readable Rust source");
            assert_eq!(
                reason_identifier(&source),
                None,
                "{} must use the shared resolver and reason.as_str()",
                path.display()
            );
        }
    }
}

#[test]
fn bindings_cannot_construct_applicability_reasons() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for binding in [
        "crates/flexaudio-napi",
        "bindings/flexaudio-py",
        "crates/flexaudio-ffi",
        "crates/flexaudio-cli",
    ] {
        // Scan all source, examples, integration tests, and build scripts, but no
        // generated target artifacts. These crates have no nested target dirs.
        check_directory(&root.join(binding));
    }
}

#[test]
fn guard_rejects_qualified_imported_aliased_and_raw_reason_identifiers() {
    for source in [
        "flexaudio::IgnoreReason::FixedAtOpen",
        "use flexaudio::{IgnoreReason as I}; I::WrongSourceKind",
        "use flexaudio::AdjustmentReason::*; LegacySentinel",
        "flexaudio::r#IgnoreReason::FixedAtOpen",
    ] {
        assert!(reason_identifier(source).is_some(), "{source}");
    }
    assert!(reason_identifier("O::Ignored(reason) => reason.as_str()").is_none());
}
