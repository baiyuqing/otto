//! Pending candidates and their review.
//!
//! A review runs entirely under the store lock. The record or tombstone a
//! decision authorizes is written first and the decided candidate second, so
//! an interrupted review leaves the candidate pending and the repeated
//! accept fails on the already-written record instead of writing twice.

use super::FileStore;
use super::cursor::{decode_record_cursor, encode_record_cursor, fingerprint_candidates};
use super::records::Mutation;
use crate::memory::codec::{format_timestamp, parse_timestamp};
use crate::memory::guard::{guard_candidate, guard_record};
use crate::memory::validate::{
    validate_candidate, validate_candidate_list_request, validate_candidate_ref,
    validate_proposal_batch, validate_record, validate_store_review_request, validate_tombstone,
};
use crate::memory::{
    Candidate, CandidateAction, CandidateListRequest, CandidatePage, CandidateRef, CandidateState,
    Error, ErrorKind, MAX_CANDIDATE_BATCH, MAX_CANDIDATE_BATCH_BYTES, MAX_LABELS,
    MAX_METADATA_ENTRIES, MAX_PROVENANCE_MESSAGE_IDS, Record, RecordRef, Result, ReviewDecision,
    ReviewResult, Scope, StoreReviewRequest, Tombstone, invalid_request,
};

fn corrupt() -> Error {
    Error::new(ErrorKind::Corrupt)
}

/// Rejects a batch whose total text would exceed the wire budget before any of
/// it is encoded, so an oversized proposal cannot reach the disk.
fn preflight_batch_shape(candidates: &[Candidate]) -> Result<()> {
    if candidates.is_empty() || candidates.len() > MAX_CANDIDATE_BATCH {
        return Err(invalid_request("candidate batch count"));
    }
    let mut total = 0usize;
    let mut add = |value: &str| -> Result<()> {
        if value.len() > MAX_CANDIDATE_BATCH_BYTES.saturating_sub(total) {
            return Err(invalid_request("candidate batch bytes"));
        }
        total += value.len();
        Ok(())
    };
    for candidate in candidates {
        if candidate.proposed.labels.len() > MAX_LABELS
            || candidate.proposed.metadata.len() > MAX_METADATA_ENTRIES
            || candidate.proposed.source.message_ids.len() > MAX_PROVENANCE_MESSAGE_IDS
        {
            return Err(invalid_request("candidate batch collections"));
        }
        for value in [
            &candidate.id,
            &candidate.target_id,
            &candidate.reason,
            &candidate.proposed.scope.namespace,
            &candidate.proposed.scope.id,
            &candidate.proposed.kind,
            &candidate.proposed.key,
            &candidate.proposed.text,
            &candidate.proposed.source.session_id,
            &candidate.proposed.source.observation_id,
        ] {
            add(value)?;
        }
        for label in &candidate.proposed.labels {
            add(label)?;
        }
        for (key, value) in &candidate.proposed.metadata {
            add(key)?;
            add(value)?;
        }
        for message in &candidate.proposed.source.message_ids {
            add(message)?;
        }
    }
    Ok(())
}

pub(super) fn cleared_proposed(scope: &Scope) -> Record {
    Record {
        scope: scope.clone(),
        ..Record::default()
    }
}

fn accepted_record_content(candidate: &Candidate, request: &StoreReviewRequest) -> Record {
    let mut value = request
        .edited
        .clone()
        .unwrap_or_else(|| candidate.proposed.clone());
    value.scope = candidate.proposed.scope.clone();
    value.source.decision_at = Some(request.decided_at);
    value.source.decision_source = request.decision_source;
    value
}

impl FileStore {
    /// Records pending candidates, one file each. Every candidate lands as
    /// `pending`: a proposal never becomes a record without a human review.
    pub fn propose(&self, candidates: &[Candidate]) -> Result<Vec<Candidate>> {
        self.check_proposals(candidates)?;
        self.write(|_| {
            for candidate in candidates {
                Self::file_id(&candidate.id, "candidate ID")?;
                if std::fs::symlink_metadata(self.path("candidates", &candidate.id, "json")).is_ok()
                {
                    return Err(Error::new(ErrorKind::Conflict));
                }
            }
            for candidate in candidates {
                self.put_candidate(candidate)?;
            }
            Ok(candidates.to_vec())
        })
    }

