//! Tests for compaction, skill, trace and context calls.

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

/// Fails without the client method: nothing posts the interrupt route or
/// decodes `queued_guidance`.
#[tokio::test]
async fn interrupt_steering_posts_the_route_and_decodes_the_count() {
    let (client, seen, _dir) = stub(|_, path| match path {
        "/v1/sessions/s1/tasks/steer/interrupt" => {
            json_body(StatusCode::OK, r#"{"queued_guidance":2}"#)
        }
        _ => json_body(StatusCode::NOT_FOUND, ""),
    });

    assert_eq!(client.interrupt_steering("s1").await.unwrap(), 2);
    assert_eq!(
        request(&seen, 0).0,
        "POST /v1/sessions/s1/tasks/steer/interrupt"
    );
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

#[tokio::test]
async fn wake_replies_decode_the_existing_cursor_page() {
    let (client, seen, _dir) = stub(|_, _| {
        json_body(
            StatusCode::OK,
            r#"{"notices":[{"id":2,"text":"wake reply"}],"last":2}"#,
        )
    });
    let page = client.wake_replies("session", 1).await.unwrap();
    assert_eq!(page.last, 2);
    assert_eq!(page.notices[0].id, 2);
    assert_eq!(page.notices[0].text, "wake reply");
    assert_eq!(
        request(&seen, 0).0,
        "GET /v1/sessions/session/wake-replies?after=1"
    );
}
