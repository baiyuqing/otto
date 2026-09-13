//! Classifier for provider responses that reject a request as too large for
//! the context window.
//!
//! Port of `internal/provider/openaicompat/overflow.go`. The classifier is
//! deliberately narrow: it accepts an allowlisted error code, or a message
//! matching one of three phrase patterns, and nothing else. A response that
//! only complains about `max_tokens` describes the output budget, not the
//! context window, and is not an overflow.
//!
//! Ownership: the body is borrowed and never retained. The returned
//! [`ContextOverflowError`] carries only a status, an allowlisted code, and
//! two token counts, so no provider text can reach a log through it.
//!
//! Errors: this module reports "not an overflow" as `None`; a malformed body
//! is not an error, it is simply not classified.

use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;
use serde::de::{DeserializeSeed, Deserializer, Error as _, MapAccess, SeqAccess, Visitor};
use serde_json::Value;

use crate::provider::ContextOverflowError;

/// The largest error body the classifier will inspect, 32 KiB. The HTTP layer
/// truncates to the same bound before calling in; that truncation is phase 4.
pub const MAX_ERROR_BODY: usize = 32 << 10;

/// Message phrases that identify a context-window rejection.
static OVERFLOW_MESSAGE_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"\bmaximum context length\b",
        r"\bcontext window\b.{0,64}\b(exceeded|exceeds)\b",
        r"\binput tokens?\b.{0,64}\bexceed(ed|s)?\b.{0,64}\bcontext\b",
    ]
    .into_iter()
    .map(|pattern| Regex::new(pattern).expect("static pattern compiles"))
    .collect()
});

/// `requested N tokens ... maximum M`.
static REQUESTED_THEN_MAXIMUM: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\brequested[ ]+([0-9]+)[ ]+tokens\b.{0,64}\bmaximum[ ]+([0-9]+)\b")
        .expect("static pattern compiles")
});

/// `N tokens ... maximum context length is M`.
static TOKENS_THEN_CONTEXT_MAX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b([0-9]+)[ ]+tokens\b.{0,64}\bmaximum context length is[ ]+([0-9]+)\b")
        .expect("static pattern compiles")
});

/// `maximum context length is M tokens ... requested N tokens`.
static CONTEXT_MAX_THEN_REQUESTED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\bmaximum context length is[ ]+([0-9]+)[ ]+tokens\b.{0,64}\brequested[ ]+([0-9]+)[ ]+tokens\b",
    )
    .expect("static pattern compiles")
});

/// Error codes and types that are accepted as structured evidence, compared
/// case-folded.
const OVERFLOW_EVIDENCE: [&str; 3] = [
    "context_length_exceeded",
    "context_window_exceeded",
    "max_context_length",
];

/// Classifies an error response as a context-window rejection, or returns
/// `None`.
///
/// Only HTTP 400, 413, and 422 are considered. A body over [`MAX_ERROR_BODY`],
/// a body that is not a JSON object, and a body containing a duplicate JSON
/// key are all rejected, the last because a duplicate key lets a response
/// carry two different readings of the same field.
pub fn classify_overflow(status: u16, body: &[u8]) -> Option<ContextOverflowError> {
    if !matches!(status, 400 | 413 | 422) {
        return None;
    }
    if body.len() > MAX_ERROR_BODY || !has_unique_json_keys(body) {
        return None;
    }

    let root = serde_json::from_slice::<Value>(body).ok()?;
    let root = root.as_object()?;
    let mut objects = Vec::with_capacity(2);
    if let Some(nested) = root.get("error").and_then(Value::as_object) {
        objects.push(nested);
    }
    objects.push(root);

    let mut code = "";
    let mut output_token_param = false;
    let mut messages: Vec<&str> = Vec::with_capacity(objects.len());
    for object in objects {
        if code.is_empty() {
            code = recognized_overflow_value(object.get("code"));
        }
        if code.is_empty() {
            code = recognized_overflow_value(object.get("type"));
        }
        if object.get("param").and_then(Value::as_str) == Some("max_tokens") {
            output_token_param = true;
        }
        if let Some(message) = object.get("message").and_then(Value::as_str) {
            messages.push(message);
        }
    }

    let message_match = messages.iter().any(|message| is_overflow_message(message));
    if code.is_empty() && (output_token_param || !message_match) {
        return None;
    }

    let mut overflow = ContextOverflowError {
        status,
        code: code.to_owned(),
        ..ContextOverflowError::default()
    };
    for message in messages {
        let (current, maximum) = extract_token_counts(message);
        if current > 0 && maximum > 0 {
            overflow.current_tokens = current;
            overflow.maximum_tokens = maximum;
            break;
        }
    }
    Some(overflow)
}

