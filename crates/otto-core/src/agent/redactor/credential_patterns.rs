//! Local credential patterns. Provider prefixes follow Gitleaks' MIT-licensed
//! rules; attribution is in `docs/development/credential-detection.md`.

use std::ops::Range;
use std::sync::LazyLock;

use regex::Regex;

use super::heuristics::{credential_value, sensitive_key};

static PROVIDER_KEYS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"\b(?:A3T[A-Z0-9]|AKIA|ASIA|ABIA|ACCA)[A-Z2-7]{16}\b",
        r"|\bLTAI[a-zA-Z0-9]{20}\b",
        r"|\b[a-zA-Z0-9_~.]{3}\dQ~[a-zA-Z0-9_~.-]{31,34}\b"
    ))
    .expect("credential prefix patterns")
});

static ASSIGNMENTS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)\b([a-z_][a-z0-9_.-]{0,80})["']?\s*[:=]\s*"#)
        .expect("credential assignment pattern")
});

static BASIC_AUTH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bauthorization\s*:\s*basic\s+([a-z0-9+/]+=*)")
        .expect("basic authorization pattern")
});

pub(super) fn finding_ranges(text: &str) -> Vec<Range<usize>> {
    use base64::Engine;
    let mut findings: Vec<_> = PROVIDER_KEYS
        .find_iter(text)
        .map(|value| value.range())
        .collect();
    for captures in ASSIGNMENTS.captures_iter(text) {
        if !sensitive_key(&captures[1]) {
            continue;
        }
        let start = captures[0].len() + captures.get(0).expect("whole match").start();
        if super::source_values::nonliteral_assignment(text, start) {
            continue;
        }
        let remaining = &text[start..];
        let (offset, length) = match remaining.as_bytes().first() {
            Some(b'\'' | b'"' | b'`') => {
                let quote = remaining.as_bytes()[0] as char;
                (1, remaining[1..].find(quote).unwrap_or(remaining.len() - 1))
            }
            _ => (
                0,
                remaining
                    .find(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | '}' | ']'))
                    .unwrap_or(remaining.len()),
            ),
        };
        let range = start + offset..start + offset + length;
        if !super::source_values::public_api_key_reference(&captures[1], &text[range.clone()])
            && credential_value(&text[range.clone()])
        {
            findings.push(range);
        }
    }
    for captures in BASIC_AUTH.captures_iter(text) {
        let value = captures.get(1).expect("authorization value");
        if base64::engine::general_purpose::STANDARD
            .decode(value.as_str())
            .is_ok_and(|decoded| decoded.contains(&b':'))
        {
            findings.push(value.range());
        }
    }
    findings
}
