//! Content guards ported from Go `internal/memory/guard.go`.
//!
//! Nothing the guards reject is ever written to the store. `DefaultGuard`
//! recognises credential shapes; `ExactGuard` recognises the literal secret
//! values the process already holds (API keys, tokens) without keeping them in
//! memory as plaintext. `CompositeGuard` runs both and collapses the error to
//! a sentinel so no rejected span leaks through the message.

use std::sync::LazyLock;

use regex::Regex;
use sha2::{Digest, Sha256};

use super::contracts::{
    Candidate, Error, ErrorKind, GuardField, GuardInput, MAX_EXACT_GUARD_SPANS,
    MAX_EXACT_GUARD_VALUE_BYTES, MAX_EXACT_GUARD_VALUES, MAX_GUARD_BYTES, MAX_GUARD_FIELDS, Record,
    Result, invalid_request, sensitive,
};

static GUARD_URI_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(?:[a-z][a-z0-9+.-]*:)?//[^\s<>"']+"#).expect("guard URI pattern")
});
static CREDENTIAL_ASSIGNMENT_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(?:^|[\s,;{(])(?:api[_-]?key|access[_-]?token|auth[_-]?token|client[_-]?secret|secret|password|passwd)\s*[:=]\s*[^\s,;}]+",
    )
    .expect("credential assignment pattern")
});
static PRIVATE_KEY_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)-----\s*(?:BEGIN|END)\s+(?:[A-Z0-9]+\s+)*PRIVATE KEY\s*-----")
        .expect("private key pattern")
});

/// A guard inspects request fields and refuses the ones that look like
/// secrets. Implementations must be cheap and allocation-light: they run on
/// every write.
pub trait ContentGuard: Send + Sync {
    fn check(&self, input: &GuardInput) -> Result<()>;
}

/// Shape-based guard: redaction markers, URI userinfo, private key
/// delimiters, credential headers and `key = value` credential assignments.
pub struct DefaultGuard;

impl ContentGuard for DefaultGuard {
    fn check(&self, input: &GuardInput) -> Result<()> {
        validate_guard_input(input)?;
        for field in &input.fields {
            let value = field.value.as_str();
            if value.contains("[REDACTED]") {
                return Err(sensitive("redaction marker"));
            }
            if has_uri_userinfo(value) {
                return Err(sensitive("URI userinfo"));
            }
            if field.opaque {
                continue;
            }
            if PRIVATE_KEY_PATTERN.is_match(value) {
                return Err(sensitive("private key delimiter"));
            }
            if has_sensitive_header(value) {
                return Err(sensitive("credential header"));
            }
            if CREDENTIAL_ASSIGNMENT_PATTERN.is_match(value) {
                return Err(sensitive("credential assignment"));
            }
        }
        Ok(())
    }
}

fn validate_guard_input(input: &GuardInput) -> Result<()> {
    if input.fields.len() > MAX_GUARD_FIELDS {
        return Err(sensitive("field limit"));
    }
    let mut total = 0usize;
    for field in &input.fields {
        if field.value.len() > MAX_GUARD_BYTES.saturating_sub(total) {
            return Err(sensitive("byte limit"));
        }
        total += field.value.len();
    }
    Ok(())
}

fn has_sensitive_header(value: &str) -> bool {
    value.split('\n').any(|line| {
        let line = line.strip_suffix('\r').unwrap_or(line).trim();
        match line.split_once(':') {
            Some((name, rest)) if !rest.trim().is_empty() => matches!(
                name.trim().to_ascii_lowercase().as_str(),
                "authorization" | "proxy-authorization" | "cookie" | "set-cookie"
            ),
            _ => false,
        }
    })
}

fn hierarchical_uri_spans(value: &str) -> Vec<&str> {
    GUARD_URI_PATTERN
        .find_iter(value)
        .map(|found| trim_uri_trailing_punctuation(found.as_str()))
        .filter(|span| !span.is_empty())
        .collect()
}

