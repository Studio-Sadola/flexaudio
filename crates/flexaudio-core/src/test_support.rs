//! Internal binding contract fixtures; no stability guarantee.

pub use crate::error_context::shutdown_fields;
pub use crate::errors::error_cases;

/// C and Node code symbol for a canonical camelCase kind name.
///
/// `backend` is the one existing ABI exception and is spelled `FLEX_FAILURE`.
/// The symbol is derived from the name; this function does not list kinds.
pub fn canonical_error_code_symbol(name: &str) -> String {
    if name == "backend" {
        return "FLEX_FAILURE".to_owned();
    }
    let mut symbol = String::from("FLEX_");
    for ch in name.chars() {
        if ch.is_ascii_uppercase() {
            symbol.push('_');
            symbol.push(ch);
        } else {
            symbol.push(ch.to_ascii_uppercase());
        }
    }
    symbol
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
