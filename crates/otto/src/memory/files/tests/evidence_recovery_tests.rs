use super::*;

#[test]
fn evidence_upstream_restore_and_restart_never_restore_downstream() {
    let (_dir, store) = open_temp();
    let (a, b, c) = chain(&store);
    let a = update(&store, &a);
    let a = store
        .restore(&StoreRestoreRequest {
            reference: record_ref(&a.scope, &a.id),
            revision: 1,
            expected_revision: a.revision,
            restored_at: Utc::now(),
        })
        .unwrap();
    assert_eq!(a.state, RecordState::Active);
    assert_eq!(get(&store, &b).state, RecordState::NeedsReview);
    let reopened = FileStore::open(
        store.root(),
        crate::memory::repository::MemoryOptions::default(),
    )
    .unwrap();
    assert_eq!(get(&reopened, &c).state, RecordState::NeedsReview);
    assert_eq!(get(&reopened, &b).revision, 2);
}

#[test]
fn evidence_successful_verify_checks_current_pins_and_does_not_restore_dependents() {
    let (_dir, store) = open_temp();
    let (_a, b, c) = chain(&store);
    let b = verify(&store, &b).unwrap();
    assert_eq!(b.state, RecordState::Active);
    assert!(b.verified_at.is_some());
    assert_eq!(get(&store, &c).state, RecordState::NeedsReview);
}

#[test]
fn evidence_replacement_cannot_depend_transitively_on_its_retired_source() {
    let (_dir, store) = open_temp();
    let (a, _b, c) = chain(&store);
    let mut replacement = sample_record("replacement", &a.scope, "", "replacement conclusion");
    evidence::bind(&mut replacement.metadata, &[anchor(&c)], "condition", false).unwrap();
    let snapshot = store.snapshot().unwrap();
    assert!(
        store
            .check_evidence(&snapshot, &replacement, Some(&a.id))
            .is_err()
    );
}

#[test]
fn evidence_forgotten_source_allows_dependent_retirement() {
    for action in [
        CandidateAction::Retract,
        CandidateAction::Correct,
        CandidateAction::Supersede,
    ] {
        let (_dir, store) = open_temp();
        let (a, b, _) = chain(&store);
        store
            .forget(&StoreForgetRequest {
                reference: record_ref(&a.scope, &a.id),
                expected_revision: a.revision,
                forgotten_at: Utc::now(),
            })
            .unwrap();
        let b = get(&store, &b);
        assert_eq!(b.state, RecordState::NeedsReview);
        store
            .propose(&[proposal(&b.scope, "retire", action, &b.id, b.revision)])
            .unwrap();
        let mut request = accept(
            &b.scope,
            "retire",
            if action == CandidateAction::Retract {
                ""
            } else {
                "replacement"
            },
        );
        request.decided_at = Utc::now();
        store.review(&request).unwrap();
        let retired = get(&store, &b);
        assert_eq!(
            retired.state,
            if action == CandidateAction::Supersede {
                RecordState::Superseded
            } else {
                RecordState::Retracted
            }
        );
        assert_eq!(retired.metadata, b.metadata);
        assert!(
            store.get(&record_ref(&b.scope, "replacement")).is_ok()
                == (action != CandidateAction::Retract)
        );
    }
}

#[test]
fn evidence_failed_retirement_does_not_publish_replacement() {
    for action in [CandidateAction::Correct, CandidateAction::Supersede] {
        let (_dir, store) = open_temp();
        let (_, mut b, _) = chain(&store);
        // Simulate an existing inactive dependency cycle: retirement must still
        // validate it before publishing an otherwise valid replacement.
        b.state = RecordState::NeedsReview;
        let self_anchor = anchor(&b);
        evidence::bind(&mut b.metadata, &[self_anchor], "condition", false).unwrap();
        store.put_record(&b).unwrap();
        store
            .propose(&[proposal(&b.scope, "retire", action, &b.id, b.revision)])
            .unwrap();
        let mut request = accept(&b.scope, "retire", "replacement");
        request.decided_at = Utc::now();
        assert!(store.review(&request).is_err());
        assert!(store.get(&record_ref(&b.scope, "replacement")).is_err());
        assert_eq!(get(&store, &b), b);
        assert_eq!(
            store
                .get_candidate(&candidate_ref(&b.scope, "retire"))
                .unwrap()
                .state,
            CandidateState::Pending
        );
    }
}

