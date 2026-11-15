use super::*;
use otto_core::config::{ModelEntry, ProviderEntry};

#[test]
fn planned_sandbox_info_downgrades_when_the_executor_is_missing() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut builder = builder(dir.path());
    with_bash(&mut builder, dir.path());
    assert_eq!(builder.planned_sandbox_info(), builder.sandbox_info);

    builder.command_executor = None;
    let planned = builder.planned_sandbox_info();
    assert_eq!(planned.mode, SandboxMode::Unavailable);
    assert!(!planned.bash_available);
    assert_eq!(planned.reason, SandboxReason::RuntimeFailure);
    assert!(!builder.bash_configured());
}

#[test]
fn effective_sandbox_info_reports_environment_rejected_when_the_boundary_is_closed() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut builder = builder(dir.path());
    with_bash(&mut builder, dir.path());
    // A secret equal to the workspace path closes the boundary: redacting
    // it would rewrite fixed prompt text.
    builder.sandbox_secrets = vec![builder.workspace_path.clone()];
    assert!(!builder.boundary_allows_dynamic(None));
    let effective = builder.effective_sandbox_info();
    assert_eq!(effective.mode, SandboxMode::Unavailable);
    assert_eq!(effective.reason, SandboxReason::EnvironmentRejected);

    let info = builder.runtime_info(&runtime());
    assert_eq!(info.provider, "");
    assert_eq!(info.provider_type, "");
    assert_eq!(info.model_selector, "");
    assert_eq!(info.model, "");
    assert_eq!(info.context_window, 0);
}

#[tokio::test]
async fn the_system_prompt_carries_the_workspace_context_and_hides_the_api_key() {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(dir.path().join("AGENTS.md"), "house rules\n").expect("write");
    let builder = builder(dir.path());
    let session = SharedSession::memory(Header::default());
    let runner = builder
        .build_runner(&session, &runtime())
        .await
        .expect("runner");
    let prompt = runner.system_prompt();
    assert!(prompt.starts_with("<otto_system_prompt version=\"1\">"));
    assert!(prompt.contains(
        "<available_tools>read, grep, find, ls, write, edit, \
                session_info, submit_verification, web_fetch, web_search, list_models, agent, \
                agent_status, agent_yield, agent_send, agent_cancel, remind, \
                remind_status, remind_cancel, request_approval, approval_pending, \
                approval_revoke, schedule, schedule_list, schedule_cancel, task_queue, \
                update_plan, workflow_list, workflow_inspect, workflow_start, \
                workflow_status, otto_help</available_tools>"
    ));
    assert!(prompt.contains("<task_control_policy>"));
    assert!(prompt.contains("<workspace_instructions"), "{prompt}");
    assert!(prompt.ends_with("</otto_system_prompt>"), "{prompt}");
    assert!(prompt.contains("house rules"), "{prompt}");
    assert!(!prompt.contains("sk-secret-value"), "{prompt}");

    let report = runner.context_report();
    let parts = &report.sections[0].items;
    let labels: Vec<&str> = parts.iter().map(|part| part.label.as_str()).collect();
    assert_eq!(
        labels,
        [
            "Base",
            "Environment",
            "Workspace instructions",
            "Document end"
        ],
        "the parts must concatenate to the prompt, or the report falls back to one part"
    );
    assert!(parts[2].text.contains("house rules"));
}

#[tokio::test]
async fn configured_child_models_appear_only_in_the_parent_prompt() {
    let dir = tempfile::tempdir().expect("temp dir");
    let builder = builder(dir.path());
    let session = SharedSession::memory(Header::default());
    let mut runtime = runtime();
    runtime.subagent_models = vec!["cheap".into(), "strong".into()];
    let runner = builder
        .build_runner(&session, &runtime)
        .await
        .expect("runner");
    let prompt = runner.system_prompt();
    assert!(prompt.contains("<model id=\"cheap\"/>"));
    assert!(prompt.contains("<model id=\"strong\"/>"));
    let report = runner.context_report();
    assert!(
        report.sections[0]
            .items
            .iter()
            .any(|part| part.label == "Sub-agent models")
    );

    let child_prompt = builder.child_prompt_for(&runtime, "", "")(&[]);
    assert!(!child_prompt.contains("<subagent_models"));

    runtime.subagent_models = vec!["sk-secret-value".into()];
    let session = SharedSession::memory(Header::default());
    let runner = builder
        .build_runner(&session, &runtime)
        .await
        .expect("runner");
    assert!(!runner.system_prompt().contains("sk-secret-value"));
}

