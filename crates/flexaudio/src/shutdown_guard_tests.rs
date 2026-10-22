//! A deliberately small lexical guard, not a Rust data-flow analysis.
//!
//! For each production function, a `let _ =` statement calling stop,
//! stop_checked, shutdown, finish_shutdown, stop_binding, stop_owner,
//! stop_client, stop_producers, finish_worker, close, release_resources,
//! cleanup, cleanup_* or *_cleanup requires an explicit retention operation in
//! that same function. Storing operations count even inside `let _ =`: calls to
//! record_cleanup, record_native and retain_shutdown_outcome, and construction
//! with ShutdownReport::new(...). Struct literals, destructuring patterns and
//! type mentions do not count. Discarding a summary return value does not undo
//! storing operations. Value-returning operations, with_cleanup and
//! shutdown_report, count only outside `let _ =`: discarding
//! their combined error or report snapshot loses that result. Explicitly named
//! retaining teardown helpers are the single implementation used by void and
//! checked entry points.
//! Locks, channel sends, logging, and encoder finish/finalize are not this call set.
//! Comments and literals cannot satisfy the rule. There is no location allowlist.

use std::path::Path;

fn tokens(source: &str) -> Vec<String> {
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if bytes[i..].starts_with(b"//") {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if bytes[i..].starts_with(b"/*") {
            let mut depth = 1;
            i += 2;
            while i < bytes.len() && depth > 0 {
                if bytes[i..].starts_with(b"/*") {
                    depth += 1;
                    i += 2;
                } else if bytes[i..].starts_with(b"*/") {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            continue;
        }
        // Raw strings may contain quotes, braces, comments and test-like code.
        let raw = if bytes[i] == b'r' {
            Some(i + 1)
        } else if bytes[i..].starts_with(b"br") || bytes[i..].starts_with(b"cr") {
            Some(i + 2)
        } else {
            None
        };
        if let Some(start) = raw {
            let mut quote = start;
            while quote < bytes.len() && bytes[quote] == b'#' {
                quote += 1;
            }
            if bytes.get(quote) == Some(&b'"') {
                let end = format!("\"{}", "#".repeat(quote - start));
                i = source[quote + 1..]
                    .find(&end)
                    .map_or(bytes.len(), |offset| quote + 1 + offset + end.len());
                out.push("<literal>".into());
                continue;
            }
        }
        if bytes[i] == b'"' {
            i += 1;
            while i < bytes.len() {
                match bytes[i] {
                    b'\\' => i = (i + 2).min(bytes.len()),
                    b'"' => {
                        i += 1;
                        break;
                    }
                    _ => i += 1,
                }
            }
            out.push("<literal>".into());
            continue;
        }
        // An apostrophe followed by an identifier is normally a lifetime.
        // Skip a character literal only when its closing apostrophe is present.
        if bytes[i] == b'\'' {
            let mut end = i + 1;
            if bytes.get(end) == Some(&b'\\') {
                end += 2;
            } else if end < bytes.len() {
                end += source[end..].chars().next().unwrap().len_utf8();
            }
            if bytes.get(end) == Some(&b'\'') {
                i = end + 1;
                out.push("<literal>".into());
                continue;
            }
        }
        let start = i;
        if bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' {
            i += 1;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
        } else {
            i += source[i..].chars().next().unwrap().len_utf8();
        }
        out.push(source[start..i].to_owned());
    }
    out
}

fn end_group(tokens: &[String], start: usize, open: &str, close: &str) -> usize {
    let mut depth = 0;
    for (i, token) in tokens.iter().enumerate().skip(start) {
        if token == open {
            depth += 1;
        }
        if token == close {
            depth -= 1;
            if depth == 0 {
                return i + 1;
            }
        }
    }
    panic!("unclosed Rust source group");
}

fn production_tokens(source: &str) -> Vec<String> {
    let mut source = tokens(source);
    let mut i = 0;
    while i + 2 < source.len() {
        if source[i] != "#" || source[i + 1] != "[" {
            i += 1;
            continue;
        }
        let end = end_group(&source, i + 1, "[", "]");
        let attribute = source[i + 2..end - 1].join("");
        if attribute != "cfg(test)" && attribute != "test" {
            i = end;
            continue;
        }
        let mut item = end;
        while item + 1 < source.len() && source[item] == "#" && source[item + 1] == "[" {
            item = end_group(&source, item + 1, "[", "]");
        }
        let finish = item_body(&source, item).map_or_else(
            || {
                source[item..]
                    .iter()
                    .position(|token| token == ";")
                    .map_or(source.len(), |offset| item + offset + 1)
            },
            |(_, end)| end,
        );
        source.drain(i..finish);
    }
    source
}

fn cleanup_call(name: &str) -> bool {
    matches!(
        name,
        "stop"
            | "stop_checked"
            | "shutdown"
            | "finish_shutdown"
            | "stop_binding"
            | "stop_owner"
            | "stop_client"
            | "stop_producers"
            | "finish_worker"
            | "close"
            | "release_resources"
            | "cleanup"
    ) || name.starts_with("cleanup_")
        || name.ends_with("_cleanup")
}

fn retains_failure(body: &[String]) -> bool {
    let discarded = discarded_statements(body);
    body.windows(2).enumerate().any(|(index, pair)| {
        (matches!(
            pair[0].as_str(),
            "record_cleanup" | "record_native" | "retain_shutdown_outcome"
        ) && pair[1] == "(")
            || (matches!(pair[0].as_str(), "with_cleanup" | "shutdown_report")
                && pair[1] == "("
                && !discarded.iter().any(|statement| statement.contains(&index)))
            || body
                .get(index..index + 5)
                .is_some_and(|tokens| tokens == ["ShutdownReport", ":", ":", "new", "("])
    })
}

fn discarded_statements(body: &[String]) -> Vec<std::ops::Range<usize>> {
    let mut statements = Vec::new();
    for i in 0..body.len().saturating_sub(3) {
        if body[i..i + 3] != ["let", "_", "="] {
            continue;
        }
        let mut depth = 0isize;
        let mut end = body.len();
        for (index, token) in body.iter().enumerate().skip(i + 3) {
            if token == ";" && depth == 0 {
                end = index + 1;
                break;
            }
            if matches!(token.as_str(), "(" | "[" | "{") {
                depth += 1;
            }
            if matches!(token.as_str(), ")" | "]" | "}") {
                depth -= 1;
            }
        }
        statements.push(i..end);
    }
    statements
}

fn discarded_calls(body: &[String]) -> Vec<&str> {
    let mut calls = Vec::new();
    for statement in discarded_statements(body) {
        for pair in body[statement].windows(2) {
            if cleanup_call(&pair[0]) && pair[1] == "(" {
                calls.push(pair[0].as_str());
            }
        }
    }
    calls
}

fn item_body(source: &[String], mut start: usize) -> Option<(usize, usize)> {
    let mut depth = 0;
    while start < source.len() {
        match source[start].as_str() {
            "(" | "[" => depth += 1,
            ")" | "]" => depth -= 1,
            ";" if depth == 0 => return None,
            "{" if depth == 0 => return Some((start, end_group(source, start, "{", "}"))),
            _ => {}
        }
        start += 1;
    }
    None
}

fn function_violations(source: &[String]) -> Vec<String> {
    let mut errors = Vec::new();
    let mut i = 0;
    while i + 2 < source.len() {
        if source[i] != "fn" || source[i + 1] == "(" {
            i += 1;
            continue;
        }
        let name = &source[i + 1];
        let Some((start, end)) = item_body(source, i + 2) else {
            i += 2;
            continue;
        };
        let mut body = source[start + 1..end - 1].to_vec();
        // A nested function's reporter cannot discharge the enclosing function's
        // obligation. Closures remain part of the enclosing capture operation.
        errors.extend(function_violations(&body));
        let mut nested = 0;
        while nested + 1 < body.len() {
            if body[nested] == "fn" && body[nested + 1] != "(" {
                if let Some((_, finish)) = item_body(&body, nested + 2) {
                    body.drain(nested..finish);
                    continue;
                }
            }
            nested += 1;
        }
        if !retains_failure(&body) {
            for call in discarded_calls(&body) {
                errors.push(format!(
                    "{name} discards {call} without retaining the shutdown failure"
                ));
            }
        }
        i = end;
    }
    errors
}

fn violations(source: &str) -> Vec<String> {
    function_violations(&production_tokens(source))
}

fn check_directory(path: &Path, errors: &mut Vec<String>) {
    for entry in std::fs::read_dir(path).expect("workspace source directory") {
        let path = entry.expect("readable source entry").path();
        let name = path.file_name().unwrap().to_string_lossy();
        if path.is_dir() {
            if !matches!(name.as_ref(), "tests" | "examples" | "target") {
                check_directory(&path, errors);
            }
        } else if path.extension().is_some_and(|extension| extension == "rs")
            && name != "tests.rs"
            && !name.ends_with("_tests.rs")
            && !name.ends_with("_test.rs")
            && !name.starts_with("test_")
        {
            let source = std::fs::read_to_string(&path).expect("readable Rust source");
            errors.extend(
                violations(&source)
                    .into_iter()
                    .map(|error| format!("{}: {error}", path.display())),
            );
        }
    }
}

#[test]
fn production_stop_and_cleanup_discards_retain_failures() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut errors = Vec::new();
    for directory in ["crates", "bindings"] {
        check_directory(&root.join(directory), &mut errors);
    }
    assert!(errors.is_empty(), "{}", errors.join("\n"));
}

