//! Cross-workspace delivery primitives.
//!
//! This module deliberately contains no model tool, approval decision, or
//! runner. It validates the immutable facts those layers must bind: one
//! canonical Git repository, one new target-local worktree and a bounded,
//! frozen handoff packet. Keeping those facts independent makes it impossible
//! for an approval to accidentally turn a source workspace into a second live
//! file-tool root.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use otto_core::model::ToolDefinition;
use otto_core::tool::{ToolCall, ToolResult};
use serde::Deserialize;
use serde_json::{json, value::RawValue};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::sandbox::{CommandExecutor, Request as SandboxRequest, Streams};
use crate::storage::{Connection, OptionalExtension, params};
use crate::tool::result::decode_strict_json;
use crate::tool::{ApprovalSpec, Tool, definition, error_result, text_result};
use std::sync::Mutex;

pub const TOOL_NAME: &str = "start_delivery_task";
const MAX_OBJECTIVE_BYTES: usize = 16 * 1024;
const MAX_SOURCE_REFS: usize = 32;
const MAX_SOURCE_REF_BYTES: usize = 4 * 1024;

pub const HANDOFF_MAX_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// Canonical primary checkout path, never a linked worktree.
    pub repository: PathBuf,
    /// Git common directory, canonicalized by Git.
    pub common_dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreePlan {
    pub target: Target,
    pub delivery_id: String,
    pub branch: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffItem {
    pub label: String,
    pub text: String,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handoff {
    pub objective: String,
    pub items: Vec<HandoffItem>,
    pub bytes: usize,
    pub digest: String,
}

fn invalid(message: &str) -> String {
    format!("invalid delivery target: {message}")
}

/// Derives a new branch name when none was requested. The generated form is
/// intentionally small and Git-ref-safe; callers must still verify absence.
pub fn branch(requested: Option<&str>, objective: &str) -> Result<String, String> {
    if let Some(value) = requested.filter(|value| !value.trim().is_empty()) {
        validate_branch(value)?;
        return Ok(value.to_owned());
    }
    let slug: String = objective
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .take(6)
        .collect::<Vec<_>>()
        .join("-");
    let slug = if slug.is_empty() { "delivery" } else { &slug };
    let generated = format!("agent/{}", &slug[..slug.len().min(48)]);
    validate_branch(&generated)?;
    Ok(generated)
}

/// Stable delivery id derived from immutable preview facts. It is not an
/// authority token; the approval id remains the durable idempotency key.
pub fn delivery_id(target: &Target, branch: &str, handoff_digest: &str) -> String {
    let material = format!(
        "{}\n{}\n{}",
        target.repository.display(),
        branch,
        handoff_digest
    );
    hash(&material)[..24].to_string()
}

pub fn plan(target: Target, delivery_id: &str, branch: String) -> Result<WorktreePlan, String> {
    if !valid_delivery_id(delivery_id) {
        return Err("invalid delivery id".into());
    }
    validate_branch(&branch)?;
    let path = target
        .repository
        .join(".otto/delivery-worktrees")
        .join(delivery_id);
    if path.exists() {
        return Err("delivery worktree path already exists".into());
    }
    Ok(WorktreePlan {
        target,
        delivery_id: delivery_id.into(),
        branch,
        path,
    })
}

/// Fixed, shell-free argv for the only Git mutation the delivery setup may
/// perform. The caller must execute it through an approval-scoped constrained
/// executor, then canonicalize and revalidate the created worktree before it
/// can become a tool root.
pub fn worktree_add_argv(plan: &WorktreePlan, base: &str) -> Result<Vec<String>, String> {
    if !valid_object_id(base) {
        return Err("invalid delivery base revision".into());
    }
    if plan.path.exists()
        || !plan
            .path
            .starts_with(plan.target.repository.join(".otto/delivery-worktrees"))
    {
        return Err("delivery worktree path is no longer available".into());
    }
    Ok(vec![
        "git".into(),
        "-c".into(),
        "core.hooksPath=/dev/null".into(),
        "-c".into(),
        "core.fsmonitor=false".into(),
        "-c".into(),
        "protocol.ext.allow=never".into(),
        "worktree".into(),
        "add".into(),
        "-b".into(),
        plan.branch.clone(),
        plan.path.to_string_lossy().into_owned(),
        base.into(),
    ])
}

fn valid_object_id(value: &str) -> bool {
    (40..=64).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Runs only a prevalidated `git worktree add` plan through an existing
/// sandbox executor. It deliberately has no API for arbitrary argv, shell
/// text, a caller-selected directory, or caller-selected environment.
#[derive(Clone)]
pub struct WorktreeExecutor {
    executor: Arc<dyn CommandExecutor>,
    environment: Vec<String>,
}

impl WorktreeExecutor {
    pub fn new(executor: Arc<dyn CommandExecutor>, environment: Vec<String>) -> Self {
        Self {
            executor,
            environment,
        }
    }

    /// The caller is responsible for a separate approval that permits this
    /// fixed Git metadata mutation. A normal workspace sandbox is expected to
    /// reject it, and this type never treats a denial as success.
    pub async fn add(
        &self,
        plan: &WorktreePlan,
        base: &str,
        cancel: &CancellationToken,
    ) -> Result<(), String> {
        let argv = worktree_add_argv(plan, base)?;
        let mut stdout = std::io::sink();
        let mut stderr = std::io::sink();
        let status = self
            .executor
            .execute(
                SandboxRequest {
                    argv,
                    dir: plan.target.repository.clone(),
                    env: self.environment.clone(),
                    report_denials: true,
                    ..Default::default()
                },
                Streams {
                    stdout: &mut stdout,
                    stderr: &mut stderr,
                },
                cancel,
            )
            .await;
        match status {
            (status, Ok(())) if status.code == 0 && !status.signaled => Ok(()),
            (status, _) if !status.denials.is_empty() => Err(
                "delivery worktree setup was denied by the sandbox; approve the constrained delivery setup permission and retry".into(),
            ),
            (status, _) if status.signaled => Err("delivery worktree setup was cancelled".into()),
            (status, _) => Err(format!("delivery worktree setup failed with Git exit status {}", status.code)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiptState {
    Prepared,
    Applying,
    Started,
    Unknown,
}

impl ReceiptState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::Applying => "applying",
            Self::Started => "started",
            Self::Unknown => "unknown",
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "prepared" => Ok(Self::Prepared),
            "applying" => Ok(Self::Applying),
            "started" => Ok(Self::Started),
            "unknown" => Ok(Self::Unknown),
            _ => Err("delivery receipt state is invalid".into()),
        }
    }
}

/// Immutable facts committed before a delivery may mutate Git metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiptBinding {
    pub approval_id: String,
    pub source_workspace: String,
    pub target_repository: String,
    pub target_common_dir: String,
    pub worktree: String,
    pub branch: String,
    pub base: String,
    pub handoff_digest: String,
}

impl ReceiptBinding {
    fn valid(&self) -> bool {
        !self.approval_id.is_empty()
            && !self.source_workspace.is_empty()
            && Path::new(&self.target_repository).is_absolute()
            && Path::new(&self.target_common_dir).is_absolute()
            && Path::new(&self.worktree).is_absolute()
            && validate_branch(&self.branch).is_ok()
            && valid_object_id(&self.base)
            && self.handoff_digest.len() == 64
            && self
                .handoff_digest
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredReceipt {
    pub binding: ReceiptBinding,
    pub state: ReceiptState,
}

/// Terminal and in-progress state for the target-only delivery runner. This
/// deliberately lives outside the ordinary sub-agent task schema: a delivery
/// may retain a worktree for inspection after failure and cannot be replayed
/// from an interrupted effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    Running,
    Succeeded,
    Failed,
    Canceled,
    Unknown,
}

impl TaskState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
            Self::Unknown => "unknown",
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "running" => Ok(Self::Running),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            "canceled" => Ok(Self::Canceled),
            "unknown" => Ok(Self::Unknown),
            _ => Err("delivery task state is invalid".into()),
        }
    }

    pub fn terminal(self) -> bool {
        !matches!(self, Self::Running)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredTask {
    pub approval_id: String,
    pub source_session: String,
    pub target_session: String,
    pub worktree: String,
    pub state: TaskState,
    pub result: String,
    pub error: String,
    pub source_notified: bool,
}

/// Separate durable receipt store. It intentionally does not share the
/// ordinary sub-agent task table: delivery has a different recovery contract.
pub struct ReceiptStore {
    connection: Mutex<Connection>,
}

impl ReceiptStore {
    const SCHEMA: &'static str = "
CREATE TABLE IF NOT EXISTS delivery_receipts (
 approval_id TEXT PRIMARY KEY,
 source_workspace TEXT NOT NULL,
 target_repository TEXT NOT NULL,
 target_common_dir TEXT NOT NULL,
 worktree TEXT NOT NULL,
 branch TEXT NOT NULL,
 base TEXT NOT NULL,
 handoff_digest TEXT NOT NULL,
 state TEXT NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS delivery_sessions (
 approval_id TEXT PRIMARY KEY REFERENCES delivery_receipts(approval_id),
 session_id TEXT NOT NULL,
 worktree TEXT NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS delivery_tasks (
 approval_id TEXT PRIMARY KEY REFERENCES delivery_receipts(approval_id),
 source_session TEXT NOT NULL,
 target_session TEXT NOT NULL,
 worktree TEXT NOT NULL,
 state TEXT NOT NULL,
 result TEXT NOT NULL,
 error TEXT NOT NULL,
 source_notified INTEGER NOT NULL
) STRICT;";

    pub fn open(path: &Path) -> Result<Self, String> {
        use std::os::unix::fs::PermissionsExt;

        if let Some(parent) = path.parent().filter(|path| !path.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .map_err(|_| "delivery receipt storage is unavailable")?;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
                .map_err(|_| "delivery receipt storage is unavailable")?;
        }
        let connection =
            Connection::open(path).map_err(|_| "delivery receipt storage is unavailable")?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|_| "delivery receipt storage is unavailable")?;
        Self::from_connection(connection)
    }

    pub fn from_connection(connection: Connection) -> Result<Self, String> {
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(|_| "delivery receipt storage is unavailable")?;
        connection
            .execute_batch(Self::SCHEMA)
            .map_err(|_| "delivery receipt storage is unavailable")?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    #[cfg(test)]
    fn in_memory() -> Self {
        Self::from_connection(Connection::open_in_memory().unwrap()).unwrap()
    }

    /// Commits the binding once. A retry for exactly the same approved facts
    /// returns the existing receipt; mismatched facts never overwrite it.
    pub fn prepare(&self, binding: ReceiptBinding) -> Result<StoredReceipt, String> {
        if !binding.valid() {
            return Err("delivery receipt binding is invalid".into());
        }
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| "delivery receipt storage is unavailable")?;
        let transaction = connection
            .transaction()
            .map_err(|_| "delivery receipt storage is unavailable")?;
        let existing = Self::get_from(&transaction, &binding.approval_id)?;
        if let Some(existing) = existing {
            if existing.binding != binding {
                return Err("delivery approval receipt conflicts with existing binding".into());
            }
            transaction
                .commit()
                .map_err(|_| "delivery receipt storage is unavailable")?;
            return Ok(existing);
        }
        transaction
            .execute(
                "INSERT INTO delivery_receipts VALUES (?1,?2,?3,?4,?5,?6,?7,?8,'prepared')",
                params![
                    binding.approval_id,
                    binding.source_workspace,
                    binding.target_repository,
                    binding.target_common_dir,
                    binding.worktree,
                    binding.branch,
                    binding.base,
                    binding.handoff_digest,
                ],
            )
            .map_err(|_| "delivery receipt storage is unavailable")?;
        transaction
            .commit()
            .map_err(|_| "delivery receipt storage is unavailable")?;
        Ok(StoredReceipt {
            binding,
            state: ReceiptState::Prepared,
        })
    }

    pub fn get(&self, approval_id: &str) -> Result<Option<StoredReceipt>, String> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| "delivery receipt storage is unavailable")?;
        Self::get_from(&connection, approval_id)
    }

    /// Persists the one target-worktree session bound to an approved receipt.
    /// A retry may observe the same binding but can never replace it.
    pub fn bind_session(
        &self,
        approval_id: &str,
        session_id: &str,
        worktree: &Path,
    ) -> Result<(), String> {
        if session_id.is_empty() || !worktree.is_absolute() {
            return Err("delivery session binding is invalid".into());
        }
        let connection = self
            .connection
            .lock()
            .map_err(|_| "delivery receipt storage is unavailable")?;
        let existing: Option<(String, String)> = connection
            .query_row(
                "SELECT session_id,worktree FROM delivery_sessions WHERE approval_id=?1",
                [approval_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|_| "delivery receipt storage is unavailable")?;
        let worktree = worktree.to_string_lossy().into_owned();
        match existing {
            Some((id, path)) if id == session_id && path == worktree => Ok(()),
            Some(_) => Err("delivery session binding conflicts with existing receipt".into()),
            None => connection
                .execute(
                    "INSERT INTO delivery_sessions VALUES (?1,?2,?3)",
                    params![approval_id, session_id, worktree],
                )
                .map(|_| ())
                .map_err(|_| "delivery receipt storage is unavailable".into()),
        }
    }

    pub fn session(&self, approval_id: &str) -> Result<Option<(String, String)>, String> {
        self.connection
            .lock()
            .map_err(|_| "delivery receipt storage is unavailable")?
            .query_row(
                "SELECT session_id,worktree FROM delivery_sessions WHERE approval_id=?1",
                [approval_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|_| "delivery receipt storage is unavailable".into())
    }

    /// Creates the one target-runner task for a receipt. Existing identical
    /// bindings are idempotent; no caller may replace its target session.
    pub fn begin_task(
        &self,
        approval_id: &str,
        source_session: &str,
        target_session: &str,
        worktree: &Path,
    ) -> Result<StoredTask, String> {
        if source_session.is_empty() || target_session.is_empty() || !worktree.is_absolute() {
            return Err("delivery task binding is invalid".into());
        }
        let worktree = worktree.to_string_lossy().into_owned();
        let connection = self
            .connection
            .lock()
            .map_err(|_| "delivery receipt storage is unavailable")?;
        if let Some(existing) = Self::task_from(&connection, approval_id)? {
            if existing.source_session == source_session
                && existing.target_session == target_session
                && existing.worktree == worktree
            {
                return Ok(existing);
            }
            return Err("delivery task binding conflicts with existing receipt".into());
        }
        connection
            .execute(
                "INSERT INTO delivery_tasks VALUES (?1,?2,?3,?4,'running','','',0)",
                params![approval_id, source_session, target_session, worktree],
            )
            .map_err(|_| "delivery receipt storage is unavailable")?;
        Self::task_from(&connection, approval_id)?
            .ok_or("delivery task storage is unavailable".into())
    }

    pub fn task(&self, approval_id: &str) -> Result<Option<StoredTask>, String> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| "delivery receipt storage is unavailable")?;
        Self::task_from(&connection, approval_id)
    }

    pub fn finish_task(
        &self,
        approval_id: &str,
        state: TaskState,
        result: &str,
        error: &str,
    ) -> Result<StoredTask, String> {
        if !state.terminal() {
            return Err("delivery task must finish in a terminal state".into());
        }
        let connection = self
            .connection
            .lock()
            .map_err(|_| "delivery receipt storage is unavailable")?;
        let task =
            Self::task_from(&connection, approval_id)?.ok_or("delivery task is unavailable")?;
        if task.state.terminal() {
            if task.state == state && task.result == result && task.error == error {
                return Ok(task);
            }
            return Err("delivery task is already terminal".into());
        }
        connection.execute(
            "UPDATE delivery_tasks SET state=?2,result=?3,error=?4 WHERE approval_id=?1 AND state='running'",
            params![approval_id, state.as_str(), result, error],
        ).map_err(|_| "delivery receipt storage is unavailable")?;
        Self::task_from(&connection, approval_id)?
            .ok_or("delivery task storage is unavailable".into())
    }

    /// Claims the source notification once after a terminal task. The caller
    /// must append only a text summary, never a target capability or path.
    pub fn claim_source_notification(
        &self,
        approval_id: &str,
    ) -> Result<Option<StoredTask>, String> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| "delivery receipt storage is unavailable")?;
        let task =
            Self::task_from(&connection, approval_id)?.ok_or("delivery task is unavailable")?;
        if !task.state.terminal() || task.source_notified {
            return Ok(None);
        }
        connection.execute(
            "UPDATE delivery_tasks SET source_notified=1 WHERE approval_id=?1 AND source_notified=0",
            [approval_id],
        ).map_err(|_| "delivery receipt storage is unavailable")?;
        Ok(Some(
            Self::task_from(&connection, approval_id)?
                .ok_or("delivery task storage is unavailable")?,
        ))
    }

    /// Marks a crash-interrupted target run unknown; it is never resumed
    /// automatically because its provider/tool effects may be ambiguous.
    pub fn mark_running_tasks_unknown(&self) -> Result<usize, String> {
        self.connection.lock().map_err(|_| "delivery receipt storage is unavailable")?
            .execute("UPDATE delivery_tasks SET state='unknown',error='delivery runner interrupted' WHERE state='running'", ())
            .map_err(|_| "delivery receipt storage is unavailable".into())
    }

    /// Marks the exact receipt as executing. Retrying a started receipt is
    /// idempotent; an unknown receipt is never replayed.
    pub fn begin(&self, approval_id: &str) -> Result<StoredReceipt, String> {
        self.transition(approval_id, ReceiptState::Prepared, ReceiptState::Applying)
    }

    pub fn started(&self, approval_id: &str) -> Result<StoredReceipt, String> {
        self.transition(approval_id, ReceiptState::Applying, ReceiptState::Started)
    }

    pub fn unknown(&self, approval_id: &str) -> Result<StoredReceipt, String> {
        self.transition(approval_id, ReceiptState::Applying, ReceiptState::Unknown)
    }

    /// Crash recovery is deliberately conservative: no external Git effect is
    /// replayed. The caller must inspect the recorded worktree before a future
    /// explicit resume feature may continue it.
    pub fn mark_interrupted_unknown(&self) -> Result<usize, String> {
        self.connection
            .lock()
            .map_err(|_| "delivery receipt storage is unavailable")?
            .execute(
                "UPDATE delivery_receipts SET state='unknown' WHERE state='applying'",
                (),
            )
            .map_err(|_| "delivery receipt storage is unavailable".to_string())
    }

    fn transition(
        &self,
        approval_id: &str,
        expected: ReceiptState,
        next: ReceiptState,
    ) -> Result<StoredReceipt, String> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| "delivery receipt storage is unavailable")?;
        let transaction = connection
            .transaction()
            .map_err(|_| "delivery receipt storage is unavailable")?;
        let receipt =
            Self::get_from(&transaction, approval_id)?.ok_or("delivery receipt is unavailable")?;
        let receipt = match receipt.state {
            state if state == next => receipt,
            state if state == expected => {
                transaction
                    .execute(
                        "UPDATE delivery_receipts SET state=?2 WHERE approval_id=?1 AND state=?3",
                        params![approval_id, next.as_str(), expected.as_str()],
                    )
                    .map_err(|_| "delivery receipt storage is unavailable")?;
                StoredReceipt {
                    state: next,
                    ..receipt
                }
            }
            ReceiptState::Unknown => {
                return Err(
                    "delivery receipt outcome is unknown; inspect the worktree before retrying"
                        .into(),
                );
            }
            _ => return Err("delivery receipt is in an invalid state".into()),
        };
        transaction
            .commit()
            .map_err(|_| "delivery receipt storage is unavailable")?;
        Ok(receipt)
    }

    fn task_from(connection: &Connection, approval_id: &str) -> Result<Option<StoredTask>, String> {
        connection
            .query_row(
                "SELECT approval_id,source_session,target_session,worktree,state,result,error,source_notified FROM delivery_tasks WHERE approval_id=?1",
                [approval_id],
                |row| {
                    Ok(StoredTask {
                        approval_id: row.get(0)?,
                        source_session: row.get(1)?,
                        target_session: row.get(2)?,
                        worktree: row.get(3)?,
                        state: TaskState::parse(&row.get::<String>(4)?).map_err(|_| {
                            crate::storage::Error::Corrupt("delivery task state".into())
                        })?,
                        result: row.get(5)?,
                        error: row.get(6)?,
                        source_notified: row.get::<i64>(7)? != 0,
                    })
                },
            )
            .optional()
            .map_err(|_| "delivery receipt storage is unavailable".into())
    }

    fn get_from(
        connection: &Connection,
        approval_id: &str,
    ) -> Result<Option<StoredReceipt>, String> {
        connection
            .query_row(
                "SELECT approval_id,source_workspace,target_repository,target_common_dir,worktree,branch,base,handoff_digest,state FROM delivery_receipts WHERE approval_id=?1",
                [approval_id],
                |row| {
                    Ok(StoredReceipt {
                        binding: ReceiptBinding {
                            approval_id: row.get(0)?,
                            source_workspace: row.get(1)?,
                            target_repository: row.get(2)?,
                            target_common_dir: row.get(3)?,
                            worktree: row.get(4)?,
                            branch: row.get(5)?,
                            base: row.get(6)?,
                            handoff_digest: row.get(7)?,
                        },
                        state: ReceiptState::parse(&row.get::<String>(8)?).map_err(|_| {
                            crate::storage::Error::Corrupt("delivery receipt state".into())
                        })?,
                    })
                },
            )
            .optional()
            .map_err(|_| "delivery receipt storage is unavailable".into())
    }
}

