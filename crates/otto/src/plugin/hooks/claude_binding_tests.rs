//! End-to-end runs of Claude-format hooks from an installed, trusted `claude` package.
use super::*;
use crate::sandbox::{Executor, FilesystemMode, NetworkMode, Policy, direct::DirectDriver};
use otto_core::config::File;
use std::{collections::HashMap, fs};

/// Prints the environment the hook runtime sets, as a `systemMessage` notice.
const ENVIRONMENT_SCRIPT: &str = "read input\n\
    printf '{\"systemMessage\":\"root=%s data=%s project=%s attended=%s legacy_root=%s\"}' \
    \"$CLAUDE_PLUGIN_ROOT\" \"$CLAUDE_PLUGIN_DATA\" \"$CLAUDE_PROJECT_DIR\" \
    \"$CLAUDE_CODE_SESSION_ATTENDED\" \"$PLUGIN_ROOT\"\n";

/// Installs `<home>/source-<name>` as a `claude` package whose `hooks/hooks.json` holds
/// `hooks`, and trusts it when `trust` is set.
fn install_claude_package(home: &Path, name: &str, hooks: Value, trust: bool) -> PathBuf {
    let source = home.join(format!("source-{name}"));
    fs::create_dir_all(source.join(".claude-plugin")).unwrap();
    fs::create_dir_all(source.join("hooks")).unwrap();
    fs::create_dir_all(source.join("scripts")).unwrap();
    fs::write(
        source.join(".claude-plugin/plugin.json"),
        json!({"name":name}).to_string(),
    )
    .unwrap();
    fs::write(
        source.join("hooks/hooks.json"),
        json!({"description":"test hooks","hooks":hooks}).to_string(),
    )
    .unwrap();
    fs::write(
        source.join("scripts/report-environment.sh"),
        ENVIRONMENT_SCRIPT,
    )
    .unwrap();
    let service = super::super::Service::new(home);
    let installed = service.install(source.to_str().unwrap(), None).unwrap();
    if trust {
        service.change(name, "trust").unwrap();
    }
    service.package_path(&installed.digest)
}

/// A runtime over the packages installed under `home`, with an unconfined direct executor.
fn runtime(home: &Path) -> (tempfile::TempDir, Arc<Runtime>, super::super::Snapshot) {
    let workspace_dir = tempfile::tempdir().unwrap();
    let workspace = fs::canonicalize(workspace_dir.path()).unwrap();
    let executor = Executor::new(
        Arc::new(DirectDriver::new()),
        Policy {
            filesystem: FilesystemMode::Unconfined,
            network: NetworkMode::Allow,
        },
        &workspace,
    )
    .unwrap();
    let host = HashMap::from([("HOME".to_string(), home.to_string_lossy().into_owned())]);
    let snapshot = super::super::Snapshot::load(home, &File::default(), &host);
    let runtime = Runtime::new(
        &snapshot,
        home,
        &Default::default(),
        Execution {
            executor: Some(Arc::new(executor)),
            environment: vec![
                "PATH=/usr/bin:/bin".into(),
                "CLAUDE_CODE_SESSION_ATTENDED=inherited".into(),
            ],
            package_environment: Default::default(),
            workspace,
            guard: None,
            secrets: vec!["test-private-value".into()],
            logger: Arc::new(crate::log::Logger::discard()),
        },
    )
    .unwrap();
    (workspace_dir, runtime, snapshot)
}

fn handler(command: &str) -> Value {
    json!({"type":"command","command":command,"timeout":5})
}

