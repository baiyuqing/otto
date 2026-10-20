//! Heuristic detection of credential-shaped text and the redaction of detected ranges.

use super::*;

/// Provider-bound request fields that may contain prose are transformed on a
/// clone. Structural and provider-opaque fields are checked but never changed.
const MINIMUM_CREDENTIAL_BYTES: usize = 8;
const MINIMUM_PREFIXED_TOKEN_BYTES: usize = 20;
pub(super) const MAXIMUM_EGRESS_JSON_DEPTH: usize = 128;

pub(super) fn blocked() -> AgentError {
    AgentError::ProviderEgressSecret
}

fn placeholder(value: &str) -> bool {
    let normalized: String = value
        .trim_matches(|character: char| {
            character.is_ascii_whitespace()
                || matches!(
                    character,
                    '"' | '\'' | '`' | '<' | '>' | '[' | ']' | '{' | '}'
                )
        })
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect();
    normalized.is_empty()
        || matches!(
            normalized.as_str(),
            "redacted"
                | "placeholder"
                | "example"
                | "sample"
                | "changeme"
                | "replace"
                | "replaceme"
                | "yourkey"
                | "yourtoken"
                | "yoursecret"
                | "yourpassword"
                | "secret"
                | "password"
                | "token"
                | "apikey"
                | "none"
                | "null"
                | "undefined"
        )
        || normalized.starts_with("example")
        || normalized.starts_with("dummy")
        || normalized.starts_with("testonly")
}

pub(super) fn sensitive_key(key: &str) -> bool {
    let normalized: String = key
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_uppercase)
        .collect();
    matches!(
        normalized.as_str(),
        "TOKEN"
            | "ACCESSTOKEN"
            | "AUTHTOKEN"
            | "BEARERTOKEN"
            | "SECRET"
            | "CLIENTSECRET"
            | "PASSWORD"
            | "PASSWD"
            | "PRIVATEKEY"
            | "SECRETACCESSKEY"
    ) || normalized.ends_with("APIKEY")
        || normalized.ends_with("TOKEN")
        || normalized.ends_with("SECRET")
        || normalized.ends_with("PASSWORD")
        || normalized.ends_with("PRIVATEKEY")
        || normalized.ends_with("SECRETACCESSKEY")
        || normalized.ends_with("ACCESSTOKEN")
}

fn credential_value(value: &str) -> bool {
    let trimmed = value.trim_matches(|character: char| {
        character.is_ascii_whitespace() || matches!(character, '"' | '\'' | '`' | ',' | ';')
    });
    trimmed.len() >= MINIMUM_CREDENTIAL_BYTES && !placeholder(trimmed)
}

pub(super) fn strong_prefixed_token(value: &str) -> bool {
    let token = value.trim_matches(|character: char| {
        !character.is_ascii_alphanumeric() && !matches!(character, '-' | '_')
    });
    if token.len() < MINIMUM_PREFIXED_TOKEN_BYTES || placeholder(token) {
        return false;
    }
    let strong_prefix = token.starts_with("sk-")
        || token.starts_with("ghp_")
        || token.starts_with("gho_")
        || token.starts_with("github_pat_")
        || token.starts_with("xoxb-")
        || token.starts_with("xoxp-")
        || token.starts_with("xoxa-")
        || token.starts_with("AKIA");
    strong_prefix
        && token
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
}

fn delimited_value_range(value: &str) -> Option<std::ops::Range<usize>> {
    let leading = value.len() - value.trim_start().len();
    let value = value.get(leading..)?;
    if let Some(quote) = value
        .chars()
        .next()
        .filter(|character| matches!(character, '"' | '\'' | '`'))
    {
        let start = leading + quote.len_utf8();
        let quoted = value.get(quote.len_utf8()..)?;
        let length = quoted.find(quote).unwrap_or(quoted.len());
        return Some(start..start + length);
    }
    let length = value
        .find(|character: char| character.is_ascii_whitespace() || matches!(character, ',' | ';'))
        .unwrap_or(value.len());
    Some(leading..leading + length)
}

fn assignment_finding(line: &str) -> Option<std::ops::Range<usize>> {
    let separator = line.find(['=', ':'])?;
    let key = line.get(..separator)?.trim_matches(|character: char| {
        character.is_ascii_whitespace()
            || (!character.is_ascii_alphanumeric() && !matches!(character, '_' | '-'))
    });
    if !sensitive_key(key) && !key.eq_ignore_ascii_case("authorization") {
        return None;
    }
    let after_separator = separator + 1;
    let mut value = line.get(after_separator..)?;
    let mut offset = after_separator;
    let whitespace = value.len() - value.trim_start().len();
    value = value.get(whitespace..)?;
    offset += whitespace;
    if key.eq_ignore_ascii_case("authorization") {
        let scheme_length = "bearer".len();
        let (Some(scheme), Some(after_scheme)) =
            (value.get(..scheme_length), value.get(scheme_length..))
        else {
            return None;
        };
        if !scheme.eq_ignore_ascii_case("bearer") {
            return None;
        }
        let separators = after_scheme
            .chars()
            .take_while(|character| matches!(character, ' ' | '\t'))
            .map(char::len_utf8)
            .sum::<usize>();
        if separators == 0 {
            return None;
        }
        offset += scheme_length + separators;
        value = after_scheme.get(separators..)?;
    }
    let range = delimited_value_range(value)?;
    let finding = offset + range.start..offset + range.end;
    credential_value(line.get(finding.clone())?).then_some(finding)
}

