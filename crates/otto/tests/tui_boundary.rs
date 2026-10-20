//! Dependency guards for the reusable TUI interaction layer.

use std::path::{Path, PathBuf};

fn rust_sources(path: &Path) -> Vec<PathBuf> {
    if path.is_file() {
        return vec![path.to_path_buf()];
    }
    let mut files = Vec::new();
    for entry in std::fs::read_dir(path).expect("source directory is readable") {
        let path = entry.expect("source entry is readable").path();
        let name = path.file_name().unwrap().to_string_lossy();
        // Out-of-line test modules: `tests`, `tests.rs`, `*_tests`, `*_tests.rs`.
        let stem = name.trim_end_matches(".rs");
        if stem == "tests" || stem.ends_with("_tests") {
            continue;
        }
        if path.is_dir() {
            files.extend(rust_sources(&path));
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            files.push(path);
        }
    }
    files.sort();
    files
}

fn production(source: &str) -> &str {
    source.split("#[cfg(test)]").next().unwrap_or(source)
}

fn imports(source: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut current = String::new();
    for line in production(source).lines() {
        let line = line.trim();
        if current.is_empty() && (line.starts_with("use ") || line.starts_with("pub(crate) use ")) {
            current.push_str(line);
        } else if !current.is_empty() {
            current.push(' ');
            current.push_str(line);
        } else {
            continue;
        }
        if current.contains(';') {
            found.push(std::mem::take(&mut current));
        }
    }
    found
}

fn code(source: &str) -> String {
    let bytes = production(source).as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(b"//") {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
        } else if bytes[i..].starts_with(b"/*") {
            i += 2;
            let mut depth = 1;
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
        } else if let Some(end) = raw_string_end(bytes, i) {
            i = end;
        } else if bytes[i] == b'"' || bytes[i] == b'\'' {
            let quote = bytes[i];
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\\' {
                    i = (i + 2).min(bytes.len());
                } else if bytes[i] == quote {
                    i += 1;
                    break;
                } else {
                    i += 1;
                }
            }
        } else {
            output.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(output).expect("removing ASCII syntax preserves UTF-8")
}

fn raw_string_end(bytes: &[u8], start: usize) -> Option<usize> {
    let raw_start = if bytes[start..].starts_with(b"br") || bytes[start..].starts_with(b"cr") {
        start + 1
    } else {
        start
    };
    if bytes.get(raw_start) != Some(&b'r') {
        return None;
    }
    let mut quote = raw_start + 1;
    while bytes.get(quote) == Some(&b'#') {
        quote += 1;
    }
    if bytes.get(quote) != Some(&b'"') {
        return None;
    }
    let hashes = quote - raw_start - 1;
    let mut i = quote + 1;
    while i < bytes.len() {
        if bytes[i] == b'"'
            && bytes.get(i + 1..i + 1 + hashes) == Some(&bytes[quote + 1..quote + 1 + hashes])
        {
            return Some(i + 1 + hashes);
        }
        i += 1;
    }
    Some(bytes.len())
}

fn has_identifier(source: &str, needle: &str) -> bool {
    let source = code(source);
    source.match_indices(needle).any(|(start, _)| {
        let end = start + needle.len();
        let is_word =
            |byte: Option<u8>| byte.is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_');
        !is_word(source.as_bytes().get(start.wrapping_sub(1)).copied())
            && !is_word(source.as_bytes().get(end).copied())
    })
}

fn forbidden(source: &str, rules: &[(&str, &str)]) -> Vec<String> {
    rules
        .iter()
        .filter_map(|(marker, reason)| source.contains(marker).then_some((*reason).to_string()))
        .collect()
}

fn kernel_offenses(source: &str) -> Vec<String> {
    let mut offenses = Vec::new();
    for import in imports(source) {
        offenses.extend(forbidden(
            &import,
            &[
                ("ratatui", "terminal rendering"),
                ("crossterm", "terminal events"),
                ("::backend", "TUI backend"),
                ("crate::app", "application service"),
                ("Controller", "controller handle"),
                ("Remote", "remote handle"),
            ],
        ));
    }
    offenses
}

