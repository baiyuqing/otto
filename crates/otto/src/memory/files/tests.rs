mod candidate_archive;
mod history;

use std::fs;
use std::time::Duration;

use super::candidates::cleared_proposed;
use super::testsupport::{at, open_temp, sample_record};
use super::*;
use crate::memory::{
    CandidateAction, CandidateListRequest, CandidateRef, CandidateState, ListRequest,
    NAMESPACE_WORKSPACE, Origin, Provenance, RecordKey, RecordRef, RetrievalRequest,
    RetrievalResult, ReviewDecision, StoreForgetRequest, StoreReviewRequest, UpsertRequest,
};

fn user(store: &FileStore) -> Scope {
    store.identity().expect("identity").user_scope
}

fn put(store: &FileStore, id: &str, scope: &Scope, key: &str, text: &str) -> Record {
    store
        .upsert(&UpsertRequest {
            record: sample_record(id, scope, key, text),
            expected_revision: None,
        })
        .expect("upsert")
}

fn record_ref(scope: &Scope, id: &str) -> RecordRef {
    RecordRef {
        scope: scope.clone(),
        id: id.into(),
    }
}

fn record_path(store: &FileStore, id: &str) -> PathBuf {
    store.root().join("records").join(format!("{id}.md"))
}

fn request(scopes: Vec<Scope>, query: &str) -> RetrievalRequest {
    RetrievalRequest {
        query: query.to_string(),
        scopes,
        kinds: Vec::new(),
        labels: Vec::new(),
        include_expired: false,
        include_baseline: false,
        limit: 10,
        token_budget: 2000,
        cursor: String::new(),
        now: at(5),
        estimate_tokens: None,
    }
}

fn ids(result: &RetrievalResult) -> Vec<String> {
    result.matches.iter().map(|m| m.record.id.clone()).collect()
}

#[test]
fn a_record_round_trips_through_create_read_update_and_forget() {
    let (_dir, store) = open_temp();
    let scope = user(&store);
    let created = put(&store, "rec-1", &scope, "tone", "prefers terse replies");
    assert_eq!(store.get(&record_ref(&scope, "rec-1")), Ok(created.clone()));
    assert_eq!(
        store.get_by_key(&RecordKey {
            scope: scope.clone(),
            kind: "preference".into(),
            key: "tone".into()
        }),
        Ok(created.clone())
    );

    let mut next = created.clone();
    next.text = "prefers long replies".into();
    next.updated_at = at(2);
    let updated = store
        .upsert(&UpsertRequest {
            record: next,
            expected_revision: Some(1),
        })
        .expect("update");
    assert_eq!(updated.revision, 2);
    assert_eq!(updated.created_at, created.created_at);
}

#[test]
fn a_stale_revision_is_a_conflict_and_leaves_the_file_unchanged() {
    let (_dir, store) = open_temp();
    let scope = user(&store);
    let created = put(&store, "rec-1", &scope, "tone", "one");
    let before = fs::read(record_path(&store, "rec-1")).unwrap();
    let mut next = created;
    next.text = "two".into();
    next.revision = 7;
    next.updated_at = at(2);
    let error = store
        .upsert(&UpsertRequest {
            record: next,
            expected_revision: Some(7),
        })
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Conflict);
    assert_eq!(fs::read(record_path(&store, "rec-1")).unwrap(), before);
}

#[test]
fn a_second_record_with_the_same_key_is_a_conflict() {
    let (_dir, store) = open_temp();
    let scope = user(&store);
    put(&store, "rec-1", &scope, "tone", "one");
    let error = store
        .upsert(&UpsertRequest {
            record: sample_record("rec-2", &scope, "tone", "two"),
            expected_revision: None,
        })
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Conflict);
    assert!(!record_path(&store, "rec-2").exists());
}

#[test]
fn a_full_standing_budget_rejects_a_standing_write() {
    let (_dir, store) = open_temp();
    let scope = user(&store);
    let big = "x".repeat(600);
    let mut written = 0;
    let error = loop {
        let result = store.upsert_within_standing_budget(&UpsertRequest {
            record: sample_record(
                &format!("rec-{written}"),
                &scope,
                &format!("k{written}"),
                &big,
            ),
            expected_revision: None,
        });
        match result {
            Ok(_) => written += 1,
            Err(error) => break error,
        }
        assert!(written < 100, "budget never filled");
    };
    assert!(written > 0);
    assert_eq!(error.kind, ErrorKind::InvalidRequest);
    assert!(!record_path(&store, &format!("rec-{written}")).exists());
}

