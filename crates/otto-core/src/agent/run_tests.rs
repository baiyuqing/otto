//! Run-loop tests ported from `internal/agent/agent_test.go`,
//! `overflow_test.go`, `compaction_test.go`, `memory_context_test.go`, and
//! `tasks_test.go`.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::value::RawValue;
use tokio_util::sync::CancellationToken;

use crate::model::{Block, BlockType, FinishReason, Message, Role, ToolDefinition, Usage};
use crate::provider::{
    ContextOverflowError, Provider, ProviderError, Request, RequestSizer, Response, StreamEvent,
    StreamSink,
};
use crate::session::{MemorySession, Session};
use crate::tool::{ToolExecutor, ToolResult};

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
        _cancel: &CancellationToken,
    ) -> Result<Response, ProviderError> {
        self.requests
            .lock()
            .expect("requests")
            .push(request.clone());
        let Some(turn) = self.turns.lock().expect("turns").pop_front() else {
            return Err(ProviderError::Other("no scripted turn left".into()));
        };
        for event in turn.events {
            emit(event);
        }
        turn.outcome
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
        name: &str,
        arguments: &RawValue,
        _cancel: &CancellationToken,
    ) -> ToolResult {
        if name != "echo" {
            return ToolResult::unknown_tool(name);
        }
        ToolResult {
            content: self
                .content
                .clone()
                .unwrap_or_else(|| arguments.get().to_owned()),
            persisted_content: self.persisted.clone(),
            is_error: false,
        }
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

// -- inbox and wake turns --------------------------------------------------

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn pending_notifications_are_delivered_after_the_user_message() {
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
            inbox: inbox.clone(),
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

    let messages = agent.session().messages();
    assert_eq!(messages[1].role, Role::Context);
    assert_eq!(messages[1].context_type, "task_notification");
    assert_eq!(messages[1].text(), "child finished");
    assert!(messages[1].display);
    assert_eq!(
        messages[1]
            .context_metadata
            .as_ref()
            .expect("metadata")
            .task_id,
        "t1"
    );
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
            inbox: inbox.clone(),
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
            inbox: inbox.clone(),
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
            inbox: inbox.clone(),
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
    let provider = FakeProvider::new(vec![Turn::summary(&structured_summary())]);
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
    // Go always emits the event and marks the absence with `present`.
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
        _name: &str,
        arguments: &RawValue,
        _cancel: &CancellationToken,
    ) -> ToolResult {
        self.cancel.cancel();
        ToolResult {
            content: arguments.get().to_owned(),
            persisted_content: None,
            is_error: false,
        }
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
