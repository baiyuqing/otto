//! Source guard for the session crate: it holds no SQL and no storage SDK
//! import, its `legacy.rs` stays a set of source-compatible constructors, and
//! its non-test sources never reexport `otto_core`.
//!
//! Cargo already keeps `otto-session` from importing `otto`, `otto-storage` or
//! `rusqlite`: none of them is in its manifest. These checks cover what Cargo
//! cannot: SQL text in string literals, the shape of the constructor facade
//! and the `otto_core` single-path rule. They moved here from
//! `crates/otto/tests/storage_boundary.rs` with the session store.

use std::path::{Path, PathBuf};

fn sources(path: &Path) -> Vec<PathBuf> {
    if path.is_file() {
        return vec![path.to_path_buf()];
    }
    let mut found = Vec::new();
    for entry in std::fs::read_dir(path).expect("read source directory") {
        let path = entry.expect("read source entry").path();
        let name = path.file_name().unwrap().to_string_lossy();
        // Out-of-line test modules: `tests`, `tests.rs`, `*_tests`, `*_tests.rs`.
        let stem = name.trim_end_matches(".rs");
        if stem == "tests" || stem.ends_with("_tests") {
            continue;
        }
        if path.is_dir() {
            found.extend(sources(&path));
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            found.push(path);
        }
    }
    found.sort();
    found
}

/// Net `{` minus `}` on one line, ignoring braces inside string and char
/// literals (format strings hold `{{` and `}}`).
fn brace_balance(line: &str) -> i32 {
    let mut balance = 0;
    let mut in_string = false;
    let mut chars = line.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '\\' if in_string => {
                chars.next();
            }
            '"' => in_string = !in_string,
            '\'' if !in_string => {
                // A char literal such as '{' or '\''; a lifetime has no closing quote.
                let mut ahead = chars.clone();
                let first = ahead.next();
                let close = if first == Some('\\') {
                    ahead.next();
                    ahead.next()
                } else {
                    ahead.next()
                };
                if close == Some('\'') {
                    chars = ahead;
                }
            }
            '{' if !in_string => balance += 1,
            '}' if !in_string => balance -= 1,
            _ => {}
        }
    }
    balance
}

/// Remove `#[cfg(test)]` items so persistence tests may inspect SQL directly.
fn non_test(source: &str) -> String {
    let lines: Vec<&str> = source.lines().collect();
    let mut kept = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        if lines[index].trim() != "#[cfg(test)]" {
            kept.push(lines[index]);
            index += 1;
            continue;
        }
        index += 1;
        let mut depth = 0i32;
        let mut opened = false;
        while index < lines.len() {
            let line = lines[index];
            index += 1;
            depth += brace_balance(line);
            opened |= line.contains('{');
            if (opened && depth <= 0) || (!opened && line.trim_end().ends_with(';')) {
                break;
            }
        }
    }
    kept.join("\n")
}

fn without_line_comments(source: &str) -> String {
    source
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn has_sql(source: &str) -> bool {
    let source = without_line_comments(source);
    let bytes = source.as_bytes();
    let mut literals = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'r' {
            let mut quote = index + 1;
            while quote < bytes.len() && bytes[quote] == b'#' {
                quote += 1;
            }
            if quote < bytes.len() && bytes[quote] == b'"' {
                let hashes = quote - index - 1;
                let body_start = quote + 1;
                let mut end = body_start;
                while end < bytes.len() {
                    if bytes[end] == b'"' && bytes[end + 1..].starts_with(&vec![b'#'; hashes]) {
                        break;
                    }
                    end += 1;
                }
                if let Some(literal) = source.get(body_start..end) {
                    literals.push(literal.to_string());
                }
                index = (end + 1 + hashes).min(bytes.len());
                continue;
            }
        }
        if bytes[index] != b'"' {
            index += 1;
            continue;
        }
        let mut literal = String::new();
        index += 1;
        while index < bytes.len() && bytes[index] != b'"' {
            if bytes[index] == b'\\' && index + 1 < bytes.len() {
                index += 1;
                literal.push(match bytes[index] {
                    b'n' | b'r' => ' ',
                    byte => byte as char,
                });
            } else {
                literal.push(bytes[index] as char);
            }
            index += 1;
        }
        literals.push(literal);
        index = (index + 1).min(bytes.len());
    }
    literals.iter().any(|literal| {
        let statement = literal.trim_start().to_ascii_uppercase();
        statement.starts_with("CREATE TABLE ")
            || statement.starts_with("CREATE INDEX ")
            || statement.starts_with("ALTER TABLE ")
            || (statement.starts_with("INSERT INTO ") && statement.contains(" VALUES"))
            || (statement.starts_with("SELECT ") && statement.contains(" FROM "))
            || (statement.starts_with("UPDATE ") && statement.contains(" SET "))
            || statement.starts_with("DELETE FROM ")
            || statement.starts_with("PRAGMA USER_VERSION")
            || statement.starts_with("PRAGMA FOREIGN_KEYS")
            || statement.starts_with("PRAGMA JOURNAL_MODE")
    })
}

