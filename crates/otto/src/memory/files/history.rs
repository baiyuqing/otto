//! Revision history: `history/<id>/<revision>.md` holds every superseded
//! revision of a record as the former record file, byte for byte.
//!
//! The current revision lives in `records/<id>.md` only; revision 1 of a
//! record that was never updated has no history file. A history file is
//! written under the store lock before the record file it supersedes is
//! replaced, so a crash between the two leaves a history file that the next
//! replacement rewrites with the same content.
//!
//! Forget removes the directory after the tombstone is written; `open`
//! removes the history of every tombstoned id and tombstones a record whose
//! file was deleted by hand while its history survived.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::path::PathBuf;

use chrono::{DateTime, Utc};

use super::format::{MAX_FILE_BYTES, parse_record, valid_file_id};
use super::records::Mutation;
use super::{FileStore, Snapshot, ensure_directory, publish_path};
use crate::memory::guard::guard_record;
use crate::memory::validate::{validate_candidate, validate_record, validate_record_ref};
use crate::memory::{
    CandidateState, CopyStatus, Error, ErrorKind, ForgetCleanupEntry, Origin, Provenance,
    RESTORED_FROM_REVISION_METADATA, Record, RecordHistory, RecordRef, Result, SkippedRevision,
    StoreRestoreRequest, Tombstone, invalid_request,
};

/// Why one history file did not yield a record.
enum RevisionProblem {
    Missing,
    Invalid(String),
}

impl FileStore {
    fn history_dir(&self, id: &str) -> PathBuf {
        self.root.join("history").join(id)
    }

    /// The file stems of `history/<id>/` that are canonical decimal revision
    /// numbers, ascending. An absent directory yields an empty list.
    fn history_revisions(&self, id: &str) -> Result<Vec<u64>> {
        let listing = match std::fs::read_dir(self.history_dir(id)) {
            Ok(listing) => listing,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(_) => return Err(Error::new(ErrorKind::Unavailable)),
        };
        let mut revisions = Vec::new();
        for entry in listing {
            let name = entry
                .map_err(|_| Error::new(ErrorKind::Unavailable))?
                .file_name()
                .to_string_lossy()
                .into_owned();
            let Some(stem) = name.strip_suffix(".md") else {
                continue;
            };
            if let Ok(revision) = stem.parse::<u64>()
                && revision.to_string() == stem
            {
                revisions.push(revision);
            }
        }
        revisions.sort_unstable();
        Ok(revisions)
    }

