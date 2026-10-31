//!
//! Cases the Rust contracts make unreachable, and what stands in for them:
//!
//! 1. Caller mutation of a retained request: the constructor takes an owned
//!    `Vec<String>` and a `&str` it copies, and the executor receives an
//!    owned [`Request`], so no caller or callee holds storage the tool
//!    still reads. The delegation check keeps the exact-request half.
//! 2. Per-call writer independence is checked behaviourally, by the
//!    concurrent-calls test, rather than by comparing writer identities.
//! 3. The timeout is a real `tokio::time::sleep` driven by a paused test
//!    clock, so it fires deterministically with no production seam.
//! 4. Writes into [`RedactingCollector`] cannot fail, so a capture failure
//!    has no outcome to test.
//! 5. Allocation counting on the suppressed path has no stable equivalent;
//!    the test keeps the observable half, that the command still runs
//!    exactly once and the result stays empty.
//! 6. Raw, wrapped and joined executor errors are unrepresentable:
//!    [`Error`] is a closed enum whose variants carry no text.
//! 7. Syntactically invalid argument payloads (`{"command":"true"}X`, an
//!    unterminated object) cannot reach a tool: [`RawValue`] is validated
//!    at the trust boundary, so only well-formed JSON with the wrong shape
//!    is representable.

use super::*;
use crate::sandbox::{Executor, FilesystemMode, NetworkMode, Policy, UnavailableReason};
use crate::tool::testutil::raw;
use std::sync::Mutex;

mod approval;
mod execution;
mod facts;
mod output;
mod redaction;

#[test]
fn parse_approval_request_decodes_the_formatted_result() {
    let content = "approval required for unsandboxed bash execution.\n\
Approve in Otto: /approve approval-7\n\
Command: \"git push \\\"x\\\"\"\n\
Justification: \"push branch\"\n\
The command did not run.";
    let request = parse_approval_request("bash", &ToolResult::error(content)).expect("request");
    assert_eq!(
        request,
        ApprovalRequest {
            id: "approval-7".to_string(),
            command: "git push \"x\"".to_string(),
            justification: "push branch".to_string(),
            read_path: String::new(),
        }
    );
    assert_eq!(
        parse_approval_request("read", &ToolResult::error(content)),
        None
    );
    let ok = ToolResult {
        content: content.to_string(),
        ..Default::default()
    };
    assert_eq!(parse_approval_request("bash", &ok), None);
    assert_eq!(
        parse_approval_request("bash", &ToolResult::error("Approve in Otto: /approve a b")),
        None
    );
}

type BehaviorFuture<'a> = std::pin::Pin<
    Box<dyn std::future::Future<Output = (ExitStatus, Result<(), Error>)> + Send + 'a>,
>;
type Behavior = Box<
    dyn for<'a> Fn(Request, Streams<'a>, CancellationToken) -> BehaviorFuture<'a> + Send + Sync,
>;

#[derive(Default)]
struct FakeExecutor {
    recorded: Mutex<Vec<Request>>,
    stdout_chunks: Vec<Vec<u8>>,
    stderr_chunks: Vec<Vec<u8>>,
    status: ExitStatus,
    error: Option<Error>,
    behavior: Option<Behavior>,
}

impl FakeExecutor {
    fn chunks(stdout: &[&str], stderr: &[&str]) -> Self {
        Self {
            stdout_chunks: stdout
                .iter()
                .map(|chunk| chunk.as_bytes().to_vec())
                .collect(),
            stderr_chunks: stderr
                .iter()
                .map(|chunk| chunk.as_bytes().to_vec())
                .collect(),
            ..Self::default()
        }
    }

    fn with_status(mut self, status: ExitStatus) -> Self {
        self.status = status;
        self
    }

    fn behaving(behavior: Behavior) -> Self {
        Self {
            behavior: Some(behavior),
            ..Self::default()
        }
    }

    fn calls(&self) -> usize {
        self.recorded.lock().unwrap().len()
    }

    fn requests(&self) -> Vec<Request> {
        self.recorded.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl CommandExecutor for FakeExecutor {
    async fn execute(
        &self,
        request: Request,
        streams: Streams<'_>,
        cancel: &CancellationToken,
    ) -> (ExitStatus, Result<(), Error>) {
        self.recorded.lock().unwrap().push(request.clone());
        if let Some(behavior) = &self.behavior {
            return behavior(request, streams, cancel.clone()).await;
        }
        for chunk in &self.stdout_chunks {
            streams
                .stdout
                .write_all(chunk)
                .expect("the sink never fails");
        }
        for chunk in &self.stderr_chunks {
            streams
                .stderr
                .write_all(chunk)
                .expect("the sink never fails");
        }
        (self.status.clone(), self.error.clone().map_or(Ok(()), Err))
    }
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

fn bash(
    workspace: &Workspace,
    executor: Arc<FakeExecutor>,
    environment: &[&str],
    max_output_bytes: usize,
    redactions: &[&str],
) -> BashTool {
    BashTool::new(
        workspace,
        executor,
        "/bin/sh",
        strings(environment),
        Duration::from_secs(3),
        max_output_bytes,
        &strings(redactions),
        Arc::new(crate::tool::bash_output::Logs::for_session("test-session")),
    )
    .expect("the fixture configuration is valid")
}

fn outcome(result: &ToolResult) -> &otto_core::model::OperationOutcome {
    result
        .outcome_override
        .as_ref()
        .expect("the concrete tool classified this boundary")
}

async fn run(tool: &BashTool, command: &str) -> ToolResult {
    let arguments = serde_json::to_string(&serde_json::json!({ "command": command }))
        .expect("the arguments encode");
    tool.execute(&raw(&arguments), &CancellationToken::new())
        .await
}

async fn run_escalated(tool: &BashTool, command: &str, justification: &str) -> ToolResult {
    let arguments = serde_json::to_string(&serde_json::json!({
        "command": command,
        "sandbox_permissions": "require_escalated",
        "justification": justification,
    }))
    .expect("the arguments encode");
    tool.execute(&raw(&arguments), &CancellationToken::new())
        .await
}

/// The captured stdout body.
fn stdout_body(content: &str) -> String {
    let rest = content
        .strip_prefix("stdout:\n")
        .expect("the result starts with the stdout section");
    let (stdout, _) = rest
        .split_once("\nstderr:\n")
        .expect("the result carries a stderr delimiter");
    if let Some((captured, _)) = stdout.split_once("\n[truncated:") {
        return captured.to_owned();
    }
    if stdout.starts_with("[truncated:") {
        return String::new();
    }
    stdout.to_owned()
}

fn temp_workspace() -> (tempfile::TempDir, std::sync::Arc<Workspace>) {
    let dir = tempfile::tempdir().expect("a temporary directory is creatable");
    let workspace = std::sync::Arc::new(Workspace::new(dir.path()).expect("the workspace opens"));
    (dir, workspace)
}
