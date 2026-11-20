//! Record reads and mutations, and the standing set's token budget.

use chrono::{DateTime, Utc};

use super::cursor::{decode_record_cursor, encode_record_cursor, fingerprint_list};
use super::retrieve::record_cost;
use super::{FileStore, Snapshot};
use crate::memory::codec::{format_timestamp, parse_timestamp};
use crate::memory::validate::{
    validate_list_request, validate_record, validate_record_key, validate_record_ref,
    validate_store_forget_request, validate_upsert_request,
};
use crate::memory::{
    Error, ErrorKind, ForgetOutcome, GuardField, GuardInput, ListRequest, MAX_STANDING_TOKENS,
    NAMESPACE_USER, Record, RecordKey, RecordPage, RecordRef, RecordState, Result, Scope,
    StandingUsage, StoreForgetRequest, Tombstone, UpsertRequest, invalid_request,
};

fn corrupt() -> Error {
    Error::new(ErrorKind::Corrupt)
}

fn conflict_record(id: &str, expected: u64, actual: u64) -> Error {
    Error::conflict("record", id, expected, actual)
}

/// The state of one record id inside a scope.
#[allow(clippy::large_enum_variant)] // short-lived value returned by one lookup
pub(super) enum Mutation {
    Active(Record),
    Forgotten(Tombstone),
}

/// Whether `record` belongs to the standing set when it is stored.
fn standing_eligible(record: &Record, now: DateTime<Utc>) -> bool {
    record.scope.namespace == NAMESPACE_USER
        && !record.key.is_empty()
        && matches!(record.kind.as_str(), "preference" | "instruction")
        && record.effective(now)
}

/// The standing set of `user` that passes the request filters, oldest first.
pub(super) fn standing_set(
    snapshot: &Snapshot,
    user: &Scope,
    kinds: &[String],
    include_expired: bool,
    now: DateTime<Utc>,
) -> Vec<Record> {
    let mut records: Vec<Record> = snapshot
        .records
        .iter()
        .filter(|record| {
            &record.scope == user
                && !record.key.is_empty()
                && matches!(record.kind.as_str(), "preference" | "instruction")
                && (kinds.is_empty() || kinds.contains(&record.kind))
                && record.state == RecordState::Active
                && !record.review_due(now)
                && (include_expired || !record.expired(now))
        })
        .cloned()
        .collect();
    records.sort_by(|left, right| {
        left.created_at
            .cmp(&right.created_at)
            .then_with(|| left.id.cmp(&right.id))
    });
    records
}

/// Rejects `record` when storing it would push its user scope's standing set
/// over the budget. The record being replaced and the record `retiring_id`
/// names, which the same review retires, are not counted.
pub(super) fn check_standing_budget(
    snapshot: &Snapshot,
    record: &Record,
    retiring_id: Option<&str>,
) -> Result<()> {
    let now = Utc::now();
    if !standing_eligible(record, now) {
        return Ok(());
    }
    let used: usize = standing_set(snapshot, &record.scope, &[], false, now)
        .iter()
        .filter(|existing| existing.id != record.id && Some(existing.id.as_str()) != retiring_id)
        .map(|existing| record_cost(None, existing))
        .sum();
    let needed = record_cost(None, record);
    if used + needed > MAX_STANDING_TOKENS {
        return Err(Error::detailed(
            ErrorKind::InvalidRequest,
            format!(
                "standing preferences are full: {used} of {MAX_STANDING_TOKENS} tokens \
                    used, this record needs {needed}; forget a user-scope preference or \
                    instruction first (/memory forget <id> or otto memory forget)"
            ),
        ));
    }
    Ok(())
}

/// The lifecycle filters of a list request: state (default `Active`), content
/// update time, verification time and review deadline.
fn list_lifecycle_matches(record: &Record, request: &ListRequest) -> bool {
    let state_listed = if request.states.is_empty() {
        record.state == RecordState::Active
    } else {
        request.states.contains(&record.state)
    };
    state_listed
        && request
            .updated_after
            .is_none_or(|after| record.updated_at >= after)
        && request
            .verified_after
            .is_none_or(|after| record.verified_at.is_some_and(|verified| verified >= after))
        && (!request.unverified_only || record.verified_at.is_none())
        && (!request.review_due_only || record.review_due(request.now))
}