fn associated_calls(source: &str, owner: &str) -> Vec<String> {
    let mut rest = source;
    let mut calls = Vec::new();
    let prefix = format!("{owner}::");
    while let Some(index) = rest.find(&prefix) {
        rest = &rest[index + prefix.len()..];
        let end = rest
            .find(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
            .unwrap_or(rest.len());
        if rest[end..].trim_start().starts_with('(') {
            calls.push(rest[..end].to_owned());
        }
        rest = &rest[end..];
    }
    calls
}

fn method_calls(source: &str) -> Vec<String> {
    let bytes = source.as_bytes();
    let mut calls = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'.'
            || index + 1 >= bytes.len()
            || !bytes[index + 1].is_ascii_alphabetic()
        {
            index += 1;
            continue;
        }
        let start = index + 1;
        index = start + 1;
        while index < bytes.len() && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_')
        {
            index += 1;
        }
        let name = &source[start..index];
        if source[index..].trim_start().starts_with('(') {
            calls.push(name.to_owned());
        }
    }
    calls
}

fn session_legacy_violations(source: &str) -> Vec<String> {
    let source = without_line_comments(&non_test(source));
    let mut problems = Vec::new();
    let allowed_functions = [
        "create",
        "create_lazy",
        "create_lazy_with_driver",
        "create_lazy_with_driver_and_keeper",
        "create_child_lazy",
        "create_child_lazy_with_driver",
        "create_child_lazy_with_driver_and_keeper",
        "open",
        "prepare",
        "prepare_with_driver",
        "prepare_listed",
        "prepare_listed_with_driver",
        "archive",
    ];
    let mut rest = source.as_str();
    while let Some(index) = rest.find("fn ") {
        rest = &rest[index + 3..];
        let end = rest
            .find(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
            .unwrap_or(rest.len());
        let name = &rest[..end];
        if !allowed_functions.contains(&name) {
            problems.push(format!("non-constructor function `{name}`"));
        }
        rest = &rest[end..];
    }
    if source.contains("::sqlite::") {
        problems.push("session compatibility facade imports a SQLite repository".into());
    }
    if source.contains("otto_storage::drivers::") {
        problems.push("session compatibility facade imports a storage driver".into());
    }
    for forbidden in [
        "otto_storage::Connection",
        "otto_storage::Transaction",
        "otto_storage::params",
        "otto_storage::drivers::sqlite::",
        "crate::app::",
        "crate::runtime::",
        "crate::cli::",
        "crate::tui::",
        "crate::client::",
    ] {
        if source.contains(forbidden) {
            problems.push(format!("forbidden compatibility dependency `{forbidden}`"));
        }
    }
    if has_sql(&source) {
        problems.push("SQL/schema logic in compatibility constructors".into());
    }
    let allowed_self = [
        "create_lazy",
        "create_lazy_with_driver",
        "create_lazy_with_driver_and_keeper",
        "create_child_lazy_with_driver",
        "create_child_lazy_with_driver_and_keeper",
        "prepare_with_driver",
        "prepare_with_driver_and_keeper",
        "prepare_listed_with_driver",
        "prepare_listed_with_driver_and_keeper",
    ];
    for call in associated_calls(&source, "Self") {
        if !allowed_self.contains(&call.as_str()) {
            problems.push(format!("non-constructor forwarding call `Self::{call}`"));
        }
    }
    if !associated_calls(&source, "Store").is_empty() {
        problems.push("direct Store operation in compatibility facade".into());
    }
    if associated_calls(&source, "Prepared")
        .iter()
        .any(|call| call != "prepare")
    {
        problems.push("non-constructor Prepared operation in compatibility facade".into());
    }
    let allowed_methods = ["as_ref", "lock", "ensure_file", "activate"];
    for call in method_calls(&source) {
        if !allowed_methods.contains(&call.as_str()) {
            problems.push(format!("non-constructor method call `.{call}`"));
        }
    }
    problems
}

fn core_reexport_violations(source: &str) -> Vec<String> {
    without_line_comments(&non_test(source))
        .lines()
        .map(str::trim)
        .filter_map(|line| {
            let rest = line.strip_prefix("pub")?;
            let rest = match rest.strip_prefix('(') {
                Some(restricted) => restricted.split_once(')')?.1,
                None => rest,
            };
            let import = rest.trim_start().strip_prefix("use ")?.trim_start();
            (import.starts_with("otto_core") || import.starts_with("::otto_core"))
                .then(|| format!("reexports otto-core: `{line}`"))
        })
        .collect()
}

fn src() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn storage_violations(source: &str) -> Vec<String> {
    let source = non_test(source);
    let source = without_line_comments(&source);
    let mut problems = Vec::new();
    for forbidden in ["::sqlite::", "otto_storage", "rusqlite::", "libsql::"] {
        if source.contains(forbidden) {
            problems.push(format!("storage import `{forbidden}`"));
        }
    }
    if has_sql(&source) {
        problems.push("inline SQL or physical schema".into());
    }
    problems
}

#[test]
fn session_sources_hold_no_sql_or_storage_import() {
    let mut offenders = Vec::new();
    let files = sources(&src());
    assert!(files.len() >= 25, "source discovery found too few files");
    for path in files {
        let body = std::fs::read_to_string(&path).expect("read session source");
        for problem in storage_violations(&body) {
            offenders.push(format!("{}: {problem}", path.display()));
        }
    }
    offenders.sort();
    assert!(
        offenders.is_empty(),
        "session code owns no SQL:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn legacy_constructors_stay_a_pure_facade() {
    let path = src().join("legacy.rs");
    let body = std::fs::read_to_string(&path).expect("read legacy.rs");
    let problems = session_legacy_violations(&body);
    assert!(
        problems.is_empty(),
        "legacy.rs must only forward constructors:\n{}",
        problems.join("\n")
    );
}

#[test]
fn session_does_not_reexport_core() {
    let mut offenders = Vec::new();
    for path in sources(&src()) {
        let body = std::fs::read_to_string(&path).expect("read source");
        for problem in core_reexport_violations(&body) {
            offenders.push(format!("{}: {problem}", path.display()));
        }
    }
    offenders.sort();
    assert!(
        offenders.is_empty(),
        "otto-core items are imported from `otto_core::` directly so each has one path:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn fixtures_prove_forbidden_edges_are_detected() {
    let allowed_constructor = "use super::Store;\nuse \
        otto_platform::fsops::PosixDriver;\nimpl Store { fn \
        create_lazy() { Self::create_lazy_with_driver_and_keeper(); } }";
    assert!(session_legacy_violations(allowed_constructor).is_empty());
    let service_call =
        "use super::Store;\nimpl Store { fn create_lazy() { Store::recover(\"scope\"); } }";
    assert!(!session_legacy_violations(service_call).is_empty());
    let mutating_constructor = "use super::Store;\nimpl Store { fn \
        create_lazy() { Self::create_lazy_with_driver_and_keeper(); } }\nconst \
        SQL: &str = \"UPDATE sessions SET status='closed'\";";
    assert!(!session_legacy_violations(mutating_constructor).is_empty());
    let backend_import = "use crate::usage::sqlite::Store;\nimpl Store { fn create_lazy() {} }";
    assert!(!session_legacy_violations(backend_import).is_empty());
    assert!(!storage_violations("use crate::usage::sqlite::Store;").is_empty());
    assert!(!storage_violations("const SQL: &str = \"SELECT * FROM runs\";").is_empty());
    assert!(!storage_violations("use rusqlite::Connection;").is_empty());
    assert!(storage_violations("fn f() { invalid(\"update identity\"); }").is_empty());
    let test_only_sql = "#[cfg(test)]\nmod tests { const SQL: &str = \"SELECT * FROM runs\"; }";
    assert!(storage_violations(test_only_sql).is_empty());
    assert!(!core_reexport_violations("pub use otto_core::approval::*;").is_empty());
    assert!(
        !core_reexport_violations(
            "pub(crate) use otto_core::session::turn::MAX_AUTOMATIC_RESUMES;"
        )
        .is_empty()
    );
    assert!(core_reexport_violations("use otto_core::approval::ApprovalMode;").is_empty());
    assert!(core_reexport_violations("//! pub use otto_core::x;").is_empty());
}
