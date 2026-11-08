//! `POST /v1/sessions/{id}/sandbox/changes`.

use super::*;

fn seatbelt(network: SandboxNetwork) -> SandboxInfo {
    fixtures::seatbelt_info(network)
}

fn sandbox_options(failure: Option<&str>) -> HarnessOptions {
    let info = seatbelt(SandboxNetwork::Denied);
    let (control, _calls) = fixtures::FakeSandbox::new(info.clone(), info.clone(), failure);
    HarnessOptions {
        sandbox_reload: Some(Ok(info)),
        sandbox_control: Some(control),
        ..Default::default()
    }
}

async fn change(harness: &Harness, session: &str, body: &str) -> Reply {
    harness
        .send(
            "POST",
            &format!("/v1/sessions/{session}/sandbox/changes"),
            Some(body),
        )
        .await
}

fn config_text(harness: &Harness) -> String {
    std::fs::read_to_string(&harness.factory.builder.shared.config_path).unwrap_or_default()
}

fn error_code(reply: &Reply) -> String {
    reply.json()["error"]["code"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

#[tokio::test]
async fn allow_read_path_resolves_writes_the_configuration_and_returns_a_followup() {
    let harness = Harness::with(sandbox_options(None));
    let id = harness.create().await;
    let granted = tempfile::tempdir().expect("granted directory");
    let canonical = std::fs::canonicalize(granted.path()).expect("canonical path");
    let body = serde_json::json!({
        "change": "allow_read_path",
        "value": granted.path().to_string_lossy(),
    });
    let reply = change(&harness, &id, &body.to_string()).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let body = reply.json();
    assert_eq!(body["mode"], "seatbelt");
    assert!(body["summary"].is_string(), "{body}");
    assert!(
        body["change_summary"]
            .as_str()
            .is_some_and(|text| text.contains(canonical.to_str().expect("utf-8 path"))),
        "{body}"
    );
    assert!(
        body["config_path"]
            .as_str()
            .is_some_and(|text| text.ends_with("config.toml")),
        "{body}"
    );
    assert!(
        body["followup"]
            .as_str()
            .is_some_and(|text| text.contains("retry it now")),
        "{body}"
    );
    assert!(body.get("workspaces").is_none(), "{body}");
    assert!(
        config_text(&harness).contains(canonical.to_str().expect("utf-8 path")),
        "{}",
        config_text(&harness)
    );
}

#[tokio::test]
async fn network_allow_has_a_followup_and_network_deny_has_none() {
    let harness = Harness::with(sandbox_options(None));
    let id = harness.create().await;
    let allow = change(&harness, &id, r#"{"change":"network","value":"allow"}"#).await;
    assert_eq!(allow.status, StatusCode::OK, "{}", allow.body);
    assert!(allow.json()["followup"].is_string(), "{}", allow.body);
    assert!(config_text(&harness).contains("network = 'allow'"));

    let deny = change(&harness, &id, r#"{"change":"network","value":"deny"}"#).await;
    assert_eq!(deny.status, StatusCode::OK, "{}", deny.body);
    assert!(deny.json()["followup"].is_null(), "{}", deny.body);
    assert_eq!(deny.json()["change_summary"], "network deny");
    assert!(config_text(&harness).contains("network = 'deny'"));
}

#[tokio::test]
async fn exclude_command_writes_an_excluded_commands_entry() {
    let harness = Harness::with(sandbox_options(None));
    let id = harness.create().await;
    let reply = change(
        &harness,
        &id,
        r#"{"change":"exclude_command","value":"cargo *"}"#,
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert!(reply.json()["followup"].is_string(), "{}", reply.body);
    let text = config_text(&harness);
    assert!(
        text.contains("excluded_commands") && text.contains("cargo *"),
        "{text}"
    );
}

#[tokio::test]
async fn invalid_requests_are_400_and_write_nothing() {
    let harness = Harness::with(sandbox_options(None));
    let id = harness.create().await;
    for body in [
        "not json",
        r#"{"change":"network"}"#,
        r#"{"change":"unknown","value":"x"}"#,
        r#"{"change":"exclude_command","value":"  "}"#,
        r#"{"change":"network","value":"maybe"}"#,
        r#"{"change":"allow_read_path","value":"/nonexistent/otto-sandbox-change"}"#,
    ] {
        let reply = change(&harness, &id, body).await;
        assert_eq!(
            reply.status,
            StatusCode::BAD_REQUEST,
            "{body}: {}",
            reply.body
        );
        assert_eq!(error_code(&reply), "invalid_sandbox_change", "{body}");
    }
    assert_eq!(config_text(&harness), "");
}

#[tokio::test]
async fn unknown_session_is_404_and_a_missing_sandbox_is_501() {
    let harness = Harness::with(sandbox_options(None));
    let reply = change(
        &harness,
        "missing",
        r#"{"change":"network","value":"deny"}"#,
    )
    .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);

    let harness = Harness::new();
    let id = harness.create().await;
    let reply = change(&harness, &id, r#"{"change":"network","value":"deny"}"#).await;
    assert_eq!(reply.status, StatusCode::NOT_IMPLEMENTED, "{}", reply.body);
    assert_eq!(error_code(&reply), "not_implemented");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_change_is_refused_while_a_turn_is_active() {
    let (gate, gated_options) = gated();
    let harness = Harness::with(HarnessOptions {
        script: gated_options.script,
        ..sandbox_options(None)
    });
    let id = harness.create().await;
    let (_stream, _turn_id) = start_stream(&harness, &id).await;
    harness.provider.wait_started(1).await;
    let reply = change(&harness, &id, r#"{"change":"network","value":"allow"}"#).await;
    gate.cancel();
    assert_eq!(reply.status, StatusCode::CONFLICT, "{}", reply.body);
    assert_eq!(error_code(&reply), "turn_active");
    assert_eq!(config_text(&harness), "");
}

#[tokio::test]
async fn a_failed_reload_is_409_and_rolls_the_configuration_back() {
    let harness = Harness::with(sandbox_options(Some("driver refused the profile")));
    let id = harness.create().await;
    let reply = change(&harness, &id, r#"{"change":"network","value":"allow"}"#).await;
    assert_eq!(reply.status, StatusCode::CONFLICT, "{}", reply.body);
    assert_eq!(error_code(&reply), "sandbox_change_failed");
    assert!(
        reply.json()["error"]["message"]
            .as_str()
            .is_some_and(|text| text.contains("rolled back")),
        "{}",
        reply.body
    );
    assert!(!config_text(&harness).contains("network = 'allow'"));
}

#[tokio::test]
async fn other_workspaces_are_reloaded_and_listed() {
    let info = seatbelt(SandboxNetwork::Denied);
    let harness = Harness::with(HarnessOptions {
        sandbox_reload_others: vec![("/other".to_string(), Ok(info))],
        ..sandbox_options(None)
    });
    let id = harness.create().await;
    let reply = change(&harness, &id, r#"{"change":"network","value":"deny"}"#).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let body = reply.json();
    assert_eq!(
        body["workspaces"].as_array().map(Vec::len),
        Some(2),
        "{body}"
    );
    assert!(body["followup"].is_null(), "{body}");

    let failing = Harness::with(HarnessOptions {
        sandbox_reload_others: vec![("/other".to_string(), Err("no driver".to_string()))],
        ..sandbox_options(None)
    });
    let id = failing.create().await;
    let reply = change(&failing, &id, r#"{"change":"network","value":"deny"}"#).await;
    assert_eq!(reply.status, StatusCode::CONFLICT, "{}", reply.body);
    assert_eq!(error_code(&reply), "sandbox_reload_failed");
}