impl FileStore {
    /// Finds a record id the way a write sees it: active, forgotten, an
    /// unreadable file (`Corrupt`) or absent (`NotFound`).
    pub(super) fn lookup(&self, snapshot: &Snapshot, reference: &RecordRef) -> Result<Mutation> {
        if let Some(record) = snapshot.record(&reference.scope, &reference.id) {
            return Ok(Mutation::Active(record.clone()));
        }
        if let Some(tombstone) = snapshot.tombstone(&reference.scope, &reference.id) {
            return Ok(Mutation::Forgotten(tombstone.clone()));
        }
        if snapshot.is_invalid(&self.path("records", &reference.id, "md"))
            || snapshot.is_invalid(&self.path("tombstones", &reference.id, "json"))
        {
            return Err(corrupt());
        }
        Err(Error::new(ErrorKind::NotFound))
    }

    pub fn get(&self, reference: &RecordRef) -> Result<Record> {
        validate_record_ref(reference)?;
        self.snapshot()?
            .record(&reference.scope, &reference.id)
            .cloned()
            .ok_or_else(|| Error::new(ErrorKind::NotFound))
    }

    /// The forgotten counterpart of [`FileStore::get`]. A tombstone keeps no
    /// content, so the guard only checks its identifiers.
    pub(crate) fn get_tombstone(&self, reference: &RecordRef) -> Result<Tombstone> {
        validate_record_ref(reference)?;
        let tombstone = self
            .snapshot()?
            .tombstone(&reference.scope, &reference.id)
            .cloned()
            .ok_or_else(|| Error::new(ErrorKind::NotFound))?;
        self.guard().check(&GuardInput {
            fields: vec![
                GuardField {
                    name: "tombstone ID".into(),
                    value: tombstone.id.clone(),
                    opaque: true,
                },
                GuardField {
                    name: "tombstone scope namespace".into(),
                    value: tombstone.scope.namespace.clone(),
                    opaque: false,
                },
                GuardField {
                    name: "tombstone scope ID".into(),
                    value: tombstone.scope.id.clone(),
                    opaque: true,
                },
            ],
        })?;
        Ok(tombstone)
    }

    pub fn get_by_key(&self, key: &RecordKey) -> Result<Record> {
        validate_record_key(key)?;
        self.snapshot()?
            .records
            .iter()
            .filter(|record| {
                record.scope == key.scope && record.kind == key.kind && record.key == key.key
            })
            // An active record wins over an inactive one that kept the same key.
            .min_by_key(|record| record.state != RecordState::Active)
            .cloned()
            .ok_or_else(|| Error::new(ErrorKind::NotFound))
    }

    pub fn list(&self, request: &ListRequest) -> Result<RecordPage> {
        validate_list_request(request)?;
        let fingerprint = fingerprint_list(request);
        let cursor = decode_record_cursor(&request.cursor, &fingerprint)?;
        let after = match &cursor {
            Some(cursor) => Some((
                parse_timestamp(&cursor.updated_at)
                    .map_err(|_| Error::new(ErrorKind::InvalidCursor))?,
                cursor.id.clone(),
            )),
            None => None,
        };
        let snapshot = self.snapshot()?;
        if !request.all_scopes && request.scopes.is_empty() {
            return Ok(RecordPage::default());
        }
        let mut records: Vec<Record> = snapshot
            .records
            .into_iter()
            .filter(|record| {
                (request.all_scopes || request.scopes.contains(&record.scope))
                    && (request.kinds.is_empty() || request.kinds.contains(&record.kind))
                    && request
                        .labels
                        .iter()
                        .all(|label| record.labels.contains(label))
                    && (request.include_expired || !record.expired(request.now))
                    && list_lifecycle_matches(record, request)
            })
            .collect();
        records.sort_by(|left, right| {
            right
                .updated_at
                .cmp(&left.updated_at)
                .then_with(|| left.id.cmp(&right.id))
        });
        if let Some((updated_at, id)) = after {
            records.retain(|record| {
                record.updated_at < updated_at
                    || (record.updated_at == updated_at && record.id > id)
            });
        }
        records.truncate(request.limit + 1);
        let mut page = RecordPage {
            records,
            next_cursor: String::new(),
        };
        if page.records.len() > request.limit {
            page.records.truncate(request.limit);
            let last = page.records.last().expect("limit is positive");
            page.next_cursor =
                encode_record_cursor(&fingerprint, &format_timestamp(last.updated_at), &last.id)?;
        }
        Ok(page)
    }

    pub fn upsert(&self, request: &UpsertRequest) -> Result<Record> {
        self.upsert_inner(request, false)
    }

    /// [`FileStore::upsert`] that also rejects a record that would push the
    /// standing set over its token budget, checked under the store lock.
    pub(crate) fn upsert_within_standing_budget(&self, request: &UpsertRequest) -> Result<Record> {
        self.upsert_inner(request, true)
    }

