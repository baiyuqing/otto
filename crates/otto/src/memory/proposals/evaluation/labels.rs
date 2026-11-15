//! The labeled dataset: item types, the JSONL parser and its consistency checks.

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use otto_core::memory::Trust;
use serde::{Deserialize, Serialize};

use crate::memory::{NAMESPACE_WORKSPACE, Record, Scope};

use super::super::{EvidenceItem, MAX_EXPLANATION_BYTES, ProposalAction, ProposalInput};

/// Repository path of the labeled dataset.
#[cfg(test)]
pub(crate) const LABELED_DATASET_PATH: &str = "testdata/memory-eval/proposals/labeled.jsonl";
/// The version of the label format and the scoring rubric.
pub const DATASET_VERSION: &str = "memory-proposal-labels/1";

const RECORD_KINDS: [&str; 3] = ["preference", "fact", "instruction"];
/// Seconds since the Unix epoch of the fixed record time (2023-11-14).
const FIXED_RECORD_SECONDS: i64 = 1_700_000_000;

/// Serde adapter: a list of actions as snake_case names.
pub(super) mod action_list {
    use serde::de::Error;
    use serde::{Deserialize, Deserializer, Serializer};

    use super::ProposalAction;

    pub fn serialize<S: Serializer>(actions: &[ProposalAction], out: S) -> Result<S::Ok, S::Error> {
        out.collect_seq(actions.iter().map(|action| action.as_str()))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        input: D,
    ) -> Result<Vec<ProposalAction>, D::Error> {
        Vec::<String>::deserialize(input)?
            .iter()
            .map(|name| {
                ProposalAction::parse(name)
                    .ok_or_else(|| D::Error::custom(format!("unknown action {name}")))
            })
            .collect()
    }
}

/// Serde adapter: a trust level as `local` or `external`.
mod trust_name {
    use otto_core::memory::Trust;
    use serde::de::Error;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(trust: &Trust, out: S) -> Result<S::Ok, S::Error> {
        out.serialize_str(match trust {
            Trust::Local => "local",
            Trust::External => "external",
        })
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(input: D) -> Result<Trust, D::Error> {
        match String::deserialize(input)?.as_str() {
            "local" => Ok(Trust::Local),
            "external" => Ok(Trust::External),
            other => Err(D::Error::custom(format!("unknown trust {other}"))),
        }
    }
}

fn all_actions() -> Vec<ProposalAction> {
    ProposalAction::ALL.to_vec()
}

/// The record under evaluation. `marker` is a unique token inside `text`; a
/// leakage check searches for it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LabeledRecord {
    pub text: String,
    pub kind: String,
    pub key: String,
    pub marker: String,
}

/// One evidence item of a labeled input.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LabeledEvidence {
    pub id: String,
    pub text: String,
    #[serde(with = "trust_name")]
    pub trust: Trust,
    pub source: String,
}

/// The human label of one item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Label {
    /// The acceptable actions; a proposal with any of them is action-correct.
    #[serde(with = "action_list")]
    pub expected_actions: Vec<ProposalAction>,
    /// The evidence ids that support the expected change.
    pub supporting_evidence: Vec<String>,
    /// Lowercase terms a replacement must contain (preserved claims).
    pub replacement_must_contain: Vec<String>,
    /// Lowercase terms a replacement must not contain (broadened scope).
    pub replacement_must_not_contain: Vec<String>,
    /// Lowercase terms that show the opposite of a withdrawn claim.
    pub inversion_terms: Vec<String>,
    /// A replacement that satisfies the label; present exactly when a
    /// `supersede` or `correct` action is expected.
    pub reference_replacement: Option<String>,
    /// Why the labeler chose these actions.
    pub rationale: String,
}

/// One labeled item: a record, its evidence and the expected outcome.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LabeledItem {
    pub name: String,
    pub category: String,
    pub record: LabeledRecord,
    pub evidence: Vec<LabeledEvidence>,
    #[serde(default = "all_actions", with = "action_list")]
    pub permitted: Vec<ProposalAction>,
    pub label: Label,
}