#[test]
fn forget_writes_a_tombstone_and_removes_the_record_file() {
    let (_dir, store) = open_temp();
    let scope = user(&store);
    put(&store, "rec-1", &scope, "tone", "one");
    let tombstone = store
        .forget(&StoreForgetRequest {
            reference: record_ref(&scope, "rec-1"),
            expected_revision: 1,
            forgotten_at: at(3),
        })
        .expect("forget")
        .tombstone;
    assert_eq!(tombstone.revision, 2);
    assert!(!record_path(&store, "rec-1").exists());
    assert!(store.root().join("tombstones/rec-1.json").exists());
    assert_eq!(
        store.get(&record_ref(&scope, "rec-1")).unwrap_err().kind,
        ErrorKind::NotFound
    );
    assert_eq!(
        store.get_tombstone(&record_ref(&scope, "rec-1")),
        Ok(tombstone)
    );
}

#[test]
fn a_leftover_record_with_a_tombstone_is_not_read_and_open_removes_it() {
    let (dir, store) = open_temp();
    let scope = user(&store);
    put(&store, "rec-1", &scope, "tone", "one");
    let bytes = fs::read(record_path(&store, "rec-1")).unwrap();
    store
        .forget(&StoreForgetRequest {
            reference: record_ref(&scope, "rec-1"),
            expected_revision: 1,
            forgotten_at: at(3),
        })
        .unwrap();
    // A crash between the tombstone write and the record removal leaves both.
    fs::write(record_path(&store, "rec-1"), bytes).unwrap();
    assert_eq!(
        store.get(&record_ref(&scope, "rec-1")).unwrap_err().kind,
        ErrorKind::NotFound
    );
    store.close().unwrap();
    let reopened = FileStore::open(&dir.path().join("memory"), MemoryOptions::default()).unwrap();
    assert!(!record_path(&reopened, "rec-1").exists());
}

#[test]
fn an_invalid_file_is_skipped_and_listed_and_a_write_to_it_is_corrupt() {
    let (_dir, store) = open_temp();
    let scope = user(&store);
    put(&store, "rec-1", &scope, "tone", "one");
    fs::write(record_path(&store, "bad"), "not a record").unwrap();
    fs::write(record_path(&store, "bad name"), "x").unwrap();
    fs::write(store.root().join("records/.hidden.md"), "x").unwrap();

    let page = store
        .list(&ListRequest {
            all_scopes: false,
            scopes: vec![scope.clone()],
            kinds: Vec::new(),
            labels: Vec::new(),
            limit: 10,
            cursor: String::new(),
            now: at(5),
            include_expired: false,
        })
        .unwrap();
    assert_eq!(page.records.len(), 1);
    let invalid = store.invalid_files().unwrap();
    let names: Vec<_> = invalid
        .iter()
        .map(|file| {
            file.path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(names, vec!["bad name.md", "bad.md"]);

    let error = store
        .upsert(&UpsertRequest {
            record: sample_record("bad", &scope, "other", "text"),
            expected_revision: None,
        })
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Corrupt);
    assert_eq!(
        fs::read(record_path(&store, "bad")).unwrap(),
        b"not a record"
    );
}

#[test]
fn a_duplicate_key_keeps_the_newer_record_and_reports_the_other() {
    let (_dir, store) = open_temp();
    let scope = user(&store);
    put(&store, "rec-1", &scope, "tone", "older");
    let mut newer = sample_record("rec-2", &scope, "tone", "newer");
    newer.revision = 1;
    newer.updated_at = at(4);
    fs::write(
        record_path(&store, "rec-2"),
        encode_record_file(&newer).unwrap(),
    )
    .unwrap();

    let key = RecordKey {
        scope: scope.clone(),
        kind: "preference".into(),
        key: "tone".into(),
    };
    assert_eq!(store.get_by_key(&key).unwrap().id, "rec-2");
    let invalid = store.invalid_files().unwrap();
    assert_eq!(invalid.len(), 1);
    assert!(invalid[0].path.ends_with("rec-1.md"));
    assert!(invalid[0].reason.contains("rec-2"));
}

#[test]
fn a_hand_edit_is_visible_without_a_revision_change() {
    let (_dir, store) = open_temp();
    let scope = user(&store);
    put(&store, "rec-1", &scope, "tone", "prefers terse replies");
    let path = record_path(&store, "rec-1");
    let edited = fs::read_to_string(&path)
        .unwrap()
        .replace("terse", "verbose");
    fs::write(&path, edited).unwrap();
    let record = store.get(&record_ref(&scope, "rec-1")).unwrap();
    assert_eq!(record.text, "prefers verbose replies");
    assert_eq!(record.revision, 1);
    let found = store.retrieve(&request(vec![scope], "verbose")).unwrap();
    assert_eq!(ids(&found), vec!["rec-1"]);
}

#[test]
fn a_hand_written_file_with_loose_json_and_a_short_source_is_read() {
    let (_dir, store) = open_temp();
    let scope = user(&store);
    let content = format!(
        "---\nscope:   {{ \"namespace\" : \"user\", \"id\":\"{}\" }}\nkind: \
            \"preference\"\nkey: \"k\"\nlabels: [ ]\nconfidence: 1\nrevision: \
            1\ncreated_at: \"2026-03-01T00:00:01Z\"\nupdated_at: \
            \"2026-03-01T00:00:01Z\"\nexpires_at: null\nsource: \
            {{\"origin\":\"human\"}}\nmetadata: {{}}\n---\nhand written\n",
        scope.id
    );
    fs::write(record_path(&store, "hand"), content).unwrap();
    let record = store.get(&record_ref(&scope, "hand")).unwrap();
    assert_eq!(record.text, "hand written");
    assert_eq!(record.source.origin, Some(Origin::Human));
}

#[test]
fn a_second_store_on_the_same_root_waits_for_the_lock_then_reports_busy() {
    let (dir, store) = open_temp();
    let scope = user(&store);
    let options = MemoryOptions {
        busy_timeout: Duration::from_millis(50),
        ..MemoryOptions::default()
    };
    let other = FileStore::open(&dir.path().join("memory"), options).unwrap();
    let held = acquire(store.root(), Duration::from_secs(1)).unwrap();
    let error = other
        .upsert(&UpsertRequest {
            record: sample_record("rec-1", &scope, "k", "t"),
            expected_revision: None,
        })
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Busy);
    assert!(!record_path(&store, "rec-1").exists());
    // Reads take no lock.
    assert!(other.get(&record_ref(&scope, "rec-1")).is_err());
    drop(held);
    put(&other, "rec-1", &scope, "k", "t");
    assert!(store.get(&record_ref(&scope, "rec-1")).is_ok());
}

