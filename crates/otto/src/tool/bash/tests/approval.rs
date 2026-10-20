//! Tests for elevation, read approval, excluded commands and the effect gate.

use super::*;

#[tokio::test]
async fn elevated_command_requires_an_exact_one_shot_approval() {
    let (_dir, workspace) = temp_workspace();
    let confined = Arc::new(FakeExecutor::default());
    let elevated = Arc::new(FakeExecutor::default());
    let approvals = Arc::new(BashApprovals::new(
        elevated.clone(),
        strings(&["HOME=/real-home"]),
    ));
    let tool = bash(&workspace, confined.clone(), &[], 1024, &[])
        .with_approvals("session-1", approvals.clone());

    let requested = run_escalated(&tool, "git push", "push the reviewed branch").await;
    assert!(requested.is_error);
    assert_eq!(
        requested.content,
        "approval required for unsandboxed bash execution.\n\
Approve in Otto: /approve approval-1\n\
Command: \"git push\"\n\
Justification: \"push the reviewed branch\"\n\
The command did not run. Only the user or their session approval mode \
            can approve it in Otto; do not run /approve in a shell. Ask the user to \
            review the command above, then retry this exact elevated Bash command \
            after approval."
    );
    assert_eq!(confined.calls(), 0);
    assert_eq!(elevated.calls(), 0);

    assert!(approvals.approve("another-session", "approval-1").is_err());
    approvals
        .approve("session-1", "approval-1")
        .expect("approve exact request");
    run_escalated(&tool, "git status", "different command").await;
    assert_eq!(elevated.calls(), 0);

    let requested = run_escalated(&tool, "git push", "push the reviewed branch").await;
    assert_eq!(
        requested.content,
        "approval required for unsandboxed bash execution.\n\
Approve in Otto: /approve approval-3\n\
Command: \"git push\"\n\
Justification: \"push the reviewed branch\"\n\
The command did not run. Only the user or their session approval mode \
            can approve it in Otto; do not run /approve in a shell. Ask the user to \
            review the command above, then retry this exact elevated Bash command \
            after approval."
    );
    approvals
        .approve("session-1", "approval-3")
        .expect("approve exact request");

    let approved = run_escalated(&tool, "git push", "push the reviewed branch").await;
    assert!(!approved.is_error);
    assert_eq!(elevated.calls(), 1);
    assert_eq!(elevated.requests()[0].env, strings(&["HOME=/real-home"]));

    let consumed = run_escalated(&tool, "git push", "push the reviewed branch").await;
    assert!(consumed.is_error);
    assert_eq!(elevated.calls(), 1);
}

#[tokio::test]
async fn read_approval_is_bound_to_path_command_and_session_and_stays_confined() {
    let (_dir, workspace) = temp_workspace();
    let path = tempfile::tempdir().unwrap();
    let path = path.path().canonicalize().unwrap();
    let path = path.to_str().unwrap();
    let confined = Arc::new(FakeExecutor::default());
    let elevated = Arc::new(FakeExecutor::default());
    let approvals = Arc::new(BashApprovals::new(elevated.clone(), Vec::new()));
    let tool = bash(&workspace, confined.clone(), &[], 4096, &[])
        .with_approvals("s1", approvals.clone())
        .with_excluded_commands(excluded(&elevated, &["cat *"]));
    let args = json!({"command": "cat fixture", "sandbox_read_path": path, "justification": "read fixture"}).to_string();
    let result = tool.execute(&raw(&args), &CancellationToken::new()).await;
    let request = parse_approval_request("bash", &result).unwrap();
    assert_eq!(request.read_path, path);
    assert!(result.content.contains("saves the path to read_paths"));
    assert_eq!(confined.calls(), 0);
    assert_eq!(elevated.calls(), 0);
    assert!(approvals.approve("s2", &request.id).is_err());
    approvals.approve("s1", &request.id).unwrap();
    // A read grant cannot become an unsandboxed grant.
    assert!(!approvals.take("s1", "cat fixture"));
    assert!(!approvals.take_read_path("s1", "different", path));
    assert!(!approvals.take_read_path("s1", "cat fixture", "/different"));
    let result = tool.execute(&raw(&args), &CancellationToken::new()).await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(confined.calls(), 1);
    assert_eq!(elevated.calls(), 0);
    // The retry marker is consumed, independently of persistent config.
    let result = tool.execute(&raw(&args), &CancellationToken::new()).await;
    let fresh = parse_approval_request("bash", &result).unwrap();
    assert_ne!(fresh.id, request.id);
    approvals.discard("s1", &request.id);
    assert_eq!(approvals.pending_count("s1"), 1);
    approvals.discard("s1", &fresh.id);
    assert!(approvals.approve("s1", &fresh.id).is_err());
}

