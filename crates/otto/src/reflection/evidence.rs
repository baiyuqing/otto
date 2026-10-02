//! Evidence verification: every proposal must quote the transcript, and the
//! harness checks the quotes in code.
//!
//! A quote is accepted only when it is a verbatim substring (after whitespace
//! normalization) of the text of an entry the model was actually shown, that
//! entry is not external, and the quote is long enough to mean something. A
//! proposal that fails is dropped; the model is never asked to vouch for
//! itself.

use std::collections::HashMap;

use serde::Deserialize;

use super::transcript::{Entry, EntryRole};

/// The fewest characters a quote may have once whitespace is normalized.
pub const MINIMUM_QUOTE_CHARS: usize = 6;
/// The most evidence items one proposal may cite.
pub const MAXIMUM_ITEMS: usize = 8;
/// The most distinct entries one proposal may cite; the memory store accepts
/// at most this many provenance message ids.
pub const MAXIMUM_CITED_ENTRIES: usize = 32;

/// One citation as the model wrote it.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Item {
    pub entry: String,
    pub quote: String,
}

/// Why a proposal's evidence was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// No citations, or more than [`MAXIMUM_ITEMS`].
    Count,
    /// A cited id is not an entry the model was shown.
    UnknownEntry,
    /// A cited entry is external content.
    ExternalEntry,
    /// A quote is shorter than [`MINIMUM_QUOTE_CHARS`].
    ShortQuote,
    /// A quote is not in the cited entry's text.
    QuoteMismatch,
    /// The proposal needs a user message among its citations and has none.
    NoUserEntry,
}

impl Failure {
    pub fn reason(self) -> &'static str {
        match self {
            Self::Count => "evidence_count",
            Self::UnknownEntry => "evidence_unknown_entry",
            Self::ExternalEntry => "evidence_external_entry",
            Self::ShortQuote => "evidence_short_quote",
            Self::QuoteMismatch => "evidence_quote_mismatch",
            Self::NoUserEntry => "evidence_no_user_entry",
        }
    }
}

/// Collapses every run of whitespace to one space and trims.
pub fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Verifies `items` against the entries the model was shown. On success
/// returns the distinct cited entry ids in citation order.
///
/// `require_user` demands at least one citation of a user message.
pub fn verify(
    items: &[Item],
    entries: &HashMap<&str, &Entry>,
    require_user: bool,
) -> Result<Vec<String>, Failure> {
    if items.is_empty() || items.len() > MAXIMUM_ITEMS {
        return Err(Failure::Count);
    }
    let mut cited: Vec<String> = Vec::new();
    let mut has_user = false;
    for item in items {
        let entry = entries
            .get(item.entry.as_str())
            .ok_or(Failure::UnknownEntry)?;
        if entry.external {
            return Err(Failure::ExternalEntry);
        }
        let quote = normalize(&item.quote);
        if quote.chars().count() < MINIMUM_QUOTE_CHARS {
            return Err(Failure::ShortQuote);
        }
        if !normalize(&entry.text).contains(&quote) {
            return Err(Failure::QuoteMismatch);
        }
        has_user |= entry.role == EntryRole::User;
        if !cited.contains(&item.entry) {
            cited.push(item.entry.clone());
        }
    }
    if require_user && !has_user {
        return Err(Failure::NoUserEntry);
    }
    cited.truncate(MAXIMUM_CITED_ENTRIES);
    Ok(cited)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, role: EntryRole, external: bool, text: &str) -> Entry {
        Entry {
            id: id.into(),
            role,
            tool: String::new(),
            is_error: false,
            external,
            text: text.into(),
        }
    }

    fn item(entry: &str, quote: &str) -> Item {
        Item {
            entry: entry.into(),
            quote: quote.into(),
        }
    }

    fn verify_with(
        entries: &[Entry],
        items: &[Item],
        require_user: bool,
    ) -> Result<Vec<String>, Failure> {
        let map: HashMap<&str, &Entry> = entries.iter().map(|e| (e.id.as_str(), e)).collect();
        verify(items, &map, require_user)
    }

    fn fixture() -> Vec<Entry> {
        vec![
            entry(
                "a0000001",
                EntryRole::User,
                false,
                "Please always answer in\n  Chinese from now on.",
            ),
            entry(
                "a0000002",
                EntryRole::Assistant,
                false,
                "Understood, switching to Chinese.",
            ),
            entry("a0000003", EntryRole::Tool, true, ""),
        ]
    }

    #[test]
    fn a_verbatim_quote_is_accepted_across_whitespace_differences() {
        let cited = verify_with(
            &fixture(),
            &[item("a0000001", "always answer in Chinese")],
            true,
        )
        .expect("verify");
        assert_eq!(cited, vec!["a0000001".to_owned()]);
    }

    #[test]
    fn a_quote_that_is_not_in_the_entry_is_rejected() {
        assert_eq!(
            verify_with(
                &fixture(),
                &[item("a0000001", "always answer in French")],
                true
            ),
            Err(Failure::QuoteMismatch)
        );
    }

    #[test]
    fn a_quote_cannot_come_from_a_different_entry() {
        assert_eq!(
            verify_with(
                &fixture(),
                &[item("a0000002", "always answer in Chinese")],
                false
            ),
            Err(Failure::QuoteMismatch)
        );
    }

    #[test]
    fn unknown_and_external_entries_cannot_be_cited() {
        assert_eq!(
            verify_with(&fixture(), &[item("ffffffff", "whatever text")], false),
            Err(Failure::UnknownEntry)
        );
        assert_eq!(
            verify_with(&fixture(), &[item("a0000003", "whatever text")], false),
            Err(Failure::ExternalEntry)
        );
    }

    #[test]
    fn a_short_quote_is_rejected() {
        assert_eq!(
            verify_with(&fixture(), &[item("a0000001", "in")], false),
            Err(Failure::ShortQuote)
        );
    }

    #[test]
    fn a_preference_needs_a_user_citation() {
        let items = [item("a0000002", "switching to Chinese")];
        assert_eq!(
            verify_with(&fixture(), &items, true),
            Err(Failure::NoUserEntry)
        );
        assert!(verify_with(&fixture(), &items, false).is_ok());
    }

    #[test]
    fn empty_and_oversized_citation_lists_are_rejected() {
        assert_eq!(verify_with(&fixture(), &[], false), Err(Failure::Count));
        let many: Vec<Item> = (0..=MAXIMUM_ITEMS)
            .map(|_| item("a0000001", "always answer in Chinese"))
            .collect();
        assert_eq!(verify_with(&fixture(), &many, false), Err(Failure::Count));
    }

    #[test]
    fn repeated_citations_of_one_entry_are_reported_once() {
        let items = [
            item("a0000001", "always answer in Chinese"),
            item("a0000001", "from now on"),
        ];
        assert_eq!(
            verify_with(&fixture(), &items, true).expect("verify"),
            vec!["a0000001".to_owned()]
        );
    }
}