#[test]
fn workspace_scopes_do_not_leak() {
    let (_dir, store) = open_temp();
    let user = user(&store);
    let a = Scope::new(NAMESPACE_WORKSPACE, "ws-a");
    let b = Scope::new(NAMESPACE_WORKSPACE, "ws-b");
    put(&store, "rec-a", &a, "k", "shared word");
    put(&store, "rec-b", &b, "k", "shared word");
    put(&store, "rec-u", &user, "k", "shared word");
    for query in ["shared", ""] {
        let found = store
            .retrieve(&request(vec![user.clone(), a.clone()], query))
            .unwrap();
        assert!(!ids(&found).contains(&"rec-b".to_string()), "{query}");
        assert_eq!(
            found.matches.len(),
            1,
            "workspace twin replaces user: {query}"
        );
        assert_eq!(found.matches[0].record.id, "rec-a");
    }
}

#[test]
fn an_empty_query_is_workspace_first_then_newest_first() {
    let (_dir, store) = open_temp();
    let user = user(&store);
    let ws = Scope::new(NAMESPACE_WORKSPACE, "ws-a");
    for (id, scope, second, text) in [
        ("u-old", &user, 2, "alpha"),
        ("u-new", &user, 4, "bravo"),
        ("w-old", &ws, 1, "charlie"),
        ("w-new", &ws, 3, "delta"),
    ] {
        let mut record = sample_record(id, scope, &format!("key-{id}"), text);
        record.created_at = at(second);
        record.updated_at = at(second);
        store
            .upsert(&UpsertRequest {
                record,
                expected_revision: None,
            })
            .unwrap();
    }
    let found = store.retrieve(&request(vec![user, ws], "")).unwrap();
    assert_eq!(ids(&found), vec!["w-new", "w-old", "u-new", "u-old"]);
}