/// Reports whether every JSON object in `body` has distinct keys and the body
/// holds exactly one JSON value.
///
/// `serde_json` keeps the last of a repeated key instead of rejecting it, so
/// this walks the document with its own visitor. Escaped keys are compared
/// after unescaping, which is why `"code"` collides with `"code"`.
pub fn has_unique_json_keys(body: &[u8]) -> bool {
    let mut deserializer = serde_json::Deserializer::from_slice(body);
    if UniqueKeys.deserialize(&mut deserializer).is_err() {
        return false;
    }
    deserializer.end().is_ok()
}

/// Returns the allowlisted spelling of a code or type value, or `""`.
fn recognized_overflow_value(raw: Option<&Value>) -> &'static str {
    let value = match raw.and_then(Value::as_str) {
        Some(value) => value.to_lowercase(),
        None => return "",
    };
    OVERFLOW_EVIDENCE
        .into_iter()
        .find(|candidate| *candidate == value)
        .unwrap_or("")
}

/// Reports whether a message describes a context-window rejection. A message
/// mentioning `max_tokens` is about the output budget and is never accepted.
fn is_overflow_message(message: &str) -> bool {
    let normalized = normalize_overflow_message(message);
    if normalized.contains("max_tokens") {
        return false;
    }
    OVERFLOW_MESSAGE_PATTERNS
        .iter()
        .any(|pattern| pattern.is_match(&normalized))
}

/// Extracts the `(current, maximum)` token pair from a message, or `(0, 0)`
/// when no pattern matches or a count does not fit in an `i64`.
fn extract_token_counts(message: &str) -> (i64, i64) {
    let normalized = normalize_overflow_message(message);
    if let Some(captures) = REQUESTED_THEN_MAXIMUM.captures(&normalized) {
        return token_pair(&captures[1], &captures[2]);
    }
    if let Some(captures) = TOKENS_THEN_CONTEXT_MAX.captures(&normalized) {
        return token_pair(&captures[1], &captures[2]);
    }
    if let Some(captures) = CONTEXT_MAX_THEN_REQUESTED.captures(&normalized) {
        let (maximum, current) = token_pair(&captures[1], &captures[2]);
        return (current, maximum);
    }
    (0, 0)
}

fn token_pair(current_text: &str, maximum_text: &str) -> (i64, i64) {
    match (current_text.parse::<i64>(), maximum_text.parse::<i64>()) {
        (Ok(current), Ok(maximum)) if current > 0 && maximum > 0 => (current, maximum),
        _ => (0, 0),
    }
}

/// Folds line breaks to spaces and lowercases, so the phrase patterns can use
/// `.{0,64}` without crossing a line break by accident.
fn normalize_overflow_message(message: &str) -> String {
    message.replace(['\r', '\n'], " ").to_lowercase()
}

/// Walks a JSON document rejecting any object with a repeated key.
struct UniqueKeys;

impl<'de> DeserializeSeed<'de> for UniqueKeys {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for UniqueKeys {
    type Value = ();

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("any JSON value")
    }

    fn visit_bool<E>(self, _value: bool) -> Result<(), E> {
        Ok(())
    }

    fn visit_i64<E>(self, _value: i64) -> Result<(), E> {
        Ok(())
    }

    fn visit_u64<E>(self, _value: u64) -> Result<(), E> {
        Ok(())
    }

    fn visit_f64<E>(self, _value: f64) -> Result<(), E> {
        Ok(())
    }

