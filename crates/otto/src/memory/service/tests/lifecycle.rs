//! Tests for the lifecycle service methods: validation before the store,
//! request forwarding, and the end-to-end state and time behavior.

use super::*;
use crate::memory::{
    RecordState, SetStateRequest, StoreSetStateRequest, StoreVerifyRequest, VerifyRequest,
};
use std::sync::Mutex;

fn reference_of(id: &str) -> RecordRef {
    RecordRef {
        scope: user_scope(),
        id: id.into(),
    }
}

fn set_state_request(state: RecordState, superseded_by: &str, reason: &str) -> SetStateRequest {
    SetStateRequest {
        reference: reference_of("r1"),
        expected_revision: 1,
        state,
        superseded_by: superseded_by.into(),
        reason: reason.into(),
    }
}

/// Answers `Unsupported` for every call and records the lifecycle requests.
struct RecordingStore {
    inner: AlternateStore,
    lists: Mutex<Vec<ListRequest>>,
    state_changes: Mutex<Vec<StoreSetStateRequest>>,
    verifications: Mutex<Vec<StoreVerifyRequest>>,
}

fn recording_service() -> (tempfile::TempDir, Arc<RecordingStore>, Service) {
    let (directory, store) = open_temp();
    let recording = Arc::new(RecordingStore {
        inner: AlternateStore(store),
        lists: Mutex::default(),
        state_changes: Mutex::default(),
        verifications: Mutex::default(),
    });
    let service = Service::from_store(recording.clone(), decide_default_policy);
    (directory, recording, service)
}

impl MemoryStore for RecordingStore {
    fn identity(&self) -> Result<crate::memory::StoreIdentity> {
        self.inner.identity()
    }
    fn close(&self) -> Result<()> {
        self.inner.close()
    }
    fn get(&self, reference: &RecordRef) -> Result<Record> {
        self.inner.get(reference)
    }
    fn get_by_key(&self, key: &RecordKey) -> Result<Record> {
        self.inner.get_by_key(key)
    }
    fn list(&self, request: &ListRequest) -> Result<super::super::super::RecordPage> {
        self.lists.lock().unwrap().push(request.clone());
        Ok(Default::default())
    }
    fn get_tombstone(&self, reference: &RecordRef) -> Result<Tombstone> {
        self.inner.get_tombstone(reference)
    }
    fn get_candidate(&self, reference: &crate::memory::CandidateRef) -> Result<Candidate> {
        self.inner.get_candidate(reference)
    }
    fn list_candidates(&self, r: &CandidateListRequest) -> Result<crate::memory::CandidatePage> {
        self.inner.list_candidates(r)
    }
    fn retrieve(&self, request: &RetrievalRequest) -> Result<crate::memory::RetrievalResult> {
        self.inner.retrieve(request)
    }
    fn upsert_within_standing_budget(&self, request: &UpsertRequest) -> Result<Record> {
        self.inner.upsert_within_standing_budget(request)
    }
    fn standing_usage(&self, scope: &Scope, now: DateTime<Utc>) -> Result<StandingUsage> {
        self.inner.standing_usage(scope, now)
    }
    fn standing_records(&self, scope: &Scope, now: DateTime<Utc>) -> Result<Vec<Record>> {
        self.inner.standing_records(scope, now)
    }
    fn forget(&self, request: &StoreForgetRequest) -> Result<crate::memory::ForgetOutcome> {
        self.inner.forget(request)
    }
    fn history(&self, reference: &RecordRef) -> Result<RecordHistory> {
        self.inner.history(reference)
    }
    fn restore(&self, request: &StoreRestoreRequest) -> Result<Record> {
        self.inner.restore(request)
    }
    fn set_state(&self, request: &StoreSetStateRequest) -> Result<Record> {
        self.state_changes.lock().unwrap().push(request.clone());
        unsupported()
    }
    fn verify(&self, request: &StoreVerifyRequest) -> Result<Record> {
        self.verifications.lock().unwrap().push(request.clone());
        unsupported()
    }
    fn review_within_standing_budget(&self, r: &StoreReviewRequest) -> Result<ReviewResult> {
        self.inner.review_within_standing_budget(r)
    }
    fn propose(&self, candidates: &[Candidate]) -> Result<Vec<Candidate>> {
        self.inner.propose(candidates)
    }
    fn check_proposals(&self, candidates: &[Candidate]) -> Result<()> {
        self.inner.check_proposals(candidates)
    }
}

