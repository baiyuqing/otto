//! Model-callable task queueing and isolated-dialogue routing tools.

use std::sync::Arc;

use super::{ControlRoute, TaskControl, checked_text};
use crate::tool::{Tool, definition, error_result, text_result};
use otto_core::agent::redactor::Redactor;
use otto_core::model::ToolDefinition;
use otto_core::tool::ToolResult;
use serde::Deserialize;
use serde_json::json;
use serde_json::value::RawValue;
use tokio_util::sync::CancellationToken;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QueueArgs {
    prompt: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyArgs {}

/// Queues a model-originated prompt behind this logical user turn. In control
/// dialogue mode the same tool only classifies the original user message; the
/// frontend performs queue admission after the dialogue returns.
pub(crate) struct TaskQueue {
    control: Option<Arc<TaskControl>>,
    redactor: Option<Redactor>,
    route: Option<Arc<ControlRoute>>,
}

impl TaskQueue {
    pub(crate) fn new(control: Arc<TaskControl>, redaction_values: &[String]) -> Self {
        Self {
            control: Some(control),
            redactor: Some(Redactor::new(redaction_values)),
            route: None,
        }
    }

    pub(crate) fn for_control(route: Arc<ControlRoute>) -> Self {
        Self {
            control: None,
            redactor: None,
            route: Some(route),
        }
    }
}

pub(crate) const TASK_QUEUE_RETRY: (&str, otto_core::model::RetrySafety) =
    ("task_queue", otto_core::model::RetrySafety::NonIdempotent);

#[async_trait::async_trait]
impl Tool for TaskQueue {
    fn retry_safety(&self) -> otto_core::model::RetrySafety {
        TASK_QUEUE_RETRY.1
    }
    fn definition(&self) -> ToolDefinition {
        if self.route.is_some() {
            return definition(
                "task_queue",
                "Classify the original user message as a separate follow-up for after \
                    the active task. The frontend will queue that original message after \
                    this dialogue; do not rewrite or summarize it.",
                json!({"type":"object","additionalProperties":false,"properties":{},"required":[]}),
            );
        }
        definition(
            "task_queue",
            "Schedule a separate, agent-originated follow-up prompt to run after the \
                current logical task ends. Use only for work subordinate to the user's \
                current objective, not for new authority or approvals. Prompt must be \
                nonblank and at most 32000 UTF-8 bytes. At most 16 follow-ups run in \
                FIFO order per logical task, and are discarded if the task is cancelled \
                or fails. If a direct terminal turn ends while approval is pending, \
                remaining follow-ups are discarded with a notice. Follow-ups are saved \
                with the session: if Otto stops during the task, they run after the \
                task is automatically resumed, otherwise they are discarded with a \
                notice. Returns an error, and queues nothing, when the follow-up cannot \
                be saved.",
            json!({"type":"object","additionalProperties":false,"properties":{"prompt":{"type":"string","minLength":1,"maxLength":32000}},"required":["prompt"]}),
        )
    }

    async fn execute(&self, arguments: &RawValue, cancel: &CancellationToken) -> ToolResult {
        if cancel.is_cancelled() {
            return error_result(crate::tool::CONTEXT_CANCELED);
        }
        if let Some(route) = &self.route {
            if serde_json::from_str::<EmptyArgs>(arguments.get()).is_err() {
                return error_result("task_queue accepts no arguments in control dialogue mode");
            }
            return if route.queue() {
                text_result(
                    "The original user message is marked for follow-up; no prompt has been queued yet.",
                )
            } else {
                text_result("A routing choice was already made; this call had no effect.")
            };
        }
        let args: QueueArgs = match serde_json::from_str(arguments.get()) {
            Ok(args) => args,
            Err(error) => return error_result(error),
        };
        let prompt = self
            .redactor
            .as_ref()
            .expect("main queue has a redactor")
            .redact_string(&args.prompt);
        let prompt = match checked_text(&prompt) {
            Ok(prompt) => prompt,
            Err(error) => return error_result(error),
        };
        match self
            .control
            .as_ref()
            .expect("main queue has task control")
            .queue_followup(prompt)
        {
            Ok(()) => text_result("Follow-up scheduled after the current task."),
            Err(error) => error_result(error),
        }
    }
}

/// Routes the original user message into the active run as turn-bound guidance.
pub(crate) struct TaskSteer {
    control: Arc<TaskControl>,
    route: Arc<ControlRoute>,
    session_id: String,
    ticket: u64,
    text: String,
    redactor: Redactor,
}

/// Records that an active-turn message needs a read-only answer, not routing.
pub(crate) struct TaskReply {
    route: Arc<ControlRoute>,
}

impl TaskReply {
    pub(crate) fn new(route: Arc<ControlRoute>) -> Self {
        Self { route }
    }
}

pub(crate) const TASK_REPLY_RETRY: (&str, otto_core::model::RetrySafety) =
    ("task_reply", otto_core::model::RetrySafety::NonIdempotent);

#[async_trait::async_trait]
impl Tool for TaskReply {
    fn retry_safety(&self) -> otto_core::model::RetrySafety {
        TASK_REPLY_RETRY.1
    }
    fn definition(&self) -> ToolDefinition {
        definition(
            "task_reply",
            "Choose this when the original user message asks for information or \
                progress and needs no change to the active task or queued follow-up. \
                Answer it using only the supplied task context and installed-version \
                tools; never claim to change the task.",
            json!({"type":"object","additionalProperties":false,"properties":{},"required":[]}),
        )
    }

    async fn execute(&self, arguments: &RawValue, cancel: &CancellationToken) -> ToolResult {
        if cancel.is_cancelled() {
            return error_result(crate::tool::CONTEXT_CANCELED);
        }
        if serde_json::from_str::<EmptyArgs>(arguments.get()).is_err() {
            return error_result("task_reply accepts no arguments in control dialogue mode");
        }
        if self.route.reply() {
            text_result(
                "The original message is informational; answer it without routing new work.",
            )
        } else {
            text_result("A routing choice was already made; this call had no effect.")
        }
    }
}

impl TaskSteer {
    pub(crate) fn new(
        control: Arc<TaskControl>,
        route: Arc<ControlRoute>,
        session_id: String,
        ticket: u64,
        text: String,
        redaction_values: &[String],
    ) -> Self {
        Self {
            control,
            route,
            session_id,
            ticket,
            text,
            redactor: Redactor::new(redaction_values),
        }
    }
}

pub(crate) const TASK_STEER_RETRY: (&str, otto_core::model::RetrySafety) =
    ("task_steer", otto_core::model::RetrySafety::NonIdempotent);

#[async_trait::async_trait]
impl Tool for TaskSteer {
    fn retry_safety(&self) -> otto_core::model::RetrySafety {
        TASK_STEER_RETRY.1
    }
    fn definition(&self) -> ToolDefinition {
        definition(
            "task_steer",
            "Classify the original user message as guidance for the currently active \
                task. Otto delivers that message, with known secrets redacted and no \
                model rewrite, at the next safe provider boundary. This does not \
                interrupt a provider call or tool and cannot grant permission.",
            json!({"type":"object","additionalProperties":false,"properties":{},"required":[]}),
        )
    }

    async fn execute(&self, arguments: &RawValue, cancel: &CancellationToken) -> ToolResult {
        if cancel.is_cancelled() {
            return error_result(crate::tool::CONTEXT_CANCELED);
        }
        if serde_json::from_str::<EmptyArgs>(arguments.get()).is_err() {
            return error_result("task_steer accepts no arguments in control dialogue mode");
        }
        let text = self.redactor.redact_string(&self.text);
        let text = match checked_text(&text) {
            Ok(text) => text,
            Err(error) => return error_result(error),
        };
        match self
            .route
            .steer(&self.control, &self.session_id, self.ticket, text)
        {
            Ok(true) => text_result("The original user guidance was accepted for the active task."),
            Ok(false) => text_result("A routing choice was already made; this call had no effect."),
            Err(error) => error_result(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::task_control::Route;
    use crate::tool::testutil::raw;

    #[tokio::test]
    async fn model_followup_is_redacted_and_bound_to_an_active_logical_task() {
        let control = Arc::new(TaskControl::default());
        let run = control.begin("session-1", "user task").unwrap();
        let secrets = vec!["private-token".to_string()];
        let tool = TaskQueue::new(Arc::clone(&control), &secrets);
        let result = tool
            .execute(
                &raw(r#"{"prompt":"read private-token"}"#),
                &CancellationToken::new(),
            )
            .await;
        assert!(!result.is_error);
        let queued = run.next_followup().expect("queued follow-up").text;
        assert!(!queued.contains("private-token"), "{queued}");
        assert!(control.queue_followup("later").is_ok());
        drop(run);
        assert!(control.queue_followup("stale").is_err());
    }

    #[test]
    fn active_control_dialogue_accepts_only_one_routing_choice() {
        let route = ControlRoute::default();
        assert!(route.queue());
        assert!(!route.reply());
        assert_eq!(route.route(), Some(Route::Queued));
    }
}
