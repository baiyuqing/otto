//! The evaluation-only memory binding: it answers recall and lifecycle queries as the
//! condition prescribes and is registered nowhere else.

use otto_core::agent::memory::{
    MemoryError, MemoryRecall, RecallRequest, RecallResult, Record, RecordLifecycle, RecordStatus,
    Scope,
};
use tokio_util::sync::CancellationToken;

use super::argument_parsing::Condition;
use super::items::{Item, Retirement};

/// Namespace of every evaluation record's scope.
pub const SCOPE_NAMESPACE: &str = "workspace";

/// The records and lifecycle answers of one item under one condition.
pub struct EvalMemory {
    /// Returned by every recall, ignoring the query.
    pub recalled: Vec<Record>,
    /// Returned by every lifecycle query for the ids it names; an id outside the list is
    /// `Unknown`.
    pub lifecycle: Vec<RecordLifecycle>,
}

impl EvalMemory {
    /// A: the obsolete record active and recalled with the control record. B: the control
    /// record and the replacement (if any) recalled; the obsolete id is `Unknown`. C and D:
    /// the same recall; the obsolete id reports the item's retirement.
    pub fn for_condition(item: &Item, condition: Condition) -> Self {
        let record = &item.record;
        let control = &item.control.record;
        let obsolete_record = eval_record(
            &record.id,
            &record.revision,
            &record.kind,
            &record.key,
            &record.scope_id,
            &record.text,
        );
        let control_record = eval_record(
            &control.id,
            &control.revision,
            &control.kind,
            &control.key,
            &control.scope_id,
            &control.text,
        );
        let replacement_record = item.replacement.as_ref().map(|replacement| {
            eval_record(
                &replacement.id,
                &replacement.revision,
                &record.kind,
                &record.key,
                &record.scope_id,
                &replacement.text,
            )
        });
        let mut lifecycle = vec![active(&control_record)];
        if let Some(replacement) = &replacement_record {
            lifecycle.push(active(replacement));
        }
        let mut recalled = Vec::new();
        match condition {
            Condition::A => {
                recalled.push(obsolete_record.clone());
                lifecycle.push(active(&obsolete_record));
            }
            Condition::B => recalled.extend(replacement_record),
            Condition::C | Condition::D => {
                recalled.extend(replacement_record);
                lifecycle.push(RecordLifecycle {
                    id: record.id.clone(),
                    revision: Some(record.revision.clone()),
                    status: retired_status(item),
                });
            }
        }
        recalled.push(control_record);
        Self {
            recalled,
            lifecycle,
        }
    }
}

fn eval_record(
    id: &str,
    revision: &str,
    kind: &str,
    key: &str,
    scope_id: &str,
    text: &str,
) -> Record {
    Record {
        id: id.to_owned(),
        revision: Some(revision.to_owned()),
        scope: Scope {
            namespace: SCOPE_NAMESPACE.to_owned(),
            id: scope_id.to_owned(),
        },
        kind: kind.to_owned(),
        key: key.to_owned(),
        text: text.to_owned(),
        standing: false,
    }
}

fn active(record: &Record) -> RecordLifecycle {
    RecordLifecycle {
        id: record.id.clone(),
        revision: record.revision.clone(),
        status: RecordStatus::Active,
    }
}

/// The lifecycle status the item's retirement maps to.
fn retired_status(item: &Item) -> RecordStatus {
    match (item.retirement, &item.replacement) {
        (Retirement::Superseded, Some(replacement)) => RecordStatus::Superseded {
            replacement_id: replacement.id.clone(),
        },
        (Retirement::Retracted, _) => RecordStatus::Retracted,
        (Retirement::Forgotten, _) => RecordStatus::Forgotten,
        // `parse_items` rejects a superseded item without a replacement.
        (Retirement::Superseded, None) => RecordStatus::Retracted,
    }
}

#[async_trait::async_trait]
impl MemoryRecall for EvalMemory {
    async fn recall(
        &self,
        _request: &RecallRequest,
        _cancel: &CancellationToken,
    ) -> Result<RecallResult, MemoryError> {
        Ok(RecallResult {
            records: self.recalled.clone(),
            used_tokens: 0,
        })
    }

    async fn lifecycle(
        &self,
        ids: &[String],
        _cancel: &CancellationToken,
    ) -> Result<Vec<RecordLifecycle>, MemoryError> {
        Ok(ids
            .iter()
            .map(|id| {
                self.lifecycle
                    .iter()
                    .find(|entry| entry.id == *id)
                    .cloned()
                    .unwrap_or_else(|| RecordLifecycle {
                        id: id.clone(),
                        revision: None,
                        status: RecordStatus::Unknown,
                    })
            })
            .collect())
    }
}
