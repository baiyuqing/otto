//! The `bash` tool.
//!
//! Runs one shell command through a [`CommandExecutor`], starting in the
//! workspace root. The tool never reports a `Result`: every failure is an
//! in-band error result, and every infrastructure failure collapses to the
//! fixed text `sandbox execution unavailable` so a driver diagnostic can never
//! reach the model.
//!
//! Ownership: the constructor copies the workspace root, shell, environment and
//! redaction set, so a later mutation by the caller cannot change what is
//! executed. Concurrency: [`Tool::execute`] takes `&self` and builds fresh
//! collectors per call, so concurrent calls share no output or redaction state.
//! Cancellation: an already-cancelled token reports `status: cancelled` without
//! starting a child; a token cancelled during the call kills the child's
//! process group and still reports the partial output and the signal. The
//! configured timeout cancels a private child token and reports `status: timed
//! out after <duration>`; a parent cancellation observed at the same time wins.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use otto_core::model::ToolDefinition;
use otto_core::tool::ToolResult;
use serde::Deserialize;
use serde_json::json;
use serde_json::value::RawValue;
use tokio_util::sync::CancellationToken;

use crate::sandbox::{CommandExecutor, Error, ExitStatus, Request, Streams};
use otto_core::safetext::{dynamic_redaction_marker, secret_forms};

use super::result::{
    CappedByteCollector, RedactingCollector, decode_strict_json, redact_exact_text,
};
use super::workspace::Workspace;
use super::{Tool, definition};

/// The text every infrastructure failure collapses to.
const SANDBOX_EXECUTION_UNAVAILABLE: &str = "sandbox execution unavailable";

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct BashArgs {
    #[serde(default)]
    command: String,
    #[serde(default)]
    sandbox_permissions: SandboxPermissions,
    #[serde(default)]
    justification: String,
}

#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum SandboxPermissions {
    #[default]
    UseDefault,
    RequireEscalated,
}

const APPROVAL_LIFETIME: Duration = Duration::from_secs(5 * 60);

struct Approval {
    id: String,
    command: String,
    created_at: Instant,
    granted: bool,
}

#[derive(Default)]
struct ApprovalState {
    next_id: u64,
    requests: HashMap<String, Approval>,
}

/// Process-local, one-shot permission grants for exact Bash commands.
///
/// One mutex serializes requests across sessions. A session keeps only its
/// newest request; approval expires after five minutes and is removed before
/// the matching command starts, so failure or cancellation cannot reuse it.
pub struct BashApprovals {
    executor: Arc<dyn CommandExecutor>,
    environment: Vec<String>,
    lifetime: Duration,
    state: Mutex<ApprovalState>,
}

impl BashApprovals {
    pub fn new(executor: Arc<dyn CommandExecutor>, environment: Vec<String>) -> Self {
        Self::with_lifetime(executor, environment, APPROVAL_LIFETIME)
    }

    fn with_lifetime(
        executor: Arc<dyn CommandExecutor>,
        environment: Vec<String>,
        lifetime: Duration,
    ) -> Self {
        Self {
            executor,
            environment,
            lifetime,
            state: Mutex::new(ApprovalState::default()),
        }
    }

    fn retain_fresh(&self, state: &mut ApprovalState) {
        state
            .requests
            .retain(|_, request| request.created_at.elapsed() < self.lifetime);
    }

    fn request(&self, session_id: &str, command: &str) -> String {
        let mut state = self.state.lock().expect("bash approval mutex");
        self.retain_fresh(&mut state);
        if let Some(request) = state
            .requests
            .get(session_id)
            .filter(|request| request.command == command)
        {
            return request.id.clone();
        }
        state.next_id += 1;
        let id = format!("approval-{}", state.next_id);
        state.requests.insert(
            session_id.to_owned(),
            Approval {
                id: id.clone(),
                command: command.to_owned(),
                created_at: Instant::now(),
                granted: false,
            },
        );
        id
    }

    /// Grants one pending command for `session_id`.
    pub fn approve(&self, session_id: &str, id: &str) -> Result<(), &'static str> {
        let mut state = self.state.lock().expect("bash approval mutex");
        self.retain_fresh(&mut state);
        let request = state
            .requests
            .get_mut(session_id)
            .filter(|request| request.id == id)
            .ok_or("approval request not found or expired")?;
        request.granted = true;
        Ok(())
    }

    /// Unexpired pending approvals for `session_id`: 0 or 1, since a session
    /// keeps only its newest request. Applies the same expiry filter as
    /// [`BashApprovals::approve`].
    pub fn pending_count(&self, session_id: &str) -> usize {
        let mut state = self.state.lock().expect("bash approval mutex");
        self.retain_fresh(&mut state);
        usize::from(state.requests.contains_key(session_id))
    }

    fn take(&self, session_id: &str, command: &str) -> bool {
        let mut state = self.state.lock().expect("bash approval mutex");
        self.retain_fresh(&mut state);
        let granted = state
            .requests
            .get(session_id)
            .is_some_and(|request| request.granted && request.command == command);
        if !granted {
            return false;
        }
        state.requests.remove(session_id);
        true
    }
}

/// The single error [`BashTool::new`] reports.
///
/// Every rejected boundary produces this one value, so the text can never
/// describe which host path or setting was wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("invalid sandboxed bash configuration")]
pub struct InvalidConfiguration;

/// Executes a shell command in the sandbox.
pub struct BashTool {
    workspace_root: PathBuf,
    executor: Arc<dyn CommandExecutor>,
    shell: String,
    environment: Vec<String>,
    timeout: Duration,
    max_output_bytes: usize,
    redact_values: Vec<String>,
    redaction_marker: String,
    dynamic_content: bool,
    approvals: Option<(String, Arc<BashApprovals>)>,
}

/// How the shell is invoked.
///
/// macOS gets `-l`, a login shell, because that is how a command sees the
/// `PATH` `path_helper` assembles from `/etc/paths` and `/etc/paths.d`;
/// without it a Homebrew or Xcode tool the user installed is simply not
/// found. `/etc/profile` there is Apple's and quiet.
///
/// Everywhere else the login profile is skipped. `/etc/profile.d/*` is
/// arbitrary vendor code: it writes to stdout, which lands in the middle of
/// the tool result the model reads, and it can restore the very variables
/// the environment filter in `sandbox::environment` removed. On a platform
/// with no confined driver nothing would contain it either. The command
/// still gets the `PATH` Otto passes it.
#[cfg(target_os = "macos")]
const SHELL_FLAGS: &str = "-lc";
/// See the macOS flags above.
#[cfg(not(target_os = "macos"))]
const SHELL_FLAGS: &str = "-c";

