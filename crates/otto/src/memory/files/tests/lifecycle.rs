use super::*;
use crate::memory::{RecordState, StoreRestoreRequest, StoreSetStateRequest, StoreVerifyRequest};
use chrono::{DateTime, Utc};

fn set_state_request(
    scope: &Scope,
    id: &str,
    expected: u64,
    state: RecordState,
    superseded_by: &str,
    reason: &str,
) -> StoreSetStateRequest {
    StoreSetStateRequest {
        reference: record_ref(scope, id),
        expected_revision: expected,
        state,
        superseded_by: superseded_by.into(),
        reason: reason.into(),
        changed_at: at(10),
    }
}

fn verify_request(
    scope: &Scope,
    id: &str,
    expected: u64,
    review_at: Option<DateTime<Utc>>,
    verified_at: DateTime<Utc>,
) -> StoreVerifyRequest {
    StoreVerifyRequest {
        reference: record_ref(scope, id),
        expected_revision: expected,
        review_at,
        verified_at,
    }
}

pub(super) fn history_file(store: &FileStore, id: &str, name: &str) -> PathBuf {
    store.root().join("history").join(id).join(name)
}

/// Every file under `records/` and `history/` with its bytes, sorted by path.
pub(super) fn stored_files(store: &FileStore) -> Vec<(PathBuf, Vec<u8>)> {
    fn walk(directory: &Path, files: &mut Vec<(PathBuf, Vec<u8>)>) {
        let Ok(entries) = fs::read_dir(directory) else {
            return;
        };
        for entry in entries {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, files);
            } else {
                files.push((path.clone(), fs::read(&path).unwrap()));
            }
        }
    }
    let mut files = Vec::new();
    walk(&store.root().join("records"), &mut files);
    walk(&store.root().join("history"), &mut files);
    files.sort();
    files
}

pub(super) fn retrieved_ids(
    store: &FileStore,
    scope: &Scope,
    include_expired: bool,
) -> Vec<String> {
    let mut query = request(vec![scope.clone()], "");
    query.include_expired = include_expired;
    let mut ids = ids(&store.retrieve(&query).expect("retrieve"));
    ids.sort();
    ids
}

/// `rec-eff` is effective; every other record is excluded from normal recall
/// for one reason: needs review, superseded, retracted, expired, review due.
fn records_in_every_recall_state(store: &FileStore, scope: &Scope) {
    for (id, key) in [
        ("rec-eff", "k-eff"),
        ("rec-review", "k-review"),
        ("rec-super", "k-super"),
        ("rec-retract", "k-retract"),
        ("rec-due", "k-due"),
    ] {
        put(store, id, scope, key, &format!("text of {id}"));
    }
    let mut expired = sample_record("rec-expired", scope, "k-expired", "text of rec-expired");
    expired.expires_at = Some(at(3));
    store
        .upsert(&UpsertRequest {
            record: expired,
            expected_revision: None,
        })
        .unwrap();
    store
        .set_state(&set_state_request(
            scope,
            "rec-review",
            1,
            RecordState::NeedsReview,
            "",
            "",
        ))
        .unwrap();
    store
        .set_state(&set_state_request(
            scope,
            "rec-super",
            1,
            RecordState::Superseded,
            "rec-eff",
            "",
        ))
        .unwrap();
    store
        .set_state(&set_state_request(
            scope,
            "rec-retract",
            1,
            RecordState::Retracted,
            "",
            "wrong",
        ))
        .unwrap();
    store
        .verify(&verify_request(scope, "rec-due", 1, Some(at(2)), at(1)))
        .unwrap();
}

