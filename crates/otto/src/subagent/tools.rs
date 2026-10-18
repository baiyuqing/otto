//! The parent-facing sub-agent tools: `agent`, `agent_status`, `agent_send`
//! `agent_yield` and `agent_cancel`.
//!
//! Ownership: each tool holds an `Arc` to the session's [`Runner`] and owns
//! nothing else. `agent` always returns as soon as the task is registered;
//! completion is delivered through the parent's mailbox and a runtime-owned
//! wake turn, never by parking the parent agent loop in a tool call. Errors are
//! reported in band, as the tool contract requires.
//!
//! Security: `agent` is the only way a turn can start a child, and the runner
//! decides what that child may call. These three tools are themselves excluded
//! from every child tool set, so a child cannot delegate further.

use std::sync::Arc;

use otto_core::model::ToolDefinition;
use otto_core::tool::ToolResult;
use serde::Deserialize;
use serde_json::value::RawValue;
use tokio_util::sync::CancellationToken;

use super::format::{first_runes, task_label};
use super::runner::{Runner, StartRequest, agent_label, definition_label, pluralize_tools};
use super::tasks::{Task, TaskStatus, Tasks};
use super::{Catalog, runner};
use crate::tool::result::decode_strict_json;
use crate::tool::{CONTEXT_CANCELED, Tool, definition, error_result, text_result};

const AGENT_DESCRIPTION: &str = "Start a sub-agent on a self-contained task and return immediately with its task id. The sub-agent runs in parallel with you, has its own context (fresh unless context is \"inherit\" or fork_before is set), the same workspace and file tools, and receives follow-up parent task updates only when you call agent_send. Research stays in the child context: use fresh context, download pinned sources once into its temporary research directory, and return a summary, uncertainty, report path and section index within 4 KiB; put longer findings in the assigned persistent report directory using write when permitted. Oversized final answers are automatically saved when workspace storage is available. Temporary research directories are removed at task exit; report directories persist. Its final report arrives later as a [task-notification] message that starts a runtime-owned wake turn when the parent is idle; when only child dependencies remain, call agent_yield rather than polling agent_status. Use agent_status to inspect progress or retrieve a report path, and agent_send when the user adds constraints, priorities, facts, or direction changes for a queued or running task. For implementation, settle shared interfaces first and assign each writable file to one owner. Delegate a complete coherent change, including its call-site adaptations; keep tightly coupled edits in one task. Assign a bounded file scope and request findings with file/line evidence. After delegation, do not read those same files again; rely on the child report, use read on its report file for omitted sections, and ask follow-up questions only while the child is queued or running. Put the initial goal, relevant paths, constraints, and requested report format in prompt; pass agent to use a named definition from the Agents list.";

const AGENT_YIELD_DESCRIPTION: &str = "Yield this parent turn when only child dependencies remain and there is no independent work to do. Returns immediately and requests a safe-boundary stop of model calls after this tool batch settles; child messages/completion or user input resume the parent through the runtime. Does not wait in the tool, poll, or cancel children. Takes no arguments. Returns an error without yielding if no child is queued or running. Pending input, tool errors and approval replanning take precedence.";

const AGENT_STATUS_DESCRIPTION: &str = "Show task status and a bounded final conclusion (at most 4 KiB plus truncation and report-file metadata). Recent research steps are omitted by default; set include_history only when explicitly needed. For a truncated final answer, returns its persistent report path when storage succeeds; use read on that file to retrieve sections without restarting the child. Omit task_id to list tasks. When only child dependencies remain, call agent_yield instead of polling. Live detail distinguishes tool, provider/runtime and approval phases; unchanged last-tool counters do not establish a stall.";

const AGENT_SEND_DESCRIPTION: &str = "Send a task update to a queued or running sub-agent. Use this when the user adds constraints, clarifies priorities, provides new facts, or asks the child to adjust direction. You can also ask the child a question; the answer arrives later as a [task-report] notification. The child reads updates at the next safe checkpoint; this does not interrupt an in-flight provider or tool call. It cannot be sent to a finished task. Returns an error, and the child does not receive the update, when it cannot be saved to the child's pending-message file. Updates not yet read survive a restart and reach the child when it is resumed. To check progress instead, use agent_status, which reads the task registry and does not run the child model.";

const AGENT_CANCEL_DESCRIPTION: &str = "Cancel a queued or running sub-agent task. Use it for tasks that are no longer needed or have concrete failure/stall evidence. Do not infer a stall from an unchanged last tool or step count while the task is in provider/runtime or approval activity. Cancellation is cooperative: the task stops at its next safe checkpoint, a task inside a tool call stops once that call settles, and a queued task never starts. The task ends as canceled and produces the usual [task-notification].";

const AGENT_CONTEXT_DESCRIPTION: &str = "How the sub-agent starts. fresh (default, or the definition's setting): it sees only prompt. inherit: it also receives a copy of this conversation up to this call, or up to fork_before when that is set. Prefer, in order: (1) fresh with a self-contained prompt: goal, paths, constraints, what to report back; (2) fresh, with the context you already obtained pasted into prompt (file excerpts, tool output, decisions), so the sub-agent skips the tool calls that produced it; (3) inherit, when that context is too large or too scattered to paste and the sub-agent would otherwise repeat expensive tool calls. A sub-agent never shares your prompt cache, so inherit costs one full uncached pass over this conversation per sub-agent, and everything irrelevant to the task goes in with it.";

const AGENT_NAME_DESCRIPTION: &str = "Optional task name, unique in this session; usable instead of the task id in agent_status, agent_send, agent_cancel, and /task. 1 to 64 letters, digits, '_' or '-'.";

const AGENT_FORK_BEFORE_DESCRIPTION: &str = "Text of an earlier user prompt in this conversation that started a turn: its full text or its first line. The sub-agent receives this conversation as it was just before that prompt, including turns that compaction has since summarized, and context becomes inherit. Exactly one prompt must match; otherwise the error lists the prompts that can be used.";
const AGENT_RESUME_DESCRIPTION: &str = "Task id or name of an interrupted sub-agent task in this session. With resume set, this continues that task on its own transcript instead of starting a new one: prompt is the next user message delivered to it, and agent, model, name, description, and fork_before must be absent or empty, and context absent or \"fresh\"; the task keeps its recorded values. For completed tasks, use agent_status and read the saved report instead.";

/// Parent agent tools, including explicit nonblocking `agent_yield`,
/// without a built [`Runner`]. The redaction boundary check needs the tool set
/// a real runner would register, and none of the definitions read runner state.
pub fn tool_definitions() -> Vec<ToolDefinition> {
    vec![
        agent_definition(),
        agent_status_definition(),
        agent_yield_definition(),
        agent_send_definition(),
        agent_cancel_definition(),
    ]
}

/// The parent-side tools in registration order.
pub fn tools(runner: &Arc<Runner>) -> Vec<Box<dyn Tool + Send + Sync>> {
    vec![
        Box::new(AgentTool {
            runner: Arc::clone(runner),
        }),
        Box::new(AgentStatusTool {
            runner: Arc::clone(runner),
        }),
        Box::new(AgentYieldTool {
            runner: Arc::clone(runner),
        }),
        Box::new(AgentSendTool {
            runner: Arc::clone(runner),
        }),
        Box::new(AgentCancelTool {
            runner: Arc::clone(runner),
        }),
    ]
}