/// Fails if the service validates after calling the store: the recording
/// store would hold the request, and the error would be `Unsupported`.
#[test]
fn set_state_validation_errors_are_invalid_request_before_the_store() {
    let (_directory, recording, service) = recording_service();
    let too_long_reason = "x".repeat(crate::memory::MAX_STATE_REASON_BYTES + 1);
    let mut zero_revision = set_state_request(RecordState::Retracted, "", "");
    zero_revision.expected_revision = 0;
    let invalid = [
        (
            "superseded without replacement",
            set_state_request(RecordState::Superseded, "", ""),
        ),
        (
            "replacement is the record itself",
            set_state_request(RecordState::Superseded, "r1", ""),
        ),
        (
            "replacement is not an ID",
            set_state_request(RecordState::Superseded, "a b", ""),
        ),
        (
            "replacement on a non-superseded state",
            set_state_request(RecordState::Retracted, "r2", ""),
        ),
        (
            "reason over the limit",
            set_state_request(RecordState::Retracted, "", &too_long_reason),
        ),
        ("revision zero", zero_revision),
    ];
    for (name, request) in invalid {
        let error = service.set_state(&request).expect_err(name);
        assert!(error.is(ErrorKind::InvalidRequest), "{name}: {error}");
    }
    assert!(recording.state_changes.lock().unwrap().is_empty());
}

#[test]
fn verify_validation_errors_are_invalid_request_before_the_store() {
    let (_directory, recording, service) = recording_service();
    let error = service
        .verify(&VerifyRequest {
            reference: reference_of("r1"),
            expected_revision: 0,
            review_at: None,
        })
        .expect_err("revision zero");
    assert!(error.is(ErrorKind::InvalidRequest), "{error}");
    let error = service
        .verify(&VerifyRequest {
            reference: reference_of("bad id"),
            expected_revision: 1,
            review_at: None,
        })
        .expect_err("record ID with a space");
    assert!(error.is(ErrorKind::InvalidRequest), "{error}");
    assert!(recording.verifications.lock().unwrap().is_empty());
}

#[test]
fn valid_set_state_and_verify_reach_the_store_with_every_field_and_a_timestamp() {
    let (_directory, recording, service) = recording_service();
    let before = Utc::now();
    let error = service
        .set_state(&set_state_request(RecordState::Superseded, "r2", "newer"))
        .expect_err("the recording store answers Unsupported");
    assert!(error.is(ErrorKind::Unsupported), "{error}");
    let deadline = before + chrono::Duration::days(7);
    let error = service
        .verify(&VerifyRequest {
            reference: reference_of("r1"),
            expected_revision: 4,
            review_at: Some(deadline),
        })
        .expect_err("the recording store answers Unsupported");
    assert!(error.is(ErrorKind::Unsupported), "{error}");

    let changes = recording.state_changes.lock().unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].state, RecordState::Superseded);
    assert_eq!(changes[0].superseded_by, "r2");
    assert_eq!(changes[0].reason, "newer");
    assert_eq!(changes[0].expected_revision, 1);
    assert!(changes[0].changed_at >= before);
    let verifications = recording.verifications.lock().unwrap();
    assert_eq!(verifications.len(), 1);
    assert_eq!(verifications[0].expected_revision, 4);
    assert_eq!(verifications[0].review_at, Some(deadline));
    assert!(verifications[0].verified_at >= before);
}

