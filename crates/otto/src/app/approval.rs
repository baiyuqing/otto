//! The Bash elevation and persistent-read approval loop `otto acp` and `otto serve` share.
//!
//! One call runs a first step (a prompt or a wake turn). When the step ends
//! with a Bash permission request, the loop asks the caller's decision
//! callback. `Allow` applies the grant and runs the retry
//! prompt; `Deny` ends the turn normally; `Cancelled` ends it as a cancelled
//! prompt. The controller is idle while the callback waits, so other
//! operations are not blocked by it; a frontend that must keep the session
//! busy holds its own admission rule.

use std::future::Future;
use std::sync::Arc;

use otto_core::agent::{AgentError, Event, EventSink};
use otto_core::model::Block;
use tokio_util::sync::CancellationToken;

use super::{Controller, WakeOperation};

/// The first step of a turn.
pub enum Step<'a> {
    Text(&'a str),
    Image(&'a str, Block),
    /// An agent-originated queued task, stored as display-only context.
    Followup(&'a str),
    Wake(WakeOperation<'a>),
}

/// The command and optional persistent read path a turn waits on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRequest {
    pub approval_id: String,
    pub tool_call_id: String,
    pub command: String,
    pub justification: String,
    pub read_path: String,
}

/// What the decision callback returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalDecision {
    Allow,
    /// Any answer other than allow, including a timeout.
    Deny,
    /// The turn's cancellation token fired during the wait.
    Cancelled,
}

/// How a turn that did not fail ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    EndTurn,
    Cancelled,
}

struct PendingApprovalCleanup {
    approvals: Arc<crate::tool::bash::BashApprovals>,
    session_id: String,
    request_id: String,
    armed: bool,
}

impl PendingApprovalCleanup {
    fn new(
        approvals: Arc<crate::tool::bash::BashApprovals>,
        session_id: &str,
        request_id: &str,
    ) -> Self {
        Self {
            approvals,
            session_id: session_id.to_string(),
            request_id: request_id.to_string(),
            armed: true,
        }
    }

    fn discard(&mut self) {
        if self.armed {
            self.approvals.discard(&self.session_id, &self.request_id);
            self.armed = false;
        }
    }
}

impl Drop for PendingApprovalCleanup {
    fn drop(&mut self) {
        self.discard();
    }
}

impl Controller {
    /// Runs `first` and every approval retry to its end. `emit` receives every
    /// event of every step. An error is the failing step's error, or the
    /// (already redacted) text `approve_bash` returned.
    pub async fn run_with_approvals<F, Fut>(
        &self,
        first: Step<'_>,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
        decide: F,
    ) -> Result<Stop, AgentError>
    where
        F: FnMut(ApprovalRequest) -> Fut,
        Fut: Future<Output = ApprovalDecision>,
    {
        self.run_with_channel(first, None, emit, cancel, decide)
            .await
    }

