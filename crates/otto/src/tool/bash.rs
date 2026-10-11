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
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use otto_core::model::ToolDefinition;
use otto_core::tool::ToolResult;
use serde::Deserialize;
use serde_json::json;
use serde_json::value::RawValue;
use sha2::Digest;
use tokio_util::sync::CancellationToken;

use crate::sandbox::{
    CommandExecutor, DEFAULT_CANCELLATION_GRACE, Error, ExitStatus, Request, Streams,
};
use otto_core::safetext::{dynamic_redaction_marker, secret_forms};

use super::bash_output::{Logs, Output, PREVIEW_BYTES};
use super::result::{RedactingCollector, decode_strict_json, redact_exact_text};
use super::root::Root;
use super::shell;
use super::workspace::Workspace;
use super::{Tool, definition};

/// An elevated-command approval request decoded from a `bash` error result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRequest {
    pub id: String,
    pub command: String,
    pub justification: String,
    pub read_path: String,
}

/// Decodes the approval-required result `BashTool::execute` returns for an
/// elevated call that has no grant (the `Approve in Otto: /approve {id}`,
/// `Command:` and `Justification:` lines). `None` for any other result.
pub fn parse_approval_request(tool_name: &str, result: &ToolResult) -> Option<ApprovalRequest> {
    if tool_name != "bash" || !result.is_error {
        return None;
    }
    let approve = result
        .content
        .lines()
        .find_map(|line| line.strip_prefix("Approve in Otto: "))?;
    let mut parts = approve.split_whitespace();
    if parts.next()? != "/approve" {
        return None;
    }
    let id = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let field = |prefix: &str| {
        result
            .content
            .lines()
            .find_map(|line| line.strip_prefix(prefix))
            .map(decode_approval_field)
            .unwrap_or_default()
    };
    Some(ApprovalRequest {
        id: id.to_string(),
        command: field("Command: "),
        justification: field("Justification: "),
        read_path: field("Read path: "),
    })
}

fn decode_approval_field(value: &str) -> String {
    serde_json::from_str::<String>(value).unwrap_or_else(|_| value.to_string())
}

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
    sandbox_read_path: String,
    #[serde(default)]
    justification: String,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum SandboxPermissions {
    #[default]
    UseDefault,
    RequireEscalated,
}

struct Approval {
    id: String,
    command: String,
    read_path: String,
    granted: bool,
    deciding: bool,
}

#[derive(Default)]
struct ApprovalState {
    next_id: u64,
    requests: HashMap<String, Approval>,
}

/// Process-local pending Bash permissions and exact-command retry grants.
///
/// One mutex serializes requests across sessions. A session keeps only its
/// newest request. A grant is removed before the matching command starts, so
/// failure or cancellation cannot reuse it.
pub struct BashApprovals {
    executor: Arc<dyn CommandExecutor>,
    environment: Vec<String>,
    state: Mutex<ApprovalState>,
    changes: tokio::sync::watch::Sender<u64>,
    pub(crate) decision: tokio::sync::Mutex<()>,
}

/// Commands the user listed in `[sandbox].excluded_commands`, with the
/// unconfined executor and real-home environment they run in.
///
/// One value per workspace sandbox, shared by the parent and child bash tools
/// and by the reloader. Entries are replaced as a whole under a lock;
/// `matches` reads a consistent list.
pub struct ExcludedCommands {
    executor: Arc<dyn CommandExecutor>,
    environment: Vec<String>,
    entries: RwLock<Vec<String>>,
}

impl ExcludedCommands {
    pub fn new(
        executor: Arc<dyn CommandExecutor>,
        environment: Vec<String>,
        entries: Vec<String>,
    ) -> Self {
        Self {
            executor,
            environment,
            entries: RwLock::new(entries),
        }
    }

    pub fn set_entries(&self, entries: Vec<String>) {
        *self.entries.write().expect("excluded commands lock") = entries;
    }

    pub(crate) fn matches(&self, command: &str) -> bool {
        let entries = self.entries.read().expect("excluded commands lock");
        otto_core::config::sandbox::excluded_command_matches(&entries, command)
    }
}

impl BashApprovals {
    pub fn new(executor: Arc<dyn CommandExecutor>, environment: Vec<String>) -> Self {
        Self {
            executor,
            environment,
            state: Mutex::new(ApprovalState::default()),
            changes: tokio::sync::watch::channel(0).0,
            decision: tokio::sync::Mutex::new(()),
        }
    }

    pub(crate) fn request(&self, session_id: &str, command: &str) -> String {
        self.request_read_path(session_id, command, "")
    }

