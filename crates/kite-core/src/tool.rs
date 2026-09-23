//! The tool execution contract.
//!
//! Phase 0 carries only the parts the agent loop depends on; the concrete tools
//! and the registry arrive in phase 3.
//!
//! Ownership: `arguments` is borrowed read-only for the duration of the call
//! and must not be retained or mutated. The returned [`ToolResult`] belongs to
//! the caller.
//!
//! Concurrency and cancellation: `execute` takes `&self` and may be called
//! concurrently on a shared executor. A cancelled token means the executor
//! should stop and return an error result rather than block.
//!
//! Errors: tool failures are reported in band as a [`ToolResult`] with
//! `is_error` set, never as a `Result`, because the failure text is fed back to
//! the model as the tool result.

use serde_json::value::RawValue;
use tokio_util::sync::CancellationToken;

use crate::model::ToolDefinition;

/// The outcome of one tool call.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolResult {
    /// The text delivered to the live frontend event.
    pub content: String,
    /// Replaces `content` in the durable transcript when present. `Some("")`
    /// is an intentional empty override; `content` still reaches the frontend
    /// unchanged.
    pub persisted_content: Option<String>,
    pub is_error: bool,
}

impl ToolResult {
    /// The result an executor returns for a name it does not serve.
    pub fn unknown_tool(name: &str) -> Self {
        Self {
            content: format!("unknown tool: {name}"),
            persisted_content: None,
            is_error: true,
        }
    }

    /// An error result carrying `content` as its text.
    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            persisted_content: None,
            is_error: true,
        }
    }

    /// The text that belongs in the durable transcript.
    pub fn persisted_text(&self) -> &str {
        self.persisted_content.as_deref().unwrap_or(&self.content)
    }
}

/// Runs the tools the model may call.
///
/// See the module documentation for the ownership, concurrency, cancellation,
/// and error rules every implementation must follow.
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
pub trait ToolExecutor {
    /// The schemas advertised to the provider, in a stable order.
    fn definitions(&self) -> Vec<ToolDefinition>;

    /// Runs one call. A name this executor does not serve must produce
    /// [`ToolResult::unknown_tool`].
    async fn execute(
        &self,
        name: &str,
        arguments: &RawValue,
        cancel: &CancellationToken,
    ) -> ToolResult;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_tool_result_names_the_tool() {
        let result = ToolResult::unknown_tool("nope");
        assert_eq!(result.content, "unknown tool: nope");
        assert!(result.is_error);
        assert!(result.persisted_content.is_none());
    }

    #[test]
    fn persisted_text_prefers_the_override_including_an_empty_one() {
        let plain = ToolResult {
            content: "live".into(),
            persisted_content: None,
            is_error: false,
        };
        assert_eq!(plain.persisted_text(), "live");
        let overridden = ToolResult {
            content: "live".into(),
            persisted_content: Some(String::new()),
            is_error: false,
        };
        assert_eq!(overridden.persisted_text(), "");
    }
}
