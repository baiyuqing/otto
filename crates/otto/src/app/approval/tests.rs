
use super::*;

#[tokio::test]
async fn stale_callback_allow_or_deny_cannot_decide_reopened_revision() {
    for stale in [ApprovalDecision::Allow, ApprovalDecision::Deny] {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let controller =
            Arc::new(crate::cli::testutil::controller(workspace.path(), sessions.path()).await);
        let scope = controller.approval_scope();
        let service = controller.approval_service().clone();
        service
            .set_mode(&scope, otto_core::approval::ApprovalMode::Manual)
            .unwrap();
        let request = service
            .create(crate::approval::Proposal {
                kind: otto_core::approval::Kind::Authorization,
                authority: otto_core::approval::Authority::Boundary,
                scope: scope.clone(),
                subject: otto_core::approval::Subject {
                    tool_call_id: "call".into(),
                    source: "mcp".into(),
                    id: "prepared".into(),
                    version: "v1".into(),
                    details: "exact prepared call".into(),
                },
                title: "Prepared effect".into(),
                reason: "User choice".into(),
                persistent: false,
                blocking: true,
                expires_at: None,
            })
            .unwrap();
        let (shown, shown_rx) = tokio::sync::oneshot::channel();
        let shown = Arc::new(std::sync::Mutex::new(Some(shown)));
        let release = CancellationToken::new();
        let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let runner_controller = controller.clone();
        let runner_service = service.clone();
        let runner_request = request.clone();
        let callback_request = request.clone();
        let runner_release = release.clone();
        let runner_executions = executions.clone();
        let job = tokio::spawn(async move {
            let wait_service = runner_service.clone();
            let run = async move {
                let final_request = wait_service
                    .wait(
                        &runner_request.id,
                        &runner_request.scope,
                        &CancellationToken::new(),
                    )
                    .await
                    .map_err(|error| AgentError::Other(error.to_string()))?;
                if final_request.state != otto_core::approval::State::Approved {
                    return Err(AgentError::Other("Wrong rejection".into()));
                }
                runner_executions.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            };
            let (_events, received) = tokio::sync::mpsc::unbounded_channel();
            let mut first = true;
            let mut decide = move |_: ApprovalRequest| {
                let is_first = std::mem::replace(&mut first, false);
                let service = runner_service.clone();
                let scope = scope.clone();
                let request = callback_request.clone();
                let shown = shown.clone();
                let release = runner_release.clone();
                async move {
                    if is_first {
                        service
                            .set_mode(&scope, otto_core::approval::ApprovalMode::Yolo)
                            .unwrap();
                        let automatic = service
                            .get(&request.id, &scope.workspace, Some(&scope.session_id))
                            .unwrap();
                        assert_eq!(automatic.state, otto_core::approval::State::Approved);
                        service
                            .set_mode(&scope, otto_core::approval::ApprovalMode::Manual)
                            .unwrap();
                        assert_eq!(
                            service.revalidate_authorization(&automatic).unwrap().state,
                            otto_core::approval::State::Pending
                        );
                        stale
                    } else {
                        shown.lock().unwrap().take().unwrap().send(()).unwrap();
                        release.cancelled().await;
                        ApprovalDecision::Allow
                    }
                }
            };
            runner_controller
                .step_with_shared_approvals(
                    run,
                    received,
                    &mut |_| {},
                    &CancellationToken::new(),
                    &mut decide,
                )
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), shown_rx)
            .await
            .unwrap()
            .unwrap();
        let current = service
            .get(
                &request.id,
                &request.scope.workspace,
                Some(&request.scope.session_id),
            )
            .unwrap();
        assert_eq!(current.state, otto_core::approval::State::Pending);
        assert!(current.decision.is_none());
        assert_eq!(executions.load(std::sync::atomic::Ordering::SeqCst), 0);
        release.cancel();
        assert!(job.await.unwrap().is_ok());
        assert_eq!(executions.load(std::sync::atomic::Ordering::SeqCst), 1);
        controller.close().unwrap();
    }
}
use crate::{
    app::PROMPT_ACTIVE,
    cli::testutil::initial_runtime,
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

#[tokio::test]
async fn model_followups_wait_for_approval_and_are_discarded_on_denial() {
    use otto_core::session::Session;

    for decision in [ApprovalDecision::Allow, ApprovalDecision::Deny] {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let (controller, _, _) =
            scripted_approval_controller(workspace.path(), sessions.path(), false).await;
        let control = controller.runner().unwrap().task_control();
        let mut discard = |_| {};
        let stop = controller
            .run_with_approvals(
                Step::Text("finish the reviewed task"),
                &mut discard,
                &CancellationToken::new(),
                |_| {
                    control.queue_followup("write the final report").unwrap();
                    async move { decision }
                },
            )
            .await
            .unwrap();
        assert_eq!(stop, Stop::EndTurn);
        let history = controller.current_session().messages();
        let followup = history
            .iter()
            .position(|message| message.context_type == "agent_followup");
        match decision {
            ApprovalDecision::Allow => {
                let retry = history
                    .iter()
                    .position(|message| {
                        message.role == Role::User && message.text().contains("The user approved")
                    })
                    .expect("approval retry");
                assert!(followup.expect("accepted follow-up") > retry);
            }
            ApprovalDecision::Deny => {
                assert!(followup.is_none(), "denied work must not continue")
            }
            ApprovalDecision::Cancelled => unreachable!(),
        }
        assert!(control.queue_followup("late work").is_err());
    }
}

#[tokio::test]
async fn replacing_the_runner_during_approval_cleans_only_the_captured_request() {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let (controller, approvals, _) =
        crate::app::controller_with_approvals(workspace.path(), sessions.path()).await;
    let controller = Arc::new(controller);
    let session_id = controller.info().session_id;
    let old_id = approvals.request(&session_id, "git push");
    let session = controller.current_session();
    let tasks = Arc::new(Tasks::new());
    let runner = Runner::scripted_with_tools(
        session,
        Arc::new(ApprovalProvider {
            calls: AtomicUsize::new(0),
            duplicate: false,
        }),
        Arc::clone(&tasks),
        vec![Box::new(PendingApprovalTool {
            request_id: old_id.clone(),
        })],
    );
    let runtime = initial_runtime(controller.builder());
    let info = controller.builder().runtime_info(&runtime);
    assert!(
        controller
            .replace_runner_if_current(&session_id, runner, info)
            .unwrap()
    );

    let deciding = Arc::clone(&controller);
    let decisions = Arc::clone(&approvals);
    let run_session = session_id.clone();
    let mut discard = |_| {};
    let result = controller
        .run_with_channel(
            Step::Text("run the command"),
            None,
            None,
            &mut discard,
            &CancellationToken::new(),
            move |_| {
                let deciding = Arc::clone(&deciding);
                let decisions = Arc::clone(&decisions);
                let session_id = run_session.clone();
                async move {
                    let session = deciding.current_session();
                    let runtime = initial_runtime(deciding.builder());
                    let candidate = Runner::scripted(
                        session,
                        Arc::new(ApprovalProvider {
                            calls: AtomicUsize::new(0),
                            duplicate: false,
                        }),
                        Arc::new(Tasks::new()),
                    );
                    let info = deciding.builder().runtime_info(&runtime);
                    assert!(
                        deciding
                            .replace_runner_if_current(&session_id, candidate, info)
                            .unwrap()
                    );
                    decisions.request(&session_id, "new command");
                    ApprovalDecision::Allow
                }
            },
        )
        .await;

    assert!(
        matches!(result, Err(AgentError::Other(ref message)) if message.contains("active session changed")),
        "{result:?}"
    );
    assert!(approvals.pending_command(&session_id, &old_id).is_none());
    assert_eq!(
        approvals
            .pending(&session_id)
            .map(|(_, command, _)| command),
        Some("new command".into())
    );
}

#[tokio::test]
async fn duplicate_tool_calls_share_one_pending_request_cleanup_guard() {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let (controller, approvals, request_id) =
        scripted_approval_controller(workspace.path(), sessions.path(), true).await;
    let session_id = controller.info().session_id;

    let decisions = AtomicUsize::new(0);
    let mut discard = |_| {};
    let stop = controller
        .run_with_channel(
            Step::Text("run the command"),
            None,
            None,
            &mut discard,
            &CancellationToken::new(),
            |_| {
                decisions.fetch_add(1, Ordering::SeqCst);
                assert_eq!(
                    approvals
                        .pending_command(&session_id, &request_id)
                        .as_deref(),
                    Some("git push")
                );
                async { ApprovalDecision::Deny }
            },
        )
        .await
        .unwrap();

    assert_eq!(stop, Stop::EndTurn);
    assert_eq!(decisions.load(Ordering::SeqCst), 1);
    assert!(
        approvals
            .pending_command(&session_id, &request_id)
            .is_none()
    );
}

#[tokio::test]
async fn busy_controller_during_approval_discards_the_captured_request() {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let (controller, approvals, request_id) =
        scripted_approval_controller(workspace.path(), sessions.path(), false).await;
    let session_id = controller.info().session_id;
    let controller = Arc::new(controller);
    let (busy_started_tx, busy_started_rx) = oneshot::channel();
    let (release_busy_tx, release_busy_rx) = oneshot::channel();
    let mut busy_started_tx = Some(busy_started_tx);
    let mut busy_started_rx = Some(busy_started_rx);
    let mut release_busy_rx = Some(release_busy_rx);
    let job = Arc::new(Mutex::new(None));
    let deciding = Arc::clone(&controller);
    let job_slot = Arc::clone(&job);
    let mut discard = |_| {};
    let result = controller
        .run_with_channel(
            Step::Text("run the command"),
            None,
            None,
            &mut discard,
            &CancellationToken::new(),
            move |_| {
                let deciding = Arc::clone(&deciding);
                let job_slot = Arc::clone(&job_slot);
                let busy_started_tx = busy_started_tx.take().unwrap();
                let busy_started_rx = busy_started_rx.take().unwrap();
                let release_busy_rx = release_busy_rx.take().unwrap();
                async move {
                    let busy = Arc::clone(&deciding);
                    let handle = tokio::spawn(async move {
                        let _admission = busy.begin_operation().unwrap();
                        busy_started_tx.send(()).unwrap();
                        release_busy_rx.await.unwrap();
                    });
                    *job_slot.lock().unwrap() = Some(handle);
                    busy_started_rx.await.unwrap();
                    ApprovalDecision::Allow
                }
            },
        )
        .await;

    assert!(matches!(result, Err(AgentError::Other(message)) if message == PROMPT_ACTIVE));
    assert!(
        approvals
            .pending_command(&session_id, &request_id)
            .is_none()
    );
    release_busy_tx.send(()).unwrap();
    let handle = job.lock().unwrap().take().unwrap();
    handle.await.unwrap();
}

#[tokio::test]
async fn controller_close_during_approval_discards_the_captured_request() {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let (controller, approvals, request_id) =
        scripted_approval_controller(workspace.path(), sessions.path(), false).await;
    let session_id = controller.info().session_id;
    let controller = Arc::new(controller);
    let deciding = Arc::clone(&controller);
    let mut discard = |_| {};
    let result = controller
        .run_with_channel(
            Step::Text("run the command"),
            None,
            None,
            &mut discard,
            &CancellationToken::new(),
            move |_| {
                let deciding = Arc::clone(&deciding);
                async move {
                    deciding.request_close();
                    ApprovalDecision::Allow
                }
            },
        )
        .await;

    assert!(matches!(result, Err(AgentError::Other(message)) if message == crate::app::CLOSED));
    assert!(
        approvals
            .pending_command(&session_id, &request_id)
            .is_none()
    );
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
    let tasks = Arc::new(Tasks::new());
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

#[tokio::test]
async fn a_wake_turn_queues_its_text_with_a_blank_line_after_a_tool_call() {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let controller = wake_controller(
        workspace.path(),
        sessions.path(),
        vec![("checking  ", true), ("all done", false)],
    )
    .await;
    let mut seen = Vec::new();
    let wake = controller.prepare_wake().unwrap().expect("wake claimed");
    controller
        .run_with_channel(
            Step::Wake(wake),
            None,
            None,
            &mut |event| seen.push(event),
            &CancellationToken::new(),
            |_| async { ApprovalDecision::Deny },
        )
        .await
        .unwrap();
    assert!(
        seen.iter()
            .any(|event| matches!(event, Event::TextDelta { text } if text == "all done")),
        "events still reach the caller's sink"
    );
    assert_eq!(
        controller.wake_replies_since(0),
        (vec![(1, "checking\n\nall done".to_string())], 1)
    );
}

#[tokio::test]
async fn a_wake_turn_without_text_queues_nothing() {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let controller = wake_controller(workspace.path(), sessions.path(), vec![("  ", false)]).await;
    assert_eq!(
        run_wake(&controller, &CancellationToken::new()).await,
        Stop::EndTurn
    );
    assert_eq!(controller.wake_replies_since(0), (Vec::new(), 0));
}

#[tokio::test]
async fn a_cancelled_wake_turn_queues_nothing() {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let controller =
        wake_controller(workspace.path(), sessions.path(), vec![("partial", false)]).await;
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert_eq!(run_wake(&controller, &cancel).await, Stop::Cancelled);
    assert_eq!(controller.wake_replies_since(0), (Vec::new(), 0));
}

#[tokio::test]
async fn a_prompt_turn_queues_no_wake_reply() {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let controller = wake_controller(
        workspace.path(),
        sessions.path(),
        vec![("an answer", false)],
    )
    .await;
    controller
        .run_with_approvals(
            Step::Text("hello"),
            &mut |_| {},
            &CancellationToken::new(),
            |_| async { ApprovalDecision::Deny },
        )
        .await
        .unwrap();
    assert_eq!(controller.wake_replies_since(0), (Vec::new(), 0));
}

/// Queues one more inbox notification so another wake turn can be claimed.
fn queue_wake(controller: &Controller) {
    controller
        .runner()
        .unwrap()
        .tasks
        .as_ref()
        .expect("task registry")
        .notifications()
        .push(otto_core::agent::inbox::Notification {
            text: "[timer] ping".to_string(),
            ..otto_core::agent::inbox::Notification::default()
        });
}

fn origin_of(controller: &Controller) -> Option<crate::schedule::Origin> {
    controller.runner().unwrap().schedule_origin.get()
}

async fn turn(controller: &Controller, text: &str, origin: Option<crate::schedule::Origin>) {
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
    origin: crate::schedule::Origin,
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

#[tokio::test]
async fn user_turns_set_the_origin_and_wake_turns_keep_it() {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let controller = wake_controller(
        workspace.path(),
        sessions.path(),
        vec![("a", false), ("b", false), ("c", false), ("d", false)],
    )
    .await;
    assert_eq!(origin_of(&controller), None);
    controller
        .prompt("hi", &mut |_| {}, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        origin_of(&controller),
        Some(crate::schedule::Origin::terminal())
    );
    let chat = crate::schedule::Origin::chat("telegram", "42").unwrap();
    turn(&controller, "hi", Some(chat.clone())).await;
    assert_eq!(origin_of(&controller), Some(chat.clone()));
    queue_wake(&controller);
    run_wake(&controller, &CancellationToken::new()).await;
    assert_eq!(origin_of(&controller), Some(chat));
    turn(&controller, "hi", None).await;
    assert_eq!(origin_of(&controller), None);
}

#[tokio::test]
async fn the_next_user_turn_appends_a_delivery_once_before_the_user_message() {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let store = Arc::new(crate::schedule::Store::open_in_memory().unwrap());
    let controller = schedule_controller(
        workspace.path(),
        sessions.path(),
        vec![("a", false), ("b", false), ("c", false)],
        Some(store.clone()),
    )
    .await;
    let session_id = controller.info().session_id;
    let terminal = crate::schedule::Origin::terminal();
    let row = delivered_row(&store, terminal.clone(), &session_id, "drink water");

    // A wake turn appends nothing.
    run_wake(&controller, &CancellationToken::new()).await;
    assert!(scheduled_messages(&controller).is_empty());

    turn(&controller, "hello", Some(terminal.clone())).await;
    let history = controller.current_session().messages();
    let appended = history
        .iter()
        .position(|message| message.context_type == "scheduled_delivery")
        .expect("delivery appended");
    assert_eq!(history[appended].role, Role::Context);
    assert_eq!(history[appended].text(), row.context_text());
    let user = history
        .iter()
        .position(|message| message.role == Role::User && message.text() == "hello")
        .expect("user message");
    assert!(appended < user);
    assert!(
        store
            .pending_writebacks(&terminal, &session_id, 10)
            .unwrap()
            .is_empty()
    );

    turn(&controller, "again", Some(terminal)).await;
    assert_eq!(scheduled_messages(&controller).len(), 1);
}

#[tokio::test]
async fn a_chat_turn_appends_deliveries_created_by_another_session() {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let store = Arc::new(crate::schedule::Store::open_in_memory().unwrap());
    let controller = schedule_controller(
        workspace.path(),
        sessions.path(),
        vec![("a", false), ("b", false)],
        Some(store.clone()),
    )
    .await;
    let chat = crate::schedule::Origin::chat("telegram", "42").unwrap();
    let other = crate::schedule::Origin::chat("telegram", "7").unwrap();
    delivered_row(&store, chat.clone(), "old-session", "for chat 42");
    delivered_row(&store, other, "old-session", "for chat 7");

    turn(&controller, "hello", Some(chat)).await;
    let messages = scheduled_messages(&controller);
    assert_eq!(messages.len(), 1);
    assert!(messages[0].text().ends_with("for chat 42"));

    // A turn with no origin appends nothing.
    turn(&controller, "again", None).await;
    assert_eq!(scheduled_messages(&controller).len(), 1);
}

async fn scheduled_run(script: Vec<(&'static str, bool)>) -> crate::schedule::RunOutcome {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let controller = wake_controller(workspace.path(), sessions.path(), script).await;
    crate::app::scheduled::run(&controller, "check it", &CancellationToken::new()).await
}

#[tokio::test]
async fn a_scheduled_run_maps_the_turn_end_to_an_outcome() {
    use crate::schedule::RunOutcome;
    assert_eq!(
        scheduled_run(vec![("checking ", true), ("all fine ", false)]).await,
        RunOutcome::Reply("checking\n\nall fine".into())
    );
    assert!(matches!(
        scheduled_run(Vec::new()).await,
        RunOutcome::FailedBeforeTools(message) if message.contains("script exhausted")
    ));
    assert!(matches!(
        scheduled_run(vec![("checking", true)]).await,
        RunOutcome::Failed(message) if message.contains("script exhausted")
    ));
}
