//! Narrow source-expression recognition for contextual credential assignments.
//! Quoted values, literals and known secret values are never exempted here.

/// An unquoted, semicolon-terminated call whose function and arguments consist
/// only of identifiers/paths/references contains no literal credential value.
/// Do not apply this to JSON values or arbitrary bare identifiers.
pub(super) fn nonliteral_assignment(text: &str, start: usize) -> bool {
    let line = text[start..].split('\n').next().unwrap_or("").trim();
    let Some(expression) = line.strip_suffix(';') else {
        return false;
    };
    let expression = expression
        .trim_end()
        .strip_suffix('?')
        .unwrap_or(expression.trim_end());
    let Some((function, arguments)) = expression.split_once('(') else {
        return false;
    };
    if !identifier_path(function) {
        return false;
    }
    let Some(arguments) = arguments.strip_suffix(')') else {
        return false;
    };
    let arguments = arguments.trim().trim_end_matches(',');
    arguments.is_empty()
        || arguments.split(',').all(|argument| {
            let argument = argument.trim();
            identifier_path(argument.strip_prefix('&').unwrap_or(argument))
        })
}

fn identifier_path(value: &str) -> bool {
    !value.is_empty()
        && value
            .split("::")
            .flat_map(|part| part.split('.'))
            .all(|part| {
                if super::heuristics::strong_prefixed_token(part) {
                    return false;
                }
                let mut characters = part.chars();
                characters
                    .next()
                    .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
                    && characters
                        .all(|character| character.is_ascii_alphanumeric() || character == '_')
            })
}

/// Public API-key environment references are not a blanket exemption for
/// password/token fields or arbitrary uppercase strings. Exact secrets still
/// win before contextual detection is applied.
pub(super) fn public_api_key_reference(key: &str, value: &str) -> bool {
    let key: String = key
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .map(|character| character.to_ascii_uppercase())
        .collect();
    key.ends_with("APIKEY")
        && matches!(
            value,
            "ANTHROPIC_API_KEY" | "OPENAI_API_KEY" | "OTTO_API_KEY"
        )
}
