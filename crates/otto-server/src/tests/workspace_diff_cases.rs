//! Workspace diff route tests.

use super::*;

// ---- workspace diff ----

/// A `CommandExecutor` double that records every request and answers
/// each call with the next canned `(exit code, stdout)` pair, or
/// `(0, "")` once the list runs out.
#[derive(Default)]
pub(super) struct RecordingExecutor {
    requests: Mutex<Vec<otto::sandbox::Request>>,
    outputs: Mutex<Vec<(i32, String)>>,
}

impl RecordingExecutor {
    fn with(outputs: Vec<(i32, String)>) -> Arc<Self> {
        Arc::new(Self {
            requests: Mutex::new(Vec::new()),
            outputs: Mutex::new(outputs),
        })
    }
}

#[async_trait::async_trait]
impl otto::sandbox::CommandExecutor for RecordingExecutor {
    async fn execute(
        &self,
        request: otto::sandbox::Request,
        streams: otto::sandbox::Streams<'_>,
        _cancel: &CancellationToken,
    ) -> (otto::sandbox::ExitStatus, Result<(), otto::sandbox::Error>) {
        self.requests
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push(request);
        let (code, text) = {
            let mut outputs = self
                .outputs
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            if outputs.is_empty() {
                (0, String::new())
            } else {
                outputs.remove(0)
            }
        };
        let _ = streams.stdout.write_all(text.as_bytes());
        (
            otto::sandbox::ExitStatus {
                code,
                ..otto::sandbox::ExitStatus::default()
            },
            Ok(()),
        )
    }
}

pub(super) fn diff_options(
    executor: &Arc<RecordingExecutor>,
    environment: Vec<String>,
) -> HarnessOptions {
    HarnessOptions {
        diff_runner: Some((
            Arc::clone(executor) as Arc<dyn otto::sandbox::CommandExecutor>,
            environment,
        )),
        ..HarnessOptions::default()
    }
}

#[tokio::test]
async fn the_diff_route_runs_git_through_the_workspace_executor() {
    let environment = vec!["PATH=/usr/bin".to_string()];
    let executor = RecordingExecutor::with(vec![
        (0, "true\n".to_string()),
        (0, "main\n".to_string()),
        (0, String::new()),
        (0, String::new()),
    ]);
    let harness = Harness::with(diff_options(&executor, environment.clone()));
    let reply = harness.send("GET", "/v1/workspaces/diff", None).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(reply.json()["repository"], true);
    assert_eq!(reply.json()["branch"], "main");
    assert_eq!(reply.json()["files"], serde_json::json!([]));
    assert_eq!(reply.json()["truncated"], false);

    let requests = executor.requests.lock().unwrap();
    assert_eq!(requests.len(), 4);
    assert_eq!(
        requests[0].argv,
        vec![
            "git",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.quotepath=off",
            "rev-parse",
            "--is-inside-work-tree",
        ]
    );
    assert_eq!(
        requests[2].argv,
        vec![
            "git",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.quotepath=off",
            "diff",
            "HEAD",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--relative",
            "-M",
            "--",
            ".",
        ]
    );
    assert_eq!(
        requests[3].argv,
        vec![
            "git",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.quotepath=off",
            "ls-files",
            "--others",
            "--exclude-standard",
            "-z",
            "--",
            ".",
        ]
    );
    assert_eq!(
        requests[0].dir,
        PathBuf::from(&harness.factory.workspaces.startup)
    );
    let mut expected_env = environment.clone();
    expected_env.push("GIT_OPTIONAL_LOCKS=0".to_string());
    assert_eq!(requests[0].env, expected_env);
}