fn trim_uri_trailing_punctuation(candidate: &str) -> &str {
    let open = candidate.bytes().filter(|byte| *byte == b'[').count();
    let mut close = candidate.bytes().filter(|byte| *byte == b']').count();
    let mut candidate = candidate;
    while let Some(last) = candidate.as_bytes().last() {
        match last {
            b'.' | b',' | b';' | b'!' | b'?' | b')' | b'}' | b'>' => {
                candidate = &candidate[..candidate.len() - 1];
            }
            b']' => {
                if close <= open {
                    return candidate;
                }
                close -= 1;
                candidate = &candidate[..candidate.len() - 1];
            }
            _ => return candidate,
        }
    }
    candidate
}

/// True when the authority component of a hierarchical URI carries userinfo.
/// Go reaches the same answer through `url.Parse`; the authority runs from
/// after `//` to the first `/`, `?` or `#`, and userinfo is whatever precedes
/// an `@` inside it.
fn has_uri_userinfo(value: &str) -> bool {
    hierarchical_uri_spans(value).iter().any(|span| {
        let Some(position) = span.find("//") else {
            return false;
        };
        let rest = &span[position + 2..];
        let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        rest[..authority_end].contains('@')
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct ExactFingerprint {
    length: usize,
    digest: [u8; 32],
}

/// Rejects any field, whitespace token, `key: value` tail or URI span that
/// equals a configured secret. Only length-plus-digest fingerprints are kept,
/// so the guard never holds the plaintext itself.
pub struct ExactGuard {
    fingerprints: Vec<ExactFingerprint>,
}

impl ExactGuard {
    pub fn new(values: &[String]) -> Result<Self> {
        if values.len() > MAX_EXACT_GUARD_VALUES {
            return Err(invalid_request(&format!(
                "exact guard value count exceeds {MAX_EXACT_GUARD_VALUES}"
            )));
        }
        let mut fingerprints: Vec<ExactFingerprint> = Vec::with_capacity(values.len());
        for value in values {
            if value.is_empty() || value.len() > MAX_EXACT_GUARD_VALUE_BYTES {
                return Err(invalid_request(&format!(
                    "exact guard value exceeds {MAX_EXACT_GUARD_VALUE_BYTES}"
                )));
            }
            let fingerprint = ExactFingerprint {
                length: value.len(),
                digest: Sha256::digest(value.as_bytes()).into(),
            };
            if !fingerprints.contains(&fingerprint) {
                fingerprints.push(fingerprint);
            }
        }
        Ok(Self { fingerprints })
    }

    fn matches(&self, value: &str) -> bool {
        let digest: [u8; 32] = Sha256::digest(value.as_bytes()).into();
        let mut found = false;
        for fingerprint in &self.fingerprints {
            let mut difference = 0u8;
            for (left, right) in digest.iter().zip(fingerprint.digest.iter()) {
                difference |= left ^ right;
            }
            found |= difference == 0 && value.len() == fingerprint.length;
        }
        found
    }
}

impl ContentGuard for ExactGuard {
    fn check(&self, input: &GuardInput) -> Result<()> {
        validate_guard_input(input)?;
        let mut spans = 0usize;
        let mut check = |value: &str| -> Result<bool> {
            spans += 1;
            if spans > MAX_EXACT_GUARD_SPANS {
                return Err(sensitive("exact span limit"));
            }
            Ok(self.matches(value))
        };
        for field in &input.fields {
            if check(&field.value)? {
                return Err(sensitive("configured value"));
            }
            for token in field.value.split_whitespace() {
                if check(token)? {
                    return Err(sensitive("configured value"));
                }
                let trimmed = token.trim_matches(|character| {
                    matches!(
                        character,
                        '"' | '\''
                            | '('
                            | ')'
                            | '['
                            | ']'
                            | '{'
                            | '}'
                            | '<'
                            | '>'
                            | ','
                            | ';'
                            | '!'
                            | '?'
                            | '.'
                    )
                });
                if trimmed.is_empty() || trimmed == token {
                    continue;
                }
                if check(trimmed)? {
                    return Err(sensitive("configured value"));
                }
            }
            for line in field.value.split('\n') {
                if let Some((_, rest)) = line.split_once(':') {
                    let span = rest.trim();
                    if !span.is_empty() && check(span)? {
                        return Err(sensitive("configured value"));
                    }
                }
            }
            for span in hierarchical_uri_spans(&field.value) {
                if check(span)? {
                    return Err(sensitive("configured value"));
                }
            }
        }
        Ok(())
    }
}

/// Runs each member in order and collapses any failure to a sentinel, so a
/// guard implementation cannot leak the rejected text through its message.
pub struct CompositeGuard {
    members: Vec<Box<dyn ContentGuard>>,
}

impl CompositeGuard {
    pub fn new(members: Vec<Box<dyn ContentGuard>>) -> Self {
        Self { members }
    }
}

impl ContentGuard for CompositeGuard {
    fn check(&self, input: &GuardInput) -> Result<()> {
        if self.members.is_empty() {
            return Err(Error::new(ErrorKind::Unavailable));
        }
        validate_guard_input(input)?;
        for member in &self.members {
            if let Err(error) = member.check(input) {
                return Err(match error.kind {
                    ErrorKind::SensitiveMemory => Error::new(ErrorKind::SensitiveMemory),
                    _ => Error::new(ErrorKind::Unavailable),
                });
            }
        }
        Ok(())
    }
}

fn field(name: &str, value: &str, opaque: bool) -> GuardField {
    GuardField {
        name: name.to_string(),
        value: value.to_string(),
        opaque,
    }
}

fn record_fields(fields: &mut Vec<GuardField>, record: &Record, prefix: &str) {
    fields.push(field(&format!("{prefix}ID"), &record.id, true));
    fields.push(field(
        &format!("{prefix}scope namespace"),
        &record.scope.namespace,
        false,
    ));
    fields.push(field(&format!("{prefix}scope ID"), &record.scope.id, true));
    fields.push(field(&format!("{prefix}kind"), &record.kind, false));
    fields.push(field(&format!("{prefix}key"), &record.key, false));
    fields.push(field(&format!("{prefix}text"), &record.text, false));
    for label in &record.labels {
        fields.push(field(&format!("{prefix}label"), label, false));
    }
    for (key, value) in &record.metadata {
        fields.push(field(&format!("{prefix}metadata key"), key, false));
        fields.push(field(&format!("{prefix}metadata value"), value, false));
    }
    let origin = record
        .source
        .origin
        .map(|origin| origin.as_str())
        .unwrap_or_default();
    fields.push(field(&format!("{prefix}source origin"), origin, false));
    fields.push(field(
        &format!("{prefix}source session ID"),
        &record.source.session_id,
        true,
    ));
    for id in &record.source.message_ids {
        fields.push(field(&format!("{prefix}source message ID"), id, true));
    }
    fields.push(field(
        &format!("{prefix}source observation ID"),
        &record.source.observation_id,
        true,
    ));
    let decision = record
        .source
        .decision_source
        .map(|origin| origin.as_str())
        .unwrap_or_default();
    fields.push(field(
        &format!("{prefix}source decision source"),
        decision,
        false,
    ));
}

fn candidate_fields(fields: &mut Vec<GuardField>, candidate: &Candidate) {
    fields.push(field("candidate ID", &candidate.id, true));
    record_fields(fields, &candidate.proposed, "candidate proposed ");
    fields.push(field("candidate action", candidate.action.as_str(), false));
    fields.push(field("candidate target ID", &candidate.target_id, true));
    fields.push(field("candidate reason", &candidate.reason, false));
    fields.push(field("candidate state", candidate.state.as_str(), false));
    let decision = candidate
        .decision_source
        .map(|origin| origin.as_str())
        .unwrap_or_default();
    fields.push(field("candidate decision source", decision, false));
    fields.push(field(
        "candidate result record ID",
        &candidate.result_record_id,
        true,
    ));
}

pub fn guard_record(guard: &dyn ContentGuard, record: &Record) -> Result<()> {
    let mut fields = Vec::new();
    record_fields(&mut fields, record, "record ");
    run_guard(guard, fields)
}

pub fn guard_candidate(guard: &dyn ContentGuard, candidate: &Candidate) -> Result<()> {
    let mut fields = Vec::new();
    candidate_fields(&mut fields, candidate);
    run_guard(guard, fields)
}

fn run_guard(guard: &dyn ContentGuard, fields: Vec<GuardField>) -> Result<()> {
    let input = GuardInput { fields };
    validate_guard_input(&input)?;
    guard.check(&input)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(value: &str, opaque: bool) -> GuardInput {
        GuardInput {
            fields: vec![field("test", value, opaque)],
        }
    }

    #[test]
    fn default_guard_rejects_credential_shapes() {
        let guard = DefaultGuard;
        for (value, category) in [
            ("value [REDACTED] here", "redaction marker"),
            ("see https://user:pw@example.com/x", "URI userinfo"),
            ("-----BEGIN RSA PRIVATE KEY-----", "private key delimiter"),
            ("Authorization: Bearer abc", "credential header"),
            ("api_key = abc123", "credential assignment"),
        ] {
            let error = guard.check(&input(value, false)).expect_err(value);
            assert_eq!(error.kind, ErrorKind::SensitiveMemory);
            assert_eq!(error.detail.as_deref(), Some(category));
        }
    }

    #[test]
    fn default_guard_skips_shape_rules_for_opaque_fields() {
        let guard = DefaultGuard;
        guard
            .check(&input("api_key = abc123", true))
            .expect("opaque field passes");
        // The marker and userinfo rules still apply to opaque fields.
        assert!(guard.check(&input("[REDACTED]", true)).is_err());
        assert!(guard.check(&input("https://u:p@h/", true)).is_err());
    }

    #[test]
    fn default_guard_accepts_ordinary_text() {
        DefaultGuard
            .check(&input("prefers tabs over spaces", false))
            .expect("plain text");
        DefaultGuard
            .check(&input("see https://example.com/docs", false))
            .expect("plain URI");
    }

    #[test]
    fn exact_guard_matches_whole_fields_tokens_and_spans() {
        let guard = ExactGuard::new(&["sk-secret-value".to_string()]).expect("guard");
        for value in [
            "sk-secret-value",
            "token is sk-secret-value here",
            "token: sk-secret-value",
            "wrapped (sk-secret-value)",
        ] {
            let error = guard.check(&input(value, false)).expect_err(value);
            assert_eq!(error.detail.as_deref(), Some("configured value"));
        }
        guard
            .check(&input("sk-other-value", false))
            .expect("different value");
    }

    #[test]
    fn exact_guard_rejects_oversized_configuration() {
        assert!(ExactGuard::new(&[String::new()]).is_err());
        let values: Vec<String> = (0..MAX_EXACT_GUARD_VALUES + 1)
            .map(|i| i.to_string())
            .collect();
        assert!(ExactGuard::new(&values).is_err());
    }

    #[test]
    fn composite_guard_collapses_member_errors() {
        let composite = CompositeGuard::new(vec![
            Box::new(DefaultGuard),
            Box::new(ExactGuard::new(&["topsecret".to_string()]).expect("guard")),
        ]);
        composite
            .check(&input("ordinary text", false))
            .expect("clean");
        let error = composite
            .check(&input("value topsecret", false))
            .expect_err("secret");
        assert_eq!(error.kind, ErrorKind::SensitiveMemory);
        assert_eq!(error.detail, None);
    }

    #[test]
    fn empty_composite_is_unavailable() {
        let error = CompositeGuard::new(Vec::new())
            .check(&input("x", false))
            .expect_err("no members");
        assert_eq!(error.kind, ErrorKind::Unavailable);
    }

    #[test]
    fn guard_input_limits_are_enforced() {
        let guard = DefaultGuard;
        let many = GuardInput {
            fields: (0..MAX_GUARD_FIELDS + 1)
                .map(|_| field("f", "x", false))
                .collect(),
        };
        assert_eq!(
            guard.check(&many).expect_err("fields").detail.as_deref(),
            Some("field limit")
        );
        let big = GuardInput {
            fields: vec![field("f", &"x".repeat(MAX_GUARD_BYTES + 1), false)],
        };
        assert_eq!(
            guard.check(&big).expect_err("bytes").detail.as_deref(),
            Some("byte limit")
        );
    }

    #[test]
    fn guard_record_walks_every_field() {
        let guard = CompositeGuard::new(vec![Box::new(DefaultGuard)]);
        let mut record = Record {
            text: "plain".into(),
            ..Record::default()
        };
        guard_record(&guard, &record).expect("clean record");
        record
            .metadata
            .insert("note".into(), "password=hunter2".into());
        assert!(guard_record(&guard, &record).is_err());
    }
}