    /// Runs every check [`FileStore::propose`] runs before it writes: batch
    /// shape, proposal validation, duplicate ids, and the content guard.
    pub fn check_proposals(&self, candidates: &[Candidate]) -> Result<()> {
        self.ensure_open()?;
        preflight_batch_shape(candidates)?;
        validate_proposal_batch(candidates)?;
        let mut seen = std::collections::BTreeSet::new();
        for candidate in candidates {
            if !candidate.proposed.source.observation_id.is_empty() {
                return Err(invalid_request("candidate observation"));
            }
            if !seen.insert(candidate.id.clone()) {
                return Err(Error::new(ErrorKind::Conflict));
            }
            guard_candidate(self.guard(), candidate)?;
        }
        Ok(())
    }

    pub fn get_candidate(&self, reference: &CandidateRef) -> Result<Candidate> {
        validate_candidate_ref(reference)?;
        self.snapshot()?
            .candidate(&reference.scope, &reference.id)
            .cloned()
            .ok_or_else(|| Error::new(ErrorKind::NotFound))
    }

    pub fn list_candidates(&self, request: &CandidateListRequest) -> Result<CandidatePage> {
        validate_candidate_list_request(request)?;
        let fingerprint = fingerprint_candidates(request);
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
        if request.scopes.is_empty() {
            return Ok(CandidatePage::default());
        }
        let mut candidates: Vec<Candidate> = snapshot
            .candidates
            .into_iter()
            .filter(|candidate| {
                request.scopes.contains(&candidate.proposed.scope)
                    && (request.states.is_empty() || request.states.contains(&candidate.state))
            })
            .collect();
        candidates.sort_by(|left, right| {
            right
                .created_at
                .cmp(&left.created_at)
                .then_with(|| left.id.cmp(&right.id))
        });
        if let Some((created_at, id)) = after {
            candidates.retain(|candidate| {
                candidate.created_at < created_at
                    || (candidate.created_at == created_at && candidate.id > id)
            });
        }
        candidates.truncate(request.limit + 1);
        let mut page = CandidatePage {
            candidates,
            next_cursor: String::new(),
        };
        if page.candidates.len() > request.limit {
            page.candidates.truncate(request.limit);
            let last = page.candidates.last().expect("limit is positive");
            page.next_cursor =
                encode_record_cursor(&fingerprint, &format_timestamp(last.created_at), &last.id)?;
        }
        Ok(page)
    }

    /// Applies a human decision to one pending candidate.
    pub fn review(&self, request: &StoreReviewRequest) -> Result<ReviewResult> {
        self.review_inner(request, false)
    }

    /// [`FileStore::review`] that also rejects an accept whose record would
    /// push the standing set over its token budget. A rejection leaves the
    /// candidate pending.
    pub fn review_within_standing_budget(
        &self,
        request: &StoreReviewRequest,
    ) -> Result<ReviewResult> {
        self.review_inner(request, true)
    }