#[test]
fn search_with_states_forwards_them_to_the_store_list_on_an_empty_query() {
    let (_directory, recording, service) = recording_service();
    service
        .search(&SearchRequest {
            scopes: vec![user_scope()],
            states: vec![RecordState::NeedsReview, RecordState::Retracted],
            limit: 10,
            token_budget: 1000,
            now: Utc::now(),
            ..SearchRequest::default()
        })
        .expect("an empty query lists");
    service
        .search(&SearchRequest {
            scopes: vec![user_scope()],
            limit: 10,
            token_budget: 1000,
            now: Utc::now(),
            ..SearchRequest::default()
        })
        .expect("an empty query lists");
    let lists = recording.lists.lock().unwrap();
    assert_eq!(
        lists[0].states,
        vec![RecordState::NeedsReview, RecordState::Retracted]
    );
    assert!(lists[1].states.is_empty());
}

// Needs the store implementation of `review_at` on write.
#[test]
fn remember_with_review_at_stores_it_and_a_plain_remember_has_none() {
    let (_directory, service) = service();
    let deadline = Utc::now() + chrono::Duration::days(7);
    let reviewed = service
        .remember(&RememberRequest {
            review_at: Some(deadline),
            ..remember(&user_scope(), "editor", "prefers vim")
        })
        .unwrap();
    assert_eq!(
        reviewed.review_at.map(|t| t.timestamp()),
        Some(deadline.timestamp())
    );
    let stored = service.get(&reference_of(&reviewed.id)).unwrap();
    assert_eq!(
        stored.review_at.map(|t| t.timestamp()),
        Some(deadline.timestamp())
    );
    assert_eq!(stored.state, RecordState::Active);
    assert!(stored.verified_at.is_none());
    let plain = service
        .remember(&remember(&user_scope(), "shell", "prefers fish"))
        .unwrap();
    assert!(plain.review_at.is_none());
}

// Needs the store implementation of `set_state`, `verify` and the list state filter.
#[test]
fn state_changes_keep_updated_at_and_verify_sets_verified_at_only() {
    let (_directory, service) = service();
    let record = service
        .remember(&remember(&user_scope(), "editor", "prefers vim"))
        .unwrap();
    let flagged = service
        .set_state(&SetStateRequest {
            reference: reference_of(&record.id),
            expected_revision: record.revision,
            state: RecordState::NeedsReview,
            superseded_by: String::new(),
            reason: "check".into(),
        })
        .unwrap();
    assert_eq!(flagged.revision, record.revision + 1);
    assert_eq!(flagged.updated_at, record.updated_at);
    assert_eq!(flagged.state_reason, "check");
    assert!(flagged.verified_at.is_none());

    let search = |states: Vec<RecordState>| {
        service
            .search(&SearchRequest {
                scopes: vec![user_scope()],
                states,
                limit: 10,
                token_budget: 1000,
                now: Utc::now(),
                ..SearchRequest::default()
            })
            .unwrap()
            .records
    };
    assert!(search(Vec::new()).is_empty());
    assert_eq!(search(vec![RecordState::NeedsReview]).len(), 1);

    let verified = service
        .verify(&VerifyRequest {
            reference: reference_of(&record.id),
            expected_revision: flagged.revision,
            review_at: None,
        })
        .unwrap();
    assert_eq!(verified.state, RecordState::Active);
    assert!(verified.verified_at.is_some());
    assert_eq!(verified.updated_at, record.updated_at);
    assert_eq!(search(Vec::new()).len(), 1);
}

// Needs the store implementation of recall exclusion.
#[test]
fn recall_excludes_a_record_whose_review_deadline_has_passed() {
    let (_directory, service) = service();
    let record = service
        .remember(&RememberRequest {
            review_at: Some(Utc::now() + chrono::Duration::seconds(1)),
            ..remember(&user_scope(), "editor", "prefers vim")
        })
        .unwrap();
    let search_at = |now: DateTime<Utc>| {
        service
            .search(&SearchRequest {
                query: "vim".into(),
                scopes: vec![user_scope()],
                limit: 10,
                token_budget: 1000,
                now,
                ..SearchRequest::default()
            })
            .unwrap()
            .records
    };
    assert_eq!(search_at(Utc::now()).len(), 1);
    let due = search_at(Utc::now() + chrono::Duration::days(1));
    assert!(due.iter().all(|found| found.id != record.id), "{due:?}");
}
