//! Source guard for the layer order inside `crates/otto`.
//!
//! Every top-level module of `src/` belongs to one layer in `LAYERS`, lowest
//! first. In non-test code a module may reference modules of its own layer
//! or a lower one. This test reports three kinds of violation: a reference to
//! a higher layer, a reference to a test-support module (`fixtures`,
//! `faults`) from a non-test-support module, and a reference between two
//! modules of the same layer that lie on a reference cycle in that layer.
//! Any violation fails the test.
//!
//! The scan covers non-test source: it skips out-of-line test files and
//! items after `#[cfg(test)]`. See
//! `docs/development/architecture.md#layers-inside-otto`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// The layers, lowest first, with the top-level modules of each.
const LAYERS: &[(&str, &[&str])] = &[
    (
        "platform",
        &[
            "hex",
            "deadline",
            "retry",
            "fsops",
            "log",
            "tls_provider",
            "gourl",
            "urlprivacy",
            "process_identity",
            "program_status",
            "herdr",
            "random",
            "child_fence",
        ],
    ),
    ("persistence", &["storage", "workspace", "session", "usage"]),
    (
        "capability",
        &[
            "auth", "provider", "typesafe", "sandbox", "config", "tool", "mcp", "skill", "memory",
            "approval", "decision",
        ],
    ),
    (
        "orchestration",
        &[
            "subagent", "workflow", "schedule", "delivery", "plugin", "failover", "metrics",
        ],
    ),
    ("assembly", &["runtime"]),
    ("application", &["app"]),
    ("frontend", &["cli", "client", "tui"]),
    ("test-support", &["fixtures", "faults"]),
];

/// The layer whose modules may import anything and may not be imported by
/// the other layers.
const TEST_SUPPORT: &str = "test-support";

/// The crate root file, which declares every module and is not a layer.
const ROOT_FILES: &[&str] = &["lib.rs"];

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

/// The source without its test-only items (their lines become empty, so line
/// numbers stay valid): each item after a
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
        kept.push("");
        index += 1;
        let mut depth = 0i32;
        let mut opened = false;
        while index < lines.len() {
            let line = lines[index];
            index += 1;
            kept.push("");
            depth += line.matches('{').count() as i32 - line.matches('}').count() as i32;
            opened |= line.contains('{');
            if (opened && depth <= 0) || (!opened && line.trim_end().ends_with(';')) {
                break;
            }
        }
    }
    kept.join("\n")
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

/// The top-level modules `source` imports or names by path, through
/// `crate::<module>` or a `crate::{...}` group, and through `super::` when
/// `top_level` says the file is a top-level module, whose parent is the
/// crate root. Each result is `(module, line)` with a 1-based line.
fn module_references(
    source: &str,
    top_level: bool,
    modules: &BTreeSet<String>,
) -> Vec<(String, usize)> {
    let mut code = String::new();
    let mut starts = Vec::new();
    for line in source.lines() {
        starts.push(code.len());
        if !line.trim_start().starts_with("//") {
            code.push_str(line);
        }
        code.push(' ');
    }
    let line_of = |offset: usize| starts.partition_point(|start| *start <= offset);
    let prefixes: &[&str] = if top_level {
        &["crate::", "super::"]
    } else {
        &["crate::"]
    };
    let mut found = Vec::new();
    for prefix in prefixes {
        let mut from = 0;
        while let Some(index) = code[from..].find(prefix) {
            let start = from + index + prefix.len();
            from = start;
            let rest = &code[start..];
            let line = line_of(start);
            if let Some(group) = rest.strip_prefix('{') {
                for head in group_heads(group) {
                    if modules.contains(&head) {
                        found.push((head, line));
                    }
                }
            } else {
                let head = path_head(rest);
                if modules.contains(head) {
                    found.push((head.to_string(), line));
                }
            }
        }
    }
    found
}

/// The top-level modules of `src/` mapped to their non-test source files.
/// `x.rs` and `x/` are both module `x`.
fn module_files(src: &Path) -> BTreeMap<String, Vec<PathBuf>> {
    let mut modules: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    for entry in std::fs::read_dir(src).expect("src is readable") {
        let path = entry.expect("a readable entry").path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if ROOT_FILES.contains(&name.as_str()) || !(path.is_dir() || name.ends_with(".rs")) {
            continue;
        }
        modules
            .entry(name.trim_end_matches(".rs").to_string())
            .or_default()
            .extend(sources(&path));
    }
    modules
}

fn layer_of(module: &str) -> Option<usize> {
    LAYERS
        .iter()
        .position(|(_, members)| members.contains(&module))
}

fn in_test_support(module: &str) -> bool {
    layer_of(module).is_some_and(|rank| LAYERS[rank].0 == TEST_SUPPORT)
}

/// Check that the table and the module list name the same modules, each once.
fn table_problems(modules: &BTreeSet<String>) -> Vec<String> {
    let mut problems = Vec::new();
    let mut seen = BTreeSet::new();
    for (layer, members) in LAYERS {
        for member in *members {
            if !seen.insert(*member) {
                problems.push(format!("`{member}` appears more than once in LAYERS"));
            } else if !modules.contains(*member) {
                problems.push(format!(
                    "LAYERS lists `{member}` in `{layer}` but src/ has no such module; remove it from the table"
                ));
            }
        }
    }
    for module in modules {
        if !seen.contains(module.as_str()) {
            problems.push(format!(
                "src/{module} is in no layer; add it to LAYERS in tests/layer_boundary.rs"
            ));
        }
    }
    problems
}