#[test]
fn query_terms_match_as_case_insensitive_substrings_including_cjk() {
    let (_dir, store) = open_temp();
    let scope = user(&store);
    put(
        &store,
        "rec-1",
        &scope,
        "language",
        "回复技术问题时使用中文",
    );
    put(&store, "rec-2", &scope, "editor", "Uses NeoVim daily");
    let run = |query: &str| {
        ids(&store
            .retrieve(&request(vec![scope.clone()], query))
            .unwrap())
    };
    assert_eq!(run("中文"), vec!["rec-1"]);
    assert_eq!(run("vim"), vec!["rec-2"]);
    assert_eq!(run("EDIT"), vec!["rec-2"]);
    assert_eq!(run("preference").len(), 2);
    assert!(run("kubernetes").is_empty());
    // More matching terms rank first.
    assert_eq!(run("daily nomatch neovim vim"), vec!["rec-2"]);
}

#[test]
fn retrieval_pages_by_cursor_and_rejects_a_cursor_for_another_query() {
    let (_dir, store) = open_temp();
    let scope = user(&store);
    for index in 1..=3 {
        put(
            &store,
            &format!("rec-{index}"),
            &scope,
            &format!("k{index}"),
            &format!("terse reply {index}"),
        );
    }
    let mut input = request(vec![scope.clone()], "terse");
    input.limit = 1;
    let first = store.retrieve(&input).unwrap();
    assert_eq!(first.matches.len(), 1);
    assert!(!first.next_cursor.is_empty());
    let mut next = input.clone();
    next.cursor = first.next_cursor.clone();
    let second = store.retrieve(&next).unwrap();
    assert_ne!(ids(&second), ids(&first));

    let mut other = input.clone();
    other.query = "reply".into();
    other.cursor = first.next_cursor.clone();
    assert_eq!(
        store.retrieve(&other).unwrap_err().kind,
        ErrorKind::InvalidCursor
    );

    // The snapshot moved so the id at the cursor ordinal no longer matches.
    store
        .forget(&StoreForgetRequest {
            reference: record_ref(&scope, &ids(&first)[0]),
            expected_revision: 1,
            forgotten_at: at(9),
        })
        .unwrap();
    assert_eq!(
        store.retrieve(&next).unwrap_err().kind,
        ErrorKind::InvalidCursor
    );
}

#[test]
fn the_baseline_leads_and_an_oversized_record_is_skipped() {
    let (_dir, store) = open_temp();
    let scope = user(&store);
    put(&store, "rec-1", &scope, "tone", "terse ".repeat(200).trim());
    put(&store, "rec-2", &scope, "editor", "terse is best");
    let mut input = request(vec![scope.clone()], "terse");
    input.token_budget = 120;
    assert_eq!(ids(&store.retrieve(&input).unwrap()), vec!["rec-2"]);

    let mut baseline = request(vec![scope], "nomatch");
    baseline.include_baseline = true;
    let found = store.retrieve(&baseline).unwrap();
    assert!(found.matches.iter().all(|m| m.standing));
    assert!(!found.matches.is_empty());
}

fn proposal(
    scope: &Scope,
    id: &str,
    action: CandidateAction,
    target: &str,
    base: u64,
) -> Candidate {
    let source = Provenance {
        origin: Some(Origin::Model),
        ..Provenance::default()
    };
    let proposed = if action == CandidateAction::Forget {
        Record {
            scope: scope.clone(),
            source,
            ..Record::default()
        }
    } else {
        Record {
            scope: scope.clone(),
            kind: "preference".into(),
            key: "tone".into(),
            text: "prefers terse replies".into(),
            labels: vec!["style".into()],
            source,
            confidence: 0.5,
            ..Record::default()
        }
    };
    Candidate {
        id: id.into(),
        proposed,
        action,
        target_id: target.into(),
        base_revision: base,
        reason: "the user said so".into(),
        state: CandidateState::Pending,
        created_at: at(1),
        decided_at: None,
        decision_source: None,
        result_record_id: String::new(),
        result_revision: 0,
    }
}

fn candidate_ref(scope: &Scope, id: &str) -> CandidateRef {
    CandidateRef {
        scope: scope.clone(),
        id: id.into(),
    }
}

fn accept(scope: &Scope, id: &str, result: &str) -> StoreReviewRequest {
    StoreReviewRequest {
        reference: candidate_ref(scope, id),
        result_record_id: result.into(),
        decision: ReviewDecision::Accept,
        edited: None,
        target_revision: None,
        decision_source: Some(Origin::Human),
        decided_at: at(2),
    }
}

