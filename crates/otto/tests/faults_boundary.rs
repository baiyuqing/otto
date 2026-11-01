//! Source guard for the fault-injection module boundary.
//!
//! `faults` exists only under `cfg(test)` or the `fault-injection` feature.
//! Production code must not reference it, and it must not depend on
//! frontends or the application layer. `fixtures` (`cfg(test)` or the
//! `test-support` feature, which implies `fault-injection`) is the other
//! test-only module and may reference `faults`.

use std::path::{Path, PathBuf};

/// Modules compiled only for tests or a test feature; see `src/lib.rs`.
const TEST_ONLY: &[&str] = &["faults", "fixtures"];

const UPWARD: &[&str] = &[
    "cli", "tui", "acp", "server", "client", "app", "runtime", "provider", "tool",
];

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

/// Remove `#[cfg(test)]` items.
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
            depth += line.matches('{').count() as i32 - line.matches('}').count() as i32;
            opened |= line.contains('{');
            if (opened && depth <= 0) || (!opened && line.trim_end().ends_with(';')) {
                break;
            }
        }
    }
    kept.join("\n")
}

fn code(source: &str) -> String {
    non_test(source)
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `src/lib.rs` declares the module and the test-only modules may use it;
/// no other production file names it.
fn production_violations(relative: &Path, source: &str) -> Vec<String> {
    if relative == Path::new("lib.rs")
        || TEST_ONLY.iter().any(|module| relative.starts_with(module))
    {
        return Vec::new();
    }
    let source = code(source);
    if source.contains("crate::faults") || source.contains("otto::faults") {
        vec!["production code references `faults`".into()]
    } else {
        Vec::new()
    }
}

fn faults_violations(source: &str) -> Vec<String> {
    let source = code(source);
    UPWARD
        .iter()
        .filter(|module| source.contains(&format!("crate::{module}::")))
        .map(|module| format!("depends on `{module}`"))
        .collect()
}

#[test]
fn production_code_does_not_reference_faults() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders = Vec::new();
    for path in sources(&src) {
        let relative = path.strip_prefix(&src).unwrap();
        let body = std::fs::read_to_string(&path).expect("read source");
        for problem in production_violations(relative, &body) {
            offenders.push(format!("{}: {problem}", path.display()));
        }
    }
    assert!(
        offenders.is_empty(),
        "fault injection is test-only; move the use under #[cfg(test)]:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn faults_do_not_depend_on_upper_layers() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/faults");
    let mut offenders = Vec::new();
    for path in sources(&root) {
        let body = std::fs::read_to_string(&path).expect("read faults source");
        for problem in faults_violations(&body) {
            offenders.push(format!("{}: {problem}", path.display()));
        }
    }
    assert!(
        offenders.is_empty(),
        "faults is a leaf module:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn fixtures_prove_forbidden_edges_are_detected() {
    let file = Path::new("auth/token.rs");
    assert!(!production_violations(file, "use crate::faults::http;").is_empty());
    assert!(
        production_violations(file, "#[cfg(test)]\nmod tests { use crate::faults::http; }")
            .is_empty()
    );
    assert!(production_violations(file, "// crate::faults::http").is_empty());
    assert!(production_violations(Path::new("lib.rs"), "pub mod faults;").is_empty());
    assert!(
        production_violations(
            Path::new("fixtures/scripted_provider.rs"),
            "use crate::faults::http;"
        )
        .is_empty()
    );
    assert!(
        production_violations(Path::new("faults/http.rs"), "use crate::faults::io;").is_empty()
    );
    assert!(
        production_violations(file, "use otto_core::config::resolve::SessionDefaults::x;")
            .is_empty()
    );
    assert!(!faults_violations("use crate::provider::Client;").is_empty());
    assert!(faults_violations("use std::net::TcpListener;").is_empty());
}
