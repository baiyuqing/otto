//! Compares the fast egress scan paths with the previous reference algorithms.

use super::*;
use std::ops::Range;

fn secret_values(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

/// The previous `redact_string`: sequential `str::replace` over the sorted values.
fn reference_redact_string(redactor: &Redactor, text: &str) -> String {
    if text.is_empty() || !redactor.complete {
        return String::new();
    }
    let mut redacted = text.to_owned();
    for value in &redactor.values {
        redacted = redacted.replace(value.as_str(), &redactor.marker);
    }
    redacted
}

fn pem_boundary(kind: &str, label: &str) -> String {
    format!("-----{} {label} KEY-----", kind)
}

/// The previous `independent_finding_ranges`: per-call boundary strings,
/// `str::find` for the boundaries and `split_ascii_whitespace` plus `find`
/// for the token offsets.
fn reference_independent_ranges(text: &str) -> Vec<Range<usize>> {
    let mut findings = Vec::new();
    for label in ["PRIVATE", "RSA PRIVATE", "EC PRIVATE", "OPENSSH PRIVATE"] {
        let begin = pem_boundary("BEGIN", label);
        let end = pem_boundary("END", label);
        let mut offset = 0;
        while let Some(relative_start) = text[offset..].find(&begin) {
            let start = offset + relative_start;
            let after_begin = start + begin.len();
            let finish = text[after_begin..]
                .find(&end)
                .map_or(text.len(), |relative_end| {
                    after_begin + relative_end + end.len()
                });
            findings.push(start..finish);
            offset = finish;
            if offset == text.len() {
                break;
            }
        }
    }
    let mut offset = 0;
    for token in text.split_ascii_whitespace() {
        let relative = text[offset..].find(token).unwrap_or_default();
        let token_start = offset + relative;
        let is_token_character =
            |character: char| character.is_ascii_alphanumeric() || matches!(character, '-' | '_');
        let trimmed_start = token.find(is_token_character).unwrap_or(token.len());
        let trimmed_end = token
            .rfind(is_token_character)
            .map_or(trimmed_start, |index| index + 1);
        if strong_prefixed_token(token) {
            findings.push(token_start + trimmed_start..token_start + trimmed_end);
        }
        offset = token_start + token.len();
    }
    findings
}

fn strong_token(length: usize) -> String {
    format!("sk-{}", "a".repeat(length))
}

fn pem_block(label: &str, body: &str) -> String {
    format!(
        "{}\n{body}\n{}",
        pem_boundary("BEGIN", label),
        pem_boundary("END", label)
    )
}

fn scan_matrix() -> Vec<String> {
    let token = strong_token(32);
    let longer_token = format!("x{token}x");
    let assignment = format!("api_{}: {}", "token", "q".repeat(24));
    vec![
        String::new(),
        "ab".to_owned(),
        "alpha-secret".to_owned(),
        "alpha-secret in the middle".to_owned(),
        "start alpha-secret".to_owned(),
        "alpha-secret start".to_owned(),
        "alpha-secretalpha-secret".to_owned(),
        "alpha-secret alpha-secret".to_owned(),
        "contains beta and beta-secret-long and alpha-secret".to_owned(),
        "betaα日本-密钥 next to 日本語 密钥 text".to_owned(),
        "日本-密钥日本-密钥 and 日本-密钥".to_owned(),
        "a b\tc\r\nd\x0Ce  f".to_owned(),
        format!("{token}\t{token}\r\n{token}\x0C{token}"),
        format!("{longer_token} {token} {token}"),
        format!("{token} {longer_token}"),
        format!("  {token}  "),
        format!("\"{token}\", (alpha-secret)"),
        pem_block("PRIVATE", "AAAA"),
        format!("lead {}", pem_block("RSA PRIVATE", "AAAA")),
        format!(
            "lead {}\n{}",
            pem_block("EC PRIVATE", "AAAA"),
            pem_block("EC PRIVATE", "BBBB")
        ),
        format!(
            "{}\n{}",
            pem_block("OPENSSH PRIVATE", "AAAA"),
            pem_block("PRIVATE", "BBBB")
        ),
        format!(
            "text {}\nbody without end",
            pem_boundary("BEGIN", "PRIVATE")
        ),
        pem_boundary("BEGIN", "RSA PRIVATE"),
        pem_boundary("END", "RSA PRIVATE"),
        "-----BEGIN".to_owned(),
        format!("{assignment}\nother line\r\n{assignment}"),
        format!("{assignment}\r\n{}", pem_block("PRIVATE", "AAAA")),
        "key = value\nname: short".to_owned(),
    ]
}

fn redactors() -> Vec<Redactor> {
    let overlapping = secret_values(&["alpha-secret", "beta", "beta-secret-long", "日本-密钥"]);
    vec![
        Redactor::new(&overlapping),
        Redactor::new(&secret_values(&["alpha-secret"])),
        Redactor::new(&[]),
        Redactor::with_completeness(&overlapping, false),
    ]
}

#[test]
fn redact_string_matches_sequential_replace() {
    for redactor in redactors() {
        for text in scan_matrix() {
            assert_eq!(
                redactor.redact_string(&text),
                reference_redact_string(&redactor, &text),
                "text: {text:?}"
            );
        }
    }
}

#[test]
fn unchanged_by_redaction_matches_redact_string_equality() {
    for redactor in redactors() {
        for text in scan_matrix() {
            assert_eq!(
                redactor.unchanged_by_redaction(&text),
                reference_redact_string(&redactor, &text) == text,
                "text: {text:?}"
            );
        }
    }
}

#[test]
fn contains_secret_requires_complete_redactor_and_long_enough_text() {
    let redactor = Redactor::new(&secret_values(&["alpha-secret"]));
    assert!(!redactor.contains_secret(""));
    assert!(!redactor.contains_secret("alpha-secre"));
    assert!(redactor.contains_secret("alpha-secret"));
    assert!(redactor.contains_secret("xx alpha-secret yy"));
    let incomplete = Redactor::with_completeness(&secret_values(&["alpha-secret"]), false);
    assert!(!incomplete.contains_secret("alpha-secret"));
    assert!(!incomplete.unchanged_by_redaction("alpha-secret"));
    assert!(incomplete.unchanged_by_redaction(""));
    assert!(!Redactor::new(&[]).contains_secret("alpha-secret"));
}

#[test]
fn independent_finding_ranges_match_reference_offsets() {
    for text in scan_matrix() {
        assert_eq!(
            independent_finding_ranges(&text),
            reference_independent_ranges(&text),
            "text: {text:?}"
        );
    }
}

#[test]
fn repeated_prefixed_token_gets_one_range_per_occurrence() {
    let token = strong_token(32);
    let text = format!("x{token}x {token} {token}");
    let ranges = independent_finding_ranges(&text);
    let first_standalone = text.find(&format!(" {token} ")).unwrap() + 1;
    let second_standalone = text.rfind(&token).unwrap();
    assert_eq!(
        ranges,
        vec![
            first_standalone..first_standalone + token.len(),
            second_standalone..second_standalone + token.len(),
        ]
    );
}

#[test]
fn unterminated_pem_block_runs_to_end_of_text() {
    let begin = pem_boundary("BEGIN", "PRIVATE");
    let text = format!("lead {begin}\nbody");
    let start = text.find(&begin).unwrap();
    assert_eq!(independent_finding_ranges(&text), vec![start..text.len()]);
}

#[test]
fn heuristic_secret_equals_non_empty_ranges() {
    for text in scan_matrix() {
        assert_eq!(
            heuristic_secret(&text),
            !heuristic_finding_ranges(&text).is_empty(),
            "text: {text:?}"
        );
    }
}

#[test]
fn heuristic_secret_detects_each_detector_alone() {
    assert!(heuristic_secret(&pem_block("PRIVATE", "AAAA")));
    assert!(heuristic_secret(&format!("note {}", strong_token(32))));
    assert!(heuristic_secret(&format!(
        "api_{}: {}",
        "token",
        "q".repeat(24)
    )));
    assert!(!heuristic_secret("plain prose with no credential"));
    assert!(!heuristic_secret(""));
}

/// `secret` is a suffix of `alpha-secret`. A value that is a prefix of another
/// value, or two values that partly overlap, produce chunking-dependent stream
/// output before and after this change, so they are not part of this check.
#[test]
fn stream_redactor_matches_whole_text_redaction() {
    let redactor = Redactor::new(&secret_values(&["alpha-secret", "secret", "日本-密钥"]));
    let texts = [
        "pre alpha-secret-tail post secret and alpha-secret end",
        "alpha-secretalpha-secretsecret",
        "日本-密钥日本-密钥 tail 日本",
        "no secret here",
    ];
    for text in texts {
        let expected = redactor.redact_string(text);
        let boundaries: Vec<usize> = (0..=text.len())
            .filter(|index| text.is_char_boundary(*index))
            .collect();
        for &split in &boundaries {
            for &second in boundaries.iter().filter(|index| **index >= split) {
                let mut stream = redactor.new_stream();
                let mut output = stream.write(&text[..split]);
                output.push_str(&stream.write(&text[split..second]));
                output.push_str(&stream.write(&text[second..]));
                output.push_str(&stream.flush());
                assert_eq!(output, expected, "split {split}/{second} of {text:?}");
            }
        }
    }
}
