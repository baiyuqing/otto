//! Source guard: otto-core performs no I/O.
//!
//! otto-core holds I/O-free contracts, formats and the agent loop. Filesystem,
//! process, network, environment, stdio and clock access live in `crates/otto`.
//! `cargo check --target wasm32-unknown-unknown` does not catch std I/O,
//! because `std::fs`, `std::process`, `std::net` and `std::env` compile on that
//! target and fail only at runtime.
#![cfg(not(target_arch = "wasm32"))]

use std::path::{Path, PathBuf};

const FORBIDDEN: &[&str] = &[
    "std::fs",
    "std::process",
    "std::net",
    "std::env",
    "std::io::stdin",
    "std::io::stdout",
    "std::io::stderr",
    "println!",
    "eprintln!",
    "print!",
    "eprint!",
    "SystemTime::now",
    "Instant::now",
];
const FORBIDDEN_STD_MODULES: &[&str] = &["fs", "process", "net", "env"];

fn rust_files(path: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(path).expect("read source directory") {
        let path = entry.expect("read source entry").path();
        if path.is_dir() {
            found.extend(rust_files(&path));
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            found.push(path);
        }
    }
    found.sort();
    found
}

/// Files declared by `#[cfg(test)]` followed by `mod NAME;` in a scanned file.
fn out_of_line_test_files(files: &[PathBuf]) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for file in files {
        let source = std::fs::read_to_string(file).expect("read source file");
        let stem = file.file_stem().unwrap().to_string_lossy();
        let parent = file.parent().unwrap();
        let module_dir = if matches!(&*stem, "lib" | "main" | "mod") {
            parent.to_path_buf()
        } else {
            parent.join(&*stem)
        };
        let lines: Vec<&str> = source.lines().collect();
        for pair in lines.windows(2) {
            if pair[0].trim() != "#[cfg(test)]" {
                continue;
            }
            let declaration = pair[1].trim();
            let declaration = match declaration.strip_prefix("pub") {
                Some(rest) => match rest.find(')') {
                    Some(end) if rest.starts_with('(') => &rest[end + 1..],
                    _ => rest,
                },
                None => declaration,
            };
            let Some(name) = declaration
                .trim()
                .strip_prefix("mod ")
                .and_then(|rest| rest.trim().strip_suffix(';'))
            else {
                continue;
            };
            for candidate in [
                module_dir.join(format!("{name}.rs")),
                module_dir.join(name).join("mod.rs"),
            ] {
                if candidate.is_file() {
                    found.push(candidate);
                }
            }
        }
    }
    found.sort();
    found
}

// `brace_balance`, `non_test` and `without_line_comments` mirror
// `crates/otto/tests/storage_boundary.rs`; test crates cannot share a module.

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

/// Forbidden modules named by top-level items of every `std::{ ... }` group.
fn grouped_std_modules(source: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = source;
    while let Some(start) = rest.find("std::{") {
        let body = &rest[start + "std::{".len()..];
        let mut depth = 1;
        let mut item = String::new();
        let mut items = Vec::new();
        let mut end = body.len();
        for (index, character) in body.char_indices() {
            match character {
                '{' => depth += 1,
                '}' => depth -= 1,
                _ => {}
            }
            if depth == 0 {
                end = index;
                break;
            }
            if character == ',' && depth == 1 {
                items.push(std::mem::take(&mut item));
            } else {
                item.push(character);
            }
        }
        items.push(item);
        for item in items {
            let name = item.trim().split("::").next().unwrap_or("").trim();
            if FORBIDDEN_STD_MODULES.contains(&name) {
                found.push(name.to_string());
            }
        }
        rest = &body[end..];
    }
    found
}

fn violations(source: &str) -> Vec<String> {
    let source = without_line_comments(&non_test(source));
    let mut found: Vec<String> = FORBIDDEN
        .iter()
        .filter(|pattern| source.contains(**pattern))
        .map(|pattern| format!("uses `{pattern}`"))
        .collect();
    found.extend(
        grouped_std_modules(&source)
            .into_iter()
            .map(|name| format!("imports `std::{name}` in a group")),
    );
    found
}

#[test]
fn core_source_performs_no_io() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let files = rust_files(&src);
    let test_only = out_of_line_test_files(&files);
    let mut problems = Vec::new();
    for file in files.iter().filter(|file| !test_only.contains(file)) {
        let source = std::fs::read_to_string(file).expect("read source file");
        for problem in violations(&source) {
            problems.push(format!("{}: {problem}", file.display()));
        }
    }
    problems.sort();
    assert!(
        problems.is_empty(),
        "otto-core must perform no I/O (filesystem, process, network, environment, stdio, \
         clock); native I/O belongs in crates/otto:\n{}",
        problems.join("\n")
    );
}

#[test]
fn fixtures_prove_io_is_detected() {
    let flagged = [
        "use std::fs::read;",
        "fn f() { std::process::Command::new(\"x\"); }",
        "use std::{fs, path::Path};",
        "use std::{\n    collections::HashMap,\n    env,\n};",
        "use std::{io, process::Command};",
        "fn f() { let t = Instant::now(); }",
        "fn f() { println!(\"x\"); }",
    ];
    for source in flagged {
        assert!(!violations(source).is_empty(), "not flagged: {source}");
    }

    let allowed = [
        "//! never via std::env::var",
        "#[cfg(test)]\nmod tests {\n    fn f() { std::fs::read(\"x\").unwrap(); }\n}",
        "#[cfg(test)]\nfn helper() { std::fs::read(\"x\").unwrap(); }",
        "use std::{collections::HashMap, sync::Mutex};",
        "const S: &str = \"}}\"; fn f() {}",
    ];
    for source in allowed {
        assert_eq!(
            violations(source),
            Vec::<String>::new(),
            "flagged: {source}"
        );
    }

    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let test_only = out_of_line_test_files(&rust_files(&src));
    for expected in ["memory/tests.rs", "agent/run_tests.rs"] {
        assert!(
            test_only.contains(&src.join(expected)),
            "out-of-line test file not derived: {expected}; found {test_only:?}"
        );
    }
}