    /// Runs one turn with trusted, transient channel context and every
    /// approval retry. The context is passed to each provider request and is
    /// never appended to the session.
    pub async fn run_with_channel<F, Fut>(
        &self,
        first: Step<'_>,
        channel: Option<super::Channel>,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
        mut decide: F,
    ) -> Result<Stop, AgentError>
    where
        F: FnMut(ApprovalRequest) -> Fut,
        Fut: Future<Output = ApprovalDecision>,
    {
        let system_context = channel.map_or("", super::Channel::system_context);
        let first_admission = if matches!(&first, Step::Wake(_)) {
            None
        } else {
            Some(self.begin_operation().map_err(AgentError::Other)?)
        };
        let runner = self.runner().map_err(AgentError::Other)?;
        let session_id = runner.session().header().id.clone();
        let original_task = match &first {
            Step::Text(text) | Step::Followup(text) => runner.redact_text(text),
            Step::Image(text, _) => runner.redact_text(text),
            Step::Wake(_) => String::new(),
        };
        let task_run = runner
            .task_control()
            .begin(&session_id, &original_task)
            .map_err(AgentError::Other)?;
        let stop = self
            .run_approved_step(
                first,
                &runner,
                system_context,
                &task_run,
                first_admission,
                &mut *emit,
                cancel,
                &mut decide,
            )
            .await?;
        if stop == Stop::Cancelled {
            return Ok(stop);
        }
        if self.pending_approval().is_some() {
            task_run.clear_followups();
            return Ok(stop);
        }
        while let Some(text) = task_run.next_followup() {
            if self.pending_approval().is_some() {
                task_run.clear_followups();
                return Ok(stop);
            }
            let followup_stop = self
                .run_approved_step(
                    Step::Followup(&text),
                    &runner,
                    system_context,
                    &task_run,
                    None,
                    &mut *emit,
                    cancel,
                    &mut decide,
                )
                .await?;
            if followup_stop == Stop::Cancelled {
                return Ok(followup_stop);
            }
        }
        Ok(stop)
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_approved_step<F, Fut>(
        &self,
        first: Step<'_>,
        runner: &crate::cli::runtime_builder::Runner,
        system_context: &str,
        task_run: &super::task_control::Run,
        first_admission: Option<super::Admission<'_>>,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
        decide: &mut F,
    ) -> Result<Stop, AgentError>
    where
        F: FnMut(ApprovalRequest) -> Fut,
        Fut: Future<Output = ApprovalDecision>,
    {
        let mut first_admission = first_admission;
        let mut step = Some(first);
        let mut retry = String::new();
        let followup = matches!(step, Some(Step::Followup(_)));
        let session_id = runner.session().header().id.clone();
        let approval_store = self.builder().bash_approvals.clone();
        let mut pending_cleanup = Vec::<PendingApprovalCleanup>::new();
        loop {
            let mut found = None;
            let mut observed_cleanup = Vec::<PendingApprovalCleanup>::new();
            let outcome = {
                let mut sink = |event: Event| {
                    if let Event::ToolCallFinished {
                        tool_name,
                        tool_call_id,
                        result,
                        ..
                    } = &event
                        && let Some(request) =
                            crate::tool::bash::parse_approval_request(tool_name, result)
                    {
                        if let Some(approvals) = &approval_store
                            && !observed_cleanup
                                .iter()
                                .any(|cleanup| cleanup.request_id == request.id)
                        {
                            observed_cleanup.push(PendingApprovalCleanup::new(
                                Arc::clone(approvals),
                                &session_id,
                                &request.id,
                            ));
                        }
                        found = Some((tool_call_id.clone(), request));
                    }
                    emit(event);
                };
                match step.take() {
                    Some(Step::Text(text)) => {
                        let _admission = match first_admission.take() {
                            Some(admission) => admission,
                            None => self
                                .begin_runner_operation(runner)
                                .map_err(AgentError::Other)?,
                        };
                        self.run_prompt_step(runner, text, system_context, &mut sink, cancel)
                            .await
                    }
                    Some(Step::Image(text, image)) => {
                        let _admission = match first_admission.take() {
                            Some(admission) => admission,
                            None => self
                                .begin_runner_operation(runner)
                                .map_err(AgentError::Other)?,
                        };
                        let observation = super::auto_reflection::RunObservation::default();
                        let result = runner
                            .run_with_image_system_context(
                                text,
                                image,
                                system_context,
                                &mut super::auto_reflection::observing_prompt(
                                    &mut sink,
                                    &observation,
                                ),
                                cancel,
                            )
                            .await;
                        self.after_prompt(&observation, result.is_ok() && !cancel.is_cancelled());
                        result
                    }
                    Some(Step::Followup(text)) => {
                        let _admission = match first_admission.take() {
                            Some(admission) => admission,
                            None => self
                                .begin_runner_operation(runner)
                                .map_err(AgentError::Other)?,
                        };
                        task_run.set_current_task(text);
                        self.run_followup_step(runner, text, system_context, &mut sink, cancel)
                            .await
                    }
                    Some(Step::Wake(wake)) => wake.run(&mut sink, cancel).await,
                    None => {
                        let _admission = match first_admission.take() {
                            Some(admission) => admission,
                            None => self
                                .begin_runner_operation(runner)
                                .map_err(AgentError::Other)?,
                        };
                        if followup {
                            self.run_followup_step(
                                runner,
                                &retry,
                                system_context,
                                &mut sink,
                                cancel,
                            )
                            .await
                        } else {
                            self.run_prompt_step(runner, &retry, system_context, &mut sink, cancel)
                                .await
                        }
                    }
                }
            };
            if cancel.is_cancelled() {
                return Ok(Stop::Cancelled);
            }
            outcome?;
            let Some((tool_call_id, request)) = found else {
                return Ok(Stop::EndTurn);
            };
            let approvals = approval_store
                .as_ref()
                .cloned()
                .ok_or_else(|| AgentError::Other("temporary elevation is unavailable".into()))?;
            let pending = approvals.pending_command(&session_id, &request.id);
            let Some(command) = pending else {
                return Ok(Stop::EndTurn);
            };
            let mut cleanup = observed_cleanup
                .into_iter()
                .find(|cleanup| cleanup.request_id == request.id)
                .expect("observed approval owns its cleanup guard");
            let decision = decide(ApprovalRequest {
                approval_id: request.id.clone(),
                tool_call_id,
                command,
                justification: request.justification,
                read_path: self
                    .bash_approvals()
                    .ok()
                    .and_then(|approvals| approvals.pending_read_path(&session_id, &request.id))
                    .unwrap_or_default(),
            })
            .await;
            if cancel.is_cancelled() {
                cleanup.discard();
                return Ok(Stop::Cancelled);
            }
            let decision_admission = match self.begin_runner_operation(runner) {
                Ok(admission) => admission,
                Err(failure) => {
                    cleanup.discard();
                    return Err(AgentError::Other(failure));
                }
            };
            match decision {
                ApprovalDecision::Cancelled => {
                    cleanup.discard();
                    return Ok(Stop::Cancelled);
                }
                ApprovalDecision::Deny => {
                    cleanup.discard();
                    task_run.clear_followups();
                    return Ok(Stop::EndTurn);
                }
                ApprovalDecision::Allow => match self
                    .approve_tool_for_session(&session_id, &request.id)
                    .await
                {
                    Ok(prompt) => {
                        retry = prompt;
                        if pending_cleanup
                            .iter()
                            .any(|pending| pending.request_id == cleanup.request_id)
                        {
                            cleanup.armed = false;
                        }
                        pending_cleanup.push(cleanup);
                        first_admission = Some(decision_admission);
                    }
                    Err(message) => {
                        cleanup.discard();
                        return Err(AgentError::Other(message));
                    }
                },
            }
        }
    }

    /// `message` with provider secrets removed, for the current runtime.
    pub fn redact_error(&self, message: &str) -> String {
        let runtime = self.current_runtime().ok();
        self.builder.redact_error(message, runtime.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        app::PROMPT_ACTIVE,
        cli::{runtime_builder::Runner, testutil::initial_runtime},
        subagent::tasks::Tasks,
        tool::{Tool, definition},
    };
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
                                r#"{"command":"git push","sandbox_permissions":"require_escalated"}"#.into(),
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
            ProviderSettlement::succeeded(Response { message }, 1)
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
}