    /// Reads and parses `history/<id>/<revision>.md`.
    fn load_revision(
        &self,
        id: &str,
        revision: u64,
    ) -> std::result::Result<Record, RevisionProblem> {
        let path = self.history_dir(id).join(format!("{revision}.md"));
        let mut bytes = Vec::new();
        match File::open(&path)
            .and_then(|file| file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes))
        {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(RevisionProblem::Missing);
            }
            Err(_) => return Err(RevisionProblem::Invalid("file cannot be read".into())),
        }
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return Err(RevisionProblem::Invalid(
                "file is larger than 128 KiB".into(),
            ));
        }
        let Ok(content) = String::from_utf8(bytes) else {
            return Err(RevisionProblem::Invalid("file is not UTF-8".into()));
        };
        let record = parse_record(id, &content).map_err(RevisionProblem::Invalid)?;
        if record.revision != revision {
            return Err(RevisionProblem::Invalid(
                "revision differs from file name".into(),
            ));
        }
        guard_record(self.guard(), &record)
            .map_err(|error| RevisionProblem::Invalid(error.to_string()))?;
        Ok(record)
    }

    /// Copies the live file of `replacement.id` to `history/<id>/<revision>.md`
    /// before the caller replaces it. Caller holds the store lock. A live file
    /// that disappeared since the snapshot has nothing to copy.
    pub(super) fn archive_current_revision(
        &self,
        snapshot: &Snapshot,
        replacement: &Record,
    ) -> Result<()> {
        let Some(live) = snapshot.record(&replacement.scope, &replacement.id) else {
            return Ok(());
        };
        let bytes = match std::fs::read(self.path("records", &live.id, "md")) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(_) => return Err(Error::new(ErrorKind::Unavailable)),
        };
        let content = String::from_utf8(bytes).map_err(|_| Error::new(ErrorKind::Corrupt))?;
        let directory = self.history_dir(&live.id);
        ensure_directory(&directory)?;
        publish_path(&directory.join(format!("{}.md", live.revision)), &content)
    }

    /// The revisions of an active record, ascending, the current record last.
    pub fn history(&self, reference: &RecordRef) -> Result<RecordHistory> {
        validate_record_ref(reference)?;
        let snapshot = self.snapshot()?;
        let current = snapshot
            .record(&reference.scope, &reference.id)
            .cloned()
            .ok_or_else(|| Error::new(ErrorKind::NotFound))?;
        let mut history = RecordHistory::default();
        for revision in self.history_revisions(&reference.id)? {
            match self.load_revision(&reference.id, revision) {
                Ok(record) => history.revisions.push(record),
                // Removed between the listing and the read.
                Err(RevisionProblem::Missing) => {}
                Err(RevisionProblem::Invalid(reason)) => history.skipped.push(SkippedRevision {
                    file: format!("history/{}/{revision}.md", reference.id),
                    reason,
                }),
            }
        }
        history.revisions.push(current);
        Ok(history)
    }

    /// Writes a new revision whose content fields equal `request.revision`.
    pub fn restore(&self, request: &StoreRestoreRequest) -> Result<Record> {
        validate_record_ref(&request.reference)?;
        if request.revision >= request.expected_revision {
            return Err(invalid_request("restore revision"));
        }
        self.write(|snapshot| {
            let current = match self.lookup(snapshot, &request.reference)? {
                Mutation::Forgotten(tombstone) => {
                    return Err(Error::conflict(
                        "record",
                        &tombstone.id,
                        request.expected_revision,
                        tombstone.revision,
                    ));
                }
                Mutation::Active(record) => record,
            };
            if current.revision != request.expected_revision {
                return Err(Error::conflict(
                    "record",
                    &current.id,
                    request.expected_revision,
                    current.revision,
                ));
            }
            if request.restored_at < current.updated_at {
                return Err(invalid_request("record update time moved backwards"));
            }
            if current.revision >= i64::MAX as u64 {
                return Err(Error::new(ErrorKind::Corrupt));
            }
            let restored = match self.load_revision(&current.id, request.revision) {
                Ok(record) => record,
                Err(RevisionProblem::Missing) => return Err(Error::new(ErrorKind::NotFound)),
                Err(RevisionProblem::Invalid(_)) => return Err(Error::new(ErrorKind::Corrupt)),
            };
            let mut metadata = restored.metadata;
            metadata.insert(
                RESTORED_FROM_REVISION_METADATA.into(),
                request.revision.to_string(),
            );
            let desired = Record {
                id: current.id.clone(),
                scope: current.scope.clone(),
                kind: restored.kind,
                key: restored.key,
                text: restored.text,
                labels: restored.labels,
                metadata,
                source: Provenance {
                    origin: Some(Origin::Human),
                    ..Provenance::default()
                },
                confidence: restored.confidence,
                revision: current.revision + 1,
                created_at: current.created_at,
                updated_at: request.restored_at,
                expires_at: restored.expires_at,
                // Restored content starts active and unverified.
                ..Record::default()
            };
            validate_record(&desired)?;
            guard_record(self.guard(), &desired)?;
            self.replace_record(snapshot, &desired, true)?;
            Ok(desired)
        })
    }

    /// Caller holds the store lock and has written `tombstone`. Removes the
    /// record's history directory and the text of pending candidates that
    /// target the record, and reports each copy.
    pub(super) fn forget_cleanup(
        &self,
        snapshot: &Snapshot,
        tombstone: &Tombstone,
        forgotten_at: DateTime<Utc>,
    ) -> Result<Vec<ForgetCleanupEntry>> {
        let mut cleanup = vec![ForgetCleanupEntry {
            copy: format!("records/{}.md", tombstone.id),
            status: CopyStatus::Removed,
            detail: "record file removed".into(),
        }];
        let removed_files = self.remove_history_directory(&tombstone.id)?;
        cleanup.push(ForgetCleanupEntry {
            copy: format!("history/{}/", tombstone.id),
            status: CopyStatus::Removed,
            detail: if removed_files == 0 {
                "no history files".into()
            } else {
                format!("{removed_files} revision files removed")
            },
        });
        for candidate in &snapshot.candidates {
            if candidate.state != CandidateState::Pending
                || candidate.target_id != tombstone.id
                || candidate.proposed.scope != tombstone.scope
            {
                continue;
            }
            let rejected = super::candidates::reject_pending_candidate(
                candidate,
                forgotten_at,
                Some(Origin::Human),
            );
            validate_candidate(&rejected).map_err(|_| Error::new(ErrorKind::Corrupt))?;
            self.put_candidate(&rejected)?;
            cleanup.push(ForgetCleanupEntry {
                copy: format!("candidates/{}.json", candidate.id),
                status: CopyStatus::Removed,
                detail: format!(
                    "pending {} proposal rejected, text cleared, receipt archived",
                    candidate.action.as_str()
                ),
            });
        }
        Ok(cleanup)
    }

    /// Removes `history/<id>/` and returns the number of entries it held.
    fn remove_history_directory(&self, id: &str) -> Result<usize> {
        let directory = self.history_dir(id);
        let held = match std::fs::read_dir(&directory) {
            Ok(listing) => listing.count(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(_) => return Err(Error::new(ErrorKind::Unavailable)),
        };
        match std::fs::remove_dir_all(&directory) {
            Ok(()) => Ok(held),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(held),
            Err(_) => Err(Error::new(ErrorKind::Unavailable)),
        }
    }

    /// Run by `open` under the store lock. Removes the history of every
    /// tombstoned id, and tombstones each id whose history directory survives
    /// without a record file or tombstone (the live file was deleted by hand):
    /// the deleted live revision was one above the highest history revision.
    pub(super) fn recover_history(&self, snapshot: &Snapshot) -> Result<()> {
        for tombstone in &snapshot.tombstones {
            self.remove_history_directory(&tombstone.id)?;
        }
        let listing = std::fs::read_dir(self.root.join("history"))
            .map_err(|_| Error::new(ErrorKind::Unavailable))?;
        let mut orphan_ids: BTreeMap<String, ()> = BTreeMap::new();
        for entry in listing {
            let entry = entry.map_err(|_| Error::new(ErrorKind::Unavailable))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_directory = entry.file_type().is_ok_and(|kind| kind.is_dir());
            if !is_directory
                || !valid_file_id(&name)
                || std::fs::symlink_metadata(self.path("records", &name, "md")).is_ok()
                || std::fs::symlink_metadata(self.path("tombstones", &name, "json")).is_ok()
            {
                continue;
            }
            orphan_ids.insert(name, ());
        }
        for id in orphan_ids.into_keys() {
            let Some(highest) = self.history_revisions(&id)?.last().copied() else {
                continue;
            };
            let Ok(newest) = self.load_revision(&id, highest) else {
                continue;
            };
            let now = Utc::now();
            self.put_tombstone(&Tombstone {
                id: id.clone(),
                scope: newest.scope,
                revision: highest + 2,
                created_at: newest.created_at,
                updated_at: now,
                forgotten_at: now,
            })?;
            self.remove_history_directory(&id)?;
        }
        Ok(())
    }
}
