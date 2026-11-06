//! Record reads and mutations, and the standing set's token budget.

use chrono::{DateTime, Utc};

use super::cursor::{decode_record_cursor, encode_record_cursor, fingerprint_list};
use super::retrieve::record_cost;
use super::{FileStore, Snapshot};
use crate::memory::codec::{format_timestamp, parse_timestamp};
use crate::memory::validate::{
    validate_list_request, validate_record_key, validate_record_ref, validate_store_forget_request,
    validate_upsert_request,
};
use crate::memory::{
    Error, ErrorKind, ForgetOutcome, GuardField, GuardInput, ListRequest, MAX_STANDING_TOKENS,
    NAMESPACE_USER, Record, RecordKey, RecordPage, RecordRef, Result, Scope, StandingUsage,
    StoreForgetRequest, Tombstone, UpsertRequest, invalid_request,
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
        && record.expires_at.is_none_or(|expires| expires > now)
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
                && (include_expired || record.expires_at.is_none_or(|expires| expires > now))
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
/// over the budget. The record being replaced, if any, is not counted.
pub(super) fn check_standing_budget(snapshot: &Snapshot, record: &Record) -> Result<()> {
    let now = Utc::now();
    if !standing_eligible(record, now) {
        return Ok(());
    }
    let used: usize = standing_set(snapshot, &record.scope, &[], false, now)
        .iter()
        .filter(|existing| existing.id != record.id)
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
    pub fn get_tombstone(&self, reference: &RecordRef) -> Result<Tombstone> {
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
            .find(|record| {
                record.scope == key.scope && record.kind == key.kind && record.key == key.key
            })
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
                    && (request.include_expired
                        || record
                            .expires_at
                            .is_none_or(|expires| expires > request.now))
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
    pub fn upsert_within_standing_budget(&self, request: &UpsertRequest) -> Result<Record> {
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
        self.write_checked(snapshot, record, enforce_standing)
    }

    /// Rewrites the file of an existing active record.
    pub(super) fn replace_record(
        &self,
        snapshot: &Snapshot,
        record: &Record,
        enforce_standing: bool,
    ) -> Result<()> {
        self.check_writable(snapshot, record, enforce_standing)?;
        self.archive_current_revision(snapshot, record)?;
        self.put_record(record)
    }

    fn write_checked(
        &self,
        snapshot: &Snapshot,
        record: &Record,
        enforce_standing: bool,
    ) -> Result<()> {
        self.check_writable(snapshot, record, enforce_standing)?;
        self.put_record(record)
    }

    /// Rejects a duplicate non-empty key and, when asked, a standing-budget overrun.
    fn check_writable(
        &self,
        snapshot: &Snapshot,
        record: &Record,
        enforce_standing: bool,
    ) -> Result<()> {
        if !record.key.is_empty()
            && snapshot.records.iter().any(|other| {
                other.id != record.id
                    && other.scope == record.scope
                    && other.kind == record.kind
                    && other.key == record.key
            })
        {
            return Err(Error::new(ErrorKind::Conflict));
        }
        if enforce_standing {
            check_standing_budget(snapshot, record)?;
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
    pub fn standing_records(&self, user: &Scope, now: DateTime<Utc>) -> Result<Vec<Record>> {
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
