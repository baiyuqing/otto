//! Run-loop tests: the turn loop, overflow, compaction, memory context, and
//! tasks.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;

use serde_json::value::RawValue;
use tokio_util::sync::CancellationToken;

use crate::model::{Block, BlockType, FinishReason, Message, Role, ToolDefinition, Usage};
use crate::operation::OperationControl;
use crate::provider::{
    ContextOverflowError, Provider, ProviderError, ProviderSettlement, Request, RequestSizer,
    Response, StreamEvent, StreamSink,
};
use crate::session::{MemorySession, Session};
use crate::tool::{ToolExecutor, ToolResult};

use super::context_report::SectionKind;
use super::inbox::{Inbox, Notification, NotificationKind};
use super::memory::{MemoryError, MemoryRecall, RecallRequest, RecallResult, Record, Scope};
use super::redactor::Redactor;
use super::summary::SUMMARIZATION_SYSTEM_PROMPT;
use super::summary_validate::REQUIRED_SUMMARY_HEADINGS;
use super::{Agent, CompactionSettings, Event, Options};

// -- harness ---------------------------------------------------------------

fn raw(json: &str) -> Box<RawValue> {
    RawValue::from_string(json.to_owned()).expect("valid JSON")
}

fn clock() -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp(1_700_000_000, 0).expect("in range")
}

fn options() -> Options {
    let counter = AtomicUsize::new(0);
    Options {
        model: "test-model".into(),
        provider_name: "fake".into(),
        system_prompt: "be brief".into(),
        now: Box::new(clock),
        new_id: Box::new(move || format!("id-{}", counter.fetch_add(1, Ordering::SeqCst))),
        request_sizer: Some(Arc::new(JsonSizer)),
        ..Options::default()
    }
}

/// One scripted provider turn.
struct Turn {
    events: Vec<StreamEvent>,
    outcome: Result<Response, ProviderError>,
}

impl Turn {
    fn text(text: &str) -> Self {
        Self {
            events: vec![StreamEvent::TextDelta { text: text.into() }],
            outcome: Ok(Response {
                message: Message {
                    role: Role::Assistant,
                    finish_reason: Some(FinishReason::Stop),
                    blocks: vec![Block::text(text)],
                    ..Message::default()
                },
            }),
        }
    }

    fn tool_call(call_id: &str, arguments: &str) -> Self {
        Self {
            events: Vec::new(),
            outcome: Ok(Response {
                message: Message {
                    role: Role::Assistant,
                    finish_reason: Some(FinishReason::ToolCalls),
                    blocks: vec![Block {
                        block_type: BlockType::ToolCall,
                        tool_call_id: call_id.into(),
                        tool_name: "echo".into(),
                        arguments: Some(raw(arguments)),
                        ..Block::default()
                    }],
                    ..Message::default()
                },
            }),
        }
    }

    fn summary(text: &str) -> Self {
        Self {
            events: Vec::new(),
            outcome: Ok(Response {
                message: Message {
                    role: Role::Assistant,
                    finish_reason: Some(FinishReason::Stop),
                    blocks: vec![Block::text(text)],
                    usage: Some(Usage {
                        input_tokens: 5,
                        output_tokens: 7,
                        cached_input_tokens: 0,
                    }),
                    ..Message::default()
                },
            }),
        }
    }

    fn overflow() -> Self {
        Self {
            events: Vec::new(),
            outcome: Err(ProviderError::Overflow(ContextOverflowError::default())),
        }
    }

    fn failure(message: &str) -> Self {
        Self {
            events: Vec::new(),
            outcome: Err(ProviderError::Other(message.into())),
        }
    }

    fn with_events(mut self, events: Vec<StreamEvent>) -> Self {
        self.events = events;
        self
    }
}

/// Replays scripted turns and records every request it was given.
struct FakeProvider {
    turns: Mutex<VecDeque<Turn>>,
    requests: Mutex<Vec<Request>>,
}

impl FakeProvider {
    fn new(turns: Vec<Turn>) -> Self {
        Self {
            turns: Mutex::new(turns.into()),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<Request> {
        self.requests.lock().expect("requests").clone()
    }

    /// The requests that are ordinary turns, not summary calls. A summary
    /// request carries no tool definitions and one user message.
    fn normal_requests(&self) -> Vec<Request> {
        self.requests()
            .into_iter()
            .filter(|request| {
                !request
                    .system_prompt
                    .starts_with(SUMMARIZATION_SYSTEM_PROMPT)
            })
            .collect()
    }

    fn summary_requests(&self) -> Vec<Request> {
        self.requests()
            .into_iter()
            .filter(|request| {
                request
                    .system_prompt
                    .starts_with(SUMMARIZATION_SYSTEM_PROMPT)
            })
            .collect()
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl Provider for FakeProvider {
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
        let Some(turn) = self.turns.lock().expect("turns").pop_front() else {
            return ProviderSettlement::failed(
                ProviderError::Other("no scripted turn left".into()),
                0,
                crate::model::EffectCertainty::NotStarted,
            );
        };
        for event in turn.events {
            emit(event);
        }
        match turn.outcome {
            Ok(response) => ProviderSettlement::succeeded(response, 1),
            Err(error) => {
                ProviderSettlement::failed(error, 1, crate::model::EffectCertainty::Completed)
            }
        }
    }
}

/// The size a request serializes to, used by the summary-request bound.
struct JsonSizer;

impl RequestSizer for JsonSizer {
    fn serialized_request_size(&self, request: &Request) -> Result<usize, ProviderError> {
        let mut total = request.system_prompt.len() + request.model.len();
        for message in &request.messages {
            total += message.text().len();
        }
        Ok(total)
    }
}

/// Returns its arguments, or the configured override.
#[derive(Default)]
struct EchoExecutor {
    persisted: Option<String>,
    content: Option<String>,
}

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
            content: self
                .content
                .clone()
                .unwrap_or_else(|| call.arguments.get().to_owned()),
            persisted_content: self.persisted.clone(),
            is_error: false,
            outcome_override: None,
        })
    }
}

/// A memory binding that returns a fixed result and counts its calls.
struct FakeMemory {
    records: Vec<Record>,
    error: Option<String>,
    calls: AtomicUsize,
    close_error: Option<String>,
}

impl FakeMemory {
    fn with_records(records: Vec<Record>) -> Self {
        Self {
            records,
            error: None,
            calls: AtomicUsize::new(0),
            close_error: None,
        }
    }

    fn failing(message: &str) -> Self {
        Self {
            records: Vec::new(),
            error: Some(message.into()),
            calls: AtomicUsize::new(0),
            close_error: None,
        }
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl MemoryRecall for FakeMemory {
    async fn recall(
        &self,
        _request: &RecallRequest,
        _cancel: &CancellationToken,
    ) -> Result<RecallResult, MemoryError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match &self.error {
            Some(message) => Err(MemoryError(message.clone())),
            None => Ok(RecallResult {
                records: self.records.clone(),
                used_tokens: 1,
            }),
        }
    }

    fn close(&self) -> Result<(), MemoryError> {
        match &self.close_error {
            Some(message) => Err(MemoryError(message.clone())),
            None => Ok(()),
        }
    }
}

/// A structured summary that passes validation.
fn structured_summary() -> String {
    REQUIRED_SUMMARY_HEADINGS
        .iter()
        .map(|heading| format!("{heading}\nnote\n"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn collect(events: &Arc<Mutex<Vec<Event>>>) -> Vec<Event> {
    events.lock().expect("events").clone()
}

fn names(events: &[Event]) -> Vec<&'static str> {
    events.iter().map(Event::name).collect()
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn image_is_persisted_and_sent_with_the_user_prompt() {
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::text("ok")]),
        EchoExecutor::default(),
        MemorySession::new(),
        options(),
    );
    agent
        .run_with_image(
            "read it",
            Some(Block::image("iVBORw0KGgo=", "image/png")),
            &mut |_| {},
            &CancellationToken::new(),
        )
        .await
        .expect("run");

    let messages = agent.session().messages();
    assert_eq!(messages[0].blocks[0], Block::text("read it"));
    assert_eq!(messages[0].blocks[1].block_type, BlockType::Image);
    let requests = agent.provider().normal_requests();
    assert_eq!(
        requests[0].messages[0].blocks[1].block_type,
        BlockType::Image
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn image_without_text_is_persisted_and_sent() {
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::text("ok")]),
        EchoExecutor::default(),
        MemorySession::new(),
        options(),
    );
    agent
        .run_with_image(
            "",
            Some(Block::image("iVBORw0KGgo=", "image/png")),
            &mut |_| {},
            &CancellationToken::new(),
        )
        .await
        .expect("run");