    pub(crate) fn request_read_path(
        &self,
        session_id: &str,
        command: &str,
        read_path: &str,
    ) -> String {
        let mut state = self.state.lock().expect("bash approval mutex");
        if let Some(request) = state
            .requests
            .get(session_id)
            .filter(|request| request.command == command && request.read_path == read_path)
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
                read_path: read_path.to_owned(),
                granted: false,
                deciding: false,
            },
        );
        self.changes
            .send_modify(|revision| *revision = revision.wrapping_add(1));
        id
    }

    /// Grants one pending command for `session_id`.
    pub fn approve(&self, session_id: &str, id: &str) -> Result<(), &'static str> {
        let mut state = self.state.lock().expect("bash approval mutex");
        let request = state
            .requests
            .get_mut(session_id)
            .filter(|request| request.id == id)
            .ok_or("approval request not found")?;
        request.granted = true;
        Ok(())
    }

    /// The command of `session_id`'s pending request `id`, granted or not.
    pub fn pending_command(&self, session_id: &str, id: &str) -> Option<String> {
        let state = self.state.lock().expect("bash approval mutex");
        state
            .requests
            .get(session_id)
            .filter(|request| request.id == id)
            .map(|request| request.command.clone())
    }

    /// The canonical read path, or an empty string for an elevated command.
    pub fn pending_read_path(&self, session_id: &str, id: &str) -> Option<String> {
        self.state
            .lock()
            .expect("bash approval mutex")
            .requests
            .get(session_id)
            .filter(|request| request.id == id)
            .map(|request| request.read_path.clone())
    }

    /// A snapshot of the ungranted request bound to this session.
    pub fn pending(&self, session_id: &str) -> Option<(String, String, String)> {
        let state = self.state.lock().expect("bash approval mutex");
        state
            .requests
            .get(session_id)
            .filter(|request| !request.granted && !request.deciding)
            .map(|request| {
                (
                    request.id.clone(),
                    request.command.clone(),
                    request.read_path.clone(),
                )
            })
    }

    /// Reserves a user decision before waking the retry loop. It does not
    /// grant execution; the controller still validates and applies any read
    /// grant. Model withdrawal cannot overtake a decision already accepted.
    pub fn reserve(&self, session_id: &str, id: &str) -> Result<(), &'static str> {
        let mut state = self.state.lock().expect("bash approval mutex");
        let request = state
            .requests
            .get_mut(session_id)
            .filter(|request| request.id == id && !request.granted && !request.deciding)
            .ok_or("approval is no longer pending")?;
        request.deciding = true;
        Ok(())
    }

    /// Only an ungranted, exact request may be withdrawn. Never undoes execution.
    pub fn revoke(&self, session_id: &str, id: &str) -> Result<(), &'static str> {
        let mut state = self.state.lock().expect("bash approval mutex");
        if !state
            .requests
            .get(session_id)
            .is_some_and(|request| request.id == id && !request.granted && !request.deciding)
        {
            return Err("approval is no longer pending");
        }
        state.requests.remove(session_id);
        self.changes
            .send_modify(|revision| *revision = revision.wrapping_add(1));
        Ok(())
    }

    /// Subscribe before testing the request to avoid losing a concurrent withdrawal.
    pub async fn withdrawn(&self, session_id: &str, id: &str) {
        let mut changes = self.changes.subscribe();
        loop {
            if self.pending_command(session_id, id).is_none() {
                return;
            }
            if changes.changed().await.is_err() {
                return;
            }
        }
    }

    pub fn discard(&self, session_id: &str, id: &str) {
        let mut state = self.state.lock().expect("bash approval mutex");
        if state
            .requests
            .get(session_id)
            .is_some_and(|request| request.id == id)
        {
            state.requests.remove(session_id);
            self.changes
                .send_modify(|revision| *revision = revision.wrapping_add(1));
        }
    }

    /// Pending approvals for `session_id`: 0 or 1, since a session keeps only
    /// its newest request.
    pub fn pending_count(&self, session_id: &str) -> usize {
        let state = self.state.lock().expect("bash approval mutex");
        usize::from(state.requests.contains_key(session_id))
    }

    pub(crate) fn take(&self, session_id: &str, command: &str) -> bool {
        self.take_read_path(session_id, command, "")
    }

    pub(crate) fn take_read_path(&self, session_id: &str, command: &str, read_path: &str) -> bool {
        let mut state = self.state.lock().expect("bash approval mutex");
        let granted = state.requests.get(session_id).is_some_and(|request| {
            request.granted && request.command == command && request.read_path == read_path
        });
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
    grant_control: Option<(PathBuf, Arc<dyn crate::app::SandboxControl>)>,
    workspace_root: PathBuf,
    output_root: Arc<Root>,
    output_logs: Arc<Logs>,
    executor: Arc<dyn CommandExecutor>,
    shell: String,
    environment: Vec<String>,
    timeout: Duration,
    cancellation_grace: Duration,
    max_output_bytes: usize,
    redact_values: Vec<String>,
    redaction_marker: String,
    dynamic_content: bool,
    approvals: Option<(String, Arc<BashApprovals>)>,
    excluded: Option<Arc<ExcludedCommands>>,
    private_home: bool,
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
    /// the timeout or cancellation grace is zero, or the output cap is zero.
    pub fn new(
        workspace: &Workspace,
        executor: Arc<dyn CommandExecutor>,
        shell: &str,
        environment: Vec<String>,
        timeout: Duration,
        max_output_bytes: usize,
        redaction_values: &[String],
    ) -> Result<Self, InvalidConfiguration> {
        Self::new_with_grace(
            workspace,
            executor,
            shell,
            environment,
            timeout,
            DEFAULT_CANCELLATION_GRACE,
            max_output_bytes,
            redaction_values,
        )
    }

    /// Binds the tool to one executor with a caller-selected process
    /// termination grace.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_grace(
        workspace: &Workspace,
        executor: Arc<dyn CommandExecutor>,
        shell: &str,
        environment: Vec<String>,
        timeout: Duration,
        cancellation_grace: Duration,
        max_output_bytes: usize,
        redaction_values: &[String],
    ) -> Result<Self, InvalidConfiguration> {
        if !valid_workspace(workspace)
            || shell.trim().is_empty()
            || shell.contains('\0')
            || timeout.is_zero()
            || cancellation_grace.is_zero()
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
            grant_control: None,
            output_logs: Arc::default(),
            workspace_root: workspace.root().to_path_buf(),
            output_root: Arc::new(
                Root::from_directory(
                    workspace
                        .root_fs()
                        .open_file(
                            Path::new("."),
                            nix::fcntl::OFlag::O_RDONLY | nix::fcntl::OFlag::O_DIRECTORY,
                            nix::sys::stat::Mode::empty(),
                        )
                        .map_err(|_| InvalidConfiguration)?,
                )
                .map_err(|_| InvalidConfiguration)?,
            ),
            executor,
            shell: shell.to_owned(),
            environment,
            timeout,
            cancellation_grace,
            max_output_bytes,
            redact_values,
            redaction_marker,
            dynamic_content,
            approvals: None,
            excluded: None,
            private_home: false,
        })
    }

    pub(crate) fn with_output_logs(mut self, logs: Arc<Logs>) -> Self {
        self.output_logs = logs;
        self
    }

    /// Enables explicit, one-shot elevation for this session.
    pub fn with_grant_control(
        mut self,
        path: PathBuf,
        control: Arc<dyn crate::app::SandboxControl>,
    ) -> Self {
        self.grant_control = Some((path, control));
        self
    }

    pub fn with_approvals(
        mut self,
        session_id: impl Into<String>,
        approvals: Arc<BashApprovals>,
    ) -> Self {
        self.approvals = Some((session_id.into(), approvals));
        self
    }

    /// Runs commands matching the excluded list outside the sandbox.
    pub fn with_excluded_commands(mut self, excluded: Arc<ExcludedCommands>) -> Self {
        self.excluded = Some(excluded);
        self
    }

    /// Tells the model that `HOME` is a private sandbox directory.
    pub fn with_private_home(mut self) -> Self {
        self.private_home = true;
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
            report_denials: true,
            stdin: None,
        }
    }

    /// Bounded read-only payload snapshots become part of the exact prepared
    /// version, so a changed script/build recipe invalidates authorization.
    fn review_facts(&self, command: &str) -> serde_json::Value {
        let parsed = shell::parse(command);
        let (cwd, segments) = match self.review_command_target(&parsed.command) {
            Ok(target) => target,
            Err(()) => return json!({"operation_unavailable":true,"complex_shell":true,"files":[]}),
        };
        let mut paths = Vec::<String>::new();
        let workspace = Workspace::new(&self.workspace_root);
        for (index, word) in segments.iter().flat_map(|words| words.iter().enumerate()) {
            let name = Path::new(word)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("");
            let command_word = index == 0;
            match name {
                "make" | "gmake" if command_word => {
                    let recipe = ["GNUmakefile", "makefile", "Makefile"]
                        .into_iter()
                        .find(|path| {
                            workspace.as_ref().is_ok_and(|workspace| {
                                workspace.open(&cwd.join(path)).is_ok_and(|file| {
                                    file.metadata().is_ok_and(|metadata| metadata.is_file())
                                })
                            })
                        })
                        .unwrap_or("Makefile");
                    paths.push(recipe.into());
                }
                "cargo" if command_word => paths.extend(["Cargo.toml".into(), "build.rs".into()]),
                "npm" | "pnpm" | "yarn" | "bun" if command_word => {
                    paths.push("package.json".into())
                }
                "git" if command_word => paths.extend([".git/HEAD".into(), ".git/config".into()]),
                _ => {}
            }
            if (command_word && word.starts_with("./"))
                || ["sh", "py", "js", "mjs", "cjs", "rb", "pl"]
                    .iter()
                    .any(|extension| word.ends_with(&format!(".{extension}")))
            {
                paths.push(word.clone());
            }
        }
        paths.sort();
        paths.dedup();
        let required_facts_truncated = paths.len() > 12;
        let mut total = 0usize;
        let files: Vec<_> = paths
            .into_iter()
            .take(12)
            .map(|path| {
                let read = (|| -> std::io::Result<Vec<u8>> {
                    let file = workspace
                        .as_ref()
                        .map_err(|_| std::io::Error::other("workspace unavailable"))?
                        .open(&cwd.join(&path))?;
                    if !file.metadata()?.is_file() {
                        return Err(std::io::Error::other("not a regular file"));
                    }
                    let limit = (24 * 1024usize).saturating_sub(total).min(12 * 1024);
                    let mut bytes = Vec::new();
                    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
                    if bytes.len() > limit {
                        return Err(std::io::Error::other("payload exceeds limit"));
                    }
                    total += bytes.len();
                    Ok(bytes)
                })();
                match read {
                    Ok(bytes) => {
                        let body = String::from_utf8_lossy(&bytes);
                        match self.redact_review_fact(&body) {
                            Some(body) => json!({"path":path,"sha256":format!("{:x}",sha2::Sha256::digest(&bytes)),"body":body}),
                            None => json!({"path":path,"unavailable":true}),
                        }
                    }
                    Err(error) => json!({"path":path,"unavailable":true,"optional": path == "build.rs" && error.kind() == std::io::ErrorKind::NotFound}),
                }
            })
            .collect();
        let mut target_unavailable = parsed.write_targets.iter().any(|target| !target.is_static);
        let targets: Vec<_> = parsed.write_targets.iter()
            .map(|target| {
                let path = cwd.join(&target.path);
                let resolved = workspace.as_ref().ok().and_then(|workspace| workspace.resolve_for_write(&path).ok())
                    .or_else(|| path.canonicalize().ok())
                    .or_else(|| Some(path.parent()?.canonicalize().ok()?.join(path.file_name()?)));
                match resolved {
                    Some(resolved) if target.is_static && !target.path.contains(['$', '`', '*', '?', '~']) => {
                        let metadata = std::fs::metadata(&resolved).ok();
                        json!({"path":target.path,"resolved":resolved,"exists":metadata.is_some(),
                            "regular_file":metadata.as_ref().is_some_and(|metadata| metadata.is_file()),
                            "outside_workspace":!resolved.starts_with(&self.workspace_root)})
                    },
                    _ => { target_unavailable = true; json!({"path":target.path,"unavailable":true}) },
                }
            }).collect();
        json!({"cwd":cwd,"cwd_semantics":"initial literal cwd; the complete shell program remains untrusted operation data",
            "files":files,"targets":targets,"required_facts_truncated":required_facts_truncated,"target_unavailable":target_unavailable,
            "complex_shell":!parsed.command.complete,
            "scope":"read-only workspace payload snapshots; unavailable or indirect effects require human review"})
    }

    fn redact_review_fact(&self, body: &str) -> Option<String> {
        let mut secrets = self.redact_values.clone();
        for token in body
            .split_whitespace()
            .filter(|token| token.contains("://"))
        {
            let (values, incomplete) = crate::urlprivacy::userinfo_forms(token.as_bytes());
            if incomplete {
                return None;
            }
            secrets.extend(values);
        }
        let marker = dynamic_redaction_marker(&secrets)?;
        Some(redact_exact_text(body, &secrets, &marker))
    }

    /// Directory and remaining commands for the review facts. A first
    /// `cd <dir>` sets the directory; any other `cd`, `pushd` or `popd`, or a
    /// `cd` whose arguments are not all static, makes the target unavailable.
    fn review_command_target(
        &self,
        analysis: &otto_core::approval::ShellCommand,
    ) -> Result<(PathBuf, Vec<Vec<String>>), ()> {
        let mut cwd = self.workspace_root.clone();
        let mut segments = analysis.segments.as_slice();
        let first = segments.first().ok_or(())?;
        if shell::is_program(&first[0], "cd") {
            if !analysis.complete || first.len() != 2 {
                return Err(());
            }
            cwd = cwd.join(&first[1]);
            segments = &segments[1..];
        }
        let words = segments.first().ok_or(())?;
        if segments.iter().any(|words| {
            ["cd", "pushd", "popd"]
                .iter()
                .any(|name| shell::is_program(&words[0], name))
        }) {
            return Err(());
        }
        let git = shell::is_program(&words[0], "git");
        if git || shell::is_program(&words[0], "make") || shell::is_program(&words[0], "gmake") {
            let end = if git {
                shell::git_subcommand(words).map_or(words.len(), |(index, _)| index)
            } else {
                words.len()
            };
            let mut index = 1;
            while index < end {
                if words[index] == "-C" {
                    cwd = cwd.join(words.get(index + 1).ok_or(())?);
                    index += 2;
                } else if let Some(path) = words[index].strip_prefix("-C") {
                    cwd = cwd.join(path);
                    index += 1;
                } else {
                    index += 1;
                }
            }
        }
        if cwd.to_string_lossy().contains(['$', '`']) {
            return Err(());
        }
        let workspace = Workspace::new(&self.workspace_root).map_err(|_| ())?;
        let cwd = workspace.resolve_existing(&cwd).map_err(|_| ())?;
        if !cwd.is_dir() {
            return Err(());
        }
        Ok((cwd, segments.to_vec()))
    }

    /// Resolve only literal git cwd syntax. Ambiguous shell/config overrides
    /// never borrow branch or diff facts from the original workspace.
    fn git_review_target(&self, command: &str) -> Result<Option<PathBuf>, ()> {
        let parsed = shell::parse(command);
        if parsed.assigns_variables {
            return Err(());
        }
        let (cwd, segments) = self.review_command_target(&parsed.command)?;
        if !shell::is_program(&segments[0][0], "git") {
            return Ok(None);
        }
        if segments.iter().flatten().any(|arg| {
            arg == "-c" || arg.starts_with("--git-dir") || arg.starts_with("--work-tree")
        }) {
            return Err(());
        }
        Ok(Some(cwd))
    }

    /// First push has no upstream. Use only an existing remote-tracking base
    /// and a verified common ancestor; never fabricate an empty outgoing diff.
    async fn first_push_diff(
        &self,
        target: &Path,
        command: &str,
        deadline: tokio::time::Instant,
        cancel: &CancellationToken,
    ) -> Option<(String, serde_json::Value)> {
        let analysis = shell::analyze(command);
        let (words, push) = analysis.segments.iter().find_map(|words| {
            let (index, subcommand) = shell::git_subcommand(words)?;
            (subcommand == "push").then_some((words, index))
        })?;
        let remote = words[push + 1..]
            .iter()
            .find(|word| !word.starts_with('-'))?;
        if !remote
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
        {
            return None;
        }
        for name in ["HEAD", "main", "master"] {
            let reference = format!("refs/remotes/{remote}/{name}");
            let commit_ref = format!("{reference}^{{commit}}");
            let Some(base) = crate::runtime::workspace_context::run_git(
                &self.executor,
                &target.to_string_lossy(),
                &self.environment,
                &["rev-parse", "--verify", &commit_ref],
                deadline,
                cancel,
            )
            .await
            else {
                continue;
            };
            let base = base.trim();
            if !matches!(base.len(), 40 | 64) || !base.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                continue;
            }
            let Some(ancestor) = crate::runtime::workspace_context::run_git(
                &self.executor,
                &target.to_string_lossy(),
                &self.environment,
                &["merge-base", base, "HEAD"],
                deadline,
                cancel,
            )
            .await
            else {
                continue;
            };
            let ancestor = ancestor.trim();
            if !matches!(ancestor.len(), 40 | 64)
                || !ancestor.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                continue;
            }
            let Some(diff) = crate::runtime::workspace_context::run_git(
                &self.executor,
                &target.to_string_lossy(),
                &self.environment,
                &[
                    "--no-pager",
                    "diff",
                    "--no-ext-diff",
                    "--no-textconv",
                    ancestor,
                    "HEAD",
                ],
                deadline,
                cancel,
            )
            .await
            else {
                continue;
            };
            return Some((
                self.redact_review_fact(&diff)?,
                json!({"reference":reference,"commit":base,"merge_base":ancestor}),
            ));
        }
        None
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
        let execute = executor.execute_with_grace(
            self.request(command, environment),
            streams,
            &child,
            self.cancellation_grace,
        );
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
            return ToolResult::default().cancelled_not_started();
        }
        let mut stdout = std::io::sink();
        let mut stderr = std::io::sink();
        let (_, outcome, timed_out) = self
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
            Ok(()) => ToolResult::default(),
            Err(Error::Cancelled) if cancel.is_cancelled() => {
                ToolResult::default().cancelled_unknown()
            }
            Err(Error::Cancelled) if timed_out => ToolResult::default().deadline_unknown(),
            Err(Error::Cancelled) => ToolResult::default().cancelled_unknown(),
            Err(_) => ToolResult {
                is_error: true,
                ..ToolResult::default()
            }
            .transport_unknown(),
        }
    }

    fn argument_error(&self, message: &str) -> ToolResult {
        if !self.dynamic_content {
            return ToolResult {
                is_error: true,
                ..ToolResult::default()
            }
            .not_started();
        }
        ToolResult {
            content: redact_exact_text(message, &self.redact_values, &self.redaction_marker),
            is_error: true,
            ..ToolResult::default()
        }
        .not_started()
    }

    fn result(&self, stdout: &str, stderr: &str, status: &ExitStatus, summary: &str) -> ToolResult {
        let mut summary = summary.to_owned();
        if status.signaled && !status.signal.is_empty() {
            summary.push_str("; signal: ");
            summary.push_str(&status.signal);
        }
        let mut denied = String::new();
        if !status.denials.is_empty() {
            denied.push_str("sandbox_denied:\n");
            for denial in &status.denials {
                denied.push_str(&format!("{} {}\n", denial.operation, denial.target));
            }
            if status.denials_omitted > 0 {
                denied.push_str(&format!("[{} more omitted]\n", status.denials_omitted));
            }
        }
        let formatted = format!("{}\n{}\n{denied}{summary}", stdout, stderr);
        ToolResult {
            content: redact_exact_text(&formatted, &self.redact_values, &self.redaction_marker),
            ..ToolResult::default()
        }
    }
}

