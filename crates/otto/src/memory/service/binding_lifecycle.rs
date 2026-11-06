//! The lifecycle query of the agent seam: the current status of the records
//! a conversation refers to, read from the built-in store.

use otto_core::agent::memory::{MemoryError, RecordLifecycle, RecordStatus};

use super::{Binding, Error, ErrorKind, Record, RecordRef, RecordState};

impl Binding {
    /// The status of each id in the first scope of this binding that holds it.
    /// An id with no record falls back to its tombstone (`Forgotten`), then to
    /// `Unknown`. A closed binding fails; a null service answers `Unknown`.
    pub(super) fn record_lifecycles(&self, ids: &[String]) -> Result<Vec<RecordLifecycle>, Error> {
        self.check_open()?;
        if self.service.store.is_none() {
            return Ok(ids.iter().map(|id| unknown_lifecycle(id)).collect());
        }
        let now = (self.now)();
        let mut lifecycles = Vec::with_capacity(ids.len());
        for id in ids {
            lifecycles.push(self.record_lifecycle(id, now)?);
        }
        Ok(lifecycles)
    }

    fn record_lifecycle(
        &self,
        id: &str,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<RecordLifecycle, Error> {
        for scope in &self.scopes {
            let reference = RecordRef {
                scope: scope.clone(),
                id: id.to_string(),
            };
            match self.service.get(&reference) {
                Ok(record) => return Ok(live_lifecycle(&record, now)),
                Err(error) if error.is(ErrorKind::NotFound) => {}
                Err(error) => return Err(error),
            }
            match self.service.get_tombstone(&reference) {
                Ok(tombstone) => {
                    return Ok(RecordLifecycle {
                        id: id.to_string(),
                        revision: Some(tombstone.revision.to_string()),
                        status: RecordStatus::Forgotten,
                    });
                }
                Err(error) if error.is(ErrorKind::NotFound) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(unknown_lifecycle(id))
    }
}

fn unknown_lifecycle(id: &str) -> RecordLifecycle {
    RecordLifecycle {
        id: id.to_string(),
        revision: None,
        status: RecordStatus::Unknown,
    }
}

fn live_lifecycle(record: &Record, now: chrono::DateTime<chrono::Utc>) -> RecordLifecycle {
    let status = match record.state {
        RecordState::Active if record.expired(now) => RecordStatus::Expired,
        RecordState::Active => RecordStatus::Active,
        RecordState::NeedsReview => RecordStatus::NeedsReview,
        RecordState::Superseded => RecordStatus::Superseded {
            replacement_id: record.superseded_by.clone(),
        },
        RecordState::Retracted => RecordStatus::Retracted,
    };
    RecordLifecycle {
        id: record.id.clone(),
        revision: Some(record.revision.to_string()),
        status,
    }
}

pub(super) fn memory_error(error: Error) -> MemoryError {
    MemoryError(error.to_string())
}
