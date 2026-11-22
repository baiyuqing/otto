use super::*;

use crate::{
    app::PROMPT_ACTIVE,
    fixtures::initial_runtime,
    runtime::builder::Runner,
    subagent::tasks::Tasks,
    tool::{Tool, definition},
};
use otto_core::session::Session as _;
use otto_core::{
    model::{Block, BlockType, FinishReason, Message, Role},
    operation::OperationControl,
    provider::{Provider, ProviderSettlement, Request, Response, StreamSink},
    tool::ToolResult,
};
use serde_json::{json, value::RawValue};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::oneshot;

struct ApprovalProvider {
    calls: AtomicUsize,
    duplicate: bool,
}

#[async_trait::async_trait]
impl Provider for ApprovalProvider {
    async fn complete(
        &self,
        _: &Request,
        _: StreamSink<'_>,
        _: &dyn OperationControl,
    ) -> ProviderSettlement {
        let message = if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            let mut blocks = vec![Block {
                block_type: BlockType::ToolCall,
                tool_call_id: "call-1".into(),
                tool_name: "bash".into(),
                arguments: Some(
                    RawValue::from_string(
                        r#"{"command":"git push","sandbox_permissions":"require_escalated"}"#
                            .into(),
                    )
                    .unwrap(),
                ),
                ..Block::default()
            }];
            if self.duplicate {
                blocks.push(Block {
                    block_type: BlockType::ToolCall,
                    tool_call_id: "call-2".into(),
                    tool_name: "bash".into(),
                    arguments: Some(
                        RawValue::from_string(
                            r#"{"command":"git push","sandbox_permissions":"require_escalated"}"#
                                .into(),
                        )
                        .unwrap(),
                    ),
                    ..Block::default()
                });
            }
            Message {
                role: Role::Assistant,
                blocks,
                finish_reason: Some(FinishReason::ToolCalls),
                ..Message::default()
            }
        } else {
            Message {
                role: Role::Assistant,
                blocks: vec![Block::text("approval needed")],
                finish_reason: Some(FinishReason::Stop),
                ..Message::default()
            }
        };
        ProviderSettlement::succeeded(
            Response {
                message,
                ..Response::default()
            },
            1,
        )
    }
}

struct PendingApprovalTool {
    request_id: String,
}

#[async_trait::async_trait]
impl Tool for PendingApprovalTool {
    fn definition(&self) -> otto_core::model::ToolDefinition {
        definition(
            "bash",
            "fixture",
            json!({"type":"object","additionalProperties":false,"properties":{"command":{"type":"string"}},"required":["command"]}),
        )
    }

    async fn execute(&self, _: &RawValue, _: &CancellationToken) -> ToolResult {
        ToolResult::error(format!(
            "approval required\nApprove in Otto: /approve {}\nCommand: \"git push\"\nJustification: \"push branch\"",
            self.request_id
        ))
    }
}

async fn scripted_approval_controller(
    workspace: &std::path::Path,
    sessions: &std::path::Path,
    duplicate: bool,
) -> (Controller, Arc<crate::tool::bash::BashApprovals>, String) {
    let (controller, approvals, _) =
        crate::app::controller_with_approvals(workspace, sessions).await;
    let session_id = controller.info().session_id;
    let request_id = approvals.request(&session_id, "git push");
    let runner = Runner::scripted_with_tools(
        controller.current_session(),
        Arc::new(ApprovalProvider {
            calls: AtomicUsize::new(0),
            duplicate,
        }),
        Arc::new(Tasks::new()),
        vec![Box::new(PendingApprovalTool {
            request_id: request_id.clone(),
        })],
    );
    let runtime = initial_runtime(controller.builder());
    let info = controller.builder().runtime_info(&runtime);
    assert!(
        controller
            .replace_runner_if_current(&session_id, runner, info)
            .unwrap()
    );
    (controller, approvals, request_id)
}

/// One scripted provider call: streamed text and whether the reply ends
/// with a tool call.
struct WakeProvider {
    script: Mutex<std::collections::VecDeque<(&'static str, bool)>>,
}

#[async_trait::async_trait]
impl Provider for WakeProvider {
    async fn complete(
        &self,
        _: &Request,
        sink: StreamSink<'_>,
        _: &dyn OperationControl,
    ) -> ProviderSettlement {
        let Some((text, call)) = self.script.lock().unwrap().pop_front() else {
            return ProviderSettlement::failed(
                otto_core::provider::ProviderError::Other("script exhausted".into()),
                1,
                otto_core::model::EffectCertainty::NotStarted,
            );
        };
        let mut blocks = Vec::new();
        if !text.is_empty() {
            sink(otto_core::provider::StreamEvent::TextDelta { text: text.into() });
            blocks.push(Block::text(text));
        }
        if call {
            blocks.push(Block {
                block_type: BlockType::ToolCall,
                tool_call_id: "call-1".into(),
                tool_name: "echo".into(),
                arguments: Some(RawValue::from_string("{}".into()).unwrap()),
                ..Block::default()
            });
        }
        let finish_reason = if call {
            FinishReason::ToolCalls
        } else {
            FinishReason::Stop
        };
        let message = Message {
            role: Role::Assistant,
            blocks,
            finish_reason: Some(finish_reason),
            ..Message::default()
        };
        ProviderSettlement::succeeded(
            Response {
                message,
                ..Response::default()
            },
            1,
        )
    }
}

struct EchoTool;

#[async_trait::async_trait]
impl Tool for EchoTool {
    fn definition(&self) -> otto_core::model::ToolDefinition {
        definition("echo", "fixture", json!({"type":"object"}))
    }