fn infrastructure_result(error: &Error) -> ToolResult {
    let result = ToolResult {
        content: SANDBOX_EXECUTION_UNAVAILABLE.to_owned(),
        is_error: true,
        ..ToolResult::default()
    };
    match error {
        Error::Closed
        | Error::InvalidRequest
        | Error::UnsupportedPolicy
        | Error::Unavailable(_)
        | Error::EnvironmentUnsafe
        | Error::ChildLaunch => result.not_started(),
        Error::ChildWait | Error::ChildTerminate | Error::Cancelled => result.transport_unknown(),
    }
}

/// The output contract shared by sandboxed and elevated calls.
const OUTPUT_NOTE: &str = "Return exit status and stdout/stderr previews of at most 4 KiB each (or the smaller configured cap); longer redacted output is saved temporarily under .otto/tool-output and automatically removed after the turn and its children finish. Delete individual logs after analysis when no task still needs them; never remove the shared directory. Analyze logs with scripts and print only verified summaries or relevant failures; use read with offset/limit for excerpts instead of printing complete logs.";

/// Appended when the child's `HOME` is a private sandbox directory.
const PRIVATE_HOME_NOTE: &str = "HOME and ~ are a private sandbox directory, not the user's home directory; use absolute paths for the user's files.";

fn describe(base: &str, private_home: bool) -> String {
    let base = base.trim_end_matches('.');
    let mut description = format!("{base}. {OUTPUT_NOTE}");
    if private_home {
        description.push(' ');
        description.push_str(PRIVATE_HOME_NOTE);
    }
    description
}