#[tokio::test]
async fn read_approval_rejects_invalid_paths_missing_reasons_and_elevation() {
    let (_dir, workspace) = temp_workspace();
    let executor = Arc::new(FakeExecutor::default());
    let approvals = Arc::new(BashApprovals::new(executor.clone(), Vec::new()));
    let tool =
        bash(&workspace, executor.clone(), &[], 4096, &[]).with_approvals("s1", approvals.clone());
    for args in [
        json!({"command": "cat x", "sandbox_read_path": "relative", "justification": "read"}),
        json!({"command": "cat x", "sandbox_read_path": "/otto-test-missing-read-path", "justification": "read"}),
        json!({"command": "cat x", "sandbox_read_path": "/tmp"}),
        json!({"command": "cat x", "sandbox_read_path": "/tmp", "justification": "read", "sandbox_permissions": "require_escalated"}),
    ] {
        let result = tool
            .execute(&raw(&args.to_string()), &CancellationToken::new())
            .await;
        assert!(result.is_error);
        assert!(parse_approval_request("bash", &result).is_none());
    }
    assert_eq!(executor.calls(), 0);
    assert_eq!(approvals.pending_count("s1"), 0);
}

fn excluded(executor: &Arc<FakeExecutor>, entries: &[&str]) -> Arc<ExcludedCommands> {
    Arc::new(ExcludedCommands::new(
        executor.clone(),
        strings(&["HOME=/real-home"]),
        strings(entries),
    ))
}

#[tokio::test]
async fn excluded_command_runs_unconfined_and_compound_does_not() {
    let (_dir, workspace) = temp_workspace();
    let confined = Arc::new(FakeExecutor::default());
    let unconfined = Arc::new(FakeExecutor::default());
    let tool = bash(&workspace, confined.clone(), &["HOME=/sandbox"], 1024, &[])
        .with_excluded_commands(excluded(&unconfined, &["lark-cli *"]));

    run(&tool, "lark-cli doc get").await;
    assert_eq!((confined.calls(), unconfined.calls()), (0, 1));
    assert_eq!(unconfined.requests()[0].env, strings(&["HOME=/real-home"]));

    run(&tool, "git status").await;
    run(&tool, "lark-cli x && rm -rf y").await;
    assert_eq!((confined.calls(), unconfined.calls()), (2, 1));
    assert_eq!(confined.requests()[0].env, strings(&["HOME=/sandbox"]));
}

#[tokio::test]
async fn excluded_command_with_require_escalated_needs_no_approval() {
    let (_dir, workspace) = temp_workspace();
    let confined = Arc::new(FakeExecutor::default());
    let unconfined = Arc::new(FakeExecutor::default());
    let approvals = Arc::new(BashApprovals::new(
        Arc::new(FakeExecutor::default()),
        Vec::new(),
    ));
    let tool = bash(&workspace, confined.clone(), &[], 1024, &[])
        .with_approvals("session-1", approvals)
        .with_excluded_commands(excluded(&unconfined, &["lark-cli *"]));

    let result = run_escalated(&tool, "lark-cli doc get", "needs keychain").await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!((confined.calls(), unconfined.calls()), (0, 1));
}

#[tokio::test]
async fn set_entries_changes_which_commands_are_excluded() {
    let (_dir, workspace) = temp_workspace();
    let confined = Arc::new(FakeExecutor::default());
    let unconfined = Arc::new(FakeExecutor::default());
    let list = excluded(&unconfined, &[]);
    let tool =
        bash(&workspace, confined.clone(), &[], 1024, &[]).with_excluded_commands(list.clone());

    run(&tool, "lark-cli doc get").await;
    assert_eq!((confined.calls(), unconfined.calls()), (1, 0));

    list.set_entries(strings(&["lark-cli *"]));
    run(&tool, "lark-cli doc get").await;
    assert_eq!((confined.calls(), unconfined.calls()), (1, 1));
}

