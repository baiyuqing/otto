//! Deterministic text canonicalization for security boundaries.
//! Port of `internal/safetext`.
//!
//! Scope: the agent's secret redactor, the sandbox environment classifier,
//! and the `bash` tool all need this, so it lives in the shared crate. It is
//! pure string manipulation with no I/O, so it builds for wasm32.
//!
//! Ownership: every function returns owned data. [`SecretCollector`] is a
//! plain owned value with no interior mutability, so callers serialize access
//! through `&mut` as usual. No function performs I/O, blocks, or fails
//! partially: a rejected value leaves the collector unchanged.

pub const MAX_SECRET_VALUES: usize = 512;
pub const MAX_SECRET_BYTES: usize = 1 << 20;
pub const MAX_DYNAMIC_VALUES: usize = 64;
pub const MAX_DYNAMIC_BYTES: usize = 16 << 10;
pub const MAX_DYNAMIC_VALUE_BYTES: usize = 8 << 10;

/// The first private-use code point the dynamic marker search considers.
const PREFERRED_DYNAMIC_MARKER: u32 = 0xE000;
const DYNAMIC_MARKER_COUNT: u32 = 64;

/// Replaces each invalid UTF-8 byte with `U+FFFD`, leaving valid input alone.
///
/// Go stores strings as bytes, so this takes bytes and returns a `String`.
/// Callers that already hold a `&str` get their input back unchanged.
pub fn canonicalize_utf8(value: &[u8]) -> String {
    String::from_utf8_lossy(value).into_owned()
}

/// At most two canonical forms of a secret: the supplied text and, when that
/// text is valid JSON string content containing an escape, its JSON-decoded
/// equivalent.
pub fn secret_forms(raw: &[u8]) -> Vec<String> {
    let canonical = canonicalize_utf8(raw);
    if canonical.is_empty() {
        return Vec::new();
    }
    let mut forms = vec![canonical.clone()];
    if !canonical.contains('\\') {
        return forms;
    }
    let wrapped = format!("\"{canonical}\"");
    let Ok(decoded) = serde_json::from_str::<String>(&wrapped) else {
        return forms;
    };
    let decoded = canonicalize_utf8(decoded.as_bytes());
    if decoded.is_empty() || decoded == canonical {
        return forms;
    }
    forms.push(decoded);
    forms
}

/// The shared exact-redaction marker for a bounded, fully known secret set.
///
/// Returns `None` when the set exceeds the exact-redaction capability or no
/// safe literal JSON marker remains; exact dynamic redaction must then be
/// disabled rather than approximated.
pub fn dynamic_redaction_marker(values: &[String]) -> Option<String> {
    if !supports_dynamic_redaction(values) {
        return None;
    }
    shared_redaction_marker(values)
}

/// The lowest private-use marker that appears in no value, in neither its
/// literal nor its JSON-serialized form.
pub fn shared_redaction_marker(values: &[String]) -> Option<String> {
    let used: std::collections::HashSet<char> =
        values.iter().flat_map(|value| value.chars()).collect();
    for offset in 0..DYNAMIC_MARKER_COUNT {
        let Some(candidate) = char::from_u32(PREFERRED_DYNAMIC_MARKER + offset) else {
            continue;
        };
        if candidate.is_control() || used.contains(&candidate) {
            continue;
        }
        let marker = candidate.to_string();
        let encoded = serde_json::to_string(&marker).ok()?;
        // Strip the surrounding quotes to get the escaped body, as Go does.
        let serialized = &encoded[1..encoded.len() - 1];
        if contains_retained_form(&marker, values) || contains_retained_form(serialized, values) {
            continue;
        }
        return Some(marker);
    }
    None
}

fn supports_dynamic_redaction(values: &[String]) -> bool {
    if values.len() > MAX_DYNAMIC_VALUES {
        return false;
    }
    let mut total = 0usize;
    for value in values {
        if value.len() > MAX_DYNAMIC_VALUE_BYTES {
            return false;
        }
        total += value.len();
        if total > MAX_DYNAMIC_BYTES {
            return false;
        }
    }
    true
}

fn contains_retained_form(candidate: &str, values: &[String]) -> bool {
    values
        .iter()
        .any(|value| !value.is_empty() && candidate.contains(value.as_str()))
}

/// Sorts longest first, then lexicographically, so leftmost-longest redaction
/// prefers the most specific match.
pub fn sort_longest_first(values: &mut [String]) {
    values.sort_by(|left, right| right.len().cmp(&left.len()).then_with(|| left.cmp(right)));
}