/// The schema advertised for `bash`.
pub fn bash_definition(private_home: bool) -> ToolDefinition {
    definition(
        "bash",
        &describe("Execute a shell command from the workspace", private_home),
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Shell command to execute; for verbose jobs prefer quiet flags or a script that captures logs, preserves the command exit status, and prints a concise checked summary"
                }
            },
            "required": ["command"]
        }),
    )
}

pub fn bash_definition_with_approvals(private_home: bool) -> ToolDefinition {
    definition(
        "bash",
        &describe(
            "Execute a shell command from the workspace. Set sandbox_permissions to require_escalated only when sandboxed execution cannot complete the authorized task; include a justification. AUTO reviews the exact command and bounded native script/build facts against original user evidence; missing or changed payloads require human review. Inside the sandbox, a command that changes external state (git push, gh writes, curl or wget uploads, package publish, kubectl/helm/terraform changes) or deletes workspace or repository content (git reset --hard, git clean -f, rm -r, find -delete) waits for the same approval without a justification, then runs in the sandbox. To request persistent sandbox read access, set sandbox_read_path to one absolute existing path and include a justification; keep sandbox_permissions as use_default. Persistent security grants require a person in AUTO. The command runs only after authority is recorded; MANUAL asks, NEVER rejects additional authority, and YOLO bypasses review while retaining hard checks.",
            private_home,
        ),
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Shell command to execute; for verbose jobs prefer quiet flags or a script that captures logs, preserves the command exit status, and prints a concise checked summary"
                },
                "sandbox_permissions": {
                    "type": "string",
                    "enum": ["use_default", "require_escalated"],
                    "description": "Use require_escalated for one exact unsandboxed command; the backend records authority through AUTO review, an explicit rule or the user's decision before dispatch"
                },
                "sandbox_read_path": {
                    "type": "string",
                    "description": "Request user approval to permanently add one absolute existing path to sandbox read_paths before running this command inside the sandbox"
                },
                "justification": {
                    "type": "string",
                    "description": "Why the requested permission is required: one short sentence, under 80 characters"
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
            bash_definition_with_approvals(self.private_home)
        } else {
            bash_definition(self.private_home)
        }
    }

    fn approval_spec(
        &self,
        arguments: &RawValue,
    ) -> Result<Option<super::ApprovalSpec>, ToolResult> {
        let args: BashArgs = decode_strict_json(arguments.get(), &["command"])
            .map_err(|e| self.argument_error(&e))?;
        let excluded = self
            .excluded
            .as_ref()
            .is_some_and(|excluded| excluded.matches(&args.command));
        let gate = self.effect_gate(&args, excluded);
        if args.sandbox_read_path.is_empty()
            && (args.sandbox_permissions == SandboxPermissions::UseDefault || excluded)
            && gate.is_none()
        {
            return Ok(None);
        }
        if !self.dynamic_content
            || self.approvals.is_none()
            || args.command.trim().is_empty()
            || (gate.is_none() && args.justification.trim().is_empty())
        {
            return Err(
                self.argument_error("approval is unavailable or command/justification is missing")
            );
        }
        let read_path = if args.sandbox_read_path.is_empty() {
            String::new()
        } else {
            if args.sandbox_permissions != SandboxPermissions::UseDefault
                || self.grant_control.is_none()
            {
                return Err(self.argument_error("persistent sandbox read approval is unavailable"));
            }
            let path = Path::new(&args.sandbox_read_path);
            if !path.is_absolute() {
                return Err(self.argument_error("read path must be absolute"));
            }
            path.canonicalize()
                .map_err(|_| self.argument_error("read path is unavailable"))?
                .to_str()
                .ok_or_else(|| self.argument_error("read path must be UTF-8"))?
                .to_string()
        };
        let config_version = if let Some((config, _)) = &self.grant_control {
            let bytes = match std::fs::read(config) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => vec![],
                Err(_) => return Err(self.argument_error("configuration is unavailable")),
            };
            format!("{:x}", sha2::Sha256::digest(&bytes))
        } else {
            String::new()
        };
        let facts = self.review_facts(&args.command);
        let version=serde_json::json!({"arguments":crate::approval::digest(arguments.get()),"config":config_version,"path":read_path,"facts":crate::approval::digest(&facts.to_string())}).to_string();
        // One escalation leaves the sandbox for a single command; a read path
        // is a persistent grant outside the workspace; a gated command runs
        // in the sandbox after one approval.
        let (authority, title) = match &gate {
            Some((shell::Effect::ExternalState, _)) => (
                crate::approval::Authority::Boundary,
                "Run a command that changes external state?",
            ),
            Some((shell::Effect::Destructive, _)) => (
                crate::approval::Authority::Bounded,
                "Run a destructive workspace command?",
            ),
            None if read_path.is_empty() => (
                crate::approval::Authority::Boundary,
                "Run an unsandboxed command?",
            ),
            None => (
                crate::approval::Authority::Consent,
                "Save persistent sandbox read access?",
            ),
        };
        let mut details = serde_json::json!({"command":args.command,"workspace":self.workspace_root,"read_path":read_path,"native_facts":facts});
        let mut reason = args.justification;
        if let Some((effect, segment)) = &gate {
            details["sandboxed"] = true.into();
            details["effect"] = serde_json::json!({"class":effect.as_str(),"segment":segment});
            if reason.trim().is_empty() {
                reason = match effect {
                    shell::Effect::ExternalState => {
                        format!("Changes external state: {}", segment.join(" "))
                    }
                    shell::Effect::Destructive => {
                        format!(
                            "Deletes workspace or repository content: {}",
                            segment.join(" ")
                        )
                    }
                };
            }
        }
        Ok(Some(super::ApprovalSpec {
            version,
            source: "sandbox".into(),
            authority,
            title: title.into(),
            reason,
            persistent: !read_path.is_empty(),
            details: details.to_string(),
        }))
    }

    async fn approval_spec_async(
        &self,
        arguments: &RawValue,
        cancel: &CancellationToken,
    ) -> Result<Option<super::ApprovalSpec>, ToolResult> {
        let Some(mut spec) = self.approval_spec(arguments)? else {
            return Ok(None);
        };
        let args: BashArgs = decode_strict_json(arguments.get(), &["command"])
            .map_err(|error| self.argument_error(&error))?;
        let analysis = shell::analyze(&args.command);
        if analysis
            .segments
            .iter()
            .flatten()
            .any(|word| shell::is_program(word, "git"))
        {
            let target = match self.git_review_target(&args.command) {
                Ok(Some(target)) => target,
                _ => {
                    let mut details: serde_json::Value = serde_json::from_str(&spec.details)
                        .map_err(|_| self.argument_error("invalid prepared Bash details"))?;
                    details["native_facts"]["git"]["target"] = json!({"unavailable":true});
                    spec.details = details.to_string();
                    spec.version =
                        crate::approval::digest(&format!("{}:{}", spec.version, spec.details));
                    return Ok(Some(spec));
                }
            };
            let child = cancel.child_token();
            let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
            let commands: [(&str, &[&str]); 4] = [
                ("branch", &["rev-parse", "--abbrev-ref", "HEAD"]),
                ("remotes", &["remote", "-v"]),
                (
                    "working_diff",
                    &[
                        "--no-pager",
                        "diff",
                        "--no-ext-diff",
                        "--no-textconv",
                        "HEAD",
                    ],
                ),
                (
                    "outgoing_diff",
                    &[
                        "--no-pager",
                        "diff",
                        "--no-ext-diff",
                        "--no-textconv",
                        "@{upstream}",
                        "HEAD",
                    ],
                ),
            ];
            let mut git = serde_json::Map::new();
            git.insert("cwd".into(), json!(target));
            for (name, argv) in commands {
                let body = crate::runtime::workspace_context::run_git(
                    &self.executor,
                    &target.to_string_lossy(),
                    &self.environment,
                    argv,
                    deadline,
                    &child,
                )
                .await;
                let body = body
                    .filter(|body| body.len() <= 12 * 1024)
                    .and_then(|body| self.redact_review_fact(&body));
                git.insert(
                    name.into(),
                    body.map_or(
                        json!({"unavailable":true}),
                        |body| json!({"body":body,"sha256":crate::approval::digest(&body)}),
                    ),
                );
            }
            if git["outgoing_diff"]["unavailable"] == true
                && let Some((body, base)) = self
                    .first_push_diff(&target, &args.command, deadline, &child)
                    .await
            {
                git.insert(
                    "outgoing_diff".into(),
                    json!({"sha256":crate::approval::digest(&body),"body":body}),
                );
                git.insert("chosen_base".into(), base);
            }
            let mut details: serde_json::Value = serde_json::from_str(&spec.details)
                .map_err(|_| self.argument_error("invalid prepared Bash details"))?;
            // Worktree gitfiles make .git/HEAD and .git/config unavailable;
            // the native sandboxed git queries supply those canonical facts.
            if git["branch"]["unavailable"] != true
                && git["remotes"]["unavailable"] != true
                && let Some(files) = details
                    .pointer_mut("/native_facts/files")
                    .and_then(|files| files.as_array_mut())
            {
                files.retain(|file| {
                    !file["path"]
                        .as_str()
                        .is_some_and(|path| path.starts_with(".git/"))
                });
            }
            details["native_facts"]["git"] = serde_json::Value::Object(git);
            spec.details = details.to_string();
            spec.version = crate::approval::digest(&format!("{}:{}", spec.version, spec.details));
        }
        Ok(Some(spec))
    }

    async fn apply_approval(
        &self,
        arguments: &RawValue,
        cancel: &CancellationToken,
        request: &crate::approval::Request,
    ) -> Result<(), String> {
        let args: BashArgs =
            serde_json::from_str(arguments.get()).map_err(|_| "invalid Bash arguments")?;
        self.approvals.as_ref().ok_or("approval unavailable")?;
        if cancel.is_cancelled() {
            return Err("approval operation cancelled".into());
        }
        if !args.sandbox_read_path.is_empty() {
            let path = Path::new(&args.sandbox_read_path)
                .canonicalize()
                .map_err(|_| "read path unavailable")?;
            let path = path.to_str().ok_or("read path is not UTF-8")?;
            let (config, control) = self
                .grant_control
                .as_ref()
                .ok_or("sandbox reload unavailable")?;
            let expected: serde_json::Value = serde_json::from_str(&request.subject.version)
                .map_err(|_| "invalid authorization revision")?;
            if expected["path"].as_str() != Some(path) {
                return Err("read path changed after approval".into());
            }
            let amendment = crate::runtime::sandbox_config::amend_sandbox_config_checked(
                config,
                &crate::runtime::sandbox_config::SandboxChange::AllowReadPath(path.into()),
                expected["config"].as_str(),
            )?;
            if let Err(error) = control.reload().await {
                let rollback =
                    crate::runtime::sandbox_config::revert_sandbox_config(config, &amendment);
                return Err(format!("{error}; rollback: {rollback:?}"));
            }
        }
        Ok(())
    }

    async fn execute(&self, arguments: &RawValue, cancel: &CancellationToken) -> ToolResult {
        self.execute_inner(arguments, cancel, false).await
    }
    async fn execute_approved(
        &self,
        call: otto_core::tool::ToolCall<'_>,
        cancel: &CancellationToken,
        _request: &crate::approval::Request,
    ) -> ToolResult {
        self.execute_inner(call.arguments, cancel, true).await
    }
}