#[tokio::test]
async fn a_directory_outside_any_work_tree_answers_repository_false() {
    let executor = RecordingExecutor::with(vec![(1, String::new())]);
    let harness = Harness::with(diff_options(&executor, Vec::new()));
    let reply = harness.send("GET", "/v1/workspaces/diff", None).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(reply.json()["repository"], false);
    assert_eq!(reply.json()["branch"], Value::Null);
    assert_eq!(reply.json()["files"], serde_json::json!([]));
    assert_eq!(executor.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn before_the_first_commit_diffs_against_the_empty_tree() {
    let executor = RecordingExecutor::with(vec![
        (0, "true\n".to_string()),
        (1, String::new()),
        (0, String::new()),
        (0, String::new()),
    ]);
    let harness = Harness::with(diff_options(&executor, Vec::new()));
    let reply = harness.send("GET", "/v1/workspaces/diff", None).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(reply.json()["repository"], true);
    assert_eq!(reply.json()["branch"], Value::Null);

    let requests = executor.requests.lock().unwrap();
    assert_eq!(
        requests[2].argv[5..7],
        [
            "diff".to_string(),
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904".to_string()
        ]
    );
}

#[tokio::test]
async fn an_untracked_file_is_reported_and_its_diff_no_index_exit_1_is_accepted() {
    let patch = "diff --git a/dev/null b/new.txt\nnew file mode 100644\nindex 0000000..1111111\n--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1 @@\n+hello\n";
    let executor = RecordingExecutor::with(vec![
        (0, "true\n".to_string()),
        (0, "main\n".to_string()),
        (0, String::new()),
        (0, "new.txt\0".to_string()),
        (1, patch.to_string()),
    ]);
    let harness = Harness::with(diff_options(&executor, Vec::new()));
    let reply = harness.send("GET", "/v1/workspaces/diff", None).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let files = reply.json()["files"].as_array().expect("files").clone();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["path"], "new.txt");
    assert_eq!(files[0]["status"], "untracked");
    assert!(
        files[0]["patch"]
            .as_str()
            .expect("patch")
            .contains("+hello")
    );

    let requests = executor.requests.lock().unwrap();
    assert_eq!(requests.len(), 5);
    assert_eq!(
        requests[4].argv,
        vec![
            "git",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.quotepath=off",
            "diff",
            "--no-index",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--",
            "/dev/null",
            "new.txt",
        ]
    );
}

#[tokio::test]
async fn no_executor_answers_501_diff_unavailable() {
    let harness = Harness::new();
    let reply = harness.send("GET", "/v1/workspaces/diff", None).await;
    assert_eq!(reply.status, StatusCode::NOT_IMPLEMENTED, "{}", reply.body);
    assert_eq!(reply.json()["error"]["code"], "diff_unavailable");
}

#[tokio::test]
async fn the_diff_route_requires_a_token_like_every_v1_route() {
    let executor = RecordingExecutor::with(vec![(1, String::new())]);
    let mut options = diff_options(&executor, Vec::new());
    options.token = "secret".to_string();
    let harness = Harness::with(options);
    let reply = harness.send("GET", "/v1/workspaces/diff", None).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{}", reply.body);
}

#[tokio::test]
async fn an_unadmitted_workspace_query_answers_403() {
    let executor = RecordingExecutor::with(vec![(1, String::new())]);
    let mut options = diff_options(&executor, Vec::new());
    options.workspaces = FakeWorkspaces::default();
    let harness = Harness::with(options);
    let reply = harness
        .send("GET", "/v1/workspaces/diff?workspace=/elsewhere", None)
        .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN, "{}", reply.body);
    assert_eq!(reply.json()["error"]["code"], "WORKSPACE_NOT_ADMITTED");
}

#[tokio::test]
async fn a_failing_git_diff_answers_500_git_failed() {
    let executor = RecordingExecutor::with(vec![
        (0, "true\n".to_string()),
        (0, "main\n".to_string()),
        (128, String::new()),
    ]);
    let harness = Harness::with(diff_options(&executor, Vec::new()));
    let reply = harness.send("GET", "/v1/workspaces/diff", None).await;
    assert_eq!(
        reply.status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "{}",
        reply.body
    );
    assert_eq!(reply.json()["error"]["code"], "git_failed");
    assert!(
        reply.json()["error"]["message"]
            .as_str()
            .expect("message")
            .contains("128")
    );
}

#[tokio::test]
async fn a_real_git_repository_reports_modified_added_and_untracked_files() {
    if std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipping: git not on PATH");
        return;
    }
    // The repository lives in the harness's own startup workspace, so
    // the route's default (no `?workspace=`) resolves straight to it.
    let repo = tempfile::tempdir().expect("temp dir");
    // Canonicalized to match what `Harness::with` derives as the
    // workspace path (see its comment on `canonical_workspace`).
    let root = std::fs::canonicalize(repo.path()).expect("canonicalize");
    let run = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(&root)
            .status()
            .expect("git runs");
        assert!(status.success(), "git {args:?}");
    };
    std::fs::write(root.join("tracked.txt"), "one\n").expect("write");
    run(&["init", "-q"]);
    run(&["config", "user.email", "test@example.com"]);
    run(&["config", "user.name", "test"]);
    run(&["add", "tracked.txt"]);
    run(&["commit", "-q", "-m", "initial"]);
    std::fs::write(root.join("tracked.txt"), "one\ntwo\n").expect("write");
    std::fs::write(root.join("added.txt"), "added\n").expect("write");
    run(&["add", "added.txt"]);
    std::fs::write(root.join("untracked.txt"), "untracked\n").expect("write");

    let executor: Arc<dyn otto::sandbox::CommandExecutor> = Arc::new(
        otto::sandbox::Executor::new(
            Arc::new(otto::sandbox::direct::DirectDriver::new()),
            otto::sandbox::Policy {
                filesystem: otto::sandbox::FilesystemMode::Unconfined,
                network: otto::sandbox::NetworkMode::Allow,
            },
            &root,
        )
        .expect("the executor opens"),
    );
    // The direct driver runs with only the given environment (no
    // inherited `PATH`), so `git` must be resolvable from it.
    let path = std::env::var("PATH").unwrap_or_default();
    let harness = Harness::with(HarnessOptions {
        workspace: Some(repo),
        diff_runner: Some((executor, vec![format!("PATH={path}")])),
        ..HarnessOptions::default()
    });
    let reply = harness.send("GET", "/v1/workspaces/diff", None).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(reply.json()["repository"], true);
    let files = reply.json()["files"].as_array().expect("files").clone();
    let by_path = |path: &str| files.iter().find(|file| file["path"] == path);
    assert_eq!(
        by_path("tracked.txt").expect("tracked.txt")["status"],
        "modified"
    );
    assert_eq!(by_path("added.txt").expect("added.txt")["status"], "added");
    assert_eq!(
        by_path("untracked.txt").expect("untracked.txt")["status"],
        "untracked"
    );
}
