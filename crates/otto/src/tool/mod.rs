//! Workspace-confined tools.
//!
//! Port of `internal/tool`. Every filesystem tool resolves paths through
//! [`workspace::Workspace`], which rejects escapes from the selected workspace
//! after canonical-path and symlink validation, and every tool caps its output
//! and reports failures in band as an error `ToolResult` rather than as a
//! `Result`, because the text is fed back to the model.
//!
//! Ownership: a tool borrows its workspace and owns nothing the caller can
//! observe. Concurrency: [`Tool::execute`] takes `&self` and may run
//! concurrently; the edit tool serializes writes to one path itself.
//! Cancellation: long-running tools check the token between filesystem steps
//! and return the Go `context canceled` text.

pub mod bash;
pub mod edit;
pub mod find;
pub(crate) mod gopath;
pub mod grep;
pub mod ls;
pub mod read;
pub mod registry;
pub mod result;
mod root;
mod search;
pub mod skill;
pub mod workspace;
pub mod write;

use otto_core::model::ToolDefinition;
use otto_core::tool::ToolResult;
use serde_json::value::RawValue;
use tokio_util::sync::CancellationToken;

/// The text Go reports when the request context is already cancelled.
pub(crate) const CONTEXT_CANCELED: &str = "context canceled";

/// One callable tool. Port of the `Tool` interface in
/// `internal/tool/registry.go`.
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    /// The schema advertised to the provider. The returned value is owned by
    /// the caller.
    fn definition(&self) -> ToolDefinition;

    /// Runs one call. `arguments` is borrowed for the duration of the call.
    async fn execute(&self, arguments: &RawValue, cancel: &CancellationToken) -> ToolResult;
}

/// Builds a tool definition from a JSON schema literal.
pub(crate) fn definition(
    name: &str,
    description: &str,
    parameters: serde_json::Value,
) -> ToolDefinition {
    ToolDefinition {
        name: name.to_owned(),
        description: description.to_owned(),
        parameters: Some(
            RawValue::from_string(parameters.to_string()).expect("schema literal is valid JSON"),
        ),
    }
}

/// The in-band error result for a failed tool call.
pub(crate) fn error_result(message: impl std::fmt::Display) -> ToolResult {
    ToolResult::error(message.to_string())
}

/// The in-band success result for `content`.
pub(crate) fn text_result(content: impl Into<String>) -> ToolResult {
    ToolResult {
        content: content.into(),
        ..ToolResult::default()
    }
}

#[cfg(test)]
pub(crate) mod testutil {
    use super::*;
    use std::path::Path;

    /// The default output cap the Go tests pass to every tool constructor.
    pub(crate) const MAX_OUTPUT_BYTES: usize = 51200;

    pub(crate) fn raw(json: &str) -> Box<RawValue> {
        RawValue::from_string(json.to_owned()).expect("test JSON is valid")
    }

    /// Runs `tool` with `arguments` and no cancellation.
    pub(crate) async fn run(tool: &dyn Tool, arguments: &str) -> ToolResult {
        tool.execute(&raw(arguments), &CancellationToken::new())
            .await
    }

    /// Runs `tool` with an already-cancelled token.
    pub(crate) async fn run_cancelled(tool: &dyn Tool, arguments: &str) -> ToolResult {
        let cancel = CancellationToken::new();
        cancel.cancel();
        tool.execute(&raw(arguments), &cancel).await
    }

    /// Creates a file and its parent directories under `root`. Port of the
    /// `writeSearchFile` helper in `search_tools_test.go`.
    pub(crate) fn write_search_file(root: &Path, name: &str, contents: &str) {
        let path = root.join(name);
        std::fs::create_dir_all(path.parent().expect("the path has a parent"))
            .expect("test directories are creatable");
        std::fs::write(path, contents).expect("test files are writable");
    }

    pub(crate) fn workspace(root: &Path) -> workspace::Workspace {
        workspace::Workspace::new(root).expect("workspace should open")
    }
}
