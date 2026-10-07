//! Source guard for native storage ownership and dependency direction.
//!
//! Domain services own state semantics and import typed repository ports.
//! Concrete drivers and repository adapters live under `storage/`; only domain
//! `mod.rs` files may retain transitional constructors. Three named legacy
//! facades may contain only a pure backend Store reexport.

use std::path::{Path, PathBuf};

const DOMAIN_MODULES: &[&str] = &[
    "approval",
    "memory",
    "reflection",
    "session",
    "schedule",
    "workflow",
    "skill",
    "usage",
    "subagent",
];
const FRONTENDS: &[&str] = &["cli", "tui", "acp", "server", "client"];
const DOMAINS: &[&str] = &[
    "approval",
    "memory",
    "reflection",
    "session",
    "schedule",
    "workflow",
    "skill",
    "usage",
    "subagent",
];

fn sources(path: &Path) -> Vec<PathBuf> {
    if path.is_file() {
        return vec![path.to_path_buf()];
    }
    let mut found = Vec::new();
    for entry in std::fs::read_dir(path).expect("read source directory") {
        let path = entry.expect("read source entry").path();
        let name = path.file_name().unwrap().to_string_lossy();
        if path.is_dir() {
            if name != "tests" {
                found.extend(sources(&path));
            }
        } else if path.extension().is_some_and(|extension| extension == "rs") && name != "tests.rs"
        {
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

fn is_domain_mod(path: &Path) -> bool {
    path.file_name().and_then(|name| name.to_str()) == Some("mod.rs")
}

fn is_named_legacy_facade(path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(Path::new(env!("CARGO_MANIFEST_DIR")).join("src")) else {
        return false;
    };
    [
        Path::new("memory/turso.rs"),
        Path::new("reflection/store.rs"),
        Path::new("subagent/record.rs"),
    ]
    .contains(&relative)
}

fn is_pure_legacy_facade(path: &Path, source: &str) -> bool {
    if !is_named_legacy_facade(path) {
        return false;
    }
    let source = non_test(source);
    let relevant: Vec<&str> = source
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .filter(|line| line.contains("crate::storage::backends"))
        .map(str::trim)
        .collect();
    let code = without_line_comments(&source);
    !relevant.is_empty()
        && relevant
            .iter()
            .all(|line| line.starts_with("pub use ") || line.starts_with("pub(crate) use "))
        && !code.contains("Store::")
        && !code.contains("crate::storage::Connection")
        && !code.contains("crate::storage::params")
        && !has_sql(&code)
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
    let unsupported_backends = source.replace("crate::storage::backends::posix::session", "");
    let unsupported_drivers = source.replace("crate::storage::drivers::posix::PosixDriver", "");
    if unsupported_backends.contains("crate::storage::backends::") {
        problems.push("session compatibility facade imports a non-session backend".into());
    }
    if unsupported_drivers.contains("crate::storage::drivers::") {
        problems.push("session compatibility facade imports a non-POSIX driver".into());
    }
    for forbidden in [
        "crate::storage::Connection",
        "crate::storage::Transaction",
        "crate::storage::params",
        "crate::storage::drivers::turso::",
        "crate::app::",
        "crate::runtime::",
        "crate::cli::",
        "crate::server::",
        "crate::tui::",
        "crate::acp::",
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

fn domain_violations(path: &Path, source: &str) -> Vec<String> {
    if path
        .strip_prefix(Path::new(env!("CARGO_MANIFEST_DIR")).join("src"))
        .is_ok_and(|relative| relative == Path::new("session/legacy.rs"))
    {
        return session_legacy_violations(source);
    }
    let source = non_test(source);
    let legacy_facade = is_pure_legacy_facade(path, &source);
    let source = without_line_comments(&source);
    let mut problems = Vec::new();
    if !is_domain_mod(path) && !legacy_facade {
        for forbidden in ["crate::storage::backends", "crate::storage::drivers"] {
            if source.contains(forbidden) {
                problems.push(format!("concrete storage import `{forbidden}`"));
            }
        }
    } else if legacy_facade {
        let mut imports = source
            .lines()
            .filter(|line| line.contains("crate::storage::backends"));
        if imports.any(|line| {
            !line.trim_start().starts_with("pub use ")
                && !line.trim_start().starts_with("pub(crate) use ")
        }) {
            problems.push("legacy facade contains backend logic instead of a pure reexport".into());
        }
    }
    if [
        "turso::Builder",
        "turso::Connection",
        "rusqlite::",
        "libsql::",
    ]
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
            "crate::reflection::Service",
            "crate::workflow::Controller",
        ]
        .iter()
        .any(|token| source.contains(token))
    {
        problems.push("domain service implementation".into());
    }
    problems
}

fn driver_violations(source: &str) -> Vec<String> {
    let source = without_line_comments(&non_test(source));
    let mut problems = frontend_or_runtime_imports(&source);
    for domain in DOMAINS {
        if source.contains(&format!("crate::{domain}::")) {
            problems.push(format!("domain `{domain}`"));
        }
    }
    problems
}

fn migration_core_violations(source: &str) -> Vec<String> {
    let source = without_line_comments(&non_test(source));
    let mut problems = Vec::new();
    for forbidden in [
        "crate::storage::backends",
        "crate::storage::drivers",
        "turso::",
        "rusqlite::",
        "libsql::",
        "crate::storage::Connection",
    ] {
        if source.contains(forbidden) {
            problems.push(format!("physical storage dependency `{forbidden}`"));
        }
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
fn drivers_and_adapters_point_downward() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/storage");
    let mut offenders = Vec::new();
    for path in sources(&src.join("drivers")) {
        let body = std::fs::read_to_string(&path).expect("read driver source");
        for problem in driver_violations(&body) {
            offenders.push(format!("{}: {problem}", path.display()));
        }
    }
    for path in sources(&src.join("backends")) {
        let body = std::fs::read_to_string(&path).expect("read backend source");
        for problem in backend_violations(&body) {
            offenders.push(format!("{}: {problem}", path.display()));
        }
    }
    offenders.sort();
    assert!(
        offenders.is_empty(),
        "drivers and repositories may not depend on application services or frontends:\n{}",
        offenders.join("\n")
    );
}

fn direct_turso_open_violations(source: &str) -> Vec<String> {
    let source = without_line_comments(&non_test(source));
    if source.contains("Connection::open(") {
        vec!["`Connection::open(` bypasses the injected SqlDriver".into()]
    } else {
        Vec::new()
    }
}

#[test]
fn turso_repositories_open_through_the_injected_driver() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/storage/backends/turso");
    let mut offenders = Vec::new();
    for path in sources(&root) {
        let body = std::fs::read_to_string(&path).expect("read repository source");
        for problem in direct_turso_open_violations(&body) {
            offenders.push(format!("{}: {problem}", path.display()));
        }
    }
    offenders.sort();
    assert!(
        offenders.is_empty(),
        "repositories open connections with `Connection::open_with_driver` so fault injection reaches them:\n{}",
        offenders.join("\n")
    );
    assert!(!direct_turso_open_violations("fn f() { Connection::open(path); }").is_empty());
    let braces_in_string = "#[cfg(test)]\nmod t {\n    const S: &str = \"}}\";\n    fn f() { Connection::open(p); }\n}";
    assert!(direct_turso_open_violations(braces_in_string).is_empty());
    assert!(
        direct_turso_open_violations("#[cfg(test)]\nfn f() { Connection::open(path); }").is_empty()
    );
}

#[test]
fn migration_contracts_do_not_depend_on_physical_backends() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/storage/migration");
    let mut offenders = Vec::new();
    for path in sources(&root) {
        let body = std::fs::read_to_string(&path).expect("read migration contract source");
        for problem in migration_core_violations(&body) {
            offenders.push(format!("{}: {problem}", path.display()));
        }
    }
    offenders.sort();
    assert!(
        offenders.is_empty(),
        "the migration coordinator owns ordering, not physical storage:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn fixtures_prove_forbidden_edges_are_detected() {
    let service = Path::new("workflow/service.rs");
    assert!(
        !domain_violations(
            service,
            "use crate::storage::backends::turso::workflow::Store;"
        )
        .is_empty()
    );
    assert!(!domain_violations(service, "const SQL: &str = \"SELECT * FROM runs\";").is_empty());
    assert!(domain_violations(service, "fn f() { invalid(\"update identity\"); }").is_empty());
    assert!(!driver_violations("use crate::memory::Service;").is_empty());
    assert!(!backend_violations("use crate::runtime::Builder;").is_empty());
    assert!(
        !migration_core_violations("use crate::storage::drivers::turso::TursoDriver;").is_empty()
    );
    assert!(
        domain_violations(
            Path::new("workflow/mod.rs"),
            "pub use crate::storage::backends::turso::workflow::Store;"
        )
        .is_empty()
    );
    let memory_turso = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/memory/turso.rs");
    assert!(
        domain_violations(
            &memory_turso,
            "pub use crate::storage::backends::turso::memory::Store;"
        )
        .is_empty()
    );
    assert!(
        !domain_violations(
            &memory_turso,
            "pub use crate::storage::backends::turso::memory::Store;\nfn open() { Store::open(); }"
        )
        .is_empty()
    );
    let reflection_store = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/reflection/store.rs");
    assert!(
        domain_violations(
            &reflection_store,
            "pub use crate::storage::backends::turso::reflection::Store;"
        )
        .is_empty()
    );
    let facade_with_preceding_imports = "//! legacy facade\nuse std::fmt;\n\npub use crate::storage::backends::turso::reflection::Store;";
    assert!(domain_violations(&reflection_store, facade_with_preceding_imports).is_empty());

    assert!(!domain_violations(
        &reflection_store,
        "pub use crate::storage::backends::turso::reflection::Store;\nfn open() { Store::open(); }"
    ).is_empty());
    let task_record = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/subagent/record.rs");
    assert!(
        domain_violations(
            &task_record,
            "pub use crate::storage::backends::turso::tasks::Store;"
        )
        .is_empty()
    );
    assert!(
        !domain_violations(
            &task_record,
            "pub use crate::storage::backends::turso::tasks::Store;\nfn open() { Store::open(); }"
        )
        .is_empty()
    );
    let legacy_session = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/session/legacy.rs");
    let allowed_constructor = "use crate::storage::backends::posix::session::Store;\nuse crate::storage::drivers::posix::PosixDriver;\nimpl Store { fn create_lazy() { Self::create_lazy_with_driver_and_keeper(); } }";
    assert!(domain_violations(&legacy_session, allowed_constructor).is_empty());
    let service_call = "use crate::storage::backends::posix::session::Store;\nimpl Store { fn create_lazy() { Store::recover(\"scope\"); } }";
    assert!(!domain_violations(&legacy_session, service_call).is_empty());
    let mutating_constructor = "use crate::storage::backends::posix::session::Store;\nimpl Store { fn create_lazy() { Self::create_lazy_with_driver_and_keeper(); } }\nconst SQL: &str = \"UPDATE sessions SET status='closed'\";";
    assert!(!domain_violations(&legacy_session, mutating_constructor).is_empty());
    let test_only_sql = "#[cfg(test)]\nmod tests { const SQL: &str = \"SELECT * FROM runs\"; }";
    assert!(domain_violations(service, test_only_sql).is_empty());
}
