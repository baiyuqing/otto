#[path = "evidence_recovery_tests.rs"]
mod recovery_tests;

use super::*;
use crate::memory::evidence::{self, Evidence, EvidenceProvenance};
use crate::memory::{StoreRestoreRequest, StoreVerifyRequest};
use chrono::Utc;

fn anchor(record: &Record) -> Evidence {
    Evidence::Record {
        id: record.id.clone(),
        revision: record.revision,
        provenance: EvidenceProvenance::User,
    }
}

fn dependent(store: &FileStore, scope: &Scope, id: &str, source: &Record) -> Record {
    let mut record = sample_record(id, scope, "", "derived conclusion");
    evidence::bind(
        &mut record.metadata,
        &[anchor(source)],
        "while the reported conditions hold",
        false,
    )
    .unwrap();
    store
        .upsert(&UpsertRequest {
            record,
            expected_revision: None,
        })
        .unwrap()
}

fn get(store: &FileStore, record: &Record) -> Record {
    store.get(&record_ref(&record.scope, &record.id)).unwrap()
}

fn update(store: &FileStore, record: &Record) -> Record {
    let mut next = record.clone();
    next.text.push_str(" changed");
    store
        .upsert(&UpsertRequest {
            record: next,
            expected_revision: Some(record.revision),
        })
        .unwrap()
}

fn chain(store: &FileStore) -> (Record, Record, Record) {
    let scope = user(store);
    let a = put(store, "a", &scope, "", "source");
    let b = dependent(store, &scope, "b", &a);
    let c = dependent(store, &scope, "c", &b);
    (a, b, c)
}

fn verify(store: &FileStore, record: &Record) -> Result<Record> {
    store.verify(&StoreVerifyRequest {
        reference: record_ref(&record.scope, &record.id),
        expected_revision: record.revision,
        verified_at: Utc::now(),
        review_at: None,
    })
}

#[test]
fn evidence_update_invalidates_chain_and_requires_explicit_rebinding() {
    let (_dir, store) = open_temp();
    let (a, b, c) = chain(&store);
    let a = update(&store, &a);
    let b = get(&store, &b);
    let c = get(&store, &c);
    assert_eq!(
        (b.state, c.state),
        (RecordState::NeedsReview, RecordState::NeedsReview)
    );
    assert!(verify(&store, &b).is_err());
    assert!(
        store
            .set_state(&StoreSetStateRequest {
                reference: record_ref(&b.scope, &b.id),
                expected_revision: b.revision,
                state: RecordState::Active,
                superseded_by: String::new(),
                reason: String::new(),
                changed_at: Utc::now(),
            })
            .is_err()
    );
    assert!(
        store
            .restore(&StoreRestoreRequest {
                reference: record_ref(&b.scope, &b.id),
                revision: 1,
                expected_revision: b.revision,
                restored_at: Utc::now(),
            })
            .is_err()
    );
    let mut rebound = b.clone();
    evidence::bind(
        &mut rebound.metadata,
        &[anchor(&a)],
        "conditions checked by the user",
        false,
    )
    .unwrap();
    rebound.state = RecordState::Active;
    rebound.state_reason.clear();
    let rebound = store
        .upsert(&UpsertRequest {
            record: rebound,
            expected_revision: Some(b.revision),
        })
        .unwrap();
    assert_eq!(rebound.state, RecordState::Active);
    assert_eq!(get(&store, &c).state, RecordState::NeedsReview);
}