/// The violating `(from, to)` pairs among `edges`, each with its sites.
fn violations(
    edges: &BTreeMap<(String, String), Vec<String>>,
) -> BTreeMap<(String, String), Vec<String>> {
    // Same-layer reachability, to find the edges that lie on a cycle.
    let mut next: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (from, to) in edges.keys() {
        if layer_of(from) == layer_of(to) {
            next.entry(from).or_default().push(to);
        }
    }
    let reaches = |start: &str, goal: &str| {
        let mut seen = BTreeSet::new();
        let mut stack = vec![start];
        while let Some(node) = stack.pop() {
            if node == goal {
                return true;
            }
            if seen.insert(node) {
                stack.extend(next.get(node).into_iter().flatten().copied());
            }
        }
        false
    };
    let mut found = BTreeMap::new();
    for ((from, to), sites) in edges {
        let (Some(from_layer), Some(to_layer)) = (layer_of(from), layer_of(to)) else {
            continue;
        };
        let bad = if in_test_support(from) {
            false
        } else if in_test_support(to) {
            true
        } else if from_layer == to_layer {
            reaches(to, from)
        } else {
            to_layer > from_layer
        };
        if bad {
            found.insert((from.clone(), to.clone()), sites.clone());
        }
    }
    found
}

#[test]
fn modules_respect_the_layer_order() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let files = module_files(&src);
    let names: BTreeSet<String> = files.keys().cloned().collect();
    assert!(
        names.contains("runtime"),
        "src/runtime is missing from the scan"
    );
    let problems = table_problems(&names);
    assert!(problems.is_empty(), "{}", problems.join("\n"));

    let mut edges: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for (module, paths) in &files {
        for path in paths {
            let source = std::fs::read_to_string(path).expect("a readable source");
            let top_level = path.parent() == Some(src.as_path())
                || (path.ends_with("mod.rs")
                    && path.parent().and_then(Path::parent) == Some(src.as_path()));
            let file = path.strip_prefix(&src).unwrap_or(path).display();
            for (target, line) in module_references(&non_test(&source), top_level, &names) {
                if &target != module {
                    edges
                        .entry((module.clone(), target))
                        .or_default()
                        .push(format!("src/{file}:{line}"));
                }
            }
        }
    }
    let found = violations(&edges);
    let mut report = Vec::new();
    for (pair, sites) in &found {
        let mut sites = sites.clone();
        sites.dedup();
        report.push(format!(
            "violation `{}` -> `{}` at {}",
            pair.0,
            pair.1,
            sites.join(", ")
        ));
    }
    if !report.is_empty() {
        report.push(
            "move the type or contract down to the lower layer, or pass it in from the higher \
             layer"
                .to_string(),
        );
    }
    assert!(report.is_empty(), "layer order:\n{}", report.join("\n"));
}

fn names(list: &[&str]) -> BTreeSet<String> {
    list.iter().map(|name| name.to_string()).collect()
}

#[test]
fn the_scan_finds_paths_groups_and_super_imports() {
    let modules = names(&["cli", "tui", "client", "app", "runtime", "tool"]);
    let source = "use crate::cli::run;\n\
                  use crate::{\n    app::Controller,\n    tui::{self, app},\n};\n\
                  fn f() { super::client::connect(); }\n\
                  // crate::client::connect in a comment is ignored\n\
                  use crate::clientele::Thing;\n\
                  use crate::{runtime::{cli_like, builder::Runner}, tool};\n";
    let heads =
        |top_level| -> Vec<(String, usize)> { module_references(source, top_level, &modules) };
    let expected = [
        ("cli", 1),
        ("app", 2),
        ("tui", 2),
        ("client", 6),
        ("runtime", 9),
        ("tool", 9),
    ];
    let expected: Vec<(String, usize)> =
        expected.iter().map(|(m, l)| (m.to_string(), *l)).collect();
    let mut found = heads(true);
    found.sort_by_key(|(_, line)| *line);
    assert_eq!(found, expected);
    // Below the top level, `super::client` is a sibling module of that name.
    let below: Vec<_> = expected
        .into_iter()
        .filter(|(m, _)| m != "client")
        .collect();
    let mut found = heads(false);
    found.sort_by_key(|(_, line)| *line);
    assert_eq!(found, below);
}

#[test]
fn test_only_items_are_not_scanned() {
    let modules = names(&["cli", "tui", "client", "app"]);
    let source = "use crate::app::Controller;\n\
                  #[cfg(test)]\nmod mcp_tests {\n    use crate::cli::flags;\n}\n\
                  #[cfg(test)]\npub(crate) fn helper() {\n    use crate::cli::flags::parse;\n}\n\
                  #[cfg(test)]\nuse crate::tui::app;\n\
                  fn production() { crate::client::connect(); }\n";
    assert_eq!(
        module_references(&non_test(source), true, &modules),
        vec![("app".to_string(), 1), ("client".to_string(), 12)]
    );
}

#[test]
fn a_same_layer_cycle_is_a_violation_and_a_one_way_edge_is_not() {
    let edge = |a: &str, b: &str| ((a.to_string(), b.to_string()), vec!["site".to_string()]);
    let one_way: BTreeMap<_, _> = [edge("tool", "skill")].into_iter().collect();
    assert!(violations(&one_way).is_empty());
    let cycle: BTreeMap<_, _> = [edge("tool", "skill"), edge("skill", "tool")]
        .into_iter()
        .collect();
    assert_eq!(violations(&cycle).len(), 2);
    let upward: BTreeMap<_, _> = [
        edge("hex", "tui"),
        edge("tui", "hex"),
        edge("app", "fixtures"),
    ]
    .into_iter()
    .collect();
    let keys: Vec<_> = violations(&upward).into_keys().collect();
    assert_eq!(
        keys,
        vec![
            ("app".to_string(), "fixtures".to_string()),
            ("hex".to_string(), "tui".to_string())
        ]
    );
}
