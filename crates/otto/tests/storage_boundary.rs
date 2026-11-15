//! Source guard for native storage ownership and dependency direction.
//!
//! Domain services own state semantics and import typed repository ports.
//! Concrete drivers and the SQL layer live in the `otto-storage` crate, which
//! does not depend on `otto`, so Cargo enforces that it imports no domain
//! module. Each domain's repository implementation is a file named
//! `sqlite.rs` or a file under a `sqlite/` directory inside the domain module
//! (see `is_domain_repository`); only those files may hold SQL and import the
//! SQL layer and the SQLite driver. Only domain `mod.rs` files may retain
//! transitional constructors. A named legacy facade may contain only a pure
//! reexport of the domain's own repository Store. Non-test sources
//! never reexport `otto_core`; each of its items has the single path
//! `otto_core::...`.

use std::path::{Path, PathBuf};

const DOMAIN_MODULES: &[&str] = &[
    "approval", "memory", "schedule", "workflow", "skill", "usage", "subagent",
];
const FRONTENDS: &[&str] = &["cli", "tui", "acp", "server", "client"];

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

/// A domain's repository implementation: `<domain>/sqlite.rs` or any file
/// below `<domain>/.../sqlite/`. It holds the domain's SQL and opens its
/// connection through the SQLite driver; no other file of a domain does.
fn is_domain_repository(path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(Path::new(env!("CARGO_MANIFEST_DIR")).join("src")) else {
        return false;
    };
    let parts: Vec<&str> = relative.iter().filter_map(|part| part.to_str()).collect();
    let [domain, rest @ ..] = parts.as_slice() else {
        return false;
    };
    let Some((file, directories)) = rest.split_last() else {
        return false;
    };
    DOMAIN_MODULES.contains(domain) && (*file == "sqlite.rs" || directories.contains(&"sqlite"))
}

/// Every domain repository file under `src/`, tests excluded.
fn repository_sources() -> Vec<PathBuf> {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    DOMAIN_MODULES
        .iter()
        .flat_map(|module| sources(&src.join(module)))
        .filter(|path| is_domain_repository(path))
        .collect()
}

fn is_domain_mod(path: &Path) -> bool {
    path.file_name().and_then(|name| name.to_str()) == Some("mod.rs")
}

fn is_named_legacy_facade(path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(Path::new(env!("CARGO_MANIFEST_DIR")).join("src")) else {
        return false;
    };
    [Path::new("subagent/record.rs")].contains(&relative)
}

fn is_pure_legacy_facade(path: &Path, source: &str) -> bool {
    if !is_named_legacy_facade(path) {
        return false;
    }
    let source = non_test(source);
    let relevant: Vec<&str> = source
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .filter(|line| line.contains("::sqlite::"))
        .map(str::trim)
        .collect();
    let code = without_line_comments(&source);
    !relevant.is_empty()
        && relevant
            .iter()
            .all(|line| line.starts_with("pub use ") || line.starts_with("pub(crate) use "))
        && !code.contains("Store::")
        && !code.contains("otto_storage::Connection")
        && !code.contains("otto_storage::params")
        && !has_sql(&code)
}

fn domain_violations(path: &Path, source: &str) -> Vec<String> {
    let source = non_test(source);
    if is_domain_repository(path) {
        return ["rusqlite::", "libsql::"]
            .iter()
            .any(|token| source.contains(token))
            .then(|| "storage SDK import".to_owned())
            .into_iter()
            .collect();
    }
    let legacy_facade = is_pure_legacy_facade(path, &source);
    let source = without_line_comments(&source);
    let mut problems = Vec::new();
    if !is_domain_mod(path) && !legacy_facade {
        for forbidden in ["::sqlite::", "otto_storage::drivers"] {
            if source.contains(forbidden) {
                problems.push(format!("concrete storage import `{forbidden}`"));
            }
        }
    } else if legacy_facade {
        let mut imports = source.lines().filter(|line| line.contains("::sqlite::"));
        if imports.any(|line| {
            !line.trim_start().starts_with("pub use ")
                && !line.trim_start().starts_with("pub(crate) use ")
        }) {
            problems.push("legacy facade contains backend logic instead of a pure reexport".into());
        }
    }
    if ["rusqlite::", "libsql::"]
        .iter()
        .any(|token| source.contains(token))
    {
        problems.push("storage SDK import".into());
    }
    if has_sql(&source) {
        problems.push("inline SQL or physical schema".into());
    }
    problems
}

fn frontend_or_runtime_imports(source: &str) -> Vec<String> {
    let source = without_line_comments(&non_test(source));
    let mut problems = Vec::new();
    for module in FRONTENDS {
        if source.contains(&format!("crate::{module}::")) {
            problems.push(format!("frontend `{module}`"));
        }
    }
    if source.contains("crate::app::") || source.contains("crate::runtime::") {
        problems.push("application/runtime layer".into());
    }
    problems
}

fn backend_violations(source: &str) -> Vec<String> {
    let source = without_line_comments(&non_test(source));
    let mut problems = frontend_or_runtime_imports(&source);
    if source.contains("::service::")
        || [
            "crate::approval::Service",
            "crate::memory::Service",
            "crate::workflow::Controller",
        ]
        .iter()
        .any(|token| source.contains(token))
    {
        problems.push("domain service implementation".into());
    }
    problems
}

