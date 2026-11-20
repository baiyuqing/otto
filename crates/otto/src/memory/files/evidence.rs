//! Built-in dependency validation and durable, transitive invalidation.
use std::collections::BTreeSet;

use chrono::Utc;

use super::{FileStore, Snapshot};
use crate::memory::evidence::{self, Evidence};
use crate::memory::{Record, RecordState, Result, invalid_request};

impl FileStore {
    pub(super) fn check_evidence(
        &self,
        snapshot: &Snapshot,
        record: &Record,
        retiring_id: Option<&str>,
    ) -> Result<()> {
        let anchors = evidence::read(&record.metadata)?;
        // Inactive transitions may retain an exact old anchor after its source
        // disappears or becomes stale/ineffective. New anchors must be current
        // and effective regardless of destination state; cycles remain invalid.
        let retained = snapshot
            .record(&record.scope, &record.id)
            .map(|existing| evidence::read(&existing.metadata))
            .transpose()?
            .unwrap_or_default();
        let mut pending = Vec::new();
        for anchor in anchors {
            let is_retained = record.state != RecordState::Active && retained.contains(&anchor);
            if let Evidence::Record { id, revision, .. } = anchor {
                let source = snapshot.record(&record.scope, &id);
                if source.is_none() && !is_retained {
                    return Err(invalid_request("evidence source unavailable in this scope"));
                }
                if !is_retained
                    && source.is_none_or(|source| {
                        source.revision != revision || !source.effective(Utc::now())
                    })
                {
                    return Err(invalid_request(
                        "evidence revision is stale or not effective; rebind after verification",
                    ));
                }
                pending.push(id);
            }
        }
        let mut visited = BTreeSet::new();
        while let Some(id) = pending.pop() {
            if id == record.id {
                return Err(invalid_request("evidence dependency cycle"));
            }
            if Some(id.as_str()) == retiring_id {
                return Err(invalid_request(
                    "replacement cannot depend on the evidence it retires",
                ));
            }
            if !visited.insert(id.clone()) {
                continue;
            }
            if let Some(source) = snapshot.record(&record.scope, &id) {
                for anchor in evidence::read(&source.metadata)? {
                    if let Evidence::Record { id, .. } = anchor {
                        pending.push(id);
                    }
                }
            }
        }
        Ok(())
    }

    /// Called only under the exclusive file-store lock. Every invalidated
    /// record gets a new revision, so restoration cannot revive old bindings.
    pub(super) fn reconcile_evidence(&self, snapshot: &mut Snapshot) -> Result<()> {
        loop {
            let mut changed = false;
            for index in 0..snapshot.records.len() {
                let current = &snapshot.records[index];
                if current.state != RecordState::Active {
                    continue;
                }
                let mut invalid = evidence::condition_changed(current);
                for anchor in evidence::read(&current.metadata)? {
                    let Evidence::Record { id, revision, .. } = anchor else {
                        continue;
                    };
                    if let Some(source) = snapshot.record(&current.scope, &id) {
                        invalid |= source.revision != revision || !source.effective(Utc::now());
                    } else if snapshot.tombstone(&current.scope, &id).is_some() {
                        invalid = true;
                    } else {
                        // Absence alone may be a temporary read/parse failure.
                        otto_platform::log::Logger::stderr().warn(
                            "memory evidence source unavailable; dependency validity is unknown",
                            &[],
                        );
                    }
                }
                if !invalid {
                    continue;
                }
                let mut desired = current.clone();
                desired.revision = desired
                    .revision
                    .checked_add(1)
                    .ok_or_else(|| invalid_request("record revision overflow"))?;
                desired.state = RecordState::NeedsReview;
                desired.state_reason =
                    "evidence changed or is no longer effective; verify and rebind".into();
                desired
                    .metadata
                    .insert(evidence::CONDITION_CHANGED.into(), "true".into());
                self.archive_current_revision(snapshot, &desired)?;
                self.put_record(&desired)?;
                snapshot.records[index] = desired;
                changed = true;
            }
            if !changed {
                return Ok(());
            }
        }
    }
}