#[test]
fn set_state_writes_a_new_revision_and_keeps_content_and_update_time() {
    let (_dir, store) = open_temp();
    let scope = user(&store);
    let original = put(&store, "rec-1", &scope, "tone", "prefers terse replies");
    put(&store, "rec-2", &scope, "other", "replacement text");
    let mut expected_revision = 1;
    let mut previous_bytes = fs::read(record_path(&store, "rec-1")).unwrap();
    let steps = [
        (RecordState::NeedsReview, "", "check again"),
        (RecordState::Retracted, "", "outdated"),
        (RecordState::Superseded, "rec-2", ""),
    ];
    for (state, replacement, reason) in steps {
        let written = store
            .set_state(&set_state_request(
                &scope,
                "rec-1",
                expected_revision,
                state,
                replacement,
                reason,
            ))
            .unwrap();
        // Fails if a lifecycle write does not advance the revision by one.
        assert_eq!(written.revision, expected_revision + 1);
        // Fails if a lifecycle write moves the content update time.
        assert_eq!(written.updated_at, original.updated_at);
        assert_eq!(written.state, state);
        assert_eq!(written.superseded_by, replacement);
        assert_eq!(written.state_reason, reason);
        let mut content = written.clone();
        content.revision = original.revision;
        content.state = original.state;
        content.superseded_by = String::new();
        content.state_reason = String::new();
        assert_eq!(content, original, "content fields changed for {state:?}");
        // Fails if the superseded revision is not archived under history/<id>/<n>.md.
        assert_eq!(
            fs::read(history_file(
                &store,
                "rec-1",
                &format!("{expected_revision}.md")
            ))
            .unwrap(),
            previous_bytes
        );
        assert_eq!(store.get(&record_ref(&scope, "rec-1")).unwrap(), written);
        previous_bytes = fs::read(record_path(&store, "rec-1")).unwrap();
        expected_revision += 1;
    }
}

#[test]
fn returning_to_active_clears_the_replacement_and_the_reason() {
    let (_dir, store) = open_temp();
    let scope = user(&store);
    put(&store, "rec-1", &scope, "tone", "one");
    put(&store, "rec-2", &scope, "other", "two");
    store
        .set_state(&set_state_request(
            &scope,
            "rec-1",
            1,
            RecordState::Superseded,
            "rec-2",
            "replaced",
        ))
        .unwrap();
    let active = store
        .set_state(&set_state_request(
            &scope,
            "rec-1",
            2,
            RecordState::Active,
            "",
            "ignored",
        ))
        .unwrap();
    assert_eq!(active.state, RecordState::Active);
    // Fails if superseded_by survives the return to active.
    assert_eq!(active.superseded_by, "");
    // Fails if state_reason survives the return to active.
    assert_eq!(active.state_reason, "");
    assert_eq!(active.revision, 3);
    let bytes = fs::read_to_string(record_path(&store, "rec-1")).unwrap();
    assert!(bytes.contains("\nstate: \"active\"\n"), "{bytes}");
}

#[test]
fn set_state_superseded_rejects_a_bad_replacement_and_changes_no_file() {
    let (_dir, store) = open_temp();
    let scope = user(&store);
    let workspace = Scope::new(NAMESPACE_WORKSPACE, "ws-a");
    put(&store, "rec-1", &scope, "tone", "one");
    put(
        &store,
        "rec-other-scope",
        &workspace,
        "tone",
        "workspace record",
    );
    put(&store, "rec-retracted", &scope, "gone", "withdrawn record");
    store
        .set_state(&set_state_request(
            &scope,
            "rec-retracted",
            1,
            RecordState::Retracted,
            "",
            "no",
        ))
        .unwrap();
    let before = stored_files(&store);
    for replacement in ["rec-missing", "rec-other-scope", "rec-retracted", "rec-1"] {
        let error = store
            .set_state(&set_state_request(
                &scope,
                "rec-1",
                1,
                RecordState::Superseded,
                replacement,
                "",
            ))
            .unwrap_err();
        // Fails if a missing, foreign-scope, non-active or self replacement is accepted.
        assert_eq!(error.kind, ErrorKind::InvalidRequest, "{replacement}");
        assert_eq!(stored_files(&store), before, "{replacement}");
    }
}