impl LabeledItem {
    /// The generator input of this item: a workspace record with fixed ids and
    /// times, the evidence and the permitted actions.
    pub fn input(&self, session_id: &str) -> ProposalInput {
        let fixed_time = DateTime::<Utc>::from_timestamp(FIXED_RECORD_SECONDS, 0)
            .unwrap_or(DateTime::UNIX_EPOCH);
        ProposalInput {
            target: Record {
                id: format!("eval-{}", self.name),
                scope: Scope::new(NAMESPACE_WORKSPACE, "memory-eval"),
                kind: self.record.kind.clone(),
                key: self.record.key.clone(),
                text: self.record.text.clone(),
                confidence: 1.0,
                revision: 1,
                created_at: fixed_time,
                updated_at: fixed_time,
                ..Record::default()
            },
            evidence: self
                .evidence
                .iter()
                .map(|item| EvidenceItem {
                    id: item.id.clone(),
                    text: item.text.clone(),
                    trust: item.trust,
                    source: item.source.clone(),
                })
                .collect(),
            permitted: self.permitted.clone(),
            session_id: session_id.to_owned(),
        }
    }
}

/// Whether the action removes or replaces the record's assertion.
pub fn retires(action: ProposalAction) -> bool {
    matches!(
        action,
        ProposalAction::Retract
            | ProposalAction::Supersede
            | ProposalAction::Correct
            | ProposalAction::Forget
    )
}

/// Whether the action needs supporting evidence (everything but abstention).
pub(crate) fn requires_evidence(action: ProposalAction) -> bool {
    retires(action)
}

/// Whether the action carries replacement text.
pub(crate) fn requires_replacement(action: ProposalAction) -> bool {
    matches!(action, ProposalAction::Supersede | ProposalAction::Correct)
}

/// Case-insensitive substring match of `term` in `text`.
pub fn contains_term(text: &str, term: &str) -> bool {
    text.to_lowercase().contains(&term.to_lowercase())
}

/// Parses every non-empty line, then checks the dataset's consistency. The
/// error names the line or the item and the rule.
pub fn parse_labeled_items(jsonl: &str) -> Result<Vec<LabeledItem>, String> {
    let mut items: Vec<LabeledItem> = Vec::new();
    for (index, line) in jsonl.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let item =
            serde_json::from_str(line).map_err(|error| format!("line {}: {error}", index + 1))?;
        items.push(item);
    }
    let mut names = BTreeSet::new();
    let mut markers = BTreeSet::new();
    for item in &items {
        if !names.insert(item.name.as_str()) {
            return Err(rule_error(item, "duplicate name"));
        }
        if !markers.insert(item.record.marker.as_str()) {
            return Err(rule_error(item, "duplicate marker"));
        }
        check_item(item)?;
    }
    Ok(items)
}

fn rule_error(item: &LabeledItem, rule: &str) -> String {
    format!("item {}: {rule}", item.name)
}

fn is_marker(marker: &str) -> bool {
    let bytes = marker.as_bytes();
    bytes.len() == 9
        && bytes.starts_with(b"m-")
        && bytes[2].is_ascii_digit()
        && bytes[3].is_ascii_digit()
        && bytes[4] == b'-'
        && bytes[5..].iter().all(u8::is_ascii_lowercase)
}

