//! Internal binding contract fixtures; no stability guarantee.

pub use crate::error_context::shutdown_fields;
pub use crate::errors::error_cases;

/// Require named arms even when a fallback happens to equal the real Backend value.
pub fn assert_explicit_error_kind_arms(source: &str, function: &str) {
    let body = source
        .split_once(function)
        .expect("error mapping function")
        .1
        .split_once("\n}")
        .expect("error mapping function end")
        .0;
    for (kind, _, _) in error_cases() {
        assert!(
            body.contains(&format!("ErrorKind::{kind:?} =>")),
            "{function} must explicitly map {kind:?}, without using the wildcard fallback"
        );
    }
}

/// Extract declaration field names, ignoring whole-line and block documentation.
/// This intentionally supports the simple TypeScript interface and Python TypedDict
/// declarations used by the bindings, rather than parsing either language generally.
pub fn declaration_fields(source: &str, start: &str, end: &str) -> Vec<String> {
    let body = source.split_once(start).expect("contract declaration").1;
    let body = body.split_once(end).expect("contract declaration end").0;
    let mut comment = false;
    let mut fields = Vec::new();
    for line in body.lines() {
        let line = line.trim();
        if line.starts_with("/*") {
            comment = true;
        }
        if comment {
            if line.contains("*/") {
                comment = false;
            }
            continue;
        }
        if line.starts_with("//") || line.starts_with('#') {
            continue;
        }
        if let Some((name, _)) = line.split_once(':') {
            fields.push(name.trim().to_owned());
        }
    }
    fields.sort();
    fields
}