pub(super) fn independent_finding_ranges(text: &str) -> Vec<std::ops::Range<usize>> {
    let mut findings = Vec::new();
    for label in ["PRIVATE", "RSA PRIVATE", "EC PRIVATE", "OPENSSH PRIVATE"] {
        // Construct PEM boundaries from fragments so repository secret scanners
        // do not mistake the detector itself for embedded key material.
        let begin = format!("-----{} {label} KEY-----", "BEGIN");
        let end = format!("-----{} {label} KEY-----", "END");
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
        let trimmed_start = token
            .find(|character: char| {
                character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
            })
            .unwrap_or(token.len());
        let trimmed_end = token
            .rfind(|character: char| {
                character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
            })
            .map_or(trimmed_start, |index| index + 1);
        if strong_prefixed_token(token) {
            findings.push(token_start + trimmed_start..token_start + trimmed_end);
        }
        offset = token_start + token.len();
    }
    findings
}

fn heuristic_finding_ranges(text: &str) -> Vec<std::ops::Range<usize>> {
    let mut findings = independent_finding_ranges(text);
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let content = line.strip_suffix('\n').unwrap_or(line);
        let content = content.strip_suffix('\r').unwrap_or(content);
        if let Some(range) = assignment_finding(content) {
            findings.push(offset + range.start..offset + range.end);
        }
        offset += line.len();
    }
    findings
}

pub(super) fn heuristic_secret(text: &str) -> bool {
    !heuristic_finding_ranges(text).is_empty()
}

pub(super) fn redact_ranges(
    redactor: &Redactor,
    text: &str,
    mut ranges: Vec<std::ops::Range<usize>>,
) -> Option<String> {
    let mut marker_inputs = redactor.values.clone();
    marker_inputs.extend(ranges.iter().map(|range| text[range.clone()].to_owned()));
    let marker = safetext::shared_redaction_marker(&marker_inputs)?;
    ranges.sort_by_key(|range| (range.start, std::cmp::Reverse(range.end)));
    let mut redacted = String::with_capacity(text.len());
    let mut cursor = 0;
    for range in ranges {
        if range.start < cursor || range.start >= range.end {
            continue;
        }
        redacted.push_str(&text[cursor..range.start]);
        redacted.push_str(&marker);
        cursor = range.end;
    }
    redacted.push_str(&text[cursor..]);
    Some(redacted)
}

fn json_strings_are_safe(redactor: &Redactor, value: &serde_json::Value, depth: usize) -> bool {
    if depth > MAXIMUM_EGRESS_JSON_DEPTH {
        return false;
    }
    match value {
        serde_json::Value::String(value) => protected_text_is_safe(redactor, value),
        serde_json::Value::Array(values) => values
            .iter()
            .all(|value| json_strings_are_safe(redactor, value, depth + 1)),
        serde_json::Value::Object(values) => values.iter().all(|(key, value)| {
            protected_text_is_safe(redactor, key)
                && !(matches!(value, serde_json::Value::String(value)
                    if sensitive_key(key) && credential_value(value)))
                && json_strings_are_safe(redactor, value, depth + 1)
        }),
        _ => true,
    }
}

pub(super) fn protected_json_is_safe(redactor: &Redactor, raw: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
        return false;
    };
    json_strings_are_safe(redactor, &value, 0)
}

pub(super) fn redact_heuristic_text(redactor: &Redactor, text: &str) -> Option<String> {
    let exact = redactor.redact_string(text);
    let ranges = heuristic_finding_ranges(&exact);
    if ranges.is_empty() {
        return Some(exact);
    }
    redact_ranges(redactor, &exact, ranges)
}

pub(super) fn protected_text_is_safe(redactor: &Redactor, text: &str) -> bool {
    redactor.redact_string(text) == text && !heuristic_secret(text)
}

pub(super) fn redact_json_string_value(
    redactor: &Redactor,
    text: &str,
    sensitive_association: bool,
) -> Option<String> {
    let exact = redactor.redact_string(text);
    let mut ranges = independent_finding_ranges(&exact);
    if sensitive_association && credential_value(&exact) {
        ranges.push(0..exact.len());
    }
    if ranges.is_empty() {
        Some(exact)
    } else {
        redact_ranges(redactor, &exact, ranges)
    }
}

pub(super) fn redact_egress_json(redactor: &Redactor, raw: &str) -> Option<Box<RawValue>> {
    let text = redactor.redact_egress_json_text(raw)?;
    RawValue::from_string(text).ok()
}

pub(super) fn finish_reason_is_safe(
    redactor: &Redactor,
    reason: &crate::model::FinishReason,
) -> bool {
    match reason {
        crate::model::FinishReason::Other(value) => protected_text_is_safe(redactor, value),
        _ => true,
    }
}

pub(super) fn role_is_safe(redactor: &Redactor, role: &crate::model::Role) -> bool {
    match role {
        crate::model::Role::Other(value) => protected_text_is_safe(redactor, value),
        _ => true,
    }
}

pub(super) fn block_type_is_safe(redactor: &Redactor, block_type: &BlockType) -> bool {
    match block_type {
        BlockType::Other(value) => protected_text_is_safe(redactor, value),
        _ => true,
    }
}