#[test]
fn set_state_and_verify_reject_stale_forgotten_and_pre_creation_times() {
    let (_dir, store) = open_temp();
    let scope = user(&store);
    put(&store, "rec-1", &scope, "tone", "one");
    let mut late = sample_record("rec-late", &scope, "late", "created late");
    late.created_at = at(5);
    late.updated_at = at(5);
    store
        .upsert(&UpsertRequest {
            record: late,
            expected_revision: None,
        })
        .unwrap();
    let before = stored_files(&store);

    let stale = store
        .set_state(&set_state_request(
            &scope,
            "rec-1",
            7,
            RecordState::Retracted,
            "",
            "",
        ))
        .unwrap_err();
    assert_eq!(stale.kind, ErrorKind::Conflict);
    let stale = store
        .verify(&verify_request(&scope, "rec-1", 7, None, at(10)))
        .unwrap_err();
    assert_eq!(stale.kind, ErrorKind::Conflict);

    let mut early = set_state_request(&scope, "rec-late", 1, RecordState::Retracted, "", "");
    early.changed_at = at(1);
    // Fails if a change time before the record was created is accepted.
    assert_eq!(
        store.set_state(&early).unwrap_err().kind,
        ErrorKind::InvalidRequest
    );
    let early_verify = verify_request(&scope, "rec-late", 1, None, at(1));
    assert_eq!(
        store.verify(&early_verify).unwrap_err().kind,
        ErrorKind::InvalidRequest
    );
    assert_eq!(stored_files(&store), before);

    store
        .forget(&StoreForgetRequest {
            reference: record_ref(&scope, "rec-1"),
            expected_revision: 1,
            forgotten_at: at(3),
        })
        .unwrap();
    let forgotten = set_state_request(&scope, "rec-1", 2, RecordState::Retracted, "", "");
    // Fails if a tombstoned record can change state.
    assert_eq!(
        store.set_state(&forgotten).unwrap_err().kind,
        ErrorKind::Conflict
    );
    let forgotten_verify = verify_request(&scope, "rec-1", 2, None, at(10));
    assert_eq!(
        store.verify(&forgotten_verify).unwrap_err().kind,
        ErrorKind::Conflict
    );
    assert!(!record_path(&store, "rec-1").exists());
}

#[test]
fn verify_sets_the_verification_time_and_replaces_the_review_deadline() {
    let (_dir, store) = open_temp();
    let scope = user(&store);
    let original = put(&store, "rec-1", &scope, "tone", "one");
    let first = store
        .verify(&verify_request(&scope, "rec-1", 1, Some(at(100)), at(10)))
        .unwrap();
    assert_eq!(first.revision, 2);
    // Fails if verify does not record the verification time.
    assert_eq!(first.verified_at, Some(at(10)));
    assert_eq!(first.review_at, Some(at(100)));
    // Fails if verify moves the content update time.
    assert_eq!(first.updated_at, original.updated_at);
    assert!(history_file(&store, "rec-1", "1.md").is_file());
    let second = store
        .verify(&verify_request(&scope, "rec-1", 2, None, at(20)))
        .unwrap();
    // Fails if a verify without a deadline keeps the old deadline.
    assert_eq!(second.review_at, None);
    assert_eq!(second.verified_at, Some(at(20)));
    assert_eq!(second.revision, 3);
    assert!(history_file(&store, "rec-1", "2.md").is_file());
}

#[test]
fn verify_returns_needs_review_to_active_and_keeps_other_states() {
    let (_dir, store) = open_temp();
    let scope = user(&store);
    put(&store, "rec-1", &scope, "tone", "one");
    put(&store, "rec-2", &scope, "other", "two");
    store
        .set_state(&set_state_request(
            &scope,
            "rec-1",
            1,
            RecordState::NeedsReview,
            "",
            "unsure",
        ))
        .unwrap();
    let verified = store
        .verify(&verify_request(&scope, "rec-1", 2, None, at(10)))
        .unwrap();
    // Fails if verify leaves a needs_review record in needs_review.
    assert_eq!(verified.state, RecordState::Active);
    assert_eq!(verified.state_reason, "");

    store
        .set_state(&set_state_request(
            &scope,
            "rec-2",
            1,
            RecordState::Retracted,
            "",
            "wrong",
        ))
        .unwrap();
    let again = store
        .verify(&verify_request(&scope, "rec-2", 2, None, at(11)))
        .unwrap();
    // Fails if verify reactivates a retracted record.
    assert_eq!(again.state, RecordState::Retracted);
    assert_eq!(again.state_reason, "wrong");
    assert_eq!(again.verified_at, Some(at(11)));
    let twice = store
        .verify(&verify_request(&scope, "rec-2", 3, None, at(12)))
        .unwrap();
    assert_eq!(twice.state, RecordState::Retracted);
}