/// Renders the only source-derived input supplied to a target delivery runner.
/// The handoff is already bounded and frozen; callers must not append source
/// workspace paths, live file handles, or unbounded transcript data.
pub fn render_handoff_prompt(handoff: &Handoff) -> String {
    let mut prompt = format!(
        "Delivery objective:\n{}\n\nFrozen source handoff:",
        handoff.objective
    );
    for item in &handoff.items {
        prompt.push_str("\n\n## ");
        prompt.push_str(&item.label);
        prompt.push('\n');
        prompt.push_str(&item.text);
    }
    prompt
}

/// Builds the exact untrusted text handoff before a target runner exists.
pub fn handoff(
    objective: &str,
    items: impl IntoIterator<Item = (String, String)>,
) -> Result<Handoff, String> {
    if objective.trim().is_empty() {
        return Err("delivery objective is required".into());
    }
    let mut bytes = objective.len();
    if bytes > HANDOFF_MAX_BYTES {
        return Err("delivery handoff exceeds 64 KiB".into());
    }
    let mut frozen = Vec::new();
    for (label, text) in items {
        if label.trim().is_empty() || label.contains('\0') || text.contains('\0') {
            return Err("delivery handoff contains an invalid item".into());
        }
        bytes = bytes
            .checked_add(label.len() + text.len())
            .ok_or("delivery handoff exceeds 64 KiB")?;
        if bytes > HANDOFF_MAX_BYTES {
            return Err("delivery handoff exceeds 64 KiB".into());
        }
        frozen.push(HandoffItem {
            sha256: hash(&text),
            label,
            text,
        });
    }
    let mut manifest = objective.to_owned();
    for item in &frozen {
        manifest.push('\n');
        manifest.push_str(&item.label);
        manifest.push('\n');
        manifest.push_str(&item.sha256);
    }
    Ok(Handoff {
        objective: objective.into(),
        items: frozen,
        bytes,
        digest: hash(&manifest),
    })
}

