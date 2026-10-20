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
            "Execute a shell command from the workspace. Set sandbox_permissions to require_escalated only when sandboxed execution cannot complete the authorized task; include a justification. AUTO reviews the exact command and bounded native script/build facts against original user evidence; missing or changed payloads require human review. A command that changes external state (git push, gh writes, curl or wget uploads, package publish, kubectl/helm/terraform changes) or deletes workspace or repository content (git reset --hard, git clean -f, rm -r, find -delete) waits for the same approval without a justification, then runs in the sandbox, or outside it when it matches [sandbox] excluded_commands. To request persistent sandbox read access, set sandbox_read_path to one absolute existing path and include a justification; keep sandbox_permissions as use_default. Persistent security grants require a person in AUTO. The command runs only after authority is recorded; MANUAL asks, NEVER rejects additional authority, and YOLO bypasses review while retaining hard checks.",
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

pub(crate) const BASH_RETRY: (&str, otto_core::model::RetrySafety) =
    ("bash", otto_core::model::RetrySafety::NonIdempotent);

#[async_trait::async_trait]
impl Tool for BashTool {
    fn retry_safety(&self) -> otto_core::model::RetrySafety {
        BASH_RETRY.1
    }

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
        // in the sandbox after one approval, and a gated excluded command
        // runs outside the sandbox after one approval.
        let (authority, title) = match &gate {
            Some((shell::Effect::ExternalState, _)) => (
                otto_core::approval::Authority::Boundary,
                "Run a command that changes external state?",
            ),
            Some((shell::Effect::Destructive, _)) => (
                otto_core::approval::Authority::Bounded,
                "Run a destructive workspace command?",
            ),
            None if read_path.is_empty() => (
                otto_core::approval::Authority::Boundary,
                "Run an unsandboxed command?",
            ),
            None => (
                otto_core::approval::Authority::Consent,
                "Save persistent sandbox read access?",
            ),
        };
        let mut details = serde_json::json!({"command":args.command,"workspace":self.workspace_root,"read_path":read_path,"native_facts":facts});
        let mut reason = args.justification;
        if let Some((effect, segment)) = &gate {
            details["sandboxed"] = (!excluded).into();
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
        request: &otto_core::approval::Request,
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
        _request: &otto_core::approval::Request,
    ) -> ToolResult {
        self.execute_inner(call.arguments, cancel, true).await
    }
}

impl BashTool {
    /// The effect class and command words when this call needs approval for
    /// its effect: approvals are available and it changes external state or
    /// deletes content. An excluded command is gated whatever its
    /// `sandbox_permissions`, because every excluded command runs on the
    /// excluded executor.
    fn effect_gate(&self, args: &BashArgs, excluded: bool) -> Option<(shell::Effect, Vec<String>)> {
        if !args.sandbox_read_path.is_empty()
            || (args.sandbox_permissions != SandboxPermissions::UseDefault && !excluded)
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
        // An excluded command needs no escalation grant and never consumes
        // one; a classified excluded command still needs the effect approval.
        let excluded = self.excluded.as_ref().filter(|excluded| {
            args.sandbox_read_path.is_empty() && excluded.matches(&args.command)
        });
        if !authorized && let Some((effect, segment)) = self.effect_gate(&args, excluded.is_some())
        {
            let what = match effect {
                shell::Effect::ExternalState => "changes external state",
                shell::Effect::Destructive => "deletes workspace or repository content",
            };
            let segment = serde_json::to_string(&segment.join(" ")).expect("string encodes");
            return self.argument_error(&format!(
                "approval required: this command {what} ({segment}).\n\
The command did not run. This tool call has no approval flow; ask the user to run the command or to approve it in a session with approvals."
            ));
        }
        let (executor, environment) = match (excluded, args.sandbox_permissions) {
            (Some(excluded), _) => (&excluded.executor, excluded.environment.as_slice()),
            (None, SandboxPermissions::UseDefault) => (&self.executor, self.environment.as_slice()),
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
mod tests;