impl BashTool {
    /// The effect class and command words when this call runs in the sandbox
    /// but needs approval: approvals are available, the command is not
    /// excluded and it changes external state or deletes content.
    fn effect_gate(&self, args: &BashArgs, excluded: bool) -> Option<(shell::Effect, Vec<String>)> {
        if args.sandbox_permissions != SandboxPermissions::UseDefault
            || !args.sandbox_read_path.is_empty()
            || excluded
            || !self.dynamic_content
            || self.approvals.is_none()
        {
            return None;
        }
        let parsed = shell::parse(&args.command);
        shell::classify(&parsed).map(|(effect, segment)| (effect, segment.to_vec()))
    }

    async fn execute_inner(
        &self,
        arguments: &RawValue,
        cancel: &CancellationToken,
        authorized: bool,
    ) -> ToolResult {
        let args: BashArgs = match decode_strict_json(arguments.get(), &["command"]) {
            Ok(args) => args,
            Err(message) => return self.argument_error(&message),
        };
        if args.command.trim().is_empty() {
            return self.argument_error("missing required argument: command");
        }
        if !args.sandbox_read_path.is_empty() {
            if args.sandbox_permissions != SandboxPermissions::UseDefault {
                return self.argument_error(
                    "sandbox_read_path cannot be combined with elevated execution",
                );
            }
            if args.justification.trim().is_empty() {
                return self.argument_error("justification is required for sandbox read access");
            }
            if !self.dynamic_content || cancel.is_cancelled() {
                return self.argument_error("sandbox read approval is unavailable or cancelled");
            }
            let Some((session_id, approvals)) = &self.approvals else {
                return self.argument_error("sandbox read approval is unavailable");
            };
            let path = Path::new(&args.sandbox_read_path);
            if !path.is_absolute() {
                return self.argument_error("sandbox_read_path must be an absolute existing path");
            }
            let Ok(path) = path.canonicalize() else {
                return self.argument_error("sandbox_read_path must be an absolute existing path");
            };
            let Some(path) = path.to_str() else {
                return self.argument_error("sandbox_read_path must be valid UTF-8");
            };
            if !authorized && !approvals.take_read_path(session_id, &args.command, path) {
                let id = approvals.request_read_path(session_id, &args.command, path);
                let command = serde_json::to_string(&args.command).expect("string encodes");
                let justification =
                    serde_json::to_string(&args.justification).expect("string encodes");
                let path = serde_json::to_string(path).expect("string encodes");
                return self.argument_error(&format!(
                    "approval required for persistent sandbox read access.\n\
Approve in Otto: /approve {id}\n\
Command: {command}\n\
Justification: {justification}\n\
Read path: {path}\n\
Approval saves the path to read_paths and reloads the sandbox. The command did not run. \
Only the user or their session approval mode can approve; do not run /approve in a shell. After approval retry this exact Bash call inside the sandbox."
                ));
            }
        }
        if !self.dynamic_content {
            return self.execute_suppressed(&args.command, cancel).await;
        }
        // An excluded command never asks for approval and never consumes one.
        let excluded = self.excluded.as_ref().filter(|excluded| {
            args.sandbox_read_path.is_empty() && excluded.matches(&args.command)
        });
        let (executor, environment) = match (excluded, args.sandbox_permissions) {
            (Some(excluded), _) => (&excluded.executor, excluded.environment.as_slice()),
            (None, SandboxPermissions::UseDefault) => {
                if !authorized && let Some((effect, segment)) = self.effect_gate(&args, false) {
                    let what = match effect {
                        shell::Effect::ExternalState => "changes external state",
                        shell::Effect::Destructive => "deletes workspace or repository content",
                    };
                    let segment =
                        serde_json::to_string(&segment.join(" ")).expect("string encodes");
                    return self.argument_error(&format!(
                        "approval required: this command {what} ({segment}).\n\
The command did not run. This tool call has no approval flow; ask the user to run the command or to approve it in a session with approvals."
                    ));
                }
                (&self.executor, self.environment.as_slice())
            }
            (None, SandboxPermissions::RequireEscalated) => {
                if args.justification.trim().is_empty() {
                    return self.argument_error("justification is required for elevated execution");
                }
                let Some((session_id, approvals)) = &self.approvals else {
                    return self.argument_error("elevated execution is unavailable");
                };
                if !authorized && !approvals.take(session_id, &args.command) {
                    let id = approvals.request(session_id, &args.command);
                    let command = serde_json::to_string(&args.command).expect("string encodes");
                    let justification =
                        serde_json::to_string(&args.justification).expect("string encodes");
                    return self.argument_error(&format!(
                        "approval required for unsandboxed bash execution.\n\
Approve in Otto: /approve {id}\n\
Command: {command}\n\
Justification: {justification}\n\
The command did not run. Only the user or their session approval mode can approve it in Otto; do not run \
/approve in a shell. Ask the user to review the command above, then retry \
this exact elevated Bash command after approval."
                    ));
                }
                (&approvals.executor, approvals.environment.as_slice())
            }
        };
        if cancel.is_cancelled() {
            return self
                .result(
                    "stdout:\n",
                    "stderr:\n",
                    &ExitStatus::default(),
                    "status: cancelled",
                )
                .cancelled_not_started();
        }

        let mut stdout = RedactingCollector::with_collector(
            Output::with_logs(
                Arc::clone(&self.output_root),
                self.max_output_bytes.min(PREVIEW_BYTES),
                Arc::clone(&self.output_logs),
            ),
            &self.redact_values,
            &self.redaction_marker,
        );
        let mut stderr = RedactingCollector::with_collector(
            Output::with_logs(
                Arc::clone(&self.output_root),
                self.max_output_bytes.min(PREVIEW_BYTES),
                Arc::clone(&self.output_logs),
            ),
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
        if let Err(ref error) = outcome
            && *error != Error::Cancelled
        {
            return infrastructure_result(error);
        }
        stdout.flush();
        stderr.flush();
        stdout.collector_mut().finish();
        stderr.collector_mut().finish();

        let summary = if cancel.is_cancelled() {
            "status: cancelled".to_owned()
        } else if timed_out {
            format!("status: timed out after {}", go_duration(self.timeout))
        } else if outcome.is_err() {
            "status: cancelled".to_owned()
        } else {
            format!("exit_code: {}", status.code)
        };
        let result = self.result(
            &stdout.collector().render("stdout"),
            &stderr.collector().render("stderr"),
            &status,
            &summary,
        );
        if cancel.is_cancelled() {
            result.cancelled_unknown()
        } else if timed_out {
            result.deadline_unknown()
        } else if outcome.is_err() {
            result.cancelled_unknown()
        } else {
            result
        }
    }
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
The command did not run. Only the user or their session approval mode can approve it in Otto; do not run /approve in a shell. Ask the user to review the command above, then retry this exact elevated Bash command after approval."
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
The command did not run. Only the user or their session approval mode can approve it in Otto; do not run /approve in a shell. Ask the user to review the command above, then retry this exact elevated Bash command after approval."
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
        let tool = bash(&workspace, executor.clone(), &[], 4096, &[])
            .with_approvals("s1", approvals.clone());
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
        let tool = bash(workspace, sandboxed.clone(), &[], 1024, &[])
            .with_approvals("s1", approvals.clone());
        (tool, approvals)
    }