#[test]
fn runtime_info_reports_the_resolved_runtime_when_the_boundary_is_open() {
    let dir = tempfile::tempdir().expect("temp dir");
    let builder = builder(dir.path());
    let mut runtime = runtime();
    runtime.compaction.context_window = 128_000;
    let info = builder.runtime_info(&runtime);
    assert_eq!(info.provider, "default");
    assert_eq!(info.provider_type, "openai-compatible");
    assert_eq!(info.model_selector, "default/gpt-test");
    assert_eq!(info.model, "gpt-test");
    assert_eq!(info.context_window, 128_000);
    assert_eq!(info.sandbox.mode, SandboxMode::Unavailable);
}

#[test]
fn resolve_initial_runtime_ignores_session_selection() {
    let mut file = File {
        default_model: "work/profile-model".to_string(),
        ..File::default()
    };
    file.providers.insert(
        "work".to_string(),
        ProviderEntry {
            type_: "openai-compatible".to_string(),
            base_url: "https://gw.example.com/v1".to_string(),
            trusted_origins: vec!["https://gw.example.com".into()],
            api_key_env: "WORK_KEY".to_string(),
            models: std::collections::BTreeMap::from([
                ("profile-model".to_string(), ModelEntry::default()),
                ("session-model".to_string(), ModelEntry::default()),
            ]),
            ..ProviderEntry::default()
        },
    );
    let environment = HashMap::from([("WORK_KEY".to_string(), "sk-1".to_string())]);

    let fresh = resolve_initial_runtime(&file, &environment, None, &Overrides::default())
        .expect("fresh runtime");
    assert_eq!(fresh.model, "profile-model");

    let metadata = RuntimeMetadata {
        profile: "removed-profile".to_string(),
        provider: "removed-provider".to_string(),
        model: "session-model".to_string(),
    };
    let resumed =
        resolve_initial_runtime(&file, &environment, Some(&metadata), &Overrides::default())
            .expect("resumed runtime");
    assert_eq!(resumed.model, "profile-model");
    assert_eq!(resumed.provider_name, fresh.provider_name);
    assert_eq!(resumed.thinking, fresh.thinking);
    assert_eq!(resumed.model_selector.to_string(), "work/profile-model");

    let overrides = Overrides {
        model: "work/profile-model".to_string(),
        ..Overrides::default()
    };
    let overridden = resolve_initial_runtime(&file, &environment, Some(&metadata), &overrides)
        .expect("overridden runtime");
    assert_eq!(overridden.model, "profile-model");
    let overrides = Overrides {
        model: "work/session-model".to_string(),
        thinking: "high".to_string(),
        ..overrides
    };
    let overridden = resolve_initial_runtime(&file, &environment, Some(&metadata), &overrides)
        .expect("explicit model and thinking");
    assert_eq!(overridden.model, "session-model");
    assert_eq!(overridden.thinking, "high");
    let environment = HashMap::from([
        ("WORK_KEY".to_string(), "sk-1".to_string()),
        ("OTTO_MODEL".to_string(), "work/session-model".to_string()),
    ]);
    let from_environment =
        resolve_initial_runtime(&file, &environment, Some(&metadata), &Overrides::default())
            .expect("environment selection");
    assert_eq!(from_environment.model, "session-model");
}