#[test]
fn normal_recall_returns_only_effective_records() {
    let (_dir, store) = open_temp();
    let scope = user(&store);
    records_in_every_recall_state(&store, &scope);
    // Fails if recall returns a needs_review, superseded, retracted, expired or review-due record.
    assert_eq!(retrieved_ids(&store, &scope, false), vec!["rec-eff"]);
    // include_expired widens expiry only: the review-due and non-active records stay out.
    assert_eq!(
        retrieved_ids(&store, &scope, true),
        vec!["rec-eff", "rec-expired"]
    );
}

#[test]
fn the_standing_set_and_its_usage_include_only_effective_records() {
    let (_dir, store) = open_temp();
    let scope = user(&store);
    records_in_every_recall_state(&store, &scope);
    let standing: Vec<String> = store
        .standing_records(&scope, at(5))
        .unwrap()
        .into_iter()
        .map(|record| record.id)
        .collect();
    // Fails if a non-effective record is in the standing set.
    assert_eq!(standing, vec!["rec-eff"]);
    let usage = store.standing_usage(&scope, at(5)).unwrap();
    assert_eq!(usage.records, 1);
    assert_eq!(usage.omitted, 0);
    let mut baseline = request(vec![scope.clone()], "");
    baseline.include_baseline = true;
    let result = store.retrieve(&baseline).unwrap();
    assert_eq!(ids(&result), vec!["rec-eff"]);
}

#[test]
fn the_standing_budget_ignores_records_that_are_not_effective() {
    let (_dir, store) = open_temp();
    let scope = user(&store);
    let big = "x".repeat(600);
    let mut written = 0;
    loop {
        let result = store.upsert_within_standing_budget(&UpsertRequest {
            record: sample_record(
                &format!("rec-{written}"),
                &scope,
                &format!("k{written}"),
                &big,
            ),
            expected_revision: None,
        });
        if result.is_err() {
            break;
        }
        written += 1;
        assert!(written < 100, "budget never filled");
    }
    assert!(written > 0);
    let next = UpsertRequest {
        record: sample_record("rec-next", &scope, "k-next", &big),
        expected_revision: None,
    };
    assert!(store.upsert_within_standing_budget(&next).is_err());
    for index in 0..written {
        store
            .set_state(&set_state_request(
                &scope,
                &format!("rec-{index}"),
                1,
                RecordState::Retracted,
                "",
                "full",
            ))
            .unwrap();
    }
    // Fails if retracted records still count against the budget.
    store
        .upsert_within_standing_budget(&next)
        .expect("budget is free again");
    assert_eq!(store.standing_usage(&scope, at(5)).unwrap().records, 1);
}

#[test]
fn reads_never_write_the_record_file_or_the_verification_time() {
    let (_dir, store) = open_temp();
    let scope = user(&store);
    put(&store, "rec-1", &scope, "tone", "one");
    store
        .verify(&verify_request(&scope, "rec-1", 1, None, at(2)))
        .unwrap();
    put(&store, "rec-2", &scope, "other", "two");
    let before = stored_files(&store);
    let record_one = record_ref(&scope, "rec-1");
    store
        .retrieve(&request(vec![scope.clone()], "one"))
        .unwrap();
    store.list(&list_request(&scope)).unwrap();
    store.get(&record_one).unwrap();
    store.get(&record_ref(&scope, "rec-2")).unwrap();
    store
        .get_by_key(&RecordKey {
            scope: scope.clone(),
            kind: "preference".into(),
            key: "other".into(),
        })
        .unwrap();
    store.history(&record_one).unwrap();
    // Fails if any read rewrites a record or history file.
    assert_eq!(stored_files(&store), before);
    assert_eq!(
        store.get(&record_ref(&scope, "rec-2")).unwrap().verified_at,
        None
    );
    assert_eq!(store.get(&record_one).unwrap().verified_at, Some(at(2)));
}

