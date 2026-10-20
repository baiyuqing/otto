//! Parent-only conversational access to the workspace workflow controller.
use super::{Tool, definition, text_result};
use crate::workflow::Controller;
use otto_core::model::{RetrySafety, ToolDefinition};
use otto_core::tool::{ToolCall, ToolResult};
use serde::Deserialize;
use serde_json::{json, value::RawValue};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub const NAMES: [&str; 4] = [
    "workflow_list",
    "workflow_inspect",
    "workflow_start",
    "workflow_status",
];
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Named {
    name: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Start {
    name: String,
    input: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Status {
    run_id: String,
}

/// Identity is injected by parent composition; arguments cannot select it.
pub(crate) struct Parent {
    controller: Result<Arc<Controller>, String>,
    session: String,
    durable: bool,
}
impl Parent {
    pub fn new(
        controller: Result<Arc<Controller>, String>,
        session: String,
        durable: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            controller,
            session,
            durable,
        })
    }
    pub fn tools(self: &Arc<Self>) -> Vec<Box<dyn Tool + Send + Sync>> {
        NAMES
            .iter()
            .map(|name| {
                Box::new(WorkflowTool {
                    parent: self.clone(),
                    name,
                }) as Box<dyn Tool + Send + Sync>
            })
            .collect()
    }
}
struct WorkflowTool {
    parent: Arc<Parent>,
    name: &'static str,
}

