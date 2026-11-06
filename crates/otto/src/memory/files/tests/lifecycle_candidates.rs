//! Review of the lifecycle candidate actions: retract, needs_review, supersede
//! and correct.

use super::lifecycle::{history_file, list_request, retrieved_ids, stored_files};
use super::*;
use crate::memory::{CORRECTS_METADATA, MAX_STANDING_TOKENS, RecordState};

const STATE_CHANGE_ACTIONS: [CandidateAction; 2] =
    [CandidateAction::Retract, CandidateAction::NeedsReview];
const REPLACEMENT_ACTIONS: [CandidateAction; 2] =
    [CandidateAction::Supersede, CandidateAction::Correct];
const ALL_LIFECYCLE_ACTIONS: [CandidateAction; 4] = [
    CandidateAction::Retract,
    CandidateAction::NeedsReview,
    CandidateAction::Supersede,
    CandidateAction::Correct,
];

fn propose_against(store: &FileStore, scope: &Scope, id: &str, action: CandidateAction) {
    store
        .propose(&[proposal(scope, id, action, "rec-t", 1)])
        .expect("propose");
}

fn accepted_state_change(action: CandidateAction, expected_state: RecordState) {
    let (_dir, store) = open_temp();
    let scope = user(&store);
    let target = put(&store, "rec-t", &scope, "tone", "prefers long replies");
    propose_against(&store, &scope, "cand-1", action);
    let outcome = store.review(&accept(&scope, "cand-1", "")).unwrap();

    let stored = store.get(&record_ref(&scope, "rec-t")).unwrap();
    assert_eq!(outcome.record.as_ref(), Some(&stored));
    assert_eq!(stored.state, expected_state, "{action:?}");
    assert_eq!(stored.state_reason, "the user said so");
    assert_eq!(stored.superseded_by, "");
    assert_eq!(stored.revision, 2);
    assert_eq!(stored.text, target.text);
    assert_eq!(stored.updated_at, target.updated_at);
    assert_eq!(stored.created_at, target.created_at);
    assert!(history_file(&store, "rec-t", "1.md").is_file());
    assert!(outcome.tombstone.is_none());

    let receipt = store
        .get_candidate(&candidate_ref(&scope, "cand-1"))
        .unwrap();
    assert_eq!(receipt, outcome.candidate);
    assert_eq!(receipt.state, CandidateState::Accepted);
    assert_eq!(receipt.result_revision, 2);
    assert_eq!(receipt.result_record_id, "");
    assert!(retrieved_ids(&store, &scope, false).is_empty());
}

#[test]
fn an_accepted_retract_marks_the_target_retracted_without_changing_its_content() {
    accepted_state_change(CandidateAction::Retract, RecordState::Retracted);
}

#[test]
fn an_accepted_needs_review_marks_the_target_needs_review_without_changing_its_content() {
    accepted_state_change(CandidateAction::NeedsReview, RecordState::NeedsReview);
}

#[test]
fn an_accepted_supersede_writes_the_replacement_and_marks_the_target_superseded() {
    let (_dir, store) = open_temp();
    let scope = user(&store);
    let target = put(&store, "rec-t", &scope, "tone", "prefers long replies");
    propose_against(&store, &scope, "cand-1", CandidateAction::Supersede);
    let outcome = store.review(&accept(&scope, "cand-1", "rec-new")).unwrap();

    let replacement = store.get(&record_ref(&scope, "rec-new")).unwrap();
    assert_eq!(outcome.record.as_ref(), Some(&replacement));
    assert_eq!(replacement.revision, 1);
    assert_eq!(replacement.state, RecordState::Active);
    assert_eq!(replacement.text, "prefers terse replies");
    assert_eq!(
        (replacement.kind.as_str(), replacement.key.as_str()),
        ("preference", "tone")
    );
    assert_eq!(
        (replacement.created_at, replacement.updated_at),
        (at(2), at(2))
    );
    assert!(!replacement.metadata.contains_key(CORRECTS_METADATA));

    let retired = store.get(&record_ref(&scope, "rec-t")).unwrap();
    assert_eq!(retired.state, RecordState::Superseded);
    assert_eq!(retired.superseded_by, "rec-new");
    assert_eq!(retired.state_reason, "the user said so");
    assert_eq!(retired.revision, 2);
    assert_eq!(retired.text, target.text);
    assert_eq!(retired.updated_at, target.updated_at);
    assert!(history_file(&store, "rec-t", "1.md").is_file());

    let receipt = store
        .get_candidate(&candidate_ref(&scope, "cand-1"))
        .unwrap();
    assert_eq!(receipt.result_record_id, "rec-new");
    assert_eq!(receipt.result_revision, 1);
    assert_eq!(
        retrieved_ids(&store, &scope, false),
        vec!["rec-new".to_string()]
    );
    let active = store.list(&list_request(&scope)).unwrap();
    assert_eq!(active.records, vec![replacement]);
    let mut superseded_only = list_request(&scope);
    superseded_only.states = vec![RecordState::Superseded];
    assert_eq!(store.list(&superseded_only).unwrap().records, vec![retired]);
}