impl BashTool {
    /// Binds the tool to one executor.
    ///
    /// `environment` replaces the host environment exactly; an empty vector is
    /// meaningful and is passed through. `redaction_values` are the secrets to
    /// hide from every model-visible byte; each is canonicalized into its
    /// literal and JSON-decoded forms. When no collision-safe marker exists for
    /// that set the tool suppresses all result text rather than approximating
    /// the redaction.
    ///
    /// Errors: [`InvalidConfiguration`] when the workspace root no longer
    /// resolves to itself as a directory, the shell is blank or contains NUL,
    /// the timeout is zero, or the output cap is zero.
    pub fn new(
        workspace: &Workspace,
        executor: Arc<dyn CommandExecutor>,
        shell: &str,
        environment: Vec<String>,
        timeout: Duration,
        max_output_bytes: usize,
        redaction_values: &[String],
    ) -> Result<Self, InvalidConfiguration> {
        if !valid_workspace(workspace)
            || shell.trim().is_empty()
            || shell.contains('\0')
            || timeout.is_zero()
            || max_output_bytes == 0
        {
            return Err(InvalidConfiguration);
        }

        let mut redact_values = canonical_redactions(redaction_values);
        let mut redaction_marker = dynamic_redaction_marker(&redact_values).unwrap_or_default();
        let dynamic_content = !redaction_marker.is_empty();
        if !dynamic_content {
            redact_values = Vec::new();
            redaction_marker = String::new();
        }

        Ok(Self {
            workspace_root: workspace.root().to_path_buf(),
            executor,
            shell: shell.to_owned(),
            environment,
            timeout,
            max_output_bytes,
            redact_values,
            redaction_marker,
            dynamic_content,
            approvals: None,
        })
    }

    /// Enables explicit, one-shot elevation for this session.
    pub fn with_approvals(
        mut self,
        session_id: impl Into<String>,
        approvals: Arc<BashApprovals>,
    ) -> Self {
        self.approvals = Some((session_id.into(), approvals));
        self
    }

    fn request(&self, command: &str, environment: &[String]) -> Request {
        Request {
            argv: vec![
                self.shell.clone(),
                SHELL_FLAGS.to_owned(),
                command.to_owned(),
            ],
            dir: self.workspace_root.clone(),
            env: environment.to_vec(),
        }
    }

    /// Runs `request`, cancelling a private child token once the timeout
    /// elapses. Returns whether the timeout fired alongside the outcome.
    async fn run_with(
        &self,
        executor: &Arc<dyn CommandExecutor>,
        environment: &[String],
        command: &str,
        stdout: &mut (dyn std::io::Write + Send),
        stderr: &mut (dyn std::io::Write + Send),
        cancel: &CancellationToken,
    ) -> (ExitStatus, Result<(), Error>, bool) {
        let child = cancel.child_token();
        let streams = Streams { stdout, stderr };
        let execute = executor.execute(self.request(command, environment), streams, &child);
        let mut execute = std::pin::pin!(execute);
        let sleep = tokio::time::sleep(self.timeout);
        let mut sleep = std::pin::pin!(sleep);
        let mut timed_out = false;
        let (status, outcome) = loop {
            tokio::select! {
                outcome = &mut execute => break outcome,
                () = &mut sleep, if !timed_out => {
                    timed_out = true;
                    child.cancel();
                }
            }
        };
        (status, outcome, timed_out)
    }

    /// The path taken when no collision-safe marker exists: the command still
    /// runs, but nothing it produced may be described.
    async fn execute_suppressed(&self, command: &str, cancel: &CancellationToken) -> ToolResult {
        if cancel.is_cancelled() {
            return ToolResult::default();
        }
        let mut stdout = std::io::sink();
        let mut stderr = std::io::sink();
        let (_, outcome, _) = self
            .run_with(
                &self.executor,
                &self.environment,
                command,
                &mut stdout,
                &mut stderr,
                cancel,
            )
            .await;
        match outcome {
            Ok(()) | Err(Error::Cancelled) => ToolResult::default(),
            Err(_) => ToolResult {
                is_error: true,
                ..ToolResult::default()
            },
        }
    }

    fn argument_error(&self, message: &str) -> ToolResult {
        if !self.dynamic_content {
            return ToolResult {
                is_error: true,
                ..ToolResult::default()
            };
        }
        ToolResult {
            content: redact_exact_text(message, &self.redact_values, &self.redaction_marker),
            is_error: true,
            ..ToolResult::default()
        }
    }

    fn result(
        &self,
        stdout: &CappedByteCollector,
        stderr: &CappedByteCollector,
        status: &ExitStatus,
        summary: &str,
    ) -> ToolResult {
        let mut summary = summary.to_owned();
        if status.signaled && !status.signal.is_empty() {
            summary.push_str("; signal: ");
            summary.push_str(&status.signal);
        }
        let formatted = format!(
            "{}\n{}\n{summary}",
            format_stream("stdout", stdout),
            format_stream("stderr", stderr)
        );
        ToolResult {
            content: redact_exact_text(&formatted, &self.redact_values, &self.redaction_marker),
            ..ToolResult::default()
        }
    }
}

fn infrastructure_result() -> ToolResult {
    ToolResult {
        content: SANDBOX_EXECUTION_UNAVAILABLE.to_owned(),
        is_error: true,
        ..ToolResult::default()
    }
}

/// The schema advertised for `bash`.
pub fn bash_definition() -> ToolDefinition {
    definition(
        "bash",
        "Execute a shell command from the workspace",
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Shell command to execute"
                }
            },
            "required": ["command"]
        }),
    )
}

pub fn bash_definition_with_approvals() -> ToolDefinition {
    definition(
        "bash",
        "Execute a shell command from the workspace. Set sandbox_permissions to require_escalated only when sandboxed execution cannot complete the task; include a justification. Only the user can grant it, by typing /approve in Otto, and the command does not run until they do.",
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Shell command to execute"
                },
                "sandbox_permissions": {
                    "type": "string",
                    "enum": ["use_default", "require_escalated"],
                    "description": "Use require_escalated to request one-time unsandboxed execution; the user must approve it before the command runs"
                },
                "justification": {
                    "type": "string",
                    "description": "Why unsandboxed execution is required"
                }
            },
            "required": ["command"]
        }),
    )
}

#[async_trait::async_trait]
impl Tool for BashTool {
    fn definition(&self) -> ToolDefinition {
        if self.approvals.is_some() {
            bash_definition_with_approvals()
        } else {
            bash_definition()
        }
    }

    async fn execute(&self, arguments: &RawValue, cancel: &CancellationToken) -> ToolResult {
        let args: BashArgs = match decode_strict_json(arguments.get(), &["command"]) {
            Ok(args) => args,
            Err(message) => return self.argument_error(&message),
        };
        if args.command.trim().is_empty() {
            return self.argument_error("missing required argument: command");
        }
        if !self.dynamic_content {
            return self.execute_suppressed(&args.command, cancel).await;
        }
        let (executor, environment) = match args.sandbox_permissions {
            SandboxPermissions::UseDefault => (&self.executor, self.environment.as_slice()),
            SandboxPermissions::RequireEscalated => {
                if args.justification.trim().is_empty() {
                    return self.argument_error("justification is required for elevated execution");
                }
                let Some((session_id, approvals)) = &self.approvals else {
                    return self.argument_error("elevated execution is unavailable");
                };
                if !approvals.take(session_id, &args.command) {
                    let id = approvals.request(session_id, &args.command);
                    let command = serde_json::to_string(&args.command).expect("string encodes");
                    let justification =
                        serde_json::to_string(&args.justification).expect("string encodes");
                    return self.argument_error(&format!(
                        "unsandboxed execution was not approved; the command did not run. \
Only the user can approve it, by typing /approve {id} in Otto. That is not a shell \
command, so do not try to run it yourself. Ask the user for approval and then reissue \
this exact command; if you continue without it, tell the user what you are doing \
instead. command={command}; justification={justification}"
                    ));
                }
                (&approvals.executor, approvals.environment.as_slice())
            }
        };
        if cancel.is_cancelled() {
            return self.result(
                &CappedByteCollector::new(self.max_output_bytes),
                &CappedByteCollector::new(self.max_output_bytes),
                &ExitStatus::default(),
                "status: cancelled",
            );
        }

        let mut stdout = RedactingCollector::new(
            self.max_output_bytes,
            &self.redact_values,
            &self.redaction_marker,
        );
        let mut stderr = RedactingCollector::new(
            self.max_output_bytes,
            &self.redact_values,
            &self.redaction_marker,
        );
        let (status, outcome, timed_out) = self
            .run_with(
                executor,
                environment,
                &args.command,
                &mut stdout,
                &mut stderr,
                cancel,
            )
            .await;
        if matches!(outcome, Err(ref error) if *error != Error::Cancelled) {
            return infrastructure_result();
        }
        stdout.flush();
        stderr.flush();

        let summary = if cancel.is_cancelled() {
            "status: cancelled".to_owned()
        } else if timed_out {
            format!("status: timed out after {}", go_duration(self.timeout))
        } else if outcome.is_err() {
            "status: cancelled".to_owned()
        } else {
            format!("exit_code: {}", status.code)
        };
        self.result(stdout.collector(), stderr.collector(), &status, &summary)
    }
}