#[cfg(test)]
mod tests;
fn identifier(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
    {
        return Err("name/run_id must be 1..128 ASCII letters, digits, _, - or .".into());
    }
    Ok(())
}
fn decode<T: serde::de::DeserializeOwned>(raw: &RawValue) -> Result<T, String> {
    serde_json::from_str(raw.get()).map_err(|error| format!("invalid workflow arguments: {error}"))
}
pub(crate) fn bounded(value: serde_json::Value) -> ToolResult {
    let text = value.to_string();
    if text.len() <= 65536 {
        text_result(text)
    } else {
        text_result(json!({"truncated":true,"summary":text.chars().take(7000).collect::<String>(),"guidance":"Use workflow_inspect for one definition or workflow_status for one run; full results remain in workflow storage."}).to_string())
    }
}
fn excerpt(value: &str) -> String {
    value.chars().take(2048).collect()
}
pub(crate) fn status(controller: &Controller, id: &str) -> Result<serde_json::Value, String> {
    let run = controller.get(id).map_err(|e| e.to_string())?;
    let requests = controller.requests(id).map_err(|e| e.to_string())?;
    Ok(
        json!({"run_id":run.id,"workflow":run.workflow,"status":run.status,
        "steps":run.steps.iter().map(|step| json!({"id":step.id,"kind":step.kind,"status":step.status,"attempt":step.attempt,"result":excerpt(&step.result),"error":excerpt(&step.error),"truncated":step.result.chars().count()>2048 || step.error.chars().count()>2048})).collect::<Vec<_>>(),
        "requests":requests,"guidance":"Human gates require an actual human decision via approvals or `otto workflow approve REQUEST_ID` / `otto workflow reject REQUEST_ID`. Paused/interrupted: inspect possible effects first; a paused run requires selecting a failed/interrupted step for explicit retry with `otto workflow resume RUN_ID --retry STEP_ID`. `otto workflow resume RUN_ID` without --retry does not retry failed steps and rejects paused runs. Never automatically retry uncertain effects."}),
    )
}
#[async_trait::async_trait]
impl Tool for WorkflowTool {
    fn definition(&self) -> ToolDefinition {
        let (description, properties, required) = match self.name {
            "workflow_list" => (
                "List existing workflow summaries, source paths and discovery diagnostics (100 definitions, 20 diagnostics maximum). Empty catalog includes setup guidance; catalog changes require owner restart. Does not execute anything.",
                json!({}),
                json!([]),
            ),
            "workflow_inspect" => (
                "Inspect one existing workflow's snapshot, steps, dependencies, named agents, tools, write policies and human gates. Does not execute or grant permission. Output capped at 64KiB.",
                json!({"name":{"type":"string","minLength":1,"maxLength":128,"description":"Exact discovered workflow name."}}),
                json!(["name"]),
            ),
            "workflow_start" => (
                "Start one authorized existing workflow asynchronously; returns run_id/status and delivery limits. Input is a string up to 65536 UTF-8 bytes. Durable operation receipt prevents duplicate same-operation starts. Human gates remain human-owned; no automatic uncertain-effect retry. Owner process must remain alive.",
                json!({"name":{"type":"string","minLength":1,"maxLength":128,"description":"Exact inspected workflow name."},"input":{"type":"string","maxLength":65536,"description":"Existing workflow's plain text domain input, at most 65536 UTF-8 bytes."}}),
                json!(["name", "input"]),
            ),
            _ => (
                "Read one workspace workflow run's status, step results/errors (2048 characters each), human gate IDs and recovery guidance. Does not wait, poll, approve or retry. Output capped at 64KiB.",
                json!({"run_id":{"type":"string","minLength":1,"maxLength":128,"description":"Run ID returned by workflow_start."}}),
                json!(["run_id"]),
            ),
        };
        definition(
            self.name,
            description,
            json!({"type":"object","additionalProperties":false,"properties":properties,"required":required}),
        )
    }
    fn approval_spec(
        &self,
        arguments: &RawValue,
    ) -> Result<Option<super::ApprovalSpec>, ToolResult> {
        if self.name != "workflow_start" {
            return Ok(None);
        }
        crate::workflow::provenance::admit_start()
            .map_err(|error| ToolResult::error(error.to_string()))?;
        let args: Start = decode(arguments).map_err(ToolResult::error)?;
        identifier(&args.name).map_err(ToolResult::error)?;
        if args.input.len() > 65536 {
            return Err(ToolResult::error(
                "workflow input exceeds 65536 UTF-8 bytes",
            ));
        }
        let controller = self
            .parent
            .controller
            .as_ref()
            .map_err(|error| ToolResult::error(error.clone()))?;
        let definition = match controller.inspect(&args.name) {
            Ok(definition) => definition,
            // A removed definition cannot admit a new effectful start. Let the
            // trusted call-aware execution reconcile an existing receipt first;
            // otherwise start_parent returns not-found without creating a run.
            Err(_) => return Ok(None),
        };
        Ok(Some(super::ApprovalSpec {
            version: crate::approval::digest(&format!("{}:{}", definition.hash, arguments.get())),
            source: "workflow_start".into(), authority: otto_core::approval::Authority::Bounded,
            title: format!("Start workflow {}?",args.name),
            reason: "Runs the inspected ordered process asynchronously using live tool/sandbox policy. This admission never decides its human gates or authorizes uncertain-effect retry.".into(),
            details: json!({"name":args.name,"definition_hash":definition.hash,"steps":definition.steps,"agents":definition.agents,"input":args.input}).to_string(),
            persistent: false,
        }))
    }
    fn retry_safety(&self) -> RetrySafety {
        if self.name == "workflow_start" {
            RetrySafety::NonIdempotent
        } else {
            RetrySafety::ReadOnly
        }
    }
    async fn execute(&self, _arguments: &RawValue, _cancel: &CancellationToken) -> ToolResult {
        ToolResult::error(
            "workflow tools require a trusted parent tool-call identity; use the parent conversation",
        )
    }
    async fn execute_call(&self, call: ToolCall<'_>, cancel: &CancellationToken) -> ToolResult {
        if cancel.is_cancelled() {
            return ToolResult::error("context canceled");
        }
        if call.name != self.name {
            return ToolResult::error("workflow tool identity mismatch");
        }
        match self.invoke(call).await {
            Ok(value) => bounded(value),
            Err(error) => ToolResult::error(error),
        }
    }
}
impl WorkflowTool {
    async fn invoke(&self, call: ToolCall<'_>) -> Result<serde_json::Value, String> {
        let controller = self.parent.controller.as_ref().map_err(|error| format!("Workflow controller unavailable: {error}. Stop the other workspace owner, enable agents if disabled, then restart this owner. No run was admitted."))?;
        match self.name {
            "workflow_list" => {
                let _: Empty = decode(call.arguments)?;
                let (definitions, diagnostics) = controller.catalog();
                Ok(
                    json!({"definitions":definitions.iter().take(100).map(|d| json!({"name":d.name,"description":d.description,"source":d.source,"steps":d.steps.len(),"gates":d.steps.iter().filter(|s| s.kind==crate::workflow::StepKind::Approval).count()})).collect::<Vec<_>>(),"diagnostics":diagnostics.iter().take(20).collect::<Vec<_>>(),"truncated":definitions.len()>100 || diagnostics.len()>20,"guidance":"Place TOML definitions in ~/.otto/workflows or <workspace>/.otto/workflows, named agents in .otto/agents/<name>/AGENT.md, then restart the owner. Workspace names override home definitions. Empty discovery is normal on a fresh install; never invent a workflow."}),
                )
            }
            "workflow_inspect" => {
                let args: Named = decode(call.arguments)?;
                identifier(&args.name)?;
                let d = controller.inspect(&args.name).map_err(|e| e.to_string())?;
                Ok(
                    json!({"definition":d,"input":"Plain text string, at most 65536 UTF-8 bytes; no typed required input fields.","guidance":"Inspection grants no authority. Human approval steps require actual human decisions."}),
                )
            }
            "workflow_status" => {
                let args: Status = decode(call.arguments)?;
                identifier(&args.run_id)?;
                status(controller, &args.run_id)
            }
            _ => {
                let args: Start = decode(call.arguments)?;
                identifier(&args.name)?;
                if args.input.len() > 65536 {
                    return Err("workflow input exceeds 65536 UTF-8 bytes".into());
                }
                let run = controller
                    .start_parent(
                        &args.name,
                        &args.input,
                        &self.parent.session,
                        call.operation_id.as_str(),
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(
                    json!({"run_id":run.id,"status":run.status,"delivery":if self.parent.durable {"Lifecycle notices are bound to this exact source session. Closed sessions receive pending notices when reopened; /new never redirects them. The owning process must stay alive; restart recovery requires explicit resume."} else {"No durable source session: asynchronous conversational delivery is unavailable. Save this run_id and use workflow_status or `otto workflow status RUN_ID`; keep the owner process alive."}}),
                )
            }
        }
    }
}
