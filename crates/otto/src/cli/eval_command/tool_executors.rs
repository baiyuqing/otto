//! Tool executors of the evaluation. `zoom` and `date` exist only here; they are not
//! registered in the product `Registry`.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use otto_core::model::{
    EffectCertainty, OperationDisposition, OperationOutcome, RetrySafety, ToolDefinition,
};
use otto_core::operation::OperationControl;
use otto_core::tool::{ToolCall, ToolExecution, ToolExecutor, ToolResult};
use otto_core::view_tree::{LogMessage, SummaryTree, date, tool_definitions, zoom};
use serde::{Deserialize, Serialize};

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

/// One tool call a model made, in call order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallRecord {
    pub tool: String,
    /// The raw JSON arguments.
    pub arguments: String,
    /// Parsed `id` and `n` of a `zoom` call; `id` alone for `date`.
    pub id: Option<u64>,
    pub n: Option<u64>,
    /// Whether the tool returned an error text.
    pub error: bool,
}

#[derive(Deserialize)]
struct ZoomArguments {
    id: u64,
    n: u64,
}

#[derive(Deserialize)]
struct DateArguments {
    id: u64,
}

/// `zoom` and `date` over a built tree; records every call.
pub struct TreeTools {
    tree: Arc<SummaryTree>,
    log: Arc<Vec<LogMessage>>,
    calls: Arc<Mutex<Vec<ToolCallRecord>>>,
}

impl TreeTools {
    pub fn new(tree: Arc<SummaryTree>, log: Arc<Vec<LogMessage>>) -> Self {
        Self {
            tree,
            log,
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// The record the tools append to; it stays readable after the tools move into an agent.
    pub fn recorded_calls(&self) -> Arc<Mutex<Vec<ToolCallRecord>>> {
        Arc::clone(&self.calls)
    }

    fn record(&self, record: ToolCallRecord) {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(record);
    }
}

#[async_trait::async_trait]
impl ToolExecutor for TreeTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        tool_definitions()
    }

    fn retry_safety(&self, _name: &str) -> RetrySafety {
        RetrySafety::ReadOnly
    }

    async fn execute(&self, call: ToolCall<'_>, _control: &dyn OperationControl) -> ToolExecution {
        let arguments = call.arguments.get();
        let (id, n, outcome) = match call.name {
            "zoom" => match serde_json::from_str::<ZoomArguments>(arguments) {
                Ok(parsed) => (
                    Some(parsed.id),
                    Some(parsed.n),
                    zoom(&self.tree, &self.log, parsed.id, parsed.n),
                ),
                Err(error) => (
                    None,
                    None,
                    Err(format!("zoom needs integer arguments id and n: {error}")),
                ),
            },
            "date" => match serde_json::from_str::<DateArguments>(arguments) {
                Ok(parsed) => (Some(parsed.id), None, date(&self.log, parsed.id)),
                Err(error) => (
                    None,
                    None,
                    Err(format!("date needs an integer argument id: {error}")),
                ),
            },
            other => return unknown_tool(other),
        };
        self.record(ToolCallRecord {
            tool: call.name.to_string(),
            arguments: arguments.to_string(),
            id,
            n,
            error: outcome.is_err(),
        });
        ToolExecution::completed(match outcome {
            Ok(text) => ToolResult {
                content: text,
                ..ToolResult::default()
            },
            Err(text) => ToolResult::error(text),
        })
    }
}

/// The compressor carries the same definitions as answering turns so the `[tools][system]`
/// prefix is shared, but every call it makes is answered with an error and counted.
pub struct CompressorTools {
    violations: Arc<AtomicU32>,
}

impl CompressorTools {
    pub fn new(violations: Arc<AtomicU32>) -> Self {
        Self { violations }
    }
}

#[async_trait::async_trait]
impl ToolExecutor for CompressorTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        tool_definitions()
    }

    async fn execute(&self, call: ToolCall<'_>, _control: &dyn OperationControl) -> ToolExecution {
        self.violations.fetch_add(1, Ordering::SeqCst);
        ToolExecution::completed(ToolResult::error(format!(
            "tool {} is not available while writing a summary; reply with the summary text",
            call.name
        )))
    }
}