#[test]
fn domain_services_and_repositories_use_typed_ports() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders = Vec::new();
    for module in DOMAIN_MODULES {
        for path in sources(&src.join(module)) {
            let body = std::fs::read_to_string(&path).expect("read domain source");
            for problem in domain_violations(&path, &body) {
                offenders.push(format!("{}: {problem}", path.display()));
            }
        }
    }
    offenders.sort();
    assert!(
        offenders.is_empty(),
        "domain state code must not own SQL or select concrete storage; move persistence behind a typed port:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn repositories_point_downward() {
    let mut offenders = Vec::new();
    for path in repository_sources() {
        let body = std::fs::read_to_string(&path).expect("read repository source");
        for problem in backend_violations(&body) {
            offenders.push(format!("{}: {problem}", path.display()));
        }
    }
    offenders.sort();
    assert!(
        offenders.is_empty(),
        "repositories may not depend on application services or frontends:\n{}",
        offenders.join("\n")
    );
}

fn direct_sqlite_open_violations(source: &str) -> Vec<String> {
    let source = without_line_comments(&non_test(source));
    if source.contains("Connection::open(") {
        vec!["`Connection::open(` bypasses the injected SqlDriver".into()]
    } else {
        Vec::new()
    }
}

#[test]
fn sqlite_repositories_open_through_the_injected_driver() {
    let mut offenders = Vec::new();
    let repositories = repository_sources();
    assert!(
        repositories.len() >= 15,
        "domain repository discovery found too few files"
    );
    for path in repositories {
        let body = std::fs::read_to_string(&path).expect("read repository source");
        for problem in direct_sqlite_open_violations(&body) {
            offenders.push(format!("{}: {problem}", path.display()));
        }
    }
    offenders.sort();
    assert!(
        offenders.is_empty(),
        "repositories open connections with `Connection::open_with_driver` so fault injection reaches them:\n{}",
        offenders.join("\n")
    );
    assert!(!direct_sqlite_open_violations("fn f() { Connection::open(path); }").is_empty());
    let braces_in_string = "#[cfg(test)]\nmod t {\n    const S: &str = \"}}\";\n    fn f() { Connection::open(p); }\n}";
    assert!(direct_sqlite_open_violations(braces_in_string).is_empty());
    assert!(
        direct_sqlite_open_violations("#[cfg(test)]\nfn f() { Connection::open(path); }")
            .is_empty()
    );
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

#[test]
fn otto_does_not_reexport_core() {
    let mut offenders = Vec::new();
    for path in sources(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src")) {
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
    let service = Path::new("workflow/service.rs");
    assert!(!domain_violations(service, "use crate::workflow::sqlite::Store;").is_empty());
    assert!(!domain_violations(service, "const SQL: &str = \"SELECT * FROM runs\";").is_empty());
    assert!(domain_violations(service, "fn f() { invalid(\"update identity\"); }").is_empty());
    assert!(!backend_violations("use crate::runtime::Builder;").is_empty());
    assert!(
        domain_violations(
            Path::new("workflow/mod.rs"),
            "pub use crate::workflow::sqlite::Store;"
        )
        .is_empty()
    );
    let task_record = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/subagent/record.rs");
    assert!(domain_violations(&task_record, "pub use crate::subagent::sqlite::Store;").is_empty());
    assert!(
        !domain_violations(
            &task_record,
            "pub use crate::subagent::sqlite::Store;\nfn open() { Store::open(); }"
        )
        .is_empty()
    );
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let usage_store = src.join("usage/sqlite.rs");
    let usage_sql = "use otto_storage::drivers::sqlite::SqliteDriver;\nconst SQL: &str = \"SELECT * FROM usage_events\";";
    assert!(domain_violations(&usage_store, usage_sql).is_empty());
    assert!(!domain_violations(&usage_store, "use rusqlite::Connection;").is_empty());
    // A repository is `<domain>/sqlite.rs` or a file below `<domain>/**/sqlite/`.
    for repository in [
        "approval/sqlite.rs",
        "approval/sqlite/schema.rs",
        "schedule/sqlite/tests/claims_and_backoff.rs",
        "workflow/sqlite/runs.rs",
        "subagent/sqlite.rs",
        "memory/advice/sqlite.rs",
        "skill/sqlite.rs",
    ] {
        assert!(is_domain_repository(&src.join(repository)), "{repository}");
        assert!(domain_violations(&src.join(repository), usage_sql).is_empty());
    }
    for other in [
        "approval/service.rs",
        "workflow/mod.rs",
        "subagent/tasks.rs",
        "memory/files/sqlite_like.rs",
        "skill/check.rs",
        "tool/sqlite.rs",
    ] {
        assert!(!is_domain_repository(&src.join(other)), "{other}");
    }
    let approval_service = src.join("approval/service.rs");
    assert!(!domain_violations(&approval_service, usage_sql).is_empty());
    assert!(
        !domain_violations(&approval_service, "use crate::approval::sqlite::Store;").is_empty()
    );
    assert!(
        !domain_violations(
            &src.join("workflow/service.rs"),
            "use rusqlite::Connection;"
        )
        .is_empty()
    );
    let usage_other = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/usage/mod.rs");
    assert!(
        domain_violations(
            &usage_other,
            "const SQL: &str = \"SELECT * FROM usage_events\";"
        )
        .len()
            == 1
    );
    let test_only_sql = "#[cfg(test)]\nmod tests { const SQL: &str = \"SELECT * FROM runs\"; }";
    assert!(domain_violations(service, test_only_sql).is_empty());
    assert!(!core_reexport_violations("pub use otto_core::approval::*;").is_empty());
    assert!(
        !core_reexport_violations(
            "pub(crate) use otto_core::session::turn::MAX_AUTOMATIC_RESUMES;"
        )
        .is_empty()
    );
    assert!(core_reexport_violations("use otto_core::approval::ApprovalMode;").is_empty());
    assert!(core_reexport_violations("//! pub use otto_core::x;").is_empty());
    assert!(
        core_reexport_violations(
            "#[cfg(test)]\nmod tests {\n    pub use otto_core::approval::*;\n}"
        )
        .is_empty()
    );
}