    fn use_default_spec(tool: &BashTool, command: &str) -> Option<super::super::ApprovalSpec> {
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
        assert_eq!(spec.authority, crate::approval::Authority::Boundary);
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
        assert_eq!(spec.authority, crate::approval::Authority::Bounded);
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
    fn effect_gate_needs_approvals_and_skips_excluded_commands() {
        let (_dir, workspace) = temp_workspace();
        let sandboxed = Arc::new(FakeExecutor::default());
        let plain = bash(&workspace, sandboxed.clone(), &[], 1024, &[]);
        assert!(use_default_spec(&plain, "git push").is_none());

        let list = excluded(&sandboxed, &["git push *"]);
        let (tool, _approvals) = gated_tool(&workspace, &sandboxed, &sandboxed);
        let tool = tool.with_excluded_commands(list);
        assert!(use_default_spec(&tool, "git push origin").is_none());
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
    async fn the_private_home_note_is_advertised_only_when_requested() {
        let note = "HOME and ~ are a private sandbox directory, not the user's home directory; use absolute paths for the user's files.";
        for (plain, private) in [
            (bash_definition(false), bash_definition(true)),
            (
                bash_definition_with_approvals(false),
                bash_definition_with_approvals(true),
            ),
        ] {
            assert!(!plain.description.contains("HOME"));
            assert!(
                private.description.ends_with(note),
                "{}",
                private.description
            );
            assert!(
                private
                    .description
                    .starts_with(&plain.description.trim_end_matches('.').to_owned())
            );
        }
        let (_dir, workspace) = temp_workspace();
        let tool = bash(&workspace, Arc::new(FakeExecutor::default()), &[], 1, &[]);
        assert!(!tool.definition().description.contains(note));
        assert!(
            tool.with_private_home()
                .definition()
                .description
                .ends_with(note)
        );
    }

    #[tokio::test]
    async fn the_definition_is_the_shared_schema() {
        let (_dir, workspace) = temp_workspace();
        let tool = bash(&workspace, Arc::new(FakeExecutor::default()), &[], 1, &[]);
        assert_eq!(
            serde_json::to_value(tool.definition()).unwrap(),
            serde_json::to_value(bash_definition(false)).unwrap()
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
                report_denials: true,
                stdin: None,
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
                    ..Default::default()
                },
            ),
        );
        let tool = bash(&workspace, executor, &[], 12, &[]);
        let result = run(&tool, "ignored").await;
        assert!(!result.is_error);
        assert!(
            result
                .content
                .starts_with("stdout:\n123456\n[truncated: 4 bytes omitted]\nabcdef")
        );
        assert!(
            result
                .content
                .contains("stderr:\nabcdef\n[truncated: 4 bytes omitted]\nklmnop")
        );
        assert!(result.content.ends_with("exit_code: 7"));
        let paths: Vec<_> = result
            .content
            .lines()
            .filter_map(|line| line.strip_prefix("full_output: "))
            .collect();
        assert_eq!(paths.len(), 2);
        assert_eq!(
            std::fs::read_to_string(workspace.root().join(paths[0])).unwrap(),
            "1234567890abcdef"
        );
        assert_eq!(
            std::fs::read_to_string(workspace.root().join(paths[1])).unwrap(),
            "abcdefghijklmnop"
        );
    }

