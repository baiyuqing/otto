
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use chrono::{DateTime, Utc};
use serde_json::value::RawValue;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::model::{Block, BlockType, FinishReason, Message, Role, ToolDefinition, Usage};
use crate::operation::OperationControl;
use crate::provider::{
    Provider, ProviderError, ProviderSettlement, Request, Response, StreamEvent, StreamSink,
};
use crate::session::{MemorySession, Session};
use crate::tool::{ToolExecutor, ToolResult};

struct StoppedControl {
    token: CancellationToken,
    reason: OperationStopReason,
}

impl OperationControl for StoppedControl {
    fn cancellation_token(&self) -> &CancellationToken {
        &self.token
    }

    fn remaining(&self) -> Option<std::time::Duration> {
        Some(std::time::Duration::ZERO)
    }

    fn stop_reason(&self) -> Option<OperationStopReason> {
        Some(self.reason)
    }
}

fn fixed_clock() -> DateTime<Utc> {
    DateTime::from_timestamp(1_700_000_000, 0).expect("in range")
}

fn raw(json: &str) -> Box<RawValue> {
    RawValue::from_string(json.to_owned()).expect("valid JSON")
}

fn test_options() -> Options {
    let counter = AtomicUsize::new(0);
    Options {
        model: "test-model".into(),
        provider_name: "fake".into(),
        system_prompt: "be brief".into(),
        thinking: String::new(),
        now: Box::new(fixed_clock),
        new_id: Box::new(move || format!("id-{}", counter.fetch_add(1, Ordering::SeqCst))),
        ..Options::default()
    }
}

/// Replays scripted turns and records the requests it was given.
struct ScriptedProvider {
    turns: Mutex<std::collections::VecDeque<(Vec<StreamEvent>, Response)>>,
    requests: Mutex<Vec<Request>>,
}

