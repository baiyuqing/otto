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

#[async_trait::async_trait]
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

#[async_trait::async_trait]
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

#[async_trait::async_trait]
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
            content_scope: Default::default(),
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
                ..Default::default()
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

#[async_trait::async_trait]
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

#[tokio::test]
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

#[tokio::test]
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

#[tokio::test]
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

#[tokio::test]
async fn goal_final_without_disposition_gets_one_correction_and_stays_open() {
    use crate::goal::{Execution, Goal, Status};

    let provider = ScriptedProvider::new(vec![
        (Vec::new(), assistant_text()),
        (Vec::new(), assistant_text()),
    ]);
    let session = MemorySession::new();
    let mut goal = Goal::draft("Fix duplicate exports".into());
    goal.status = Status::Open;
    goal.execution = Execution::Ready;
    goal.criteria = vec!["Repeated request creates one file".into()];
    assert!(session.compare_and_set_goal(None, &goal).await.unwrap());
    let agent = Agent::new(provider, EchoExecutor, session, test_options());
    let result = agent
        .run("Continue the goal", &mut |_| {}, &CancellationToken::new())
        .await;
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("Goal remains open")
    );
    assert_eq!(agent.provider().requests.lock().unwrap().len(), 2);
    let goal = agent.session().goal().expect("saved goal");
    assert_eq!(goal.status, Status::Open);
    assert_eq!(goal.execution, Execution::Interrupted);
}

#[tokio::test]
async fn paused_goal_records_late_child_result_without_calling_provider() {
    use crate::goal::{Execution, Goal, Status};

    let session = MemorySession::new();
    let mut goal = Goal::draft("Verify the export".into());
    goal.status = Status::Open;
    goal.execution = Execution::Paused;
    goal.criteria = vec!["Export tests pass".into()];
    assert!(session.compare_and_set_goal(None, &goal).await.unwrap());
    let inbox = Arc::new(super::inbox::Inbox::new(None));
    inbox.push(super::inbox::Notification {
        task_id: "t1".into(),
        kind: Some(super::inbox::NotificationKind::TaskFinished),
        text: "Late result".into(),
        ..Default::default()
    });
    let mut options = test_options();
    options.mailbox = inbox;
    let agent = Agent::new(
        ScriptedProvider::new(vec![]),
        EchoExecutor,
        session,
        options,
    );
    agent
        .run("", &mut |_| {}, &CancellationToken::new())
        .await
        .unwrap();
    assert!(agent.provider().requests.lock().unwrap().is_empty());
    assert_eq!(agent.session().goal(), Some(goal));
    assert!(
        agent
            .session()
            .messages()
            .iter()
            .any(|message| message.text().contains("Late result"))
    );
}

#[tokio::test]
async fn image_turn_persists_and_sends_ordered_attachments() {
    for text in ["", "compare these"] {
        let provider = ScriptedProvider::new(vec![(Vec::new(), assistant_text())]);
        let mut options = test_options();
        options.provider_name = "openai-compatible".into();
        options.model = "gpt-4.1".into();
        let agent = Agent::new(provider, EchoExecutor, MemorySession::new(), options);
        let images = vec![
            Block::image("iVBORw0KGgo=", "image/png"),
            Block::image("/9j/", "image/jpeg"),
        ];
        let mut expected = Vec::new();
        if !text.is_empty() {
            expected.push(Block::text(text));
        }
        expected.extend(images.clone());
        agent
            .run_with_images_budget(
                BudgetScope::Fresh(Default::default()),
                text,
                images,
                "",
                &mut |_| {},
                &CancellationToken::new(),
            )
            .await
            .expect("image turn");
        let persisted = agent.session().messages();
        let user = persisted
            .iter()
            .find(|m| m.role == Role::User)
            .expect("saved user message");
        assert_eq!(user.blocks, expected);
        let requests = agent.provider().requests.lock().unwrap();
        let sent = requests[0]
            .messages
            .iter()
            .find(|m| m.role == Role::User)
            .expect("sent user message");
        assert_eq!(sent.blocks, expected);
    }
}

#[tokio::test]
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
                    cached_input_tokens: 0,
                    ..Default::default()
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
                    content_scope: Default::default(),
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

#[tokio::test]
async fn run_persists_the_override_text_and_reports_the_live_text() {
    struct OverridingExecutor;

    #[async_trait::async_trait]
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
                content_scope: Default::default(),
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

#[tokio::test]
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

mod deadline_hooks;
