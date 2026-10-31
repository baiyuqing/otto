//! Tests for the approval facts derived from scripts, builds, git and redirects.

use super::*;

#[test]
fn escalated_local_script_facts_bind_content_and_change_the_prepared_version() {
    let (_dir, workspace) = temp_workspace();
    std::fs::write(workspace.root().join("script.sh"), "printf first\n").unwrap();
    let executor = Arc::new(FakeExecutor::default());
    let tool = bash(&workspace, executor.clone(), &[], 1024, &[])
        .with_approvals("root", Arc::new(BashApprovals::new(executor, Vec::new())));
    let args = raw(
        r#"{"command":"./script.sh","sandbox_permissions":"require_escalated","justification":"Run the requested local script"}"#,
    );
    let first = tool.approval_spec(&args).unwrap().unwrap();
    let facts: serde_json::Value = serde_json::from_str(&first.details).unwrap();
    assert_eq!(facts["native_facts"]["files"][0]["body"], "printf first\n");
    std::fs::write(workspace.root().join("script.sh"), "printf changed\n").unwrap();
    let changed = tool.approval_spec(&args).unwrap().unwrap();
    assert_ne!(first.version, changed.version);
    assert_ne!(first.details, changed.details);
    assert!(
        tool.approval_spec(&raw(r#"{"command":"./script.sh"}"#))
            .unwrap()
            .is_none()
    );
}

#[test]
fn script_payload_facts_redact_secrets_and_refuse_workspace_escape() {
    let (_dir, workspace) = temp_workspace();
    std::fs::write(
        workspace.root().join("script.sh"),
        "printf private-payload-secret\n",
    )
    .unwrap();
    let tool = bash(
        &workspace,
        Arc::new(FakeExecutor::default()),
        &[],
        1024,
        &["private-payload-secret"],
    );
    let facts = tool.review_facts("./script.sh ../../outside.sh");
    let serialized = facts.to_string();
    assert!(!serialized.contains("private-payload-secret"));
    assert!(
        facts["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|file| file["unavailable"] == true)
    );
}

#[test]
fn review_facts_read_scripts_and_builds_from_every_chained_command() {
    let (_dir, workspace) = temp_workspace();
    std::fs::write(workspace.root().join("Makefile"), "all:\n").unwrap();
    std::fs::write(workspace.root().join("run"), "printf run\n").unwrap();
    let tool = bash(
        &workspace,
        Arc::new(FakeExecutor::default()),
        &[],
        1024,
        &[],
    );
    let facts = tool.review_facts("make&&./run");
    let paths: Vec<_> = facts["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|file| file["path"].as_str().unwrap().to_ascii_lowercase())
        .collect();
    assert_eq!(paths, ["./run", "makefile"]);
    assert!(
        facts["files"]
            .as_array()
            .unwrap()
            .iter()
            .all(|file| file["unavailable"] != true)
    );
    assert_eq!(facts["complex_shell"], false);
    assert_eq!(tool.review_facts("make && ./run $X")["complex_shell"], true);
}

#[test]
fn review_facts_apply_leading_cd_and_make_directory_and_refuse_later_cd() {
    let (_dir, workspace) = temp_workspace();
    std::fs::create_dir_all(workspace.root().join("sub")).unwrap();
    std::fs::write(workspace.root().join("sub/Makefile"), "all:\n").unwrap();
    let tool = bash(
        &workspace,
        Arc::new(FakeExecutor::default()),
        &[],
        1024,
        &[],
    );
    let expected = json!(workspace.root().join("sub"));
    for command in ["cd sub && make", "make -C sub", "make -Csub test"] {
        let facts = tool.review_facts(command);
        assert_eq!(facts["cwd"], expected, "{command}");
        assert_eq!(
            facts["files"][0]["path"]
                .as_str()
                .unwrap()
                .to_ascii_lowercase(),
            "makefile",
            "{command}"
        );
        assert!(facts["files"][0]["sha256"].is_string(), "{command}");
    }
    for command in [
        "ls && cd sub",
        "cd sub",
        "cd \"$D\" && make",
        "cd sub; cd .. && make",
    ] {
        assert_eq!(
            tool.review_facts(command)["operation_unavailable"],
            true,
            "{command}"
        );
    }
}

#[test]
fn review_facts_report_redirect_targets() {
    let (_dir, workspace) = temp_workspace();
    let tool = bash(
        &workspace,
        Arc::new(FakeExecutor::default()),
        &[],
        1024,
        &[],
    );
    let facts = tool.review_facts("echo a >./out.txt");
    assert_eq!(facts["targets"][0]["path"], "./out.txt");
    assert_eq!(facts["target_unavailable"], false);
    let facts = tool.review_facts("echo a > \"$F\"");
    assert_eq!(facts["targets"][0]["unavailable"], true);
    assert_eq!(facts["target_unavailable"], true);
}

#[test]
fn git_facts_resolve_literal_worktree_cwd_and_refuse_ambiguous_or_escaping_targets() {
    let (_dir, workspace) = temp_workspace();
    std::fs::create_dir_all(workspace.root().join(".worktree/feature")).unwrap();
    let tool = bash(
        &workspace,
        Arc::new(FakeExecutor::default()),
        &[],
        1024,
        &[],
    );
    let expected = workspace.root().join(".worktree/feature");
    assert_eq!(
        tool.git_review_target("git -C .worktree/feature push")
            .unwrap(),
        Some(expected.clone())
    );
    assert_eq!(
        tool.git_review_target("cd .worktree/feature && git push")
            .unwrap(),
        Some(expected)
    );
    assert_eq!(
        tool.git_review_target("git --no-pager -C .worktree/feature push origin \"$B\"")
            .unwrap(),
        Some(workspace.root().join(".worktree/feature"))
    );
    assert!(tool.git_review_target("git -C \"$D\" push").is_err());
    assert!(
        tool.git_review_target("GIT_DIR=/tmp/other git push")
            .is_err()
    );
    assert!(
        tool.git_review_target("git status && GIT_WORK_TREE=/tmp git push")
            .is_err()
    );
    assert!(tool.git_review_target("cd ../outside && git push").is_err());
    assert!(
        tool.git_review_target("cd \"$TARGET\" && git push")
            .is_err()
    );
    assert!(
        tool.git_review_target("git -c remote.origin.pushurl=https://other.example push")
            .is_err()
    );
}

#[tokio::test]
async fn linked_worktree_git_and_script_previews_bind_the_effective_branch_and_payload() {
    let (_dir, workspace) = temp_workspace();
    fn git(root: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "commit.gpgSign=false",
                "-c",
                "user.name=Otto Test",
                "-c",
                "user.email=otto@example.test",
            ])
            .args(args)
            .current_dir(root)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap())
            .env("HOME", root)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }
    git(workspace.root(), &["init", "-q", "--initial-branch=main"]);
    std::fs::write(workspace.root().join("script.sh"), "printf primary\n").unwrap();
    git(workspace.root(), &["add", "script.sh"]);
    git(workspace.root(), &["commit", "-qm", "seed"]);
    let base = git(workspace.root(), &["rev-parse", "main"]);
    git(
        workspace.root(),
        &["remote", "add", "origin", "https://example.test/repo.git"],
    );
    git(
        workspace.root(),
        &["update-ref", "refs/remotes/origin/main", base.trim()],
    );
    git(
        workspace.root(),
        &["worktree", "add", "-qb", "feature", ".worktree/feature"],
    );
    let target = workspace.root().join(".worktree/feature");
    std::fs::write(target.join("script.sh"), "printf feature-only\n").unwrap();
    git(&target, &["add", "script.sh"]);
    git(&target, &["commit", "-qm", "feature payload"]);
    let executor = Arc::new(
        crate::sandbox::Executor::new(
            Arc::new(crate::sandbox::direct::DirectDriver::new()),
            crate::sandbox::Policy {
                filesystem: crate::sandbox::FilesystemMode::Unconfined,
                network: crate::sandbox::NetworkMode::Allow,
            },
            workspace.root(),
        )
        .unwrap(),
    );
    let environment = strings(&[
        &format!("PATH={}", std::env::var("PATH").unwrap()),
        &format!("HOME={}", workspace.root().display()),
        "GIT_CONFIG_NOSYSTEM=1",
        "GIT_CONFIG_GLOBAL=/dev/null",
    ]);
    let tool = BashTool::new(
        &workspace,
        executor.clone(),
        "/bin/sh",
        environment,
        Duration::from_secs(3),
        1024,
        &[],
        Arc::new(crate::tool::bash_output::Logs::for_session("test-session")),
    )
    .unwrap()
    .with_approvals(
        "root",
        Arc::new(BashApprovals::new(executor.clone(), Vec::new())),
    );
    for command in [
        "cd .worktree/feature && git push -u origin feature",
        "git -C .worktree/feature push -u origin feature",
    ] {
        let args = raw(&serde_json::json!({"command":command,"sandbox_permissions":"require_escalated","justification":"Publish the authorized feature"}).to_string());
        let preview = tool
            .approval_spec_async(&args, &CancellationToken::new())
            .await
            .unwrap()
            .unwrap();
        let details: serde_json::Value = serde_json::from_str(&preview.details).unwrap();
        assert_eq!(
            details["native_facts"]["git"]["branch"]["body"],
            "feature\n"
        );
        assert!(
            details["native_facts"]["git"]["outgoing_diff"]["body"]
                .as_str()
                .unwrap()
                .contains("feature-only")
        );
        assert_eq!(
            details["native_facts"]["git"]["chosen_base"]["reference"],
            "refs/remotes/origin/main"
        );
        assert_eq!(
            details["native_facts"]["git"]["chosen_base"]["merge_base"],
            base.trim()
        );
    }
    let args = raw(
        r#"{"command":"cd .worktree/feature && ./script.sh","sandbox_permissions":"require_escalated","justification":"Run the feature script"}"#,
    );
    let before = tool.approval_spec(&args).unwrap().unwrap();
    assert!(before.details.contains("feature-only"));
    assert!(!before.details.contains("printf primary"));
    std::fs::write(target.join("script.sh"), "printf changed-again\n").unwrap();
    assert_ne!(
        before.version,
        tool.approval_spec(&args).unwrap().unwrap().version
    );
    executor.close().unwrap();
}