#[test]
fn a_candidate_file_round_trips_and_a_create_is_accepted_into_a_record() {
    let (_dir, store) = open_temp();
    let scope = user(&store);
    let created = proposal(&scope, "cand-1", CandidateAction::Create, "", 0);
    store.propose(std::slice::from_ref(&created)).unwrap();
    assert_eq!(
        store.get_candidate(&candidate_ref(&scope, "cand-1")),
        Ok(created.clone())
    );
    let page = store
        .list_candidates(&CandidateListRequest {
            scopes: vec![scope.clone()],
            states: vec![CandidateState::Pending],
            limit: 10,
            cursor: String::new(),
        })
        .unwrap();
    assert_eq!(page.candidates, vec![created.clone()]);

    let outcome = store.review(&accept(&scope, "cand-1", "rec-1")).unwrap();
    let stored = outcome.record.expect("record");
    assert_eq!(stored.revision, 1);
    assert_eq!(stored.text, created.proposed.text);
    assert_eq!(store.get(&record_ref(&scope, "rec-1")), Ok(stored));
    let decided = store
        .get_candidate(&candidate_ref(&scope, "cand-1"))
        .unwrap();
    assert_eq!(decided.state, CandidateState::Accepted);
    assert_eq!(decided.proposed, cleared_proposed(&scope));
    assert_eq!(
        store
            .review(&accept(&scope, "cand-1", "rec-1"))
            .unwrap_err()
            .kind,
        ErrorKind::Conflict
    );
}

#[test]
fn a_rejected_candidate_writes_no_record() {
    let (_dir, store) = open_temp();
    let scope = user(&store);
    store
        .propose(&[proposal(&scope, "cand-2", CandidateAction::Create, "", 0)])
        .unwrap();
    let mut request = accept(&scope, "cand-2", "");
    request.decision = ReviewDecision::Reject;
    let outcome = store.review(&request).unwrap();
    assert_eq!(outcome.record, None);
    assert_eq!(outcome.candidate.state, CandidateState::Rejected);
    assert_eq!(
        fs::read_dir(store.root().join("records")).unwrap().count(),
        0
    );
}

#[test]
fn an_accepted_update_advances_the_target_and_an_accepted_forget_tombstones_it() {
    let (_dir, store) = open_temp();
    let scope = user(&store);
    let record = put(&store, "rec-9", &scope, "tone", "prefers long replies");
    store
        .propose(&[proposal(
            &scope,
            "cand-3",
            CandidateAction::Update,
            &record.id,
            1,
        )])
        .unwrap();
    let updated = store
        .review(&accept(&scope, "cand-3", ""))
        .unwrap()
        .record
        .unwrap();
    assert_eq!(updated.revision, 2);
    assert_eq!(updated.text, "prefers terse replies");
    assert_eq!(updated.created_at, record.created_at);

    store
        .propose(&[proposal(
            &scope,
            "cand-4",
            CandidateAction::Forget,
            &record.id,
            2,
        )])
        .unwrap();
    let tombstone = store
        .review(&accept(&scope, "cand-4", ""))
        .unwrap()
        .tombstone
        .unwrap();
    assert_eq!(tombstone.revision, 3);
    assert!(!record_path(&store, "rec-9").exists());
}

#[test]
fn an_accepted_update_whose_target_moved_is_a_conflict_and_changes_nothing() {
    let (_dir, store) = open_temp();
    let scope = user(&store);
    let record = put(&store, "rec-8", &scope, "tone", "prefers long replies");
    store
        .propose(&[proposal(
            &scope,
            "cand-5",
            CandidateAction::Update,
            &record.id,
            7,
        )])
        .unwrap();
    let before = fs::read(record_path(&store, "rec-8")).unwrap();
    let error = store.review(&accept(&scope, "cand-5", "")).unwrap_err();
    assert_eq!(error.kind, ErrorKind::Conflict);
    assert_eq!(error.conflict.expect("detail").actual_revision, 1);
    assert_eq!(fs::read(record_path(&store, "rec-8")).unwrap(), before);
    assert_eq!(
        store
            .get_candidate(&candidate_ref(&scope, "cand-5"))
            .unwrap()
            .state,
        CandidateState::Pending
    );
}

#[test]
fn a_proposal_batch_with_one_invalid_member_writes_no_file() {
    let (_dir, store) = open_temp();
    let scope = user(&store);
    let good = proposal(&scope, "cand-a", CandidateAction::Create, "", 0);
    let mut bad = proposal(&scope, "cand-b", CandidateAction::Create, "", 0);
    bad.proposed.text = String::new();
    assert!(store.propose(&[good, bad]).is_err());
    assert_eq!(
        fs::read_dir(store.root().join("candidates"))
            .unwrap()
            .filter(|entry| entry.as_ref().unwrap().file_type().unwrap().is_file())
            .count(),
        0
    );
}
