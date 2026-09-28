//! The tool execution contract.
//!
//! Ownership: call arguments are borrowed read-only for the duration of the
//! call and must not be retained or mutated. The returned [`ToolExecution`]
//! belongs to the caller.
//!
//! Concurrency and cancellation: `execute` takes `&self` and may be called
//! concurrently on a shared executor. Executors report cancellation and the
//! certainty of any externally visible effects in the typed outcome.

use serde_json::value::RawValue;
use tokio_util::sync::CancellationToken;

use crate::model::{
    EffectCertainty, OperationDisposition, OperationId, OperationOutcome, OperationStopReason,
    ToolDefinition,
};

/// The model request for one attempt of a logical tool operation.
#[derive(Debug, Clone, Copy)]
pub struct ToolCall<'a> {
    pub operation_id: &'a OperationId,
    pub name: &'a str,
    pub arguments: &'a RawValue,
    pub attempt: u32,
}

/// The model-visible result and its typed operation settlement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolExecution {
    pub result: ToolResult,
    pub outcome: OperationOutcome,
}

impl ToolExecution {
    /// A definitive normal return from a concrete tool.
    pub fn completed(mut result: ToolResult) -> Self {
        let disposition = if result.is_error {
            OperationDisposition::Error
        } else {
            OperationDisposition::Succeeded
        };
        result.outcome_override = None;
        Self {
            result,
            outcome: OperationOutcome {
                disposition,
                effect_certainty: EffectCertainty::Completed,
                stop_reason: None,
            },
        }
    }
}

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
    /// Native tools use this only when their concrete boundary can establish a
    /// more conservative effect certainty than the registry's normal mapping.
    pub outcome_override: Option<OperationOutcome>,
}

impl ToolResult {
    /// The result an executor returns for a name it does not serve.
    pub fn unknown_tool(name: &str) -> Self {
        Self {
            content: format!("unknown tool: {name}"),
            persisted_content: None,
            is_error: true,
            outcome_override: None,
        }
    }

    /// An error result carrying `content` as its text.
    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            persisted_content: None,
            is_error: true,
            outcome_override: None,
        }
    }

    /// The text that belongs in the durable transcript.
    pub fn persisted_text(&self) -> &str {
        self.persisted_content.as_deref().unwrap_or(&self.content)
    }

    /// Marks an error before the effectful boundary was crossed.
    pub fn not_started(mut self) -> Self {
        self.outcome_override = Some(OperationOutcome {
            disposition: OperationDisposition::Error,
            effect_certainty: EffectCertainty::NotStarted,
            stop_reason: None,
        });
        self
    }

    /// Marks cancellation observed before dispatch.
    pub fn cancelled_not_started(mut self) -> Self {
        self.outcome_override = Some(OperationOutcome {
            disposition: OperationDisposition::Cancelled,
            effect_certainty: EffectCertainty::NotStarted,
            stop_reason: Some(OperationStopReason::UserCancellation),
        });
        self
    }

    /// Marks cancellation after dispatch, when effects cannot be established.
    pub fn cancelled_unknown(mut self) -> Self {
        self.outcome_override = Some(OperationOutcome {
            disposition: OperationDisposition::Cancelled,
            effect_certainty: EffectCertainty::Unknown,
            stop_reason: Some(OperationStopReason::UserCancellation),
        });
        self
    }

    /// Marks a deadline after dispatch, when effects cannot be established.
    pub fn deadline_unknown(mut self) -> Self {
        self.outcome_override = Some(OperationOutcome {
            disposition: OperationDisposition::DeadlineExceeded,
            effect_certainty: EffectCertainty::Unknown,
            stop_reason: Some(OperationStopReason::Deadline),
        });
        self
    }

    /// Marks a lost transport after dispatch, when effects cannot be established.
    pub fn transport_unknown(mut self) -> Self {
        self.outcome_override = Some(OperationOutcome {
            disposition: OperationDisposition::Error,
            effect_certainty: EffectCertainty::Unknown,
            stop_reason: None,
        });
        self
    }
}

/// Runs the tools the model may call.
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
pub trait ToolExecutor {
    /// The schemas advertised to the provider, in a stable order.
    fn definitions(&self) -> Vec<ToolDefinition>;

    /// Runs one attempt. A name this executor does not serve must settle as
    /// `Error + NotStarted` with [`ToolResult::unknown_tool`].
    async fn execute(&self, call: ToolCall<'_>, cancel: &CancellationToken) -> ToolExecution;
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
        assert!(result.outcome_override.is_none());
    }

    #[test]
    fn persisted_text_prefers_the_override_including_an_empty_one() {
        let plain = ToolResult {
            content: "live".into(),
            persisted_content: None,
            is_error: false,
            outcome_override: None,
        };
        assert_eq!(plain.persisted_text(), "live");
        let overridden = ToolResult {
            content: "live".into(),
            persisted_content: Some(String::new()),
            is_error: false,
            outcome_override: None,
        };
        assert_eq!(overridden.persisted_text(), "");
    }
}
