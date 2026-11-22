//! The `before_provider_call` and `after_provider_call` events in the hook runtime:
//! configuration, response fields, edit merging, `handles` and Claude-format hooks.
use super::binding::check_response_fields;
use super::*;
use crate::sandbox::{Executor, FilesystemMode, NetworkMode, Policy, direct::DirectDriver};
use otto_core::agent::hooks::Edit;
use std::{collections::HashMap, fs};

fn execution(workspace: PathBuf) -> Execution {
    let executor = Executor::new(
        Arc::new(DirectDriver::new()),
        Policy {
            filesystem: FilesystemMode::Unconfined,
            network: NetworkMode::Allow,
        },
        &workspace,
    )
    .unwrap();
    Execution {
        executor: Some(Arc::new(executor)),
        environment: vec!["PATH=/usr/bin:/bin".into()],
        package_environment: Default::default(),
        workspace,
        guard: None,
        secrets: vec!["test-private-value".into()],
        logger: Arc::new(otto_platform::log::Logger::discard()),
    }
}

fn runtime(
    snapshot: &super::super::Snapshot,
    home: &Path,
    hooks: otto_core::config::plugins::Hooks,
) -> (tempfile::TempDir, Arc<Runtime>) {
    let workspace = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(workspace.path()).unwrap();
    let runtime = Runtime::new(snapshot, home, &hooks, execution(root)).unwrap();
    (workspace, runtime)
}

fn hook(script: &str) -> Hook {
    Hook {
        command: vec!["/bin/sh".into(), "-c".into(), script.into()],
        timeout: "5s".into(),
        matcher: Vec::new(),
        lua: None,
    }
}

fn edit(message: usize, block: usize, text: &str) -> Edit {
    Edit {
        message,
        block,
        text: text.into(),
    }
}

