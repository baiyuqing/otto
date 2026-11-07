//! Tests for the typed management dispatch used by the HTTP memory routes:
//! failure categories, `expected_revision` and the paged candidate list.

use super::{ManagementErrorKind, Reply, Request};
use crate::memory::{CandidateAction, Origin, ProposeRequest, Provenance, RecordState};

struct Fixture {
    controller: crate::app::Controller,
    _directories: [tempfile::TempDir; 3],
}

async fn fixture() -> Fixture {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let controller = crate::fixtures::controller_with_memory(
        workspace.path(),
        sessions.path(),
        &store.path().join("memory"),
    )
    .await;
    Fixture {
        controller,
        _directories: [workspace, sessions, store],
    }
}

fn remember(controller: &crate::app::Controller, text: &str) -> crate::memory::Record {
    let request = Request::parse("remember", &format!("--scope workspace {text}")).unwrap();
    match controller.management_typed(request) {
        Ok(Reply::Remembered(record)) => record,
        other => panic!("{:?}", other.map(|_| ())),
    }
}

fn retract(id: &str, expected_revision: Option<u64>) -> Request {
    Request::MemoryState {
        id: id.into(),
        state: RecordState::Retracted,
        superseded_by: String::new(),
        reason: "wrong".into(),
        expected_revision,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stale_expected_revision_is_a_conflict_and_changes_nothing() {
    let fixture = fixture().await;
    let record = remember(&fixture.controller, "uses tabs");
    let verify = Request::MemoryVerify {
        id: record.id.clone(),
        review_at: None,
        expected_revision: Some(1),
    };
    match fixture.controller.management_typed(verify) {
        Ok(Reply::MemoryVerified(verified)) => assert_eq!(verified.revision, 2),
        other => panic!("{:?}", other.map(|_| ())),
    }

    let error = fixture
        .controller
        .management_typed(retract(&record.id, Some(1)))
        .map(|_| ())
        .expect_err("stale revision");
    assert_eq!(error.kind, ManagementErrorKind::Conflict, "{error}");
    let forget = Request::MemoryForget {
        id: record.id.clone(),
        expected_revision: Some(1),
    };
    let error = fixture
        .controller
        .management_typed(forget)
        .map(|_| ())
        .expect_err("stale revision");
    assert_eq!(error.kind, ManagementErrorKind::Conflict, "{error}");

    match fixture.controller.management_typed(Request::MemoryShow {
        id: record.id.clone(),
    }) {
        Ok(Reply::MemoryRecord(current)) => {
            assert_eq!(current.state, RecordState::Active);
            assert_eq!(current.revision, 2);
        }
        other => panic!("{:?}", other.map(|_| ())),
    }
    match fixture
        .controller
        .management_typed(retract(&record.id, Some(2)))
    {
        Ok(Reply::MemoryStateChanged(changed)) => assert_eq!(changed.state, RecordState::Retracted),
        other => panic!("{:?}", other.map(|_| ())),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_records_and_candidates_are_not_found_and_text_errors_keep_their_message() {
    let fixture = fixture().await;
    let error = fixture
        .controller
        .management_typed(retract("nope", None))
        .map(|_| ())
        .expect_err("missing");
    assert_eq!(error.kind, ManagementErrorKind::NotFound);
    let text = fixture
        .controller
        .management(retract("nope", None))
        .map(|_| ())
        .expect_err("missing");
    assert_eq!(
        text, error.message,
        "the text frontends print the same message"
    );

    let review = Request::MemoryReview {
        id: "nope".into(),
        decision: crate::memory::ReviewDecision::Accept,
        transport: "test",
    };
    let error = fixture
        .controller
        .management_typed(review)
        .map(|_| ())
        .expect_err("missing");
    assert_eq!(error.kind, ManagementErrorKind::NotFound);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_candidate_page_lists_pending_retracts_and_a_second_decision_conflicts() {
    let fixture = fixture().await;
    let controller = &fixture.controller;
    let record = remember(controller, "prefers npm");
    let (service, _, workspace) = controller.memory_manager().unwrap();
    for _ in 0..2 {
        service
            .propose(&ProposeRequest {
                action: CandidateAction::Retract,
                scope: workspace.clone(),
                target_id: record.id.clone(),
                base_revision: record.revision,
                reason: "switched to pnpm".into(),
                source: Provenance {
                    origin: Some(Origin::Model),
                    ..Provenance::default()
                },
                ..ProposeRequest::default()
            })
            .unwrap();
    }
    let page = |cursor: String| match controller
        .management_typed(Request::MemoryCandidatePage { limit: 1, cursor })
    {
        Ok(Reply::MemoryCandidatePage(page)) => page,
        other => panic!("{:?}", other.map(|_| ())),
    };
    let first = page(String::new());
    assert_eq!(first.candidates.len(), 1);
    assert_eq!(first.candidates[0].action, CandidateAction::Retract);
    assert!(!first.next_cursor.is_empty());
    let second = page(first.next_cursor.clone());
    assert_eq!(second.candidates.len(), 1);
    assert_ne!(second.candidates[0].id, first.candidates[0].id);

    let review = |id: &str| Request::MemoryReview {
        id: id.into(),
        decision: crate::memory::ReviewDecision::Accept,
        transport: "test",
    };
    match controller.management_typed(review(&first.candidates[0].id)) {
        Ok(Reply::MemoryReviewed(_, result)) => {
            assert_eq!(result.record.unwrap().state, RecordState::Retracted)
        }
        other => panic!("{:?}", other.map(|_| ())),
    }
    let error = controller
        .management_typed(review(&first.candidates[0].id))
        .map(|_| ())
        .expect_err("decided");
    assert_eq!(error.kind, ManagementErrorKind::Conflict, "{error}");
}

#[test]
fn store_error_kinds_map_to_client_categories() {
    use crate::memory::{Error, ErrorKind};
    for (store, expected) in [
        (ErrorKind::Disabled, ManagementErrorKind::Unavailable),
        (ErrorKind::Busy, ManagementErrorKind::Unavailable),
        (ErrorKind::Unsupported, ManagementErrorKind::Unsupported),
        (ErrorKind::NotFound, ManagementErrorKind::NotFound),
        (ErrorKind::Conflict, ManagementErrorKind::Conflict),
        (ErrorKind::InvalidCursor, ManagementErrorKind::Invalid),
        (ErrorKind::SensitiveMemory, ManagementErrorKind::Invalid),
        (ErrorKind::Corrupt, ManagementErrorKind::Internal),
    ] {
        let error = super::ManagementError::from(Error::new(store));
        assert_eq!(error.kind, expected, "{store:?}");
        assert_eq!(error.message, Error::new(store).to_string());
    }
}
