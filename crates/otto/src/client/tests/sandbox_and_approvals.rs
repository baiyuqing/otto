//! Tests for sandbox change and approve-always calls.

use super::*;

#[tokio::test]
async fn sandbox_change_posts_the_change_and_decodes_the_outcome() {
    let (client, seen, _dir) = stub(|_, _| {
        json_body(
            StatusCode::OK,
            r#"{"mode":"seatbelt","network":"allowed","bash_available":true,
                "summary":"Seatbelt sandbox, network allowed","change_summary":"network allow",
                "config_path":"/c/config.toml","followup":"retry"}"#,
        )
    });
    let outcome = client
        .sandbox_change("s1", "network", "allow")
        .await
        .unwrap();
    assert_eq!(
        outcome,
        SandboxChangeOutcome {
            sandbox_summary: "Seatbelt sandbox, network allowed".into(),
            summary: "network allow".into(),
            config_path: "/c/config.toml".into(),
            followup: Some("retry".into()),
        }
    );
    let (line, body) = request(&seen, 0);
    assert_eq!(line, "POST /v1/sessions/s1/sandbox/changes");
    assert_eq!(
        body,
        serde_json::json!({"change":"network","value":"allow"})
    );
}

#[tokio::test]
async fn sandbox_change_decodes_a_null_followup_and_reports_the_server_error() {
    let (client, _seen, _dir) = stub(|_, _| {
        json_body(
            StatusCode::OK,
            r#"{"summary":"s","change_summary":"network deny","config_path":"/c","followup":null}"#,
        )
    });
    assert_eq!(
        client
            .sandbox_change("s1", "network", "deny")
            .await
            .unwrap()
            .followup,
        None
    );
    let (client, _seen, _dir) = stub(|_, _| {
        json_body(
            StatusCode::CONFLICT,
            r#"{"error":{"code":"turn_active","message":"a turn is active"}}"#,
        )
    });
    let error = client
        .sandbox_change("s1", "network", "allow")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("a turn is active"), "{error}");
}

#[tokio::test]
async fn approve_always_posts_allow_with_always_and_returns_the_followup() {
    let (client, seen, _dir) = stub(|_, _| {
        json_body(
            StatusCode::OK,
            r#"{"decision":"allow","always":true,"followup":"retry echo hi"}"#,
        )
    });
    assert_eq!(
        client.approve_always("s1", "ap1").await.unwrap(),
        "retry echo hi"
    );
    let (line, body) = request(&seen, 0);
    assert_eq!(line, "POST /v1/sessions/s1/approvals/ap1");
    assert_eq!(body, serde_json::json!({"decision":"allow","always":true}));
    let (client, _seen, _dir) = stub(|_, _| {
        json_body(
            StatusCode::BAD_REQUEST,
            r#"{"error":{"code":"approval_always_failed","message":"cannot exclude this command"}}"#,
        )
    });
    let error = client.approve_always("s1", "ap1").await.unwrap_err();
    assert!(error.to_string().contains("cannot exclude"), "{error}");
}