fn valid_delivery_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

fn validate_branch(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 120
        || value.starts_with('-')
        || value.starts_with('/')
        || value.ends_with('/')
        || value.contains("..")
        || value.contains("//")
        || value.ends_with('.')
        || value.ends_with(".lock")
        || value.bytes().any(|byte| {
            byte <= b' ' || matches!(byte, b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\' | b'@')
        })
    {
        return Err("invalid delivery branch".into());
    }
    Ok(())
}

fn canonical_line(output: &str, message: &str) -> Result<PathBuf, String> {
    let line = output.trim();
    if line.is_empty() || line.contains('\n') {
        return Err(invalid(message));
    }
    std::fs::canonicalize(line).map_err(|_| invalid(message))
}

fn hash(value: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(value.as_bytes());
    format!("{:x}", digest.finalize())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub target: String,
    pub objective: String,
    pub branch: Option<String>,
    pub source_refs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preview {
    pub source_workspace: String,
    pub plan: WorktreePlan,
    /// Immutable target HEAD object id selected during preview.
    pub base: String,
    pub handoff: Handoff,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Receipt {
    pub id: String,
    pub preview: Preview,
    pub base: String,
}

/// The narrow effect boundary for delivery. Its eventual native implementation
/// owns durable receipts, constrained `git worktree add`, and a target runner.
/// The tool itself neither opens a second workspace nor executes shell text.
#[async_trait]
pub trait Service: Send + Sync {
    /// Persists immutable approved facts before any Git metadata mutation.
    /// Preview is deliberately not part of this effect port: target admission
    /// and identity inspection must come from the same asynchronous,
    /// target-host-bound [`Preflight`] used for approval creation/revalidation.
    async fn prepare(&self, approval_id: &str, preview: &Preview) -> Result<Receipt, String>;
    async fn start(&self, receipt: &Receipt, cancel: &CancellationToken) -> Result<String, String>;
}

/// Asynchronous target preflight used both to render the approval subject and
/// to revalidate it after a decision. It owns no mutable delivery authority.
#[async_trait]
pub trait Preflight: Send + Sync {
    async fn preview(
        &self,
        source_workspace: &str,
        request: &Request,
        cancel: &CancellationToken,
    ) -> Result<Preview, String>;
}

/// A target-bound, read-only Git inspector. Its methods accept no model argv,
/// never invoke a shell, and can only run fixed `rev-parse` requests through
/// the injected sandbox executor. Runtime wiring will call this from an async
/// preparation path once it owns the target sandbox lifetime.
#[derive(Clone)]
pub struct SandboxTargetInspector {
    executor: Arc<dyn CommandExecutor>,
    environment: Vec<String>,
}

impl SandboxTargetInspector {
    pub fn new(executor: Arc<dyn CommandExecutor>, environment: Vec<String>) -> Self {
        Self {
            executor,
            environment,
        }
    }

    pub async fn inspect(
        &self,
        admitted_root: &Path,
        cancel: &CancellationToken,
    ) -> Result<(Target, String), String> {
        let top = self
            .git(admitted_root, &["rev-parse", "--show-toplevel"], cancel)
            .await?;
        let repository = canonical_line(&top, "Git did not report a repository root")?;
        if repository != admitted_root {
            return Err("delivery target identity is invalid".into());
        }
        let common_dir = canonical_line(
            &self
                .git(
                    admitted_root,
                    &["rev-parse", "--path-format=absolute", "--git-common-dir"],
                    cancel,
                )
                .await?,
            "Git did not report its common directory",
        )?;
        let git_dir = canonical_line(
            &self
                .git(
                    admitted_root,
                    &["rev-parse", "--path-format=absolute", "--git-dir"],
                    cancel,
                )
                .await?,
            "Git did not report its git directory",
        )?;
        if git_dir != common_dir {
            return Err("delivery target must be a primary checkout, not a linked worktree".into());
        }
        let base = self
            .git(admitted_root, &["rev-parse", "HEAD"], cancel)
            .await?;
        let base = base.trim();
        if !valid_object_id(base) {
            return Err("delivery target Git HEAD is invalid".into());
        }
        Ok((
            Target {
                repository,
                common_dir,
            },
            base.into(),
        ))
    }

    async fn git(
        &self,
        root: &Path,
        arguments: &[&str],
        cancel: &CancellationToken,
    ) -> Result<String, String> {
        let mut argv = vec![
            "git".into(),
            "-c".into(),
            "core.hooksPath=/dev/null".into(),
            "-c".into(),
            "core.fsmonitor=false".into(),
            "-c".into(),
            "protocol.ext.allow=never".into(),
        ];
        argv.extend(arguments.iter().map(|argument| (*argument).into()));
        let mut stdout = LimitedOutput::default();
        let mut stderr = std::io::sink();
        let (status, result) = self
            .executor
            .execute(
                SandboxRequest {
                    argv,
                    dir: root.into(),
                    env: self.environment.clone(),
                    report_denials: true,
                    ..Default::default()
                },
                Streams {
                    stdout: &mut stdout,
                    stderr: &mut stderr,
                },
                cancel,
            )
            .await;
        if result.is_err() || status.code != 0 || status.signaled || !status.denials.is_empty() {
            return Err("delivery target Git inspection failed".into());
        }
        stdout.finish()
    }
}

const MAX_GIT_INSPECTION_BYTES: usize = 4096;

#[derive(Default)]
struct LimitedOutput {
    bytes: Vec<u8>,
    overflow: bool,
}

impl std::io::Write for LimitedOutput {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let available = MAX_GIT_INSPECTION_BYTES.saturating_sub(self.bytes.len());
        let kept = bytes.len().min(available);
        self.bytes.extend_from_slice(&bytes[..kept]);
        self.overflow |= kept != bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl LimitedOutput {
    fn finish(self) -> Result<String, String> {
        if self.overflow {
            return Err("delivery target Git inspection output exceeds limit".into());
        }
        String::from_utf8(self.bytes)
            .map_err(|_| "delivery target Git inspection returned invalid text".into())
    }
}

/// Human-configured admission inputs shared with `otto serve`.
#[derive(Debug, Clone)]
pub struct AdmissionPolicy {
    pub startup: PathBuf,
    pub roots: Vec<PathBuf>,
    pub trusted: Vec<PathBuf>,
}

/// A read-only delivery preview backed by the target workspace's sandbox.
///
/// Approval rendering and revalidation need only admission plus fixed
/// `git rev-parse` requests, whereas a delivery effect additionally needs a
/// receipt store and the constrained worktree executor. A CLI/serve
/// composition factory may retain this value without exposing either target
/// capability to a source runner.
#[derive(Clone)]
pub struct SandboxPreflight {
    policy: AdmissionPolicy,
    inspector: SandboxTargetInspector,
}

impl SandboxPreflight {
    pub fn new(policy: AdmissionPolicy, inspector: SandboxTargetInspector) -> Self {
        Self { policy, inspector }
    }

    async fn preview_inner(
        &self,
        source_workspace: &str,
        request: &Request,
        cancel: &CancellationToken,
    ) -> Result<Preview, String> {
        let admitted = crate::runtime::workspace_admission::admit_workspace(
            &request.target,
            &self.policy.startup,
            &self.policy.roots,
            &self.policy.trusted,
        )
        .map_err(|_| "delivery target is not an admitted workspace")?;
        let (target, base) = self.inspector.inspect(&admitted, cancel).await?;
        if target.repository != admitted || !valid_object_id(&base) {
            return Err("delivery target identity is invalid".into());
        }
        let branch = branch(request.branch.as_deref(), &request.objective)?;
        let handoff = handoff(
            &request.objective,
            request
                .source_refs
                .iter()
                .enumerate()
                .map(|(index, value)| (format!("source-ref-{}", index + 1), value.clone())),
        )?;
        let delivery_id = delivery_id(&target, &branch, &handoff.digest);
        let plan = plan(target, &delivery_id, branch)?;
        Ok(Preview {
            source_workspace: source_workspace.into(),
            plan,
            base,
            handoff,
        })
    }
}

#[async_trait]
impl Preflight for SandboxPreflight {
    async fn preview(
        &self,
        source_workspace: &str,
        request: &Request,
        cancel: &CancellationToken,
    ) -> Result<Preview, String> {
        self.preview_inner(source_workspace, request, cancel).await
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolArgs {
    target: String,
    objective: String,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    source_refs: Vec<String>,
}

impl TryFrom<ToolArgs> for Request {
    type Error = String;

    fn try_from(value: ToolArgs) -> Result<Self, Self::Error> {
        if value.target.is_empty() || !Path::new(&value.target).is_absolute() {
            return Err("target must be an absolute path".into());
        }
        if value.objective.trim().is_empty() || value.objective.len() > MAX_OBJECTIVE_BYTES {
            return Err("objective must be between 1 and 16384 bytes".into());
        }
        if value.source_refs.len() > MAX_SOURCE_REFS
            || value.source_refs.iter().any(|item| {
                item.is_empty() || item.len() > MAX_SOURCE_REF_BYTES || item.contains('\0')
            })
        {
            return Err("source_refs must contain at most 32 bounded references".into());
        }
        Ok(Self {
            target: value.target,
            objective: value.objective,
            branch: value.branch,
            source_refs: value.source_refs,
        })
    }
}

pub fn start_definition() -> ToolDefinition {
    definition(
        TOOL_NAME,
        "Prepare a separately authorized delivery task in another admitted Git repository. The source workspace stays read-only; a future target worktree becomes the task's only writable root. This always requires user authorization and does not grant GitHub, network, or unsandboxed Bash access.",
        json!({
            "type":"object", "additionalProperties":false,
            "properties": {
                "target":{"type":"string","description":"Absolute path to an admitted primary Git checkout."},
                "objective":{"type":"string","description":"The delivery objective for the isolated target task."},
                "branch":{"type":"string","description":"Optional new branch; defaults to agent/<objective slug>."},
                "source_refs":{"type":"array","items":{"type":"string"},"description":"Bounded references already selected from source context; not filesystem paths."}
            },
            "required":["target","objective"]
        }),
    )
}

pub struct StartDeliveryTask {
    source_workspace: String,
    service: Arc<dyn Service>,
    /// Required target-host-bound preview source. This is intentionally not
    /// optional: no delivery approval may fall back to synchronous or bare
    /// Git inspection from a source runner.
    preflight: Arc<dyn Preflight>,
    receipts: std::sync::Mutex<std::collections::HashMap<String, Receipt>>,
}

impl StartDeliveryTask {
    pub fn new(
        source_workspace: String,
        service: Arc<dyn Service>,
        preflight: Arc<dyn Preflight>,
    ) -> Self {
        Self {
            source_workspace,
            service,
            preflight,
            receipts: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    fn request(&self, arguments: &RawValue) -> Result<Request, ToolResult> {
        decode_strict_json(arguments.get(), &["target", "objective"])
            .map_err(|error| error_result(&error).not_started())
            .and_then(|args: ToolArgs| {
                Request::try_from(args).map_err(|error| error_result(&error).not_started())
            })
    }

    fn spec(preview: &Preview) -> ApprovalSpec {
        let details = json!({
            "source_workspace": preview.source_workspace,
            "target_repository": preview.plan.target.repository,
            "target_common_dir": preview.plan.target.common_dir,
            "worktree": preview.plan.path,
            "branch": preview.plan.branch,
            "base": preview.base,
            "handoff_digest": preview.handoff.digest,
            "handoff_bytes": preview.handoff.bytes,
        })
        .to_string();
        ApprovalSpec {
            version: hash(&details),
            source: "delivery".into(),
            authority: otto_core::approval::Authority::Boundary,
            title: "Start a cross-workspace delivery task?".into(),
            reason: "Creates one target-local worktree after approval; GitHub push and PR permissions remain separate.".into(),
            details,
            persistent: false,
        }
    }
}

pub(crate) const START_DELIVERY_TASK_RETRY: (&str, otto_core::model::RetrySafety) = (
    "start_delivery_task",
    otto_core::model::RetrySafety::NonIdempotent,
);

#[async_trait]
impl Tool for StartDeliveryTask {
    fn retry_safety(&self) -> otto_core::model::RetrySafety {
        START_DELIVERY_TASK_RETRY.1
    }

    fn definition(&self) -> ToolDefinition {
        start_definition()
    }

    fn approval_spec(&self, _arguments: &RawValue) -> Result<Option<ApprovalSpec>, ToolResult> {
        Err(
            error_result("start_delivery_task requires asynchronous target preflight")
                .not_started(),
        )
    }

    async fn approval_spec_async(
        &self,
        arguments: &RawValue,
        cancel: &CancellationToken,
    ) -> Result<Option<ApprovalSpec>, ToolResult> {
        let request = self.request(arguments)?;
        let preview = self
            .preflight
            .preview(&self.source_workspace, &request, cancel)
            .await
            .map_err(|error| error_result(&error).not_started())?;
        Ok(Some(Self::spec(&preview)))
    }

    async fn apply_approval(
        &self,
        arguments: &RawValue,
        cancel: &CancellationToken,
        approval: &otto_core::approval::Request,
    ) -> Result<(), String> {
        if cancel.is_cancelled() {
            return Err("delivery approval cancelled".into());
        }
        let request = self.request(arguments).map_err(|result| result.content)?;
        let preview = self
            .preflight
            .preview(&self.source_workspace, &request, cancel)
            .await?;
        let current = Self::spec(&preview);
        if current.details != approval.subject.details
            || current.version != approval.subject.version
        {
            return Err("delivery target changed after approval".into());
        }
        let receipt = self.service.prepare(&approval.id, &preview).await?;
        self.receipts
            .lock()
            .map_err(|_| "delivery receipt store unavailable")?
            .insert(approval.id.clone(), receipt);
        Ok(())
    }

    async fn execute(&self, _arguments: &RawValue, _cancel: &CancellationToken) -> ToolResult {
        error_result("start_delivery_task requires the approval-enabled tool registry")
            .not_started()
    }

    async fn execute_approved(
        &self,
        _call: ToolCall<'_>,
        cancel: &CancellationToken,
        approval: &otto_core::approval::Request,
    ) -> ToolResult {
        let receipt = match self.receipts.lock() {
            Ok(receipts) => receipts.get(&approval.id).cloned(),
            Err(_) => return error_result("delivery receipt store unavailable").not_started(),
        };
        let Some(receipt) = receipt else {
            return error_result("delivery approval has no prepared receipt").not_started();
        };
        match self.service.start(&receipt, cancel).await {
            Ok(message) => text_result(message),
            Err(error) => error_result(error),
        }
    }
}

#[cfg(test)]
mod tests;
