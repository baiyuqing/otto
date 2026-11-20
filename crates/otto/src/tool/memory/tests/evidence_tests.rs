use super::*;
use crate::memory::{CandidateAction, RecordRef, RecordState};

#[tokio::test]
async fn evidence_model_write_is_pending_and_condition_report_preserves_bindings() {
    let (_dir, service) = service();
    let source = service
        .remember(&RememberRequest {
            scope: workspace_scope(),
            kind: "fact".into(),
            text: "reported evidence".into(),
            ..RememberRequest::default()
        })
        .unwrap();
    let tool = remember_tool(&service);
    let arguments = serde_json::json!({
        "kind":"fact", "text":"derived conclusion", "applicability":"reported conditions hold",
        "evidence":[{"type":"record", "id":source.id, "revision":source.revision, "provenance":"user"}]
    });
    let result = run(&tool, &arguments.to_string()).await;
    assert!(!result.is_error, "{result:?}");
    let reference = CandidateRef {
        scope: workspace_scope(),
        id: candidate_id(&result.content),
    };
    let candidate = service.get_candidate(&reference).unwrap();
    assert_eq!(candidate.state, CandidateState::Pending);
    assert_eq!(
        crate::memory::evidence::read(&candidate.proposed.metadata)
            .unwrap()
            .len(),
        1
    );
    let accepted = service
        .review(&ReviewRequest {
            reference,
            decision: ReviewDecision::Accept,
            edited: None,
            target_revision: None,
        })
        .unwrap()
        .record
        .unwrap();
    let mut arguments = serde_json::json!({
        "kind":"fact", "text":"conditions changed", "target_id":accepted.id,
        "base_revision":accepted.revision, "condition_changed":true, "reason":"reported by user"
    });
    let result = run(&tool, &arguments.to_string()).await;
    assert!(!result.is_error, "{result:?}");
    assert_eq!(
        service
            .get(&RecordRef {
                scope: accepted.scope.clone(),
                id: accepted.id.clone()
            })
            .unwrap()
            .state,
        RecordState::Active
    );
    let reference = CandidateRef {
        scope: workspace_scope(),
        id: candidate_id(&result.content),
    };
    let candidate = service.get_candidate(&reference).unwrap();
    assert_eq!(candidate.action, CandidateAction::NeedsReview);
    let reviewed = service
        .review(&ReviewRequest {
            reference,
            decision: ReviewDecision::Accept,
            edited: None,
            target_revision: None,
        })
        .unwrap()
        .record
        .unwrap();
    assert_eq!(reviewed.state, RecordState::NeedsReview);
    assert_eq!(
        crate::memory::evidence::read(&reviewed.metadata).unwrap(),
        crate::memory::evidence::read(&accepted.metadata).unwrap()
    );
    arguments["condition_changed"] = serde_json::json!(false);
    arguments["base_revision"] = serde_json::json!(reviewed.revision);
    let result = run(&tool, &arguments.to_string()).await;
    assert!(
        result.is_error,
        "cannot drop invalid evidence through ordinary updates: {result:?}"
    );
}

#[tokio::test]
async fn evidence_tool_requires_applicability_and_rejects_unbound_condition_reports() {
    let (_dir, service) = service();
    let tool = remember_tool(&service);
    for arguments in [
        r#"{"kind":"fact","text":"conclusion","evidence":[{"type":"observation","session_id":"s1","event_id":"e1","provenance":"tool"}]}"#,
        r#"{"kind":"fact","text":"conclusion","condition_changed":true}"#,
    ] {
        assert!(run(&tool, arguments).await.is_error);
    }
    let definition = super::super::super::memory_service::service_definition(
        otto_core::memory::Operation::Remember,
    );
    let schema: serde_json::Value =
        serde_json::from_str(definition.parameters.as_ref().unwrap().get()).unwrap();
    for field in ["evidence", "applicability", "condition_changed"] {
        assert!(
            schema["properties"].get(field).is_none(),
            "plugin schema must not promise built-in evidence"
        );
    }
}
