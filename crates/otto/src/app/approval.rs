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
#[allow(clippy::large_enum_variant)] // one short-lived value per turn
pub enum Step<'a> {
    Text(&'a str),
    Image(&'a str, Block),
    /// An agent-originated queued task, stored as display-only context.
    Followup(&'a str),
    /// A stored schedule prompt, generated from a previously authorized item.
    /// Its User wire role is never itself evidence of a human instruction.
    Scheduled(&'a str),
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
    /// (already redacted) text `approve_tool` returned.
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
        self.run_with_channel(first, None, None, emit, cancel, decide)
            .await
    }

    /// Runs one turn with trusted, transient channel context and every
    /// approval retry. The context is passed to each provider request and is
    /// never appended to the session.
    ///
    /// A wake turn also queues its reply text for [`Controller::wake_replies_since`]
    /// when it ends without error and was not cancelled.
    pub async fn run_with_channel<F, Fut>(
        &self,
        first: Step<'_>,
        channel: Option<super::Channel>,
        origin: Option<crate::schedule::Origin>,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
        decide: F,
    ) -> Result<Stop, AgentError>
    where
        F: FnMut(ApprovalRequest) -> Fut,
        Fut: Future<Output = ApprovalDecision>,
    {
        if !matches!(first, Step::Wake(_)) {
            return self
                .run_turn_steps(first, channel, origin, emit, cancel, decide)
                .await;
        }
        let mut collector = super::scheduled::ReplyCollector::default();
        let result = self
            .run_turn_steps(
                first,
                channel,
                origin,
                &mut |event: Event| {
                    collector.observe(&event);
                    emit(event);
                },
                cancel,
                decide,
            )
            .await;
        if matches!(result, Ok(Stop::EndTurn))
            && let Ok(runner) = self.runner()
        {
            let text = runner.redact_text(collector.reply().trim());
            if !text.is_empty() {
                self.wake_replies.push(text);
            }
        }
        result
    }

    async fn run_turn_steps<F, Fut>(
        &self,
        first: Step<'_>,
        channel: Option<super::Channel>,
        origin: Option<crate::schedule::Origin>,
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
        let _output_lease = runner.retain_temporary_outputs();
        if let Step::Text(text) | Step::Image(text, _) = &first {
            runner.record_user_authorization(text).await?;
        }
        let _presentation = if matches!(first, Step::Text(_) | Step::Image(..)) {
            Some(self.begin_user_turn(&runner, origin).await)
        } else {
            if matches!(first, Step::Wake(_)) {
                runner.presentation_origin.set(None);
            }
            None
        };
        let session_id = runner.session().header().id.clone();
        let original_task = match &first {
            Step::Text(text) | Step::Followup(text) | Step::Scheduled(text) => {
                runner.redact_text(text)
            }
            Step::Image(text, _) => runner.redact_text(text),
            Step::Wake(_) => String::new(),
        };
        let resumes = matches!(first, Step::Wake(_)) && self.resumable_interrupted_turn();
        let task_run = runner
            .task_control()
            .begin_resuming(&session_id, &original_task, resumes)
            .map_err(AgentError::Other)?;
        self.record_unrun_followups(&runner, task_run.discarded(), &mut *emit)
            .await?;
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
        while let Some(followup) = task_run.next_followup() {
            if self.pending_approval().is_some() {
                task_run.clear_followups();
                return Ok(stop);
            }
            if otto_core::session::Session::has_input(runner.session(), &followup.input_id) {
                // Its step ran before an interruption kept it journaled.
                task_run.followup_done(&followup);
                continue;
            }
            runner.set_next_input_id(Some(followup.input_id.clone()));
            let followup_stop = self
                .run_approved_step(
                    Step::Followup(&followup.text),
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
            task_run.followup_done(&followup);
        }
        Ok(stop)
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_approved_step<F, Fut>(
        &self,
        first: Step<'_>,
        runner: &crate::runtime::builder::Runner,
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
                let (events, received) = tokio::sync::mpsc::unbounded_channel();
                let mut send = |event| {
                    let _ = events.send(event);
                };
                let run = async {
                    match step.take() {
                        Some(Step::Text(text) | Step::Scheduled(text)) => {
                            let _admission = match first_admission.take() {
                                Some(admission) => admission,
                                None => self
                                    .begin_runner_operation(runner)
                                    .map_err(AgentError::Other)?,
                            };
                            self.run_prompt_step(runner, text, system_context, &mut send, cancel)
                                .await
                        }
                        Some(Step::Image(text, image)) => {
                            let _admission = match first_admission.take() {
                                Some(admission) => admission,
                                None => self
                                    .begin_runner_operation(runner)
                                    .map_err(AgentError::Other)?,
                            };
                            runner
                                .run_with_image_system_context(
                                    text,
                                    image,
                                    system_context,
                                    &mut send,
                                    cancel,
                                )
                                .await
                        }
                        Some(Step::Followup(text)) => {
                            let _admission = match first_admission.take() {
                                Some(admission) => admission,
                                None => self
                                    .begin_runner_operation(runner)
                                    .map_err(AgentError::Other)?,
                            };
                            task_run.set_current_task(text);
                            self.run_followup_step(runner, text, system_context, &mut send, cancel)
                                .await
                        }
                        Some(Step::Wake(wake)) => wake.run(&mut send, cancel).await,
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
                                    &mut send,
                                    cancel,
                                )
                                .await
                            } else {
                                self.run_prompt_step(
                                    runner,
                                    &retry,
                                    system_context,
                                    &mut send,
                                    cancel,
                                )
                                .await
                            }
                        }
                    }
                };
                // Boxed: unboxed, a debug build copies the step future into the
                // poll frame of every caller, and the `embed` tests that run a
                // turn needed up to 2.2 MB of stack (now 1.03 MB), more than the
                // 2 MiB of a test thread or Tokio worker thread.
                self.step_with_shared_approvals(Box::pin(run), received, &mut sink, cancel, decide)
                    .await
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

    async fn step_with_shared_approvals<F, Fut>(
        &self,
        run: impl Future<Output = Result<(), AgentError>>,
        mut received: tokio::sync::mpsc::UnboundedReceiver<Event>,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
        decide: &mut F,
    ) -> Result<(), AgentError>
    where
        F: FnMut(ApprovalRequest) -> Fut,
        Fut: Future<Output = ApprovalDecision>,
    {
        tokio::pin!(run);
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(100));
        let mut presented = std::collections::HashSet::new();
        let scope = self.approval_scope();
        loop {
            tokio::select! {
                result = &mut run => {
                    while let Ok(event) = received.try_recv() { emit(event); }
                    return result;
                },
                Some(event) = received.recv() => emit(event),
                _ = tick.tick() => {
                    let pending = self.approval_service().snapshot(&scope.workspace,Some(&scope.session_id))
                        .map_err(|e| AgentError::Other(e.to_string()))?;
                    if let Some(request) = pending.requests.into_iter().find(|r| r.blocking && presented.insert((r.id.clone(), r.revision, r.subject.version.clone()))) {
                        let decision = decide(ApprovalRequest {
                            approval_id: request.id.clone(),tool_call_id: request.subject.tool_call_id.clone(),
                            command:serde_json::from_str::<serde_json::Value>(&request.subject.details)
                                .ok()
                                .and_then(|input|input["command"].as_str().map(str::to_string))
                                .unwrap_or_else(||format!("{}\n{}",request.title,request.subject.details)),
                            justification:request.reason.clone(),
                            read_path:serde_json::from_str::<serde_json::Value>(&request.subject.details)
                                .ok()
                                .and_then(|v|v["read_path"].as_str().map(str::to_string))
                                .unwrap_or_default(),
                        }).await;
                        drop(self.approval_control.lock().await);
                        if decision == ApprovalDecision::Cancelled || cancel.is_cancelled() {
                            let _ = self.approval_service().withdraw(&request.id,&request.scope,request.revision);
                        } else {
                            let service = self.approval_service();
                            if let Ok(current) = service.get(&request.id, &request.scope.workspace, Some(&request.scope.session_id))
                                && current.scope == request.scope && current.revision == request.revision
                                && current.subject.version == request.subject.version
                                && service.decide(&request.id, &request.scope, request.revision,
                                    decision == ApprovalDecision::Allow, "", &crate::approval::Actor::local("frontend")).is_ok()
                            {
                                let _ = service.apply_available(&request.scope.workspace);
                            }
                        }
                    }
                }
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
mod tests;