#[test]
fn response_fields_are_checked_per_event() {
    let request_change = Response {
        decision: Some(Decision::Allow),
        reason: Some("r".into()),
        context: Some("c".into()),
        system_prompt: Some("s".into()),
        edits: Some(vec![edit(0, 0, "t")]),
        notice: Some("n".into()),
        ..Response::default()
    };
    assert!(check_response_fields("before_provider_call", &request_change).is_ok());
    for event in [
        "after_provider_call",
        "before_prompt",
        "before_tool",
        "turn_end",
    ] {
        assert!(
            check_response_fields(event, &request_change).is_err(),
            "{event}"
        );
    }
    let rejected_before = [
        Response {
            arguments: Some(json!({})),
            ..Response::default()
        },
        Response {
            content: Some("c".into()),
            ..Response::default()
        },
    ];
    for response in &rejected_before {
        assert!(check_response_fields("before_provider_call", response).is_err());
    }
    let notice = Response {
        notice: Some("n".into()),
        ..Response::default()
    };
    assert!(check_response_fields("after_provider_call", &notice).is_ok());
    let rejected_after = [
        Response {
            system_prompt: Some("s".into()),
            ..notice.clone()
        },
        Response {
            edits: Some(Vec::new()),
            ..notice.clone()
        },
        Response {
            context: Some("c".into()),
            ..notice.clone()
        },
        Response {
            decision: Some(Decision::Deny),
            ..notice.clone()
        },
    ];
    for response in &rejected_after {
        assert!(check_response_fields("after_provider_call", response).is_err());
    }
    // Edits must name indices; unknown edit fields are rejected.
    assert!(
        serde_json::from_str::<Response>(r#"{"edits":[{"message":-1,"block":0,"text":""}]}"#)
            .is_err()
    );
    assert!(
        serde_json::from_str::<Response>(
            r#"{"edits":[{"message":0,"block":0,"text":"","role":"user"}]}"#
        )
        .is_err()
    );
}

#[test]
fn toml_and_plugin_json_accept_the_events_without_matchers() {
    let file: otto_core::config::File = toml::from_str(
        "[[hooks.before_provider_call]]\ncommand=[\"/bin/true\"]\n\
         [[hooks.after_provider_call]]\ncommand=[\"/bin/true\"]\ntimeout=\"2s\"\n",
    )
    .unwrap();
    assert_eq!(file.hooks.before_provider_call.len(), 1);
    assert_eq!(file.hooks.after_provider_call[0].timeout, "2s");
    let events: Vec<&str> = file
        .hooks
        .entries()
        .iter()
        .filter(|(_, hooks)| !hooks.is_empty())
        .map(|(event, _)| *event)
        .collect();
    assert_eq!(events, ["before_provider_call", "after_provider_call"]);

    let components: Components = serde_json::from_value(json!({
        "version": 1,
        "hooks": {
            "before_provider_call": [{"command": ["/bin/true"]}],
            "after_provider_call": [{"command": ["/bin/true"]}]
        }
    }))
    .unwrap();
    assert!(validate_components("example", &components).is_ok());
    for event in ["before_provider_call", "after_provider_call"] {
        let components: Components = serde_json::from_value(json!({
            "version": 1,
            "hooks": {event: [{"command": ["/bin/true"], "matcher": ["write"]}]}
        }))
        .unwrap();
        assert!(
            validate_components("example", &components).is_err(),
            "{event}"
        );
        let mut standalone = otto_core::config::plugins::Hooks::default();
        let mut with_matcher = hook("true");
        with_matcher.matcher = vec!["write".into()];
        match event {
            "before_provider_call" => standalone.before_provider_call = vec![with_matcher],
            _ => standalone.after_provider_call = vec![with_matcher],
        }
        let workspace = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(workspace.path()).unwrap();
        assert!(
            Runtime::new(
                &Default::default(),
                workspace.path(),
                &standalone,
                execution(root)
            )
            .is_err(),
            "{event}"
        );
    }
}

#[tokio::test]
async fn later_hooks_see_earlier_changes_and_their_edits_replace_earlier_ones() {
    // The first hook replaces the prompt and edits two blocks; the second sees both
    // changes and edits one of the same blocks again.
    let first = hook(
        "read input; printf '%s' '{\"system_prompt\":\"first prompt\",\"edits\":[{\"message\":0,\"block\":0,\"text\":\"one\"},{\"message\":0,\"block\":1,\"text\":\"two\"}]}'",
    );
    let second = hook(
        "read input; case \"$input\" in *'\"system_prompt\":\"first prompt\"'*) ;; *) exit 3;; esac; \
         case \"$input\" in *'\"text\":\"one\"'*) ;; *) exit 4;; esac; \
         printf '%s' '{\"context\":\"note\",\"edits\":[{\"message\":0,\"block\":0,\"text\":\"three\"}]}'",
    );
    let home = tempfile::tempdir().unwrap();
    let (_workspace, runtime) = runtime(
        &Default::default(),
        home.path(),
        otto_core::config::plugins::Hooks {
            before_provider_call: vec![first, second],
            ..Default::default()
        },
    );
    let response = runtime
        .bind(super::tests::binding(), None)
        .run(
            "before_provider_call",
            json!({
                "system_prompt": "original",
                "messages": [{"role": "user", "blocks": [
                    {"type": "text", "text": "a"},
                    {"type": "text", "text": "b"}
                ]}]
            }),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(response.system_prompt.as_deref(), Some("first prompt"));
    assert_eq!(response.context.as_deref(), Some("\nnote"));
    assert_eq!(
        response.edits,
        Some(vec![edit(0, 1, "two"), edit(0, 0, "three")])
    );
}

#[tokio::test]
async fn handles_reports_only_configured_events() {
    let home = tempfile::tempdir().unwrap();
    let (_workspace, runtime) = runtime(
        &Default::default(),
        home.path(),
        otto_core::config::plugins::Hooks {
            before_provider_call: vec![hook("true")],
            ..Default::default()
        },
    );
    let bound = runtime.bind(super::tests::binding(), None);
    assert!(bound.handles("before_provider_call"));
    assert!(!bound.handles("after_provider_call"));
    assert!(!bound.handles("before_tool"));

    let (_empty_workspace, empty) =
        self::runtime(&Default::default(), home.path(), Default::default());
    let bound = empty.bind(super::tests::binding(), None);
    assert!(!bound.handles("before_provider_call"));
    assert!(!bound.handles("session_start"));
}

/// Installs and trusts a `claude` package whose hooks configure `PreToolUse`.
fn install_claude_package(home: &Path) {
    let source = home.join("source-claude-example");
    fs::create_dir_all(source.join(".claude-plugin")).unwrap();
    fs::create_dir_all(source.join("hooks")).unwrap();
    fs::write(
        source.join(".claude-plugin/plugin.json"),
        json!({"name": "claude-example"}).to_string(),
    )
    .unwrap();
    let handler = json!({"type": "command", "command": "exit 2", "timeout": 5});
    fs::write(
        source.join("hooks/hooks.json"),
        json!({"hooks": {"PreToolUse": [{"hooks": [handler]}]}}).to_string(),
    )
    .unwrap();
    let service = super::super::Service::new(home);
    service.install(source.to_str().unwrap(), None).unwrap();
    service.change("claude-example", "trust").unwrap();
}

#[tokio::test]
async fn claude_format_hooks_never_receive_provider_call_events() {
    assert!(
        crate::plugin::claude_hooks::EVENTS
            .iter()
            .all(|(_, otto)| !otto.ends_with("_provider_call"))
    );
    let session = crate::plugin::claude_hooks::payload::SessionFields {
        session_id: "s",
        parent_session_id: None,
        task_id: "t",
        mode: "new",
        history_path: "",
    };
    for event in ["before_provider_call", "after_provider_call"] {
        let data = json!({"messages": [], "outcome": "success"});
        assert!(
            crate::plugin::claude_hooks::payload::translate(
                event,
                &data,
                &session,
                Path::new("/ws")
            )
            .is_none()
        );
    }

    let home = tempfile::tempdir().unwrap();
    install_claude_package(home.path());
    let host = HashMap::from([(
        "HOME".to_string(),
        home.path().to_string_lossy().into_owned(),
    )]);
    let snapshot =
        super::super::Snapshot::load(home.path(), &otto_core::config::File::default(), &host);
    let (_workspace, runtime) = runtime(&snapshot, home.path(), Default::default());
    let bound = runtime.bind(super::tests::binding(), None);
    assert!(bound.handles("before_tool"));
    assert!(!bound.handles("before_provider_call"));
    // The `exit 2` handler would deny if it ran; the provider events reach no Claude hook.
    for event in ["before_provider_call", "after_provider_call"] {
        let response = bound
            .run(event, json!({"messages": []}), &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(response.decision, None, "{event}");
    }
    let denied = bound
        .run(
            "before_tool",
            json!({"tool": "write", "arguments": {}}),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(denied.decision, Some(Decision::Deny));
}
