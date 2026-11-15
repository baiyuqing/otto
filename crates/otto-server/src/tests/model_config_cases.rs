//! Model selection and provider configuration routes.

use super::*;

const PROVIDER_CONFIG: &str = "# preserve\ndefault_model = \"work/small\"\n\n\
    [providers.work]\ntype = \"openai-compatible\"\nbase_url = \"https://example.test/v1\"\n\
    api_key_env = \"WORK_KEY\"\n\n[providers.work.models.small]\nthinking = \"low\"\n\n\
    [providers.work.models.large]\n";

fn config_harness() -> Harness {
    Harness::with(HarnessOptions {
        config_text: Some(PROVIDER_CONFIG.into()),
        ..Default::default()
    })
}

async fn post_change(harness: &Harness, body: &str) -> super::harness::Reply {
    harness
        .send_with(
            "POST",
            "/v1/config/changes",
            Some(body),
            &[("content-type", b"application/json")],
        )
        .await
}

#[tokio::test]
async fn switching_a_session_model_rebuilds_it_and_writes_no_file() {
    let harness = Harness::new();
    let id = harness.create().await;
    let reply = harness
        .send(
            "POST",
            &format!("/v1/sessions/{id}/model"),
            Some(r#"{"model":"beta/gpt-beta"}"#),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(reply.json()["provider"], "beta");
    assert_eq!(reply.json()["provider_type"], "openai-compatible");
    assert_eq!(reply.json()["model"], "gpt-beta");
    assert!(reply.json().get("profile").is_none(), "{}", reply.body);

    for (body, expected) in [
        (r#"{"model":"missing/gpt-beta"}"#, "missing"),
        (r#"{"model":"gpt-beta"}"#, "<provider>/<model_id>"),
        (r#"{"model":""}"#, "model must not be empty"),
    ] {
        let reply = harness
            .send("POST", &format!("/v1/sessions/{id}/model"), Some(body))
            .await;
        assert_eq!(
            reply.status,
            StatusCode::BAD_REQUEST,
            "{body}: {}",
            reply.body
        );
        assert_eq!(reply.json()["error"]["code"], "bad_request");
        assert!(reply.body.contains(expected), "{body}: {}", reply.body);
    }

    let reply = harness
        .send("POST", &format!("/v1/sessions/{id}/model"), Some("{}"))
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.body);

    let reply = harness
        .send(
            "POST",
            "/v1/sessions/missing/model",
            Some(r#"{"model":"beta/gpt-beta"}"#),
        )
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND, "{}", reply.body);

    // A session switch never writes the configuration file: every other
    // session and the next serve start keep their model.
    assert!(
        !harness.factory.builder.shared.config_path.exists(),
        "the model switch wrote a configuration file"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn switching_a_model_while_a_turn_runs_is_409_turn_active() {
    let (gate, options) = gated();
    let harness = Harness::with(options);
    let id = harness.create().await;
    let (_stream, _turn_id) = start_stream(&harness, &id).await;
    harness.provider.wait_started(1).await;

    let reply = harness
        .send(
            "POST",
            &format!("/v1/sessions/{id}/model"),
            Some(r#"{"model":"beta/gpt-beta"}"#),
        )
        .await;
    assert_eq!(reply.status, StatusCode::CONFLICT, "{}", reply.body);
    assert_eq!(reply.json()["error"]["code"], "turn_active");
    gate.cancel();
}

#[tokio::test]
async fn setting_a_session_thinking_level_returns_the_session_and_writes_no_file() {
    let harness = Harness::new();
    let id = harness.create().await;
    let path = format!("/v1/sessions/{id}/thinking");
    let reply = harness
        .send("POST", &path, Some(r#"{"thinking":"high"}"#))
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(reply.json()["thinking"], "high");
    let reply = harness
        .send("POST", &path, Some(r#"{"thinking":"unset"}"#))
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert!(
        reply.json().get("thinking").is_none_or(|v| v == ""),
        "{}",
        reply.body
    );

    let reply = harness
        .send(
            "POST",
            "/v1/sessions/missing/thinking",
            Some(r#"{"thinking":"high"}"#),
        )
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND, "{}", reply.body);
    assert!(
        !harness.factory.builder.shared.config_path.exists(),
        "the thinking change wrote a configuration file"
    );
}

#[tokio::test]
async fn an_invalid_session_thinking_level_is_400_invalid_thinking() {
    let harness = Harness::new();
    let id = harness.create().await;
    for body in [
        r#"{"thinking":"bogus"}"#,
        r#"{"thinking":""}"#,
        "{}",
        "not json",
    ] {
        let reply = harness
            .send("POST", &format!("/v1/sessions/{id}/thinking"), Some(body))
            .await;
        assert_eq!(
            reply.status,
            StatusCode::BAD_REQUEST,
            "{body}: {}",
            reply.body
        );
        assert_eq!(reply.json()["error"]["code"], "invalid_thinking", "{body}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn setting_thinking_while_a_turn_runs_is_409_turn_active() {
    let (gate, options) = gated();
    let harness = Harness::with(options);
    let id = harness.create().await;
    let (_stream, _turn_id) = start_stream(&harness, &id).await;
    harness.provider.wait_started(1).await;

    let reply = harness
        .send(
            "POST",
            &format!("/v1/sessions/{id}/thinking"),
            Some(r#"{"thinking":"high"}"#),
        )
        .await;
    assert_eq!(reply.status, StatusCode::CONFLICT, "{}", reply.body);
    assert_eq!(reply.json()["error"]["code"], "turn_active");
    gate.cancel();
}

#[tokio::test]
async fn the_old_session_profile_route_is_gone() {
    let harness = Harness::new();
    let id = harness.create().await;
    let reply = harness
        .send(
            "POST",
            &format!("/v1/sessions/{id}/profile"),
            Some(r#"{"profile":"beta"}"#),
        )
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND, "{}", reply.body);
}

#[tokio::test]
async fn config_providers_list_built_ins_and_configured_models_without_environment_values() {
    let harness = config_harness();
    let reply = harness.send("GET", "/v1/config/providers", None).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let body = reply.json();
    assert_eq!(body["default_model"], "work/small");
    let providers = body["providers"].as_array().expect("providers");
    let names: Vec<_> = providers
        .iter()
        .map(|provider| provider["name"].as_str().unwrap())
        .collect();
    for built_in in ["claude", "openai-compatible", "chatgpt", "work"] {
        assert!(
            names.contains(&built_in),
            "{built_in} missing from {names:?}"
        );
    }
    let work = providers.iter().find(|p| p["name"] == "work").unwrap();
    assert_eq!(work["type"], "openai-compatible");
    assert_eq!(work["builtin"], false);
    assert_eq!(work["base_url"], "https://example.test/v1");
    assert_eq!(work["api_key_env"], "WORK_KEY");
    let small = work["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["model_id"] == "small")
        .unwrap();
    assert_eq!(small["selector"], "work/small");
    assert_eq!(small["default"], true);
    assert_eq!(small["thinking"], "low");
    let chatgpt = providers.iter().find(|p| p["name"] == "chatgpt").unwrap();
    assert_eq!(chatgpt["builtin"], true);
    assert!(!reply.body.contains("WORK_KEY_VALUE"));
}

#[tokio::test]
async fn one_provider_is_readable_and_an_unknown_one_is_404_provider_not_found() {
    let harness = config_harness();
    let reply = harness.send("GET", "/v1/config/providers/work", None).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(reply.json()["name"], "work");
    assert_eq!(reply.json()["models"].as_array().unwrap().len(), 2);

    let reply = harness.send("GET", "/v1/config/providers/nope", None).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND, "{}", reply.body);
    assert_eq!(reply.json()["error"]["code"], "provider_not_found");

    let reply = harness.send("GET", "/v1/config/profiles", None).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND, "{}", reply.body);
}

#[tokio::test]
async fn chatgpt_models_cannot_be_enumerated() {
    let harness = config_harness();
    let reply = harness
        .send("GET", "/v1/config/models?provider=chatgpt", None)
        .await;
    assert_eq!(reply.status, StatusCode::NOT_IMPLEMENTED, "{}", reply.body);
    assert_eq!(reply.json()["error"]["code"], "models_unsupported");
}

#[tokio::test]
async fn config_change_confirmation_is_one_time_and_requires_restart() {
    let harness = config_harness();
    let preview = post_change(
        &harness,
        r#"{"kind":"set_model_field","model":"work/large","field":"thinking","value":"high"}"#,
    )
    .await;
    assert_eq!(preview.status, StatusCode::OK, "{}", preview.body);
    let value = preview.json();
    let id = value["id"].as_str().expect("id").to_string();
    assert_eq!(value["operation"], "set model field");
    assert_eq!(value["target"], "work/large");
    assert_eq!(value["field"], "thinking");
    assert!(value["expires_in_seconds"].as_u64().unwrap() > 0);
    assert!(
        !preview.body.contains("high"),
        "preview must redact field values: {}",
        preview.body
    );
    let path = &harness.factory.builder.shared.config_path;
    assert_eq!(std::fs::read_to_string(path).unwrap(), PROVIDER_CONFIG);

    let confirm = harness
        .send("POST", &format!("/v1/config/changes/{id}"), None)
        .await;
    assert_eq!(confirm.status, StatusCode::OK, "{}", confirm.body);
    assert_eq!(confirm.json()["status"], "saved_restart_required");
    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        PROVIDER_CONFIG.replace(
            "[providers.work.models.large]\n",
            "[providers.work.models.large]\nthinking = \"high\"\n"
        )
    );
    let repeated = harness
        .send("POST", &format!("/v1/config/changes/{id}"), None)
        .await;
    assert_eq!(repeated.status, StatusCode::NOT_FOUND, "{}", repeated.body);
}

#[tokio::test]
async fn every_change_kind_previews_and_invalid_input_is_rejected() {
    let harness = config_harness();
    for (body, operation) in [
        (
            r#"{"kind":"set_default_model","model":"work/large"}"#,
            "set default model",
        ),
        (
            r#"{"kind":"set_provider","provider":"extra","type":"openai-compatible","base_url":"https://extra.test/v1","api_key_env":"EXTRA_KEY"}"#,
            "set provider",
        ),
        (
            r#"{"kind":"set_provider_field","provider":"work","field":"base_url","value":"https://moved.test/v1"}"#,
            "set provider field",
        ),
        (
            r#"{"kind":"set_model","model":"work/new","thinking":"medium"}"#,
            "set model",
        ),
        (
            r#"{"kind":"remove_model","model":"work/large"}"#,
            "remove model",
        ),
    ] {
        let reply = post_change(&harness, body).await;
        assert_eq!(reply.status, StatusCode::OK, "{body}: {}", reply.body);
        assert_eq!(reply.json()["operation"], operation, "{body}");
    }
    for (body, code) in [
        (
            r#"{"kind":"set_provider","provider":"extra","type":"bogus"}"#,
            "invalid_provider",
        ),
        (
            r#"{"kind":"set_provider_field","provider":"work","field":"secret","value":"x"}"#,
            "invalid_field",
        ),
        (
            r#"{"kind":"set_model_field","model":"work/large","field":"api_key","value":"x"}"#,
            "invalid_field",
        ),
        (
            r#"{"kind":"set_model_field","model":"work/large","field":"thinking","value":"extreme"}"#,
            "invalid_change",
        ),
    ] {
        let reply = post_change(&harness, body).await;
        assert_eq!(
            reply.status,
            StatusCode::BAD_REQUEST,
            "{body}: {}",
            reply.body
        );
        assert_eq!(reply.json()["error"]["code"], code, "{body}");
    }
    // The removed profile kinds are not accepted.
    let reply = post_change(&harness, r#"{"kind":"set_profile_field","profile":"work"}"#).await;
    assert_eq!(
        reply.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        reply.body
    );
}

#[tokio::test]
async fn removing_the_default_provider_or_model_is_409_model_in_use() {
    let harness = config_harness();
    for body in [
        r#"{"kind":"remove_provider","provider":"work"}"#,
        r#"{"kind":"remove_model","model":"work/small"}"#,
    ] {
        let reply = post_change(&harness, body).await;
        assert_eq!(reply.status, StatusCode::CONFLICT, "{body}: {}", reply.body);
        assert_eq!(reply.json()["error"]["code"], "model_in_use", "{body}");
    }
}

#[tokio::test]
async fn listing_models_without_a_bound_key_reports_api_key_unavailable() {
    let harness = config_harness();
    let reply = harness
        .send("GET", "/v1/config/models?provider=work", None)
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.body);
    assert_eq!(reply.json()["error"]["code"], "api_key_unavailable");
}