    fn visit_str<E>(self, _value: &str) -> Result<(), E> {
        Ok(())
    }

    fn visit_unit<E>(self) -> Result<(), E> {
        Ok(())
    }

    fn visit_none<E>(self) -> Result<(), E> {
        Ok(())
    }

    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        UniqueKeys.deserialize(deserializer)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        while seq.next_element_seed(UniqueKeys)?.is_some() {}
        Ok(())
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        let mut seen: HashSet<String> = HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !seen.insert(key) {
                return Err(A::Error::custom("duplicate JSON key"));
            }
            map.next_value_seed(UniqueKeys)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn accepts_allowlisted_structured_evidence() {
        let cases: &[(&str, &str, &str)] = &[
            (
                "nested code",
                r#"{"error":{"code":"context_length_exceeded"}}"#,
                "context_length_exceeded",
            ),
            (
                "root code",
                r#"{"code":"context_window_exceeded"}"#,
                "context_window_exceeded",
            ),
            (
                "nested type",
                r#"{"error":{"type":"max_context_length"}}"#,
                "max_context_length",
            ),
            (
                "root type case folded",
                r#"{"type":"CONTEXT_LENGTH_EXCEEDED"}"#,
                "context_length_exceeded",
            ),
            (
                "allowlisted code with output param",
                r#"{"error":{"code":"context_length_exceeded","param":"max_tokens"}}"#,
                "context_length_exceeded",
            ),
        ];
        for (name, body, code) in cases {
            let overflow = classify_overflow(400, body.as_bytes())
                .unwrap_or_else(|| panic!("{name}: expected classification"));
            assert_eq!(overflow.status, 400, "{name}");
            assert_eq!(overflow.code, *code, "{name}");
            assert!(
                overflow
                    .to_string()
                    .starts_with(ContextOverflowError::MESSAGE),
                "{name}"
            );
        }
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn accepts_only_narrow_messages() {
        let wide = "x".repeat(65);
        let gap_body = format!(r#"{{"message":"context window {wide} exceeded"}}"#);
        let cases: &[(&str, &str, bool)] = &[
            (
                "maximum context length",
                r#"{"error":{"message":"Maximum Context Length is 128000 tokens"}}"#,
                true,
            ),
            (
                "context window exceeded with newline",
                r#"{"message":"the CONTEXT WINDOW\nwas exceeded"}"#,
                true,
            ),
            (
                "input tokens exceed context",
                r#"{"error":{"message":"Input tokens in this request exceed the available context size"}}"#,
                true,
            ),
            (
                "output max tokens",
                r#"{"error":{"code":"max_tokens","message":"max_tokens exceeds the maximum context length; requested 5000 output tokens"}}"#,
                false,
            ),
            (
                "nested output max tokens param",
                r#"{"error":{"param":"max_tokens","message":"maximum context length is 128000 tokens"}}"#,
                false,
            ),
            (
                "root output max tokens param",
                r#"{"param":"max_tokens","message":"maximum context length is 128000 tokens"}"#,
                false,
            ),
            (
                "non-string output param",
                r#"{"error":{"param":123,"message":"maximum context length is 128000 tokens"}}"#,
                true,
            ),
            (
                "generic context",
                r#"{"error":{"message":"context is too large"}}"#,
                false,
            ),
            (
                "window without exceeded",
                r#"{"message":"context window is available"}"#,
                false,
            ),
            (
                "output tokens",
                r#"{"message":"output tokens exceed the context window"}"#,
                false,
            ),
            ("phrase gap too wide", gap_body.as_str(), false),
        ];
        for (name, body, want) in cases {
            let got = classify_overflow(422, body.as_bytes()).is_some();
            assert_eq!(got, *want, "{name}");
        }
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn extracts_only_bounded_token_pairs() {
        let boundary = format!(
            "requested 130000 tokens{}maximum 128000; maximum context length",
            " ".repeat(64)
        );
        let beyond = format!(
            "requested 130000 tokens{}maximum 128000; maximum context length",
            " ".repeat(65)
        );
        let overflowing = format!(
            "requested {} tokens; maximum 128000; maximum context length",
            "9".repeat(100)
        );
        let cases: &[(&str, &str, i64, i64)] = &[
            (
                "maximum then requested provider wording",
                "maximum context length is 128000 tokens; requested 130000 tokens",
                130000,
                128000,
            ),
            (
                "requested then maximum",
                "requested 130000 tokens; maximum 128000",
                130000,
                128000,
            ),
            (
                "token count then context maximum",
                "130000 tokens were supplied; maximum context length is 128000",
                130000,
                128000,
            ),
            (
                "pair at distance boundary",
                boundary.as_str(),
                130000,
                128000,
            ),
            ("pair beyond distance boundary", beyond.as_str(), 0, 0),
            (
                "unrelated numbers",
                "maximum context length: account 7, request 9, limit 11",
                0,
                0,
            ),
            ("integer overflow", overflowing.as_str(), 0, 0),
        ];
        for (name, message, current, maximum) in cases {
            let body = format!(
                r#"{{"error":{{"code":"context_length_exceeded","message":{}}}}}"#,
                serde_json::to_string(message).expect("quotes")
            );
            let overflow = classify_overflow(413, body.as_bytes())
                .unwrap_or_else(|| panic!("{name}: expected classification"));
            assert_eq!(
                (overflow.current_tokens, overflow.maximum_tokens),
                (*current, *maximum),
                "{name}"
            );
        }
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn rejects_other_statuses() {
        let valid = br#"{"error":{"code":"context_length_exceeded"}}"#;
        for status in [200u16, 401, 429, 500] {
            assert!(
                classify_overflow(status, valid).is_none(),
                "status {status}"
            );
        }
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn rejects_malformed_and_duplicate_json() {
        let bodies = [
            r#"{"code":" context_length_exceeded "}"#,
            r#"{"type":"context_length_exceeded_extra"}"#,
            r#"{"error":{"code":123,"message":"ordinary validation failure"}}"#,
            r#"{"error":{"code":"context_length_exceeded"}"#,
            r#"{"error":{"code":"other","code":"context_length_exceeded"}}"#,
            r#"{"error":{"code":"other","\u0063ode":"context_length_exceeded"}}"#,
            r#"{"message":"safe","message":"maximum context length"}"#,
            r#"{"param":"other","param":"max_tokens","message":"maximum context length"}"#,
            r#"{"code":"context_length_exceeded"} {}"#,
        ];
        for body in bodies {
            assert!(classify_overflow(400, body.as_bytes()).is_none(), "{body}");
        }
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn duplicate_key_check_walks_nested_values() {
        let unique = [
            r#"{"a":1,"b":{"a":2},"c":[{"a":3},{"a":4}]}"#,
            r#"[1,2,3]"#,
            "12",
            r#""text""#,
            "null",
        ];
        for body in unique {
            assert!(has_unique_json_keys(body.as_bytes()), "{body}");
        }
        let rejected = [
            r#"{"a":1,"a":2}"#,
            r#"{"b":{"a":1,"a":2}}"#,
            r#"[{"a":1,"a":2}]"#,
            r#"{"a":1} {"b":2}"#,
            r#"{"a":1"#,
            "",
        ];
        for body in rejected {
            assert!(!has_unique_json_keys(body.as_bytes()), "{body}");
        }
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn rejects_a_body_larger_than_the_cap() {
        let padding = "p".repeat(MAX_ERROR_BODY);
        let body = format!(r#"{{"code":"context_length_exceeded","pad":"{padding}"}}"#);
        assert!(classify_overflow(400, body.as_bytes()).is_none());
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn the_error_never_retains_the_provider_body() {
        const SECRET: &str = "body-secret-value";
        let body = format!(r#"{{"error":{{"message":"maximum context length; {SECRET}"}}}}"#);
        let overflow = classify_overflow(400, body.as_bytes()).expect("classification");
        let rendered = overflow.to_string();
        assert!(!rendered.contains(SECRET), "{rendered}");
        assert!(!rendered.contains(body.as_str()), "{rendered}");
    }
}