#[tokio::test]
async fn elevated_command_requires_a_justification() {
    let (_dir, workspace) = temp_workspace();
    let approvals = Arc::new(BashApprovals::new(
        Arc::new(FakeExecutor::default()),
        Vec::new(),
    ));
    let tool = bash(
        &workspace,
        Arc::new(FakeExecutor::default()),
        &[],
        1024,
        &[],
    )
    .with_approvals("session-1", approvals);

    let result = run_escalated(&tool, "git push", "  ").await;
    assert!(result.is_error);
    assert_eq!(
        result.content,
        "justification is required for elevated execution"
    );
}

fn gated_tool(
    workspace: &Workspace,
    sandboxed: &Arc<FakeExecutor>,
    elevated: &Arc<FakeExecutor>,
) -> (BashTool, Arc<BashApprovals>) {
    let approvals = Arc::new(BashApprovals::new(elevated.clone(), Vec::new()));
    let tool =
        bash(workspace, sandboxed.clone(), &[], 1024, &[]).with_approvals("s1", approvals.clone());
    (tool, approvals)
}

fn use_default_spec(tool: &BashTool, command: &str) -> Option<super::super::super::ApprovalSpec> {
    let arguments = json!({ "command": command }).to_string();
    tool.approval_spec(&raw(&arguments)).unwrap()
}

#[test]
fn sandboxed_effect_commands_need_approval_without_a_justification() {
    let (_dir, workspace) = temp_workspace();
    let sandboxed = Arc::new(FakeExecutor::default());
    let elevated = Arc::new(FakeExecutor::default());
    let (tool, _approvals) = gated_tool(&workspace, &sandboxed, &elevated);

    let spec = use_default_spec(&tool, "git push origin main").unwrap();
    assert_eq!(spec.authority, otto_core::approval::Authority::Boundary);
    assert_eq!(spec.title, "Run a command that changes external state?");
    assert_eq!(spec.source, "sandbox");
    assert!(!spec.persistent);
    assert!(
        spec.reason.starts_with("Changes external state: "),
        "{}",
        spec.reason
    );
    let details: serde_json::Value = serde_json::from_str(&spec.details).unwrap();
    assert_eq!(details["sandboxed"], true);
    assert_eq!(details["read_path"], "");
    assert_eq!(details["effect"]["class"], "external_state");
    assert_eq!(
        details["effect"]["segment"],
        json!(["git", "push", "origin", "main"])
    );

    let spec = use_default_spec(&tool, "rm -rf build").unwrap();
    assert_eq!(spec.authority, otto_core::approval::Authority::Bounded);
    assert!(
        spec.reason
            .starts_with("Deletes workspace or repository content: ")
    );
    let details: serde_json::Value = serde_json::from_str(&spec.details).unwrap();
    assert_eq!(details["effect"]["class"], "destructive");

    assert!(use_default_spec(&tool, "git push --dry-run").is_none());
    assert!(use_default_spec(&tool, "ls").is_none());

    // A request for escalation keeps its own spec.
    let escalated = json!({"command":"git push","sandbox_permissions":"require_escalated","justification":"publish"}).to_string();
    let spec = tool.approval_spec(&raw(&escalated)).unwrap().unwrap();
    assert_eq!(spec.title, "Run an unsandboxed command?");
    assert_eq!(spec.reason, "publish");
    let details: serde_json::Value = serde_json::from_str(&spec.details).unwrap();
    assert!(details.get("sandboxed").is_none() && details.get("effect").is_none());
}

