//! The tool executor of the evaluation: a tool set with no tools.

use otto_core::model::{EffectCertainty, OperationDisposition, OperationOutcome, ToolDefinition};
use otto_core::operation::OperationControl;
use otto_core::tool::{ToolCall, ToolExecution, ToolExecutor, ToolResult};

/// A tool set with no tools.
pub struct NoTools;

#[async_trait::async_trait]
impl ToolExecutor for NoTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        Vec::new()
    }

    async fn execute(&self, call: ToolCall<'_>, _control: &dyn OperationControl) -> ToolExecution {
        unknown_tool(call.name)
    }
}

fn unknown_tool(name: &str) -> ToolExecution {
    ToolExecution {
        result: ToolResult::unknown_tool(name),
        outcome: OperationOutcome {
            disposition: OperationDisposition::Error,
            effect_certainty: EffectCertainty::NotStarted,
            stop_reason: None,
        },
    }
}
