//! Source guard for the fault-injection module boundary.
//!
//! `otto::faults` exists only under `cfg(test)` or the `fault-injection`
//! feature, and the `otto-faults` crate is linked only through that feature
//! and dev-dependencies. Production code of every crate under `crates/` must
//! reference neither, and `otto::faults` must not depend on frontends or the
//! application layer. `fixtures` (`cfg(test)` or the `test-support` feature,
//! which implies `fault-injection`) is the other test-only module of `otto`
//! and may reference them. The same upward rule for the code in `otto-faults`
//! is enforced by Cargo: that crate cannot depend on `otto`.

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

/// `src/lib.rs` of `otto` declares the module and the test-only modules may
/// use it; no other production file names it. `relative` is the path under
/// the `src` of `krate`; only `otto` has test-only modules.
fn production_violations(krate: &str, relative: &Path, source: &str) -> Vec<String> {
    if krate == "otto"
        && (relative == Path::new("lib.rs")
            || TEST_ONLY.iter().any(|module| relative.starts_with(module)))
    {
        return Vec::new();
    }
    let source = code(source);
    if source.contains("crate::faults")
        || source.contains("otto::faults")
        || source.contains("otto_faults")
    {
        vec!["production code references `faults` or `otto_faults`".into()]
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
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut offenders = Vec::new();
    let mut scanned = 0;
    for entry in std::fs::read_dir(&crates).expect("read crates directory") {
        let root = entry.expect("read crates entry").path();
        let name = root.file_name().unwrap().to_string_lossy().into_owned();
        let src = root.join("src");
        // `otto-faults` is the test-support crate itself.
        if name == "otto-faults" || !src.is_dir() {
            continue;
        }
        scanned += 1;
        for path in sources(&src) {
            let relative = path.strip_prefix(&src).unwrap();
            let body = std::fs::read_to_string(&path).expect("read source");
            for problem in production_violations(&name, relative, &body) {
                offenders.push(format!("{}: {problem}", path.display()));
            }
        }
    }
    assert!(
        scanned >= 10,
        "scanned only {scanned} crates under {crates:?}"
    );
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
    let check = |source: &str| production_violations("otto", file, source);
    assert!(!check("use crate::faults::mcp;").is_empty());
    assert!(!check("use otto_faults::http;").is_empty());
    assert!(!check("let _ = otto_faults::FaultPlan::new();").is_empty());
    assert!(check("#[cfg(test)]\nmod tests { use otto_faults::http; }").is_empty());
    assert!(check("// otto_faults::http").is_empty());
    assert!(check("pub mod faults;").is_empty());
    assert!(production_violations("otto", Path::new("lib.rs"), "pub mod faults;").is_empty());
    assert!(
        production_violations(
            "otto",
            Path::new("fixtures/scripted_provider.rs"),
            "use otto_faults::http;"
        )
        .is_empty()
    );
    assert!(
        production_violations(
            "otto",
            Path::new("faults/mcp.rs"),
            "use otto_faults::Fault;"
        )
        .is_empty()
    );
    // Only `otto` has test-only modules; another crate's `src/lib.rs` has none.
    assert!(
        !production_violations("otto-server", Path::new("lib.rs"), "use otto::faults::mcp;")
            .is_empty()
    );
    assert!(
        !production_violations(
            "otto-cli",
            Path::new("fixtures/a.rs"),
            "use otto_faults::http;"
        )
        .is_empty()
    );
    assert!(check("use otto_core::config::resolve::SessionDefaults::x;").is_empty());
    assert!(!faults_violations("use crate::provider::Client;").is_empty());
    assert!(faults_violations("use std::net::TcpListener;").is_empty());
}
