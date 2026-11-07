//! JSON shapes of the `/v1/sessions/{id}/memory` routes.
//!
//! These DTOs are the HTTP wire contract described in `openapi.yaml`; they
//! stay separate from `otto::memory` types so a store change cannot silently
//! change the API.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::Serialize;

use otto::memory;

#[derive(Debug, Serialize)]
pub struct MemoryScope {
    pub namespace: String,
    pub id: String,
}

impl From<&memory::Scope> for MemoryScope {
    fn from(scope: &memory::Scope) -> Self {
        Self {
            namespace: scope.namespace.clone(),
            id: scope.id.clone(),
        }
    }
}

/// One record in any lifecycle state.
#[derive(Debug, Serialize)]
pub struct MemoryRecord {
    pub id: String,
    pub scope: MemoryScope,
    pub kind: String,
    pub key: String,
    pub text: String,
    pub labels: Vec<String>,
    pub metadata: BTreeMap<String, String>,
    /// `human`, `model`, `extractor`, `import`, `migration` or `unknown`.
    pub origin: &'static str,
    pub confidence: f64,
    pub revision: u64,
    pub state: &'static str,
    pub state_reason: String,
    pub superseded_by: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub verified_at: Option<DateTime<Utc>>,
    pub review_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    /// Whether the review deadline has passed.
    pub review_due: bool,
    /// Whether normal recall may use the record now: active, unexpired and
    /// not due for review.
    pub effective: bool,
}

impl From<&memory::Record> for MemoryRecord {
    fn from(record: &memory::Record) -> Self {
        let now = Utc::now();
        Self {
            id: record.id.clone(),
            scope: MemoryScope::from(&record.scope),
            kind: record.kind.clone(),
            key: record.key.clone(),
            text: record.text.clone(),
            labels: record.labels.clone(),
            metadata: record.metadata.clone(),
            origin: record
                .source
                .origin
                .map_or("unknown", |origin| origin.as_str()),
            confidence: record.confidence,
            revision: record.revision,
            state: record.state.as_str(),
            state_reason: record.state_reason.clone(),
            superseded_by: record.superseded_by.clone(),
            created_at: record.created_at,
            updated_at: record.updated_at,
            verified_at: record.verified_at,
            review_at: record.review_at,
            expires_at: record.expires_at,
            review_due: record.review_due(now),
            effective: record.effective(now),
        }
    }
}

pub fn records(records: &[memory::Record]) -> Vec<MemoryRecord> {
    records.iter().map(MemoryRecord::from).collect()
}

#[derive(Debug, Serialize)]
pub struct MemoryRecordPage {
    pub records: Vec<MemoryRecord>,
    pub next_cursor: String,
}

/// A proposed change awaiting (or past) a human decision.
#[derive(Debug, Serialize)]
pub struct MemoryCandidate {
    pub id: String,
    /// `create`, `update`, `forget`, `retract`, `supersede`, `correct` or
    /// `needs_review`.
    pub action: &'static str,
    pub state: &'static str,
    pub target_id: String,
    pub base_revision: u64,
    pub kind: String,
    pub key: String,
    pub text: String,
    pub reason: String,
    pub origin: &'static str,
    pub scope: MemoryScope,
    pub created_at: DateTime<Utc>,
    /// The one-line review advice, omitted when there is none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub advice: Option<String>,
    /// The record that accepting would write.
    pub proposed: MemoryRecord,
}

impl From<&memory::Candidate> for MemoryCandidate {
    fn from(candidate: &memory::Candidate) -> Self {
        let proposed = &candidate.proposed;
        Self {
            id: candidate.id.clone(),
            action: candidate.action.as_str(),
            state: candidate.state.as_str(),
            target_id: candidate.target_id.clone(),
            base_revision: candidate.base_revision,
            kind: proposed.kind.clone(),
            key: proposed.key.clone(),
            text: proposed.text.clone(),
            reason: candidate.reason.clone(),
            origin: proposed
                .source
                .origin
                .map_or("unknown", |origin| origin.as_str()),
            scope: MemoryScope::from(&proposed.scope),
            created_at: candidate.created_at,
            advice: memory::advice::advice_line(proposed),
            proposed: MemoryRecord::from(proposed),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct MemoryCandidatePage {
    pub candidates: Vec<MemoryCandidate>,
    pub next_cursor: String,
}

#[derive(Debug, Serialize)]
pub struct MemoryTombstone {
    pub id: String,
    pub scope: MemoryScope,
    pub revision: u64,
    pub forgotten_at: DateTime<Utc>,
}

impl From<&memory::Tombstone> for MemoryTombstone {
    fn from(tombstone: &memory::Tombstone) -> Self {
        Self {
            id: tombstone.id.clone(),
            scope: MemoryScope::from(&tombstone.scope),
            revision: tombstone.revision,
            forgotten_at: tombstone.forgotten_at,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct MemoryForgotten {
    pub tombstone: MemoryTombstone,
    pub previous_revision: u64,
}

#[derive(Debug, Serialize)]
pub struct MemorySkippedRevision {
    pub file: String,
    pub reason: String,
}

#[derive(Debug, Serialize)]
pub struct MemoryHistory {
    /// Oldest first; the current revision is last.
    pub revisions: Vec<MemoryRecord>,
    pub skipped: Vec<MemorySkippedRevision>,
}

impl From<&memory::RecordHistory> for MemoryHistory {
    fn from(history: &memory::RecordHistory) -> Self {
        Self {
            revisions: records(&history.revisions),
            skipped: history
                .skipped
                .iter()
                .map(|skipped| MemorySkippedRevision {
                    file: skipped.file.clone(),
                    reason: skipped.reason.clone(),
                })
                .collect(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct MemoryDiff {
    pub from: MemoryRecord,
    pub to: MemoryRecord,
}

#[derive(Debug, Serialize)]
pub struct MemorySearch {
    pub records: Vec<MemoryRecord>,
}

/// The result of `POST .../propose`.
#[derive(Debug, Serialize)]
pub struct MemoryProposal {
    /// `proposed`, `no_change`, `rejected` or `unavailable`.
    pub outcome: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidate: Option<MemoryCandidate>,
    /// The model explanation for `no_change`, or why the proposal was
    /// rejected or unavailable.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

impl From<&memory::proposals::Outcome> for MemoryProposal {
    fn from(outcome: &memory::proposals::Outcome) -> Self {
        use memory::proposals::Outcome;
        let (outcome, candidate, detail) = match outcome {
            Outcome::Proposed(candidate) => (
                "proposed",
                Some(MemoryCandidate::from(candidate.as_ref())),
                String::new(),
            ),
            Outcome::NoChange { explanation } => ("no_change", None, explanation.clone()),
            Outcome::Rejected(reason) => ("rejected", None, reason.clone()),
            Outcome::Unavailable(reason) => ("unavailable", None, reason.clone()),
        };
        Self {
            outcome,
            candidate,
            detail,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct MemoryReviewed {
    pub decision: &'static str,
    pub candidate: MemoryCandidate,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record: Option<MemoryRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tombstone: Option<MemoryTombstone>,
}