pub(super) fn list_request(scope: &Scope) -> ListRequest {
    ListRequest {
        all_scopes: false,
        scopes: vec![scope.clone()],
        kinds: Vec::new(),
        labels: Vec::new(),
        limit: 20,
        cursor: String::new(),
        now: at(5),
        include_expired: false,
        states: Vec::new(),
        updated_after: None,
        verified_after: None,
        unverified_only: false,
        review_due_only: false,
    }
}

fn listed_ids(store: &FileStore, request: &ListRequest) -> Vec<String> {
    let mut ids: Vec<String> = store
        .list(request)
        .expect("list")
        .records
        .into_iter()
        .map(|record| record.id)
        .collect();
    ids.sort();
    ids
}

/// a, b, e and f are active; c needs review; d is retracted. Only b was
/// updated at(10); e was verified at(20), f at(1) with a review deadline at(2).
fn records_for_list_filters(store: &FileStore, scope: &Scope) {
    for (id, key, second) in [("rec-a", "ka", 0), ("rec-b", "kb", 10), ("rec-c", "kc", 0)] {
        let mut record = sample_record(id, scope, key, &format!("text of {id}"));
        record.created_at = at(0);
        record.updated_at = at(second);
        store
            .upsert(&UpsertRequest {
                record,
                expected_revision: None,
            })
            .unwrap();
    }
    for (id, key) in [("rec-d", "kd"), ("rec-e", "ke"), ("rec-f", "kf")] {
        put(store, id, scope, key, &format!("text of {id}"));
    }
    store
        .set_state(&set_state_request(
            scope,
            "rec-c",
            1,
            RecordState::NeedsReview,
            "",
            "",
        ))
        .unwrap();
    store
        .set_state(&set_state_request(
            scope,
            "rec-d",
            1,
            RecordState::Retracted,
            "",
            "wrong",
        ))
        .unwrap();
    store
        .verify(&verify_request(scope, "rec-e", 1, None, at(20)))
        .unwrap();
    store
        .verify(&verify_request(scope, "rec-f", 1, Some(at(2)), at(1)))
        .unwrap();
}

#[test]
fn list_filters_by_state_time_verification_and_review_deadline() {
    let (_dir, store) = open_temp();
    let scope = user(&store);
    records_for_list_filters(&store, &scope);
    let base = list_request(&scope);
    // Fails if the default list includes non-active records.
    assert_eq!(
        listed_ids(&store, &base),
        ["rec-a", "rec-b", "rec-e", "rec-f"]
    );

    let review = ListRequest {
        states: vec![RecordState::NeedsReview],
        ..base.clone()
    };
    assert_eq!(listed_ids(&store, &review), ["rec-c"]);
    let all = ListRequest {
        states: RecordState::ALL.to_vec(),
        ..base.clone()
    };
    assert_eq!(
        listed_ids(&store, &all),
        ["rec-a", "rec-b", "rec-c", "rec-d", "rec-e", "rec-f"]
    );

    let updated = ListRequest {
        updated_after: Some(at(10)),
        ..base.clone()
    };
    // Fails if updated_after compares the creation or lifecycle time instead of updated_at.
    assert_eq!(listed_ids(&store, &updated), ["rec-b"]);
    let verified = ListRequest {
        verified_after: Some(at(15)),
        ..base.clone()
    };
    assert_eq!(listed_ids(&store, &verified), ["rec-e"]);
    let verified_early = ListRequest {
        verified_after: Some(at(1)),
        ..base.clone()
    };
    assert_eq!(listed_ids(&store, &verified_early), ["rec-e", "rec-f"]);
    let unverified = ListRequest {
        unverified_only: true,
        ..base.clone()
    };
    assert_eq!(listed_ids(&store, &unverified), ["rec-a", "rec-b"]);
    let unverified_all = ListRequest {
        unverified_only: true,
        states: RecordState::ALL.to_vec(),
        ..base.clone()
    };
    assert_eq!(
        listed_ids(&store, &unverified_all),
        ["rec-a", "rec-b", "rec-c", "rec-d"]
    );
    let due = ListRequest {
        review_due_only: true,
        ..base.clone()
    };
    assert_eq!(listed_ids(&store, &due), ["rec-f"]);
    let not_yet_due = ListRequest {
        review_due_only: true,
        now: at(1),
        ..base
    };
    assert!(listed_ids(&store, &not_yet_due).is_empty());
}

