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

const AGENT_DESCRIPTION: &str = "Start a sub-agent on a self-contained task and return immediately with \
    its task id. The sub-agent runs in parallel with you, has its own \
    context (fresh unless context is \"inherit\" or fork_before is set), \
    the same workspace and file tools, and receives follow-up parent task \
    updates only when you call agent_send. Research stays in the child \
    context: use fresh context, download pinned sources once into its \
    temporary research directory, and return a summary, uncertainty, report \
    path and section index within 4 KiB; put longer findings in the \
    assigned persistent report directory using write when permitted. \
    Oversized final answers are automatically saved when workspace storage \
    is available. Temporary research directories are removed at task exit; \
    report directories persist. Its final report arrives later as a \
    [task-notification] message that starts a runtime-owned wake turn when \
    the parent is idle; when only child dependencies remain, call \
    agent_yield rather than polling agent_status. Use agent_status to \
    inspect progress or retrieve a report path, and agent_send when the \
    user adds constraints, priorities, facts, or direction changes for a \
    queued or running task. For implementation, settle shared interfaces \
    first and assign each writable file to one owner. Delegate a complete \
    coherent change, including its call-site adaptations; keep tightly \
    coupled edits in one task. Assign a bounded file scope and request \
    findings with file/line evidence. After delegation, do not read those \
    same files again; rely on the child report, use read on its report file \
    for omitted sections, and ask follow-up questions only while the child \
    is queued or running. Put the initial goal, relevant paths, \
    constraints, and requested report format in prompt; pass agent to use a \
    named definition from the Agents list. When the user explicitly asks for \
    an external coding agent, agent may be acp:codex, acp:claude, or acp:pi. \
    Those run installed adapters with their own permissions and sandbox, not \
    Otto's tool restrictions; they accept fresh context only, do not support \
    agent_send or resume, and use their own model settings.";

const AGENT_YIELD_DESCRIPTION: &str = "Yield this parent turn when only child dependencies remain and there is \
    no independent work to do. Returns immediately and requests a \
    safe-boundary stop of model calls after this tool batch settles; child \
    messages/completion or user input resume the parent through the \
    runtime. Does not wait in the tool, poll, or cancel children. Takes no \
    arguments. Returns an error without yielding if no child is queued or \
    running. Pending input, tool errors and approval replanning take \
    precedence.";

const AGENT_STATUS_DESCRIPTION: &str = "Show live or recovered non-running task snapshots and recovery coverage/errors; \
    recovery never restarts execution. Show task status and a bounded final conclusion (at most 4 KiB plus \
    truncation and report-file metadata). Recent research steps are omitted \
    by default; set include_history only when explicitly needed. For a \
    truncated final answer, returns its persistent report path when storage \
    succeeds; use read on that file to retrieve sections without restarting \
    the child. Pass task_id for detail including model and model_source \
    (explicit, definition, configured_first, parent, or unknown for legacy \
    records). Omit task_id to list tasks. When only child dependencies \
    remain, call agent_yield instead of polling. Live detail distinguishes \
    tool, provider/runtime and approval phases; unchanged last-tool \
    counters do not establish a stall.";

const AGENT_SEND_DESCRIPTION: &str = "Send a task update to a queued or running sub-agent. Use this when the \
    user adds constraints, clarifies priorities, provides new facts, or \
    asks the child to adjust direction. You can also ask the child a \
    question; the answer arrives later as a [task-report] notification. The \
    child reads updates at the next safe checkpoint; this does not \
    interrupt an in-flight provider or tool call. It cannot be sent to a \
    finished task. Returns an error, and the child does not receive the \
    update, when it cannot be saved to the child's pending-message file. \
    Updates not yet read survive a restart and reach the child when it is \
    resumed. To check progress instead, use agent_status, which reads the \
    task registry and does not run the child model. External ACP tasks do \
    not support agent_send.";

const AGENT_CANCEL_DESCRIPTION: &str = "Cancel a queued or running sub-agent task. Use it for tasks that are no \
    longer needed or have concrete failure/stall evidence. Do not infer a \
    stall from an unchanged last tool or step count while the task is in \
    provider/runtime or approval activity. Cancellation is cooperative: the \
    task stops at its next safe checkpoint, a task inside a tool call stops \
    once that call settles, and a queued task never starts. External ACP \
    tasks receive session/cancel and are terminated after a grace period. The task ends \
    as canceled and produces the usual [task-notification].";

const AGENT_CONTEXT_DESCRIPTION: &str = "How the sub-agent starts. fresh (default, or the definition's setting): \
    it sees only prompt. inherit: it also receives a copy of this \
    conversation up to this call, or up to fork_before when that is set. \
    Prefer, in order: (1) fresh with a self-contained prompt: goal, paths, \
    constraints, what to report back; (2) fresh, with the context you \
    already obtained pasted into prompt (file excerpts, tool output, \
    decisions), so the sub-agent skips the tool calls that produced it; (3) \
    inherit, when that context is too large or too scattered to paste and \
    the sub-agent would otherwise repeat expensive tool calls. A sub-agent \
    never shares your prompt cache, so inherit costs one full uncached pass \
    over this conversation per sub-agent, and everything irrelevant to the \
    task goes in with it.";