#[test]
fn an_accepted_correct_writes_a_replacement_that_names_the_retracted_target() {
    let (_dir, store) = open_temp();
    let scope = user(&store);
    put(&store, "rec-t", &scope, "tone", "prefers long replies");
    propose_against(&store, &scope, "cand-1", CandidateAction::Correct);
    let outcome = store.review(&accept(&scope, "cand-1", "rec-new")).unwrap();

    let replacement = store.get(&record_ref(&scope, "rec-new")).unwrap();
    assert_eq!(outcome.record.as_ref(), Some(&replacement));
    assert_eq!(
        replacement
            .metadata
            .get(CORRECTS_METADATA)
            .map(String::as_str),
        Some("rec-t")
    );
    assert_eq!(replacement.state, RecordState::Active);
    assert_eq!(replacement.text, "prefers terse replies");
    let retired = store.get(&record_ref(&scope, "rec-t")).unwrap();
    assert_eq!(retired.state, RecordState::Retracted);
    assert_eq!(retired.superseded_by, "");
    assert_eq!(retired.state_reason, "the user said so");
    assert_eq!(retired.revision, 2);
    let receipt = store
        .get_candidate(&candidate_ref(&scope, "cand-1"))
        .unwrap();
    assert_eq!(
        (receipt.result_record_id.as_str(), receipt.result_revision),
        ("rec-new", 1)
    );
    assert_eq!(
        retrieved_ids(&store, &scope, false),
        vec!["rec-new".to_string()]
    );
}

// The replacement of a supersede or correct normally keeps the target's kind
// and key. The key check ignores a retired record: only active records must
// have a unique (scope, kind, key). The replacement is written while the target
// is still active, so that one write exempts the target, and the stored
// snapshot, which resolves duplicate keys among active records only, keeps both.
#[test]
fn a_replacement_with_the_key_of_its_retired_target_is_stored_and_both_stay_visible() {
    for action in REPLACEMENT_ACTIONS {
        let (_dir, store) = open_temp();
        let scope = user(&store);
        put(&store, "rec-t", &scope, "tone", "prefers long replies");
        propose_against(&store, &scope, "cand-1", action);
        store.review(&accept(&scope, "cand-1", "rec-new")).unwrap();

        let snapshot = store.snapshot().unwrap();
        assert!(
            snapshot.invalid.is_empty(),
            "{action:?}: {:?}",
            snapshot.invalid
        );
        assert_eq!(snapshot.records.len(), 2, "{action:?}");
        let found = store
            .get_by_key(&RecordKey {
                scope: scope.clone(),
                kind: "preference".into(),
                key: "tone".into(),
            })
            .unwrap();
        assert_eq!(found.id, "rec-new", "{action:?}");

        // A third active record with the key is still a duplicate.
        let error = store
            .upsert(&UpsertRequest {
                record: sample_record("rec-3", &scope, "tone", "third"),
                expected_revision: None,
            })
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Conflict, "{action:?}");
        // So is reactivating the retired target next to the active replacement.
        let reactivate = StoreSetStateRequest {
            reference: record_ref(&scope, "rec-t"),
            expected_revision: 2,
            state: RecordState::Active,
            superseded_by: String::new(),
            reason: String::new(),
            changed_at: at(10),
        };
        assert_eq!(
            store.set_state(&reactivate).unwrap_err().kind,
            ErrorKind::Conflict,
            "{action:?}"
        );
    }
}

