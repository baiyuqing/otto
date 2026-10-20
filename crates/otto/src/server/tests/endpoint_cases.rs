//! Error body, request logging, fixed endpoint, workflow route and token gate tests.

use super::*;

// ---- errors, logging, and the fixed endpoints ----

#[tokio::test]
async fn a_create_failure_answers_a_fixed_internal_error_and_logs_the_real_one() {
    let harness = Harness::with(HarnessOptions {
        create_error: Some("disk full".to_string()),
        ..HarnessOptions::default()
    });
    let reply = harness.send("POST", "/v1/sessions", None).await;
    assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        reply.body,
        r#"{"error":{"code":"internal","message":"internal error"}}"#
    );
    assert!(
        harness.logged().contains("disk full"),
        "the real error is logged:\n{}",
        harness.logged()
    );
}

#[tokio::test]
async fn neither_the_log_nor_the_metrics_carry_prompt_text() {
    const SECRET: &str = "quick-brown-fox-prompt-marker";
    let harness = Harness::with(HarnessOptions {
        script: Script {
            echo: true,
            ..Script::default()
        },
        ..HarnessOptions::default()
    });
    let id = harness.create().await;
    let summary = harness
        .send(
            "POST",
            &format!("/v1/sessions/{id}/turns"),
            Some(&format!(r#"{{"text":"{SECRET}","stream":false}}"#)),
        )
        .await
        .json();
    assert_eq!(summary["text"], SECRET, "the turn must actually have run");

    assert!(
        !harness.logged().contains(SECRET),
        "log: {}",
        harness.logged()
    );
    assert!(!harness.metrics_body().await.contains(SECRET));
}

#[tokio::test]
async fn the_request_id_is_truncated_to_64_bytes_and_filtered_to_ascii() {
    let harness = Harness::new();
    // 0x85 is valid obs-text but not printable ASCII. It sits inside the
    // first 64 bytes, so truncation keeps it and the filter drops it.
    let mut raw = vec![b'a'; 30];
    raw.push(0x85);
    raw.extend(std::iter::repeat_n(b'a', 40));
    let reply = harness
        .send_with("GET", "/healthz", None, &[("x-request-id", &raw)])
        .await;
    assert_eq!(reply.header("x-request-id"), "a".repeat(63));

    let generated = harness.send("GET", "/healthz", None).await;
    assert_eq!(
        generated.header("x-request-id").len(),
        32,
        "an absent header gets a generated id"
    );
}

#[tokio::test]
async fn the_info_endpoint_echoes_the_configured_info() {
    let info = Info {
        workspace: "/ws".to_string(),
        provider: "openai-compatible".to_string(),
        profile: "alpha".to_string(),
        model: "test-model".to_string(),
        thinking: "high".to_string(),
        sandbox: "off".to_string(),
        profiles: vec!["alpha".to_string(), "beta".to_string()],
    };
    let harness = Harness::with(HarnessOptions {
        info: info.clone(),
        ..HarnessOptions::default()
    });
    let reply = harness.send("GET", "/v1/info", None).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json(), serde_json::to_value(&info).expect("info"));
}

#[tokio::test]
async fn the_usage_endpoint_returns_persisted_aggregates() {
    let harness = Harness::new();
    let reply = harness.send("GET", "/v1/usage", None).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        reply.json(),
        serde_json::json!({
            "usage_events": 0,
            "reported_usage_events": 0,
            "logical_operations": 0,
            "actual_attempts": 0,
            "succeeded_operations": 0,
            "failed_operations": 0,
            "canceled_operations": 0,
            "deadline_exceeded_operations": 0,
            "interrupted_operations": 0,
            "not_started_operations": 0,
            "known_no_effect_operations": 0,
            "completed_effect_operations": 0,
            "unknown_effect_operations": 0,
            "input_tokens": 0,
            "output_tokens": 0,
            "cached_input_tokens": null,
            "cache_hit_rate": null
        })
    );
}

#[tokio::test]
async fn the_daily_usage_endpoint_validates_its_range() {
    let harness = Harness::new();
    let reply = harness.send("GET", "/v1/usage/daily?days=0", None).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.json()["error"]["code"], "invalid_usage_range");

    let reply = harness.send("GET", "/v1/usage/daily?days=7", None).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json()["summary"]["usage_events"], 0);
    assert_eq!(reply.json()["summary"]["logical_operations"], 0);
    assert_eq!(reply.json()["daily"].as_array().expect("daily").len(), 7);
}