    #[tokio::test]
    async fn default_preview_caps_large_output_and_read_can_retrieve_an_omitted_range() {
        let (_dir, workspace) = temp_workspace();
        let text = format!(
            "{}failure detail in the middle\n{}final summary\n",
            "ok\n".repeat(2000),
            "ok\n".repeat(2000)
        );
        let executor = Arc::new(FakeExecutor::chunks(&[&text], &[]).with_status(ExitStatus {
            code: 1,
            ..Default::default()
        }));
        let tool = bash(&workspace, executor, &[], 51200, &[]);
        let result = run(&tool, "test").await;
        assert!(result.content.len() < 4600);
        assert!(!result.content.contains("failure detail in the middle"));
        assert!(result.content.contains("final summary"));
        assert!(result.content.ends_with("exit_code: 1"));
        let path = result
            .content
            .lines()
            .find_map(|line| line.strip_prefix("full_output: "))
            .unwrap();
        let read = crate::tool::read::ReadTool::new(&workspace, 51200);
        let args =
            serde_json::value::to_raw_value(&json!({"path":path,"offset":2001,"limit":1})).unwrap();
        let excerpt = read.execute(&args, &CancellationToken::new()).await;
        assert!(!excerpt.is_error);
        assert_eq!(excerpt.content, "failure detail in the middle\n");
    }

