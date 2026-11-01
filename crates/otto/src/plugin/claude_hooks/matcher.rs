//! Matcher text of a Claude hook group, matched against a tool name, session source,
//! compaction trigger or session end reason depending on the event.

use regex::Regex;

/// Longest accepted matcher text in bytes.
const MAX_MATCHER_BYTES: usize = 1024;

/// A parsed matcher. Plain names compare for exact equality; other text is a regex.
#[derive(Clone, Debug)]
pub enum Matcher {
    /// Matches every value.
    Any,
    /// Matches a value equal to one of the names.
    Names(Vec<String>),
    /// Unanchored, case-sensitive regular expression.
    Pattern(Regex),
}

fn is_name_list_char(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | ' ' | ',' | '|')
}

impl Matcher {
    /// `""` and `"*"` give [`Matcher::Any`]. Text made only of letters, digits, `_`, `-`,
    /// space, `,` and `|` is a name list split on `|` and `,`; each name is trimmed (internal
    /// spaces stay part of the name) and empty names are dropped, so `"|"` gives an empty
    /// list that matches nothing. Any other text is compiled as a regex.
    pub fn parse(text: &str) -> Result<Matcher, String> {
        if text.len() > MAX_MATCHER_BYTES {
            return Err(format!("matcher exceeds {MAX_MATCHER_BYTES} bytes"));
        }
        if text.is_empty() || text == "*" {
            return Ok(Matcher::Any);
        }
        if text.chars().all(is_name_list_char) {
            let names = text
                .split(['|', ','])
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
                .collect();
            return Ok(Matcher::Names(names));
        }
        Regex::new(text)
            .map(Matcher::Pattern)
            .map_err(|error| format!("matcher is not a valid regex: {error}"))
    }

    /// Whether `value` satisfies the matcher.
    pub fn matches(&self, value: &str) -> bool {
        match self {
            Matcher::Any => true,
            Matcher::Names(names) => names.iter().any(|name| name == value),
            Matcher::Pattern(pattern) => pattern.is_match(value),
        }
    }
}