/// Accumulates the distinct secret values a redactor must hide.
///
/// The collector is bounded: once [`MAX_SECRET_VALUES`] distinct values or
/// [`MAX_SECRET_BYTES`] total bytes would be exceeded, [`SecretCollector::add`]
/// returns `false` and the collector is left unchanged. A caller that sees
/// `false` must treat its redaction set as incomplete.
#[derive(Debug, Default)]
pub struct SecretCollector {
    seen: std::collections::HashSet<String>,
    bytes: usize,
}

impl SecretCollector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds every canonical form of `value`. Returns `false` when any form
    /// does not fit, leaving the remaining forms unadded.
    pub fn add(&mut self, value: &str) -> bool {
        secret_forms(value.as_bytes())
            .into_iter()
            .all(|form| self.add_form(&form))
    }

    /// Adds one already-derived form. An empty or duplicate form succeeds
    /// without changing the collector.
    pub fn add_form(&mut self, value: &str) -> bool {
        let value = canonicalize_utf8(value.as_bytes());
        if value.is_empty() || self.seen.contains(&value) {
            return true;
        }
        // Go writes `bytes > maxSecretBytes-len(value)` over signed ints; the
        // same comparison in `usize` underflows for a value larger than the
        // whole budget, so it is rearranged instead of negated.
        if self.seen.len() >= MAX_SECRET_VALUES || self.bytes + value.len() > MAX_SECRET_BYTES {
            return false;
        }
        self.bytes += value.len();
        self.seen.insert(value);
        true
    }

    /// The collected values, longest first.
    pub fn values(&self) -> Vec<String> {
        let mut values: Vec<String> = self.seen.iter().cloned().collect();
        sort_longest_first(&mut values);
        values
    }

    pub fn len(&self) -> usize {
        self.seen.len()
    }

    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_single_value_larger_than_the_whole_byte_budget_is_rejected() {
        let mut collector = SecretCollector::new();
        assert!(!collector.add_form(&"z".repeat(MAX_SECRET_BYTES + 1)));
        assert_eq!(collector.bytes(), 0);
        assert!(collector.is_empty());
        assert!(collector.add_form(&"z".repeat(MAX_SECRET_BYTES)));
        assert_eq!(collector.bytes(), MAX_SECRET_BYTES);
    }

    #[test]
    fn invalid_utf8_becomes_the_replacement_character() {
        assert_eq!(canonicalize_utf8(b"plain"), "plain");
        assert_eq!(canonicalize_utf8(&[0xff, b'a']), "\u{fffd}a");
        assert_eq!(canonicalize_utf8(b""), "");
    }

    #[test]
    fn secret_forms_adds_the_json_decoded_variant_only_when_it_differs() {
        assert_eq!(secret_forms(b"plain"), vec!["plain".to_string()]);
        assert!(secret_forms(b"").is_empty());
        assert_eq!(
            secret_forms(br"a\nb"),
            vec!["a\\nb".to_string(), "a\nb".to_string()]
        );
        // An escape that does not decode leaves a single form.
        assert_eq!(secret_forms(br"a\qb"), vec!["a\\qb".to_string()]);
    }

    #[test]
    fn the_dynamic_marker_skips_code_points_the_values_already_use() {
        let marker = dynamic_redaction_marker(&["secret".to_string()]).unwrap();
        assert_eq!(marker, "\u{e000}");
        let with_marker = dynamic_redaction_marker(&["\u{e000}".to_string()]).unwrap();
        assert_eq!(with_marker, "\u{e001}");
    }

    #[test]
    fn the_dynamic_marker_is_refused_for_sets_beyond_the_exact_capability() {
        let many: Vec<String> = (0..=MAX_DYNAMIC_VALUES).map(|n| n.to_string()).collect();
        assert!(dynamic_redaction_marker(&many).is_none());
        let long = vec!["x".repeat(MAX_DYNAMIC_VALUE_BYTES + 1)];
        assert!(dynamic_redaction_marker(&long).is_none());
        let wide = vec!["x".repeat(MAX_DYNAMIC_VALUE_BYTES); 4];
        assert!(dynamic_redaction_marker(&wide).is_none());
    }

    #[test]
    fn the_collector_deduplicates_bounds_and_sorts_longest_first() {
        let mut collector = SecretCollector::new();
        assert!(collector.add("short"));
        assert!(collector.add("short"));
        assert!(collector.add("a-longer-secret"));
        assert!(collector.add(""));
        assert_eq!(collector.len(), 2);
        assert_eq!(
            collector.values(),
            vec!["a-longer-secret".to_string(), "short".to_string()]
        );

        let mut bounded = SecretCollector::new();
        for index in 0..MAX_SECRET_VALUES {
            assert!(
                bounded.add(&format!("value-{index}")),
                "value {index} rejected"
            );
        }
        assert!(!bounded.add("one-too-many"));
        assert_eq!(bounded.len(), MAX_SECRET_VALUES);
    }
}