#[test]
fn guard_rejects_discards_without_retention_and_ignores_test_code_and_literals() {
    for call in [
        "stop_checked",
        "stop",
        "shutdown",
        "finish_shutdown",
        "cleanup_native",
        "close",
    ] {
        let source = format!("fn broken() {{ let _ = backend.\n{call}(); }}");
        assert_eq!(
            violations(&source),
            [format!(
                "broken discards {call} without retaining the shutdown failure"
            )]
        );
        let retained =
            format!("fn retained() {{ let _ = backend.{call}(); record_cleanup(error); }}");
        assert!(violations(&retained).is_empty());
    }
    // A reporter in another function must never satisfy this function's obligation.
    assert_eq!(
        violations("fn broken() { let _ = backend.stop(); } fn other() { record_cleanup(e); }")
            .len(),
        1
    );
    assert_eq!(
        violations(
            "fn broken(a: [u8; 4]) { fn nested() { record_cleanup(e); } let _ = backend.stop(); }"
        )
        .len(),
        1
    );
    assert_eq!(violations("fn broken() { let _ = backend.stop(); /* ShutdownReport */ let text = \"record_cleanup(error)\"; }").len(), 1);
    assert!(violations(
        r###"
        #[cfg(test)] mod checks { fn ignored() { let _ = backend.stop(); } }
        #[test] fn ignored() { let _ = backend.stop(); }
        fn harmless() {
            let _ = mutex.lock(); let _ = sender.send(value);
            let sample = r#"fn fake() { let _ = backend.stop(); }"#;
        }
    "###
    )
    .is_empty());
}

