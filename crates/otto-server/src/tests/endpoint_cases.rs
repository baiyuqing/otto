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
        provider: "alpha".to_string(),
        provider_type: "openai-compatible".to_string(),
        model: "test-model".to_string(),
        thinking: "high".to_string(),
        sandbox: "off".to_string(),
        models: vec![
            "alpha/test-model".to_string(),
            "beta/test-model".to_string(),
        ],
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
            "cache_write_input_tokens": null,
            "cache_write_1h_input_tokens": null,
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
impl otto::workflow::Executor for TestWorkflowExecutor {
    async fn execute(
        &self,
        attempt: otto::workflow::Attempt,
        _control: &otto_platform::deadline::Control,
    ) -> Result<String, String> {
        Ok(format!("{} done", attempt.step_id))
    }
}

/// A real, workspace-scoped workflow controller backed by an in-memory
/// store, for exercising the HTTP routes end to end.
pub(super) fn workflow_test_controller(workspace: &str) -> Arc<otto::workflow::Controller> {
    let store = Arc::new(otto::workflow::Store::open_in_memory());
    let transcripts = tempfile::tempdir().expect("transcripts").keep();
    otto::workflow::Controller::new(
        store,
        otto::workflow::Catalog::from_definitions(vec![otto::workflow::Definition {
            source: String::new(),
            name: "review".to_string(),
            description: String::new(),
            hash: "0".repeat(64),
            agents: vec![otto::workflow::AgentSnapshot {
                name: "worker".to_string(),
                ..otto::workflow::AgentSnapshot::default()
            }],
            steps: vec![otto::workflow::Step {
                id: "work".to_string(),
                kind: otto::workflow::StepKind::Agent,
                agent: "worker".to_string(),
                prompt: "work".to_string(),
                needs: Vec::new(),
            }],
        }]),
        Arc::new(TestWorkflowExecutor),
        workspace.to_string(),
        transcripts,
        otto::workflow::RuntimeIdentity::default(),
        1,
    )
}

#[tokio::test]
async fn workflow_routes_start_and_report_a_durable_run() {
    let controller = workflow_test_controller(otto::fixtures::shared_workspace());
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
        workflows: Some(workflow_test_controller(otto::fixtures::shared_workspace())),
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
    let other = otto::fixtures::workspace_dir("other");
    let harness = Harness::with(HarnessOptions {
        workflows: Some(workflow_test_controller("/a")),
        workflow_workspaces: HashMap::from([(other.clone(), workflow_test_controller(&other))]),
        workspaces: FakeWorkspaces {
            admitted: vec![other.clone()],
            ..FakeWorkspaces::default()
        },
        ..HarnessOptions::default()
    });

    let start = harness
        .send_with(
            "POST",
            &format!("/v1/workflows?workspace={other}"),
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
        .send("GET", &format!("/v1/workflows?workspace={other}"), None)
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

/// Every route pattern the router serves, from the operation registry.
fn route_paths() -> Vec<&'static str> {
    let mut paths: Vec<&'static str> = Vec::new();
    for operation in authorization::OperationId::ALL {
        if !paths.contains(&operation.path()) {
            paths.push(operation.path());
        }
    }
    paths
}

#[test]
fn openapi_documents_exactly_the_registered_operations() {
    let document = String::from_utf8_lossy(OPENAPI_YAML);
    let mut documented = std::collections::BTreeSet::new();
    let mut path: Option<&str> = None;
    for line in document.lines() {
        if let Some(key) = line
            .strip_prefix("  ")
            .and_then(|rest| rest.strip_suffix(':'))
            && key.starts_with('/')
        {
            path = Some(key);
        } else if !line.starts_with(' ') && !line.is_empty() {
            path = None;
        } else if let (Some(path), Some(method)) = (
            path,
            line.strip_prefix("    ")
                .and_then(|rest| rest.strip_suffix(':'))
                .filter(|method| ["get", "post", "put", "patch", "delete"].contains(method)),
        ) {
            documented.insert(format!("{} {path}", method.to_uppercase()));
        }
    }
    let registered: std::collections::BTreeSet<String> = authorization::OperationId::ALL
        .iter()
        .map(|operation| format!("{} {}", operation.method(), operation.path()))
        .collect();
    let undocumented: Vec<_> = registered.difference(&documented).collect();
    let unserved: Vec<_> = documented.difference(&registered).collect();
    assert!(
        undocumented.is_empty(),
        "openapi.yaml is missing {undocumented:?}"
    );
    assert!(
        unserved.is_empty(),
        "openapi.yaml documents unserved {unserved:?}"
    );
    assert!(document.matches("enum: [telegram]").count() >= 2);
}

#[tokio::test]
async fn every_documented_route_is_reachable_on_the_router() {
    let harness = Harness::new();
    let id = harness.create().await;
    for path in route_paths() {
        let mut probe = path.replace("{id}", &id);
        for parameter in [
            "{turn_id}",
            "{task_id}",
            "{span_id}",
            "{timer_id}",
            "{approval_id}",
            "{name}",
            "{message_id}",
            "{record_id}",
            "{candidate_id}",
            "{parent_session}",
        ] {
            probe = probe.replace(parameter, "nope");
        }
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