#[tokio::test]
async fn the_openapi_endpoint_serves_the_embedded_document() {
    let harness = Harness::new();
    let reply = harness.send("GET", "/v1/openapi.yaml", None).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.header("content-type"), "application/yaml");
    assert!(reply.body.starts_with("openapi:"), "{}", &reply.body[..40]);
}

pub(super) struct TestWorkflowExecutor;

#[async_trait::async_trait]
impl crate::workflow::Executor for TestWorkflowExecutor {
    async fn execute(
        &self,
        attempt: crate::workflow::Attempt,
        _control: &crate::deadline::Control,
    ) -> Result<String, String> {
        Ok(format!("{} done", attempt.step_id))
    }
}

/// A real, workspace-scoped workflow controller backed by an in-memory
/// store, for exercising the HTTP routes end to end.
pub(super) fn workflow_test_controller(workspace: &str) -> Arc<crate::workflow::Controller> {
    let store = Arc::new(crate::workflow::Store::open_in_memory());
    let transcripts = tempfile::tempdir().expect("transcripts").keep();
    crate::workflow::Controller::new(
        store,
        crate::workflow::Catalog::from_definitions(vec![crate::workflow::Definition {
            source: String::new(),
            name: "review".to_string(),
            description: String::new(),
            hash: "0".repeat(64),
            agents: vec![crate::workflow::AgentSnapshot {
                name: "worker".to_string(),
                ..crate::workflow::AgentSnapshot::default()
            }],
            steps: vec![crate::workflow::Step {
                id: "work".to_string(),
                kind: crate::workflow::StepKind::Agent,
                agent: "worker".to_string(),
                prompt: "work".to_string(),
                needs: Vec::new(),
            }],
        }]),
        Arc::new(TestWorkflowExecutor),
        workspace.to_string(),
        transcripts,
        crate::workflow::RuntimeIdentity::default(),
        1,
    )
}