#[test]
fn list_validation_rejects_duplicate_states_and_contradictory_verification_filters() {
    let (_dir, store) = open_temp();
    let scope = user(&store);
    let base = list_request(&scope);
    let duplicate = ListRequest {
        states: vec![RecordState::Active, RecordState::Active],
        ..base.clone()
    };
    assert_eq!(
        store.list(&duplicate).unwrap_err().kind,
        ErrorKind::InvalidRequest
    );
    let contradiction = ListRequest {
        unverified_only: true,
        verified_after: Some(at(1)),
        ..base
    };
    assert_eq!(
        store.list(&contradiction).unwrap_err().kind,
        ErrorKind::InvalidRequest
    );
}

#[test]
fn a_list_cursor_is_rejected_under_a_different_state_filter() {
    let (_dir, store) = open_temp();
    let scope = user(&store);
    records_for_list_filters(&store, &scope);
    let first = ListRequest {
        limit: 1,
        ..list_request(&scope)
    };
    let page = store.list(&first).unwrap();
    assert!(!page.next_cursor.is_empty());
    let same = ListRequest {
        cursor: page.next_cursor.clone(),
        ..first.clone()
    };
    assert_eq!(store.list(&same).unwrap().records.len(), 1);
    for other in [
        ListRequest {
            states: vec![RecordState::NeedsReview],
            ..same.clone()
        },
        ListRequest {
            states: RecordState::ALL.to_vec(),
            ..same.clone()
        },
        ListRequest {
            unverified_only: true,
            ..same.clone()
        },
        ListRequest {
            updated_after: Some(at(1)),
            ..same.clone()
        },
        ListRequest {
            verified_after: Some(at(1)),
            ..same.clone()
        },
        ListRequest {
            review_due_only: true,
            ..same.clone()
        },
    ] {
        // Fails if a filter is missing from the cursor fingerprint.
        assert_eq!(
            store.list(&other).unwrap_err().kind,
            ErrorKind::InvalidCursor
        );
    }
}

#[test]
fn restoring_a_revision_that_was_retracted_yields_an_active_unverified_record() {
    let (_dir, store) = open_temp();
    let scope = user(&store);
    put(&store, "rec-1", &scope, "tone", "first text");
    store
        .set_state(&set_state_request(
            &scope,
            "rec-1",
            1,
            RecordState::Retracted,
            "",
            "wrong",
        ))
        .unwrap();
    store
        .verify(&verify_request(&scope, "rec-1", 2, Some(at(50)), at(12)))
        .unwrap();
    let retracted = store
        .history(&record_ref(&scope, "rec-1"))
        .unwrap()
        .revisions[1]
        .clone();
    assert_eq!(retracted.state, RecordState::Retracted);
    let restored = store
        .restore(&StoreRestoreRequest {
            reference: record_ref(&scope, "rec-1"),
            revision: 2,
            expected_revision: 3,
            restored_at: at(30),
        })
        .unwrap();
    // Fails if restore copies the old state, reason, verification time or deadline.
    assert_eq!(restored.state, RecordState::Active);
    assert_eq!(restored.state_reason, "");
    assert_eq!(restored.superseded_by, "");
    assert_eq!(restored.verified_at, None);
    assert_eq!(restored.review_at, None);
    assert_eq!(restored.revision, 4);
}
