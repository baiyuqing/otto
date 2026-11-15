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
use otto_platform::hex::to_hex;
use std::sync::Mutex;

pub const TOOL_NAME: &str = "start_delivery_task";
const MAX_OBJECTIVE_BYTES: usize = 16 * 1024;
const MAX_SOURCE_REFS: usize = 32;
const MAX_SOURCE_REF_BYTES: usize = 4 * 1024;

pub(crate) const HANDOFF_MAX_BYTES: usize = 64 * 1024;

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
pub(crate) fn worktree_add_argv(plan: &WorktreePlan, base: &str) -> Result<Vec<String>, String> {
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
    to_hex(&digest.finalize())
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
        let admitted = crate::workspace::admission::admit_workspace(
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

mod receipt;
mod tool;

pub use receipt::*;
pub use tool::*;

#[cfg(test)]
mod receipt_owner_tests;
#[cfg(test)]
mod tests;