    async fn execute(&self, _: &RawValue, _: &CancellationToken) -> ToolResult {
        crate::tool::text_result("ok")
    }
}

/// A controller whose provider plays `script` and whose inbox holds one
/// notification, so a wake turn can be claimed.
async fn wake_controller(
    workspace: &std::path::Path,
    sessions: &std::path::Path,
    script: Vec<(&'static str, bool)>,
) -> Controller {
    schedule_controller(workspace, sessions, script, None).await
}

async fn schedule_controller(
    workspace: &std::path::Path,
    sessions: &std::path::Path,
    script: Vec<(&'static str, bool)>,
    store: Option<Arc<dyn crate::schedule::ScheduleStore>>,
) -> Controller {
    let (controller, _, _) =
        crate::app::controller_with_approvals_and_schedule(workspace, sessions, store).await;
    // The session owns its registry and inbox; the replacement runner uses them.
    let tasks = controller.subagent_tasks().expect("session registry");
    tasks
        .notifications()
        .push(otto_core::agent::inbox::Notification {
            text: "[timer] ping".to_string(),
            ..otto_core::agent::inbox::Notification::default()
        });
    let runner = Runner::scripted_with_tools(
        controller.current_session(),
        Arc::new(WakeProvider {
            script: Mutex::new(script.into()),
        }),
        tasks,
        vec![Box::new(EchoTool)],
    );
    let runtime = initial_runtime(controller.builder());
    let info = controller.builder().runtime_info(&runtime);
    let session_id = controller.info().session_id;
    assert!(
        controller
            .replace_runner_if_current(&session_id, runner, info)
            .unwrap()
    );
    controller
}

async fn run_wake(controller: &Controller, cancel: &CancellationToken) -> Stop {
    let wake = controller.prepare_wake().unwrap().expect("wake claimed");
    controller
        .run_with_channel(
            Step::Wake(wake),
            None,
            None,
            &mut |_| {},
            cancel,
            |_| async { ApprovalDecision::Deny },
        )
        .await
        .unwrap()
}

/// Queues one more inbox notification so another wake turn can be claimed.
fn queue_wake(controller: &Controller) {
    controller
        .runner()
        .unwrap()
        .tasks()
        .expect("task registry")
        .notifications()
        .push(otto_core::agent::inbox::Notification {
            text: "[timer] ping".to_string(),
            ..otto_core::agent::inbox::Notification::default()
        });
}

fn origin_of(controller: &Controller) -> Option<otto_session::origin::Origin> {
    controller.runner().unwrap().schedule_origin.get()
}

async fn turn(controller: &Controller, text: &str, origin: Option<otto_session::origin::Origin>) {
    controller
        .run_with_channel(
            Step::Text(text),
            None,
            origin,
            &mut |_| {},
            &CancellationToken::new(),
            |_| async { ApprovalDecision::Deny },
        )
        .await
        .unwrap();
}

fn scheduled_messages(controller: &Controller) -> Vec<Message> {
    controller
        .current_session()
        .messages()
        .into_iter()
        .filter(|message| message.context_type == "scheduled_delivery")
        .collect()
}

/// A delivered, not yet appended row for `origin`, created by `session`.
fn delivered_row(
    store: &crate::schedule::Store,
    origin: otto_session::origin::Origin,
    session: &str,
    text: &str,
) -> crate::schedule::Delivery {
    use crate::schedule::Kind;
    let now = chrono::Utc::now();
    let item = store
        .create(
            crate::schedule::NewItem {
                origin: origin.clone(),
                session_id: session.into(),
                workspace: "/work".into(),
                kind: Kind::Text,
                body: text.into(),
                repeat: None,
                local_time: None,
                due_at: now,
            },
            now,
        )
        .unwrap();
    store
        .claim_deliveries(
            &crate::schedule::Scope::Any,
            std::time::Duration::from_secs(60),
            10,
            now,
        )
        .unwrap();
    assert!(store.delivered(item.id, item.due_at, now).unwrap());
    store
        .pending_writebacks(&origin, session, 10)
        .unwrap()
        .pop()
        .or_else(|| {
            store
                .pending_writebacks(&origin, "other-session", 10)
                .unwrap()
                .pop()
        })
        .expect("pending row")
}

async fn scheduled_run(script: Vec<(&'static str, bool)>) -> crate::schedule::RunOutcome {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let controller = wake_controller(workspace.path(), sessions.path(), script).await;
    crate::app::scheduled::run(&controller, "check it", &CancellationToken::new()).await
}

mod captured_requests;
mod scheduled_delivery;
mod turn_budget;
mod turn_duration;
mod wake_replies;