#[test]
fn a_supersede_of_a_standing_preference_does_not_count_the_retired_target_against_the_budget() {
    let (_dir, store) = open_temp();
    let scope = user(&store);
    let big = "x".repeat(600);
    let mut written = 0;
    while store
        .upsert_within_standing_budget(&UpsertRequest {
            record: sample_record(
                &format!("rec-{written}"),
                &scope,
                &format!("k{written}"),
                &big,
            ),
            expected_revision: None,
        })
        .is_ok()
    {
        written += 1;
        assert!(
            written < 100,
            "budget never filled, limit {MAX_STANDING_TOKENS}"
        );
    }
    let mut candidate = proposal(&scope, "cand-1", CandidateAction::Supersede, "rec-0", 1);
    candidate.proposed.key = "k0".into();
    candidate.proposed.text = big;
    store.propose(&[candidate]).unwrap();
    let outcome = store
        .review_within_standing_budget(&accept(&scope, "cand-1", "rec-new"))
        .unwrap();
    assert_eq!(outcome.record.unwrap().id, "rec-new");
    assert_eq!(
        store.get(&record_ref(&scope, "rec-0")).unwrap().state,
        RecordState::Superseded
    );
}

#[test]
fn a_stale_tombstoned_or_missing_target_changes_no_file_and_leaves_the_candidate_pending() {
    for action in ALL_LIFECYCLE_ACTIONS {
        let (_dir, store) = open_temp();
        let scope = user(&store);
        put(&store, "rec-t", &scope, "tone", "prefers long replies");
        put(&store, "rec-f", &scope, "forgotten", "to be forgotten");
        store
            .forget(&StoreForgetRequest {
                reference: record_ref(&scope, "rec-f"),
                expected_revision: 1,
                forgotten_at: at(3),
            })
            .unwrap();
        let stale_base = proposal(&scope, "cand-stale", action, "rec-t", 7);
        let tombstoned = proposal(&scope, "cand-gone", action, "rec-f", 1);
        let missing = proposal(&scope, "cand-none", action, "rec-none", 1);
        store.propose(&[stale_base, tombstoned, missing]).unwrap();
        store
            .propose(&[proposal(&scope, "cand-req", action, "rec-t", 1)])
            .unwrap();
        let result_id = if REPLACEMENT_ACTIONS.contains(&action) {
            "rec-new"
        } else {
            ""
        };
        let before = stored_files(&store);

        let stale_base_error = store
            .review(&accept(&scope, "cand-stale", result_id))
            .unwrap_err();
        assert_eq!(
            stale_base_error.kind,
            ErrorKind::Conflict,
            "{action:?} stale base"
        );
        assert_eq!(
            stale_base_error.conflict.expect("detail").actual_revision,
            1
        );
        let mut stale_target = accept(&scope, "cand-req", result_id);
        stale_target.target_revision = Some(5);
        assert_eq!(
            store.review(&stale_target).unwrap_err().kind,
            ErrorKind::Conflict,
            "{action:?} target_revision"
        );
        let gone = store
            .review(&accept(&scope, "cand-gone", result_id))
            .unwrap_err();
        assert_eq!(gone.kind, ErrorKind::Conflict, "{action:?} tombstone");
        let none = store
            .review(&accept(&scope, "cand-none", result_id))
            .unwrap_err();
        assert_eq!(none.kind, ErrorKind::NotFound, "{action:?} missing");

        assert_eq!(stored_files(&store), before, "{action:?}");
        assert!(!record_path(&store, "rec-new").exists(), "{action:?}");
        for id in ["cand-stale", "cand-gone", "cand-none", "cand-req"] {
            let state = store
                .get_candidate(&candidate_ref(&scope, id))
                .unwrap()
                .state;
            assert_eq!(state, CandidateState::Pending, "{action:?} {id}");
        }
    }
}

