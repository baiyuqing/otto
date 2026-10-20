//! Tests for compaction, skill, reflection, trace and context calls.

use super::*;

#[tokio::test]
async fn compact_decodes_a_compaction_and_maps_an_empty_body_to_none() {
    let (client, seen, _dir) = stub(|_, path| {
        if path.contains("full") {
            json_body(
                StatusCode::OK,
                r#"{"reason":"manual","tokens_before":900,"estimated_tokens_after":100}"#,
            )
        } else {
            Response::new(Body::empty())
        }
    });
    let done = client.compact("full", "api").await.unwrap().expect("some");
    assert_eq!(
        (done.tokens_before, done.estimated_tokens_after),
        (Some(900), Some(100))
    );
    assert_eq!(
        request(&seen, 0),
        (
            "POST /v1/sessions/full/compact".into(),
            json!({ "focus": "api" })
        )
    );
    assert_eq!(client.compact("cut", "").await.unwrap(), None);
}

#[tokio::test]
async fn skill_manager_client_uses_session_routes_and_encodes_qualified_names() {
    let (client, seen, _dir) = stub(|_, path| {
        if path.ends_with("/skill-manager") {
            json_body(StatusCode::OK, r#"{"global_enabled":false,"skills":[]}"#)
        } else if path.contains("/skill-manager/") {
            json_body(
                StatusCode::OK,
                r#"{"skill":{"name":"plugin:lint","description":"lint","contract":null,"directory":"/skills/lint","path":"/skills/lint/SKILL.md"},"body":"instructions","contract_check":null}"#,
            )
        } else {
            json_body(
                StatusCode::OK,
                r#"{"name":"plugin:lint","enabled":true,"restart_required":true}"#,
            )
        }
    });
    let inventory = client.skill_manager("s1").await.unwrap();
    assert!(!inventory.global_enabled);
    assert!(inventory.skills.is_empty());
    assert_eq!(
        client.skill_info("s1", "plugin:lint").await.unwrap().body,
        "instructions"
    );
    assert!(
        client
            .set_skill_enabled("s1", "plugin:lint", true)
            .await
            .unwrap()
            .contains("restart Otto")
    );
    assert_eq!(request(&seen, 0).0, "GET /v1/sessions/s1/skill-manager");
    assert_eq!(
        request(&seen, 1).0,
        "GET /v1/sessions/s1/skill-manager/plugin:lint"
    );
    assert_eq!(
        request(&seen, 2).0,
        "POST /v1/sessions/s1/skills/plugin:lint/enable"
    );
    client.skill_info("s1", "bad/name").await.unwrap();
    assert_eq!(
        request(&seen, 3).0,
        "GET /v1/sessions/s1/skill-manager/bad%2Fname"
    );
}

#[tokio::test]
async fn reflection_client_calls_use_the_existing_routes_and_empty_cancel_reply() {
    let (client, seen, _dir) = stub(|_, path| match path.split('?').next().unwrap_or(path) {
        "/v1/sessions/s1/reflect" => json_body(
            StatusCode::OK,
            r#"{"line":"reflection: 1 candidate queued"}"#,
        ),
        "/v1/sessions/s1/reflect/cancel" => json_body(StatusCode::ACCEPTED, ""),
        "/v1/sessions/s1/reflection/skills" => json_body(
            StatusCode::OK,
            r#"{"enabled":true,"skills":[{"name":"lint","run_id":"r1","session_id":"s1","reason":"reason","created_at":"now","updated_at":"now","owned":true}]}"#,
        ),
        "/v1/sessions/s1/notices" => json_body(
            StatusCode::OK,
            r#"{"notices":[{"id":3,"text":"reflection: done","presentation_origin":"chat:telegram:42"}],"last":3}"#,
        ),

        _ => json_body(StatusCode::OK, r#"{"name":"lint","result":"removed"}"#),
    });
    assert_eq!(
        client
            .reflect("s1", "focus", "acp-1")
            .await
            .unwrap()
            .unwrap()
            .line,
        "reflection: 1 candidate queued"
    );
    client.cancel_reflection("s1", "acp-1").await.unwrap();
    let skills = client.generated_skills("s1").await.unwrap();
    assert_eq!(skills.skills[0].name, "lint");
    assert_eq!(
        client.revert_generated_skill("s1", "lint").await.unwrap(),
        "removed"
    );
    let notices = client.reflection_notices("s1", 2).await.unwrap();
    assert_eq!(notices.notices[0].id, 3);
    assert_eq!(notices.notices[0].presentation_origin, "chat:telegram:42");
    assert_eq!(notices.last, 3);
    assert_eq!(
        request(&seen, 0).1,
        json!({"focus":"focus", "request_id":"acp-1"})
    );
    assert_eq!(request(&seen, 1).1, json!({"request_id":"acp-1"}));
    assert_eq!(request(&seen, 4).0, "GET /v1/sessions/s1/notices?after=2");
}

#[tokio::test]
async fn reflection_run_logs_use_their_routes_and_a_missing_run_is_none() {
    let (client, seen, _dir) = stub(|_, path| match path {
        "/v1/sessions/s1/reflection/runs" => json_body(
            StatusCode::OK,
            r#"{"enabled":true,"runs":[{"id":"abcd1234","path":"/h/.otto/reflection/runs/x.md","started":"2026-10-05T02:03:00Z","trigger":"requested","status":"ok","headline":"Read the docs first."}]}"#,
        ),
        "/v1/sessions/s1/reflection/runs/abcd1234" => json_body(
            StatusCode::OK,
            r##"{"id":"abcd1234","path":"/h/.otto/reflection/runs/x.md","content":"# Reflection run"}"##,
        ),
        _ => json_body(
            StatusCode::NOT_FOUND,
            r#"{"error":{"code":"not_found","message":"reflection run not found"}}"#,
        ),
    });
    let list = client.reflection_runs("s1").await.unwrap();
    assert!(list.enabled);
    assert_eq!(list.runs[0].id, "abcd1234");
    assert_eq!(list.runs[0].headline, "Read the docs first.");
    let log = client
        .reflection_run("s1", "abcd1234")
        .await
        .unwrap()
        .expect("log");
    assert_eq!(log.content, "# Reflection run");
    assert!(client.reflection_run("s1", "a/b").await.unwrap().is_none());
    assert_eq!(
        request(&seen, 2).0,
        "GET /v1/sessions/s1/reflection/runs/a%2Fb"
    );
}

#[tokio::test]
async fn trace_span_escapes_the_span_id_and_maps_404_to_none() {
    let (client, seen, _dir) = stub(|_, path| match path {
        "/v1/sessions/s1/trace/t1%3Acall%2F1" => {
            json_body(StatusCode::OK, r#"{"spanId":"t1:call/1","sections":[]}"#)
        }
        _ => json_body(
            StatusCode::NOT_FOUND,
            r#"{"error":{"code":"not_found","message":"span not found"}}"#,
        ),
    });

    let content = client.trace_span("s1", "t1:call/1").await.unwrap();
    assert_eq!(content.expect("content").span_id, "t1:call/1");
    assert!(client.trace_span("s1", "other").await.unwrap().is_none());
    assert_eq!(
        request(&seen, 0).0,
        "GET /v1/sessions/s1/trace/t1%3Acall%2F1"
    );
}

#[tokio::test]
async fn task_control_client_uses_session_routes_and_transient_channel() {
    let (client, seen, _dir) = stub(|_, path| match path {
        "/v1/sessions/s1/tasks/steer" => json_body(
            StatusCode::OK,
            r#"{"id":7,"text":"inspect the result","status":"queued"}"#,
        ),
        "/v1/sessions/s1/tasks/control" => json_body(
            StatusCode::OK,
            r#"{"text":"I can guide the task","queued":false}"#,
        ),
        _ => json_body(StatusCode::NOT_FOUND, ""),
    });

    let steer = client.steer("s1", "inspect the result").await.unwrap();
    assert_eq!(steer.id, 7);
    assert_eq!(steer.text, "inspect the result");
    assert_eq!(steer.status, otto_core::agent::steering::Status::Queued);
    let reply = client
        .control_message_with_channel("s1", "what can you do?", Some(Channel::Telegram))
        .await
        .unwrap()
        .expect("routed reply");
    assert_eq!(reply.text, "I can guide the task");
    assert!(!reply.queued);
    assert_eq!(
        request(&seen, 0),
        (
            "POST /v1/sessions/s1/tasks/steer".into(),
            json!({"text":"inspect the result"})
        )
    );
    assert_eq!(
        request(&seen, 1),
        (
            "POST /v1/sessions/s1/tasks/control".into(),
            json!({"text":"what can you do?","channel":"telegram"})
        )
    );
}

#[tokio::test]
async fn reflection_treats_an_empty_cancelled_response_as_none() {
    let (client, _, _dir) = stub(|_, _| json_body(StatusCode::OK, "  "));
    assert!(client.reflect("s1", "", "acp-2").await.unwrap().is_none());
}

#[tokio::test]
async fn context_decodes_the_report() {
    let report = ContextReport {
        model: "m".into(),
        context_window: 1000,
        compaction_threshold: 800,
        estimated_total: 120,
        estimate_complete: true,
        reported_input_tokens: Some(110),
        sections: vec![otto_core::agent::context_report::ContextSection {
            kind: otto_core::agent::context_report::SectionKind::Messages,
            tokens: 120,
            items: vec![],
        }],
    };
    let body = serde_json::to_string(&report).unwrap();
    let (client, seen, _dir) = stub(move |_, _| json_body(StatusCode::OK, &body));
    assert_eq!(client.context("s1").await.unwrap(), report);
    assert_eq!(request(&seen, 0).0, "GET /v1/sessions/s1/context");
}