/// Renders one stream the way `formatBashStream` does: the name, the captured
/// bytes, and a truthful omission count when the cap discarded anything.
fn format_stream(name: &str, collector: &CappedByteCollector) -> String {
    let data = collector.bytes();
    let mut rendered = format!("{name}:\n");
    rendered.push_str(&String::from_utf8_lossy(data));
    if collector.discarded() > 0 {
        if data.last().is_some_and(|byte| *byte != b'\n') {
            rendered.push('\n');
        }
        rendered.push_str(&format!(
            "[truncated: {} bytes omitted]",
            collector.discarded()
        ));
    }
    rendered
}

/// Revalidates the workspace root at construction time.
///
/// The type is already validated, so this only catches a root that was removed
/// or replaced between opening the workspace and building the tool.
fn valid_workspace(workspace: &Workspace) -> bool {
    let root = workspace.root();
    root.is_absolute()
        && resolves_to(root, root)
        && root.metadata().is_ok_and(|metadata| metadata.is_dir())
        && resolves_to(workspace.lexical_root(), root)
}

fn resolves_to(path: &Path, expected: &Path) -> bool {
    std::fs::canonicalize(path).is_ok_and(|resolved| resolved == expected)
}

/// Expands each configured secret into its canonical forms, preserving order
/// and dropping duplicates.
fn canonical_redactions(values: &[String]) -> Vec<String> {
    let mut canonical: Vec<String> = Vec::with_capacity(values.len());
    for value in values {
        for form in secret_forms(value.as_bytes()) {
            if !canonical.contains(&form) {
                canonical.push(form);
            }
        }
    }
    canonical
}

/// Formats a duration the way Go's `time.Duration.String` does, because the
/// timeout text is model-visible and the tests pin it.
fn go_duration(duration: Duration) -> String {
    let nanos = duration.as_nanos();
    if nanos == 0 {
        return "0s".to_owned();
    }
    if nanos < 1_000 {
        return format!("{nanos}ns");
    }
    if nanos < 1_000_000 {
        return format!("{}{}\u{b5}s", nanos / 1_000, fraction(nanos, 1_000));
    }
    if nanos < 1_000_000_000 {
        return format!("{}{}ms", nanos / 1_000_000, fraction(nanos, 1_000_000));
    }
    let seconds = nanos / 1_000_000_000;
    let mut rendered = format!("{}{}s", seconds % 60, fraction(nanos, 1_000_000_000));
    if seconds >= 60 {
        rendered.insert_str(0, &format!("{}m", (seconds / 60) % 60));
    }
    if seconds >= 3_600 {
        rendered.insert_str(0, &format!("{}h", seconds / 3_600));
    }
    rendered
}

/// The fractional part of `value / scale`, with trailing zeros removed, or an
/// empty string when the division is exact.
fn fraction(value: u128, scale: u128) -> String {
    let remainder = value % scale;
    if remainder == 0 {
        return String::new();
    }
    let width = scale.ilog10() as usize;
    let mut digits = format!("{remainder:0width$}");
    while digits.ends_with('0') {
        digits.pop();
    }
    format!(".{digits}")
}

