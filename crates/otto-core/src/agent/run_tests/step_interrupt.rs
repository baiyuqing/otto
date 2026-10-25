//! Run-loop tests: a step interrupt delivers queued guidance inside the turn.

use super::*;
use crate::agent::steering::Steering;
use crate::model::{EffectCertainty, OperationDisposition, OperationOutcome, OperationStopReason};
use crate::session::{TurnOutcome, TurnRecord};
use crate::tool::ToolExecution;

/// Serves `echo`. In blocking mode it waits for its control token and then
/// returns a cancelled execution, as a killed `bash` call does.
struct BlockingExecutor {
    blocking: bool,
    invocations: Arc<AtomicUsize>,
    /// Runs when a call reaches the executor, before it waits.
    on_invocation: Box<dyn Fn() + Send + Sync>,
}

impl BlockingExecutor {
    fn new(
        blocking: bool,
        invocations: &Arc<AtomicUsize>,
        on_invocation: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        Self {
            blocking,
            invocations: Arc::clone(invocations),
            on_invocation: Box::new(on_invocation),
        }
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl ToolExecutor for BlockingExecutor {
    fn definitions(&self) -> Vec<ToolDefinition> {
        EchoExecutor::default().definitions()
    }

    async fn execute(
        &self,
        call: crate::tool::ToolCall<'_>,
        control: &dyn OperationControl,
    ) -> ToolExecution {
        self.invocations.fetch_add(1, Ordering::SeqCst);
        (self.on_invocation)();
        if !self.blocking {
            return ToolExecution::completed(ToolResult {
                content: call.arguments.get().to_owned(),
                ..ToolResult::default()
            });
        }
        control.cancellation_token().cancelled().await;
        ToolExecution {
            result: ToolResult {
                content: "partial output".into(),
                is_error: true,
                ..ToolResult::default()
            },
            outcome: OperationOutcome {
                disposition: OperationDisposition::Cancelled,
                effect_certainty: EffectCertainty::Unknown,
                stop_reason: Some(OperationStopReason::UserCancellation),
            },
        }
    }
}

/// On its first call, queues guidance, requests a step interrupt and waits for
/// its control token, then reports cancellation. Later calls are scripted.
struct InterruptedFirstCallProvider {
    steering: Arc<Steering>,
    first_call_pending: AtomicBool,
    scripted: FakeProvider,
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl Provider for InterruptedFirstCallProvider {
    async fn complete(
        &self,
        request: &Request,
        emit: StreamSink<'_>,
        control: &dyn OperationControl,
    ) -> ProviderSettlement {
        if self.first_call_pending.swap(false, Ordering::SeqCst) {
            self.steering.submit("change course").expect("active run");
            assert_eq!(self.steering.interrupt_step(), Ok(1));
            control.cancellation_token().cancelled().await;
            return ProviderSettlement::failed(
                ProviderError::Cancelled,
                1,
                EffectCertainty::Unknown,
            );
        }
        self.scripted.complete(request, emit, control).await
    }
}

/// Queues guidance and requests a step interrupt, as a frontend does on `Esc`.
fn queue_guidance_and_interrupt(steering: &Steering, text: &str) {
    steering.submit(text).expect("active run");
    assert_eq!(steering.interrupt_step(), Ok(1));
}

fn steered_options(steering: &Arc<Steering>) -> Options {
    Options {
        steering: Arc::clone(steering),
        ..options()
    }
}

fn terminal_of(session: &impl Session, tool_call_id: &str) -> OperationOutcome {
    session
        .operation_ledger()
        .operation_for_tool_call(tool_call_id)
        .and_then(|record| record.terminal.clone())
        .unwrap_or_else(|| panic!("{tool_call_id} has no terminal fact"))
}

fn tool_result_text(messages: &[Message], tool_call_id: &str) -> String {
    messages
        .iter()
        .flat_map(|message| &message.blocks)
        .find(|block| {
            block.block_type == BlockType::ToolResult && block.tool_call_id == tool_call_id
        })
        .unwrap_or_else(|| panic!("{tool_call_id} has no result"))
        .text
        .clone()
}

fn position_of(messages: &[Message], predicate: impl Fn(&Message) -> bool) -> usize {
    messages
        .iter()
        .position(predicate)
        .expect("message in transcript")
}

fn ended_outcomes(session: &TurnRecordingSession) -> Vec<TurnOutcome> {
    session
        .turns
        .lock()
        .expect("turns")
        .iter()
        .filter_map(|record| match record {
            TurnRecord::Ended { outcome, .. } => Some(*outcome),
            TurnRecord::Started { .. } => None,
        })
        .collect()
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn an_interrupt_during_a_tool_batch_delivers_guidance_in_the_same_turn() {
    let steering = Arc::new(Steering::default());
    let invocations = Arc::new(AtomicUsize::new(0));
    let agent = Agent::new(
        FakeProvider::new(vec![two_tool_calls(), Turn::text("done")]),
        BlockingExecutor::new(true, &invocations, {
            let steering = Arc::clone(&steering);
            move || queue_guidance_and_interrupt(&steering, "change course")
        }),
        TurnRecordingSession::default(),
        steered_options(&steering),
    );
    let mut interrupted = Vec::new();
    agent
        .run(
            "hi",
            &mut |event| {
                if let Event::StepInterrupted { guidance } = event {
                    interrupted.push(guidance);
                }
            },
            &CancellationToken::new(),
        )
        .await
        .expect("a step interrupt does not fail the turn");

    // Without the step control the executor is never cancelled and the run hangs.
    let first = terminal_of(agent.session(), "c1");
    assert_eq!(first.disposition, OperationDisposition::Cancelled);
    assert_eq!(first.effect_certainty, EffectCertainty::Unknown);
    let second = terminal_of(agent.session(), "c2");
    assert_eq!(second.disposition, OperationDisposition::Cancelled);
    assert_eq!(second.effect_certainty, EffectCertainty::NotStarted);
    assert_eq!(invocations.load(Ordering::SeqCst), 1);
    let messages = agent.session().messages();
    assert!(
        tool_result_text(&messages, "c2").contains(&ProviderError::Cancelled.to_string()),
        "{messages:?}"
    );
    let guidance = position_of(&messages, |m| {
        m.role == Role::User && m.text() == "change course"
    });
    let last_result = position_of(&messages, |m| {
        m.blocks
            .iter()
            .any(|b| b.tool_call_id == "c2" && b.block_type == BlockType::ToolResult)
    });
    assert!(guidance > last_result, "guidance follows both tool results");
    assert_eq!(interrupted, vec![1]);
    let requests = agent.provider().normal_requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|m| m.text() == "change course")
    );
    assert_eq!(
        ended_outcomes(agent.session()),
        vec![TurnOutcome::Completed]
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn an_interrupt_during_a_provider_call_delivers_guidance_in_the_same_turn() {
    let steering = Arc::new(Steering::default());
    let agent = Agent::new(
        InterruptedFirstCallProvider {
            steering: Arc::clone(&steering),
            first_call_pending: AtomicBool::new(true),
            scripted: FakeProvider::new(vec![Turn::text("done")]),
        },
        EchoExecutor::default(),
        TurnRecordingSession::default(),
        steered_options(&steering),
    );
    let mut interrupted = Vec::new();
    agent
        .run(
            "hi",
            &mut |event| {
                if let Event::StepInterrupted { guidance } = event {
                    interrupted.push(guidance);
                }
            },
            &CancellationToken::new(),
        )
        // Without the loop change this returns Err(Provider(Cancelled)).
        .await
        .expect("a step interrupt does not fail the turn");

    let texts: Vec<(Role, String)> = agent
        .session()
        .messages()
        .iter()
        .map(|message| (message.role.clone(), message.text()))
        .collect();
    assert_eq!(
        texts,
        vec![
            (Role::User, "hi".to_owned()),
            (Role::User, "change course".to_owned()),
            (Role::Assistant, "done".to_owned()),
        ]
    );
    let calls = agent.session().calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(
        calls[0].outcome.disposition,
        OperationDisposition::Cancelled
    );
    assert_eq!(
        calls[1].outcome.disposition,
        OperationDisposition::Succeeded
    );
    assert_eq!(interrupted, vec![1]);
    let requests = agent.provider().scripted.normal_requests();
    assert!(
        requests[0]
            .messages
            .iter()
            .any(|m| m.text() == "change course")
    );
    assert_eq!(
        ended_outcomes(agent.session()),
        vec![TurnOutcome::Completed]
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn an_interrupt_between_steps_settles_the_next_tool_call_as_not_started() {
    let steering = Arc::new(Steering::default());
    let invocations = Arc::new(AtomicUsize::new(0));
    let agent = Agent::new(
        FakeProvider::new(vec![
            Turn::tool_call("c1", r#"{"value":1}"#),
            Turn::text("done"),
        ]),
        BlockingExecutor::new(false, &invocations, || {}),
        TurnRecordingSession::default(),
        steered_options(&steering),
    );
    let mut interrupted = Vec::new();
    let usage_events = AtomicUsize::new(0);
    agent
        .run(
            "hi",
            &mut |event| match event {
                // Emitted after the provider call and before the tool call, so
                // no step token is registered when the interrupt arrives.
                Event::ProviderUsage { .. } if usage_events.fetch_add(1, Ordering::SeqCst) == 0 => {
                    queue_guidance_and_interrupt(&steering, "change course");
                }
                Event::StepInterrupted { guidance } => interrupted.push(guidance),
                _ => {}
            },
            &CancellationToken::new(),
        )
        .await
        .expect("run");

    // Without the sticky request the executor runs the call once.
    assert_eq!(invocations.load(Ordering::SeqCst), 0);
    let outcome = terminal_of(agent.session(), "c1");
    assert_eq!(outcome.disposition, OperationDisposition::Cancelled);
    assert_eq!(outcome.effect_certainty, EffectCertainty::NotStarted);
    assert_eq!(interrupted, vec![1]);
    let messages = agent.session().messages();
    let result = position_of(&messages, |m| m.role == Role::Tool);
    let guidance = position_of(&messages, |m| {
        m.role == Role::User && m.text() == "change course"
    });
    assert!(guidance > result);
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_turn_cancel_during_a_tool_call_still_cancels_the_turn() {
    let steering = Arc::new(Steering::default());
    let invocations = Arc::new(AtomicUsize::new(0));
    let cancel = CancellationToken::new();
    let agent = Agent::new(
        FakeProvider::new(vec![two_tool_calls(), Turn::text("done")]),
        BlockingExecutor::new(true, &invocations, {
            let steering = Arc::clone(&steering);
            let cancel = cancel.clone();
            move || {
                steering.submit("do not leak").expect("active run");
                cancel.cancel();
            }
        }),
        TurnRecordingSession::default(),
        steered_options(&steering),
    );
    let error = agent
        .run("hi", &mut |_| {}, &cancel)
        .await
        .expect_err("a turn cancel ends the run");

    assert!(error.is_cancelled(), "{error:?}");
    assert_eq!(
        ended_outcomes(agent.session()),
        vec![TurnOutcome::Cancelled]
    );
    assert!(
        !agent
            .session()
            .messages()
            .iter()
            .any(|m| m.text() == "do not leak")
    );
    let discarded = steering.take_discarded();
    assert_eq!(discarded.len(), 1, "{discarded:?}");
    assert_eq!(agent.provider().normal_requests().len(), 1);
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn interrupted_tool_results_do_not_trip_the_progress_guard() {
    let steering = Arc::new(Steering::default());
    let interrupted_steps = 13;
    let mut turns: Vec<Turn> = (0..interrupted_steps)
        .map(|index| Turn::tool_call(&format!("call-{index}"), &format!(r#"{{"value":{index}}}"#)))
        .collect();
    turns.push(Turn::text("done"));
    let invocations = Arc::new(AtomicUsize::new(0));
    let agent = Agent::new(
        FakeProvider::new(turns),
        BlockingExecutor::new(true, &invocations, {
            let steering = Arc::clone(&steering);
            move || queue_guidance_and_interrupt(&steering, "again")
        }),
        TurnRecordingSession::default(),
        steered_options(&steering),
    );
    let mut interrupts = 0;
    agent
        .run(
            "hi",
            &mut |event| {
                if matches!(event, Event::StepInterrupted { .. }) {
                    interrupts += 1;
                }
            },
            &CancellationToken::new(),
        )
        // Without the guard change the twelfth consecutive failure returns
        // Err(NoProgress(RepeatedFailure)).
        .await
        .expect("interrupted tools are not repeated failures");

    assert_eq!(interrupts, interrupted_steps);
    assert_eq!(
        ended_outcomes(agent.session()),
        vec![TurnOutcome::Completed]
    );
}