#[tokio::test]
async fn pre_tool_use_receives_the_claude_payload_and_exit_2_denies_with_stderr() {
    let home = tempfile::tempdir().unwrap();
    let script = "input=$(cat); \
        case \"$input\" in *'\"hook_event_name\":\"PreToolUse\"'*) ;; *) echo no-event >&2; exit 1;; esac; \
        case \"$input\" in *'\"tool_name\":\"Edit\"'*) ;; *) echo no-tool >&2; exit 1;; esac; \
        case \"$input\" in *\"\\\"file_path\\\":\\\"$CLAUDE_PROJECT_DIR/a.rs\\\"\"*) ;; *) echo no-path >&2; exit 1;; esac; \
        case \"$input\" in *'\"cwd\":'*'\"session_id\":\"s\"'*|*'\"session_id\":\"s\"'*'\"cwd\":'*) ;; *) echo no-common >&2; exit 1;; esac; \
        echo 'edits are blocked' >&2; exit 2";
    install_claude_package(
        home.path(),
        "demo",
        json!({"PreToolUse":[{"matcher":"Edit|Write","hooks":[handler(script)]}]}),
        true,
    );
    let (_workspace, runtime, snapshot) = runtime(home.path());
    assert!(
        snapshot.diagnostics.is_empty(),
        "{:?}",
        snapshot.diagnostics
    );
    assert!(runtime.has_hooks());
    let bound = runtime.bind(super::tests::binding(), None);
    let response = bound
        .run(
            "before_tool",
            json!({"tool":"edit","tool_call_id":"c1","arguments":{"path":"a.rs","old_text":"x","new_text":"y"}}),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(response.decision, Some(Decision::Deny));
    assert_eq!(response.reason.as_deref(), Some("edits are blocked"));
    // The matcher names Edit and Write; a Bash call does not run the hook.
    let response = bound
        .run(
            "before_tool",
            json!({"tool":"bash","arguments":{"command":"ls"}}),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(response.decision, None);
}

#[tokio::test]
async fn updated_input_is_translated_back_to_otto_arguments() {
    let home = tempfile::tempdir().unwrap();
    let script = "read input; printf '%s' \"{\\\"hookSpecificOutput\\\":{\\\"hookEventName\\\":\\\"PreToolUse\\\",\
        \\\"permissionDecision\\\":\\\"allow\\\",\\\"updatedInput\\\":{\\\"file_path\\\":\\\"$CLAUDE_PROJECT_DIR/b.rs\\\",\
        \\\"old_string\\\":\\\"x\\\",\\\"new_string\\\":\\\"y\\\"}}}\"";
    install_claude_package(
        home.path(),
        "demo",
        json!({"PreToolUse":[{"matcher":"Edit","hooks":[handler(script)]}]}),
        true,
    );
    let (_workspace, runtime, _snapshot) = runtime(home.path());
    let response = runtime
        .bind(super::tests::binding(), None)
        .run(
            "before_tool",
            json!({"tool":"edit","arguments":{"path":"a.rs","old_text":"o","new_text":"n"}}),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    // The runner forwards only Deny; an allow leaves the combined decision unset.
    assert_ne!(response.decision, Some(Decision::Deny));
    assert_eq!(
        response.arguments,
        Some(json!({"path":"b.rs","old_text":"x","new_text":"y"}))
    );
}

#[tokio::test]
async fn environment_and_placeholders_reach_the_handler() {
    let home = tempfile::tempdir().unwrap();
    let package_root = install_claude_package(
        home.path(),
        "demo",
        json!({"Stop":[{"hooks":[handler("sh \"${CLAUDE_PLUGIN_ROOT}/scripts/report-environment.sh\"")]}]}),
        true,
    );
    let (workspace, runtime, _snapshot) = runtime(home.path());
    let workspace_path = fs::canonicalize(workspace.path()).unwrap();
    let data_path = super::super::Service::new(home.path())
        .ensure_data("demo")
        .unwrap();
    let expected = |attended: &str| {
        format!(
            "root={root} data={data} project={project} attended={attended} legacy_root={root}",
            root = package_root.display(),
            data = data_path.display(),
            project = workspace_path.display(),
        )
    };
    let turn = json!({"status":"success","turn":{"messages":[]}});
    let response = runtime
        .bind(super::tests::binding(), None)
        .run("turn_end", turn.clone(), &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(response.notice.as_deref(), Some(expected("1").as_str()));
    let headless = SessionBinding {
        headless: true,
        ..super::tests::binding()
    };
    let response = runtime
        .bind(headless, None)
        .run("turn_end", turn, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(response.notice.as_deref(), Some(expected("0").as_str()));
    // A cancelled turn produces no Stop event, so the hook does not run.
    let response = runtime
        .bind(super::tests::binding(), None)
        .run(
            "turn_end",
            json!({"status":"cancelled"}),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(response.notice, None);
}

#[tokio::test]
async fn session_start_and_subagent_start_hooks_fire_for_their_own_sessions() {
    let home = tempfile::tempdir().unwrap();
    install_claude_package(
        home.path(),
        "demo",
        json!({
            "SessionStart":[{"matcher":"startup","hooks":[handler(
                "read input; printf '%s' '{\"hookSpecificOutput\":{\"hookEventName\":\"SessionStart\",\"additionalContext\":\"parent context\"}}'"
            )]}],
            "SubagentStart":[{"hooks":[handler("read input; printf '%s' '{\"systemMessage\":\"child started\"}'")]}]
        }),
        true,
    );
    let (_workspace, runtime, _snapshot) = runtime(home.path());
    let response = runtime
        .bind(super::tests::binding(), None)
        .run("session_start", json!({}), &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(response.context.as_deref(), Some("\nparent context"));
    assert_eq!(response.notice, None);
    let child = SessionBinding {
        parent_session_id: Some("parent".into()),
        task_id: "t1".into(),
        ..super::tests::binding()
    };
    let response = runtime
        .bind(child, None)
        .run("session_start", json!({}), &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(response.context, None);
    assert_eq!(response.notice.as_deref(), Some("child started"));
    // A resumed session does not match the `startup` matcher.
    let resumed = SessionBinding {
        mode: "resume".into(),
        ..super::tests::binding()
    };
    let response = runtime
        .bind(resumed, None)
        .run("session_start", json!({}), &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(response.context, None);
}

#[tokio::test]
async fn post_tool_use_appends_stderr_and_nonzero_exits_become_notices() {
    let home = tempfile::tempdir().unwrap();
    install_claude_package(
        home.path(),
        "demo",
        json!({
            "PostToolUse":[{"hooks":[handler("read input; echo 'remember to run tests' >&2; exit 2")]}],
            "UserPromptSubmit":[{"hooks":[handler("read input; echo 'test-private-value failed' >&2; exit 3")]}]
        }),
        true,
    );
    let (_workspace, runtime, _snapshot) = runtime(home.path());
    let bound = runtime.bind(super::tests::binding(), None);
    let response = bound
        .run(
            "after_tool",
            json!({"tool":"bash","content":"stdout:\nok\n","is_error":false}),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.content.as_deref(),
        Some("stdout:\nok\n\n[hook] remember to run tests")
    );
    let response = bound
        .run(
            "before_prompt",
            json!({"prompt":"hi"}),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(response.decision, None);
    assert_eq!(
        response.notice.as_deref(),
        Some("hook exited 3: [REDACTED] failed")
    );
}

#[tokio::test]
async fn a_timed_out_pre_hook_fails_closed() {
    let home = tempfile::tempdir().unwrap();
    install_claude_package(
        home.path(),
        "demo",
        json!({"PreToolUse":[{"hooks":[json!({"type":"command","command":"sleep 20","timeout":1})]}]}),
        true,
    );
    let (_workspace, runtime, _snapshot) = runtime(home.path());
    let result = runtime
        .bind(super::tests::binding(), None)
        .run(
            "before_tool",
            json!({"tool":"bash","arguments":{"command":"ls"}}),
            &CancellationToken::new(),
        )
        .await;
    assert_eq!(result.unwrap_err(), "plugin command timed out");
}

#[tokio::test]
async fn untrusted_package_hooks_do_not_run_and_are_reported() {
    let home = tempfile::tempdir().unwrap();
    install_claude_package(
        home.path(),
        "demo",
        json!({"PreToolUse":[{"hooks":[handler("exit 2")]}]}),
        false,
    );
    let (_workspace, runtime, snapshot) = runtime(home.path());
    assert!(!runtime.has_hooks());
    assert!(
        snapshot.diagnostics.iter().any(|d| d
            == "plugin demo: hooks, custom tools and memory disabled; run otto plugin trust demo"),
        "{:?}",
        snapshot.diagnostics
    );
}