fn is_identifier(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn check_item(item: &LabeledItem) -> Result<(), String> {
    let fail = |rule: &str| Err(rule_error(item, rule));
    let label = &item.label;
    if !is_identifier(&item.name) || item.category.is_empty() {
        return fail("name must be a snake_case identifier and category non-empty");
    }
    if !RECORD_KINDS.contains(&item.record.kind.as_str()) || !is_identifier(&item.record.key) {
        return fail(
            "record kind must be preference, fact or instruction and key a lowercase identifier",
        );
    }
    if !is_marker(&item.record.marker) || !item.record.text.contains(&item.record.marker) {
        return fail("marker must match m-NN-xxxx and appear in the record text");
    }
    let mut evidence_ids = BTreeSet::new();
    for evidence in &item.evidence {
        if evidence.text.trim().is_empty() || !evidence_ids.insert(evidence.id.as_str()) {
            return fail("evidence ids must be unique and texts non-empty");
        }
    }
    if item.evidence.is_empty() || item.permitted.is_empty() || label.expected_actions.is_empty() {
        return fail("evidence, permitted and expected_actions must be non-empty");
    }
    if let Some(action) = label
        .expected_actions
        .iter()
        .find(|a| !item.permitted.contains(a))
    {
        return Err(rule_error(
            item,
            &format!("expected action {} is not permitted", action.as_str()),
        ));
    }
    let mut supporting_ids = BTreeSet::new();
    for id in &label.supporting_evidence {
        if !evidence_ids.contains(id.as_str()) {
            return Err(rule_error(
                item,
                &format!("supporting evidence {id} does not exist"),
            ));
        }
        if !supporting_ids.insert(id.as_str()) {
            return Err(rule_error(
                item,
                &format!("supporting evidence {id} is listed twice"),
            ));
        }
    }
    if label.expected_actions.iter().any(|a| requires_evidence(*a))
        && label.supporting_evidence.is_empty()
    {
        return fail("a retiring expected action needs supporting evidence");
    }
    if label.rationale.trim().is_empty() || label.rationale.len() > MAX_EXPLANATION_BYTES {
        return fail("rationale must be non-empty and within the explanation bound");
    }
    check_terms(item)?;
    check_reference_replacement(item)
}

fn check_terms(item: &LabeledItem) -> Result<(), String> {
    let label = &item.label;
    let lists = [
        &label.replacement_must_contain,
        &label.replacement_must_not_contain,
        &label.inversion_terms,
    ];
    for term in lists.iter().flat_map(|list| list.iter()) {
        if term.is_empty() || term.trim() != term {
            return Err(rule_error(item, "terms must be non-empty and trimmed"));
        }
        if term.to_lowercase() != *term {
            return Err(rule_error(
                item,
                &format!("term {term:?} must be lowercase"),
            ));
        }
    }
    for required in &label.replacement_must_contain {
        let forbidden = label
            .replacement_must_not_contain
            .iter()
            .chain(&label.inversion_terms);
        if forbidden.into_iter().any(|term| term == required) {
            return Err(rule_error(
                item,
                &format!("term {required:?} is both required and forbidden"),
            ));
        }
    }
    Ok(())
}

fn check_reference_replacement(item: &LabeledItem) -> Result<(), String> {
    let label = &item.label;
    let fail = |rule: &str| Err(rule_error(item, rule));
    let replaces = label
        .expected_actions
        .iter()
        .any(|a| requires_replacement(*a));
    let Some(reference) = &label.reference_replacement else {
        return if replaces {
            fail("reference_replacement is required for supersede and correct")
        } else {
            Ok(())
        };
    };
    if !replaces {
        return fail("reference_replacement is allowed only with supersede or correct");
    }
    if reference.is_empty()
        || reference.trim() != reference
        || reference.chars().any(char::is_control)
    {
        return fail(
            "reference_replacement must be non-empty, trimmed and free of control characters",
        );
    }
    if reference.trim() == item.record.text.trim() || reference.contains(&item.record.marker) {
        return fail("reference_replacement must differ from the record text and omit the marker");
    }
    if let Some(term) = label
        .replacement_must_contain
        .iter()
        .find(|t| !contains_term(reference, t))
    {
        return Err(rule_error(
            item,
            &format!("reference_replacement lacks required term {term:?}"),
        ));
    }
    let forbidden = label
        .replacement_must_not_contain
        .iter()
        .chain(&label.inversion_terms);
    if let Some(term) = forbidden.into_iter().find(|t| contains_term(reference, t)) {
        return Err(rule_error(
            item,
            &format!("reference_replacement holds forbidden term {term:?}"),
        ));
    }
    Ok(())
}
