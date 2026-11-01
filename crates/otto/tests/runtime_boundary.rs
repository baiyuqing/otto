//! Source guard for the layer boundary inside `crates/otto`.
//!
//! The frontends (`cli`, `tui`, `acp`, `server`, `client`) build on the
//! runtime layer: every other top-level module, including `runtime`, which
//! assembles the agent. The runtime layer must not depend on a frontend, so
//! that the two can become separate crates. See
//! `docs/development/architecture.md#package-boundaries`.
//!
//! The scan covers the non-test source of every runtime module. Test code
//! may still import frontend modules. Fix a failure by
//! moving the item the runtime needs into the runtime layer, not by adding an
//! exemption.

use std::path::{Path, PathBuf};

/// The frontend modules, which only the frontends and `main.rs` may import.
const FRONTENDS: &[&str] = &["cli", "tui", "acp", "server", "client"];

/// The crate root files, which declare every module and are not a layer.
const ROOT_FILES: &[&str] = &["lib.rs", "main.rs"];

fn sources(path: &Path) -> Vec<PathBuf> {
    if path.is_file() {
        return vec![path.to_path_buf()];
    }
    let mut found = Vec::new();
    for entry in std::fs::read_dir(path).expect("a readable source directory") {
        let path = entry.expect("a readable entry").path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        // Out-of-line test modules: `tests`, `tests.rs`, `*_tests`, `*_tests.rs`.
        let stem = name.trim_end_matches(".rs");
        if stem == "tests" || stem.ends_with("_tests") {
            continue;
        }
        if path.is_dir() {
            found.extend(sources(&path));
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            found.push(path);
        }
    }
    found.sort();
    found
}

/// The source without its test-only items: each item after a
/// `#[cfg(test)]` line, whether a test module of any name or a test helper,
/// up to its closing brace or semicolon.
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

/// The source without line comments, on one line, so a `use crate::{...}`
/// group that spans lines reads as one.
fn code(source: &str) -> String {
    source
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The frontend modules `source` imports or names by path, through
/// `crate::<module>` or a `crate::{...}` group, and through `super::` when
/// `top_level` says the file is a top-level module, whose parent is the
/// crate root.
fn frontend_references(source: &str, top_level: bool) -> Vec<String> {
    let code = code(source);
    let mut found = Vec::new();
    let prefixes: &[&str] = if top_level {
        &["crate::", "super::"]
    } else {
        &["crate::"]
    };
    for prefix in prefixes {
        let mut rest = code.as_str();
        while let Some(index) = rest.find(prefix) {
            rest = &rest[index + prefix.len()..];
            if let Some(group) = rest.strip_prefix('{') {
                found.extend(
                    group_heads(group)
                        .into_iter()
                        .filter(|head| is_frontend(head)),
                );
            } else {
                let head = path_head(rest);
                if is_frontend(head) {
                    found.push(head.to_string());
                }
            }
        }
    }
    found
}

/// The first path segment of each top-level item in a `{...}` group, given
/// the text after its opening brace.
fn group_heads(group: &str) -> Vec<String> {
    let mut heads = Vec::new();
    let mut depth = 0;
    let mut at_item_start = true;
    for (index, ch) in group.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' if depth == 0 => break,
            '}' => depth -= 1,
            ',' if depth == 0 => at_item_start = true,
            c if at_item_start && depth == 0 && !c.is_whitespace() => {
                heads.push(path_head(&group[index..]).to_string());
                at_item_start = false;
            }
            _ => {}
        }
    }
    heads
}

fn path_head(path: &str) -> &str {
    let end = path
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(path.len());
    &path[..end]
}

fn is_frontend(head: &str) -> bool {
    FRONTENDS.contains(&head)
}

/// Every top-level module of `src/` that is not a frontend.
fn runtime_modules(src: &Path) -> Vec<PathBuf> {
    let mut modules = Vec::new();
    for entry in std::fs::read_dir(src).expect("src is readable") {
        let path = entry.expect("a readable entry").path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let module = name.trim_end_matches(".rs");
        if ROOT_FILES.contains(&name.as_str()) || is_frontend(module) {
            continue;
        }
        if path.is_dir() || name.ends_with(".rs") {
            modules.push(path);
        }
    }
    modules.sort();
    modules
}

#[test]
fn the_runtime_layer_does_not_depend_on_a_frontend() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let modules = runtime_modules(&src);
    assert!(
        modules.iter().any(|path| path.ends_with("runtime")),
        "src/runtime is missing from the scan"
    );

    let mut offenders = Vec::new();
    for module in &modules {
        for path in sources(module) {
            let source = std::fs::read_to_string(&path).expect("a readable source");
            let top_level = path.parent() == Some(src.as_path())
                || (path.ends_with("mod.rs")
                    && path.parent().and_then(Path::parent) == Some(src.as_path()));
            for frontend in frontend_references(&non_test(&source), top_level) {
                let file = path.strip_prefix(&src).unwrap_or(&path).display();
                offenders.push(format!("src/{file} uses `{frontend}`"));
            }
        }
    }
    offenders.sort();
    offenders.dedup();
    assert!(
        offenders.is_empty(),
        "runtime modules may not depend on a frontend ({}); move what they need into the \
         runtime layer (docs/specs/2026-10-05-otto-crate-split.md):\n{}",
        FRONTENDS.join(", "),
        offenders.join("\n")
    );
}

#[test]
fn the_scan_finds_paths_groups_and_super_imports() {
    let source = "use crate::cli::run;\n\
                  use crate::{\n    app::Controller,\n    tui::{self, app},\n};\n\
                  fn f() { super::server::listen(); }\n\
                  // crate::acp::serve in a comment is ignored\n\
                  use crate::clientele::Thing;\n\
                  use crate::{runtime::{cli_like, builder::Runner}, tool};\n";
    assert_eq!(
        frontend_references(source, true),
        vec!["cli".to_string(), "tui".to_string(), "server".to_string()]
    );
    // Below the top level, `super::server` is a sibling module of that name.
    assert_eq!(
        frontend_references(source, false),
        vec!["cli".to_string(), "tui".to_string()]
    );
}

#[test]
fn test_only_items_are_not_scanned() {
    let source = "use crate::app::Controller;\n\
                  #[cfg(test)]\nmod mcp_tests {\n    use crate::cli::flags;\n}\n\
                  #[cfg(test)]\npub(crate) fn helper() {\n    use crate::cli::flags::parse;\n}\n\
                  #[cfg(test)]\nuse crate::tui::app;\n\
                  fn production() { crate::server::listen(); }\n";
    assert_eq!(
        frontend_references(&non_test(source), true),
        vec!["server".to_string()]
    );
}
