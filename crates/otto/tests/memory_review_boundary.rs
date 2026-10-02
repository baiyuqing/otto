//! Source guard: no model-facing tool may decide a memory candidate.
//!
//! Tools (`src/tool/`) run on the model's behalf, so they may only queue
//! candidates (`Service::propose`). Review is a human act, reached through the
//! REPL/TUI commands and the ACP `_otto/memory/*` requests, which the model
//! cannot send. See `docs/specs/2026-10-02-connect-memory-review.md`. Fix a
//! failure by moving the review call to a human-driven entry point.

use std::path::{Path, PathBuf};

const FORBIDDEN: &[&str] = &[".review(", "ReviewRequest", "ReviewDecision"];

fn sources(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).expect("readable directory") {
        let path = entry.expect("readable entry").path();
        if path.is_dir() {
            found.extend(sources(&path));
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            found.push(path);
        }
    }
    found
}

#[test]
fn model_facing_tools_never_decide_a_memory_candidate() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/tool");
    let mut violations = Vec::new();
    for path in sources(&root) {
        let text = std::fs::read_to_string(&path).expect("readable source");
        let production = text.split("#[cfg(test)]").next().unwrap_or(&text);
        for token in FORBIDDEN {
            if production.contains(token) {
                violations.push(format!("{}: contains {token}", path.display()));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "a tool decides memory candidates; only a human may review: {violations:#?}"
    );
}