fn feature_offenses(source: &str) -> Vec<String> {
    let source = production(source);
    let mut offenses = Vec::new();
    for import in imports(source) {
        offenses.extend(forbidden(
            &import,
            &[
                ("ratatui", "terminal rendering"),
                ("crossterm", "terminal events"),
            ],
        ));
    }
    for identifier in ["KeyCode", "KeyEvent", "Controller", "Remote"] {
        if has_identifier(source, identifier) {
            offenses.push(format!("terminal or service handle `{identifier}`"));
        }
    }
    offenses
}

fn rendering_offenses(source: &str) -> Vec<String> {
    let source = production(source);
    let mut offenses = Vec::new();
    for import in imports(source) {
        offenses.extend(forbidden(
            &import,
            &[
                ("Controller", "controller handle"),
                ("Remote", "remote handle"),
                ("::backend", "backend handle"),
                ("std::fs", "filesystem access"),
                ("std::net", "network access"),
                ("tokio::fs", "filesystem access"),
                ("tokio::net", "network access"),
                ("reqwest", "network client"),
            ],
        ));
    }
    for identifier in ["Controller", "Remote", "TcpStream", "File", "OpenOptions"] {
        if has_identifier(source, identifier) {
            offenses.push(format!("service or I/O handle `{identifier}`"));
        }
    }
    offenses
}

#[test]
fn boundary_helpers_reject_forbidden_code_and_ignore_tests_and_comments() {
    assert!(kernel_offenses("use crossterm::event::KeyCode;").contains(&"terminal events".into()));
    assert!(feature_offenses("use crate::app::Controller; fn f() { let _: KeyCode; }").len() >= 2);
    assert!(rendering_offenses("use std::fs::File; fn f() { File::open(\"x\"); }").len() >= 2);
    assert!(
        kernel_offenses("// use ratatui::Frame;\n#[cfg(test)]\nuse crossterm::event::KeyCode;")
            .is_empty()
    );
    assert!(feature_offenses("// Controller KeyEvent\nlet title = \"Remote\";").is_empty());
    assert!(
        feature_offenses("use crate::app::Info; use super::backend::{Request, Reply};").is_empty()
    );
    assert!(
        rendering_offenses("use crate::tui::app::{App, TurnStatus}; use ratatui::Frame;")
            .is_empty()
    );
}

#[test]
fn interaction_kernel_has_no_terminal_backend_or_service_imports() {
    let tui = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/tui");
    let mut files = vec![tui.join("interaction.rs")];
    files.extend(rust_sources(&tui.join("interaction")));
    let offenses = scan(&files, kernel_offenses);
    assert!(
        offenses.is_empty(),
        "interaction kernel boundary violations:\n{}",
        offenses.join("\n")
    );
}

#[test]
fn feature_presenters_do_not_own_terminal_or_service_handles() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/tui/features");
    let files = rust_sources(&root);
    let offenses = scan(&files, feature_offenses);
    assert!(
        offenses.is_empty(),
        "feature boundary violations:\n{}",
        offenses.join("\n")
    );
}

#[test]
fn rendering_reads_views_and_snapshots_without_io_or_services() {
    let tui = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/tui");
    let files = [tui.join("widgets.rs"), tui.join("render.rs")];
    let offenses = scan(&files, rendering_offenses);
    assert!(
        offenses.is_empty(),
        "rendering boundary violations:\n{}",
        offenses.join("\n")
    );
}

fn scan(files: &[PathBuf], check: fn(&str) -> Vec<String>) -> Vec<String> {
    let mut offenses = Vec::new();
    for path in files {
        let source = std::fs::read_to_string(path).expect("a readable source file");
        for offense in check(&source) {
            offenses.push(format!("{}: {offense}", path.display()));
        }
    }
    offenses.sort();
    offenses
}