    #[tokio::test(start_paused = true)]
    async fn timeout_keeps_partial_overflow_log_and_execution_status() {
        let (_dir, workspace) = temp_workspace();
        let executor = Arc::new(FakeExecutor::behaving(Box::new(|_, streams, cancel| {
            Box::pin(async move {
                streams
                    .stdout
                    .write_all(b"begin lots of partial output end")
                    .unwrap();
                cancel.cancelled().await;
                (
                    ExitStatus {
                        code: -1,
                        signaled: true,
                        signal: "killed".into(),
                        ..Default::default()
                    },
                    Err(Error::Cancelled),
                )
            })
        })));
        let tool = bash(&workspace, executor, &[], 8, &[]);
        let result = run(&tool, "test").await;
        assert!(
            result
                .content
                .contains("status: timed out after 3s; signal: killed")
        );
        assert_eq!(
            outcome(&result).disposition,
            otto_core::model::OperationDisposition::DeadlineExceeded
        );
        let path = result
            .content
            .lines()
            .find_map(|line| line.strip_prefix("full_output: "))
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(workspace.root().join(path)).unwrap(),
            "begin lots of partial output end"
        );
    }

    #[tokio::test]
    async fn a_signaled_exit_appends_the_signal() {
        let (_dir, workspace) = temp_workspace();
        let executor = Arc::new(FakeExecutor::default().with_status(ExitStatus {
            code: -1,
            signaled: true,
            signal: "killed".to_owned(),
            ..Default::default()
        }));
        let tool = bash(&workspace, executor, &[], 1024, &[]);
        let result = run(&tool, "ignored").await;
        assert!(!result.is_error);
        assert_eq!(
            result.content,
            "stdout:\n\nstderr:\n\nexit_code: -1; signal: killed"
        );
    }

    fn denied_status(count: usize, omitted: usize, target: &str) -> ExitStatus {
        ExitStatus {
            code: 1,
            denials: (0..count)
                .map(|i| crate::sandbox::Denial {
                    operation: "file-read-data".to_owned(),
                    target: format!("{target}{i}"),
                })
                .collect(),
            denials_omitted: omitted,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn denials_render_between_stderr_and_the_summary() {
        let (_dir, workspace) = temp_workspace();
        let executor = Arc::new(FakeExecutor::default().with_status(denied_status(2, 0, "/x/")));
        let tool = bash(&workspace, executor, &[], 1024, &[]);
        let result = run(&tool, "ignored").await;
        assert_eq!(
            result.content,
            "stdout:\n\nstderr:\n\nsandbox_denied:\nfile-read-data /x/0\nfile-read-data /x/1\nexit_code: 1"
        );
    }

    #[tokio::test]
    async fn omitted_denials_are_counted() {
        let (_dir, workspace) = temp_workspace();
        let executor = Arc::new(FakeExecutor::default().with_status(denied_status(1, 3, "/x/")));
        let tool = bash(&workspace, executor, &[], 1024, &[]);
        let result = run(&tool, "ignored").await;
        assert!(
            result
                .content
                .contains("file-read-data /x/0\n[3 more omitted]\nexit_code: 1"),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn no_denials_render_no_section() {
        let (_dir, workspace) = temp_workspace();
        let executor = Arc::new(FakeExecutor::default().with_status(denied_status(0, 0, "")));
        let tool = bash(&workspace, executor, &[], 1024, &[]);
        let result = run(&tool, "ignored").await;
        assert_eq!(result.content, "stdout:\n\nstderr:\n\nexit_code: 1");
    }

    #[tokio::test]
    async fn denial_targets_are_redacted() {
        let (_dir, workspace) = temp_workspace();
        const SECRET: &str = "denied-secret-value";
        let executor = Arc::new(FakeExecutor::default().with_status(denied_status(1, 0, SECRET)));
        let tool = bash(&workspace, executor, &[], 1024, &[SECRET]);
        let result = run(&tool, "ignored").await;
        assert!(!result.content.contains(SECRET), "{result:?}");
        assert!(result.content.contains("sandbox_denied:"), "{result:?}");
    }

    #[tokio::test]
    async fn the_signal_text_is_redacted_too() {
        let (_dir, workspace) = temp_workspace();
        const SECRET: &str = "signal-secret-value";
        let executor = Arc::new(FakeExecutor::default().with_status(ExitStatus {
            code: -1,
            signaled: true,
            signal: format!("killed-{SECRET}"),
            ..Default::default()
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
                    ..Default::default()
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
        assert_eq!(stdout_body(&result.content), "pref");
        assert!(
            result.content.contains("[truncated: 2 bytes omitted]"),
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
            result.content.contains("[truncated: 8 bytes omitted]"),
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
                    ..Default::default()
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
            assert_eq!(
                outcome(&result).effect_certainty,
                otto_core::model::EffectCertainty::NotStarted
            );
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
        assert_eq!(
            outcome(&result).effect_certainty,
            otto_core::model::EffectCertainty::NotStarted
        );
        assert_eq!(
            outcome(&result).disposition,
            otto_core::model::OperationDisposition::Cancelled
        );
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
                            ..Default::default()
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
        assert_eq!(
            outcome(&result).effect_certainty,
            otto_core::model::EffectCertainty::Unknown
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
                        ..Default::default()
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
        assert_eq!(
            outcome(&result).disposition,
            otto_core::model::OperationDisposition::DeadlineExceeded
        );
        assert_eq!(
            outcome(&result).effect_certainty,
            otto_core::model::EffectCertainty::Unknown
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
                        ..Default::default()
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
                        ..Default::default()
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
            let expected = match error {
                Error::ChildWait | Error::ChildTerminate => {
                    otto_core::model::EffectCertainty::Unknown
                }
                _ => otto_core::model::EffectCertainty::NotStarted,
            };
            assert_eq!(outcome(&result).effect_certainty, expected);
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
                            ..Default::default()
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
                    // Register before publishing readiness: notify_waiters does not
                    // retain a wakeup for a future that has not registered yet.
                    let notified = release.notified();
                    tokio::pin!(notified);
                    notified.as_mut().enable();
                    arrived.add_permits(1);
                    notified.await;
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

    #[tokio::test]
    async fn a_driver_launch_failure_is_not_started_and_runs_nothing() {
        use crate::faults::sandbox::{FaultyDriver, SandboxOp};
        use crate::faults::{Fault, FaultError, FaultPlan, Rule, Trigger};

        let (_dir, workspace) = temp_workspace();
        let plan = FaultPlan::new(vec![Rule::new(
            SandboxOp::Execute,
            Trigger::Nth(1),
            Fault::Before(FaultError::Os(libc::EAGAIN)),
        )]);
        let executor = Executor::new(
            Arc::new(FaultyDriver::new(
                Arc::new(crate::sandbox::direct::DirectDriver::new()),
                plan.clone(),
            )),
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
            strings(&["PATH=/usr/bin:/bin"]),
            Duration::from_secs(3),
            4096,
            &[],
        )
        .expect("the configuration is valid");

        let result = run(&tool, "touch marker").await;

        assert!(result.is_error, "{result:?}");
        assert_eq!(result.content, SANDBOX_EXECUTION_UNAVAILABLE);
        assert_eq!(
            outcome(&result).effect_certainty,
            otto_core::model::EffectCertainty::NotStarted
        );
        assert!(!workspace.root().join("marker").exists());
        plan.assert_all_fired();
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

        let operation_id = otto_core::model::OperationId::new("op_test").unwrap();
        let arguments = raw(r#"{"command":"true"}"#);
        let result = otto_core::tool::ToolExecutor::execute(
            &registry,
            otto_core::tool::ToolCall {
                tool_call_id: "test",
                operation_id: &operation_id,
                name: "bash",
                arguments: &arguments,
                attempt: 1,
            },
            &CancellationToken::new(),
        )
        .await;
        assert!(!result.result.is_error, "{result:?}");
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