#[test]
fn evidence_inactive_update_cannot_introduce_missing_or_cross_scope_anchors() {
    let (_dir, store) = open_temp();
    let (_, b, _) = chain(&store);
    put(
        &store,
        "other-scope",
        &Scope::new(NAMESPACE_WORKSPACE, "foreign"),
        "",
        "foreign source",
    );
    for id in ["missing", "other-scope"] {
        let mut desired = b.clone();
        desired.state = RecordState::Retracted;
        let mut source = b.clone();
        source.id = id.into();
        evidence::bind(
            &mut desired.metadata,
            &[anchor(&source)],
            "condition",
            false,
        )
        .unwrap();
        assert!(
            store
                .upsert(&UpsertRequest {
                    record: desired,
                    expected_revision: Some(b.revision)
                })
                .is_err()
        );
    }
}

#[test]
fn evidence_inactive_update_cannot_introduce_stale_revision() {
    let (_dir, store) = open_temp();
    let scope = user(&store);
    let source = put(&store, "source", &scope, "", "source");
    let current = update(&store, &source);
    assert_ne!(source.revision, current.revision);
    assert!(current.effective(Utc::now()));
    assert_inactive_targets_reject_new_anchor(&store, &source);
}

#[test]
fn evidence_inactive_update_cannot_introduce_ineffective_source() {
    let (_dir, store) = open_temp();
    let scope = user(&store);
    let mut source = sample_record("source", &scope, "", "source");
    source.state = RecordState::NeedsReview;
    let source = store
        .upsert(&UpsertRequest {
            record: source,
            expected_revision: None,
        })
        .unwrap();
    assert_eq!(get(&store, &source).revision, source.revision);
    assert!(!source.effective(Utc::now()));
    assert_inactive_targets_reject_new_anchor(&store, &source);
}

fn assert_inactive_targets_reject_new_anchor(store: &FileStore, source: &Record) {
    for state in [
        RecordState::NeedsReview,
        RecordState::Retracted,
        RecordState::Superseded,
    ] {
        for condition_changed in [false, true] {
            let mut target = sample_record(
                &format!("target-{state:?}-{condition_changed}"),
                &source.scope,
                "",
                "inactive target",
            );
            target.state = state;
            if state == RecordState::Superseded {
                target.superseded_by = source.id.clone();
            }
            let target = store
                .upsert(&UpsertRequest {
                    record: target,
                    expected_revision: None,
                })
                .unwrap();
            let mut desired = target.clone();
            evidence::bind(
                &mut desired.metadata,
                &[anchor(source)],
                "condition",
                condition_changed,
            )
            .unwrap();
            assert!(
                store
                    .upsert(&UpsertRequest {
                        record: desired,
                        expected_revision: Some(target.revision),
                    })
                    .is_err(),
                "new invalid anchor accepted for {state:?}, condition_changed={condition_changed}"
            );
            assert_eq!(get(store, &target), target);
        }
    }
}

#[test]
fn evidence_human_keyed_service_update_preserves_invalidated_bindings() {
    use crate::memory::{RememberRequest, Service};
    let (_dir, store) = open_temp();
    let (a, mut b, _) = chain(&store);
    b.key = "derived".into();
    let b = store
        .upsert(&UpsertRequest {
            record: b.clone(),
            expected_revision: Some(b.revision),
        })
        .unwrap();
    update(&store, &a);
    let invalid = get(&store, &b);
    let service = Service::new(store, crate::memory::decide_default_policy);
    let request = RememberRequest {
        scope: b.scope.clone(),
        kind: b.kind.clone(),
        key: b.key.clone(),
        text: "ordinary HUMAN keyed update".into(),
        ..RememberRequest::default()
    };
    let updated = service.remember(&request).unwrap();
    assert_eq!(updated.id, b.id);
    assert_eq!(updated.state, RecordState::NeedsReview);
    assert_eq!(updated.metadata, invalid.metadata);
    assert_eq!(
        updated
            .metadata
            .get(evidence::CONDITION_CHANGED)
            .map(String::as_str),
        Some("true")
    );
    let mut explicit = request;
    evidence::bind(&mut explicit.metadata, &[], "independent conclusion", false).unwrap();
    let cleared = service.remember(&explicit).unwrap();
    assert_eq!(cleared.state, RecordState::Active);
    assert!(evidence::read(&cleared.metadata).unwrap().is_empty());
}

#[test]
fn evidence_review_marker_has_reserved_metadata_capacity() {
    let (_dir, store) = open_temp();
    let scope = user(&store);
    let a = put(&store, "a", &scope, "", "source");
    let mut record = sample_record("b", &scope, "", "conclusion");
    for index in 0..31 {
        record
            .metadata
            .insert(format!("custom.{index}"), "value".into());
    }
    assert!(evidence::bind(&mut record.metadata, &[anchor(&a)], "condition", false).is_err());
}