    fn review_inner(
        &self,
        request: &StoreReviewRequest,
        enforce_standing: bool,
    ) -> Result<ReviewResult> {
        validate_candidate_ref(&request.reference)?;
        self.write(|snapshot| {
            let candidate =
                match snapshot.candidate(&request.reference.scope, &request.reference.id) {
                    Some(candidate) => candidate.clone(),
                    None if snapshot.is_invalid(&self.path(
                        "candidates",
                        &request.reference.id,
                        "json",
                    )) =>
                    {
                        return Err(corrupt());
                    }
                    None => return Err(Error::new(ErrorKind::NotFound)),
                };
            if candidate.state != CandidateState::Pending {
                return Err(Error::new(ErrorKind::Conflict));
            }
            if let Some(edited) = &request.edited
                && (edited.labels.len() > MAX_LABELS
                    || edited.metadata.len() > MAX_METADATA_ENTRIES
                    || edited.source.message_ids.len() > MAX_PROVENANCE_MESSAGE_IDS)
            {
                return Err(invalid_request("review edit collections"));
            }
            validate_store_review_request(request, &candidate)?;
            if let Some(edited) = &request.edited {
                guard_record(self.guard(), edited)?;
            }

            let accepts_existing = request.decision == ReviewDecision::Accept
                && candidate.action != CandidateAction::Create;
            let target = if accepts_existing {
                let reference = RecordRef {
                    scope: candidate.proposed.scope.clone(),
                    id: candidate.target_id.clone(),
                };
                let record = match self.lookup(snapshot, &reference)? {
                    Mutation::Forgotten(tombstone) => {
                        return Err(Error::conflict(
                            "record",
                            &candidate.target_id,
                            candidate.base_revision,
                            tombstone.revision,
                        ));
                    }
                    Mutation::Active(record) => record,
                };
                let expected = request.target_revision.unwrap_or(candidate.base_revision);
                if record.revision != expected {
                    return Err(Error::conflict(
                        "record",
                        &candidate.target_id,
                        expected,
                        record.revision,
                    ));
                }
                Some(record)
            } else {
                None
            };

            let mut decided = candidate.clone();
            decided.state = CandidateState::Rejected;
            decided.decided_at = Some(request.decided_at);
            decided.decision_source = request.decision_source;
            decided.reason = String::new();
            decided.proposed = cleared_proposed(&candidate.proposed.scope);

            let mut desired_record: Option<Record> = None;
            let mut desired_tombstone: Option<Tombstone> = None;
            if request.decision == ReviewDecision::Accept {
                decided.state = CandidateState::Accepted;
                match candidate.action {
                    CandidateAction::Create => {
                        let mut desired = accepted_record_content(&candidate, request);
                        desired.id = request.result_record_id.clone();
                        desired.revision = 1;
                        desired.created_at = request.decided_at;
                        desired.updated_at = request.decided_at;
                        validate_record(&desired)?;
                        guard_record(self.guard(), &desired)?;
                        decided.result_record_id = desired.id.clone();
                        decided.result_revision = desired.revision;
                        desired_record = Some(desired);
                    }
                    CandidateAction::Update => {
                        let target = target.as_ref().expect("update reads its target");
                        if request.decided_at < target.updated_at
                            || target.revision >= i64::MAX as u64
                        {
                            return Err(invalid_request("review decision time"));
                        }
                        let mut desired = accepted_record_content(&candidate, request);
                        desired.id = target.id.clone();
                        desired.revision = target.revision + 1;
                        desired.created_at = target.created_at;
                        desired.updated_at = request.decided_at;
                        validate_record(&desired)?;
                        guard_record(self.guard(), &desired)?;
                        decided.result_revision = desired.revision;
                        desired_record = Some(desired);
                    }
                    CandidateAction::Forget => {
                        let target = target.as_ref().expect("forget reads its target");
                        if request.decided_at < target.updated_at
                            || target.revision >= i64::MAX as u64
                        {
                            return Err(invalid_request("review decision time"));
                        }
                        let desired = Tombstone {
                            id: target.id.clone(),
                            scope: target.scope.clone(),
                            revision: target.revision + 1,
                            created_at: target.created_at,
                            updated_at: request.decided_at,
                            forgotten_at: request.decided_at,
                        };
                        validate_tombstone(&desired)?;
                        decided.result_revision = desired.revision;
                        desired_tombstone = Some(desired);
                    }
                }
            }
            if validate_candidate(&decided).is_err() {
                return Err(corrupt());
            }
            guard_candidate(self.guard(), &decided)?;

            if let Some(record) = &desired_record {
                if candidate.action == CandidateAction::Create {
                    self.insert_new(snapshot, record, enforce_standing)?;
                } else {
                    self.replace_record(snapshot, record, enforce_standing)?;
                }
            }
            if let Some(tombstone) = &desired_tombstone {
                self.tombstone_record(tombstone)?;
            }
            self.put_candidate(&decided)?;
            Ok(ReviewResult {
                candidate: decided,
                record: desired_record,
                tombstone: desired_tombstone,
            })
        })
    }
}
