use super::{Reply, Request};
use crate::memory::RecordState;

#[tokio::test(flavor = "multi_thread")]
async fn evidence_human_inspect_condition_change_and_rebind_are_usable() {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let controller = crate::fixtures::controller_with_memory(
        workspace.path(),
        sessions.path(),
        &store.path().join("memory"),
    )
    .await;
    let mut records = Vec::new();
    for (key, text) in [
        ("source", "reported source"),
        ("derived", "derived conclusion"),
    ] {
        let request =
            Request::parse("remember", &format!("--scope workspace --key {key} {text}")).unwrap();
        match controller.management_typed(request).unwrap() {
            Reply::Remembered(record) => records.push(record),
            _ => panic!("remember reply"),
        }
    }
    let source = &records[0];
    let target = &records[1];
    let anchors = serde_json::json!([{"type":"record", "id":source.id, "revision":source.revision, "provenance":"user"}]);
    let bind = |revision| {
        Request::parse(
            "memory",
            &format!(
                "bind {} {revision} {anchors} reported conditions hold",
                target.id
            ),
        )
        .unwrap()
    };
    let bound = match controller.management_typed(bind(target.revision)).unwrap() {
        Reply::Remembered(record) => record,
        _ => panic!("bind reply"),
    };
    assert!(
        controller.management_typed(bind(target.revision)).is_err(),
        "stale CAS must fail"
    );
    let text = controller
        .management(Request::MemoryShow {
            id: target.id.clone(),
        })
        .unwrap();
    let text = super::reply_text::format_management_reply(&text);
    assert!(
        text.contains("applicability=reported conditions hold") && text.contains(&source.id),
        "{text}"
    );
    let changed = Request::parse(
        "memory",
        &format!(
            "condition-changed {} user reports a new environment",
            target.id
        ),
    )
    .unwrap();
    controller.management_typed(changed).unwrap();
    assert!(
        controller
            .management_typed(Request::MemoryVerify {
                id: target.id.clone(),
                review_at: None,
                expected_revision: None,
            })
            .is_err()
    );
    let updated = match controller
        .management_typed(
            Request::parse(
                "remember",
                "--scope workspace --key derived ordinary human update",
            )
            .unwrap(),
        )
        .unwrap()
    {
        Reply::Remembered(record) => record,
        _ => panic!("remember reply"),
    };
    assert_eq!(updated.state, RecordState::NeedsReview);
    assert_eq!(
        crate::memory::evidence::read(&updated.metadata).unwrap(),
        crate::memory::evidence::read(&bound.metadata).unwrap()
    );
    assert_eq!(
        updated
            .metadata
            .get(crate::memory::evidence::CONDITION_CHANGED)
            .map(String::as_str),
        Some("true")
    );
    let rebound = match controller.management_typed(bind(updated.revision)).unwrap() {
        Reply::Remembered(record) => record,
        _ => panic!("bind reply"),
    };
    assert_eq!(rebound.state, RecordState::Active);
    // Source revision invalidation must also survive the HUMAN keyed path.
    controller
        .management_typed(
            Request::parse("remember", "--scope workspace --key source revised source").unwrap(),
        )
        .unwrap();
    let attempted = controller.management_typed(
        Request::parse(
            "remember",
            "--scope workspace --key derived cannot erase evidence",
        )
        .unwrap(),
    );
    if let Ok(Reply::Remembered(record)) = attempted {
        assert_eq!(record.state, RecordState::NeedsReview);
        assert_eq!(
            crate::memory::evidence::read(&record.metadata).unwrap(),
            crate::memory::evidence::read(&rebound.metadata).unwrap()
        );
    } else {
        assert!(attempted.is_err());
    }
    // Explicit empty binding remains the deliberate removal path.
    let shown = controller
        .management_typed(Request::MemoryShow {
            id: target.id.clone(),
        })
        .unwrap();
    let revision = match shown {
        Reply::MemoryRecord(record) => record.revision,
        _ => panic!("show reply"),
    };
    let cleared = controller
        .management_typed(
            Request::parse(
                "memory",
                &format!(
                    "bind {} {revision} [] conclusion no longer relies on source",
                    target.id
                ),
            )
            .unwrap(),
        )
        .unwrap();
    match cleared {
        Reply::Remembered(record) => {
            assert_eq!(record.state, RecordState::Active);
            assert!(
                crate::memory::evidence::read(&record.metadata)
                    .unwrap()
                    .is_empty()
            );
        }
        _ => panic!("bind reply"),
    }
}