#[cfg(test)]
mod tests {
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
        )
        .expect("the fixture configuration is valid")
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
            "unsandboxed execution was not approved; the command did not run. Only the user can approve it, by typing /approve approval-1 in Otto. That is not a shell command, so do not try to run it yourself. Ask the user for approval and then reissue this exact command; if you continue without it, tell the user what you are doing instead. command=\"git push\"; justification=\"push the reviewed branch\""
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
            "unsandboxed execution was not approved; the command did not run. Only the user can approve it, by typing /approve approval-3 in Otto. That is not a shell command, so do not try to run it yourself. Ask the user for approval and then reissue this exact command; if you continue without it, tell the user what you are doing instead. command=\"git push\"; justification=\"push the reviewed branch\""
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

    #[test]
    fn expired_approval_requests_cannot_be_granted() {
        let approvals = BashApprovals::with_lifetime(
            Arc::new(FakeExecutor::default()),
            Vec::new(),
            Duration::ZERO,
        );
        let id = approvals.request("session-1", "git push");
        assert_eq!(
            approvals.approve("session-1", &id),
            Err("approval request not found or expired")
        );
    }

    #[test]
    fn pending_count_counts_unexpired_entries_only() {
        let approvals = BashApprovals::with_lifetime(
            Arc::new(FakeExecutor::default()),
            Vec::new(),
            Duration::from_secs(60),
        );
        assert_eq!(approvals.pending_count("session-1"), 0);
        approvals.request("session-1", "git push");
        assert_eq!(approvals.pending_count("session-1"), 1);
        assert_eq!(approvals.pending_count("other-session"), 0);

        let expired = BashApprovals::with_lifetime(
            Arc::new(FakeExecutor::default()),
            Vec::new(),
            Duration::ZERO,
        );
        expired.request("session-1", "git push");
        assert_eq!(expired.pending_count("session-1"), 0);
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

    fn temp_workspace() -> (tempfile::TempDir, Workspace) {
        let dir = tempfile::tempdir().expect("a temporary directory is creatable");
        let workspace = Workspace::new(dir.path()).expect("the workspace opens");
        (dir, workspace)
    }

    #[tokio::test]
    async fn the_definition_is_the_shared_schema() {
        let (_dir, workspace) = temp_workspace();
        let tool = bash(&workspace, Arc::new(FakeExecutor::default()), &[], 1, &[]);
        assert_eq!(
            serde_json::to_value(tool.definition()).unwrap(),
            serde_json::to_value(bash_definition()).unwrap()
        );
    }

    #[tokio::test]
    async fn it_delegates_the_exact_request_and_keeps_redaction_state_per_call() {
        let (_dir, workspace) = temp_workspace();
        let executor = Arc::new(
            FakeExecutor::chunks(&["split-*", "secret"], &["problem"]).with_status(ExitStatus {
                code: 7,
                ..ExitStatus::default()
            }),
        );
        let tool = bash(
            &workspace,
            executor.clone(),
            &["FIRST=original", "SECOND=preserved"],
            1024,
            &["split-*secret"],
        );

        let first = run(&tool, "first command").await;
        assert!(!first.is_error, "{first:?}");
        let marker = stdout_body(&first.content);
        assert_eq!(marker.chars().count(), 1, "{first:?}");
        assert_ne!(marker, "*");
        assert!(!first.content.contains("split-*secret"));

        let second = run(&tool, "second command").await;
        assert!(!second.is_error, "{second:?}");
        assert_eq!(stdout_body(&second.content), marker);

        let expected: Vec<Request> = ["first command", "second command"]
            .iter()
            .map(|command| Request {
                argv: strings(&["/bin/sh", SHELL_FLAGS, command]),
                dir: workspace.root().to_path_buf(),
                env: strings(&["FIRST=original", "SECOND=preserved"]),
            })
            .collect();
        assert_eq!(executor.requests(), expected);
    }

    #[tokio::test]
    async fn it_caps_each_stream_independently_and_reports_the_exit_code() {
        let (_dir, workspace) = temp_workspace();
        let executor = Arc::new(
            FakeExecutor::chunks(&["1234567890abcdef"], &["abcdefghijklmnop"]).with_status(
                ExitStatus {
                    code: 7,
                    signaled: false,
                    signal: "must-be-ignored".to_owned(),
                },
            ),
        );
        let tool = bash(&workspace, executor, &[], 12, &[]);
        let result = run(&tool, "ignored").await;
        assert!(!result.is_error);
        assert_eq!(
            result.content,
            "stdout:\n1234567890ab\n[truncated: 4 bytes omitted]\nstderr:\nabcdefghijkl\n[truncated: 4 bytes omitted]\nexit_code: 7"
        );
    }

    #[tokio::test]
    async fn a_signaled_exit_appends_the_signal() {
        let (_dir, workspace) = temp_workspace();
        let executor = Arc::new(FakeExecutor::default().with_status(ExitStatus {
            code: -1,
            signaled: true,
            signal: "killed".to_owned(),
        }));
        let tool = bash(&workspace, executor, &[], 1024, &[]);
        let result = run(&tool, "ignored").await;
        assert!(!result.is_error);
        assert_eq!(
            result.content,
            "stdout:\n\nstderr:\n\nexit_code: -1; signal: killed"
        );
    }

    #[tokio::test]
    async fn the_signal_text_is_redacted_too() {
        let (_dir, workspace) = temp_workspace();
        const SECRET: &str = "signal-secret-value";
        let executor = Arc::new(FakeExecutor::default().with_status(ExitStatus {
            code: -1,
            signaled: true,
            signal: format!("killed-{SECRET}"),
        }));
        let tool = bash(&workspace, executor, &[], 1024, &[SECRET]);
        let result = run(&tool, "ignored").await;
        assert!(!result.is_error);
        assert!(!result.content.contains(SECRET), "{result:?}");
        assert!(
            result
                .content
                .contains(&format!("signal: killed-{}", tool.redaction_marker)),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn split_and_overlapping_secrets_are_redacted_before_the_cap() {
        let (_dir, workspace) = temp_workspace();
        let long = format!("credential-{}", "z".repeat(32));
        let executor = Arc::new(FakeExecutor {
            stdout_chunks: vec![
                long.as_bytes()[..8].to_vec(),
                long.as_bytes()[8..].to_vec(),
                b" | overlap-".to_vec(),
                b"secret-tail | ".to_vec(),
                b"x".repeat(48),
                long.as_bytes().to_vec(),
            ],
            stderr_chunks: vec![
                b"overlap-".to_vec(),
                b"secret-tail | ".to_vec(),
                b"y".repeat(48),
                long.as_bytes().to_vec(),
            ],
            ..FakeExecutor::default()
        });
        let tool = bash(
            &workspace,
            executor,
            &[],
            32,
            &[&long, "overlap-secret", "secret-tail"],
        );
        let result = run(&tool, "ignored").await;
        assert!(!result.is_error);
        for forbidden in [long.as_str(), &long[..32], "overlap-secret", "secret-tail"] {
            assert!(
                !result.content.contains(forbidden),
                "leaked {forbidden:?}: {result:?}"
            );
        }
        assert!(
            result
                .content
                .contains(&format!("stdout:\n{}", tool.redaction_marker))
        );
        assert!(
            result
                .content
                .contains(&format!("stderr:\n{}", tool.redaction_marker))
        );
        assert_eq!(
            result.content.matches("[truncated:").count(),
            2,
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn the_marker_is_one_collision_safe_rune() {
        let (_dir, workspace) = temp_workspace();
        let cases: &[(&str, &[&str], &str, &str)] = &[
            ("TOKEN", &["TOKEN"], "", ""),
            ("*", &["*", "[REDACTED]"], "", ""),
            (
                "leftTOKENright",
                &["TOKEN", "left*right", "left!right", "[REDACTED]"],
                "left",
                "right",
            ),
            ("[REDACTED]", &["[REDACTED]"], "", ""),
        ];
        for (stdout, values, prefix, suffix) in cases {
            let executor = Arc::new(
                FakeExecutor::chunks(&[stdout], &[]).with_status(ExitStatus {
                    code: -1,
                    signaled: true,
                    signal: format!("signal-{}", values[0]),
                }),
            );
            let tool = bash(&workspace, executor, &[], 1024, values);
            let result = run(&tool, "ignored").await;
            assert!(!result.is_error, "{result:?}");
            for value in *values {
                assert!(
                    !result.content.contains(value),
                    "exposed {value:?}: {result:?}"
                );
            }
            let body = stdout_body(&result.content);
            let marker = body
                .strip_prefix(prefix)
                .and_then(|rest| rest.strip_suffix(suffix))
                .unwrap_or_else(|| panic!("body {body:?} lacks {prefix:?}/{suffix:?}"));
            let mut runes = marker.chars();
            let rune = runes.next().expect("the marker is not empty");
            assert!(
                runes.next().is_none(),
                "marker {marker:?} is more than one rune"
            );
            assert!(!rune.is_control(), "marker {marker:?} is a control rune");
            for byte in marker.as_bytes() {
                for value in *values {
                    assert!(
                        !value.as_bytes().contains(byte),
                        "marker byte {byte:#x} occurs in {value:?}"
                    );
                }
            }
        }
    }

    fn printable_ascii() -> String {
        (0x20u8..=0x7e).map(char::from).collect()
    }

    #[tokio::test]
    async fn a_multibyte_marker_cap_cannot_synthesize_a_secret() {
        let (_dir, workspace) = temp_workspace();
        let ascii = printable_ascii();
        let values = vec!["TOKEN".to_owned(), "prefix\u{fffd}".to_owned(), ascii];
        let borrowed: Vec<&str> = values.iter().map(String::as_str).collect();
        let executor = Arc::new(FakeExecutor::chunks(&["prefixTOKEN"], &[]));
        let tool = bash(&workspace, executor, &[], "prefix".len() + 1, &borrowed);
        let result = run(&tool, "ignored").await;
        assert!(!result.is_error);
        for secret in &values {
            assert!(!result.content.contains(secret.as_str()), "{result:?}");
        }
        assert_eq!(stdout_body(&result.content), "prefix");
        assert!(
            result.content.contains("[truncated: 3 bytes omitted]"),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn an_omitted_atomic_marker_cannot_join_bytes_into_another_secret() {
        let (_dir, workspace) = temp_workspace();
        let ascii = printable_ascii();
        let values = vec![
            "TOKEN".to_owned(),
            "prefix\u{fffd}".to_owned(),
            ascii,
            "prefixs".to_owned(),
        ];
        let borrowed: Vec<&str> = values.iter().map(String::as_str).collect();
        let executor = Arc::new(FakeExecutor::chunks(&["prefixTOKENsuffix"], &[]));
        let tool = bash(&workspace, executor, &[], "prefix".len() + 1, &borrowed);
        let result = run(&tool, "ignored").await;
        assert!(!result.is_error);
        for secret in &values {
            assert!(!result.content.contains(secret.as_str()), "{result:?}");
        }
        assert!(
            result.content.contains("[truncated: 9 bytes omitted]"),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn invalid_child_bytes_are_normalized_then_redacted() {
        let (_dir, workspace) = temp_workspace();
        let ascii = printable_ascii();
        let values = ["TOKEN".to_owned(), "prefix\u{fffd}".to_owned(), ascii];
        let borrowed: Vec<&str> = values.iter().map(String::as_str).collect();
        let executor = Arc::new(FakeExecutor {
            stdout_chunks: vec![b"prefix\xff".to_vec()],
            ..FakeExecutor::default()
        });
        let tool = bash(&workspace, executor, &[], 1024, &borrowed);
        let result = run(&tool, "ignored").await;
        assert!(!result.is_error);
        assert!(!result.content.contains("prefix\u{fffd}"), "{result:?}");
    }

    #[tokio::test]
    async fn invalid_redactions_are_canonicalized_separately_from_the_environment() {
        let (_dir, workspace) = temp_workspace();
        let invalid_ff = String::from_utf8_lossy(b"decoded-prefix\xff").into_owned();
        let invalid_overlong = String::from_utf8_lossy(b"overlong-prefix\xc0\xaf").into_owned();
        let environment = vec![format!("RAW_ENDPOINT={invalid_ff}")];
        for max_output in [1usize, 1024] {
            let expected = environment.clone();
            let executor = Arc::new(FakeExecutor::behaving(Box::new(
                move |request, streams, _| {
                    let expected = expected.clone();
                    Box::pin(async move {
                        assert_eq!(request.env, expected, "the environment was modified");
                        for byte in b"decoded-prefix\xff" {
                            streams.stdout.write_all(&[*byte]).unwrap();
                        }
                        for byte in b"overlong-prefix\xc0\xaf" {
                            streams.stderr.write_all(&[*byte]).unwrap();
                        }
                        (ExitStatus::default(), Ok(()))
                    })
                },
            )));
            let tool = BashTool::new(
                &workspace,
                executor,
                "/bin/sh",
                environment.clone(),
                Duration::from_secs(3),
                max_output,
                &[invalid_ff.clone(), invalid_overlong.clone()],
            )
            .expect("the configuration is valid");
            let result = run(&tool, "ignored").await;
            assert!(!result.is_error, "{result:?}");
            for secret in ["decoded-prefix\u{fffd}", "overlong-prefix\u{fffd}\u{fffd}"] {
                assert!(
                    !result.content.contains(secret),
                    "cap {max_output}: {result:?}"
                );
            }
            if max_output == 1 {
                assert_eq!(stdout_body(&result.content), "");
                assert!(
                    result
                        .content
                        .contains("stdout:\n[truncated: 3 bytes omitted]"),
                    "{result:?}"
                );
                assert!(
                    result
                        .content
                        .contains("stderr:\n[truncated: 3 bytes omitted]"),
                    "{result:?}"
                );
            }
        }
    }

    #[tokio::test]
    async fn the_collision_safe_marker_survives_every_byte_cap() {
        let (_dir, workspace) = temp_workspace();
        let values = vec!["TOKEN".to_owned(), printable_ascii()];
        let borrowed: Vec<&str> = values.iter().map(String::as_str).collect();
        let for_cap = |cap: usize| {
            let executor = Arc::new(FakeExecutor::chunks(&["TOKEN"], &[]).with_status(
                ExitStatus {
                    code: -1,
                    signaled: true,
                    signal: "signal-TOKEN".to_owned(),
                },
            ));
            bash(&workspace, executor, &[], cap, &borrowed)
        };

        let full = run(&for_cap(1024), "ignored").await;
        assert!(!full.is_error, "{full:?}");
        let marker = stdout_body(&full.content);
        assert_eq!(marker.chars().count(), 1);
        assert!(
            marker.len() > 1,
            "fallback marker {marker:?} should be multibyte"
        );

        for cap in 1..=marker.len() + 1 {
            let result = run(&for_cap(cap), "ignored").await;
            assert!(!result.is_error, "cap {cap}: {result:?}");
            for value in &values {
                assert!(
                    !result.content.contains(value.as_str()),
                    "cap {cap}: {result:?}"
                );
            }
            let want = if cap < marker.len() {
                ""
            } else {
                marker.as_str()
            };
            assert_eq!(stdout_body(&result.content), want, "cap {cap}");
            if cap < marker.len() {
                assert!(
                    result
                        .content
                        .contains(&format!("[truncated: {} bytes omitted]", marker.len())),
                    "cap {cap}: {result:?}"
                );
            }
        }
    }

    /// Every UTF-8 byte class in one secret, port of `markerByteClassAdversary`.
    fn marker_byte_class_adversary() -> String {
        let mut value: Vec<u8> = (0x20u8..=0x7e).collect();
        for continuation in 0x80u8..=0xbf {
            value.extend_from_slice(&[0xc2, continuation]);
        }
        for lead in 0xc3u8..=0xdf {
            value.extend_from_slice(&[lead, 0x80]);
        }
        value.extend_from_slice(&[0xe0, 0xa0, 0x80]);
        for lead in 0xe1u8..=0xec {
            value.extend_from_slice(&[lead, 0x80, 0x80]);
        }
        value.extend_from_slice(&[0xed, 0x80, 0x80]);
        for lead in 0xeeu8..=0xef {
            value.extend_from_slice(&[lead, 0x80, 0x80]);
        }
        value.extend_from_slice(&[0xf0, 0x90, 0x80, 0x80]);
        for lead in 0xf1u8..=0xf3 {
            value.extend_from_slice(&[lead, 0x80, 0x80, 0x80]);
        }
        value.extend_from_slice(&[0xf4, 0x80, 0x80, 0x80]);
        String::from_utf8(value).expect("the adversary is valid UTF-8")
    }

    #[tokio::test]
    async fn every_utf8_byte_class_in_a_secret_still_constructs() {
        let (_dir, workspace) = temp_workspace();
        let secret = marker_byte_class_adversary();
        let executor = Arc::new(FakeExecutor::chunks(&[&secret], &[]));
        let tool = bash(&workspace, executor, &[], 1024, &[&secret]);
        let result = run(&tool, "ignored").await;
        assert!(!result.is_error, "{result:?}");
        assert!(!result.content.contains(&secret), "{result:?}");
    }

    #[tokio::test]
    async fn json_string_escapes_in_secrets_are_expanded() {
        let (_dir, workspace) = temp_workspace();
        let cases: &[(&str, &str)] = &[
            (r"less<than", "less<than"),
            (r"greater>than", "greater>than"),
            (r"amp&ersand", "amp&ersand"),
            (r#"quote\"value"#, "quote\"value"),
            (r"back\\slash", r"back\slash"),
            (r"slash\/value", "slash/value"),
            (r"line\nbreak", "line\nbreak"),
            (r"separator value", "separator\u{2028}value"),
            (r"paragraph value", "paragraph\u{2029}value"),
        ];
        let values: Vec<&str> = cases.iter().map(|(raw, _)| *raw).collect();
        let chunks: Vec<String> = cases
            .iter()
            .map(|(_, decoded)| format!("{decoded}\n"))
            .collect();
        let executor = Arc::new(FakeExecutor {
            stdout_chunks: chunks
                .iter()
                .map(|chunk| chunk.as_bytes().to_vec())
                .collect(),
            ..FakeExecutor::default()
        });
        let tool = bash(&workspace, executor, &[], 64 << 10, &values);
        let result = run(&tool, "ignored").await;
        assert!(!result.is_error, "{result:?}");
        for (raw_form, decoded) in cases {
            assert!(!result.content.contains(raw_form), "{result:?}");
            assert!(!result.content.contains(decoded), "{result:?}");
        }
    }

    /// Every non-control scalar value, port of `allNonControlSandboxRunes`.
    fn all_non_control_runes() -> String {
        (1u32..=char::MAX as u32)
            .filter_map(char::from_u32)
            .filter(|rune| !rune.is_control())
            .collect()
    }

    #[tokio::test]
    async fn an_exhausted_marker_set_suppresses_all_result_text() {
        let (_dir, workspace) = temp_workspace();
        let marker = otto_core::safetext::dynamic_redaction_marker(&[])
            .expect("the empty set has a shared marker");
        let values = [
            all_non_control_runes(),
            format!("{}z", "a".repeat(1 << 20)),
            marker,
            "X".to_owned(),
            "ab".to_owned(),
        ];
        let borrowed: Vec<&str> = values.iter().map(String::as_str).collect();
        let executor = Arc::new(FakeExecutor::chunks(&["a", "X", "b"], &["aX", "b"]));
        let tool = bash(&workspace, executor.clone(), &[], 2 << 20, &borrowed);
        assert!(tool.redaction_marker.is_empty());
        assert!(!tool.dynamic_content);

        let result = run(&tool, "ignored").await;
        assert_eq!(result.content, "");
        assert_eq!(executor.calls(), 1);

        let invalid = tool
            .execute(
                &raw(r#"{"command":123,"X":"X"}"#),
                &CancellationToken::new(),
            )
            .await;
        assert_eq!(invalid.content, "");
        assert!(invalid.is_error);
        assert_eq!(executor.calls(), 1);
    }

    #[tokio::test]
    async fn unrepresentable_output_still_runs_the_command_once() {
        let (_dir, workspace) = temp_workspace();
        const HALF: usize = 4 << 10;
        let executor = Arc::new(FakeExecutor::behaving(Box::new(|_, streams, _| {
            Box::pin(async move {
                for _ in 0..HALF {
                    streams.stdout.write_all(b"a").unwrap();
                }
                streams.stdout.write_all(b"X").unwrap();
                for _ in 0..HALF {
                    streams.stdout.write_all(b"b").unwrap();
                }
                (ExitStatus::default(), Ok(()))
            })
        })));
        let secret = format!("{}z", "a".repeat(1 << 20));
        let tool = bash(&workspace, executor.clone(), &[], 2 * HALF + 1, &[&secret]);
        for _ in 0..3 {
            assert_eq!(run(&tool, "ignored").await.content, "");
        }
        assert_eq!(executor.calls(), 3);
    }

    #[tokio::test]
    async fn an_earlier_fragment_is_held_until_the_later_match_resolves() {
        let (_dir, workspace) = temp_workspace();
        const LONG: &str = "credential-zzSHORT-rest";
        for split in 0..=LONG.len() {
            let executor = Arc::new(FakeExecutor::chunks(&[&LONG[..split], &LONG[split..]], &[]));
            let tool = bash(&workspace, executor, &[], 12, &[LONG, "SHORT"]);
            let result = run(&tool, "ignored").await;
            assert!(!result.is_error, "split {split}: {result:?}");
            assert_eq!(
                stdout_body(&result.content),
                tool.redaction_marker,
                "split {split}"
            );
            assert!(
                !result.content.contains("credential-"),
                "split {split}: {result:?}"
            );
            for value in [LONG, "SHORT"] {
                assert!(!result.content.contains(value), "split {split}: {result:?}");
            }
        }
    }

    #[tokio::test]
    async fn strict_json_errors_are_redacted_and_never_delegate() {
        let (_dir, workspace) = temp_workspace();
        let cases: &[(&str, &str, &str)] = &[
            (
                "private-field",
                r#"{"command":"true","private-field":"private-field"}"#,
                "json: unknown field",
            ),
            ("number", r#"{"command":123}"#, "invalid JSON:"),
            ("command", "{}", "missing required argument:"),
        ];
        for (secret, arguments, want) in cases {
            let executor = Arc::new(FakeExecutor::default());
            let tool = bash(&workspace, executor.clone(), &[], 1024, &[secret]);
            let result = tool
                .execute(&raw(arguments), &CancellationToken::new())
                .await;
            assert!(result.is_error, "{arguments}: {result:?}");
            assert!(result.content.contains(want), "{arguments}: {result:?}");
            assert!(!result.content.contains(secret), "{arguments}: {result:?}");
            assert_eq!(executor.calls(), 0, "{arguments}");
        }
    }

    #[tokio::test]
    async fn invalid_arguments_and_pre_cancellation_never_execute() {
        let (_dir, workspace) = temp_workspace();
        let executor = Arc::new(FakeExecutor::default());
        let tool = bash(&workspace, executor.clone(), &[], 1024, &[]);
        let cases: &[(&str, &str)] = &[
            (
                r#"{"command":"true","extra":true}"#,
                r#"json: unknown field "extra""#,
            ),
            ("{}", "missing required argument: command"),
            (
                r#"{"command":" \t "}"#,
                "missing required argument: command",
            ),
        ];
        for (arguments, want) in cases {
            let result = tool
                .execute(&raw(arguments), &CancellationToken::new())
                .await;
            assert!(result.is_error, "{arguments}: {result:?}");
            assert!(result.content.contains(want), "{arguments}: {result:?}");
        }
        assert_eq!(executor.calls(), 0);

        let cancel = CancellationToken::new();
        cancel.cancel();
        let result = tool
            .execute(&raw(r#"{"command":"must not run"}"#), &cancel)
            .await;
        assert!(!result.is_error, "{result:?}");
        assert_eq!(result.content, "stdout:\n\nstderr:\n\nstatus: cancelled");
        assert_eq!(executor.calls(), 0);
    }

    #[tokio::test]
    async fn caller_cancellation_reports_partial_output_and_the_signal() {
        let (_dir, workspace) = temp_workspace();
        let started = Arc::new(tokio::sync::Notify::new());
        let observed = started.clone();
        let executor = Arc::new(FakeExecutor::behaving(Box::new(
            move |_, streams, cancel| {
                let started = started.clone();
                Box::pin(async move {
                    started.notify_one();
                    cancel.cancelled().await;
                    streams.stdout.write_all(b"partial output").unwrap();
                    (
                        ExitStatus {
                            code: -1,
                            signaled: true,
                            signal: "killed".to_owned(),
                        },
                        Err(Error::Cancelled),
                    )
                })
            },
        )));
        let tool = bash(
            &workspace,
            executor,
            &[],
            1024,
            &["longer-than-partial-output-secret"],
        );
        let cancel = CancellationToken::new();
        let call = {
            let cancel = cancel.clone();
            async move { tool.execute(&raw(r#"{"command":"wait"}"#), &cancel).await }
        };
        let waiter = async {
            observed.notified().await;
            cancel.cancel();
        };
        let (result, ()) = tokio::join!(call, waiter);
        assert!(!result.is_error, "{result:?}");
        assert_eq!(
            result.content,
            "stdout:\npartial output\nstderr:\n\nstatus: cancelled; signal: killed"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_timeout_is_reported_with_the_go_duration_text() {
        let (_dir, workspace) = temp_workspace();
        let executor = Arc::new(FakeExecutor::behaving(Box::new(|_, _, cancel| {
            Box::pin(async move {
                cancel.cancelled().await;
                (
                    ExitStatus {
                        code: -1,
                        signaled: true,
                        signal: "killed".to_owned(),
                    },
                    Err(Error::Cancelled),
                )
            })
        })));
        let tool = bash(&workspace, executor, &[], 1024, &[]);
        let result = run(&tool, "wait").await;
        assert!(!result.is_error, "{result:?}");
        assert_eq!(
            result.content,
            "stdout:\n\nstderr:\n\nstatus: timed out after 3s; signal: killed"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_fired_timeout_survives_a_clean_executor_return() {
        let (_dir, workspace) = temp_workspace();
        let executor = Arc::new(FakeExecutor::behaving(Box::new(|_, _, cancel| {
            Box::pin(async move {
                cancel.cancelled().await;
                (
                    ExitStatus {
                        code: -1,
                        signaled: true,
                        signal: "killed".to_owned(),
                    },
                    Ok(()),
                )
            })
        })));
        let tool = bash(&workspace, executor, &[], 1024, &[]);
        let result = run(&tool, "wait").await;
        assert!(
            result
                .content
                .contains("status: timed out after 3s; signal: killed"),
            "{result:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn parent_cancellation_wins_the_timeout_race() {
        let (_dir, workspace) = temp_workspace();
        let observed = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let seen = observed.clone();
        let gate = release.clone();
        let executor = Arc::new(FakeExecutor::behaving(Box::new(move |_, _, cancel| {
            let observed = observed.clone();
            let release = release.clone();
            Box::pin(async move {
                cancel.cancelled().await;
                observed.notify_one();
                release.notified().await;
                (
                    ExitStatus {
                        code: -1,
                        signaled: true,
                        signal: "killed".to_owned(),
                    },
                    Err(Error::Cancelled),
                )
            })
        })));
        let tool = bash(&workspace, executor, &[], 1024, &[]);
        let cancel = CancellationToken::new();
        let call = {
            let cancel = cancel.clone();
            async move { tool.execute(&raw(r#"{"command":"wait"}"#), &cancel).await }
        };
        let driver = async {
            seen.notified().await;
            cancel.cancel();
            gate.notify_one();
        };
        let (result, ()) = tokio::join!(call, driver);
        assert!(
            result.content.contains("status: cancelled; signal: killed"),
            "{result:?}"
        );
        assert!(!result.content.contains("timed out"), "{result:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_successful_return_does_not_report_the_timeout() {
        let (_dir, workspace) = temp_workspace();
        let executor = Arc::new(FakeExecutor::default());
        let tool = bash(&workspace, executor, &[], 1024, &[]);
        let result = run(&tool, "true").await;
        assert!(!result.is_error, "{result:?}");
        assert!(result.content.contains("exit_code: 0"), "{result:?}");
        assert!(!result.content.contains("timed out"), "{result:?}");
    }

    #[tokio::test]
    async fn infrastructure_errors_are_fixed_and_discard_diagnostics() {
        let (_dir, workspace) = temp_workspace();
        const SECRET: &str = "infrastructure-secret-value";
        let errors = [
            Error::ChildLaunch,
            Error::ChildWait,
            Error::ChildTerminate,
            Error::Closed,
            Error::Unavailable(UnavailableReason::RuntimeFailure),
            Error::InvalidRequest,
            Error::EnvironmentUnsafe,
            Error::UnsupportedPolicy,
        ];
        for error in errors {
            let executor = Arc::new(FakeExecutor {
                stdout_chunks: vec![b"diagnostic-".to_vec(), SECRET.as_bytes().to_vec()],
                stderr_chunks: vec![format!("raw stderr {SECRET}").into_bytes()],
                error: Some(error.clone()),
                ..FakeExecutor::default()
            });
            let tool = bash(&workspace, executor, &[], 8, &[SECRET]);
            let result = run(&tool, "ignored").await;
            assert!(result.is_error, "{error:?}: {result:?}");
            assert_eq!(result.content, SANDBOX_EXECUTION_UNAVAILABLE, "{error:?}");
        }
    }

    #[tokio::test]
    async fn an_infrastructure_error_wins_an_ended_context() {
        let (_dir, workspace) = temp_workspace();
        const SECRET: &str = "infrastructure-secret-value";
        let started = Arc::new(tokio::sync::Notify::new());
        let observed = started.clone();
        let executor = Arc::new(FakeExecutor::behaving(Box::new(
            move |_, streams, cancel| {
                let started = started.clone();
                Box::pin(async move {
                    started.notify_one();
                    cancel.cancelled().await;
                    streams.stdout.write_all(SECRET.as_bytes()).unwrap();
                    (
                        ExitStatus {
                            code: -1,
                            signaled: true,
                            signal: "killed".to_owned(),
                        },
                        Err(Error::ChildTerminate),
                    )
                })
            },
        )));
        let tool = bash(&workspace, executor, &[], 1024, &[SECRET]);
        let cancel = CancellationToken::new();
        let call = {
            let cancel = cancel.clone();
            async move { tool.execute(&raw(r#"{"command":"wait"}"#), &cancel).await }
        };
        let driver = async {
            observed.notified().await;
            cancel.cancel();
        };
        let (result, ()) = tokio::join!(call, driver);
        assert!(result.is_error, "{result:?}");
        assert_eq!(result.content, SANDBOX_EXECUTION_UNAVAILABLE);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_calls_keep_output_and_redaction_independent() {
        let (_dir, workspace) = temp_workspace();
        let arrived = Arc::new(tokio::sync::Semaphore::new(0));
        let release = Arc::new(tokio::sync::Notify::new());
        let signal = arrived.clone();
        let gate = release.clone();
        let executor = Arc::new(FakeExecutor::behaving(Box::new(
            move |request, streams, _| {
                let arrived = arrived.clone();
                let release = release.clone();
                let name = request.argv[2].clone();
                Box::pin(async move {
                    streams
                        .stdout
                        .write_all(format!("{name}:shared-").as_bytes())
                        .unwrap();
                    arrived.add_permits(1);
                    release.notified().await;
                    streams.stdout.write_all(b"secret").unwrap();
                    streams
                        .stderr
                        .write_all(format!("stderr-{name}").as_bytes())
                        .unwrap();
                    (ExitStatus::default(), Ok(()))
                })
            },
        )));
        let tool = Arc::new(bash(&workspace, executor, &[], 1024, &["shared-secret"]));

        let mut handles = Vec::new();
        for name in ["first", "second"] {
            let tool = tool.clone();
            handles.push(tokio::spawn(async move { (name, run(&tool, name).await) }));
        }
        let _permits = signal.acquire_many(2).await.expect("both calls arrive");
        gate.notify_waiters();

        for handle in handles {
            let (name, result) = handle.await.expect("the call completes");
            let other = if name == "first" { "second" } else { "first" };
            assert!(!result.is_error, "{name}: {result:?}");
            assert!(
                result
                    .content
                    .contains(&format!("{name}:{}", tool.redaction_marker)),
                "{name}: {result:?}"
            );
            assert!(
                result.content.contains(&format!("stderr-{name}")),
                "{name}: {result:?}"
            );
            assert!(
                !result.content.contains(&format!("{other}:")),
                "{name}: {result:?}"
            );
            assert!(
                !result.content.contains("shared-secret"),
                "{name}: {result:?}"
            );
        }
    }

    /// Which shell invocation each platform gets. macOS runs a login shell so
    /// the command sees the `PATH` `path_helper` builds; nowhere else, where
    /// `/etc/profile.d/*` would write into the tool result the model reads
    /// and could restore variables the environment filter removed.
    #[test]
    fn only_macos_starts_the_command_shell_as_a_login_shell() {
        if cfg!(target_os = "macos") {
            assert_eq!(SHELL_FLAGS, "-lc");
        } else {
            assert_eq!(SHELL_FLAGS, "-c");
        }
    }

    #[tokio::test]
    async fn it_runs_a_real_command_through_the_direct_driver() {
        let (_dir, workspace) = temp_workspace();
        let executor = Executor::new(
            Arc::new(crate::sandbox::direct::DirectDriver::new()),
            Policy {
                filesystem: FilesystemMode::Unconfined,
                network: NetworkMode::Allow,
            },
            workspace.root(),
        )
        .expect("the executor opens");
        let tool = BashTool::new(
            &workspace,
            Arc::new(executor),
            "/bin/sh",
            strings(&[
                "PATH=/usr/bin:/bin",
                "HOME=",
                "ENV=",
                "LC_ALL=C",
                "OTTO_DIRECT_VALUE=deterministic",
            ]),
            Duration::from_secs(3),
            4096,
            &[],
        )
        .expect("the configuration is valid");
        let result = run(
            &tool,
            r#"printf 'cwd='; /bin/pwd; printf 'env=%s\n' "$OTTO_DIRECT_VALUE"; printf 'problem\n' >&2; exit 7"#,
        )
        .await;
        assert!(!result.is_error, "{result:?}");
        for expected in [
            format!("stdout:\ncwd={}", workspace.root().display()),
            "env=deterministic".to_owned(),
            "stderr:\nproblem".to_owned(),
            "exit_code: 7".to_owned(),
        ] {
            assert!(
                result.content.contains(&expected),
                "missing {expected:?}: {result:?}"
            );
        }
    }

    #[test]
    fn the_constructor_rejects_every_invalid_boundary_with_one_safe_error() {
        let (dir, workspace) = temp_workspace();
        let removed = tempfile::tempdir().expect("a temporary directory is creatable");
        let removed_root = removed.path().join("removed");
        std::fs::create_dir(&removed_root).expect("the directory is creatable");
        let removed_workspace = Workspace::new(&removed_root).expect("the workspace opens");
        std::fs::remove_dir(&removed_root).expect("the directory is removable");

        let cases: &[(&str, &Workspace, &str, Duration, usize)] = &[
            (
                "missing workspace",
                &removed_workspace,
                "/bin/sh",
                Duration::from_secs(1),
                1,
            ),
            ("empty shell", &workspace, "", Duration::from_secs(1), 1),
            (
                "whitespace shell",
                &workspace,
                " \t\n",
                Duration::from_secs(1),
                1,
            ),
            (
                "NUL shell",
                &workspace,
                "/bin/\u{0}sh",
                Duration::from_secs(1),
                1,
            ),
            ("zero timeout", &workspace, "/bin/sh", Duration::ZERO, 1),
            (
                "zero output cap",
                &workspace,
                "/bin/sh",
                Duration::from_secs(1),
                0,
            ),
        ];
        for (name, workspace, shell, timeout, max_output_bytes) in cases {
            let outcome = BashTool::new(
                workspace,
                Arc::new(FakeExecutor::default()),
                shell,
                Vec::new(),
                *timeout,
                *max_output_bytes,
                &strings(&["must-not-appear"]),
            );
            let Err(error) = outcome else {
                panic!("{name} should be rejected");
            };
            assert_eq!(
                error.to_string(),
                "invalid sandboxed bash configuration",
                "{name}"
            );
        }
        drop(dir);
    }

    #[test]
    fn the_constructor_handles_former_byte_marker_exhaustion() {
        let (_dir, workspace) = temp_workspace();
        let all_bytes = String::from_utf8_lossy(&(0u8..=255).collect::<Vec<u8>>()).into_owned();
        BashTool::new(
            &workspace,
            Arc::new(FakeExecutor::default()),
            "/bin/sh",
            Vec::new(),
            Duration::from_secs(1),
            1024,
            &[all_bytes],
        )
        .expect("an exhausted byte set still constructs safely");
    }

    #[tokio::test]
    async fn it_registers_with_an_explicit_empty_environment() {
        let (_dir, workspace) = temp_workspace();
        let executor = Arc::new(FakeExecutor::default());
        let tool = bash(&workspace, executor.clone(), &[], 1024, &[]);
        let registry = crate::tool::registry::Registry::new(vec![Box::new(tool)])
            .expect("the registry accepts one tool");
        let definitions: Vec<String> = registry
            .tools()
            .iter()
            .map(|tool| tool.definition().name)
            .collect();
        assert_eq!(definitions, vec!["bash".to_owned()]);

        let result = otto_core::tool::ToolExecutor::execute(
            &registry,
            "bash",
            &raw(r#"{"command":"true"}"#),
            &CancellationToken::new(),
        )
        .await;
        assert!(!result.is_error, "{result:?}");
        let requests = executor.requests();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].env.is_empty());
    }

    #[test]
    fn durations_render_the_way_go_prints_them() {
        assert_eq!(go_duration(Duration::from_secs(3)), "3s");
        assert_eq!(go_duration(Duration::from_millis(1500)), "1.5s");
        assert_eq!(go_duration(Duration::from_secs(90)), "1m30s");
        assert_eq!(go_duration(Duration::from_secs(3600)), "1h0m0s");
        assert_eq!(go_duration(Duration::from_millis(250)), "250ms");
        assert_eq!(go_duration(Duration::from_micros(1)), "1\u{b5}s");
        assert_eq!(go_duration(Duration::from_nanos(999)), "999ns");
    }
}