fn agent_definition() -> ToolDefinition {
    definition(
        "agent",
        AGENT_DESCRIPTION,
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "The complete task, assigned file ownership, agreed shared interfaces, constraints and requested evidence. Include all call-site adaptations for a coherent change; the sub-agent sees nothing else."
                },
                "description": {
                    "type": "string",
                    "description": "Short label (<= 80 chars) shown in status output."
                },
                "name": {
                    "type": "string",
                    "description": AGENT_NAME_DESCRIPTION
                },
                "model": {
                    "type": "string",
                    "description": "Provider model ID for the sub-agent. Overrides the definition's model. When omitted, uses the definition's model, then the first configured sub-agent model, then this session's model. Otto does not validate availability; an ID the endpoint rejects fails the task with the provider's error."
                },
                "agent": {
                    "type": "string",
                    "description": "Name of a definition from the Agents list in the system prompt: it sets the sub-agent's instructions, tool set, and default model. Omit for the default sub-agent."
                },
                "context": {
                    "type": "string",
                    "enum": ["fresh", "inherit"],
                    "description": AGENT_CONTEXT_DESCRIPTION
                },
                "fork_before": {
                    "type": "string",
                    "description": AGENT_FORK_BEFORE_DESCRIPTION
                },
                "resume": {
                    "type": "string",
                    "description": AGENT_RESUME_DESCRIPTION
                }
            },
            "required": ["prompt"]
        }),
    )
}

fn agent_yield_definition() -> ToolDefinition {
    definition(
        "agent_yield",
        AGENT_YIELD_DESCRIPTION,
        serde_json::json!({
            "type":"object", "additionalProperties":false, "properties":{}
        }),
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct YieldArgs {}

struct AgentYieldTool {
    runner: Arc<Runner>,
}

#[async_trait::async_trait]
impl Tool for AgentYieldTool {
    fn definition(&self) -> ToolDefinition {
        agent_yield_definition()
    }
    fn yields_turn(&self) -> bool {
        true
    }
    async fn execute(&self, arguments: &RawValue, cancel: &CancellationToken) -> ToolResult {
        if let Err(message) = decode_strict_json::<YieldArgs>(arguments.get(), &[]) {
            return error_result(message).not_started();
        }
        if cancel.is_cancelled() {
            return error_result(CONTEXT_CANCELED).cancelled_not_started();
        }
        let count = self
            .runner
            .tasks()
            .list()
            .iter()
            .filter(|task| !task.is_final())
            .count();
        if count == 0 {
            return error_result("No queued or running child task; continue with available results instead of yielding.").not_started();
        }
        text_result(format!(
            "Waiting for {count} child task(s). This turn can yield after the tool batch settles; the runtime resumes on child notifications or user input."
        ))
    }
}

fn agent_status_definition() -> ToolDefinition {
    definition(
        "agent_status",
        AGENT_STATUS_DESCRIPTION,
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "include_history": {
                    "type": "boolean",
                    "description": "Include recent child steps only when explicitly needed; default false. The response still has a 4 KiB cap"
                },
                "task_id": {
                    "type": "string",
                    "description": "Task id or name; show status, final conclusion preview and any saved overflow report path without research history by default"
                }
            }
        }),
    )
}

fn agent_send_definition() -> ToolDefinition {
    definition(
        "agent_send",
        AGENT_SEND_DESCRIPTION,
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "task_id": {
                    "type": "string",
                    "description": "Task id or name of the queued or running sub-agent."
                },
                "message": {
                    "type": "string",
                    "description": "The task update, clarification, new fact, or direction change to deliver to the sub-agent."
                }
            },
            "required": ["task_id", "message"]
        }),
    )
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentCancelArgs {
    #[serde(default)]
    task_id: String,
}

fn agent_cancel_definition() -> ToolDefinition {
    definition(
        "agent_cancel",
        AGENT_CANCEL_DESCRIPTION,
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "task_id": {
                    "type": "string",
                    "description": "Task id or name of the queued or running sub-agent to cancel."
                }
            },
            "required": ["task_id"]
        }),
    )
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentArgs {
    #[serde(default)]
    prompt: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    agent: String,
    #[serde(default)]
    context: String,
    #[serde(default)]
    fork_before: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    resume: String,
}

struct AgentTool {
    runner: Arc<Runner>,
}

#[async_trait::async_trait]
impl Tool for AgentTool {
    fn definition(&self) -> ToolDefinition {
        agent_definition()
    }

    async fn execute(&self, arguments: &RawValue, cancel: &CancellationToken) -> ToolResult {
        self.run(arguments, "", cancel).await
    }

    async fn execute_call(
        &self,
        call: otto_core::tool::ToolCall<'_>,
        cancel: &CancellationToken,
    ) -> ToolResult {
        self.run(call.arguments, call.tool_call_id, cancel).await
    }
}

impl AgentTool {
    async fn run(
        &self,
        arguments: &RawValue,
        tool_call_id: &str,
        cancel: &CancellationToken,
    ) -> ToolResult {
        let args: AgentArgs = match decode_strict_json(arguments.get(), &["prompt"]) {
            Ok(args) => args,
            Err(message) => return error_result(message),
        };
        let resume_target = args.resume.trim().to_string();
        if !resume_target.is_empty() {
            return self.execute_resume(&resume_target, args, cancel).await;
        }
        if args.prompt.trim().is_empty() {
            return error_result("prompt is required");
        }
        let requested_agent = args.agent.trim();
        if !requested_agent.is_empty() && self.runner.catalog().lookup(requested_agent).is_none() {
            return error_result(unknown_agent_message(
                self.runner.catalog(),
                requested_agent,
            ));
        }

        let running_before = count_running(self.runner.tasks());
        let task = match self
            .runner
            .start(StartRequest {
                prompt: args.prompt,
                description: args.description,
                model: args.model,
                agent: args.agent,
                context: args.context,
                fork_before: args.fork_before,
                name: args.name,
                parent_tool_call_id: tool_call_id.to_string(),
            })
            .await
        {
            Ok(task) => task,
            Err(error) => return error_result(error),
        };

        if !task.agent.is_empty()
            && let Some(checker) = self.runner.checker()
            && let Some(definition) = self.runner.catalog().lookup(&task.agent)
            && definition.is_skill_derived
        {
            checker.trigger(
                definition.name.clone(),
                definition.directory.clone(),
                definition.path.clone(),
            );
        }

        let name = agent_label(&task);
        let started = if running_before >= self.runner.max_parallel() {
            format!(
                "task {} {name} queued ({running_before} running, limit {})",
                task.id,
                self.runner.max_parallel()
            )
        } else {
            format!("task {} {name} started", task.id)
        };
        text_result(started)
    }

    /// The `resume` branch of `agent`: continues an interrupted sub-agent
    /// task on its own transcript instead of starting a new one.
    async fn execute_resume(
        &self,
        target: &str,
        args: AgentArgs,
        _cancel: &CancellationToken,
    ) -> ToolResult {
        if args.prompt.trim().is_empty() {
            return error_result("prompt is required");
        }
        for (field, value) in [
            ("agent", &args.agent),
            ("model", &args.model),
            ("context", &args.context),
            ("name", &args.name),
            ("description", &args.description),
            ("fork_before", &args.fork_before),
        ] {
            let value = value.trim();
            if field == "context" && value == "fresh" {
                continue;
            }
            if !value.is_empty() {
                return error_result(format!("resume does not accept {field}"));
            }
        }

        let task = match self.runner.resume(target, args.prompt).await {
            Ok(task) => task,
            Err(error) => return error_result(error),
        };

        let name = agent_label(&task);
        text_result(format!("task {} {name} resumed", task.id))
    }
}