    fn upsert_inner(&self, request: &UpsertRequest, enforce_standing: bool) -> Result<Record> {
        validate_upsert_request(request)?;
        crate::memory::guard::guard_record(self.guard(), &request.record)?;
        let input = request.record.clone();
        match request.expected_revision {
            None => self.write(|snapshot| {
                let mut desired = input;
                desired.revision = 1;
                if crate::memory::evidence::condition_changed(&desired) {
                    desired.state = RecordState::NeedsReview;
                    desired.state_reason =
                        "applicability condition changed; verify and rebind".into();
                }
                self.insert_new(snapshot, &desired, enforce_standing)
                    .map_err(|error| match error.kind {
                        ErrorKind::Conflict if error.conflict.is_none() => {
                            conflict_record(&desired.id, 0, 0)
                        }
                        _ => error,
                    })?;
                Ok(desired)
            }),
            Some(expected) => self.write(|snapshot| {
                let reference = RecordRef {
                    scope: input.scope.clone(),
                    id: input.id.clone(),
                };
                let existing = match self.lookup(snapshot, &reference)? {
                    Mutation::Forgotten(tombstone) => {
                        return Err(conflict_record(&input.id, expected, tombstone.revision));
                    }
                    Mutation::Active(record) => record,
                };
                if existing.revision != expected {
                    return Err(conflict_record(&input.id, expected, existing.revision));
                }
                if input.created_at != existing.created_at {
                    return Err(invalid_request("immutable record creation time"));
                }
                if input.updated_at < existing.updated_at {
                    return Err(invalid_request("record update time moved backwards"));
                }
                if existing.revision >= i64::MAX as u64 {
                    return Err(corrupt());
                }
                let mut desired = input;
                desired.revision = existing.revision + 1;
                // Ordinary updates omit the evidence namespace; only an explicit
                // binding (including an empty one) may replace existing evidence.
                if !desired
                    .metadata
                    .keys()
                    .any(|key| key.starts_with("evidence."))
                {
                    for (key, value) in &existing.metadata {
                        if key.starts_with("evidence.") {
                            desired.metadata.insert(key.clone(), value.clone());
                        }
                    }
                }
                if crate::memory::evidence::condition_changed(&desired) {
                    desired.state = RecordState::NeedsReview;
                    desired.state_reason =
                        "applicability condition changed; verify and rebind".into();
                }
                validate_record(&desired)?;
                self.replace_record(snapshot, &desired, enforce_standing)?;
                Ok(desired)
            }),
        }
    }

    /// Writes a new record file. Fails with `Conflict` when the id is taken by
    /// a record or a tombstone (`Corrupt` for an unreadable file), or when another active
    /// record holds the same non-empty (scope, kind, key).
    pub(super) fn insert_new(
        &self,
        snapshot: &Snapshot,
        record: &Record,
        enforce_standing: bool,
    ) -> Result<()> {
        Self::file_id(&record.id, "record ID")?;
        for taken in [
            self.path("records", &record.id, "md"),
            self.path("tombstones", &record.id, "json"),
        ] {
            if std::fs::symlink_metadata(&taken).is_ok() {
                if snapshot.is_invalid(&taken) {
                    return Err(corrupt());
                }
                return Err(conflict_record(&record.id, 0, 0));
            }
        }
        self.write_checked(snapshot, record, enforce_standing, None)
    }

    /// [`FileStore::insert_new`] for a replacement whose review retires
    /// `retiring_id` afterwards under the same lock. That record is still
    /// active when the replacement is written, so the duplicate-key and
    /// standing-budget checks ignore it.
    pub(super) fn insert_replacement(
        &self,
        snapshot: &Snapshot,
        record: &Record,
        enforce_standing: bool,
        retiring_id: &str,
    ) -> Result<()> {
        Self::file_id(&record.id, "record ID")?;
        for taken in [
            self.path("records", &record.id, "md"),
            self.path("tombstones", &record.id, "json"),
        ] {
            if std::fs::symlink_metadata(&taken).is_ok() {
                if snapshot.is_invalid(&taken) {
                    return Err(corrupt());
                }
                return Err(conflict_record(&record.id, 0, 0));
            }
        }
        self.write_checked(snapshot, record, enforce_standing, Some(retiring_id))
    }

    /// Rewrites the file of an existing active record.
    pub(super) fn replace_record(
        &self,
        snapshot: &Snapshot,
        record: &Record,
        enforce_standing: bool,
    ) -> Result<()> {
        self.check_writable(snapshot, record, enforce_standing, None)?;
        self.archive_current_revision(snapshot, record)?;
        self.put_record(record)
    }