#[test]
fn evidence_retirement_forget_expiry_and_review_due_invalidate_transitively() {
    for event in ["retract", "forget", "expire", "review", "condition"] {
        let (_dir, store) = open_temp();
        let (a, b, c) = chain(&store);
        match event {
            "retract" => {
                store
                    .set_state(&StoreSetStateRequest {
                        reference: record_ref(&a.scope, &a.id),
                        expected_revision: a.revision,
                        state: RecordState::Retracted,
                        superseded_by: String::new(),
                        reason: "withdrawn".into(),
                        changed_at: Utc::now(),
                    })
                    .unwrap();
            }
            "forget" => {
                store
                    .forget(&StoreForgetRequest {
                        reference: record_ref(&a.scope, &a.id),
                        expected_revision: a.revision,
                        forgotten_at: Utc::now(),
                    })
                    .unwrap();
            }
            event => {
                let mut edited = a.clone();
                match event {
                    "expire" => edited.expires_at = Some(Utc::now() - chrono::Duration::seconds(1)),
                    "review" => edited.review_at = Some(Utc::now() - chrono::Duration::seconds(1)),
                    "condition" => {
                        edited
                            .metadata
                            .insert(evidence::CONDITION_CHANGED.into(), "true".into());
                    }
                    _ => unreachable!(),
                }
                // Simulate passage of time or a person's edit without a revision change.
                store.put_record(&edited).unwrap();
            }
        }
        assert_eq!(get(&store, &b).state, RecordState::NeedsReview, "{event}");
        assert_eq!(get(&store, &c).state, RecordState::NeedsReview, "{event}");
    }
}

#[test]
fn evidence_rejects_cycles_self_references_and_cross_scope_sources() {
    let (_dir, store) = open_temp();
    let (a, _b, c) = chain(&store);
    for source in [&a, &c] {
        let mut next = a.clone();
        evidence::bind(&mut next.metadata, &[anchor(source)], "condition", false).unwrap();
        assert!(
            store
                .upsert(&UpsertRequest {
                    record: next,
                    expected_revision: Some(a.revision)
                })
                .is_err()
        );
    }
    let other_scope = Scope {
        namespace: NAMESPACE_WORKSPACE.into(),
        id: "other-workspace".into(),
    };
    let mut next = sample_record("other", &other_scope, "", "conclusion");
    evidence::bind(&mut next.metadata, &[anchor(&a)], "condition", false).unwrap();
    assert!(
        store
            .upsert(&UpsertRequest {
                record: next,
                expected_revision: None
            })
            .is_err()
    );
}

#[test]
fn evidence_temporary_missing_or_corrupt_source_is_not_invalidation() {
    let (_dir, store) = open_temp();
    let (a, b, c) = chain(&store);
    let path = record_path(&store, &a.id);
    let bytes = fs::read(&path).unwrap();
    fs::remove_file(&path).unwrap();
    assert_eq!(get(&store, &b).state, RecordState::Active);
    fs::write(&path, "temporarily incomplete").unwrap();
    assert_eq!(get(&store, &c).state, RecordState::Active);
    fs::write(&path, bytes).unwrap();
    assert_eq!(get(&store, &b).revision, b.revision);
}

#[test]
fn evidence_observation_is_an_anchor_not_external_monitoring() {
    let (_dir, store) = open_temp();
    let scope = user(&store);
    let mut record = sample_record("observed", &scope, "", "conclusion");
    let observation = Evidence::Observation {
        session_id: "session-1".into(),
        event_id: "event-1".into(),
        provenance: EvidenceProvenance::Tool,
    };
    evidence::bind(
        &mut record.metadata,
        &[observation.clone()],
        "reported environment unchanged",
        false,
    )
    .unwrap();
    let record = store
        .upsert(&UpsertRequest {
            record,
            expected_revision: None,
        })
        .unwrap();
    assert_eq!(
        evidence::read(&get(&store, &record).metadata).unwrap(),
        vec![observation]
    );
    let mut changed = record.clone();
    changed
        .metadata
        .insert(evidence::CONDITION_CHANGED.into(), "true".into());
    let changed = store
        .upsert(&UpsertRequest {
            record: changed,
            expected_revision: Some(record.revision),
        })
        .unwrap();
    assert_eq!(changed.state, RecordState::NeedsReview);
    assert!(verify(&store, &changed).is_err());
}
