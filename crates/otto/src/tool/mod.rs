//! The `Tool` trait, the `Registry`, and the workspace-confined file tools, plus
//! `list_models`. Feature tools (`remind`, `schedule`, sub-agents, workflows,
//! delivery, task control) live in their feature modules.
//!
//! Every filesystem tool resolves paths through [`crate::workspace::confined::Workspace`], which
//! rejects escapes from the selected workspace after canonical-path and symlink
//! validation, and every tool caps its output and reports failures in band as
//! an error `ToolResult` rather than as a `Result`, because the text is fed
//! back to the model.
//!
//! Ownership: a tool borrows its workspace and owns nothing the caller can
//! observe. Concurrency: [`Tool::execute`] takes `&self` and may run
//! concurrently; the edit tool serializes writes to one path itself.
//! Cancellation: long-running tools check the token between filesystem steps
//! and return the `context canceled` text.

pub mod approval;
pub mod bash;
pub(crate) mod bash_output;
pub mod edit;
pub mod find;
pub(crate) mod gitignore;
pub mod grep;
pub mod ls;
pub mod mcp;
pub mod memory;
pub mod memory_service;
pub mod models;
pub mod otto;
pub mod plan;
pub mod project_guard;
pub mod read;
pub mod registry;
pub mod result;
mod search;
pub(crate) mod session_directory;
pub(crate) mod session_info;
pub mod skill;
pub mod web;
pub mod write;

use otto_core::model::ToolDefinition;
use otto_core::tool::ToolResult;
use serde::Deserialize;
use serde_json::value::RawValue;
use tokio_util::sync::CancellationToken;

/// The text reported when the request is already cancelled.
pub(crate) const CONTEXT_CANCELED: &str = "context canceled";

pub struct ApprovalSpec {
    pub version: String,
    pub source: String,
    /// The authority class this tool's authorization reaches for; see
    /// [`otto_core::approval::Authority`]. A tool that stays inside the sandbox and
    /// the workspace declares `Bounded`, which the session AUTO policy may
    /// decide; anything else always waits for the user.
    pub authority: otto_core::approval::Authority,
    pub title: String,
    pub reason: String,
    pub details: String,
    pub persistent: bool,
}

/// One callable tool.
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    /// The schema advertised to the provider. The returned value is owned by
    /// the caller.
    fn definition(&self) -> ToolDefinition;

    /// Tool-owned retry safety, never supplied by model arguments or config.
    /// Unknown/custom tools conservatively forbid automatic re-execution.
    fn retry_safety(&self) -> otto_core::model::RetrySafety {
        otto_core::model::RetrySafety::NonIdempotent
    }

    fn approval_spec(&self, _arguments: &RawValue) -> Result<Option<ApprovalSpec>, ToolResult> {
        Ok(None)
    }

    /// Prepares an approval subject before it is persisted and again after the
    /// user decides. The default preserves every existing synchronous tool;
    /// target-bound tools may override it for sandboxed asynchronous preflight.
    async fn approval_spec_async(
        &self,
        arguments: &RawValue,
        _cancel: &CancellationToken,
    ) -> Result<Option<ApprovalSpec>, ToolResult> {
        self.approval_spec(arguments)
    }

    async fn apply_approval(
        &self,
        _arguments: &RawValue,
        _cancel: &CancellationToken,
        _request: &otto_core::approval::Request,
    ) -> Result<(), String> {
        Ok(())
    }

    /// Whether a successful call requests ending this provider turn at the
    /// next safe boundary. The registry checks its settled outcome first.
    fn yields_turn(&self) -> bool {
        false
    }

    fn batch_control(&self) -> otto_core::approval::BatchControl {
        otto_core::approval::BatchControl::Continue
    }

    async fn execute_approved(
        &self,
        call: otto_core::tool::ToolCall<'_>,
        cancel: &CancellationToken,
        _request: &otto_core::approval::Request,
    ) -> ToolResult {
        self.execute_call(call, cancel).await
    }

    async fn execute_call(
        &self,
        call: otto_core::tool::ToolCall<'_>,
        cancel: &CancellationToken,
    ) -> ToolResult {
        self.execute(call.arguments, cancel).await
    }

    /// Runs one call. `arguments` is borrowed for the duration of the call.
    async fn execute(&self, arguments: &RawValue, cancel: &CancellationToken) -> ToolResult;
}

/// Deserializes an optional list argument, reading an empty list the same as
/// `null` or an absent key.
///
/// Models emit `[]` in the position of an argument they are not using, beside
/// the arguments they are using. A tool that distinguishes the two rejects
/// calls it can otherwise serve, and the model cannot retry its way out: the
/// empty list is the shape it produces. Every optional list argument uses
/// this; `tests/tool_argument_contract.rs` fails on one that does not.
pub(crate) fn empty_as_none<'de, D, T>(deserializer: D) -> Result<Option<Vec<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    let items = Option::<Vec<T>>::deserialize(deserializer)?;
    Ok(items.filter(|items| !items.is_empty()))
}

/// Builds a tool definition from a JSON schema literal.
pub fn definition(name: &str, description: &str, parameters: serde_json::Value) -> ToolDefinition {
    ToolDefinition {
        name: name.to_owned(),
        description: if otto_core::tool::redaction::guards_arguments(name) {
            format!(
                "{}. {}",
                description.trim_end_matches('.'),
                otto_core::tool::redaction::ARGUMENT_POLICY
            )
        } else {
            description.to_owned()
        },
        parameters: Some(
            RawValue::from_string(parameters.to_string()).expect("schema literal is valid JSON"),
        ),
    }
}

/// The in-band error result for a failed tool call.
pub(crate) fn error_result(message: impl std::fmt::Display) -> ToolResult {
    ToolResult::error(message.to_string())
}

/// An error established before a tool crosses its effectful boundary.
pub(crate) fn preflight_error(message: impl std::fmt::Display) -> ToolResult {
    error_result(message).not_started()
}

/// The in-band success result for `content`.
pub(crate) fn text_result(content: impl Into<String>) -> ToolResult {
    ToolResult {
        content: content.into(),
        ..ToolResult::default()
    }
}

#[cfg(any(test, feature = "test-support"))]
pub mod testutil {
    use super::*;
    use std::path::Path;

    /// The default output cap every tool constructor is given.
    pub const MAX_OUTPUT_BYTES: usize = 51200;

    pub fn raw(json: &str) -> Box<RawValue> {
        RawValue::from_string(json.to_owned()).expect("test JSON is valid")
    }

    /// Runs `tool` with `arguments` and no cancellation.
    pub async fn run(tool: &dyn Tool, arguments: &str) -> ToolResult {
        tool.execute(&raw(arguments), &CancellationToken::new())
            .await
    }

    /// Runs `tool` with an already-cancelled token.
    pub async fn run_cancelled(tool: &dyn Tool, arguments: &str) -> ToolResult {
        let cancel = CancellationToken::new();
        cancel.cancel();
        tool.execute(&raw(arguments), &cancel).await
    }

    /// Creates a file and its parent directories under `root`.
    pub fn write_search_file(root: &Path, name: &str, contents: &str) {
        let path = root.join(name);
        std::fs::create_dir_all(path.parent().expect("the path has a parent"))
            .expect("test directories are creatable");
        std::fs::write(path, contents).expect("test files are writable");
    }

    pub fn workspace(root: &Path) -> std::sync::Arc<crate::workspace::confined::Workspace> {
        std::sync::Arc::new(
            crate::workspace::confined::Workspace::new(root).expect("workspace should open"),
        )
    }
}
