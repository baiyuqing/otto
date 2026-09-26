//! The parent-facing sub-agent tools: `agent`, `agent_wait`, `agent_status`.
//!
//! Ownership: each tool holds an `Arc` to the session's [`Runner`] and owns
//! nothing else. Concurrency and cancellation: `agent` returns as soon as the
//! task is registered unless `wait` is set; the two waiting paths park on the
//! task registry and stop when the tool's token is cancelled or the timeout
//! elapses. Errors are reported in band, as the tool contract requires.
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
use super::runner::{
    Runner, StartRequest, agent_label, completion_text, definition_label, pluralize_tools,
};
use super::tasks::{Task, TaskStatus, Tasks};
use super::{Catalog, runner};
use crate::tool::result::decode_strict_json;
use crate::tool::{CONTEXT_CANCELED, Tool, definition, error_result, text_result};

const DEFAULT_WAIT_TIMEOUT_SECONDS: i64 = 600;
const MAX_WAIT_TIMEOUT_SECONDS: i64 = 3600;

const AGENT_DESCRIPTION: &str = "Start a sub-agent on a self-contained task and return immediately with its task id. The sub-agent runs in parallel with you, has its own context (fresh unless context is \"inherit\"), the same workspace and file tools, and never sees what you do after this call. Its final report arrives later as a [task-notification] message. Use agent_wait when you need the result before continuing, agent_status to check progress. Put everything the sub-agent needs into prompt: goal, relevant paths, what to report back. Pass agent to use a named definition from the Agents list.";

const AGENT_WAIT_DESCRIPTION: &str = "Wait for a sub-agent task to finish. With task_id, waits for that task; without it, waits for every task that is queued or running. Blocks up to timeout_seconds (default 600, max 3600) and returns each task's completion report. Errors if the wait times out or is canceled, naming the tasks still running, or if task_id is unknown.";

const AGENT_STATUS_DESCRIPTION: &str = "Show sub-agent task status. Without task_id, one line per task in this session: id, status, elapsed time, and current activity or token total. With task_id, that line plus the task's recent steps and, once finished, its result or error.";

const AGENT_SEND_DESCRIPTION: &str = "Send a follow-up message to a queued or running sub-agent. The message is delivered into the child context at its next safe checkpoint: before its first provider request if queued, or after its current provider/tool step completes if running. It cannot be sent to a finished task.";

const AGENT_CONTEXT_DESCRIPTION: &str = "How the sub-agent starts. fresh (default, or the definition's setting): it sees only prompt. inherit: it also receives a copy of this conversation up to this call. Prefer, in order: (1) fresh with a self-contained prompt: goal, paths, constraints, what to report back; (2) fresh, with the context you already obtained pasted into prompt (file excerpts, tool output, decisions), so the sub-agent skips the tool calls that produced it; (3) inherit, when that context is too large or too scattered to paste and the sub-agent would otherwise repeat expensive tool calls. A sub-agent never shares your prompt cache, so inherit costs one full uncached pass over this conversation per sub-agent, and everything irrelevant to the task goes in with it.";

const AGENT_NAME_DESCRIPTION: &str = "Optional task name, unique in this session; usable instead of the task id in agent_wait, agent_status, and /task. 1 to 64 letters, digits, '_' or '-'.";

/// The `agent`, `agent_wait` and `agent_status` definitions, without a built
/// [`Runner`]. The redaction boundary check needs the tool set a real runner
/// would register, and none of the three definitions read runner state.
pub fn tool_definitions() -> Vec<ToolDefinition> {
    vec![
        agent_definition(),
        agent_wait_definition(),
        agent_status_definition(),
        agent_send_definition(),
    ]
}

/// The parent-side tools in registration order.
pub fn tools(runner: &Arc<Runner>) -> Vec<Box<dyn Tool + Send + Sync>> {
    vec![
        Box::new(AgentTool {
            runner: Arc::clone(runner),
        }),
        Box::new(AgentWaitTool {
            runner: Arc::clone(runner),
        }),
        Box::new(AgentStatusTool {
            runner: Arc::clone(runner),
        }),
        Box::new(AgentSendTool {
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
                    "description": "The complete task; the sub-agent sees nothing else."
                },
                "description": {
                    "type": "string",
                    "description": "Short label (<= 80 chars) shown in status output."
                },
                "name": {
                    "type": "string",
                    "description": AGENT_NAME_DESCRIPTION
                },
                "wait": {
                    "type": "boolean",
                    "description": "Block until the task ends and return its result instead of its id."
                },
                "model": {
                    "type": "string",
                    "description": "Provider model id for the sub-agent. Default: this session's model. Otto does not validate it; an id the endpoint rejects fails the task with the provider's error. Overrides the definition's model."
                },
                "agent": {
                    "type": "string",
                    "description": "Name of a definition from the Agents list in the system prompt: it sets the sub-agent's instructions, tool set, and default model. Omit for the default sub-agent."
                },
                "context": {
                    "type": "string",
                    "enum": ["fresh", "inherit"],
                    "description": AGENT_CONTEXT_DESCRIPTION
                }
            },
            "required": ["prompt"]
        }),
    )
}