    let messages = agent.session().messages();
    assert_eq!(messages[0].blocks.len(), 1);
    assert_eq!(messages[0].blocks[0].block_type, BlockType::Image);
    assert_eq!(
        agent.provider().normal_requests()[0].messages[0].blocks[0].block_type,
        BlockType::Image
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn steering_is_delivered_to_the_active_run_and_closed_before_finish() {
    let steering = Arc::new(super::steering::Steering::default());
    let provider = FakeProvider::new(vec![Turn::text("first"), Turn::text("second")]);
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        MemorySession::new(),
        Options {
            steering: Arc::clone(&steering),
            ..options()
        },
    );
    let submitted = Arc::new(AtomicUsize::new(0));
    let submit = Arc::clone(&submitted);
    let mut ticket = None;
    agent
        .run(
            "hello",
            &mut |event| match event {
                Event::ProviderApiCall { .. } if submit.fetch_add(1, Ordering::SeqCst) == 0 => {
                    let current = steering.ticket().expect("active run ticket");
                    ticket = Some(current);
                    steering
                        .submit_for(current, "change course")
                        .expect("steer active run");
                    steering
                        .submit_for(current, "and check the result")
                        .expect("second steer active run");
                }
                Event::AgentFinished => {
                    assert_eq!(steering.ticket(), None, "completion closes acceptance");
                    assert_eq!(
                        steering.submit_for(ticket.expect("captured ticket"), "too late"),
                        Err(super::steering::SubmitError::Inactive)
                    );
                }
                _ => {}
            },
            &CancellationToken::new(),
        )
        .await
        .expect("run");

    let messages = agent.session().messages();
    assert_eq!(messages[0].text(), "hello");
    let guidance: Vec<_> = messages
        .iter()
        .filter(|message| message.role == Role::User && message.text() != "hello")
        .map(|message| message.text())
        .collect();
    assert_eq!(guidance, ["change course", "and check the result"]);
    let requests = agent.provider().normal_requests();
    assert_eq!(requests.len(), 2);
    let request_guidance: Vec<_> = requests[1]
        .messages
        .iter()
        .filter(|message| message.role == Role::User && message.text() != "hello")
        .map(|message| message.text())
        .collect();
    assert_eq!(request_guidance, ["change course", "and check the result"]);
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn steering_waits_until_the_full_tool_batch_finishes() {
    let steering = Arc::new(super::steering::Steering::default());
    let agent = Agent::new(
        FakeProvider::new(vec![two_tool_calls(), Turn::text("done")]),
        EchoExecutor::default(),
        MemorySession::new(),
        Options {
            steering: Arc::clone(&steering),
            ..options()
        },
    );
    let submitted = AtomicUsize::new(0);
    agent
        .run(
            "hello",
            &mut |event| {
                if matches!(event, Event::ToolCallStarted { .. })
                    && submitted.fetch_add(1, Ordering::SeqCst) == 0
                {
                    steering
                        .submit("change course")
                        .expect("active run accepts guidance");
                }
            },
            &CancellationToken::new(),
        )
        .await
        .expect("run");

    let messages = agent.session().messages();
    let guidance = messages
        .iter()
        .position(|message| message.role == Role::User && message.text() == "change course")
        .expect("guidance persisted");
    let second_result = messages
        .iter()
        .position(|message| message.role == Role::Tool && message.blocks[0].tool_call_id == "c2")
        .expect("second tool result persisted");
    assert!(guidance > second_result, "guidance split the tool batch");
    let requests = agent.provider().normal_requests();
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|message| { message.role == Role::User && message.text() == "change course" })
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn cancellation_discards_undelivered_steering() {
    let steering = Arc::new(super::steering::Steering::default());
    let cancel = CancellationToken::new();
    let agent = Agent::new(
        FakeProvider::new(vec![two_tool_calls()]),
        CancellingExecutor {
            cancel: cancel.clone(),
        },
        MemorySession::new(),
        Options {
            steering: Arc::clone(&steering),
            ..options()
        },
    );
    let submitted = AtomicUsize::new(0);
    let error = agent
        .run(
            "hello",
            &mut |event| {
                if matches!(event, Event::ToolCallStarted { .. })
                    && submitted.fetch_add(1, Ordering::SeqCst) == 0
                {
                    steering.submit("do not leak").expect("active run");
                }
            },
            &cancel,
        )
        .await
        .expect_err("tool cancellation stops the run");
    assert!(error.is_cancelled());
    assert_eq!(steering.ticket(), None);
    assert!(
        !agent
            .session()
            .messages()
            .iter()
            .any(|message| message.text() == "do not leak")
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn followup_request_is_context_not_user_authority() {
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::text("done")]),
        EchoExecutor::default(),
        MemorySession::new(),
        options(),
    );
    agent
        .run_followup_with_system_context_control(
            "continue the analysis",
            "",
            &mut |_| {},
            &CancellationToken::new(),
        )
        .await
        .expect("follow-up");

    let messages = agent.session().messages();
    assert_eq!(messages[0].role, Role::Context);
    assert_eq!(messages[0].context_type, "agent_followup");
    assert!(messages[0].display);
    assert_eq!(
        messages[0].text(),
        "[Agent follow-up]\ncontinue the analysis"
    );
    let request = &agent.provider().normal_requests()[0];
    assert!(
        request
            .messages
            .iter()
            .any(|message| { message.text() == "[Agent follow-up]\ncontinue the analysis" })
    );
}

// -- inbox and wake turns --------------------------------------------------

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn pending_notifications_are_delivered_before_the_user_message() {
    let inbox = Arc::new(Inbox::default());
    inbox.push(Notification {
        task_id: "t1".into(),
        kind: Some(NotificationKind::TaskFinished),
        text: "child finished".into(),
        usage: Some(Usage {
            input_tokens: 2,
            output_tokens: 1,
            cached_input_tokens: 0,
        }),
    });
    let provider = FakeProvider::new(vec![Turn::text("ok")]);
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        MemorySession::new(),
        Options {
            mailbox: inbox.clone(),
            ..options()
        },
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    agent
        .run(
            "hello",
            &mut |event| sink.lock().expect("events").push(event),
            &CancellationToken::new(),
        )
        .await
        .expect("run");

    // A notification queued before the turn started is delivered before the
    // user's own message is appended: the provider sees it as something the
    // user's prompt responds to, not the other way around.
    let messages = agent.session().messages();
    assert_eq!(messages[0].role, Role::Context);
    assert_eq!(messages[0].context_type, "task_notification");
    assert_eq!(messages[0].text(), "child finished");
    assert!(messages[0].display);
    assert_eq!(
        messages[0]
            .context_metadata
            .as_ref()
            .expect("metadata")
            .task_id,
        "t1"
    );
    assert_eq!(messages[1].role, Role::User);
    assert!(inbox.is_empty(), "the inbox was not drained");
    assert!(
        names(&collect(&events)).contains(&"notification"),
        "no notification event was emitted"
    );
    // The notification reaches the provider on the very first request.
    assert_eq!(
        agent.provider().normal_requests()[0].messages.len(),
        2,
        "the notification was not in the first request"
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn the_inbox_is_drained_again_before_the_next_provider_request() {
    let inbox = Arc::new(Inbox::default());
    let provider = FakeProvider::new(vec![
        Turn::tool_call("c1", r#"{"value":1}"#),
        Turn::text("ok"),
    ]);
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        MemorySession::new(),
        Options {
            mailbox: inbox.clone(),
            ..options()
        },
    );
    // Pushed while the turn is set up; it must be delivered between the tool
    // result and the second provider call.
    inbox.push(Notification {
        task_id: "t2".into(),
        kind: Some(NotificationKind::Message),
        text: "from the parent".into(),
        usage: None,
    });
    agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("run");

    let messages = agent.session().messages();
    let contexts: Vec<&Message> = messages
        .iter()
        .filter(|message| message.role == Role::Context)
        .collect();
    assert_eq!(contexts.len(), 1);
    assert_eq!(contexts[0].context_type, "parent_message");
    let requests = agent.provider().normal_requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|message| message.text() == "from the parent"),
        "the notification did not reach the second request"
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn user_input_queued_during_a_provider_call_continues_the_same_turn() {
    let inbox = Arc::new(Inbox::default());
    let provider = FakeProvider::new(vec![Turn::text("first"), Turn::text("second")]);
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        MemorySession::new(),
        Options {
            mailbox: inbox.clone(),
            ..options()
        },
    );
    let queued = AtomicUsize::new(0);
    agent
        .run(
            "start",
            &mut |event| {
                if matches!(event, Event::TextDelta { .. })
                    && queued.fetch_add(1, Ordering::SeqCst) == 0
                {
                    inbox.push(Notification {
                        kind: Some(NotificationKind::UserMessage),
                        text: "change course".into(),
                        ..Notification::default()
                    });
                }
            },
            &CancellationToken::new(),
        )
        .await
        .expect("run");

    let messages = agent.session().messages();
    assert_eq!(
        messages
            .iter()
            .map(|message| &message.role)
            .collect::<Vec<_>>(),
        [&Role::User, &Role::Assistant, &Role::User, &Role::Assistant]
    );
    assert_eq!(messages[2].text(), "change course");
    assert_eq!(agent.provider().normal_requests().len(), 2);
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn empty_text_without_notifications_fails() {
    let provider = FakeProvider::new(Vec::new());
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        MemorySession::new(),
        options(),
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let error = agent
        .run(
            "  \t\n",
            &mut |event| sink.lock().expect("e").push(event),
            &CancellationToken::new(),
        )
        .await
        .expect_err("empty text is rejected");
    assert!(error.is_empty_user_text(), "unexpected error: {error}");
    assert_eq!(names(&collect(&events)), ["agent_error"]);
    assert!(agent.provider().requests().is_empty());
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn empty_text_with_a_notification_is_a_wake_turn() {
    let inbox = Arc::new(Inbox::default());
    inbox.push(Notification {
        task_id: "t3".into(),
        kind: Some(NotificationKind::TaskReport),
        text: "report".into(),
        usage: None,
    });
    let provider = FakeProvider::new(vec![Turn::text("ok")]);
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        MemorySession::new(),
        Options {
            mailbox: inbox.clone(),
            ..options()
        },
    );
    agent
        .run("", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("a wake turn runs");
    let messages = agent.session().messages();
    assert_eq!(messages[0].role, Role::Context, "no user message is stored");
    assert_eq!(messages[0].context_type, "task_notification");
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_notification_without_a_generated_task_id_still_persists() {
    let inbox = Arc::new(Inbox::default());
    inbox.push(Notification {
        task_id: String::new(),
        kind: Some(NotificationKind::Message),
        text: "[feishu] hello".into(),
        usage: None,
    });
    inbox.push(Notification {
        task_id: "timer".into(),
        text: "[timer] due".into(),
        usage: None,
        kind: None,
    });
    let provider = FakeProvider::new(vec![Turn::text("ok")]);
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        MemorySession::new(),
        Options {
            mailbox: inbox.clone(),
            ..options()
        },
    );
    agent
        .run("", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("a wake turn runs");
    let contexts: Vec<_> = agent
        .session()
        .messages()
        .into_iter()
        .filter(|message| message.role == Role::Context)
        .collect();
    assert_eq!(contexts.len(), 2);
    assert!(
        contexts
            .iter()
            .all(|message| message.context_metadata.is_none()),
        "ungenerated task ids must not become context metadata: {contexts:?}"
    );
    assert_eq!(contexts[0].context_type, "parent_message");
    assert_eq!(contexts[1].context_type, "task_notification");
}

// -- memory ----------------------------------------------------------------

fn one_record() -> Vec<Record> {
    vec![Record {
        id: "rec-1".into(),
        scope: Scope {
            namespace: "user".into(),
            id: "u1".into(),
        },
        kind: "preference".into(),
        key: "editor".into(),
        text: "prefers vim".into(),
        standing: false,
    }]
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn the_rendered_memory_context_is_prepended_without_being_persisted() {
    let provider = FakeProvider::new(vec![Turn::text("ok")]);
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        MemorySession::new(),
        Options {
            memory: Some(Arc::new(FakeMemory::with_records(one_record()))),
            ..options()
        },
    );
    agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("run");
    let request = &agent.provider().normal_requests()[0];
    assert_eq!(request.messages[0].role, Role::User);
    assert!(request.messages[0].text().contains("prefers vim"));
    assert!(request.messages[0].text().contains("untrusted"));
    for message in agent.session().messages() {
        assert!(
            !message.text().contains("prefers vim"),
            "the memory block was persisted"
        );
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn memory_context_keeps_existing_history_as_the_request_prefix() {
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::text("first reply"), Turn::text("second reply")]),
        EchoExecutor::default(),
        MemorySession::new(),
        Options {
            memory: Some(Arc::new(FakeMemory::with_records(one_record()))),
            ..options()
        },
    );
    let cancel = CancellationToken::new();
    agent.run("first", &mut |_| {}, &cancel).await.expect("run");
    agent
        .run("second", &mut |_| {}, &cancel)
        .await
        .expect("run");

    let requests = agent.provider().normal_requests();
    let messages = &requests[1].messages;
    assert_eq!(messages[0].text(), "first");
    assert_eq!(messages[1].text(), "first reply");
    assert!(messages[2].text().contains("prefers vim"));
    assert_eq!(messages[3].text(), "second");
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn per_run_system_context_is_sent_for_one_turn_only() {
    let agent = Agent::new(
        FakeProvider::new(vec![
            Turn::tool_call("c1", r#"{"value":1}"#),
            Turn::text("first done"),
            Turn::text("second done"),
        ]),
        EchoExecutor::default(),
        MemorySession::new(),
        options(),
    );
    let cancel = CancellationToken::new();
    agent
        .run_with_system_context("first", "transient reflection notes", &mut |_| {}, &cancel)
        .await
        .expect("first run");
    agent
        .run("second", &mut |_| {}, &cancel)
        .await
        .expect("second run");

    let requests = agent.provider().normal_requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(
        requests[0].system_prompt,
        "be brief\n\ntransient reflection notes"
    );
    assert_eq!(
        requests[1].system_prompt,
        "be brief\n\ntransient reflection notes"
    );
    assert_eq!(requests[2].system_prompt, "be brief");
    for message in agent.session().messages() {
        assert!(
            !message.text().contains("transient reflection notes"),
            "per-run system context was persisted"
        );
    }
    let report = agent.context_report();
    let prompt = report
        .sections
        .iter()
        .find(|section| section.kind == SectionKind::SystemPrompt)
        .expect("system prompt section");
    assert_eq!(prompt.items[0].text, "be brief");
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn per_run_system_context_is_sent_on_overflow_retry() {
    let provider = FakeProvider::new(vec![
        Turn::overflow(),
        Turn::summary(&structured_summary()),
        Turn::text("done"),
    ]);
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        seeded_session().await,
        Options {
            compaction: CompactionSettings {
                auto: true,
                ..CompactionSettings::default()
            },
            ..options()
        },
    );
    agent
        .run_with_system_context(
            "hello",
            "retry-only transient context",
            &mut |_| {},
            &CancellationToken::new(),
        )
        .await
        .expect("run");

    let normal_requests = agent.provider().normal_requests();
    assert_eq!(normal_requests.len(), 2);
    assert!(
        normal_requests
            .iter()
            .all(|request| request.system_prompt == "be brief\n\nretry-only transient context")
    );
    let summary_requests = agent.provider().summary_requests();
    assert_eq!(summary_requests.len(), 1);
    assert!(
        !summary_requests[0]
            .system_prompt
            .contains("retry-only transient context")
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn recall_runs_once_across_the_tool_loop() {
    let memory = Arc::new(FakeMemory::with_records(one_record()));
    let provider = FakeProvider::new(vec![
        Turn::tool_call("c1", r#"{"value":1}"#),
        Turn::text("ok"),
    ]);
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        MemorySession::new(),
        Options {
            memory: Some(memory.clone()),
            ..options()
        },
    );
    agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("run");
    assert_eq!(memory.calls.load(Ordering::SeqCst), 1);
    for request in agent.provider().normal_requests() {
        assert!(
            request.messages[0].text().contains("prefers vim"),
            "the memory block is missing from a later request"
        );
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_failed_recall_warns_and_the_turn_continues() {
    let provider = FakeProvider::new(vec![Turn::text("ok")]);
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        MemorySession::new(),
        Options {
            memory: Some(Arc::new(FakeMemory::failing("store unavailable"))),
            ..options()
        },
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    agent
        .run(
            "hello",
            &mut |event| sink.lock().expect("e").push(event),
            &CancellationToken::new(),
        )
        .await
        .expect("the turn continues");
    assert!(collect(&events).contains(&Event::MemoryWarning {
        message: "store unavailable".into()
    }));
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn close_reports_the_memory_binding_error() {
    let memory = Arc::new(FakeMemory {
        close_error: Some("close failed".into()),
        ..FakeMemory::with_records(Vec::new())
    });
    let agent = Agent::new(
        FakeProvider::new(Vec::new()),
        EchoExecutor::default(),
        MemorySession::new(),
        Options {
            memory: Some(memory),
            ..options()
        },
    );
    assert_eq!(
        agent.close().expect_err("close fails").to_string(),
        "close failed"
    );

    let plain = Agent::new(
        FakeProvider::new(Vec::new()),
        EchoExecutor::default(),
        MemorySession::new(),
        options(),
    );
    assert!(plain.close().is_ok(), "close without a binding is a no-op");
}

// -- tool-result overlay ---------------------------------------------------

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn the_placeholder_is_persisted_and_the_full_text_reaches_the_provider() {
    let provider = FakeProvider::new(vec![
        Turn::tool_call("c1", r#"{"value":1}"#),
        Turn::text("ok"),
    ]);
    let agent = Agent::new(
        provider,
        EchoExecutor {
            content: Some("the whole file".into()),
            persisted: Some("[stored elsewhere]".into()),
        },
        MemorySession::new(),
        options(),
    );
    agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("run");

    let stored = agent
        .session()
        .messages()
        .into_iter()
        .find(|message| message.role == Role::Tool)
        .expect("a tool result");
    assert_eq!(stored.blocks[0].text, "[stored elsewhere]");
    let second = &agent.provider().normal_requests()[1];
    let overlaid = second
        .messages
        .iter()
        .find(|message| message.role == Role::Tool)
        .expect("a tool result in the request");
    assert_eq!(overlaid.blocks[0].text, "the whole file");
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn the_overlay_does_not_survive_the_turn() {
    let provider = FakeProvider::new(vec![
        Turn::tool_call("c1", r#"{"value":1}"#),
        Turn::text("first"),
        Turn::text("second"),
    ]);
    let agent = Agent::new(
        provider,
        EchoExecutor {
            content: Some("the whole file".into()),
            persisted: Some("[stored elsewhere]".into()),
        },
        MemorySession::new(),
        options(),
    );
    let cancel = CancellationToken::new();
    agent.run("first", &mut |_| {}, &cancel).await.expect("run");
    agent
        .run("second", &mut |_| {}, &cancel)
        .await
        .expect("run");

    let last = agent.provider().normal_requests().pop().expect("a request");
    let tool = last
        .messages
        .iter()
        .find(|message| message.role == Role::Tool)
        .expect("a tool result");
    assert_eq!(
        tool.blocks[0].text, "[stored elsewhere]",
        "the overlay leaked into the next turn"
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn an_empty_persisted_override_still_overlays_the_live_text() {
    let provider = FakeProvider::new(vec![
        Turn::tool_call("c1", r#"{"value":1}"#),
        Turn::text("ok"),
    ]);
    let agent = Agent::new(
        provider,
        EchoExecutor {
            content: Some("shown".into()),
            persisted: Some(String::new()),
        },
        MemorySession::new(),
        options(),
    );
    agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("run");
    let stored = agent
        .session()
        .messages()
        .into_iter()
        .find(|message| message.role == Role::Tool)
        .expect("a tool result");
    assert_eq!(stored.blocks[0].text, "");
    let second = &agent.provider().normal_requests()[1];
    let overlaid = second
        .messages
        .iter()
        .find(|message| message.role == Role::Tool)
        .expect("a tool result in the request");
    assert_eq!(overlaid.blocks[0].text, "shown");
}

// -- redaction -------------------------------------------------------------

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_credential_is_redacted_from_events_persistence_and_history() {
    const SECRET: &str = "sk-live-abcdef";
    let provider = FakeProvider::new(vec![
        Turn::tool_call("c1", &format!(r#"{{"token":"{SECRET}"}}"#)),
        Turn::text("done"),
    ]);
    let agent = Agent::with_redactor(
        provider,
        EchoExecutor {
            content: Some(format!("saw {SECRET}")),
            persisted: None,
        },
        MemorySession::new(),
        options(),
        Redactor::new(&[SECRET.to_owned()]),
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    agent
        .run(
            &format!("use {SECRET} please"),
            &mut |event| sink.lock().expect("e").push(event),
            &CancellationToken::new(),
        )
        .await
        .expect("run");

    for event in collect(&events) {
        assert!(
            !format!("{event:?}").contains(SECRET),
            "the credential reached an event: {event:?}"
        );
    }
    for message in agent.session().messages() {
        assert!(
            !format!("{message:?}").contains(SECRET),
            "the credential was persisted: {message:?}"
        );
    }
    for request in agent.provider().requests() {
        assert!(
            !format!("{:?}", request.messages).contains(SECRET),
            "the credential went back to the provider"
        );
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_provider_retry_is_forwarded_before_the_reply() {
    let mut turn = Turn::text("ok");
    turn.events.insert(
        0,
        StreamEvent::Retry {
            attempt: 2,
            max_attempts: 3,
            delay: std::time::Duration::from_millis(250),
            reason: "HTTP 503".into(),
        },
    );
    let agent = Agent::new(
        FakeProvider::new(vec![turn]),
        EchoExecutor {
            content: None,
            persisted: None,
        },
        MemorySession::new(),
        options(),
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    agent
        .run(
            "hi",
            &mut |event| sink.lock().expect("e").push(event),
            &CancellationToken::new(),
        )
        .await
        .expect("run");

    let events = collect(&events);
    let retry = events
        .iter()
        .position(|event| {
            matches!(
                event,
                Event::ProviderRetry {
                    operation_id,
                    attempt: 2,
                    max_attempts: 3,
                    delay,
                    reason,
                } if !operation_id.as_str().is_empty()
                    && *delay == std::time::Duration::from_millis(250)
                    && reason == "HTTP 503"
            )
        })
        .expect("retry forwarded");
    let text = events
        .iter()
        .position(|event| matches!(event, Event::TextDelta { .. }))
        .expect("text");
    assert!(retry < text);
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn reasoning_is_streamed_redacted_and_persisted() {
    const SECRET: &str = "sk-live-abcdef";
    let reasoning = format!("use {SECRET} then");
    let provider = FakeProvider::new(vec![
        Turn {
            events: vec![
                StreamEvent::ReasoningDelta {
                    text: "use sk-live".into(),
                },
                StreamEvent::ReasoningDelta {
                    text: "-abcdef then".into(),
                },
                StreamEvent::TextDelta { text: "ok".into() },
            ],
            outcome: Ok(Response {
                message: Message {
                    role: Role::Assistant,
                    finish_reason: Some(FinishReason::Stop),
                    blocks: vec![Block::reasoning(reasoning), Block::text("ok")],
                    ..Message::default()
                },
            }),
        },
        Turn::text("second"),
    ]);
    let agent = Agent::with_redactor(
        provider,
        EchoExecutor {
            content: None,
            persisted: None,
        },
        MemorySession::new(),
        options(),
        Redactor::new(&[SECRET.to_owned()]),
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    for prompt in ["first", "again"] {
        agent
            .run(
                prompt,
                &mut |event| sink.lock().expect("e").push(event),
                &CancellationToken::new(),
            )
            .await
            .expect("run");
    }

    let streamed: Vec<Event> = collect(&events)
        .into_iter()
        .filter(|event| {
            matches!(
                event,
                Event::ReasoningDelta { .. } | Event::TextDelta { .. }
            )
        })
        .collect();
    let reasoning_text: String = streamed
        .iter()
        .take_while(|event| matches!(event, Event::ReasoningDelta { .. }))
        .map(|event| match event {
            Event::ReasoningDelta { text } => text.as_str(),
            _ => "",
        })
        .collect();
    let redacted = format!("use {} then", super::redactor::REDACTION_MARKER);
    assert_eq!(reasoning_text, redacted);
    assert!(matches!(&streamed[streamed.len() - 2], Event::TextDelta { text } if text == "ok"));

    let first_reply = &agent.session().messages()[1];
    assert_eq!(first_reply.blocks[0], Block::reasoning(redacted));
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn an_incomplete_redactor_runs_no_provider_tool_or_session_call() {
    let provider = FakeProvider::new(vec![Turn::text("never sent")]);
    let agent = Agent::with_redactor(
        provider,
        EchoExecutor::default(),
        MemorySession::new(),
        options(),
        Redactor::with_completeness(&["secret".to_owned()], false),
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    agent
        .run(
            "hello",
            &mut |event| sink.lock().expect("e").push(event),
            &CancellationToken::new(),
        )
        .await
        .expect("the turn ends without doing anything");
    assert_eq!(
        names(&collect(&events)),
        ["agent_started", "agent_finished"]
    );
    assert!(agent.provider().requests().is_empty());
    assert!(agent.session().messages().is_empty());
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn an_incomplete_redactor_still_reports_cancellation() {
    let agent = Agent::with_redactor(
        FakeProvider::new(Vec::new()),
        EchoExecutor::default(),
        MemorySession::new(),
        options(),
        Redactor::with_completeness(&["secret".to_owned()], false),
    );
    let cancel = CancellationToken::new();
    cancel.cancel();
    let error = agent
        .run("hello", &mut |_| {}, &cancel)
        .await
        .expect_err("cancellation is reported");
    assert!(error.is_cancelled(), "unexpected error: {error}");
    assert!(agent.session().messages().is_empty());
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_secret_in_the_configuration_downgrades_the_redactor() {
    let agent = Agent::with_redactor(
        FakeProvider::new(vec![Turn::text("never sent")]),
        EchoExecutor::default(),
        MemorySession::new(),
        Options {
            system_prompt: "the key is sk-live-abcdef".into(),
            ..options()
        },
        Redactor::new(&["sk-live-abcdef".to_owned()]),
    );
    agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("the turn ends without doing anything");
    assert!(
        agent.provider().requests().is_empty(),
        "a downgraded redactor still let a request through"
    );
}

// -- automatic compaction --------------------------------------------------

/// Windows small enough that a short transcript crosses the soft trigger.
fn tight_compaction() -> CompactionSettings {
    CompactionSettings {
        auto: true,
        // Large enough that the summary request, which carries the full
        // summarization system prompt, still fits under the hard budget.
        hard_input_window: 4_000,
        working_window: 150,
        reserve_tokens: 0,
        keep_recent_tokens: 0,
    }
}

/// A session with three turns, enough for a compaction boundary to exist.
async fn seeded_session() -> MemorySession {
    let session = MemorySession::new();
    for index in 0..3 {
        session
            .append(Message {
                id: format!("u{index}"),
                role: Role::User,
                blocks: vec![Block::text("x".repeat(80))],
                ..Message::default()
            })
            .await
            .expect("append");
        session
            .append(Message {
                id: format!("a{index}"),
                role: Role::Assistant,
                blocks: vec![Block::text("y".repeat(80))],
                ..Message::default()
            })
            .await
            .expect("append");
    }
    session
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_request_under_the_soft_trigger_is_not_compacted() {
    let provider = FakeProvider::new(vec![Turn::text("ok")]);
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        MemorySession::new(),
        Options {
            compaction: CompactionSettings {
                auto: true,
                hard_input_window: 1_000_000,
                working_window: 900_000,
                reserve_tokens: 16_384,
                keep_recent_tokens: 20_000,
            },
            ..options()
        },
    );
    agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("run");
    assert!(agent.provider().summary_requests().is_empty());
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn crossing_the_soft_trigger_compacts_before_the_request() {
    let provider = FakeProvider::new(vec![Turn::summary(&structured_summary()), Turn::text("ok")]);
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        seeded_session().await,
        Options {
            compaction: tight_compaction(),
            ..options()
        },
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    agent
        .run(
            "hello",
            &mut |event| sink.lock().expect("e").push(event),
            &CancellationToken::new(),
        )
        .await
        .expect("run");
    assert_eq!(agent.provider().summary_requests().len(), 1);
    let emitted = names(&collect(&events));
    assert!(emitted.contains(&"compaction_started"));
    assert!(emitted.contains(&"compaction_planned"));
    assert!(emitted.contains(&"compaction_completed"));
    assert!(
        agent
            .session()
            .latest_compaction()
            .is_some_and(|latest| !latest.id.is_empty()),
        "no checkpoint was written"
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn proactive_compaction_is_attempted_once_per_turn() {
    // Both provider dispatches sit above the soft trigger, but only the first
    // one may compact.
    let provider = FakeProvider::new(vec![
        Turn::summary(&structured_summary()),
        Turn::tool_call("c1", r#"{"value":1}"#),
        Turn::text("ok"),
    ]);
    let agent = Agent::new(
        provider,
        EchoExecutor {
            content: Some("z".repeat(1_200)),
            persisted: None,
        },
        seeded_session().await,
        Options {
            compaction: tight_compaction(),
            ..options()
        },
    );
    agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("run");
    assert_eq!(agent.provider().summary_requests().len(), 1);
    assert_eq!(agent.provider().normal_requests().len(), 2);
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_soft_compaction_failure_warns_and_sends_the_original_request() {
    let provider = FakeProvider::new(vec![Turn::failure("summary call failed"), Turn::text("ok")]);
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        seeded_session().await,
        Options {
            compaction: tight_compaction(),
            ..options()
        },
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    agent
        .run(
            "hello",
            &mut |event| sink.lock().expect("e").push(event),
            &CancellationToken::new(),
        )
        .await
        .expect("the turn continues");
    assert!(collect(&events).contains(&Event::CompactionWarning {
        message: super::overflow::AUTOMATIC_COMPACTION_WARNING_MESSAGE.into()
    }));
    assert_eq!(agent.provider().normal_requests().len(), 1);
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_compaction_failure_at_the_hard_limit_stops_the_turn() {
    let provider = FakeProvider::new(vec![Turn::failure("summary call failed")]);
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        seeded_session().await,
        Options {
            // The hard trigger is below the transcript, so a failed
            // compaction stops the dispatch instead of warning.
            compaction: CompactionSettings {
                hard_input_window: 150,
                ..tight_compaction()
            },
            ..options()
        },
    );
    let error = agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect_err("the turn stops");
    assert_eq!(
        error.to_string(),
        super::overflow::AUTOMATIC_COMPACTION_HARD_FAILURE_MESSAGE
    );
    assert!(agent.provider().normal_requests().is_empty());
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn unknown_limits_never_compact_proactively() {
    let provider = FakeProvider::new(vec![Turn::text("ok")]);
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        seeded_session().await,
        Options {
            compaction: CompactionSettings {
                auto: true,
                ..CompactionSettings::default()
            },
            ..options()
        },
    );
    agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("run");
    assert!(agent.provider().summary_requests().is_empty());
}

// -- overflow recovery -----------------------------------------------------

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_context_overflow_is_retried_once_after_compaction() {
    let provider = FakeProvider::new(vec![
        Turn::overflow(),
        Turn::summary(&structured_summary()),
        Turn::text("ok"),
    ]);
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        seeded_session().await,
        Options {
            compaction: CompactionSettings {
                auto: true,
                ..CompactionSettings::default()
            },
            ..options()
        },
    );
    agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("the retry succeeds");
    assert_eq!(agent.provider().summary_requests().len(), 1);
    assert_eq!(agent.provider().normal_requests().len(), 2);
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn an_overflow_after_visible_text_is_not_retried() {
    let provider = FakeProvider::new(vec![Turn::overflow().with_events(vec![
        StreamEvent::TextDelta {
            text: "partial answer".into(),
        },
    ])]);
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        seeded_session().await,
        Options {
            compaction: CompactionSettings {
                auto: true,
                ..CompactionSettings::default()
            },
            ..options()
        },
    );
    let error = agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect_err("the overflow is reported");
    assert!(error.to_string().contains("context window exceeded"));
    assert!(agent.provider().summary_requests().is_empty());
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn an_overflow_after_visible_reasoning_is_not_retried() {
    assert_overflow_after_visible_delta_is_not_retried(StreamEvent::ReasoningDelta {
        text: "visible reasoning that is long enough to clear redaction buffering".into(),
    })
    .await;
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn an_overflow_after_a_tool_call_delta_is_not_retried() {
    assert_overflow_after_visible_delta_is_not_retried(StreamEvent::ToolCallDelta {
        tool_call_id: "call-1".into(),
        tool_name: "echo".into(),
        arguments: r#"{"value":"partial"}"#.into(),
    })
    .await;
}

async fn assert_overflow_after_visible_delta_is_not_retried(event: StreamEvent) {
    let provider = FakeProvider::new(vec![Turn::overflow().with_events(vec![event])]);
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        seeded_session().await,
        Options {
            compaction: CompactionSettings {
                auto: true,
                ..CompactionSettings::default()
            },
            ..options()
        },
    );
    let error = agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect_err("the overflow is reported");
    assert!(error.to_string().contains("context window exceeded"));
    assert!(agent.provider().summary_requests().is_empty());
    assert_eq!(agent.provider().normal_requests().len(), 1);
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_second_overflow_after_the_retry_stops_the_turn() {
    let provider = FakeProvider::new(vec![
        Turn::overflow(),
        Turn::summary(&structured_summary()),
        Turn::overflow(),
    ]);
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        seeded_session().await,
        Options {
            compaction: CompactionSettings {
                auto: true,
                ..CompactionSettings::default()
            },
            ..options()
        },
    );
    let error = agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect_err("the turn stops");
    assert_eq!(
        error.to_string(),
        super::overflow::OVERFLOW_RETRY_FAILURE_MESSAGE
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn an_overflow_with_nothing_to_compact_returns_the_original_error() {
    let provider = FakeProvider::new(vec![Turn::overflow()]);
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        seeded_session().await,
        Options {
            compaction: CompactionSettings {
                auto: true,
                // Everything is inside the retained window, so there is no
                // prefix left to summarize.
                keep_recent_tokens: 1_000_000,
                ..CompactionSettings::default()
            },
            ..options()
        },
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let error = agent
        .run(
            "hello",
            &mut |event| sink.lock().expect("e").push(event),
            &CancellationToken::new(),
        )
        .await
        .expect_err("the overflow is reported");
    assert!(
        error.to_string().contains("context window exceeded"),
        "{error:?}"
    );
    let completed = collect(&events)
        .into_iter()
        .find_map(|event| match event {
            Event::CompactionCompleted { compaction } => Some(compaction),
            _ => None,
        })
        .expect("a completion event");
    assert!(completed.noop, "the no-op completion was not reported");
    assert!(agent.provider().summary_requests().is_empty());
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn automatic_compaction_disabled_skips_both_paths() {
    let provider = FakeProvider::new(vec![Turn::overflow()]);
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        seeded_session().await,
        Options {
            compaction: CompactionSettings {
                auto: false,
                hard_input_window: 200,
                working_window: 100,
                reserve_tokens: 40,
                keep_recent_tokens: 0,
            },
            ..options()
        },
    );
    let error = agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect_err("the overflow is reported");
    assert!(error.to_string().contains("context window exceeded"));
    assert!(agent.provider().summary_requests().is_empty());
}

// -- manual compaction -----------------------------------------------------

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_manual_compaction_persists_the_summary_and_emits_the_plan_first() {
    let mut turn = Turn::summary(&structured_summary());
    turn.events.push(StreamEvent::Retry {
        attempt: 2,
        max_attempts: 4,
        delay: std::time::Duration::from_secs(1),
        reason: "connection interrupted".into(),
    });
    let provider = FakeProvider::new(vec![turn]);
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        seeded_session().await,
        options(),
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let result = agent
        .compact(
            "focus on the parser",
            &mut |event| sink.lock().expect("e").push(event),
            &CancellationToken::new(),
        )
        .await
        .expect("compact");

    assert!(!result.checkpoint_id.is_empty());
    assert!(!result.noop);
    assert!(result.usage_present);
    assert_eq!(result.usage.output_tokens, 7);
    assert_eq!(
        names(&collect(&events)),
        [
            "compaction_started",
            "compaction_planned",
            "provider_retry",
            "provider_api_call",
            "compaction_completed"
        ]
    );
    let latest = agent.session().latest_compaction().expect("a checkpoint");
    assert!(latest.summary.contains("## Goal"));
    assert!(
        agent.provider().summary_requests()[0]
            .system_prompt
            .contains("focus on the parser"),
        "the focus was not added to the prompt"
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_manual_compaction_with_nothing_to_compact_is_a_bounded_no_op() {
    let provider = FakeProvider::new(Vec::new());
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        MemorySession::new(),
        options(),
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let result = agent
        .compact(
            "",
            &mut |event| sink.lock().expect("e").push(event),
            &CancellationToken::new(),
        )
        .await
        .expect("a no-op is not an error");
    assert!(result.noop);
    assert!(result.checkpoint_id.is_empty());
    assert_eq!(
        names(&collect(&events)),
        ["compaction_started", "compaction_completed"]
    );
    assert!(agent.provider().requests().is_empty());
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_summary_response_that_streams_past_its_bound_is_rejected() {
    let provider = FakeProvider::new(vec![Turn::summary(&structured_summary()).with_events(
        vec![StreamEvent::TextDelta {
            text: "x".repeat(200_000),
        }],
    )]);
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        seeded_session().await,
        options(),
    );
    let error = agent
        .compact("", &mut |_| {}, &CancellationToken::new())
        .await
        .expect_err("the stream is rejected");
    assert_eq!(
        error.to_string(),
        "invalid compaction summary: streamed response exceeded its bound or attempted a tool call"
    );
    assert!(agent.session().latest_compaction().is_none());
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_summary_missing_a_heading_is_rejected_before_the_append() {
    let provider = FakeProvider::new(vec![Turn::summary("## Goal\nonly one heading")]);
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        seeded_session().await,
        options(),
    );
    let error = agent
        .compact("", &mut |_| {}, &CancellationToken::new())
        .await
        .expect_err("the summary is rejected");
    assert!(
        error.is_invalid_compaction_summary(),
        "unexpected error: {error}"
    );
    assert!(agent.session().latest_compaction().is_none());
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn summarized_user_messages_are_carried_verbatim_across_compactions() {
    async fn exchange(session: &MemorySession, index: usize, text: &str) {
        for (id, role, text) in [
            (format!("u{index}"), Role::User, text),
            (format!("a{index}"), Role::Assistant, "ok"),
        ] {
            session
                .append(Message {
                    id,
                    role,
                    blocks: vec![Block::text(text)],
                    ..Message::default()
                })
                .await
                .expect("append");
        }
    }

    let session = MemorySession::new();
    exchange(&session, 0, "gpt-5.6 returns HTTP 400").await;
    exchange(&session, 1, "second").await;
    exchange(&session, 2, "third").await;
    let agent = Agent::new(
        FakeProvider::new(vec![
            Turn::summary(&structured_summary()),
            Turn::summary(&structured_summary()),
        ]),
        EchoExecutor::default(),
        session,
        options(),
    );
    agent
        .compact("", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("first compaction");
    exchange(agent.session(), 3, "fourth").await;
    agent
        .compact("", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("second compaction");

    let summary = agent
        .session()
        .latest_compaction()
        .expect("a checkpoint")
        .summary;
    assert!(
        summary.ends_with(
            "\n\n<user-messages>\n\"gpt-5.6 returns HTTP 400\"\n\"second\"\n\"third\"\n</user-messages>"
        ),
        "{summary}"
    );
    let second_request = agent.provider().summary_requests()[1].messages[0].text();
    assert!(
        !second_request.contains("gpt-5.6 returns HTTP 400")
            && !second_request.contains("user-messages"),
        "the carried block reached the summarizer: {second_request}"
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn cancellation_before_the_append_commits_nothing() {
    let provider = FakeProvider::new(vec![Turn::summary(&structured_summary())]);
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        seeded_session().await,
        options(),
    );
    let cancel = CancellationToken::new();
    cancel.cancel();
    let error = agent
        .compact("", &mut |_| {}, &cancel)
        .await
        .expect_err("cancellation stops the compaction");
    assert!(error.is_cancelled(), "unexpected error: {error}");
    assert!(agent.session().latest_compaction().is_none());
    assert!(agent.provider().requests().is_empty());
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_compaction_with_no_request_sizer_is_rejected() {
    let provider = FakeProvider::new(Vec::new());
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        seeded_session().await,
        Options {
            request_sizer: None,
            ..options()
        },
    );
    let error = agent
        .compact("", &mut |_| {}, &CancellationToken::new())
        .await
        .expect_err("sizing is required");
    assert_eq!(
        error.to_string(),
        "invalid compaction summary: invalid compaction summary request: request sizing is unavailable"
    );
    assert!(agent.provider().requests().is_empty());
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_compacted_context_still_over_the_hard_trigger_stops_the_dispatch() {
    // The system prompt alone is over the hard trigger, and compaction never
    // touches it, so the retry cannot bring the request back under.
    let provider = FakeProvider::new(vec![Turn::summary(&structured_summary())]);
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        seeded_session().await,
        Options {
            system_prompt: "p".repeat(90_000),
            compaction: CompactionSettings {
                auto: true,
                hard_input_window: 20_000,
                working_window: 150,
                reserve_tokens: 0,
                keep_recent_tokens: 0,
            },
            ..options()
        },
    );
    let error = agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect_err("the dispatch stops");
    assert_eq!(
        error.to_string(),
        super::overflow::AUTOMATIC_COMPACTION_STILL_TOO_LARGE_MESSAGE,
        "{error:?}"
    );
    assert_eq!(agent.provider().summary_requests().len(), 1);
    assert!(agent.provider().normal_requests().is_empty());
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_later_dispatch_over_the_hard_trigger_reports_the_used_attempt() {
    // The first dispatch spends the one proactive attempt. A very large tool
    // result then pushes the second dispatch past the hard trigger.
    let provider = FakeProvider::new(vec![
        Turn::summary(&structured_summary()),
        Turn::tool_call("c1", r#"{"value":1}"#),
    ]);
    let agent = Agent::new(
        provider,
        EchoExecutor {
            content: Some("z".repeat(60_000)),
            persisted: None,
        },
        seeded_session().await,
        Options {
            compaction: tight_compaction(),
            ..options()
        },
    );
    let error = agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect_err("the second dispatch stops");
    assert_eq!(
        error.to_string(),
        super::overflow::AUTOMATIC_COMPACTION_ATTEMPT_USED_MESSAGE
    );
    assert_eq!(agent.provider().summary_requests().len(), 1);
    assert_eq!(agent.provider().normal_requests().len(), 1);
}

/// Records whether the agent closed it.
#[derive(Default)]
struct CountingTasks {
    closes: AtomicUsize,
}

impl super::tasks::TaskRegistry for CountingTasks {
    fn close(&self) {
        self.closes.fetch_add(1, Ordering::SeqCst);
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn close_closes_the_task_registry() {
    let tasks = Arc::new(CountingTasks::default());
    let agent = Agent::new(
        FakeProvider::new(Vec::new()),
        EchoExecutor::default(),
        MemorySession::new(),
        Options {
            tasks: Some(tasks.clone()),
            ..options()
        },
    );
    agent.close().expect("close");
    assert_eq!(tasks.closes.load(Ordering::SeqCst), 1);
    assert!(
        agent.tasks().is_some(),
        "the registry is still reachable after close"
    );
}

// -- provider contract -----------------------------------------------------

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn text_deltas_and_usage_reach_the_event_sink() {
    let mut turn = Turn::text("hi");
    turn.events = vec![
        StreamEvent::TextDelta { text: "hi".into() },
        StreamEvent::TextDelta {
            text: " there".into(),
        },
    ];
    if let Ok(response) = &mut turn.outcome {
        response.message.usage = Some(Usage {
            input_tokens: 11,
            output_tokens: 3,
            cached_input_tokens: 2,
        });
    }
    let agent = Agent::new(
        FakeProvider::new(vec![turn]),
        EchoExecutor::default(),
        MemorySession::new(),
        options(),
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    agent
        .run(
            "hello",
            &mut |event| sink.lock().expect("e").push(event),
            &CancellationToken::new(),
        )
        .await
        .expect("run");
    let seen = collect(&events);
    assert_eq!(
        names(&seen),
        [
            "agent_started",
            "text_delta",
            "text_delta",
            "provider_api_call",
            "provider_usage",
            "agent_finished"
        ]
    );
    assert!(seen.contains(&Event::ProviderUsage {
        usage: Usage {
            input_tokens: 11,
            output_tokens: 3,
            cached_input_tokens: 2,
        },
        present: true,
    }));
    let Event::ProviderApiCall {
        provider,
        model,
        status,
        ..
    } = seen
        .iter()
        .find(|event| matches!(event, Event::ProviderApiCall { .. }))
        .expect("an api call event")
        .clone()
    else {
        unreachable!()
    };
    assert_eq!((provider.as_str(), model.as_str()), ("fake", "test-model"));
    assert_eq!(status, super::ApiStatus::Ok);
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_response_without_usage_still_emits_the_event_marked_absent() {
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::text("hi")]),
        EchoExecutor::default(),
        MemorySession::new(),
        options(),
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    agent
        .run(
            "hello",
            &mut |event| sink.lock().expect("e").push(event),
            &CancellationToken::new(),
        )
        .await
        .expect("run");
    // The event is always emitted, with the absence marked by `present`.
    assert!(collect(&events).contains(&Event::ProviderUsage {
        usage: Usage::default(),
        present: false,
    }));
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_negative_usage_count_is_rejected() {
    let mut turn = Turn::text("hi");
    if let Ok(response) = &mut turn.outcome {
        response.message.usage = Some(Usage {
            input_tokens: -1,
            output_tokens: 0,
            cached_input_tokens: 0,
        });
    }
    let agent = Agent::new(
        FakeProvider::new(vec![turn]),
        EchoExecutor::default(),
        MemorySession::new(),
        options(),
    );
    let error = agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect_err("the usage is rejected");
    assert!(
        error.to_string().starts_with("invalid provider response"),
        "{error}"
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn the_thinking_setting_is_sent_to_the_provider() {
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::text("hi")]),
        EchoExecutor::default(),
        MemorySession::new(),
        Options {
            thinking: "high".into(),
            ..options()
        },
    );
    agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("run");
    assert_eq!(agent.provider().requests()[0].thinking, "high");
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_provider_failure_ends_the_run_and_is_reported_once() {
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::failure("upstream refused")]),
        EchoExecutor::default(),
        MemorySession::new(),
        options(),
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let error = agent
        .run(
            "hello",
            &mut |event| sink.lock().expect("e").push(event),
            &CancellationToken::new(),
        )
        .await
        .expect_err("the failure is returned");
    assert!(error.to_string().contains("upstream refused"), "{error}");
    let seen = collect(&events);
    assert_eq!(
        seen.iter()
            .filter(|event| matches!(event, Event::AgentError { .. }))
            .count(),
        1
    );
    let Event::ProviderApiCall { status, .. } = seen
        .iter()
        .find(|event| matches!(event, Event::ProviderApiCall { .. }))
        .expect("an api call event")
        .clone()
    else {
        unreachable!()
    };
    assert_eq!(status, super::ApiStatus::Error);
}

// -- tool loop -------------------------------------------------------------

/// Returns two tool calls in one assistant message.
fn two_tool_calls() -> Turn {
    let call = |id: &str, value: i32| Block {
        block_type: BlockType::ToolCall,
        tool_call_id: id.into(),
        tool_name: "echo".into(),
        arguments: Some(raw(&format!(r#"{{"value":{value}}}"#))),
        ..Block::default()
    };
    Turn {
        events: Vec::new(),
        outcome: Ok(Response {
            message: Message {
                role: Role::Assistant,
                finish_reason: Some(FinishReason::ToolCalls),
                blocks: vec![call("c1", 1), call("c2", 2)],
                ..Message::default()
            },
        }),
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn several_tool_calls_run_in_order_and_emit_paired_events() {
    let agent = Agent::new(
        FakeProvider::new(vec![two_tool_calls(), Turn::text("done")]),
        EchoExecutor::default(),
        MemorySession::new(),
        options(),
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    agent
        .run(
            "hello",
            &mut |event| sink.lock().expect("e").push(event),
            &CancellationToken::new(),
        )
        .await
        .expect("run");
    let started: Vec<String> = collect(&events)
        .into_iter()
        .filter_map(|event| match event {
            Event::ToolCallStarted { tool_call_id, .. } => Some(tool_call_id),
            _ => None,
        })
        .collect();
    assert_eq!(started, ["c1", "c2"]);
    let finished = collect(&events)
        .into_iter()
        .filter(|event| matches!(event, Event::ToolCallFinished { .. }))
        .count();
    assert_eq!(finished, 2);
    let results: Vec<String> = agent
        .session()
        .messages()
        .into_iter()
        .filter(|message| message.role == Role::Tool)
        .map(|message| message.blocks[0].text.clone())
        .collect();
    assert_eq!(results, [r#"{"value":1}"#, r#"{"value":2}"#]);
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn an_unknown_tool_is_persisted_as_an_error_result() {
    let unknown = Turn {
        events: Vec::new(),
        outcome: Ok(Response {
            message: Message {
                role: Role::Assistant,
                finish_reason: Some(FinishReason::ToolCalls),
                blocks: vec![Block {
                    block_type: BlockType::ToolCall,
                    tool_call_id: "c1".into(),
                    tool_name: "missing".into(),
                    arguments: Some(raw("{}")),
                    ..Block::default()
                }],
                ..Message::default()
            },
        }),
    };
    let agent = Agent::new(
        FakeProvider::new(vec![unknown, Turn::text("done")]),
        EchoExecutor::default(),
        MemorySession::new(),
        options(),
    );
    agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("the run continues past an unknown tool");
    let result = agent
        .session()
        .messages()
        .into_iter()
        .find(|message| message.role == Role::Tool)
        .expect("a tool result");
    assert!(result.blocks[0].is_error);
    assert!(result.blocks[0].text.contains("missing"));
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn cancellation_persists_the_first_result_and_skips_the_rest() {
    let cancel = CancellationToken::new();
    let child = cancel.clone();
    let agent = Agent::new(
        FakeProvider::new(vec![two_tool_calls()]),
        CancellingExecutor { cancel: child },
        MemorySession::new(),
        options(),
    );
    let error = agent
        .run("hello", &mut |_| {}, &cancel)
        .await
        .expect_err("the run is cancelled");
    assert!(error.is_cancelled(), "{error}");
    let results: Vec<Message> = agent
        .session()
        .messages()
        .into_iter()
        .filter(|message| message.role == Role::Tool)
        .collect();
    assert_eq!(
        results.len(),
        2,
        "both tool calls need a persisted result to keep the transcript valid"
    );
    assert!(
        results[1].blocks[0].is_error,
        "the skipped call was not marked as an error"
    );
}

/// Cancels the run from inside the first tool call.
struct CancellingExecutor {
    cancel: CancellationToken,
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl ToolExecutor for CancellingExecutor {
    fn definitions(&self) -> Vec<ToolDefinition> {
        EchoExecutor::default().definitions()
    }

    async fn execute(
        &self,
        call: crate::tool::ToolCall<'_>,
        _control: &dyn OperationControl,
    ) -> crate::tool::ToolExecution {
        self.cancel.cancel();
        crate::tool::ToolExecution::completed(ToolResult {
            content: call.arguments.get().to_owned(),
            persisted_content: None,
            is_error: false,
            outcome_override: None,
        })
    }
}

// -- durable boundaries ----------------------------------------------------

/// Fails the nth append and passes the rest through.
struct FailingSession {
    inner: MemorySession,
    fail_at: usize,
    appends: AtomicUsize,
}

impl FailingSession {
    fn new(fail_at: usize) -> Self {
        Self {
            inner: MemorySession::new(),
            fail_at,
            appends: AtomicUsize::new(0),
        }
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl Session for FailingSession {
    fn messages(&self) -> Vec<Message> {
        self.inner.messages()
    }

    async fn append(&self, message: Message) -> Result<(), crate::session::SessionError> {
        if self.appends.fetch_add(1, Ordering::SeqCst) == self.fail_at {
            return Err(crate::session::SessionError::Persist("disk full".into()));
        }
        self.inner.append(message).await
    }

    fn latest_compaction(&self) -> Option<crate::session::CompactionMetadata> {
        self.inner.latest_compaction()
    }

    async fn append_compaction(
        &self,
        checkpoint: crate::session::CompactionCheckpoint,
    ) -> Result<crate::session::CompactionMetadata, crate::session::SessionError> {
        self.inner.append_compaction(checkpoint).await
    }

    async fn append_operation_fact(
        &self,
        fact: crate::session::OperationFact,
    ) -> Result<(), crate::session::SessionError> {
        self.inner.append_operation_fact(fact).await
    }

    fn observed_capabilities(&self) -> Option<crate::session::Capabilities> {
        self.inner.observed_capabilities()
    }

    fn interrupted_turn(&self) -> Option<crate::session::InterruptedTurn> {
        self.inner.interrupted_turn()
    }

    async fn append_custom(
        &self,
        custom_type: &str,
        data: &str,
    ) -> Result<(), crate::session::SessionError> {
        self.inner.append_custom(custom_type, data).await
    }
}

struct OperationFailingSession {
    inner: MemorySession,
    fail_at: usize,
    fact_appends: AtomicUsize,
}

impl OperationFailingSession {
    fn new(fail_at: usize) -> Self {
        Self {
            inner: MemorySession::new(),
            fail_at,
            fact_appends: AtomicUsize::new(0),
        }
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl Session for OperationFailingSession {
    fn messages(&self) -> Vec<Message> {
        self.inner.messages()
    }

    async fn append(&self, message: Message) -> Result<(), crate::session::SessionError> {
        self.inner.append(message).await
    }

    fn latest_compaction(&self) -> Option<crate::session::CompactionMetadata> {
        self.inner.latest_compaction()
    }

    async fn append_compaction(
        &self,
        checkpoint: crate::session::CompactionCheckpoint,
    ) -> Result<crate::session::CompactionMetadata, crate::session::SessionError> {
        self.inner.append_compaction(checkpoint).await
    }

    async fn append_operation_fact(
        &self,
        fact: crate::session::OperationFact,
    ) -> Result<(), crate::session::SessionError> {
        if self.fact_appends.fetch_add(1, Ordering::SeqCst) == self.fail_at {
            return Err(crate::session::SessionError::Persist(
                "operation disk full".into(),
            ));
        }
        self.inner.append_operation_fact(fact).await
    }

    fn operation_history(&self) -> Vec<crate::session::OperationFact> {
        self.inner.operation_history()
    }

    fn operation_ledger(&self) -> crate::session::OperationLedger {
        self.inner.operation_ledger()
    }

    fn observed_capabilities(&self) -> Option<crate::session::Capabilities> {
        self.inner.observed_capabilities()
    }

    fn interrupted_turn(&self) -> Option<crate::session::InterruptedTurn> {
        self.inner.interrupted_turn()
    }

    async fn append_custom(
        &self,
        custom_type: &str,
        data: &str,
    ) -> Result<(), crate::session::SessionError> {
        self.inner.append_custom(custom_type, data).await
    }
}

struct CountingExecutor {
    calls: Arc<AtomicUsize>,
}

struct AsyncOrderingSession {
    inner: MemorySession,
    attempt_persisted: Arc<AtomicBool>,
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl Session for AsyncOrderingSession {
    fn messages(&self) -> Vec<Message> {
        self.inner.messages()
    }
    async fn append(&self, message: Message) -> Result<(), crate::session::SessionError> {
        self.inner.append(message).await
    }
    fn latest_compaction(&self) -> Option<crate::session::CompactionMetadata> {
        self.inner.latest_compaction()
    }
    async fn append_compaction(
        &self,
        checkpoint: crate::session::CompactionCheckpoint,
    ) -> Result<crate::session::CompactionMetadata, crate::session::SessionError> {
        self.inner.append_compaction(checkpoint).await
    }
    async fn append_operation_fact(
        &self,
        fact: crate::session::OperationFact,
    ) -> Result<(), crate::session::SessionError> {
        let mut yielded = false;
        std::future::poll_fn(|cx| {
            if yielded {
                Poll::Ready(())
            } else {
                yielded = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        })
        .await;
        self.inner.append_operation_fact(fact).await?;
        self.attempt_persisted.store(true, Ordering::SeqCst);
        Ok(())
    }
    fn operation_history(&self) -> Vec<crate::session::OperationFact> {
        self.inner.operation_history()
    }
    fn operation_ledger(&self) -> crate::session::OperationLedger {
        self.inner.operation_ledger()
    }
    fn observed_capabilities(&self) -> Option<crate::session::Capabilities> {
        self.inner.observed_capabilities()
    }

    fn interrupted_turn(&self) -> Option<crate::session::InterruptedTurn> {
        self.inner.interrupted_turn()
    }

    async fn append_custom(
        &self,
        custom_type: &str,
        data: &str,
    ) -> Result<(), crate::session::SessionError> {
        self.inner.append_custom(custom_type, data).await
    }
}

struct DispatchAfterFactExecutor(Arc<AtomicBool>);

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl ToolExecutor for DispatchAfterFactExecutor {
    fn definitions(&self) -> Vec<ToolDefinition> {
        EchoExecutor::default().definitions()
    }
    async fn execute(
        &self,
        call: crate::tool::ToolCall<'_>,
        _control: &dyn OperationControl,
    ) -> crate::tool::ToolExecution {
        assert!(
            self.0.load(Ordering::SeqCst),
            "tool ran before its attempt fact was durable"
        );
        crate::tool::ToolExecution::completed(ToolResult {
            content: call.arguments.get().to_owned(),
            ..ToolResult::default()
        })
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl ToolExecutor for CountingExecutor {
    fn definitions(&self) -> Vec<ToolDefinition> {
        EchoExecutor::default().definitions()
    }

    async fn execute(
        &self,
        call: crate::tool::ToolCall<'_>,
        _control: &dyn OperationControl,
    ) -> crate::tool::ToolExecution {
        self.calls.fetch_add(1, Ordering::SeqCst);
        crate::tool::ToolExecution::completed(ToolResult {
            content: call.arguments.get().to_owned(),
            ..ToolResult::default()
        })
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_failed_attempt_fact_stops_before_tool_dispatch() {
    let calls = Arc::new(AtomicUsize::new(0));
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::tool_call("c1", r#"{"value":1}"#)]),
        CountingExecutor {
            calls: Arc::clone(&calls),
        },
        OperationFailingSession::new(0),
        options(),
    );

    let error = agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect_err("attempt fact failure stops the run");
    assert!(error.to_string().contains("operation attempt"), "{error}");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(agent.session().operation_history().is_empty());
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn async_operation_fact_is_durable_before_tool_dispatch() {
    let attempt_persisted = Arc::new(AtomicBool::new(false));
    let agent = Agent::new(
        FakeProvider::new(vec![
            Turn::tool_call("c1", r#"{"value":1}"#),
            Turn::text("done"),
        ]),
        DispatchAfterFactExecutor(Arc::clone(&attempt_persisted)),
        AsyncOrderingSession {
            inner: MemorySession::new(),
            attempt_persisted: Arc::clone(&attempt_persisted),
        },
        options(),
    );

    agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("run");
    assert!(attempt_persisted.load(Ordering::SeqCst));
    assert_eq!(agent.session().operation_history().len(), 2);
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_failed_terminal_fact_stops_after_dispatch_without_a_tool_result() {
    let calls = Arc::new(AtomicUsize::new(0));
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::tool_call("c1", r#"{"value":1}"#)]),
        CountingExecutor {
            calls: Arc::clone(&calls),
        },
        OperationFailingSession::new(1),
        options(),
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&events);

    let error = agent
        .run(
            "hello",
            &mut |event| captured.lock().expect("events").push(event),
            &CancellationToken::new(),
        )
        .await
        .expect_err("terminal fact failure stops the run");
    assert!(error.to_string().contains("operation terminal"), "{error}");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(agent.session().operation_history().len(), 1);
    let finished: Vec<_> = collect(&events)
        .into_iter()
        .filter_map(|event| match event {
            Event::ToolCallFinished {
                outcome, result, ..
            } => Some((outcome, result)),
            _ => None,
        })
        .collect();
    assert_eq!(finished.len(), 1);
    assert!(finished[0].1.is_error);
    assert!(finished[0].1.content.contains("outcome unknown"));
    assert_eq!(
        finished[0].0,
        crate::model::OperationOutcome {
            disposition: crate::model::OperationDisposition::Error,
            effect_certainty: crate::model::EffectCertainty::Unknown,
            stop_reason: None,
        }
    );
    assert!(
        !agent
            .session()
            .messages()
            .iter()
            .any(|message| message.role == Role::Tool)
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_failed_user_message_append_stops_before_the_provider() {
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::text("never sent")]),
        EchoExecutor::default(),
        FailingSession::new(0),
        options(),
    );
    let error = agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect_err("the append failure stops the run");
    assert_eq!(error.to_string(), "persist user message: disk full");
    assert!(agent.provider().requests().is_empty());
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_failed_notification_append_leaves_it_and_later_ones_queued() {
    let inbox = Arc::new(Inbox::default());
    inbox.push(Notification {
        task_id: "t1".into(),
        kind: Some(NotificationKind::TaskFinished),
        text: "first".into(),
        usage: None,
    });
    inbox.push(Notification {
        task_id: "t2".into(),
        kind: Some(NotificationKind::TaskFinished),
        text: "second".into(),
        usage: None,
    });
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::text("never sent")]),
        EchoExecutor::default(),
        FailingSession::new(0),
        Options {
            mailbox: inbox.clone(),
            ..options()
        },
    );
    let error = agent
        .run("", &mut |_| {}, &CancellationToken::new())
        .await
        .expect_err("the append failure stops the run");
    assert_eq!(error.to_string(), "persist notification: disk full");
    assert_eq!(
        inbox.len(),
        2,
        "the failed item and the one after it must stay queued"
    );
    assert!(agent.provider().requests().is_empty());

    // A later delivery over the same inbox, with a session that now accepts
    // appends, delivers each notification exactly once.
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::text("ok")]),
        EchoExecutor::default(),
        MemorySession::new(),
        Options {
            mailbox: inbox.clone(),
            ..options()
        },
    );
    agent
        .run("", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("run");
    let texts: Vec<String> = agent
        .session()
        .messages()
        .iter()
        .filter(|message| message.role == Role::Context)
        .map(|message| message.text())
        .collect();
    assert_eq!(texts, vec!["first".to_string(), "second".to_string()]);
    assert!(inbox.is_empty());
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn the_assistant_message_is_persisted_before_the_tools_run() {
    // The second append is the assistant message that asks for the tool call.
    let agent = Agent::new(
        FakeProvider::new(vec![
            Turn::tool_call("c1", r#"{"value":1}"#),
            Turn::text("done"),
        ]),
        EchoExecutor::default(),
        FailingSession::new(1),
        options(),
    );
    let error = agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect_err("the append failure stops the run");
    assert!(error.to_string().ends_with("disk full"), "{error}");
    assert!(
        !agent
            .session()
            .messages()
            .iter()
            .any(|message| message.role == Role::Tool),
        "a tool ran before its assistant message was durable"
    );
    assert_eq!(agent.provider().requests().len(), 1);
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_failed_tool_result_append_stops_before_the_next_provider_call() {
    // Appends: user, assistant, tool result.
    let agent = Agent::new(
        FakeProvider::new(vec![
            Turn::tool_call("c1", r#"{"value":1}"#),
            Turn::text("done"),
        ]),
        EchoExecutor::default(),
        FailingSession::new(2),
        options(),
    );
    let error = agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect_err("the append failure stops the run");
    assert!(error.to_string().ends_with("disk full"), "{error}");
    assert_eq!(agent.provider().requests().len(), 1);
}

// -- context report --------------------------------------------------------

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn the_context_report_lists_what_the_next_request_sends() {
    let agent = Agent::new(
        FakeProvider::new(vec![
            Turn::tool_call("c1", r#"{"value":1}"#),
            Turn::text("done"),
        ]),
        EchoExecutor::default(),
        MemorySession::new(),
        Options {
            system_prompt: "base\n## Skills\nnone".into(),
            system_prompt_parts: vec![
                ("Base".into(), "base\n".into()),
                ("Skills".into(), "## Skills\nnone".into()),
            ],
            memory: Some(Arc::new(FakeMemory::with_records(one_record()))),
            compaction: CompactionSettings {
                auto: true,
                hard_input_window: 200_000,
                working_window: 100_000,
                reserve_tokens: 1_000,
                keep_recent_tokens: 10_000,
            },
            ..options()
        },
    );
    agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("run");

    let report = agent.context_report();
    assert_eq!(report.model, "test-model");
    assert_eq!(report.context_window, 200_000);
    assert_eq!(report.compaction_threshold, 99_000);

    let section = |kind: SectionKind| {
        report
            .sections
            .iter()
            .find(|section| section.kind == kind)
            .unwrap_or_else(|| panic!("no {kind:?} section"))
    };
    let prompt = section(SectionKind::SystemPrompt);
    let labels: Vec<&str> = prompt.items.iter().map(|i| i.label.as_str()).collect();
    assert_eq!(labels, ["Base", "Skills"]);
    let text: String = prompt.items.iter().map(|i| i.text.as_str()).collect();
    assert_eq!(text, "base\n## Skills\nnone");

    let tools = section(SectionKind::Tools);
    assert_eq!(tools.items.len(), 1);
    assert_eq!(tools.items[0].label, "echo");
    assert!(tools.items[0].text.contains(r#""type":"object""#));

    // user, assistant tool call, tool result, assistant text: the session
    // transcript, in order.
    let messages = section(SectionKind::Messages);
    assert_eq!(messages.items.len(), agent.session().messages().len());
    assert_eq!(messages.items[0].label, "#1 user");
    assert_eq!(messages.items[0].text, "hello");
    assert_eq!(messages.items[1].label, "#2 assistant echo");
    assert_eq!(messages.items[2].label, "#3 tool echo");
    assert_eq!(messages.items[3].text, "done");

    let memory = section(SectionKind::Memory);
    assert!(memory.items[0].text.contains("prefers vim"));

    for section in &report.sections {
        let sum: i64 = section.items.iter().map(|item| item.tokens).sum();
        assert_eq!(section.tokens, sum, "{:?}", section.kind);
    }
    assert!(report.estimated_total > 0);
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn prompt_parts_that_do_not_match_the_prompt_fall_back_to_one_part() {
    let agent = Agent::new(
        FakeProvider::new(Vec::new()),
        EchoExecutor::default(),
        MemorySession::new(),
        Options {
            system_prompt: "the real prompt".into(),
            system_prompt_parts: vec![("Base".into(), "something else".into())],
            ..options()
        },
    );
    let report = agent.context_report();
    let prompt = &report.sections[0];
    assert_eq!(prompt.kind, SectionKind::SystemPrompt);
    assert_eq!(prompt.items.len(), 1);
    assert_eq!(prompt.items[0].label, "System prompt");
    assert_eq!(prompt.items[0].text, "the real prompt");
    // No compaction settings: no window and no threshold.
    assert_eq!(report.context_window, 0);
    assert_eq!(report.compaction_threshold, 0);
    assert!(
        !report
            .sections
            .iter()
            .any(|section| section.kind == SectionKind::Memory),
        "no recall ran, so there is no memory section"
    );
}

#[test]
fn a_compaction_summary_and_mcp_tools_get_their_own_sections() {
    use crate::session::COMPACTION_CONTEXT_TYPE;
    let request = Request {
        model: "m".into(),
        system_prompt: "p".into(),
        thinking: String::new(),
        messages: vec![
            Message {
                role: Role::Context,
                context_type: COMPACTION_CONTEXT_TYPE.into(),
                blocks: vec![Block::text("[Compaction summary]\nearlier")],
                ..Message::default()
            },
            Message {
                role: Role::User,
                blocks: vec![Block::text("next")],
                ..Message::default()
            },
        ],
        tools: vec![
            ToolDefinition {
                name: "read".into(),
                ..ToolDefinition::default()
            },
            ToolDefinition {
                name: "mcp__github__search".into(),
                ..ToolDefinition::default()
            },
        ],
    };
    let sections = super::context_report::sections(&request, &[], "");
    let kinds: Vec<SectionKind> = sections.iter().map(|section| section.kind).collect();
    assert_eq!(
        kinds,
        [
            SectionKind::SystemPrompt,
            SectionKind::Tools,
            SectionKind::McpTools,
            SectionKind::CompactionSummary,
            SectionKind::Messages,
        ]
    );
    assert_eq!(sections[3].items[0].text, "[Compaction summary]\nearlier");
    assert_eq!(sections[4].items[0].label, "#2 user");
}

// -- one-shot text completion ------------------------------------------------

fn text_request<'a>(user_text: &'a str, maximum_bytes: usize) -> super::oneshot::TextRequest<'a> {
    super::oneshot::TextRequest {
        system_prompt: "answer briefly",
        user_text,
        maximum_bytes,
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn complete_text_sends_one_tool_less_redacted_message_and_touches_no_history() {
    const SECRET: &str = "sk-live-abcdef";
    let mut turn = Turn::summary("the answer");
    turn.events.push(StreamEvent::Retry {
        attempt: 2,
        max_attempts: 4,
        delay: std::time::Duration::from_secs(1),
        reason: "connection interrupted".into(),
    });
    let agent = Agent::with_redactor(
        FakeProvider::new(vec![turn]),
        EchoExecutor::default(),
        MemorySession::new(),
        options(),
        Redactor::new(&[SECRET.to_owned()]),
    );

    let mut events = Vec::new();
    let response = agent
        .complete_text(
            &text_request(&format!("the key is {SECRET}"), 1024),
            &mut |event| events.push(event),
            &CancellationToken::new(),
        )
        .await
        .expect("complete");

    assert_eq!(response.text, "the answer");
    assert_eq!(names(&events), ["provider_retry", "provider_api_call"]);
    assert!(
        matches!(&events[0], Event::ProviderRetry { operation_id, attempt: 2, .. } if !operation_id.as_str().is_empty())
    );
    assert!(response.usage_present);
    assert_eq!(response.usage.output_tokens, 7);
    let requests = agent.provider().requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].tools.is_empty());
    assert_eq!(requests[0].system_prompt, "answer briefly");
    assert_eq!(requests[0].messages.len(), 1);
    assert!(!requests[0].messages[0].text().contains(SECRET));
    assert!(agent.session.messages().is_empty(), "no history appended");
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn complete_text_redacts_the_response() {
    const SECRET: &str = "sk-live-abcdef";
    let provider = FakeProvider::new(vec![Turn::summary(&format!("leaked {SECRET}"))]);
    let agent = Agent::with_redactor(
        provider,
        EchoExecutor::default(),
        MemorySession::new(),
        options(),
        Redactor::new(&[SECRET.to_owned()]),
    );
    let response = agent
        .complete_text(
            &text_request("hi", 1024),
            &mut |_| {},
            &CancellationToken::new(),
        )
        .await
        .expect("complete");
    assert!(!response.text.contains(SECRET), "{}", response.text);
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn complete_text_rejects_a_tool_call_a_wrong_role_and_an_oversized_answer() {
    let cases = [
        (Turn::tool_call("c1", "{}"), "non-text block"),
        (Turn::summary("0123456789"), "byte bound"),
    ];
    for (turn, expected) in cases {
        let agent = Agent::new(
            FakeProvider::new(vec![turn]),
            EchoExecutor::default(),
            MemorySession::new(),
            options(),
        );
        let error = agent
            .complete_text(
                &text_request("hi", 5),
                &mut |_| {},
                &CancellationToken::new(),
            )
            .await
            .expect_err("rejected");
        assert!(error.to_string().contains(expected), "{error}");
    }

    let mut wrong_role = Turn::summary("fine");
    if let Ok(response) = &mut wrong_role.outcome {
        response.message.role = Role::User;
    }
    let agent = Agent::new(
        FakeProvider::new(vec![wrong_role]),
        EchoExecutor::default(),
        MemorySession::new(),
        options(),
    );
    let error = agent
        .complete_text(
            &text_request("hi", 1024),
            &mut |_| {},
            &CancellationToken::new(),
        )
        .await
        .expect_err("rejected");
    assert!(error.to_string().contains("assistant"), "{error}");
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn complete_text_stops_a_stream_that_runs_past_its_bound() {
    let turn = Turn::summary("x").with_events(vec![
        StreamEvent::TextDelta {
            text: "0123".into(),
        },
        StreamEvent::TextDelta {
            text: "4567".into(),
        },
    ]);
    let agent = Agent::new(
        FakeProvider::new(vec![turn]),
        EchoExecutor::default(),
        MemorySession::new(),
        options(),
    );
    let error = agent
        .complete_text(
            &text_request("hi", 5),
            &mut |_| {},
            &CancellationToken::new(),
        )
        .await
        .expect_err("stopped");
    assert!(error.to_string().contains("exceeded"), "{error}");
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn complete_text_does_not_call_the_provider_when_already_cancelled_or_boundary_closed() {
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::summary("never")]),
        EchoExecutor::default(),
        MemorySession::new(),
        options(),
    );
    let cancel = CancellationToken::new();
    cancel.cancel();
    let error = agent
        .complete_text(&text_request("hi", 1024), &mut |_| {}, &cancel)
        .await
        .expect_err("cancelled");
    assert!(error.is_cancelled(), "{error}");

    let closed = Agent::with_redactor(
        FakeProvider::new(vec![Turn::summary("never")]),
        EchoExecutor::default(),
        MemorySession::new(),
        options(),
        Redactor::with_completeness(&[], false),
    );
    assert!(!closed.allows_dynamic_content());
    let error = closed
        .complete_text(
            &text_request("hi", 1024),
            &mut |_| {},
            &CancellationToken::new(),
        )
        .await
        .expect_err("closed");
    assert!(error.to_string().contains("redaction boundary"), "{error}");
    assert!(agent.provider().requests().is_empty());
    assert!(closed.provider().requests().is_empty());
}

struct ApprovalBarrierExecutor(Arc<AtomicUsize>);
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl ToolExecutor for ApprovalBarrierExecutor {
    fn definitions(&self) -> Vec<ToolDefinition> {
        EchoExecutor::default().definitions()
    }
    fn batch_control(
        &self,
        _name: &str,
        _execution: &crate::tool::ToolExecution,
    ) -> crate::approval::BatchControl {
        crate::approval::BatchControl::Replan
    }
    async fn execute(
        &self,
        call: crate::tool::ToolCall<'_>,
        control: &dyn OperationControl,
    ) -> crate::tool::ToolExecution {
        self.0.fetch_add(1, Ordering::SeqCst);
        EchoExecutor::default().execute(call, control).await
    }
}
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn approval_barrier_settles_later_batch_calls_without_dispatch() {
    let calls = Arc::new(AtomicUsize::new(0));
    let agent = Agent::new(
        FakeProvider::new(vec![two_tool_calls(), Turn::text("replanned")]),
        ApprovalBarrierExecutor(calls.clone()),
        MemorySession::new(),
        options(),
    );
    let mut events = Vec::new();
    agent
        .run(
            "confirm first",
            &mut |event| events.push(event),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(events.iter().any(|event|matches!(event,Event::ToolCallFinished{tool_call_id,outcome,..}
        if tool_call_id=="c2" && outcome.effect_certainty==crate::model::EffectCertainty::NotStarted)));
}

#[derive(Default)]
struct CompactionHooks {
    events: Mutex<Vec<String>>,
    deny: bool,
}
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch="wasm32",async_trait::async_trait(?Send))]
impl super::hooks::Hooks for CompactionHooks {
    async fn run(
        &self,
        event: &str,
        _: serde_json::Value,
        _: &dyn OperationControl,
    ) -> Result<super::hooks::Response, String> {
        self.events.lock().unwrap().push(event.into());
        if event == "before_compaction" {
            return Ok(super::hooks::Response {
                decision: Some(if self.deny {
                    super::hooks::Decision::Deny
                } else {
                    super::hooks::Decision::Allow
                }),
                context: Some("retain the parser contract".into()),
                ..Default::default()
            });
        }
        Ok(Default::default())
    }
}
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn compaction_hooks_gate_the_provider_and_observe_only_committed_checkpoints() {
    for deny in [false, true] {
        let hooks = Arc::new(CompactionHooks {
            deny,
            ..Default::default()
        });
        let mut opts = options();
        opts.hooks = Some(hooks.clone());
        let agent = Agent::new(
            FakeProvider::new(vec![Turn::summary(&structured_summary())]),
            EchoExecutor::default(),
            seeded_session().await,
            opts,
        );
        let result = agent
            .compact("parser", &mut |_| {}, &CancellationToken::new())
            .await;
        if deny {
            assert!(result.is_err());
            assert!(agent.provider().summary_requests().is_empty());
            assert_eq!(
                *hooks.events.lock().unwrap(),
                vec!["session_start", "before_compaction"]
            );
        } else {
            assert!(result.is_ok());
            assert!(
                agent.provider().summary_requests()[0]
                    .system_prompt
                    .contains("retain the parser contract")
            );
            assert_eq!(
                *hooks.events.lock().unwrap(),
                vec!["session_start", "before_compaction", "after_compaction"]
            );
        }
    }
}

// -- capability notice -----------------------------------------------------

const NOTICE_OPEN: &str = "<runtime_capability_update>";

fn recorded_tools(session: &MemorySession) -> Vec<String> {
    session.observed_capabilities().expect("record").tools
}

async fn session_with_record(tools: &str) -> MemorySession {
    let session = MemorySession::new();
    session
        .append_custom(
            crate::session::CAPABILITIES_CUSTOM_TYPE,
            &format!(r#"{{"tools":{tools}}}"#),
        )
        .await
        .expect("seed record");
    session
}

fn notices(messages: &[Message]) -> Vec<&Message> {
    messages
        .iter()
        .filter(|message| message.context_type == crate::session::CAPABILITY_UPDATE_CONTEXT_TYPE)
        .collect()
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_new_session_gets_a_record_and_no_notice() {
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::text("one")]),
        EchoExecutor::default(),
        MemorySession::new(),
        options(),
    );
    agent
        .run("hi", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("run");
    assert!(notices(&agent.session().messages()).is_empty());
    assert_eq!(recorded_tools(agent.session()), vec!["echo".to_owned()]);
    assert_eq!(
        agent.provider().normal_requests()[0].system_prompt,
        "be brief"
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_differing_record_appends_one_notice_before_the_user_message() {
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::text("one"), Turn::text("two")]),
        EchoExecutor::default(),
        session_with_record(r#"["web_search"]"#).await,
        options(),
    );
    agent
        .run("hi", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("run");
    let messages = agent.session().messages();
    assert_eq!(messages[0].role, Role::Context);
    assert!(messages[0].display);
    assert_eq!(messages[1].role, Role::User);
    let notice = messages[0].text();
    assert!(notice.starts_with(NOTICE_OPEN));
    assert!(notice.contains("\nAdded: echo\nRemoved: web_search\n"));
    assert_eq!(recorded_tools(agent.session()), vec!["echo".to_owned()]);

    agent
        .run("again", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("run");
    assert_eq!(notices(&agent.session().messages()).len(), 1);
    let requests = agent.provider().normal_requests();
    for request in &requests {
        assert_eq!(request.system_prompt, "be brief");
        assert_eq!(request.messages[0].text(), notice);
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_changed_definition_digest_appends_one_notice() {
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::text("one"), Turn::text("two")]),
        EchoExecutor::default(),
        session_with_record(r#"["echo"],"definitions":{"echo":"0000000000000000"}"#).await,
        options(),
    );
    for prompt in ["hi", "again"] {
        agent
            .run(prompt, &mut |_| {}, &CancellationToken::new())
            .await
            .expect("run");
    }
    let messages = agent.session().messages();
    let found = notices(&messages);
    assert_eq!(found.len(), 1);
    assert!(found[0].text().contains("\nChanged definitions: echo\n"));
    let recorded = agent.session().observed_capabilities().expect("record");
    assert_ne!(
        recorded.definitions.expect("digests")["echo"],
        "0000000000000000"
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn history_without_a_record_gets_the_legacy_notice_once() {
    let session = MemorySession::new();
    for (role, text) in [(Role::User, "old"), (Role::Assistant, "old reply")] {
        session
            .append(Message {
                role,
                blocks: vec![Block::text(text)],
                ..Message::default()
            })
            .await
            .expect("append");
    }
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::text("one"), Turn::text("two")]),
        EchoExecutor::default(),
        session,
        options(),
    );
    for prompt in ["hi", "again"] {
        agent
            .run(prompt, &mut |_| {}, &CancellationToken::new())
            .await
            .expect("run");
    }
    let messages = agent.session().messages();
    let found = notices(&messages);
    assert_eq!(found.len(), 1);
    assert!(found[0].text().contains("was not recorded"));
    assert!(found[0].text().contains("Current tools: echo\n"));
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_matching_record_appends_nothing() {
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::text("one")]),
        EchoExecutor::default(),
        session_with_record(r#"["echo"]"#).await,
        options(),
    );
    agent
        .run("hi", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("run");
    assert_eq!(agent.session().messages().len(), 2);
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_summarized_notice_is_carried_in_the_capability_updates_block() {
    async fn exchange(session: &MemorySession, index: usize, text: &str) {
        for (id, role, text) in [
            (format!("u{index}"), Role::User, text),
            (format!("a{index}"), Role::Assistant, "ok"),
        ] {
            session
                .append(Message {
                    id,
                    role,
                    blocks: vec![Block::text(text)],
                    ..Message::default()
                })
                .await
                .expect("append");
        }
    }

    // A matching record, so the run between the compactions adds no notice.
    let session = session_with_record(r#"["echo"]"#).await;
    session
        .append(crate::session::context::new_context_message(
            "n0",
            crate::session::CAPABILITY_UPDATE_CONTEXT_TYPE,
            true,
            "<runtime_capability_update>\nAdded: agent\n</runtime_capability_update>".into(),
            clock(),
            None,
        ))
        .await
        .expect("notice");
    exchange(&session, 0, "first").await;
    exchange(&session, 1, "second").await;
    exchange(&session, 2, "third").await;
    let agent = Agent::new(
        FakeProvider::new(vec![
            Turn::summary(&structured_summary()),
            Turn::text("ok"),
            Turn::summary(&structured_summary()),
        ]),
        EchoExecutor::default(),
        session,
        options(),
    );
    let block = "<capability-updates>\n\"\\u003cruntime_capability_update\\u003e\\nAdded: agent\\n\\u003c/runtime_capability_update\\u003e\"\n</capability-updates>";
    agent
        .compact("", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("first compaction");
    agent
        .run("next", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("run after compaction");
    let next_request_text: String = agent.provider().normal_requests()[0]
        .messages
        .iter()
        .map(Message::text)
        .collect();
    assert!(next_request_text.contains(block), "{next_request_text}");

    exchange(agent.session(), 3, "fourth").await;
    agent
        .compact("", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("second compaction");
    let summary = agent
        .session()
        .latest_compaction()
        .expect("a checkpoint")
        .summary;
    assert_eq!(summary.matches(block).count(), 1, "{summary}");
    let second_request = agent.provider().summary_requests()[1].messages[0].text();
    assert!(
        !second_request.contains("capability-updates"),
        "the carried block reached the summarizer: {second_request}"
    );
}

// -- turn records ----------------------------------------------------------

/// A [`MemorySession`] that also keeps every `otto.turn` record it is given.
#[derive(Default)]
struct TurnRecordingSession {
    inner: MemorySession,
    turns: Mutex<Vec<crate::session::TurnRecord>>,
}

impl TurnRecordingSession {
    fn turns(&self) -> Vec<crate::session::TurnRecord> {
        self.turns.lock().expect("turns").clone()
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl Session for TurnRecordingSession {
    fn messages(&self) -> Vec<Message> {
        self.inner.messages()
    }
    async fn append(&self, message: Message) -> Result<(), crate::session::SessionError> {
        self.inner.append(message).await
    }
    fn latest_compaction(&self) -> Option<crate::session::CompactionMetadata> {
        self.inner.latest_compaction()
    }
    async fn append_compaction(
        &self,
        checkpoint: crate::session::CompactionCheckpoint,
    ) -> Result<crate::session::CompactionMetadata, crate::session::SessionError> {
        self.inner.append_compaction(checkpoint).await
    }
    async fn append_operation_fact(
        &self,
        fact: crate::session::OperationFact,
    ) -> Result<(), crate::session::SessionError> {
        self.inner.append_operation_fact(fact).await
    }
    fn operation_ledger(&self) -> crate::session::OperationLedger {
        self.inner.operation_ledger()
    }
    fn observed_capabilities(&self) -> Option<crate::session::Capabilities> {
        self.inner.observed_capabilities()
    }
    fn interrupted_turn(&self) -> Option<crate::session::InterruptedTurn> {
        self.inner.interrupted_turn()
    }
    async fn append_custom(
        &self,
        custom_type: &str,
        data: &str,
    ) -> Result<(), crate::session::SessionError> {
        if custom_type == crate::session::TURN_CUSTOM_TYPE {
            self.turns
                .lock()
                .expect("turns")
                .push(crate::session::decode_turn_record(data).expect("valid turn record"));
        }
        self.inner.append_custom(custom_type, data).await
    }
}

fn started_trigger(
    record: &crate::session::TurnRecord,
) -> (crate::session::TurnTrigger, u32, Option<String>) {
    match record {
        crate::session::TurnRecord::Started {
            trigger,
            attempt,
            resumes,
            ..
        } => (*trigger, *attempt, resumes.clone()),
        other => panic!("expected a start record, got {other:?}"),
    }
}

fn ended_outcome(record: &crate::session::TurnRecord) -> crate::session::TurnOutcome {
    match record {
        crate::session::TurnRecord::Ended { outcome, .. } => *outcome,
        other => panic!("expected an end record, got {other:?}"),
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn every_turn_writes_a_start_and_a_matching_end() {
    use crate::session::{TurnOutcome, TurnTrigger};
    let provider = FakeProvider::new(vec![
        Turn::text("ok"),
        Turn {
            events: Vec::new(),
            outcome: Err(ProviderError::Other("boom".into())),
        },
    ]);
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        TurnRecordingSession::default(),
        options(),
    );
    agent
        .run("first", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("completed turn");
    agent
        .run("second", &mut |_| {}, &CancellationToken::new())
        .await
        .expect_err("failed turn");
    let cancel = CancellationToken::new();
    cancel.cancel();
    agent
        .run("third", &mut |_| {}, &cancel)
        .await
        .expect_err("cancelled turn");

    let turns = agent.session().turns();
    assert_eq!(turns.len(), 6, "{turns:?}");
    for (index, outcome) in [
        TurnOutcome::Completed,
        TurnOutcome::Failed,
        TurnOutcome::Cancelled,
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(
            started_trigger(&turns[index * 2]),
            (TurnTrigger::User, 1, None)
        );
        assert_eq!(ended_outcome(&turns[index * 2 + 1]), outcome);
    }
    assert_eq!(agent.session().interrupted_turn(), None);
}

/// A session whose last turn started and never ended, as process loss
/// leaves it.
async fn interrupted_session() -> TurnRecordingSession {
    let session = TurnRecordingSession::default();
    session
        .inner
        .append_custom(
            crate::session::TURN_CUSTOM_TYPE,
            &crate::session::encode_turn_record(&crate::session::TurnRecord::started(
                "op_old",
                crate::session::TurnTrigger::User,
                1,
                None,
            )),
        )
        .await
        .expect("seed start record");
    session
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_wake_turn_after_an_interrupted_turn_is_its_recovery() {
    use crate::session::TurnTrigger;
    let inbox = Arc::new(Inbox::default());
    inbox.push(Notification {
        kind: Some(NotificationKind::Message),
        text: "The previous Otto process stopped".into(),
        ..Notification::default()
    });
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::text("resumed")]),
        EchoExecutor::default(),
        interrupted_session().await,
        Options {
            mailbox: inbox,
            ..options()
        },
    );
    agent
        .run("", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("recovery turn");
    let turns = agent.session().turns();
    assert_eq!(
        started_trigger(&turns[0]),
        (TurnTrigger::Recovery, 2, Some("op_old".to_string()))
    );
    assert_eq!(agent.session().interrupted_turn(), None);
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_prompt_after_an_interrupted_turn_starts_a_new_chain() {
    use crate::session::TurnTrigger;
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::text("ok")]),
        EchoExecutor::default(),
        interrupted_session().await,
        options(),
    );
    agent
        .run("go on", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("prompt turn");
    assert_eq!(
        started_trigger(&agent.session().turns()[0]),
        (TurnTrigger::User, 1, None)
    );
}
