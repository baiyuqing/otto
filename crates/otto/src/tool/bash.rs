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

#[cfg(test)]
use crate::sandbox::DEFAULT_CANCELLATION_GRACE;
use crate::sandbox::{CommandExecutor, Error, ExitStatus, Request, Streams};
use otto_core::safetext::{dynamic_redaction_marker, secret_forms};

use super::bash_output::{Logs, Output, PREVIEW_BYTES};
use super::result::{RedactingCollector, decode_strict_json, redact_exact_text};
use super::root::Root;
use super::shell;
use super::workspace::Workspace;
use super::{Tool, definition};

mod approvals;
mod execute;
mod review;
mod tool_impl;

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
    ///
    /// `output_logs` is the owning session's overflow-log set; the tool leases
    /// and writes its logs through it.
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        workspace: &Workspace,
        executor: Arc<dyn CommandExecutor>,
        shell: &str,
        environment: Vec<String>,
        timeout: Duration,
        max_output_bytes: usize,
        redaction_values: &[String],
        output_logs: Arc<Logs>,
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
            output_logs,
        )
    }

    /// Binds the tool to one executor with a caller-selected process
    /// termination grace.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_with_grace(
        workspace: &Workspace,
        executor: Arc<dyn CommandExecutor>,
        shell: &str,
        environment: Vec<String>,
        timeout: Duration,
        cancellation_grace: Duration,
        max_output_bytes: usize,
        redaction_values: &[String],
        output_logs: Arc<Logs>,
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
            output_logs,
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
        })
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
const OUTPUT_NOTE: &str = "Return exit status and stdout/stderr previews of at most 4 KiB each (or \
    the smaller configured cap); longer redacted output is saved \
    temporarily under .otto/tool-output and automatically removed after the \
    turn and its children finish. Delete individual logs after analysis \
    when no task still needs them; never remove the shared directory. \
    Analyze logs with scripts and print only verified summaries or relevant \
    failures; use read with offset/limit for excerpts instead of printing \
    complete logs.";

/// The read capability of a sandboxed command.
const SANDBOX_READ_NOTE: &str = "Sandboxed commands read every path except the sandbox read denylist (credential and personal-data locations).";

fn describe(base: &str) -> String {
    let base = base.trim_end_matches('.');
    format!("{base}. {OUTPUT_NOTE} {SANDBOX_READ_NOTE}")
}

/// The schema advertised for `bash`.
pub fn bash_definition() -> ToolDefinition {
    definition(
        "bash",
        &describe("Execute a shell command from the workspace"),
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Shell command to execute; for verbose jobs prefer quiet flags or a \
                        script that captures logs, preserves the command exit status, and \
                        prints a concise checked summary"
                }
            },
            "required": ["command"]
        }),
    )
}

pub fn bash_definition_with_approvals() -> ToolDefinition {
    definition(
        "bash",
        &describe(
            "Execute a shell command from the workspace. Set sandbox_permissions to \
                require_escalated only when sandboxed execution cannot complete the \
                authorized task; include a justification. AUTO reviews the exact \
                command and bounded native script/build facts against original user \
                evidence; missing or changed payloads require human review. A command \
                that changes external state (git push, gh writes, curl or wget uploads, \
                package publish, kubectl/helm/terraform changes) or deletes workspace \
                or repository content (git reset --hard, git clean -f, rm -r, find \
                -delete) waits for the same approval without a justification, then runs \
                in the sandbox, or outside it when it matches [sandbox] \
                excluded_commands. To request persistent sandbox read access, set \
                sandbox_read_path to one absolute existing path and include a \
                justification; keep sandbox_permissions as use_default. Persistent \
                security grants require a person in AUTO. The command runs only after \
                authority is recorded; MANUAL asks, NEVER rejects additional authority, \
                and YOLO bypasses review while retaining hard checks.",
        ),
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Shell command to execute; for verbose jobs prefer quiet flags or a \
                        script that captures logs, preserves the command exit status, and \
                        prints a concise checked summary"
                },
                "sandbox_permissions": {
                    "type": "string",
                    "enum": ["use_default", "require_escalated"],
                    "description": "Use require_escalated for one exact unsandboxed command; the backend records authority through AUTO review, an explicit rule or the user's decision before dispatch"
                },
                "sandbox_read_path": {
                    "type": "string",
                    "description": "Request user approval to permanently allow reading one absolute existing path that the sandbox read denylist covers, before running this command inside the sandbox"
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