    fn write_checked(
        &self,
        snapshot: &Snapshot,
        record: &Record,
        enforce_standing: bool,
        retiring_id: Option<&str>,
    ) -> Result<()> {
        self.check_writable(snapshot, record, enforce_standing, retiring_id)?;
        self.put_record(record)
    }

    /// Rejects an active record whose non-empty (scope, kind, key) another
    /// active record holds and, when asked, a standing-budget overrun. A
    /// retracted, superseded or needs-review record keeps its key without
    /// blocking a replacement, which is what a supersede or correct writes.
    pub(super) fn check_writable(
        &self,
        snapshot: &Snapshot,
        record: &Record,
        enforce_standing: bool,
        retiring_id: Option<&str>,
    ) -> Result<()> {
        self.check_evidence(snapshot, record, retiring_id)?;
        if !record.key.is_empty()
            && record.state == RecordState::Active
            && snapshot.records.iter().any(|other| {
                other.id != record.id
                    && Some(other.id.as_str()) != retiring_id
                    && other.state == RecordState::Active
                    && other.scope == record.scope
                    && other.kind == record.kind
                    && other.key == record.key
            })
        {
            return Err(Error::new(ErrorKind::Conflict));
        }
        if enforce_standing {
            check_standing_budget(snapshot, record, retiring_id)?;
        }
        Ok(())
    }

    /// Writes the tombstone, then removes the record file. A crash between
    /// the two leaves a record file that readers ignore and `open` removes.
    pub(super) fn tombstone_record(&self, tombstone: &Tombstone) -> Result<()> {
        self.put_tombstone(tombstone)?;
        super::remove_if_present(&self.path("records", &tombstone.id, "md"))
    }

    /// Writes the tombstone and removes the record's text copies: the record
    /// file, its revision history and the text of pending candidates that
    /// target it. Each copy is reported in the outcome.
    pub fn forget(&self, request: &StoreForgetRequest) -> Result<ForgetOutcome> {
        validate_store_forget_request(request)?;
        self.write(|snapshot| {
            let existing = match self.lookup(snapshot, &request.reference)? {
                Mutation::Forgotten(value) => {
                    if request.expected_revision != value.revision {
                        return Err(conflict_record(
                            &value.id,
                            request.expected_revision,
                            value.revision,
                        ));
                    }
                    if request.forgotten_at < value.updated_at {
                        return Err(invalid_request("forget time moved backwards"));
                    }
                    return Ok(ForgetOutcome {
                        tombstone: value,
                        cleanup: Vec::new(),
                    });
                }
                Mutation::Active(record) => record,
            };
            if request.expected_revision != existing.revision {
                return Err(conflict_record(
                    &existing.id,
                    request.expected_revision,
                    existing.revision,
                ));
            }
            if request.forgotten_at < existing.updated_at {
                return Err(invalid_request("forget time moved backwards"));
            }
            if existing.revision >= i64::MAX as u64 {
                return Err(corrupt());
            }
            let tombstone = Tombstone {
                id: existing.id.clone(),
                scope: existing.scope.clone(),
                revision: existing.revision + 1,
                created_at: existing.created_at,
                updated_at: request.forgotten_at,
                forgotten_at: request.forgotten_at,
            };
            self.tombstone_record(&tombstone)?;
            let cleanup = self.forget_cleanup(snapshot, &tombstone, request.forgotten_at)?;
            Ok(ForgetOutcome { tombstone, cleanup })
        })
    }

    /// The standing set of `user`, oldest first: every record recall injects
    /// when the budget allows.
    pub(crate) fn standing_records(&self, user: &Scope, now: DateTime<Utc>) -> Result<Vec<Record>> {
        Ok(standing_set(&self.snapshot()?, user, &[], false, now))
    }

    /// How much of the standing budget `user` uses, counting the same way
    /// recall injects: oldest first, stopping at the first record that does
    /// not fit.
    pub fn standing_usage(&self, user: &Scope, now: DateTime<Utc>) -> Result<StandingUsage> {
        let records = self.standing_records(user, now)?;
        let mut tokens = 0;
        let mut injected = 0;
        for record in &records {
            let cost = record_cost(None, record);
            if tokens + cost > MAX_STANDING_TOKENS {
                break;
            }
            tokens += cost;
            injected += 1;
        }
        Ok(StandingUsage {
            records: records.len(),
            tokens,
            budget: MAX_STANDING_TOKENS,
            omitted: records.len() - injected,
        })
    }
}
