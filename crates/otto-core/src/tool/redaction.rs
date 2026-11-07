//! The reserved-placeholder input contract for operational tools.
use crate::safetext::contains_redaction_marker;
use regex::Regex;
use std::sync::LazyLock;

static PERSONAL_PLACEHOLDERS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"<(?:EMAIL|PHONE|PAYMENT_CARD|ID_DOCUMENT)_[0-9]+>")
        .expect("personal-data placeholder pattern")
});

pub const ARGUMENT_POLICY: &str =
    "Arguments must not contain redaction placeholders; use non-sensitive source context instead.";
pub const ARGUMENT_ERROR: &str = "redacted_input: a redaction placeholder represents withheld content, \
    not literal source text. No tool was run. Use surrounding non-sensitive context; \
    do not reconstruct hidden content with encoding or character codes.";

pub fn guards_arguments(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "grep" | "find" | "read" | "ls" | "edit" | "write" | "bash"
    )
}

pub fn argument_error(tool_name: &str, arguments: &str) -> Option<&'static str> {
    if !guards_arguments(tool_name) {
        return None;
    }
    // Decode before inspection; escaped markers must not bypass the guard.
    // If decoding cannot inspect a bounded argument, reject it rather than
    // assuming that it contains no placeholder. Never rewrite the live value.
    let Ok(value) = serde_json::from_str::<serde_json::Value>(arguments) else {
        return Some(
            "invalid_arguments: operational arguments could not be inspected for redaction placeholders; use valid bounded JSON.",
        );
    };
    contains_placeholder(&value.to_string()).then_some(ARGUMENT_ERROR)
}

/// Includes JSON-escaped placeholders without mutating live arguments.
pub fn contains_placeholder(text: &str) -> bool {
    contains_redaction_marker(text)
        || PERSONAL_PLACEHOLDERS.is_match(text)
        || serde_json::from_str::<serde_json::Value>(text)
            .is_ok_and(|value| PERSONAL_PLACEHOLDERS.is_match(&value.to_string()))
}

pub fn placeholder_only(text: &str) -> bool {
    !text.trim().is_empty()
        && PERSONAL_PLACEHOLDERS
            .replace_all(text, "")
            .chars()
            .all(|character| {
                character.is_whitespace() || (0xE000..0xE040).contains(&(character as u32))
            })
}