/// The `agent` tool's unknown-agent error: the bare `unknown agent: <name>`
/// when the catalog is empty, else with an `; available: a, b` suffix listing
/// every definition name.
fn unknown_agent_message(catalog: &Catalog, name: &str) -> String {
    let message = format!("unknown agent: {name}");
    if catalog.is_empty() {
        return message;
    }
    let names: Vec<&str> = catalog
        .definitions()
        .iter()
        .map(|definition| definition.name.as_str())
        .collect();
    format!("{message}; available: {}", names.join(", "))
}

fn count_running(tasks: &Tasks) -> usize {
    tasks
        .list()
        .iter()
        .filter(|task| task.status == TaskStatus::Running)
        .count()
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentStatusArgs {
    #[serde(default)]
    include_history: bool,
    #[serde(default)]
    task_id: String,
}

struct AgentStatusTool {
    runner: Arc<Runner>,
}

#[async_trait::async_trait]
impl Tool for AgentStatusTool {
    fn definition(&self) -> ToolDefinition {
        agent_status_definition()
    }

    async fn execute(&self, arguments: &RawValue, cancel: &CancellationToken) -> ToolResult {
        let args: AgentStatusArgs = match decode_strict_json(arguments.get(), &[]) {
            Ok(args) => args,
            Err(message) => return error_result(message),
        };
        if cancel.is_cancelled() {
            return error_result(CONTEXT_CANCELED);
        }

        let tasks = self.runner.tasks();
        let all = tasks.list();
        if all.is_empty() {
            return text_result("no tasks in this session");
        }

        let now = self.runner.now();
        if args.task_id.is_empty() {
            let lines: Vec<String> = all.iter().map(|task| status_line(task, now)).collect();
            return crate::tool::result::capped_text_result(
                &lines.join("\n"),
                self.runner
                    .max_output_bytes()
                    .min(runner::PARENT_REPORT_BYTES),
            );
        }

        let Some(task) = tasks.get(&args.task_id) else {
            return error_result(format!("unknown task: {}", args.task_id));
        };
        let mut lines = vec![status_line(&task, now)];
        if !task.model.is_empty() {
            lines.push(format!("model: {}", task.model));
        }
        if !task.is_final() {
            lines.push(format!(
                "activity: {} (last tool is historical; unchanged counters do not prove a stall)",
                self.runner.activity(&task)
            ));
        }
        if args.include_history {
            lines.extend(history_lines(
                &tasks.history(&args.task_id).unwrap_or_default(),
            ));
        }
        if task.is_final() {
            if task.error.is_empty() {
                lines.push("result:".to_string());
                lines.push(task.result.clone());
            } else {
                lines.push(format!("error: {}", task.error));
            }
        }
        text_result(self.runner.parent_text(&task, &lines.join("\n")))
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentSendArgs {
    #[serde(default)]
    task_id: String,
    #[serde(default)]
    message: String,
}

struct AgentSendTool {
    runner: Arc<Runner>,
}

#[async_trait::async_trait]
impl Tool for AgentSendTool {
    fn definition(&self) -> ToolDefinition {
        agent_send_definition()
    }

    async fn execute(&self, arguments: &RawValue, cancel: &CancellationToken) -> ToolResult {
        let args: AgentSendArgs = match decode_strict_json(arguments.get(), &["task_id", "message"])
        {
            Ok(args) => args,
            Err(message) => return error_result(message),
        };
        if cancel.is_cancelled() {
            return error_result(CONTEXT_CANCELED);
        }
        let task_id = args.task_id.trim();
        if task_id.is_empty() {
            return error_result("task_id is required");
        }
        let message = args.message.trim();
        if message.is_empty() {
            return error_result("message is required");
        }
        match self.runner.tasks().send_message(task_id, message) {
            Ok(id) => text_result(format!(
                "message queued for task {id}; it will be read at the next checkpoint"
            )),
            Err(super::tasks::TaskError::NotFound(_)) => {
                error_result(format!("unknown task: {task_id}"))
            }
            Err(super::tasks::TaskError::Finished(_)) => error_result(format!(
                "task {task_id} is already completed; cannot send message"
            )),
            Err(error) => error_result(error.to_string()),
        }
    }
}

struct AgentCancelTool {
    runner: Arc<Runner>,
}

#[async_trait::async_trait]
impl Tool for AgentCancelTool {
    fn definition(&self) -> ToolDefinition {
        agent_cancel_definition()
    }

    async fn execute(&self, arguments: &RawValue, cancel: &CancellationToken) -> ToolResult {
        let args: AgentCancelArgs = match decode_strict_json(arguments.get(), &["task_id"]) {
            Ok(args) => args,
            Err(message) => return error_result(message),
        };
        if cancel.is_cancelled() {
            return error_result(CONTEXT_CANCELED);
        }
        let task_id = args.task_id.trim();
        if task_id.is_empty() {
            return error_result("task_id is required");
        }
        match self.runner.tasks().cancel(task_id) {
            Ok(()) => text_result(format!(
                "cancel requested for task {task_id}; it stops at its next safe checkpoint and its final status arrives as a [task-notification]"
            )),
            Err(super::tasks::TaskError::NotFound(_)) => {
                error_result(format!("unknown task: {task_id}"))
            }
            Err(super::tasks::TaskError::Finished(_)) => error_result(format!(
                "task {task_id} is already completed; nothing to cancel"
            )),
            Err(error) => error_result(error.to_string()),
        }
    }
}

/// One `agent_status` table row: fixed-width id, definition, status, elapsed,
/// tool count, detail and label columns, with trailing spaces trimmed.
fn status_line(task: &Task, now: chrono::DateTime<chrono::Utc>) -> String {
    let elapsed = super::format::task_elapsed(task, now);
    // Unlike the REPL's column, this one is blank for a task that has made no
    // tool call, including a running one.
    let tools_column = if task.tool_calls > 0 {
        pluralize_tools(task.tool_calls)
    } else {
        String::new()
    };
    let detail = if task.is_final() {
        format!(
            "{} tokens",
            super::format::comma_int(task.usage.input_tokens + task.usage.output_tokens)
        )
    } else {
        task.last_tool.clone()
    };

    let line = format!(
        "{:<4} {:<10} {:<9} {:>6} {:>9}  {:<24} {}",
        task.id,
        definition_label(task),
        task.status.as_str(),
        elapsed,
        tools_column,
        detail,
        task_label(task)
    );
    line.trim_end_matches(' ').to_string()
}

/// Each assistant step's tool calls and text, keeping only the last ten lines
/// across the whole history.
fn history_lines(history: &[otto_core::model::Message]) -> Vec<String> {
    let mut lines = Vec::new();
    for message in history {
        if message.role != otto_core::model::Role::Assistant {
            continue;
        }
        for block in &message.blocks {
            if block.block_type != otto_core::model::BlockType::ToolCall {
                continue;
            }
            let mut line = format!("  → {}", block.tool_name);
            let arguments = block.arguments.as_ref().map_or("", |raw| raw.get());
            let preview = first_runes(&runner::compact_json(arguments), 80);
            if !preview.is_empty() {
                line.push(' ');
                line.push_str(&preview);
            }
            lines.push(line);
        }
        let text = message.text();
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            lines.push(format!(
                "  assistant: {}",
                runner::cap_last_bytes(trimmed, 500)
            ));
        }
    }
    if lines.len() > 10 {
        lines.drain(..lines.len() - 10);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subagent::Definition;
    use crate::subagent::runner::{Config, StartRequest};
    use crate::subagent::testsupport::{
        FakeProvider, RouteStep, StubTool, assistant_text, assistant_tool_call, match_any, raw,
        stub, test_config, wait_status,
    };
    use otto_core::agent::inbox::NotificationKind;
    use otto_core::model::Usage;
    use otto_core::tool::ToolResult;
    use std::sync::Arc;

    fn runner(config: Config) -> Arc<Runner> {
        let (runner, _) = Runner::new(config).expect("the test config is valid");
        Arc::new(runner)
    }

    fn tool_named(runner: &Arc<Runner>, name: &str) -> Box<dyn Tool + Send + Sync> {
        tools(runner)
            .into_iter()
            .find(|tool| tool.definition().name == name)
            .unwrap_or_else(|| panic!("tool {name:?} is registered"))
    }

    async fn run(tool: &dyn Tool, arguments: &str) -> ToolResult {
        tool.execute(&raw(arguments), &CancellationToken::new())
            .await
    }

    async fn wait_final(tasks: &Tasks, id: &str) -> Task {
        let cancel = CancellationToken::new();
        tokio::select! {
            result = tasks.wait(id, &cancel) => result.expect("the task exists"),
            () = tokio::time::sleep(std::time::Duration::from_secs(5)) => {
                panic!("task {id} did not finish in time")
            }
        }
    }

    /// A hook that blocks every provider call until `release` fires, or the
    /// child is cancelled.
    fn block_until(release: CancellationToken) -> crate::subagent::testsupport::Hook {
        Arc::new(move |cancel, _request| {
            let release = release.clone();
            Box::pin(async move {
                tokio::select! {
                    () = release.cancelled() => {}
                    () = cancel.cancelled() => {}
                }
            })
        })
    }

    #[tokio::test]
    async fn agent_records_the_spawning_tool_call_in_the_child_task_spec() {
        let dir = tempfile::tempdir().expect("dir");
        let parent = dir.path().join("parent.jsonl");
        let provider = FakeProvider::new();
        provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
        let tasks = Arc::new(Tasks::new());
        let mut config = test_config(&provider, &tasks, Vec::new());
        config.child_session = Some(Arc::new(move |task_id: &str| {
            let name = format!("{task_id}-child");
            let store = crate::session::Store::create_child_lazy(
                &parent,
                &name,
                otto_core::session::Header {
                    id: "child".into(),
                    workspace: parent.parent().expect("dir").to_string_lossy().into_owned(),
                    provider: "openai-compatible".into(),
                    model: "test-model".into(),
                    created_at: chrono::Utc::now(),
                    ..otto_core::session::Header::default()
                },
            )
            .expect("child store");
            let path = crate::session::Store::child_path(&parent, &name);
            Ok(Some((Arc::new(store), path.to_string_lossy().into_owned())))
        }));
        let runner = runner(config);
        let tool = tool_named(&runner, "agent");
        let operation_id = otto_core::model::OperationId::new("op_test").expect("operation id");
        let arguments = raw(r#"{"prompt":"look"}"#);
        tool.execute_call(
            otto_core::tool::ToolCall {
                tool_call_id: "call_7",
                operation_id: &operation_id,
                name: "agent",
                arguments: &arguments,
                attempt: 1,
            },
            &CancellationToken::new(),
        )
        .await;
        let task = wait_final(&tasks, "t1").await;

        let text = std::fs::read_to_string(&task.session_path).expect("child transcript");
        let spec: serde_json::Value = text
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .find(|entry| entry["customType"] == "otto.task_spec")
            .expect("task spec entry");
        assert_eq!(spec["data"]["parentToolCallId"], "call_7");
    }

    #[tokio::test(start_paused = true)]
    async fn agent_yield_is_nonblocking_and_requires_an_outstanding_child() {
        let provider = FakeProvider::new();
        let tasks = Arc::new(Tasks::new());
        let runner = runner(test_config(&provider, &tasks, Vec::new()));
        let tool = tool_named(&runner, "agent_yield");
        assert!(run(tool.as_ref(), "{}").await.is_error);
        let task = tasks.add(Task::default(), None, None).unwrap();
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(1), run(tool.as_ref(), "{}"))
                .await
                .expect("yield must not wait inside the tool");
        assert!(!result.is_error);
        assert!(tool.yields_turn());
        assert!(provider.requests().is_empty());
        assert!(!tasks.get(&task.id).unwrap().is_final());
        assert!(run(tool.as_ref(), r#"{"unexpected":true}"#).await.is_error);
    }

    #[tokio::test]
    async fn live_activity_clears_the_tool_phase_without_losing_last_tool() {
        let tasks = Tasks::new();
        let task = tasks.add(Task::default(), None, None).unwrap();
        assert_eq!(tasks.activity(&task.id), Some("queued"));
        tasks.mark_running(&task.id, chrono::Utc::now());
        assert_eq!(tasks.activity(&task.id), Some("provider_or_runtime"));
        tasks.record_tool_call(&task.id, "read file.rs");
        assert_eq!(tasks.activity(&task.id), Some("tool"));
        tasks.record_tool_finished(&task.id);
        assert_eq!(tasks.activity(&task.id), Some("provider_or_runtime"));
        assert_eq!(tasks.get(&task.id).unwrap().last_tool, "read file.rs");
    }

    #[tokio::test]
    async fn agent_returns_while_a_child_is_still_running() {
        let provider = FakeProvider::new();
        provider.set_hook(block_until(CancellationToken::new()));
        let tasks = Arc::new(Tasks::new());
        let runner = runner(test_config(&provider, &tasks, Vec::new()));

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            run(
                tool_named(&runner, "agent").as_ref(),
                r#"{"prompt":"do it"}"#,
            ),
        )
        .await
        .expect("agent blocked on its child");
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("task t1"), "{}", result.content);
        wait_status(&tasks, "t1", TaskStatus::Running).await;

        tasks.cancel("t1").expect("cancel succeeds");
        wait_final(&tasks, "t1").await;
        assert_eq!(
            tasks.notifications().snapshot()[0].kind,
            Some(NotificationKind::TaskFinished),
            "completion stays queued for the runtime wake turn"
        );
    }

    #[tokio::test]
    async fn agent_rejects_the_removed_wait_argument() {
        let provider = FakeProvider::new();
        let tasks = Arc::new(Tasks::new());
        let runner = runner(test_config(&provider, &tasks, Vec::new()));

        let result = run(
            tool_named(&runner, "agent").as_ref(),
            r#"{"prompt":"do it","wait":true}"#,
        )
        .await;
        assert!(result.is_error, "{}", result.content);
        assert!(tasks.list().is_empty());
    }

    #[tokio::test]
    async fn agent_tool_rejects_blank_prompt() {
        let provider = FakeProvider::new();
        let tasks = Arc::new(Tasks::new());
        let runner = runner(test_config(&provider, &tasks, Vec::new()));
        let agent_tool = tool_named(&runner, "agent");

        for body in [r#"{"prompt":"   "}"#, "{}"] {
            assert!(
                run(agent_tool.as_ref(), body).await.is_error,
                "body {body} must be rejected"
            );
        }
        assert!(tasks.list().is_empty());
    }

    #[tokio::test]
    async fn agent_tool_model_selection() {
        for (body, want) in [
            (r#"{"prompt":"go","model":"cheap-model"}"#, "cheap-model"),
            (r#"{"prompt":"go"}"#, "gpt-parent"),
            (r#"{"prompt":"go","model":"   "}"#, "gpt-parent"),
        ] {
            let provider = FakeProvider::new();
            provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
            let tasks = Arc::new(Tasks::new());
            let mut config = test_config(&provider, &tasks, Vec::new());
            config.template.model = "gpt-parent".into();
            let runner = runner(config);

            let result = run(tool_named(&runner, "agent").as_ref(), body).await;
            assert!(!result.is_error, "{}", result.content);

            assert_eq!(wait_final(&tasks, "t1").await.model, want, "body {body}");
            assert_eq!(
                provider.requests().first().expect("one request").model,
                want
            );
        }
    }

    #[tokio::test]
    async fn agent_tool_uses_configured_model_when_omitted() {
        let provider = FakeProvider::new();
        provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
        let tasks = Arc::new(Tasks::new());
        let mut config = test_config(&provider, &tasks, Vec::new());
        config.template.model = "parent".into();
        config.template.subagent_models = vec!["cheap".into(), "strong".into()];
        let runner = runner(config);
        let result = run(tool_named(&runner, "agent").as_ref(), r#"{"prompt":"go"}"#).await;
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(wait_final(&tasks, "t1").await.model, "cheap");
        assert_eq!(provider.requests()[0].model, "cheap");
    }

    #[tokio::test]
    async fn agent_status_empty_registry() {
        let provider = FakeProvider::new();
        let tasks = Arc::new(Tasks::new());
        let runner = runner(test_config(&provider, &tasks, Vec::new()));
        let result = run(tool_named(&runner, "agent_status").as_ref(), "{}").await;
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(result.content, "no tasks in this session");
    }

    #[tokio::test]
    async fn agent_status_unknown_task_id() {
        let provider = FakeProvider::new();
        provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
        let tasks = Arc::new(Tasks::new());
        let runner = runner(test_config(&provider, &tasks, Vec::new()));
        runner
            .start(StartRequest {
                prompt: "go".into(),
                ..StartRequest::default()
            })
            .await
            .expect("start succeeds");

        let result = run(
            tool_named(&runner, "agent_status").as_ref(),
            r#"{"task_id":"t9"}"#,
        )
        .await;
        assert!(result.is_error);
        assert_eq!(result.content, "unknown task: t9");
    }

    #[tokio::test]
    async fn agent_status_listing_shows_running_and_queued_rows() {
        let provider = FakeProvider::new();
        provider.set_hook(block_until(CancellationToken::new()));
        let tasks = Arc::new(Tasks::new());
        let mut config = test_config(&provider, &tasks, Vec::new());
        config.max_parallel = 1;
        let runner = runner(config);

        runner
            .start(StartRequest {
                prompt: "explore the repo".into(),
                description: "explore".into(),
                ..StartRequest::default()
            })
            .await
            .expect("start succeeds");
        wait_status(&tasks, "t1", TaskStatus::Running).await;
        runner
            .start(StartRequest {
                prompt: "review the diff".into(),
                ..StartRequest::default()
            })
            .await
            .expect("start succeeds");

        let result = run(tool_named(&runner, "agent_status").as_ref(), "{}").await;
        assert!(!result.is_error, "{}", result.content);
        let lines: Vec<&str> = result.content.split('\n').collect();
        assert_eq!(lines.len(), 2, "{}", result.content);
        for needle in ["t1", "(default)", "running", "explore"] {
            assert!(lines[0].contains(needle), "t1 line = {:?}", lines[0]);
        }
        for needle in ["t2", "(default)", "queued", "review the diff"] {
            assert!(lines[1].contains(needle), "t2 line = {:?}", lines[1]);
        }

        for id in ["t1", "t2"] {
            tasks.cancel(id).expect("cancel succeeds");
            wait_final(&tasks, id).await;
        }
    }

    #[tokio::test]
    async fn agent_status_detail_shows_history_and_result() {
        let provider = FakeProvider::new();
        let usage = Usage {
            input_tokens: 1,
            output_tokens: 1,
            ..Usage::default()
        };
        provider.add_route(
            match_any,
            vec![
                assistant_tool_call("call-1", "echo", r#"{"msg":"hi"}"#, usage),
                assistant_text("wrap-up", usage),
            ],
        );
        let echo = StubTool::new(
            "echo",
            ToolResult {
                content: "echoed".into(),
                ..ToolResult::default()
            },
        );
        let tasks = Arc::new(Tasks::new());
        let runner = runner(test_config(&provider, &tasks, vec![echo.boxed()]));
        runner
            .start(StartRequest {
                prompt: "go".into(),
                ..StartRequest::default()
            })
            .await
            .expect("start succeeds");
        wait_final(&tasks, "t1").await;

        let summary = run(
            tool_named(&runner, "agent_status").as_ref(),
            r#"{"task_id":"t1"}"#,
        )
        .await;
        assert!(summary.content.contains("result:\nwrap-up"));
        assert!(!summary.content.contains("assistant:"));
        assert!(!summary.content.contains("→ echo"));

        let result = run(
            tool_named(&runner, "agent_status").as_ref(),
            r#"{"task_id":"t1","include_history":true}"#,
        )
        .await;
        assert!(!result.is_error, "{}", result.content);
        for needle in ["succeeded", "echo", "assistant: wrap-up"] {
            assert!(result.content.contains(needle), "{}", result.content);
        }
        assert!(
            result.content.ends_with("result:\nwrap-up"),
            "{}",
            result.content
        );
    }

    #[tokio::test]
    async fn parent_handoffs_are_bounded_but_the_full_child_result_is_retained() {
        let provider = FakeProvider::new();
        // The runner trims the final assistant text; end the fixture without
        // whitespace so the retained result equals it byte for byte.
        let report = "conclusion and evidence "
            .repeat(1000)
            .trim_end()
            .to_owned();
        provider.add_route(match_any, vec![assistant_text(&report, Usage::default())]);
        let tasks = Arc::new(Tasks::new());
        let runner = runner(test_config(&provider, &tasks, Vec::new()));
        runner
            .start(StartRequest {
                prompt: "research".into(),
                ..StartRequest::default()
            })
            .await
            .unwrap();
        wait_final(&tasks, "t1").await;
        assert_eq!(tasks.get("t1").unwrap().result, report);
        let notices = tasks.notifications().snapshot();
        assert_eq!(notices.len(), 1);
        assert!(notices[0].text.len() < 4300);
        assert!(notices[0].text.contains("truncated"));
        let result = run(
            tool_named(&runner, "agent_status").as_ref(),
            r#"{"task_id":"t1"}"#,
        )
        .await;
        assert!(result.content.len() < 4300);
        assert!(result.content.contains("truncated"));
    }

    #[tokio::test]
    async fn long_final_report_is_readable_after_child_exit_without_another_model_call() {
        use crate::tool::{read::ReadTool, root::Root, workspace::Workspace};
        let dir = tempfile::tempdir().unwrap();
        let provider = FakeProvider::new();
        let report = format!("# 摘要\n{}\n## 证据\n末尾证据", "中文研究内容".repeat(5000));
        provider.add_route(match_any, vec![assistant_text(&report, Usage::default())]);
        let tasks = Arc::new(Tasks::new());
        let runner = runner(test_config(&provider, &tasks, Vec::new()));
        runner.bind_scratch_root(Root::open(dir.path()).unwrap());
        runner
            .start(StartRequest {
                prompt: "research".into(),
                name: "research".into(),
                ..StartRequest::default()
            })
            .await
            .unwrap();
        let task = wait_final(&tasks, "t1").await;
        assert_eq!(task.status, TaskStatus::Succeeded);
        assert_eq!(task.result, report);
        let notices = tasks.notifications().snapshot();
        let notice = &notices[0].text;
        let report_path = notice
            .split("[Full report: ")
            .nth(1)
            .expect("read pointer survives truncation")
            .split(" (")
            .next()
            .unwrap()
            .to_owned();
        assert!(notice.len() < 4700);
        assert!(notice.contains("truncated"));
        assert!(notice.contains("UTF-8 bytes"));
        assert!(notice.contains("/index.md"));
        let status = run(
            tool_named(&runner, "agent_status").as_ref(),
            r#"{"task_id":"research","include_history":true}"#,
        )
        .await;
        assert!(status.content.contains(&report_path));
        assert!(status.content.len() < 4700);
        let prompt = &provider.requests()[0].system_prompt;
        assert!(prompt.contains("<research_report>"));
        assert!(prompt.contains("/report.md"));
        assert!(prompt.contains("Existing write permissions"));
        drop(runner);

        let workspace = Workspace::new(dir.path()).unwrap();
        let read = ReadTool::new(&workspace, 16384);
        let index_path = report_path.replace("/final.md", "/index.md");
        let index = run(&read, &serde_json::json!({"path": index_path}).to_string()).await;
        assert!(!index.is_error, "{}", index.content);
        assert!(index.content.contains("line 3: 证据"));
        let tail = run(
            &read,
            &serde_json::json!({"path": report_path, "offset": 3, "limit": 2}).to_string(),
        )
        .await;
        assert!(!tail.is_error, "{}", tail.content);
        assert!(tail.content.contains("末尾证据"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join(&report_path)).unwrap(),
            report
        );
        assert_eq!(
            provider.requests().len(),
            1,
            "reading must not restart the child"
        );
    }

    #[tokio::test]
    async fn tiny_preview_keeps_the_read_pointer_and_persisted_report_is_redacted() {
        use crate::tool::root::Root;
        let dir = tempfile::tempdir().unwrap();
        let provider = FakeProvider::new();
        let private = "private-report-marker";
        provider.add_route(
            match_any,
            vec![assistant_text(
                &format!("{private}\nfindings"),
                Usage::default(),
            )],
        );
        let tasks = Arc::new(Tasks::new());
        let mut config = test_config(&provider, &tasks, Vec::new());
        config.max_output_bytes = 1;
        config.redaction_values = vec![private.to_owned()];
        let runner = runner(config);
        runner.bind_scratch_root(Root::open(dir.path()).unwrap());
        runner
            .start(StartRequest {
                prompt: "research".into(),
                ..StartRequest::default()
            })
            .await
            .unwrap();
        let task = wait_final(&tasks, "t1").await;
        let notices = tasks.notifications().snapshot();
        let path = notices[0]
            .text
            .split("[Full report: ")
            .nth(1)
            .unwrap()
            .split(" (")
            .next()
            .unwrap();
        let saved = std::fs::read_to_string(dir.path().join(path)).unwrap();
        assert!(!saved.contains(private));
        assert!(saved.contains("findings"));
        assert_eq!(saved, task.result);
        let status = run(
            tool_named(&runner, "agent_status").as_ref(),
            r#"{"task_id":"t1"}"#,
        )
        .await;
        assert!(status.content.contains(path));
    }

    #[tokio::test]
    async fn report_storage_failure_preserves_success_and_reports_the_missing_artifact() {
        use crate::tool::root::Root;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".otto"), "blocks directory creation").unwrap();
        let provider = FakeProvider::new();
        let report = "findings ".repeat(1000).trim_end().to_owned();
        provider.add_route(match_any, vec![assistant_text(&report, Usage::default())]);
        let tasks = Arc::new(Tasks::new());
        let runner = runner(test_config(&provider, &tasks, Vec::new()));
        runner.bind_scratch_root(Root::open(dir.path()).unwrap());
        runner
            .start(StartRequest {
                prompt: "research".into(),
                ..StartRequest::default()
            })
            .await
            .unwrap();
        let task = wait_final(&tasks, "t1").await;
        assert_eq!(task.status, TaskStatus::Succeeded);
        assert_eq!(task.result, report);
        let notices = tasks.notifications().snapshot();
        assert!(notices[0].text.contains("report file unavailable"));
        assert!(!notices[0].text.contains("[Full report:"));
    }

    #[tokio::test]
    async fn agent_status_detail_shows_model_when_set() {
        let provider = FakeProvider::new();
        provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
        let tasks = Arc::new(Tasks::new());
        let runner = runner(test_config(&provider, &tasks, Vec::new()));
        runner
            .start(StartRequest {
                prompt: "go".into(),
                model: "gpt-test-model".into(),
                ..StartRequest::default()
            })
            .await
            .expect("start succeeds");
        wait_final(&tasks, "t1").await;

        let result = run(
            tool_named(&runner, "agent_status").as_ref(),
            r#"{"task_id":"t1"}"#,
        )
        .await;
        let lines: Vec<&str> = result.content.split('\n').collect();
        assert!(lines.len() >= 2, "{}", result.content);
        assert_eq!(lines[1], "model: gpt-test-model", "{}", result.content);
    }

    #[tokio::test]
    async fn agent_status_detail_shows_error_on_failure() {
        let provider = FakeProvider::new();
        provider.add_route(match_any, vec![RouteStep::Fail("boom".into())]);
        let tasks = Arc::new(Tasks::new());
        let runner = runner(test_config(&provider, &tasks, Vec::new()));
        runner
            .start(StartRequest {
                prompt: "go".into(),
                ..StartRequest::default()
            })
            .await
            .expect("start succeeds");
        wait_final(&tasks, "t1").await;

        let result = run(
            tool_named(&runner, "agent_status").as_ref(),
            r#"{"task_id":"t1"}"#,
        )
        .await;
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("error: "), "{}", result.content);
        assert!(result.content.contains("boom"), "{}", result.content);
    }

    #[tokio::test]
    async fn agent_tool_unknown_agent_error() {
        let provider = FakeProvider::new();
        let tasks = Arc::new(Tasks::new());
        let mut config = test_config(&provider, &tasks, Vec::new());
        config.catalog = Catalog::from_definitions(vec![Definition {
            name: "reviewer".into(),
            ..Definition::default()
        }]);
        let runner = runner(config);

        let result = run(
            tool_named(&runner, "agent").as_ref(),
            r#"{"prompt":"go","agent":"nope"}"#,
        )
        .await;
        assert!(result.is_error);
        assert!(
            result.content.contains("unknown agent: nope")
                && result.content.contains("available: reviewer"),
            "{}",
            result.content
        );
        assert!(tasks.list().is_empty());
    }

    #[tokio::test]
    async fn agent_tool_named_agent_started_message() {
        let provider = FakeProvider::new();
        provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
        let tasks = Arc::new(Tasks::new());
        let mut config = test_config(&provider, &tasks, Vec::new());
        config.catalog = Catalog::from_definitions(vec![Definition {
            name: "reviewer".into(),
            ..Definition::default()
        }]);
        let runner = runner(config);

        let result = run(
            tool_named(&runner, "agent").as_ref(),
            r#"{"prompt":"go","agent":"reviewer"}"#,
        )
        .await;
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(result.content, "task t1 (reviewer) started");
    }

    fn write_skill_with_vague_output(directory: &std::path::Path) {
        std::fs::create_dir_all(directory).expect("mkdir");
        std::fs::write(
            directory.join("SKILL.md"),
            "---\nname: reviewer\ndescription: reviews things\ninput: a path\noutput: the result\n---\nBody.\n",
        )
        .expect("write SKILL.md");
    }

    fn checked_skill(directory: &std::path::Path) -> crate::skill::Skill {
        crate::skill::Skill {
            name: "reviewer".to_string(),
            description: String::new(),
            contract: None,
            directory: directory.to_path_buf(),
            path: directory.join("SKILL.md"),
        }
    }

    #[tokio::test]
    async fn agent_tool_triggers_one_check_for_a_skill_derived_definition() {
        let provider = FakeProvider::new();
        provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
        let tasks = Arc::new(Tasks::new());
        let dir = tempfile::tempdir().expect("tempdir");
        write_skill_with_vague_output(dir.path());
        // The base URL is never contacted: the vague `output` field fires a
        // rule, so this checks the trigger wiring without a network stub.
        let checker = Arc::new(crate::skill::check::Checker::open_in_memory(
            "http://127.0.0.1:1",
            "key",
        ));
        let mut config = test_config(&provider, &tasks, Vec::new());
        config.catalog = Catalog::from_definitions(vec![Definition {
            name: "reviewer".into(),
            directory: dir.path().to_path_buf(),
            path: dir.path().join("SKILL.md"),
            is_skill_derived: true,
            ..Definition::default()
        }]);
        config.checker = Some(Arc::clone(&checker));
        let runner = runner(config);

        let result = run(
            tool_named(&runner, "agent").as_ref(),
            r#"{"prompt":"go","agent":"reviewer"}"#,
        )
        .await;
        assert!(!result.is_error, "{}", result.content);

        let skill = checked_skill(dir.path());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        loop {
            if checker.display(&skill) != "not checked yet" {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "background check did not complete in time"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let text = checker.display(&skill);
        assert!(text.starts_with("rules"), "{text}");
    }

    #[tokio::test]
    async fn agent_tool_triggers_no_check_for_a_real_agent_definition() {
        let provider = FakeProvider::new();
        provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
        let tasks = Arc::new(Tasks::new());
        let dir = tempfile::tempdir().expect("tempdir");
        write_skill_with_vague_output(dir.path());
        let checker = Arc::new(crate::skill::check::Checker::open_in_memory(
            "http://127.0.0.1:1",
            "key",
        ));
        let mut config = test_config(&provider, &tasks, Vec::new());
        config.catalog = Catalog::from_definitions(vec![Definition {
            name: "reviewer".into(),
            directory: dir.path().to_path_buf(),
            path: dir.path().join("SKILL.md"),
            is_skill_derived: false,
            ..Definition::default()
        }]);
        config.checker = Some(Arc::clone(&checker));
        let runner = runner(config);

        let result = run(
            tool_named(&runner, "agent").as_ref(),
            r#"{"prompt":"go","agent":"reviewer"}"#,
        )
        .await;
        assert!(!result.is_error, "{}", result.content);

        let skill = checked_skill(dir.path());
        assert_eq!(checker.display(&skill), "not checked yet");
    }

    #[tokio::test]
    async fn agent_tool_name_in_started_message() {
        let provider = FakeProvider::new();
        provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
        let tasks = Arc::new(Tasks::new());
        let runner = runner(test_config(&provider, &tasks, Vec::new()));

        let result = run(
            tool_named(&runner, "agent").as_ref(),
            r#"{"prompt":"go","name":"lint-check"}"#,
        )
        .await;
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(result.content, "task t1 lint-check (default) started");
    }

    #[tokio::test]
    async fn agent_tool_duplicate_name_is_error_result() {
        let provider = FakeProvider::new();
        provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
        let tasks = Arc::new(Tasks::new());
        let runner = runner(test_config(&provider, &tasks, Vec::new()));
        let agent_tool = tool_named(&runner, "agent");

        let first = run(
            agent_tool.as_ref(),
            r#"{"prompt":"go","name":"lint-check"}"#,
        )
        .await;
        assert!(!first.is_error, "{}", first.content);
        let second = run(
            agent_tool.as_ref(),
            r#"{"prompt":"go again","name":"lint-check"}"#,
        )
        .await;
        assert!(second.is_error);
        assert!(
            second.content.contains("already used by t1"),
            "{}",
            second.content
        );
        assert_eq!(tasks.list().len(), 1);
    }

    #[tokio::test]
    async fn agent_send_queues_message_for_running_task() {
        let provider = FakeProvider::new();
        let release = CancellationToken::new();
        provider.set_hook(block_until(release.clone()));
        provider.add_route(
            match_any,
            vec![
                assistant_tool_call("call-1", "noop", "{}", Usage::default()),
                assistant_text("saw update", Usage::default()),
            ],
        );
        let tasks = Arc::new(Tasks::new());
        let runner = runner(test_config(&provider, &tasks, vec![stub("noop")]));
        runner
            .start(StartRequest {
                prompt: "start".into(),
                ..StartRequest::default()
            })
            .await
            .expect("start succeeds");
        wait_status(&tasks, "t1", TaskStatus::Running).await;

        let result = run(
            tool_named(&runner, "agent_send").as_ref(),
            r#"{"task_id":"t1","message":"use the new constraint"}"#,
        )
        .await;
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(
            result.content,
            "message queued for task t1; it will be read at the next checkpoint"
        );
        release.cancel();
        wait_final(&tasks, "t1").await;

        let requests = provider.requests();
        let second = requests.get(1).expect("a second provider request was made");
        assert!(
            second.messages.iter().any(|message| {
                message.role == otto_core::model::Role::Context
                    && message.context_type == "parent_message"
                    && message.text().contains("use the new constraint")
            }),
            "second request should include the parent message: {second:#?}"
        );
    }

    #[tokio::test]
    async fn agent_send_rejects_unknown_blank_and_finished_tasks() {
        let provider = FakeProvider::new();
        provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
        let tasks = Arc::new(Tasks::new());
        let runner = runner(test_config(&provider, &tasks, Vec::new()));
        let send = tool_named(&runner, "agent_send");

        let unknown = run(send.as_ref(), r#"{"task_id":"missing","message":"hello"}"#).await;
        assert!(unknown.is_error);
        assert_eq!(unknown.content, "unknown task: missing");

        let blank = run(send.as_ref(), r#"{"task_id":"t1","message":"   "}"#).await;
        assert!(blank.is_error);
        assert_eq!(blank.content, "message is required");

        runner
            .start(StartRequest {
                prompt: "finish".into(),
                ..StartRequest::default()
            })
            .await
            .expect("start succeeds");
        wait_final(&tasks, "t1").await;
        let finished = run(send.as_ref(), r#"{"task_id":"t1","message":"too late"}"#).await;
        assert!(finished.is_error);
        assert_eq!(
            finished.content,
            "task t1 is already completed; cannot send message"
        );
    }

    #[tokio::test]
    async fn agent_send_can_address_named_queued_task() {
        let provider = FakeProvider::new();
        let release = CancellationToken::new();
        provider.set_hook(block_until(release.clone()));
        provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
        let tasks = Arc::new(Tasks::new());
        let mut config = test_config(&provider, &tasks, Vec::new());
        config.max_parallel = 1;
        let runner = runner(config);
        for (name, prompt) in [("first", "a"), ("second", "b")] {
            runner
                .start(StartRequest {
                    prompt: prompt.into(),
                    name: name.into(),
                    ..StartRequest::default()
                })
                .await
                .expect("start succeeds");
        }
        wait_status(&tasks, "t1", TaskStatus::Running).await;
        assert_eq!(
            tasks.get("second").expect("named task").status,
            TaskStatus::Queued
        );

        let sent = run(
            tool_named(&runner, "agent_send").as_ref(),
            r#"{"task_id":"second","message":"queued context"}"#,
        )
        .await;
        assert!(!sent.is_error, "{}", sent.content);
        assert!(sent.content.starts_with("message queued for task t2"));
        release.cancel();
        wait_final(&tasks, "t1").await;
        wait_final(&tasks, "t2").await;

        let queued_request = provider
            .requests()
            .into_iter()
            .find(|request| crate::subagent::testsupport::last_user_text(request) == "b")
            .expect("queued task eventually ran");
        assert!(queued_request.messages.iter().any(|message| {
            message.role == otto_core::model::Role::Context
                && message.context_type == "parent_message"
                && message.text().contains("queued context")
        }));
    }

    #[tokio::test]
    async fn tool_definitions_include_agent_send_and_agent_cancel() {
        let names: Vec<String> = tool_definitions()
            .into_iter()
            .map(|definition| definition.name)
            .collect();
        assert_eq!(
            names,
            [
                "agent",
                "agent_status",
                "agent_yield",
                "agent_send",
                "agent_cancel"
            ]
        );
    }

    #[tokio::test]
    async fn agent_cancel_stops_a_running_task_and_reports_it() {
        let provider = FakeProvider::new();
        provider.set_hook(block_until(CancellationToken::new()));
        let tasks = Arc::new(Tasks::new());
        let runner = runner(test_config(&provider, &tasks, Vec::new()));
        runner
            .start(StartRequest {
                prompt: "go".into(),
                name: "stuck".into(),
                ..StartRequest::default()
            })
            .await
            .expect("start succeeds");
        wait_status(&tasks, "t1", TaskStatus::Running).await;

        let result = run(
            tool_named(&runner, "agent_cancel").as_ref(),
            r#"{"task_id":"t1"}"#,
        )
        .await;
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("cancel requested for task t1"));

        // A name resolves to the same task.
        let final_task = wait_final(&tasks, "t1").await;
        assert_eq!(final_task.status, TaskStatus::Canceled);
        let named = run(
            tool_named(&runner, "agent_cancel").as_ref(),
            r#"{"task_id":"stuck"}"#,
        )
        .await;
        assert!(named.is_error, "{}", named.content);
        assert_eq!(
            named.content,
            "task stuck is already completed; nothing to cancel"
        );
    }

    #[tokio::test]
    async fn agent_cancel_stops_a_queued_task_before_it_starts() {
        let provider = FakeProvider::new();
        provider.set_hook(block_until(CancellationToken::new()));
        let tasks = Arc::new(Tasks::new());
        let mut config = test_config(&provider, &tasks, Vec::new());
        config.max_parallel = 1;
        let runner = runner(config);
        for prompt in ["first", "second"] {
            runner
                .start(StartRequest {
                    prompt: prompt.into(),
                    ..StartRequest::default()
                })
                .await
                .expect("start succeeds");
        }
        wait_status(&tasks, "t1", TaskStatus::Running).await;
        assert_eq!(
            tasks.get("t2").expect("t2 exists").status,
            TaskStatus::Queued
        );

        let result = run(
            tool_named(&runner, "agent_cancel").as_ref(),
            r#"{"task_id":"t2"}"#,
        )
        .await;
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(wait_final(&tasks, "t2").await.status, TaskStatus::Canceled);
        // The running task is untouched by cancelling its neighbour.
        assert_eq!(
            tasks.get("t1").expect("t1 exists").status,
            TaskStatus::Running
        );
        tasks.cancel("t1").expect("cancel succeeds");
        wait_final(&tasks, "t1").await;
    }

    #[tokio::test]
    async fn agent_cancel_rejects_an_unknown_task_and_a_missing_id() {
        let provider = FakeProvider::new();
        let tasks = Arc::new(Tasks::new());
        let runner = runner(test_config(&provider, &tasks, Vec::new()));
        let tool = tool_named(&runner, "agent_cancel");

        let unknown = run(tool.as_ref(), r#"{"task_id":"t9"}"#).await;
        assert!(unknown.is_error);
        assert_eq!(unknown.content, "unknown task: t9");

        for body in [
            r#"{}"#,
            r#"{"task_id":"   "}"#,
            r#"{"task_id":"t1","extra":1}"#,
        ] {
            assert!(
                run(tool.as_ref(), body).await.is_error,
                "body {body} must be rejected"
            );
        }
    }

    #[tokio::test]
    async fn agent_cancel_errors_for_a_finished_task() {
        let provider = FakeProvider::new();
        provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
        let tasks = Arc::new(Tasks::new());
        let runner = runner(test_config(&provider, &tasks, Vec::new()));
        runner
            .start(StartRequest {
                prompt: "go".into(),
                ..StartRequest::default()
            })
            .await
            .expect("start succeeds");
        wait_final(&tasks, "t1").await;

        let result = run(
            tool_named(&runner, "agent_cancel").as_ref(),
            r#"{"task_id":"t1"}"#,
        )
        .await;
        assert!(result.is_error, "{}", result.content);
        assert_eq!(
            result.content,
            "task t1 is already completed; nothing to cancel"
        );
    }

    #[tokio::test]
    async fn agent_status_by_name() {
        let provider = FakeProvider::new();
        provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
        let tasks = Arc::new(Tasks::new());
        let runner = runner(test_config(&provider, &tasks, Vec::new()));
        runner
            .start(StartRequest {
                prompt: "go".into(),
                name: "lint-check".into(),
                ..StartRequest::default()
            })
            .await
            .expect("start succeeds");
        wait_final(&tasks, "t1").await;

        let result = run(
            tool_named(&runner, "agent_status").as_ref(),
            r#"{"task_id":"lint-check"}"#,
        )
        .await;
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("t1"), "{}", result.content);
        assert!(result.content.contains("succeeded"), "{}", result.content);
    }

    #[tokio::test]
    async fn agent_tool_model_selection_with_definition() {
        for (body, want) in [
            (
                r#"{"prompt":"go","agent":"reviewer","model":"call-model"}"#,
                "call-model",
            ),
            (r#"{"prompt":"go","agent":"reviewer"}"#, "def-model"),
        ] {
            let provider = FakeProvider::new();
            provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
            let tasks = Arc::new(Tasks::new());
            let mut config = test_config(&provider, &tasks, Vec::new());
            config.template.model = "gpt-parent".into();
            config.catalog = Catalog::from_definitions(vec![Definition {
                name: "reviewer".into(),
                model: "def-model".into(),
                ..Definition::default()
            }]);
            let runner = runner(config);

            let result = run(tool_named(&runner, "agent").as_ref(), body).await;
            assert!(!result.is_error, "{}", result.content);
            assert_eq!(wait_final(&tasks, "t1").await.model, want, "body {body}");
        }
    }

    /// The `agent` tool reports a queued task when the concurrency limit is
    /// already reached, rather than the started text.
    #[tokio::test]
    async fn agent_tool_reports_queued_when_at_the_limit() {
        let provider = FakeProvider::new();
        let release = CancellationToken::new();
        provider.set_hook(block_until(release.clone()));
        provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
        let tasks = Arc::new(Tasks::new());
        let mut config = test_config(&provider, &tasks, vec![stub("read")]);
        config.max_parallel = 1;
        let runner = runner(config);
        let agent_tool = tool_named(&runner, "agent");

        run(agent_tool.as_ref(), r#"{"prompt":"first"}"#).await;
        wait_status(&tasks, "t1", TaskStatus::Running).await;
        let second = run(agent_tool.as_ref(), r#"{"prompt":"second"}"#).await;
        assert_eq!(
            second.content,
            "task t2 (default) queued (1 running, limit 1)"
        );

        release.cancel();
        for id in ["t1", "t2"] {
            wait_final(&tasks, id).await;
        }
    }
}