#[test]
fn guard_distinguishes_storing_operations_from_returned_retention_values() {
    for source in [
        "fn f(x: &mut dyn CaptureBackend) -> Option<ShutdownReport> { let _ = x.stop_checked(); None }",
        "fn f(x: &mut dyn CaptureBackend) { let _ = x.stop_checked(); let report: Option<ShutdownReport> = None; }",
        "fn f(x: &mut dyn CaptureBackend) { let _ = x.stop(); let _ = x.shutdown_report(); }",
        "fn f(x: &mut dyn CaptureBackend) { let _ = x.stop(); let _ = with_cleanup(error, cleanup); }",
        "fn f(x: &mut dyn CaptureBackend, existing: ShutdownReport) { let _ = x.stop_checked(); let ShutdownReport { primary, cleanup } = existing; let _ = (primary, cleanup); }",
        "fn f(x: &mut dyn CaptureBackend) { let _ = x.stop_checked(); let r = ShutdownReport { primary: None, cleanup }; }",
    ] {
        // A discarded with_cleanup is itself a discarded cleanup call, so it may
        // add a second violation; any violation rejects the function.
        assert!(
            !violations(source).is_empty(),
            "must reject unretained cleanup: {source}"
        );
    }
    for operation in [
        "record_cleanup(error)",
        "record_native(call, status)",
        "retain_shutdown_outcome(report)",
        "ShutdownReport::new(None, cleanup)",
    ] {
        assert!(
            violations(&format!(
                "fn f() {{ let _ = x.stop(); let _ = {operation}; }}"
            ))
            .is_empty(),
            "must accept a storing operation with a discarded return value: {operation}"
        );
        assert!(
            violations(&format!(
                "fn f() {{ let _ = x.stop(); let report = {operation}; }}"
            ))
            .is_empty(),
            "must accept an explicit retention operation: {operation}"
        );
    }
    assert!(
        violations("fn drop() { let _ = self.record_cleanup(); }").is_empty(),
        "discarding a recorded cleanup's summary bool must retain its ledger entries"
    );
    for operation in ["with_cleanup(error, cleanup)", "x.shutdown_report()"] {
        assert!(
            !violations(&format!(
                "fn f() {{ let _ = x.stop(); let _ = {operation}; }}"
            ))
            .is_empty(),
            "must reject a discarded retention value: {operation}"
        );
        assert!(
            violations(&format!(
                "fn f() {{ let _ = x.stop(); let report = {operation}; }}"
            ))
            .is_empty(),
            "must accept an undiscarded retention value: {operation}"
        );
    }
}