#[test]
fn validate_session_workspace_rejects_a_different_workspace() {
    let dir = tempfile::tempdir().expect("temp dir");
    let other = tempfile::tempdir().expect("temp dir");
    let canonical = canonical_directory(dir.path()).expect("canonical");
    let workspace = canonical.to_string_lossy().into_owned();

    validate_session_workspace(dir.path().to_str().expect("utf-8"), &workspace)
        .expect("same workspace");

    let error = validate_session_workspace(other.path().to_str().expect("utf-8"), &workspace)
        .expect_err("mismatch");
    assert!(error.starts_with("session workspace "), "{error}");
    assert!(error.ends_with("does not match cwd"), "{error}");

    let missing =
        validate_session_workspace(dir.path().join("nope").to_str().expect("utf-8"), &workspace)
            .expect_err("missing");
    assert!(
        missing.starts_with("resolve session workspace: "),
        "{missing}"
    );
}

#[tokio::test]
async fn a_closed_boundary_builds_no_provider_client() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut builder = builder(dir.path());
    builder.sandbox_secrets = vec![builder.workspace_path.clone()];
    let session = SharedSession::memory(Header::default());
    let runner = builder
        .build_runner(&session, &runtime())
        .await
        .expect("runner");
    assert!(matches!(runner.provider(), ProviderClient::Unavailable));
}

#[tokio::test]
async fn a_memory_session_carries_its_header_and_name() {
    let session = SharedSession::memory(Header {
        id: "abc".to_string(),
        provider: "openai-compatible".to_string(),
        model: "gpt-test".to_string(),
        ..Header::default()
    });
    assert_eq!(session.header().id, "abc");
    assert_eq!(session.header().version, CURRENT_VERSION);
    assert_eq!(session.path(), "");
    assert_eq!(session.name(), "");
    session.rename("planning").expect("rename");
    assert_eq!(session.name(), "planning");
    session.rename("  ").expect_err("blank name");
    session
        .update_runtime(&RuntimeMetadata {
            profile: "work".to_string(),
            provider: "openai-compatible".to_string(),
            model: "gpt-next".to_string(),
        })
        .expect("update runtime");
    assert_eq!(session.header().model, "gpt-next");
}

#[test]
fn usage_collector_is_bound_to_the_runtime_and_session() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut builder = builder(dir.path());
    let store = Arc::new(crate::usage::Store::open_in_memory().expect("usage store"));
    builder.shared_mut().usage = Some(store.clone());
    let runtime = runtime();
    let session = SharedSession::memory(Header {
        id: "session-1".into(),
        workspace: builder.workspace_path.clone(),
        ..Header::default()
    });

    builder
        .usage_collector(&session, &runtime)
        .expect("collector")
        .record(&otto_core::agent::Event::ProviderUsage {
            usage: otto_core::model::Usage {
                input_tokens: 10,
                output_tokens: 2,
                cached_input_tokens: 4,
                ..Default::default()
            },
            present: true,
            cached_input_tokens_present: true,
        })
        .expect("record");

    let summary = store.summary(Some("session-1")).expect("summary");
    assert_eq!(summary.input_tokens, 10);
    assert_eq!(summary.cached_input_tokens, Some(4));
}

#[tokio::test]
async fn a_provider_without_an_api_key_starts_and_rejects_requests_with_recovery_steps() {
    use otto_core::provider::{Provider, ProviderError, Request};
    let dir = tempfile::tempdir().expect("temp dir");
    let builder = builder(dir.path());
    let session = SharedSession::memory(Header::default());
    let mut keyless = runtime();
    keyless.api_key = String::new();
    keyless.api_key_env = "GATEWAY_KEY".into();
    let runner = builder
        .build_runner(&session, &keyless)
        .await
        .expect("a missing key must not prevent building the session");
    let ProviderClient::Compat { client, .. } = runner.provider() else {
        panic!("expected an OpenAI-compatible client");
    };
    let settlement = client
        .complete(
            &Request::default(),
            &mut |_| {},
            &tokio_util::sync::CancellationToken::new(),
        )
        .await;
    assert_eq!(settlement.attempts, 0, "nothing may be sent without a key");
    let Err(ProviderError::Other(message)) = settlement.result else {
        panic!("expected a missing-key error");
    };
    assert!(message.contains("missing api key"), "{message}");
    assert!(message.contains("GATEWAY_KEY"), "{message}");
    assert!(message.contains("/model"), "{message}");
}