fn agent_wait_definition() -> ToolDefinition {
    definition(
        "agent_wait",
        AGENT_WAIT_DESCRIPTION,
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "task_id": {
                    "type": "string",
                    "description": "Task id or name; omit to wait for every queued or running task."
                },
                "timeout_seconds": {
                    "type": "integer",
                    "description": "Maximum time to wait, in seconds. Default 600, max 3600."
                }
            }
        }),
    )
}

fn agent_status_definition() -> ToolDefinition {
    definition(
        "agent_status",
        AGENT_STATUS_DESCRIPTION,
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "task_id": {
                    "type": "string",
                    "description": "Task id or name; show one task's detail, including its recent steps."
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
                    "description": "The follow-up prompt or context to deliver to the sub-agent."
                }
            },
            "required": ["task_id", "message"]
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
    wait: bool,
    #[serde(default)]
    model: String,
    #[serde(default)]
    agent: String,
    #[serde(default)]
    context: String,
    #[serde(default)]
    name: String,
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
        let args: AgentArgs = match decode_strict_json(arguments.get(), &["prompt"]) {
            Ok(args) => args,
            Err(message) => return error_result(message),
        };
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
        let task = match self.runner.start(StartRequest {
            prompt: args.prompt,
            description: args.description,
            model: args.model,
            agent: args.agent,
            context: args.context,
            name: args.name,
        }) {
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
        if !args.wait {
            return text_result(started);
        }

        match wait_tasks(
            self.runner.tasks(),
            std::slice::from_ref(&task.id),
            self.runner.max_output_bytes(),
            cancel,
        )
        .await
        {
            Ok(text) => text_result(text),
            Err(remaining) => error_result(format!(
                "wait canceled; still running: {}",
                remaining.join(", ")
            )),
        }
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

/// Waits for each id in order, stopping at the first that does not complete.
///
/// On success it removes each task's finished notification, so the inbox does
/// not deliver it a second time, and returns their completion texts joined by
/// a blank line. On failure it returns the ids from the failing one onward,
/// removing no notification and returning no text.
async fn wait_tasks(
    tasks: &Tasks,
    ids: &[String],
    max_output_bytes: usize,
    cancel: &CancellationToken,
) -> Result<String, Vec<String>> {
    let mut completed = Vec::with_capacity(ids.len());
    for (index, id) in ids.iter().enumerate() {
        match tasks.wait(id, cancel).await {
            Ok(task) => completed.push(task),
            Err(_) => return Err(ids[index..].to_vec()),
        }
    }
    let texts: Vec<String> = completed
        .iter()
        .map(|task| {
            tasks.notifications().remove(
                &task.id,
                otto_core::agent::inbox::NotificationKind::TaskFinished,
            );
            completion_text(task, max_output_bytes)
        })
        .collect();
    Ok(texts.join("\n\n"))
}

/// The ids in `ids` that have not yet reached a final status, in order. A
/// timeout drops the in-flight [`wait_tasks`] future, so the still-running
/// suffix is recomputed from the registry instead.
fn still_running(tasks: &Tasks, ids: &[String]) -> Vec<String> {
    ids.iter()
        .skip_while(|id| tasks.get(id).is_some_and(|task| task.is_final()))
        .cloned()
        .collect()
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentWaitArgs {
    #[serde(default)]
    task_id: String,
    #[serde(default)]
    timeout_seconds: i64,
}

struct AgentWaitTool {
    runner: Arc<Runner>,
}

#[async_trait::async_trait]
impl Tool for AgentWaitTool {
    fn definition(&self) -> ToolDefinition {
        agent_wait_definition()
    }

    async fn execute(&self, arguments: &RawValue, cancel: &CancellationToken) -> ToolResult {
        let args: AgentWaitArgs = match decode_strict_json(arguments.get(), &[]) {
            Ok(args) => args,
            Err(message) => return error_result(message),
        };
        if args.timeout_seconds < 0 {
            return error_result("timeout_seconds must not be negative");
        }
        let timeout_seconds = match args.timeout_seconds {
            0 => DEFAULT_WAIT_TIMEOUT_SECONDS,
            seconds => seconds.min(MAX_WAIT_TIMEOUT_SECONDS),
        };

        let tasks = self.runner.tasks();
        let ids = if args.task_id.is_empty() {
            let ids: Vec<String> = tasks
                .list()
                .into_iter()
                .filter(|task| !task.is_final())
                .map(|task| task.id)
                .collect();
            if ids.is_empty() {
                return text_result("no tasks are running");
            }
            ids
        } else {
            match tasks.get(&args.task_id) {
                Some(task) => vec![task.id],
                None => return error_result(format!("unknown task: {}", args.task_id)),
            }
        };

        let timeout = std::time::Duration::from_secs(timeout_seconds as u64);
        let outcome = tokio::select! {
            outcome = wait_tasks(tasks, &ids, self.runner.max_output_bytes(), cancel) => outcome,
            () = tokio::time::sleep(timeout) => Err(still_running(tasks, &ids)),
        };
        match outcome {
            Ok(text) => text_result(text),
            Err(remaining) if cancel.is_cancelled() => error_result(format!(
                "wait canceled; still running: {}",
                remaining.join(", ")
            )),
            Err(remaining) => error_result(format!(
                "timed out after {timeout_seconds}s; still running: {}",
                remaining.join(", ")
            )),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentStatusArgs {
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
            return text_result(lines.join("\n"));
        }

        let Some(task) = tasks.get(&args.task_id) else {
            return error_result(format!("unknown task: {}", args.task_id));
        };
        let mut lines = vec![status_line(&task, now)];
        if !task.model.is_empty() {
            lines.push(format!("model: {}", task.model));
        }
        lines.extend(history_lines(
            &tasks.history(&args.task_id).unwrap_or_default(),
        ));
        if task.is_final() {
            if task.error.is_empty() {
                lines.push("result:".to_string());
                lines.push(task.result.clone());
            } else {
                lines.push(format!("error: {}", task.error));
            }
        }
        text_result(lines.join("\n"))
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
    use crate::subagent::runner::{Config, StartRequest, completion_text};
    use crate::subagent::testsupport::{
        FakeProvider, RouteStep, StubTool, assistant_text, assistant_tool_call, match_any,
        match_prompt, raw, stub, test_config, wait_status,
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
    async fn agent_tool_wait_true_blocks_and_returns_completion() {
        let provider = FakeProvider::new();
        provider.add_route(
            match_prompt("do it"),
            vec![assistant_text(
                "all done",
                Usage {
                    input_tokens: 4,
                    output_tokens: 2,
                    ..Usage::default()
                },
            )],
        );
        let tasks = Arc::new(Tasks::new());
        let runner = runner(test_config(&provider, &tasks, Vec::new()));

        let agent_tool = tool_named(&runner, "agent");
        let result = run(agent_tool.as_ref(), r#"{"prompt":"do it","wait":true}"#).await;
        assert!(!result.is_error, "{}", result.content);

        let final_task = tasks.get("t1").expect("t1 exists");
        assert_eq!(
            result.content,
            completion_text(&final_task, runner.max_output_bytes())
        );
        assert_eq!(
            tasks
                .notifications()
                .remove("t1", NotificationKind::TaskFinished),
            None,
            "agent with wait removes the notification"
        );
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
    async fn agent_wait_named_task_waits_and_removes_its_notification() {
        let provider = FakeProvider::new();
        provider.add_route(
            match_any,
            vec![assistant_text(
                "done",
                Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                    ..Usage::default()
                },
            )],
        );
        let tasks = Arc::new(Tasks::new());
        let runner = runner(test_config(&provider, &tasks, Vec::new()));
        runner
            .start(StartRequest {
                prompt: "go".into(),
                ..StartRequest::default()
            })
            .expect("start succeeds");

        let wait_tool = tool_named(&runner, "agent_wait");
        let result = run(wait_tool.as_ref(), r#"{"task_id":"t1"}"#).await;
        assert!(!result.is_error, "{}", result.content);
        let final_task = tasks.get("t1").expect("t1 exists");
        assert_eq!(
            result.content,
            completion_text(&final_task, runner.max_output_bytes())
        );
        assert_eq!(
            tasks
                .notifications()
                .remove("t1", NotificationKind::TaskFinished),
            None
        );
    }

    #[tokio::test]
    async fn agent_wait_without_task_id_waits_for_every_non_final_task() {
        let provider = FakeProvider::new();
        let release = CancellationToken::new();
        provider.set_hook(block_until(release.clone()));
        provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);

        let tasks = Arc::new(Tasks::new());
        let mut config = test_config(&provider, &tasks, Vec::new());
        config.max_parallel = 2;
        let runner = runner(config);
        for prompt in ["a", "b"] {
            runner
                .start(StartRequest {
                    prompt: prompt.into(),
                    ..StartRequest::default()
                })
                .expect("start succeeds");
        }
        wait_status(&tasks, "t1", TaskStatus::Running).await;
        wait_status(&tasks, "t2", TaskStatus::Running).await;

        let wait_tool = tool_named(&runner, "agent_wait");
        let handle = tokio::spawn(async move { run(wait_tool.as_ref(), "{}").await });
        // Let agent_wait read the non-final task list before they finish.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        release.cancel();

        let result = handle.await.expect("the wait task did not panic");
        assert!(!result.is_error, "{}", result.content);
        let want = format!(
            "{}\n\n{}",
            completion_text(&tasks.get("t1").expect("t1"), runner.max_output_bytes()),
            completion_text(&tasks.get("t2").expect("t2"), runner.max_output_bytes())
        );
        assert_eq!(result.content, want);
    }

    #[tokio::test]
    async fn agent_wait_with_no_tasks_running_is_not_an_error() {
        let provider = FakeProvider::new();
        let tasks = Arc::new(Tasks::new());
        let runner = runner(test_config(&provider, &tasks, Vec::new()));
        let result = run(tool_named(&runner, "agent_wait").as_ref(), "{}").await;
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(result.content, "no tasks are running");
    }

    #[tokio::test]
    async fn agent_wait_unknown_task_id_errors() {
        let provider = FakeProvider::new();
        let tasks = Arc::new(Tasks::new());
        let runner = runner(test_config(&provider, &tasks, Vec::new()));
        let result = run(
            tool_named(&runner, "agent_wait").as_ref(),
            r#"{"task_id":"t9"}"#,
        )
        .await;
        assert!(result.is_error);
        assert_eq!(result.content, "unknown task: t9");
    }

    #[tokio::test]
    async fn agent_wait_negative_timeout_errors() {
        let provider = FakeProvider::new();
        let tasks = Arc::new(Tasks::new());
        let runner = runner(test_config(&provider, &tasks, Vec::new()));
        let result = run(
            tool_named(&runner, "agent_wait").as_ref(),
            r#"{"timeout_seconds":-1}"#,
        )
        .await;
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn agent_wait_timeout_reports_still_running_tasks() {
        let provider = FakeProvider::new();
        provider.set_hook(block_until(CancellationToken::new()));
        let tasks = Arc::new(Tasks::new());
        let runner = runner(test_config(&provider, &tasks, Vec::new()));
        runner
            .start(StartRequest {
                prompt: "go".into(),
                ..StartRequest::default()
            })
            .expect("start succeeds");
        wait_status(&tasks, "t1", TaskStatus::Running).await;

        let result = run(
            tool_named(&runner, "agent_wait").as_ref(),
            r#"{"timeout_seconds":1}"#,
        )
        .await;
        assert!(result.is_error, "{}", result.content);
        assert_eq!(result.content, "timed out after 1s; still running: t1");

        tasks.cancel("t1").expect("cancel succeeds");
        wait_final(&tasks, "t1").await;
    }

    #[tokio::test]
    async fn agent_wait_cancel_reports_still_running_tasks() {
        let provider = FakeProvider::new();
        provider.set_hook(block_until(CancellationToken::new()));
        let tasks = Arc::new(Tasks::new());
        let runner = runner(test_config(&provider, &tasks, Vec::new()));
        runner
            .start(StartRequest {
                prompt: "go".into(),
                ..StartRequest::default()
            })
            .expect("start succeeds");
        wait_status(&tasks, "t1", TaskStatus::Running).await;

        let wait_tool = tool_named(&runner, "agent_wait");
        let call = CancellationToken::new();
        let canceller = call.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            canceller.cancel();
        });
        let result = wait_tool.execute(&raw("{}"), &call).await;
        assert!(result.is_error, "{}", result.content);
        assert_eq!(result.content, "wait canceled; still running: t1");

        tasks.cancel("t1").expect("cancel succeeds");
        wait_final(&tasks, "t1").await;
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
            .expect("start succeeds");
        wait_status(&tasks, "t1", TaskStatus::Running).await;
        runner
            .start(StartRequest {
                prompt: "review the diff".into(),
                ..StartRequest::default()
            })
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
            .expect("start succeeds");
        wait_final(&tasks, "t1").await;

        let result = run(
            tool_named(&runner, "agent_status").as_ref(),
            r#"{"task_id":"t1"}"#,
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
    async fn agent_wait_by_name_removes_notification() {
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
            .expect("start succeeds");

        let result = run(
            tool_named(&runner, "agent_wait").as_ref(),
            r#"{"task_id":"lint-check"}"#,
        )
        .await;
        assert!(!result.is_error, "{}", result.content);
        let final_task = tasks.get("t1").expect("t1 exists");
        assert_eq!(
            result.content,
            completion_text(&final_task, runner.max_output_bytes())
        );
        assert_eq!(tasks.pending(), 0);
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
    async fn tool_definitions_include_agent_send() {
        let names: Vec<String> = tool_definitions()
            .into_iter()
            .map(|definition| definition.name)
            .collect();
        assert_eq!(names, ["agent", "agent_wait", "agent_status", "agent_send"]);
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
