//! Workspace-confined file tools, plus in-process `remind`.
//!
//! Every filesystem tool resolves paths through [`workspace::Workspace`], which
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

pub mod bash;
pub mod edit;
pub mod find;
pub(crate) mod gopath;
pub mod grep;
pub mod ls;
pub mod mcp;
pub mod memory;
pub mod read;
pub mod registry;
pub mod remind;
pub mod result;
pub(crate) mod root;
mod search;
pub mod skill;
pub mod workspace;
pub mod write;

use kite_core::model::ToolDefinition;
use kite_core::tool::ToolResult;
use serde::Deserialize;
use serde_json::value::RawValue;
use tokio_util::sync::CancellationToken;

/// The text reported when the request is already cancelled.
pub(crate) const CONTEXT_CANCELED: &str = "context canceled";

/// One callable tool.
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    /// The schema advertised to the provider. The returned value is owned by
    /// the caller.
    fn definition(&self) -> ToolDefinition;

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

    /// The default output cap every tool constructor is given.
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

    /// Creates a file and its parent directories under `root`.
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