#[test]
fn a_rejected_lifecycle_candidate_writes_only_its_receipt() {
    for action in ALL_LIFECYCLE_ACTIONS {
        let (_dir, store) = open_temp();
        let scope = user(&store);
        put(&store, "rec-t", &scope, "tone", "prefers long replies");
        propose_against(&store, &scope, "cand-1", action);
        let before = stored_files(&store);
        let mut reject = accept(&scope, "cand-1", "");
        reject.decision = ReviewDecision::Reject;
        let outcome = store.review(&reject).unwrap();

        assert_eq!(outcome.record, None, "{action:?}");
        assert_eq!(
            outcome.candidate.state,
            CandidateState::Rejected,
            "{action:?}"
        );
        assert_eq!(outcome.candidate.result_record_id, "");
        assert_eq!(outcome.candidate.result_revision, 0);
        assert_eq!(stored_files(&store), before, "{action:?}");
        assert_eq!(
            store.get(&record_ref(&scope, "rec-t")).unwrap().state,
            RecordState::Active
        );
        assert!(store.path("candidates/archive", "cand-1", "json").is_file());
    }
}

#[test]
fn a_proposal_with_content_for_retract_or_without_text_for_supersede_is_rejected() {
    let (_dir, store) = open_temp();
    let scope = user(&store);
    for action in STATE_CHANGE_ACTIONS {
        let mut with_content = proposal(&scope, "cand-c", action, "rec-t", 1);
        with_content.proposed.text = "prefers terse replies".into();
        assert!(
            store.propose(&[with_content]).is_err(),
            "{action:?} with text"
        );
        let mut with_key = proposal(&scope, "cand-k", action, "rec-t", 1);
        with_key.proposed.key = "tone".into();
        assert!(store.propose(&[with_key]).is_err(), "{action:?} with key");
    }
    for action in REPLACEMENT_ACTIONS {
        let mut without_text = proposal(&scope, "cand-t", action, "rec-t", 1);
        without_text.proposed.text = String::new();
        assert!(
            store.propose(&[without_text]).is_err(),
            "{action:?} without text"
        );
    }
    let stored = fs::read_dir(store.root().join("candidates"))
        .unwrap()
        .filter(|entry| entry.as_ref().unwrap().file_type().unwrap().is_file())
        .count();
    assert_eq!(stored, 0);
}

#[test]
fn a_review_request_must_match_the_ids_a_lifecycle_action_writes() {
    let (_dir, store) = open_temp();
    let scope = user(&store);
    put(&store, "rec-t", &scope, "tone", "prefers long replies");
    for action in ALL_LIFECYCLE_ACTIONS {
        propose_against(&store, &scope, "cand-1", action);
        let writes_replacement = REPLACEMENT_ACTIONS.contains(&action);
        let wrong_id = if writes_replacement { "" } else { "rec-new" };
        let error = store
            .review(&accept(&scope, "cand-1", wrong_id))
            .unwrap_err();
        assert_eq!(
            error.kind,
            ErrorKind::InvalidRequest,
            "{action:?} result ID {wrong_id:?}"
        );
        if !writes_replacement {
            let mut edit = accept(&scope, "cand-1", "");
            edit.edited = Some(proposal(&scope, "x", CandidateAction::Create, "", 0).proposed);
            assert_eq!(
                store.review(&edit).unwrap_err().kind,
                ErrorKind::InvalidRequest,
                "{action:?} edit"
            );
        }
        assert_eq!(
            store.get(&record_ref(&scope, "rec-t")).unwrap().revision,
            1,
            "{action:?}"
        );
        let mut reject = accept(&scope, "cand-1", "");
        reject.decision = ReviewDecision::Reject;
        store.review(&reject).unwrap();
        fs::remove_file(store.path("candidates/archive", "cand-1", "json")).unwrap();
    }
}
