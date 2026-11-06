//! Lifecycle writes: state changes and verifications.
//!
//! Both write a new revision (and so a history file) with the content fields
//! and `updated_at` unchanged, so a review never looks like a content update.

use chrono::{DateTime, Utc};

use super::records::Mutation;
use super::{FileStore, Snapshot};
use crate::memory::guard::guard_record;
use crate::memory::validate::{validate_record, validate_record_ref};
use crate::memory::{
    Error, ErrorKind, Record, RecordRef, RecordState, Result, StoreSetStateRequest,
    StoreVerifyRequest, invalid_request,
};

impl FileStore {
    /// Writes a new revision with the requested state. A superseded record
    /// names an active replacement of the same scope; an active record keeps
    /// neither a replacement nor a reason.
    pub fn set_state(&self, request: &StoreSetStateRequest) -> Result<Record> {
        validate_record_ref(&request.reference)?;
        self.write(|snapshot| {
            let current = self.lifecycle_target(
                snapshot,
                &request.reference,
                request.expected_revision,
                request.changed_at,
            )?;
            if request.state == RecordState::Superseded {
                check_replacement(snapshot, &current, &request.superseded_by)?;
            }
            let desired = state_revision(
                &current,
                request.state,
                &request.superseded_by,
                &request.reason,
            )?;
            self.write_lifecycle(snapshot, desired)
        })
    }

    /// Writes a new revision that records a verification. `review_at` is
    /// replaced by the request value; a `NeedsReview` record becomes active.
    pub fn verify(&self, request: &StoreVerifyRequest) -> Result<Record> {
        validate_record_ref(&request.reference)?;
        self.write(|snapshot| {
            let current = self.lifecycle_target(
                snapshot,
                &request.reference,
                request.expected_revision,
                request.verified_at,
            )?;
            let mut desired = next_revision(&current)?;
            desired.verified_at = Some(request.verified_at);
            desired.review_at = request.review_at;
            if current.state == RecordState::NeedsReview {
                desired.state = RecordState::Active;
                desired.state_reason = String::new();
            }
            self.write_lifecycle(snapshot, desired)
        })
    }

    /// The current record a lifecycle write applies to: present, at the
    /// expected revision, and not later than `changed_at` relative to its
    /// creation. `updated_at` is the content time and is not compared.
    fn lifecycle_target(
        &self,
        snapshot: &Snapshot,
        reference: &RecordRef,
        expected_revision: u64,
        changed_at: DateTime<Utc>,
    ) -> Result<Record> {
        let current = match self.lookup(snapshot, reference)? {
            Mutation::Forgotten(tombstone) => {
                return Err(Error::conflict(
                    "record",
                    &tombstone.id,
                    expected_revision,
                    tombstone.revision,
                ));
            }
            Mutation::Active(record) => record,
        };
        if current.revision != expected_revision {
            return Err(Error::conflict(
                "record",
                &current.id,
                expected_revision,
                current.revision,
            ));
        }
        if changed_at < current.created_at {
            return Err(invalid_request("lifecycle time precedes record creation"));
        }
        Ok(current)
    }

    /// Validates and publishes `desired`; the standing budget is unchanged by
    /// a lifecycle write, so it is not checked.
    fn write_lifecycle(&self, snapshot: &Snapshot, desired: Record) -> Result<Record> {
        validate_record(&desired)?;
        guard_record(self.guard(), &desired)?;
        self.replace_record(snapshot, &desired, false)?;
        Ok(desired)
    }
}

/// The next revision of `current` in `state`: content and `updated_at` are
/// unchanged. An active record keeps neither a reason nor a replacement, and
/// only a superseded record keeps `superseded_by`. Shared by `set_state` and
/// the review of a retract, needs-review, supersede or correct candidate.
pub(super) fn state_revision(
    current: &Record,
    state: RecordState,
    superseded_by: &str,
    reason: &str,
) -> Result<Record> {
    let mut desired = next_revision(current)?;
    desired.state = state;
    desired.superseded_by = String::new();
    desired.state_reason = reason.to_string();
    match state {
        RecordState::Superseded => desired.superseded_by = superseded_by.to_string(),
        RecordState::Active => desired.state_reason = String::new(),
        RecordState::NeedsReview | RecordState::Retracted => {}
    }
    Ok(desired)
}

/// A copy of `current` with the revision advanced; every other field is
/// unchanged.
fn next_revision(current: &Record) -> Result<Record> {
    if current.revision >= i64::MAX as u64 {
        return Err(Error::new(ErrorKind::Corrupt));
    }
    let mut next = current.clone();
    next.revision = current.revision + 1;
    Ok(next)
}

/// The replacement must be another active record of the same scope.
fn check_replacement(snapshot: &Snapshot, current: &Record, replacement_id: &str) -> Result<()> {
    let valid = replacement_id != current.id
        && snapshot
            .record(&current.scope, replacement_id)
            .is_some_and(|replacement| replacement.state == RecordState::Active);
    if valid {
        Ok(())
    } else {
        Err(invalid_request("replacement record"))
    }
}
