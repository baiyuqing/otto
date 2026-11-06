//! Source guard: retraction notices carry no record text, candidate reason or
//! model explanation.
//!
//! A notice is built from ids, revisions and a fixed reason only (#591 §9),
//! because record text, a candidate's stored reason and a model explanation
//! are untrusted text that must not reach the request through a notice. The
//! production code of the two modules that select and render notices must not
//! read those fields. A comment may name them; code may not.
#![cfg(not(target_arch = "wasm32"))]

use std::path::Path;

const GUARDED_FILES: &[&str] = &["src/agent/memory_retraction.rs", "src/agent/memory_step.rs"];

/// Tokens that appear when code reads record text, a candidate's reason or a
/// model explanation. `RetractionReason` and the `reason=` attribute of the
/// notice template are allowed and are not in this list.
const FORBIDDEN_TOKENS: &[&str] = &[
    ".text",
    "text:",
    "state_reason",
    "explanation",
    "Candidate",
    "candidate_",
];

/// Whether `token` occurs in `code` at an identifier boundary, so `text:` does
/// not match the tail of `memory_context:`.
fn contains_token(code: &str, token: &str) -> bool {
    let starts_with_identifier = token.starts_with(|c: char| c.is_alphanumeric() || c == '_');
    code.match_indices(token).any(|(start, _)| {
        !starts_with_identifier
            || !code[..start]
                .chars()
                .next_back()
                .is_some_and(|before| before.is_alphanumeric() || before == '_')
    })
}

/// Removes `#[cfg(test)]` items, as `tests/io_boundary.rs` does.
fn production_code(source: &str) -> String {
    let lines: Vec<&str> = source.lines().collect();
    let mut kept = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        index += 1;
        if line.trim() != "#[cfg(test)]" {
            if !line.trim_start().starts_with("//") {
                kept.push(line);
            }
            continue;
        }
        // Skip the item: a `mod x;` line, or a braced block to its close.
        let mut depth = 0i32;
        let mut opened = false;
        while index < lines.len() {
            let item_line = lines[index];
            index += 1;
            depth += item_line.matches('{').count() as i32 - item_line.matches('}').count() as i32;
            opened |= item_line.contains('{');
            if (opened && depth <= 0) || (!opened && item_line.trim_end().ends_with(';')) {
                break;
            }
        }
    }
    kept.join("\n")
}

fn forbidden_tokens_in(source: &str) -> Vec<&'static str> {
    let code = production_code(source);
    FORBIDDEN_TOKENS
        .iter()
        .copied()
        .filter(|token| contains_token(&code, token))
        .collect()
}

#[test]
fn retraction_notice_code_never_reads_record_text_candidate_reasons_or_explanations() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for relative in GUARDED_FILES {
        let source = std::fs::read_to_string(root.join(relative)).expect("read guarded file");
        assert!(
            production_code(&source).contains("fn "),
            "{relative}: no production code was scanned"
        );
        assert_eq!(
            forbidden_tokens_in(&source),
            Vec::<&str>::new(),
            "{relative} reads a field that can carry untrusted text; a retraction notice may hold \
             ids, revisions and a fixed reason only"
        );
    }
}

#[test]
fn fixtures_prove_text_reads_are_detected() {
    let flagged = [
        "fn f(r: &Record) -> &str { &r.text }",
        "struct Notice { text: String }",
        "fn f() { let r = candidate.state_reason; }",
        "fn f(c: &Candidate) {}",
        "fn f() { let r = candidate_reason; }",
        "fn f(e: &str) { let explanation = e; }",
        "fn f(c: Candidate) {}",
    ];
    for source in flagged {
        assert!(
            !forbidden_tokens_in(source).is_empty(),
            "not flagged: {source}"
        );
    }
    let allowed = [
        "//! no record text, no candidate reason, no model explanation\nfn f() {}",
        "/// the reason= attribute\nfn f(reason: RetractionReason) {}",
        "#[cfg(test)]\nmod tests {\n    fn f(r: &Record) -> &str { &r.text }\n}",
        "#[cfg(test)]\nmod tests;\nfn f() {}",
        "fn f(memory_context: &str, retraction_context: &str) {}",
        "fn f() { let candidate = 1; }",
    ];
    for source in allowed {
        assert_eq!(
            forbidden_tokens_in(source),
            Vec::<&str>::new(),
            "flagged: {source}"
        );
    }
}