const AGENT_NAME_DESCRIPTION: &str = "Optional task name, unique in this session; usable instead of the task \
    id in agent_status, agent_send, agent_cancel, and /task. 1 to 64 \
    letters, digits, '_' or '-'.";

const AGENT_FORK_BEFORE_DESCRIPTION: &str = "Text of an earlier user prompt in this conversation that started a \
    turn: its full text or its first line. The sub-agent receives this \
    conversation as it was just before that prompt, including turns that \
    compaction has since summarized, and context becomes inherit. Exactly \
    one prompt must match; otherwise the error lists the prompts that can \
    be used.";
const AGENT_RESUME_DESCRIPTION: &str = "Task id or name of an interrupted sub-agent task in this session. With \
    resume set, this continues that task on its own transcript instead of \
    starting a new one: prompt is the next user message delivered to it, \
    and agent, model, name, description, and fork_before must be absent or \
    empty, and context absent or \"fresh\"; the task keeps its recorded \
    values. Resume atomically admits one new attempt; inspect uncertain tool effects first. It never replays a suspended process. \
    For completed tasks, use agent_status and read the saved report \
    instead. External ACP tasks cannot be resumed.";
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
                    "description": "The complete task, assigned file ownership, agreed shared interfaces, \
                        constraints and requested evidence. Include all call-site adaptations \
                        for a coherent change; the sub-agent sees nothing else."
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
                    "description": "Provider model ID for the sub-agent. Overrides the definition's model. \
                        When omitted, uses the definition's model, then the first configured \
                        sub-agent model, then this session's model. Otto does not validate \
                        availability; an ID the endpoint rejects fails the task with the \
                        provider's error."
                },
                "agent": {
                    "type": "string",
                    "description": "Name of an agent definition, or acp:codex, acp:claude, or acp:pi for an installed external ACP adapter. Omit for the default sub-agent."
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

// Yield changes turn scheduling and waits on notifications, not a pure status read.
pub(crate) const AGENT_YIELD_RETRY: (&str, otto_core::model::RetrySafety) =
    ("agent_yield", otto_core::model::RetrySafety::NonIdempotent);

#[async_trait::async_trait]
impl Tool for AgentYieldTool {
    fn retry_safety(&self) -> otto_core::model::RetrySafety {
        AGENT_YIELD_RETRY.1
    }

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
        let children: Vec<_> = self
            .runner
            .tasks()
            .list()
            .into_iter()
            .filter(|task| !task.is_final())
            .map(|task| task.id)
            .collect();
        if children.is_empty() {
            return error_result("No queued or running child task; continue with available results instead of yielding.").not_started();
        }
        let ids = serde_json::to_string(&children).expect("string list serializes");
        text_result(format!("Waiting for child tasks.\nWait task IDs: {ids}"))
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

pub(crate) const AGENT_RETRY: (&str, otto_core::model::RetrySafety) =
    ("agent", otto_core::model::RetrySafety::NonIdempotent);

#[async_trait::async_trait]
impl Tool for AgentTool {
    fn retry_safety(&self) -> otto_core::model::RetrySafety {
        AGENT_RETRY.1
    }

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
        if !requested_agent.is_empty()
            && self.runner.catalog().lookup(requested_agent).is_none()
            && super::runner::external::adapter(requested_agent).is_none()
        {
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

pub(crate) const AGENT_STATUS_RETRY: (&str, otto_core::model::RetrySafety) =
    ("agent_status", otto_core::model::RetrySafety::ReadOnly);

#[async_trait::async_trait]
impl Tool for AgentStatusTool {
    fn retry_safety(&self) -> otto_core::model::RetrySafety {
        AGENT_STATUS_RETRY.1
    }

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
        let diagnostics = tasks.recovery_diagnostics();
        if all.is_empty() {
            return text_result(if diagnostics.is_empty() {
                "no tasks in this session".into()
            } else {
                format!(
                    "0 task snapshots; recovery coverage is not empty evidence:\n{}",
                    diagnostics.join("\n")
                )
            });
        }

        let now = self.runner.now();
        if args.task_id.is_empty() {
            let mut lines: Vec<String> = all.iter().map(|task| status_line(task, now)).collect();
            lines.extend(diagnostics);
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
        lines.extend(diagnostics);
        if !task.model.is_empty() {
            lines.push(format!("model: {}", task.model));
        }
        lines.push(format!("model_source: {}", task.model_source.as_str()));
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

pub(crate) const AGENT_SEND_RETRY: (&str, otto_core::model::RetrySafety) =
    ("agent_send", otto_core::model::RetrySafety::NonIdempotent);

#[async_trait::async_trait]
impl Tool for AgentSendTool {
    fn retry_safety(&self) -> otto_core::model::RetrySafety {
        AGENT_SEND_RETRY.1
    }

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

pub(crate) const AGENT_CANCEL_RETRY: (&str, otto_core::model::RetrySafety) =
    ("agent_cancel", otto_core::model::RetrySafety::NonIdempotent);

#[async_trait::async_trait]
impl Tool for AgentCancelTool {
    fn retry_safety(&self) -> otto_core::model::RetrySafety {
        AGENT_CANCEL_RETRY.1
    }

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
mod tests;
