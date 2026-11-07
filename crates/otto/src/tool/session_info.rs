//! Read-only identity of the runner executing a tool call.

use super::{Tool, definition, error_result, preflight_error, text_result};
use otto_core::model::ToolDefinition;
use otto_core::tool::ToolResult;
use serde::Deserialize;
use serde_json::{json, value::RawValue};
use tokio_util::sync::CancellationToken;

pub(crate) struct SessionInfo {
    pub(crate) session_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {}

#[async_trait::async_trait]
impl Tool for SessionInfo {
    fn definition(&self) -> ToolDefinition {
        definition(
            "session_info",
            "Read this runner's own session identity. Takes an empty object {}; returns only {session_id}. Read-only; no paths or history. An in-memory ID does not imply persistence.",
            json!({"type":"object", "properties":{}, "additionalProperties":false}),
        )
    }

    fn retry_safety(&self) -> otto_core::model::RetrySafety {
        otto_core::model::RetrySafety::ReadOnly
    }

    async fn execute(&self, arguments: &RawValue, cancel: &CancellationToken) -> ToolResult {
        if cancel.is_cancelled() {
            return preflight_error(super::CONTEXT_CANCELED);
        }
        if !matches!(
            serde_json::from_str::<serde_json::Value>(arguments.get()),
            Ok(serde_json::Value::Object(_))
        ) {
            return preflight_error("session_info arguments must be an empty object");
        }
        if let Err(error) = super::result::decode_strict_json::<Args>(arguments.get(), &[]) {
            return error_result(error.to_string());
        }
        text_result(json!({"session_id": self.session_id}).to_string())
    }
}

#[cfg(test)]
#[path = "session_info_tests.rs"]
mod tests;