impl ScriptedProvider {
    fn new(turns: Vec<(Vec<StreamEvent>, Response)>) -> Self {
        Self {
            turns: Mutex::new(turns.into()),
            requests: Mutex::new(Vec::new()),
        }
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl Provider for ScriptedProvider {
    async fn complete(
        &self,
        request: &Request,
        emit: StreamSink<'_>,
        _control: &dyn OperationControl,
    ) -> ProviderSettlement {
        self.requests
            .lock()
            .expect("requests")
            .push(request.clone());
        let turn = self.turns.lock().expect("turns").pop_front();
        let Some((events, response)) = turn else {
            return ProviderSettlement::failed(
                ProviderError::Other("no scripted turn left".into()),
                0,
                EffectCertainty::NotStarted,
            );
        };
        for event in events {
            emit(event);
        }
        ProviderSettlement::succeeded(response, 1)
    }
}

/// Waits for cancellation and then reports it.
struct CancellingProvider;

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl Provider for CancellingProvider {
    async fn complete(
        &self,
        _request: &Request,
        _emit: StreamSink<'_>,
        control: &dyn OperationControl,
    ) -> ProviderSettlement {
        control.cancellation_token().cancelled().await;
        ProviderSettlement::failed(ProviderError::Cancelled, 1, EffectCertainty::Unknown)
    }
}

/// Returns the call arguments as the result text.
struct EchoExecutor;

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl ToolExecutor for EchoExecutor {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "echo".into(),
            description: "returns its arguments".into(),
            parameters: Some(raw(r#"{"type":"object"}"#)),
        }]
    }

    async fn execute(
        &self,
        call: crate::tool::ToolCall<'_>,
        _control: &dyn OperationControl,
    ) -> crate::tool::ToolExecution {
        if call.name != "echo" {
            return crate::tool::ToolExecution {
                result: ToolResult::unknown_tool(call.name),
                outcome: crate::model::OperationOutcome {
                    disposition: crate::model::OperationDisposition::Error,
                    effect_certainty: crate::model::EffectCertainty::NotStarted,
                    stop_reason: None,
                },
            };
        }
        crate::tool::ToolExecution::completed(ToolResult {
            content: call.arguments.get().to_owned(),
            persisted_content: None,
            is_error: false,
            outcome_override: None,
        })
    }
}

fn assistant_tool_call() -> Response {
    Response {
        message: Message {
            role: Role::Assistant,
            finish_reason: Some(FinishReason::ToolCalls),
            usage: Some(Usage {
                input_tokens: 10,
                output_tokens: 3,
                cached_input_tokens: 0,
            }),
            blocks: vec![Block {
                block_type: BlockType::ToolCall,
                tool_call_id: "call-1".into(),
                tool_name: "echo".into(),
                arguments: Some(raw(r#"{"value":1}"#)),
                ..Block::default()
            }],
            ..Message::default()
        },
        ..Response::default()
    }
}

fn assistant_text() -> Response {
    Response {
        message: Message {
            role: Role::Assistant,
            finish_reason: Some(FinishReason::Stop),
            blocks: vec![Block::text("done")],
            ..Message::default()
        },
        ..Response::default()
    }
}

/// An executor that serves `echo` and declares the names in `replayable`
/// replayable, counting how many calls reached it.
struct ReplayExecutor {
    replayable: &'static [&'static str],
    calls: AtomicUsize,
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl ToolExecutor for ReplayExecutor {
    fn definitions(&self) -> Vec<ToolDefinition> {
        EchoExecutor.definitions()
    }

    fn retry_safety(&self, name: &str) -> crate::model::RetrySafety {
        if self.replayable.contains(&name) {
            crate::model::RetrySafety::ReadOnly
        } else {
            crate::model::RetrySafety::NonIdempotent
        }
    }

    async fn execute(
        &self,
        call: crate::tool::ToolCall<'_>,
        control: &dyn OperationControl,
    ) -> crate::tool::ToolExecution {
        self.calls.fetch_add(1, Ordering::SeqCst);
        EchoExecutor.execute(call, control).await
    }
}

/// A session that ends on an assistant message with an unanswered `echo`
/// call, the shape a takeover leaves for a replayable tool, and whose
/// recorded first attempt (when `attempted`) was never settled.
async fn session_with_pending_echo(attempted: bool) -> MemorySession {
    let session = MemorySession::new();
    let call = assistant_tool_call().message;
    session.append(call).await.expect("append call");
    if attempted {
        session
            .append_operation_fact(OperationFact::attempt(
                OperationId::new("op-prior").expect("operation id"),
                1,
                "call-1",
                "echo",
            ))
            .await
            .expect("attempt fact");
    }
    session
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn run_replays_a_replayable_call_a_takeover_left_unanswered() {
    let provider = ScriptedProvider::new(vec![(Vec::new(), assistant_text())]);
    let executor = ReplayExecutor {
        replayable: &["echo"],
        calls: AtomicUsize::new(0),
    };
    let agent = Agent::new(
        provider,
        executor,
        session_with_pending_echo(true).await,
        test_options(),
    );
    let mut events = Vec::new();
    agent
        .run(
            "continue",
            &mut |event| events.push(event),
            &CancellationToken::new(),
        )
        .await
        .expect("run");

    let started: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            Event::ToolCallStarted {
                operation_id,
                attempt,
                ..
            } => Some((operation_id.as_str().to_owned(), *attempt)),
            _ => None,
        })
        .collect();
    assert_eq!(
        started,
        vec![("op-prior".to_owned(), 2)],
        "the call reruns as the next attempt of its recorded operation"
    );
    let roles: Vec<Role> = agent
        .session()
        .messages()
        .iter()
        .map(|message| message.role.clone())
        .collect();
    assert_eq!(
        roles,
        vec![
            Role::Assistant,
            Role::Tool,
            Role::Context,
            Role::User,
            Role::Assistant
        ],
        "the result follows its call; the legacy tool-set notice (history, no record) \
             precedes the user message"
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn recovered_nonidempotent_outcome_reaches_model_and_new_call_is_distinct() {
    use crate::model::{EffectCertainty, OperationDisposition, ToolResultMetadata};
    for new_call in [false, true] {
        let executor = ReplayExecutor {
            replayable: &[],
            calls: AtomicUsize::new(0),
        };
        let prior_operation = OperationId::new("op-prior").unwrap();
        let arguments = raw("{}");
        // Execute once, then simulate process loss before its result persisted.
        executor
            .execute(
                crate::tool::ToolCall {
                    tool_call_id: "call-1",
                    operation_id: &prior_operation,
                    name: "echo",
                    arguments: &arguments,
                    attempt: 1,
                },
                &CancellationToken::new(),
            )
            .await;
        assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
        let session = session_with_pending_echo(true).await;
        let mut result = Block {
            block_type: BlockType::ToolResult,
            tool_call_id: "call-1".into(),
            tool_name: "echo".into(),
            text: "operation interrupted; effects unknown".into(),
            is_error: true,
            ..Block::default()
        };
        result.operation_metadata = Some(ToolResultMetadata {
            operation_id: Some(OperationId::new("op-prior").unwrap()),
            disposition: OperationDisposition::Interrupted,
            effect_certainty: EffectCertainty::Unknown,
            stop_reason: Some(crate::model::OperationStopReason::ProcessLost),
        });
        session
            .append(Message {
                role: Role::Tool,
                blocks: vec![result],
                ..Message::default()
            })
            .await
            .unwrap();
        let mut script = Vec::new();
        if new_call {
            // Identical arguments remain a new model decision, not a retry.
            // The transcript contract requires a fresh provider call ID.
            let mut call = assistant_tool_call();
            call.message.blocks[0].tool_call_id = "call-new".into();
            script.push((Vec::new(), call));
        }
        script.push((Vec::new(), assistant_text()));
        let agent = Agent::new(
            ScriptedProvider::new(script),
            executor,
            session,
            test_options(),
        );
        let mut events = Vec::new();
        agent
            .run(
                "continue",
                &mut |event| events.push(event),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        // One effect happened before process loss; recovery adds no execution.
        // Only the explicitly model-emitted new invocation adds one execution.
        assert_eq!(
            agent.tools.calls.load(Ordering::SeqCst),
            1 + usize::from(new_call)
        );
        let requests = agent.provider().requests.lock().unwrap();
        let old = requests[0]
            .messages
            .iter()
            .flat_map(|m| &m.blocks)
            .find(|b| b.block_type == BlockType::ToolResult)
            .expect("interrupted outcome sent to model");
        assert!(old.is_error);
        assert!(old.text.contains("effects unknown"));
        let metadata = old.operation_metadata.as_ref().unwrap();
        assert_eq!(metadata.disposition, OperationDisposition::Interrupted);
        assert_eq!(metadata.effect_certainty, EffectCertainty::Unknown);
        if new_call {
            let started: Vec<_> = events
                .iter()
                .filter_map(|event| match event {
                    Event::ToolCallStarted { operation_id, .. } => Some(operation_id),
                    _ => None,
                })
                .collect();
            assert_eq!(started.len(), 1);
            assert_ne!(started[0].as_str(), "op-prior");
        }
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn run_does_not_replay_a_call_whose_tool_is_not_replayable() {
    let provider = ScriptedProvider::new(vec![(Vec::new(), assistant_text())]);
    let executor = ReplayExecutor {
        replayable: &[],
        calls: AtomicUsize::new(0),
    };
    let agent = Agent::new(
        provider,
        executor,
        session_with_pending_echo(true).await,
        test_options(),
    );
    let result = agent
        .run("continue", &mut |_| {}, &CancellationToken::new())
        .await;
    assert!(
        result.is_err(),
        "a pending call the store should have answered is a loud error, not a silent run"
    );
    assert_eq!(agent.tools.calls.load(Ordering::SeqCst), 0);
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn run_executes_a_tool_turn_then_a_text_turn() {
    // op_1 is the turn id of the `otto.turn` records.
    let provider = ScriptedProvider::new(vec![
        (Vec::new(), assistant_tool_call()),
        (
            vec![StreamEvent::TextDelta {
                text: "done".into(),
            }],
            assistant_text(),
        ),
    ]);
    let agent = Agent::new(provider, EchoExecutor, MemorySession::new(), test_options());
    let mut events = Vec::new();
    agent
        .run(
            "hello",
            &mut |event| events.push(event),
            &CancellationToken::new(),
        )
        .await
        .expect("run");

    let api_call = Event::ProviderApiCall {
        operation_id: OperationId::new("op_2").expect("operation id"),
        kind: ProviderOperationKind::Ordinary,
        provider: "fake".into(),
        model: "test-model".into(),
        duration: std::time::Duration::ZERO,
        attempts: 1,
        status: ApiStatus::Ok,
        outcome: OperationOutcome {
            disposition: OperationDisposition::Succeeded,
            effect_certainty: EffectCertainty::Completed,
            stop_reason: None,
        },
    };
    let second_api_call = Event::ProviderApiCall {
        operation_id: OperationId::new("op_4").expect("operation id"),
        kind: ProviderOperationKind::Ordinary,
        provider: "fake".into(),
        model: "test-model".into(),
        duration: std::time::Duration::ZERO,
        attempts: 1,
        status: ApiStatus::Ok,
        outcome: OperationOutcome {
            disposition: OperationDisposition::Succeeded,
            effect_certainty: EffectCertainty::Completed,
            stop_reason: None,
        },
    };
    assert_eq!(
        events,
        vec![
            Event::AgentStarted,
            api_call.clone(),
            Event::ProviderUsage {
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 3,
                    cached_input_tokens: 0
                },
                present: true,
                cached_input_tokens_present: false,
            },
            Event::ToolCallStarted {
                operation_id: OperationId::new("op_3").expect("operation id"),
                attempt: 1,
                tool_name: "echo".into(),
                tool_call_id: "call-1".into(),
                arguments: r#"{"value":1}"#.into(),
            },
            Event::ToolCallFinished {
                operation_id: OperationId::new("op_3").expect("operation id"),
                attempt: 1,
                tool_name: "echo".into(),
                tool_call_id: "call-1".into(),
                result: ToolResult {
                    content: r#"{"value":1}"#.into(),
                    persisted_content: None,
                    is_error: false,
                    outcome_override: None,
                },
                outcome: crate::model::OperationOutcome {
                    disposition: crate::model::OperationDisposition::Succeeded,
                    effect_certainty: crate::model::EffectCertainty::Completed,
                    stop_reason: None,
                },
            },
            Event::TextDelta {
                text: "done".into()
            },
            second_api_call,
            Event::ProviderUsage {
                usage: Usage::default(),
                present: false,
                cached_input_tokens_present: false
            },
            Event::AgentFinished,
        ]
    );

    let messages = agent.session().messages();
    let roles: Vec<Role> = messages
        .iter()
        .map(|message| message.role.clone())
        .collect();
    assert_eq!(
        roles,
        vec![Role::User, Role::Assistant, Role::Tool, Role::Assistant]
    );
    assert_eq!(messages[0].text(), "hello");
    assert_eq!(messages[2].blocks[0].text, r#"{"value":1}"#);
    assert_eq!(messages[3].text(), "done");
    for message in &messages {
        assert!(!message.id.is_empty(), "message id was not filled in");
        assert_eq!(message.created_at, fixed_clock());
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn run_persists_the_override_text_and_reports_the_live_text() {
    struct OverridingExecutor;

    #[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
    #[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
    impl ToolExecutor for OverridingExecutor {
        fn definitions(&self) -> Vec<ToolDefinition> {
            Vec::new()
        }
        async fn execute(
            &self,
            _call: crate::tool::ToolCall<'_>,
            _control: &dyn OperationControl,
        ) -> crate::tool::ToolExecution {
            crate::tool::ToolExecution::completed(ToolResult {
                content: "live".into(),
                persisted_content: Some("stored".into()),
                is_error: false,
                outcome_override: None,
            })
        }
    }

    let provider = ScriptedProvider::new(vec![
        (Vec::new(), assistant_tool_call()),
        (Vec::new(), assistant_text()),
    ]);
    let agent = Agent::new(
        provider,
        OverridingExecutor,
        MemorySession::new(),
        test_options(),
    );
    let mut live = String::new();
    agent
        .run(
            "hello",
            &mut |event| {
                if let Event::ToolCallFinished { result, .. } = event {
                    live = result.content;
                }
            },
            &CancellationToken::new(),
        )
        .await
        .expect("run");
    assert_eq!(live, "live");
    assert_eq!(agent.session().messages()[2].blocks[0].text, "stored");
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn run_rejects_empty_user_text() {
    let agent = Agent::new(
        ScriptedProvider::new(Vec::new()),
        EchoExecutor,
        MemorySession::new(),
        test_options(),
    );
    let mut events = Vec::new();
    let error = agent
        .run(
            " \t\n\r ",
            &mut |event| events.push(event),
            &CancellationToken::new(),
        )
        .await
        .expect_err("empty text accepted");
    assert!(matches!(error, AgentError::EmptyUserText));
    assert_eq!(
        events,
        vec![Event::AgentError {
            message: "user text is required".into()
        }]
    );
    assert!(agent.session().messages().is_empty());
}

#[test]
fn deadline_tool_stop_is_typed_and_not_started() {
    let execution = stopped_tool_execution(OperationStopReason::Deadline);
    assert_eq!(
        execution.outcome,
        OperationOutcome {
            disposition: OperationDisposition::DeadlineExceeded,
            effect_certainty: EffectCertainty::NotStarted,
            stop_reason: Some(OperationStopReason::Deadline),
        }
    );
    assert_eq!(
        execution.result.content,
        ProviderError::DeadlineExceeded.to_string()
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn deadline_before_provider_dispatch_is_a_typed_error() {
    let provider = ScriptedProvider::new(Vec::new());
    let agent = Agent::new(
        provider,
        EchoExecutor,
        MemorySession::default(),
        test_options(),
    );
    let control = StoppedControl {
        token: CancellationToken::new(),
        reason: OperationStopReason::Deadline,
    };
    let error = agent
        .run_with_control("hello", &mut |_| {}, &control)
        .await
        .expect_err("deadline must stop dispatch");
    assert!(matches!(
        error,
        AgentError::Provider(ProviderError::DeadlineExceeded)
    ));
    assert!(
        agent
            .provider()
            .requests
            .lock()
            .expect("requests")
            .is_empty()
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn run_reports_cancellation_as_an_agent_error() {
    let agent = Agent::new(
        CancellingProvider,
        EchoExecutor,
        MemorySession::new(),
        test_options(),
    );
    let cancel = CancellationToken::new();
    cancel.cancel();
    let mut events = Vec::new();
    let error = agent
        .run("hello", &mut |event| events.push(event), &cancel)
        .await
        .expect_err("cancellation was not reported");
    assert!(matches!(
        error,
        AgentError::Provider(ProviderError::Cancelled)
    ));
    assert_eq!(
        events,
        vec![
            Event::AgentStarted,
            Event::ProviderApiCall {
                operation_id: OperationId::new("op_2").expect("operation id"),
                kind: ProviderOperationKind::Ordinary,
                provider: "fake".into(),
                model: "test-model".into(),
                duration: std::time::Duration::ZERO,
                attempts: 0,
                status: ApiStatus::Canceled,
                outcome: OperationOutcome {
                    disposition: OperationDisposition::Cancelled,
                    effect_certainty: EffectCertainty::NotStarted,
                    stop_reason: Some(OperationStopReason::UserCancellation),
                },
            },
            Event::AgentError {
                message: "provider call was cancelled".into()
            },
        ]
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn run_rejects_a_non_assistant_provider_response() {
    let provider = ScriptedProvider::new(vec![(
        Vec::new(),
        Response {
            message: Message {
                role: Role::User,
                blocks: vec![Block::text("wrong")],
                ..Message::default()
            },
            ..Response::default()
        },
    )]);
    let agent = Agent::new(provider, EchoExecutor, MemorySession::new(), test_options());
    let error = agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect_err("non-assistant response accepted");
    assert_eq!(
        error.to_string(),
        "invalid provider response: assistant role is required"
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn run_sends_the_session_transcript_and_tool_definitions() {
    let provider = ScriptedProvider::new(vec![(Vec::new(), assistant_text())]);
    let agent = Agent::new(provider, EchoExecutor, MemorySession::new(), test_options());
    agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("run");
    let requests = agent.provider().requests.lock().expect("requests").clone();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].model, "test-model");
    assert_eq!(requests[0].system_prompt, "be brief");
    assert_eq!(requests[0].messages.len(), 1);
    assert_eq!(requests[0].messages[0].text(), "hello");
    assert_eq!(requests[0].tools, EchoExecutor.definitions());
}
#[derive(Default)]
struct TestHooks {
    events: Mutex<Vec<(String, serde_json::Value)>>,
    deny: bool,
    after_fail: bool,
}
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch="wasm32",async_trait::async_trait(?Send))]
impl hooks::Hooks for TestHooks {
    async fn run(
        &self,
        event: &str,
        data: serde_json::Value,
        _: &dyn OperationControl,
    ) -> Result<hooks::Response, String> {
        self.events.lock().unwrap().push((event.into(), data));
        match event {
            "before_prompt" => Ok(hooks::Response {
                context: Some("plugin context".into()),
                ..Default::default()
            }),
            "before_tool" if self.deny => Ok(hooks::Response {
                decision: Some(hooks::Decision::Deny),
                reason: Some("denied".into()),
                ..Default::default()
            }),
            "before_tool" => Ok(hooks::Response {
                arguments: Some(serde_json::json!({"value":2})),
                ..Default::default()
            }),
            "after_tool" if self.after_fail => Err("failed after hook".into()),
            "after_tool" => Ok(hooks::Response {
                content: Some("filtered".into()),
                ..Default::default()
            }),
            _ => Ok(Default::default()),
        }
    }
}
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn hooks_rewrite_before_execution_preserve_effects_and_share_session_lifecycle() {
    let hooks = Arc::new(TestHooks::default());
    let mut options = test_options();
    options.hooks = Some(hooks.clone());
    let agent = Agent::new(
        ScriptedProvider::new(vec![
            (Vec::new(), assistant_tool_call()),
            (Vec::new(), assistant_text()),
            (Vec::new(), assistant_text()),
        ]),
        EchoExecutor,
        MemorySession::new(),
        options,
    );
    let mut events = Vec::new();
    agent
        .run("first", &mut |e| events.push(e), &CancellationToken::new())
        .await
        .unwrap();
    agent
        .run("second", &mut |e| events.push(e), &CancellationToken::new())
        .await
        .unwrap();
    agent.end_hooks().await;
    agent.end_hooks().await;
    let recorded = hooks.events.lock().unwrap();
    assert_eq!(
        recorded.iter().map(|(e, _)| e.as_str()).collect::<Vec<_>>(),
        vec![
            "session_start",
            "before_prompt",
            "before_tool",
            "after_tool",
            "turn_end",
            "before_prompt",
            "turn_end",
            "session_end"
        ]
    );
    assert_eq!(
        recorded.iter().find(|(e, _)| e == "after_tool").unwrap().1["content"],
        r#"{"value":2}"#
    );
    assert!(
        !recorded
            .iter()
            .find(|(e, _)| e == "before_prompt")
            .unwrap()
            .1["turn_id"]
            .as_str()
            .unwrap()
            .is_empty()
    );
    assert!(
        agent
            .provider
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|r| r.system_prompt.contains("Untrusted plugin context"))
    );
    assert!(events.iter().any(|e|matches!(e,Event::ToolCallFinished{result,outcome,..} if result.content=="filtered" && outcome.effect_certainty==EffectCertainty::Completed)));
    assert!(
        agent
            .session
            .messages()
            .iter()
            .any(|m| m.role == Role::Tool && m.blocks.iter().any(|b| b.text == "filtered"))
    );
}
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn hook_denial_is_not_started_and_after_failure_keeps_the_tool_result() {
    for deny in [true, false] {
        let hooks = Arc::new(TestHooks {
            deny,
            after_fail: true,
            ..Default::default()
        });
        let mut options = test_options();
        options.hooks = Some(hooks.clone());
        let agent = Agent::new(
            ScriptedProvider::new(vec![
                (Vec::new(), assistant_tool_call()),
                (Vec::new(), assistant_text()),
            ]),
            EchoExecutor,
            MemorySession::new(),
            options,
        );
        let mut events = Vec::new();
        agent
            .run("run", &mut |e| events.push(e), &CancellationToken::new())
            .await
            .unwrap();
        let terminal = events
            .iter()
            .find_map(|e| {
                if let Event::ToolCallFinished {
                    result, outcome, ..
                } = e
                {
                    Some((result, outcome))
                } else {
                    None
                }
            })
            .unwrap();
        if deny {
            assert_eq!(terminal.1.effect_certainty, EffectCertainty::NotStarted);
            assert_eq!(terminal.0.content, "denied");
            assert!(
                !hooks
                    .events
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|(e, _)| e == "after_tool")
            );
        } else {
            assert_eq!(terminal.1.effect_certainty, EffectCertainty::Completed);
            assert_eq!(terminal.0.content, r#"{"value":2}"#);
            assert!(!terminal.0.is_error);
            assert!(events.iter().any(
                |e| matches!(e,Event::Notification{text,..} if text.contains("failed after hook"))
            ));
        }
    }
}