#[test]
fn effect_gate_needs_approvals_and_covers_excluded_commands() {
    let (_dir, workspace) = temp_workspace();
    let sandboxed = Arc::new(FakeExecutor::default());
    let plain = bash(&workspace, sandboxed.clone(), &[], 1024, &[]);
    assert!(use_default_spec(&plain, "git push").is_none());

    let list = excluded(&sandboxed, &["git push *", "lark-cli *"]);
    let (tool, _approvals) = gated_tool(&workspace, &sandboxed, &sandboxed);
    let tool = tool.with_excluded_commands(list);
    let spec = use_default_spec(&tool, "git push origin").unwrap();
    assert_eq!(spec.authority, otto_core::approval::Authority::Boundary);
    let details: serde_json::Value = serde_json::from_str(&spec.details).unwrap();
    assert_eq!(details["sandboxed"], false);
    assert_eq!(details["effect"]["class"], "external_state");

    let escalated = json!({"command":"git push origin","sandbox_permissions":"require_escalated","justification":"publish"}).to_string();
    let spec = tool.approval_spec(&raw(&escalated)).unwrap().unwrap();
    assert_eq!(spec.authority, otto_core::approval::Authority::Boundary);
    let details: serde_json::Value = serde_json::from_str(&spec.details).unwrap();
    assert_eq!(details["sandboxed"], false);

    assert!(use_default_spec(&tool, "lark-cli docs list").is_none());
}

fn excluded_effect_tool(
    workspace: &Workspace,
    sandboxed: &Arc<FakeExecutor>,
    unconfined: &Arc<FakeExecutor>,
) -> BashTool {
    let (tool, _approvals) = gated_tool(workspace, sandboxed, &Arc::new(FakeExecutor::default()));
    tool.with_excluded_commands(excluded(unconfined, &["git push *"]))
}

#[tokio::test]
async fn unauthorized_excluded_effect_command_does_not_run() {
    let (_dir, workspace) = temp_workspace();
    let sandboxed = Arc::new(FakeExecutor::default());
    let unconfined = Arc::new(FakeExecutor::default());
    let tool = excluded_effect_tool(&workspace, &sandboxed, &unconfined);

    let result = run(&tool, "git push origin").await;
    assert!(result.is_error);
    assert!(
        result.content.contains("approval required"),
        "{}",
        result.content
    );
    assert_eq!((sandboxed.calls(), unconfined.calls()), (0, 0));
}

#[tokio::test]
async fn approved_excluded_effect_command_runs_unconfined() {
    let (_dir, workspace) = temp_workspace();
    let sandboxed = Arc::new(FakeExecutor::default());
    let unconfined = Arc::new(FakeExecutor::default());
    let tool = excluded_effect_tool(&workspace, &sandboxed, &unconfined);

    let arguments = json!({ "command": "git push origin" }).to_string();
    let result = tool
        .execute_inner(&raw(&arguments), &CancellationToken::new(), true)
        .await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!((sandboxed.calls(), unconfined.calls()), (0, 1));
    let request = &unconfined.requests()[0];
    assert!(request.argv.iter().any(|arg| arg == "git push origin"));
    assert_eq!(request.env, strings(&["HOME=/real-home"]));
}

#[tokio::test]
async fn unauthorized_effect_command_does_not_run_or_create_a_grant() {
    let (_dir, workspace) = temp_workspace();
    let sandboxed = Arc::new(FakeExecutor::default());
    let elevated = Arc::new(FakeExecutor::default());
    let (tool, approvals) = gated_tool(&workspace, &sandboxed, &elevated);

    let result = run(&tool, "rm -rf build").await;
    assert!(result.is_error);
    assert!(
        result.content.contains("approval required"),
        "{}",
        result.content
    );
    assert_eq!((sandboxed.calls(), elevated.calls()), (0, 0));
    assert_eq!(approvals.pending_count("s1"), 0);
    assert!(!approvals.take("s1", "rm -rf build"));

    let arguments = json!({ "command": "rm -rf build" }).to_string();
    let result = tool
        .execute_inner(&raw(&arguments), &CancellationToken::new(), true)
        .await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!((sandboxed.calls(), elevated.calls()), (1, 0));

    let result = run(&tool, "ls").await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!((sandboxed.calls(), elevated.calls()), (2, 0));
}

#[test]
fn pending_count_counts_entries_for_its_session() {
    let approvals = BashApprovals::new(Arc::new(FakeExecutor::default()), Vec::new());
    assert_eq!(approvals.pending_count("session-1"), 0);
    approvals.request("session-1", "git push");
    assert_eq!(approvals.pending_count("session-1"), 1);
    assert_eq!(approvals.pending_count("other-session"), 0);
}