#[tokio::test]
async fn workflow_routes_start_and_report_a_durable_run() {
    let controller = workflow_test_controller("/workspace");
    let harness = Harness::with(HarnessOptions {
        workflows: Some(controller),
        ..HarnessOptions::default()
    });

    let reply = harness
        .send_with(
            "POST",
            "/v1/workflows",
            Some(r#"{"name":"review","input":"request"}"#),
            &[("content-type", b"application/json")],
        )
        .await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.body);
    let id = reply.json()["run"]["id"]
        .as_str()
        .expect("run id")
        .to_string();
    let mut status = String::new();
    for _ in 0..20 {
        let reply = harness
            .send("GET", &format!("/v1/workflows/{id}"), None)
            .await;
        assert_eq!(reply.status, StatusCode::OK);
        status = reply.json()["run"]["status"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        if status == "succeeded" {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(status, "succeeded");
    let events = harness
        .send("GET", &format!("/v1/workflows/{id}/events?after=0"), None)
        .await;
    assert_eq!(events.status, StatusCode::OK);
    assert_eq!(events.header("content-type"), "text/event-stream");
    assert!(events.body.contains("step_succeeded"), "{}", events.body);

    let fork = harness
        .send_with(
            "POST",
            &format!("/v1/workflows/{id}/fork"),
            Some(r#"{"after_step":"work"}"#),
            &[("content-type", b"application/json")],
        )
        .await;
    assert_eq!(fork.status, StatusCode::CREATED, "{}", fork.body);
    assert_eq!(
        fork.json()["run"]["forked_from_run_id"].as_str(),
        Some(id.as_str())
    );
    assert_eq!(
        fork.json()["run"]["forked_from_step_id"].as_str(),
        Some("work")
    );
}

#[tokio::test]
async fn workflow_routes_map_error_kinds_and_bound_the_run_page() {
    let harness = Harness::with(HarnessOptions {
        workflows: Some(workflow_test_controller("/workspace")),
        ..HarnessOptions::default()
    });

    // An unknown run is a missing resource, not a conflict.
    let missing = harness.send("GET", "/v1/workflows/missing", None).await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND, "{}", missing.body);
    assert_eq!(missing.json()["error"]["code"], "workflow_not_found");

    // An unknown definition name is also a missing resource.
    let unknown = harness
        .send_with(
            "POST",
            "/v1/workflows",
            Some(r#"{"name":"missing"}"#),
            &[("content-type", b"application/json")],
        )
        .await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND, "{}", unknown.body);
    assert_eq!(unknown.json()["error"]["code"], "workflow_not_found");

    // A page size of zero is a caller mistake.
    let invalid = harness.send("GET", "/v1/workflows?limit=0", None).await;
    assert_eq!(invalid.status, StatusCode::BAD_REQUEST, "{}", invalid.body);
    assert_eq!(invalid.json()["error"]["code"], "invalid_workflow_limit");

    // A bounded page reports its cursor even when the page is not full.
    let page = harness.send("GET", "/v1/workflows?limit=1", None).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    assert!(page.json().get("next_before").is_some(), "{}", page.body);
    assert!(page.json()["runs"].as_array().is_some(), "{}", page.body);
}

#[tokio::test]
async fn workflow_routes_scope_runs_and_lists_by_workspace_query() {
    let harness = Harness::with(HarnessOptions {
        workflows: Some(workflow_test_controller("/a")),
        workflow_workspaces: HashMap::from([(
            "/other".to_string(),
            workflow_test_controller("/other"),
        )]),
        workspaces: FakeWorkspaces {
            admitted: vec!["/other".to_string()],
            ..FakeWorkspaces::default()
        },
        ..HarnessOptions::default()
    });

    let start = harness
        .send_with(
            "POST",
            "/v1/workflows?workspace=/other",
            Some(r#"{"name":"review","input":"request"}"#),
            &[("content-type", b"application/json")],
        )
        .await;
    assert_eq!(start.status, StatusCode::CREATED, "{}", start.body);
    let id = start.json()["run"]["id"]
        .as_str()
        .expect("run id")
        .to_string();

    let list_startup = harness.send("GET", "/v1/workflows", None).await;
    assert_eq!(list_startup.status, StatusCode::OK, "{}", list_startup.body);
    assert!(
        list_startup.json()["runs"]
            .as_array()
            .expect("runs")
            .is_empty()
    );

    let list_other = harness
        .send("GET", "/v1/workflows?workspace=/other", None)
        .await;
    assert_eq!(list_other.status, StatusCode::OK, "{}", list_other.body);
    let runs = list_other.json()["runs"].as_array().expect("runs").clone();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["id"].as_str(), Some(id.as_str()));

    // Run-scoped routes find the run without a `?workspace=` hint.
    let mut status = String::new();
    for _ in 0..20 {
        let reply = harness
            .send("GET", &format!("/v1/workflows/{id}"), None)
            .await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
        status = reply.json()["run"]["status"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        if status == "succeeded" {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(status, "succeeded");

    let cancel = harness
        .send("POST", &format!("/v1/workflows/{id}/cancel"), None)
        .await;
    assert_eq!(cancel.status, StatusCode::CONFLICT, "{}", cancel.body);
    assert_eq!(
        cancel.json()["error"]["message"],
        "workflow run already finished"
    );
}

#[tokio::test]
async fn workflow_routes_report_the_disabled_error_for_a_workspace_with_no_controller() {
    let harness = Harness::with(HarnessOptions {
        workflows: Some(workflow_test_controller("/a")),
        workspaces: FakeWorkspaces {
            admitted: vec!["/other".to_string()],
            ..FakeWorkspaces::default()
        },
        ..HarnessOptions::default()
    });

    let list = harness
        .send("GET", "/v1/workflows?workspace=/other", None)
        .await;
    assert_eq!(list.status, StatusCode::NOT_IMPLEMENTED, "{}", list.body);
    assert_eq!(list.json()["error"]["code"], "workflow_unavailable");

    let start = harness
        .send_with(
            "POST",
            "/v1/workflows?workspace=/other",
            Some(r#"{"name":"review","input":"request"}"#),
            &[("content-type", b"application/json")],
        )
        .await;
    assert_eq!(start.status, StatusCode::NOT_IMPLEMENTED, "{}", start.body);

    // The startup workspace, with its own controller, is unaffected.
    let default_list = harness.send("GET", "/v1/workflows", None).await;
    assert_eq!(default_list.status, StatusCode::OK, "{}", default_list.body);
}

#[tokio::test]
async fn switching_a_session_profile_rebuilds_it_and_writes_no_file() {
    let harness = Harness::new();
    let id = harness.create().await;
    let reply = harness
        .send(
            "POST",
            &format!("/v1/sessions/{id}/profile"),
            Some(r#"{"profile":"beta"}"#),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(reply.json()["profile"], "beta");
    assert_eq!(reply.json()["model"], "gpt-beta");

    let reply = harness
        .send(
            "POST",
            &format!("/v1/sessions/{id}/profile"),
            Some(r#"{"profile":"nope"}"#),
        )
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.body);
    assert_eq!(reply.json()["error"]["code"], "bad_request");
    assert!(reply.body.contains("nope"), "{}", reply.body);

    let reply = harness
        .send("POST", &format!("/v1/sessions/{id}/profile"), Some("{}"))
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.body);

    let reply = harness
        .send(
            "POST",
            "/v1/sessions/missing/profile",
            Some(r#"{"profile":"beta"}"#),
        )
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND, "{}", reply.body);

    // A session switch never writes the configuration file: the running
    // serve keeps its startup profile catalog, and so does the next start.
    assert!(
        !harness.factory.builder.shared.config_path.exists(),
        "the profile switch wrote a configuration file"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn switching_a_profile_while_a_turn_runs_is_409_turn_active() {
    let (gate, options) = gated();
    let harness = Harness::with(options);
    let id = harness.create().await;
    let (_stream, _turn_id) = start_stream(&harness, &id).await;
    harness.provider.wait_started(1).await;

    let reply = harness
        .send(
            "POST",
            &format!("/v1/sessions/{id}/profile"),
            Some(r#"{"profile":"beta"}"#),
        )
        .await;
    assert_eq!(reply.status, StatusCode::CONFLICT, "{}", reply.body);
    assert_eq!(reply.json()["error"]["code"], "turn_active");
    gate.cancel();
}

#[tokio::test]
async fn config_profiles_are_readable_without_environment_values() {
    let harness = Harness::with(HarnessOptions {
            config_text: Some(
                "default_profile = \"work\"\n\n[profiles.work]\nprovider = \"openai-compatible\"\nbase_url = \"https://example.test/v1\"\nmodel = \"small\"\napi_key_env = \"WORK_KEY\"\n".into(),
            ),
            ..Default::default()
        });
    let reply = harness.send("GET", "/v1/config/profiles", None).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(reply.json()["profiles"][0]["name"], "work");
    assert_eq!(reply.json()["profiles"][0]["default"], true);
    assert_eq!(reply.json()["profiles"][0]["api_key_env"], "WORK_KEY");
    assert!(!reply.body.contains("WORK_KEY_VALUE"));
}

#[tokio::test]
async fn config_change_confirmation_is_one_time_and_requires_restart() {
    let original = "# preserve\ndefault_profile = \"work\"\n\n[profiles.work]\nprovider = \"openai-compatible\"\nbase_url = \"https://example.test/v1\"\nmodel = \"old\"\napi_key_env = \"WORK_KEY\"\n";
    let harness = Harness::with(HarnessOptions {
        config_text: Some(original.into()),
        ..Default::default()
    });
    let preview = harness
        .raw(
            "POST",
            "/v1/config/changes",
            Some(r#"{"kind":"set_profile_field","profile":"work","field":"model","value":"new"}"#),
            &[("content-type", b"application/json")],
        )
        .await;
    let body = String::from_utf8(
        axum::body::to_bytes(preview.into_body(), usize::MAX)
            .await
            .expect("body")
            .to_vec(),
    )
    .expect("utf8");
    let value: Value = serde_json::from_str(&body).expect("preview json");
    let id = value["id"].as_str().expect("id");
    assert!(
        !body.contains("new"),
        "preview must redact field values: {body}"
    );
    let confirm = harness
        .send("POST", &format!("/v1/config/changes/{id}"), None)
        .await;
    assert_eq!(confirm.status, StatusCode::OK, "{}", confirm.body);
    assert_eq!(confirm.json()["status"], "saved_restart_required");
    let changed =
        std::fs::read_to_string(&harness.factory.builder.shared.config_path).expect("read config");
    assert_eq!(
        changed,
        original.replace("model = \"old\"", "model = \"new\"")
    );
    let repeated = harness
        .send("POST", &format!("/v1/config/changes/{id}"), None)
        .await;
    assert_eq!(repeated.status, StatusCode::NOT_FOUND, "{}", repeated.body);
}

/// Every API path the router serves.
///
/// ponytail: axum exposes no route table, so this list is written out once
/// and checked against both the router and `openapi.yaml`.
pub(super) const ROUTES: &[&str] = &[
    "/v1/config/profiles",
    "/v1/config/profiles/{name}",
    "/v1/config/models",
    "/v1/config/changes",
    "/v1/config/changes/{id}",
    "/v1/sessions",
    "/v1/sessions/{id}",
    "/v1/sessions/{id}/history",
    "/v1/sessions/{id}/context",
    "/v1/sessions/{id}/trace",
    "/v1/sessions/{id}/trace/{span_id}",
    "/v1/sessions/{id}/profile",
    "/v1/sessions/{id}/approvals/{approval_id}",
    "/v1/sessions/{id}/inputs",
    "/v1/sessions/{id}/tasks/steer",
    "/v1/sessions/{id}/tasks/control",
    "/v1/sessions/{id}/turns",
    "/v1/sessions/{id}/turns/{turn_id}",
    "/v1/sessions/{id}/turns/{turn_id}/events",
    "/v1/sessions/{id}/turns/{turn_id}/cancel",
    "/v1/sessions/{id}/compact",
    "/v1/sessions/{id}/reflect",
    "/v1/sessions/{id}/reflect/cancel",
    "/v1/sessions/{id}/notices",
    "/v1/sessions/{id}/wake-replies",
    "/v1/sessions/{id}/reflection/skills",
    "/v1/sessions/{id}/reflection/skills/{name}/revert",
    "/v1/sessions/{id}/tasks",
    "/v1/sessions/{id}/tasks/{task_id}",
    "/v1/sessions/{id}/tasks/{task_id}/cancel",
    "/v1/sessions/{id}/timers",
    "/v1/sessions/{id}/timers/{timer_id}/cancel",
    "/v1/sessions/{id}/mcp",
    "/v1/sessions/{id}/skill-manager",
    "/v1/sessions/{id}/skill-manager/{name}",
    "/v1/sessions/{id}/skills",
    "/v1/sessions/{id}/skills/{name}",
    "/v1/sessions/{id}/skills/{name}/enable",
    "/v1/sessions/{id}/skills/{name}/disable",
    "/v1/sandbox/reload",
    "/v1/tasks",
    "/v1/tasks/{parent_session}/{task_id}",
    "/v1/workflows",
    "/v1/workflows/{id}",
    "/v1/workflows/{id}/events",
    "/v1/workflows/{id}/resume",
    "/v1/workflows/{id}/fork",
    "/v1/workflows/{id}/cancel",
    "/v1/workflows/requests/{id}/approve",
    "/v1/workflows/requests/{id}/reject",
    "/v1/workspaces",
    "/v1/workspaces/diff",
    "/v1/info",
    "/v1/status",
    "/v1/usage",
    "/v1/usage/daily",
    "/v1/openapi.yaml",
    "/healthz",
    "/metrics",
];

#[test]
fn openapi_documents_every_route() {
    let document = String::from_utf8_lossy(OPENAPI_YAML);
    for path in ROUTES {
        let want = format!("\n  {path}:");
        assert!(document.contains(&want), "openapi.yaml is missing {path}");
    }
    assert!(document.matches("enum: [telegram, feishu]").count() >= 2);
}

#[tokio::test]
async fn every_documented_route_is_reachable_on_the_router() {
    let harness = Harness::new();
    let id = harness.create().await;
    for path in ROUTES {
        let probe = path
            .replace("{id}", &id)
            .replace("{turn_id}", "nope")
            .replace("{task_id}", "nope")
            .replace("{span_id}", "nope")
            .replace("{timer_id}", "nope")
            .replace("{approval_id}", "nope")
            .replace("{name}", "nope");
        // An unmatched path logs the route label "unmatched"; a matched
        // one logs its own pattern. That is the reachability check.
        let before = harness.logged().len();
        // `raw`, not `send`: `/v1/status`'s body is an infinite SSE
        // stream, and `send` awaits the full body.
        harness.raw("GET", &probe, None, &[]).await;
        assert!(
            !harness.logged()[before..].contains("route=unmatched"),
            "GET {probe} matched no route"
        );
    }
}

#[tokio::test]
async fn a_token_gates_the_v1_routes_but_not_healthz_or_metrics() {
    let harness = Harness::with(HarnessOptions {
        token: "secret".to_string(),
        ..HarnessOptions::default()
    });
    let reply = harness.send("GET", "/v1/info", None).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert_eq!(reply.json()["error"]["code"], "unauthorized");

    for path in ["/healthz", "/metrics"] {
        assert_eq!(
            harness.send("GET", path, None).await.status,
            StatusCode::OK,
            "{path} must not need a token"
        );
    }

    let reply = harness
        .send_with(
            "GET",
            "/v1/info",
            None,
            &[("authorization", b"Bearer secret")],
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn task_control_http_routes_validate_and_steer_the_active_task() {
    let (gate, options) = gated();
    let harness = Harness::with(options);
    let id = harness.create().await;

    let input_idle = harness
        .send(
            "POST",
            &format!("/v1/sessions/{id}/inputs"),
            Some(r#"{"text":"change direction"}"#),
        )
        .await;
    assert_eq!(input_idle.status, StatusCode::OK);
    assert_eq!(input_idle.json(), serde_json::json!("idle"));

    let idle = harness
        .send(
            "POST",
            &format!("/v1/sessions/{id}/tasks/steer"),
            Some(r#"{"text":"change direction"}"#),
        )
        .await;
    assert_eq!(idle.status, StatusCode::CONFLICT);

    let control = harness
        .send(
            "POST",
            &format!("/v1/sessions/{id}/tasks/control"),
            Some(r#"{"text":"what can you do?","channel":"telegram"}"#),
        )
        .await;
    assert_eq!(control.status, StatusCode::OK);
    assert_eq!(control.body, "null");

    let (_stream, _) = start_stream(&harness, &id).await;
    harness.provider.wait_started(1).await;
    let admitted = harness
        .send(
            "POST",
            &format!("/v1/sessions/{id}/inputs"),
            Some(r#"{"text":"finish this provider response, then inspect the failure"}"#),
        )
        .await;
    assert_eq!(admitted.status, StatusCode::OK);
    assert_eq!(admitted.json(), serde_json::json!("steered"));

    let steer = harness
        .send(
            "POST",
            &format!("/v1/sessions/{id}/tasks/steer"),
            Some(r#"{"text":"do not change the public API"}"#),
        )
        .await;
    assert_eq!(steer.status, StatusCode::OK);
    let steer = steer.json();
    assert_eq!(steer["text"], "do not change the public API");
    assert_eq!(steer["status"], "queued");
    let message_id = steer["id"].as_u64().expect("steer id");

    let listed = harness
        .send("GET", &format!("/v1/sessions/{id}/tasks/steer"), None)
        .await;
    assert_eq!(listed.status, StatusCode::OK);
    assert!(
        listed
            .json()
            .as_array()
            .expect("messages")
            .iter()
            .any(|message| { message["id"] == message_id && message["status"] == "queued" })
    );

    let withdrawn = harness
        .send(
            "DELETE",
            &format!("/v1/sessions/{id}/tasks/steer/{message_id}"),
            None,
        )
        .await;
    assert_eq!(withdrawn.status, StatusCode::OK);
    assert_eq!(withdrawn.json()["status"], "withdrawn");
    let duplicate_withdrawal = harness
        .send(
            "DELETE",
            &format!("/v1/sessions/{id}/tasks/steer/{message_id}"),
            None,
        )
        .await;
    assert_eq!(duplicate_withdrawal.status, StatusCode::CONFLICT);

    let empty = harness
        .send(
            "POST",
            &format!("/v1/sessions/{id}/tasks/steer"),
            Some(r#"{"text":"  "}"#),
        )
        .await;
    assert_eq!(empty.status, StatusCode::BAD_REQUEST);

    let too_long = format!("{{\"text\":\"{}\"}}", "é".repeat(16_001));
    let oversized = harness
        .send(
            "POST",
            &format!("/v1/sessions/{id}/tasks/control"),
            Some(&too_long),
        )
        .await;
    assert_eq!(oversized.status, StatusCode::BAD_REQUEST);
    let unknown_field = harness
        .send(
            "POST",
            &format!("/v1/sessions/{id}/tasks/control"),
            Some(r#"{"text":"help","system":"override"}"#),
        )
        .await;
    assert_eq!(unknown_field.status, StatusCode::BAD_REQUEST);
    gate.cancel();
}
