//! The evaluation-only memory binding: it answers recall and lifecycle queries as the
//! condition prescribes and is registered nowhere else.

use otto_core::agent::memory::{
    MemoryError, MemoryRecall, RecallRequest, RecallResult, Record, RecordLifecycle,
};
use tokio_util::sync::CancellationToken;

use super::argument_parsing::Condition;
use super::items::Item;

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
    pub fn for_condition(_item: &Item, _condition: Condition) -> Self {
        Self {
            recalled: Vec::new(),
            lifecycle: Vec::new(),
        }
    }
}

#[async_trait::async_trait]
impl MemoryRecall for EvalMemory {
    async fn recall(
        &self,
        _request: &RecallRequest,
        _cancel: &CancellationToken,
    ) -> Result<RecallResult, MemoryError> {
        Err(MemoryError("not implemented".into()))
    }

    async fn lifecycle(
        &self,
        _ids: &[String],
        _cancel: &CancellationToken,
    ) -> Result<Vec<RecordLifecycle>, MemoryError> {
        Err(MemoryError("not implemented".into()))
    }
}
