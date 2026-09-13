//! The provider/tool turn loop.
//!
//! Port of the core of `Agent.Run` in `internal/agent/agent.go`. Phase 0
//! carries the plain loop: one user message, then provider calls alternating
//! with tool calls until the model stops asking for tools.
//!
//! Out of scope for phase 0, each marked with a `phase 4:` comment where it
//! plugs in: compaction, memory recall, the inbox and notification messages,
//! the secret redactor, and the tool-result overlay.
//!
//! Ownership: the agent owns its provider, tool executor, and session. The
//! caller owns the event sink and the cancellation token.
//!
//! Concurrency and cancellation: `run` takes `&self` but a single agent is
//! meant to serve one turn at a time; the Go implementation serializes with a
//! mutex and the composition root will do the same in phase 4. Cancelling the
//! token stops the provider call, skips the remaining tool calls, and ends the
//! run with an error.
//!
//! Errors: every failure path emits [`Event::AgentError`] and returns the same
//! error, so a frontend that only watches events sees every failure.

use chrono::{DateTime, Utc};
use serde_json::value::RawValue;
use tokio_util::sync::CancellationToken;

use crate::model::{Block, BlockType, Message, Role, Usage, zero_time};
use crate::provider::{Provider, ProviderError, Request, StreamEvent};
use crate::session::{Session, SessionError};
use crate::tool::{ToolExecutor, ToolResult};

/// The outcome the agent records for one provider HTTP call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiStatus {
    Ok,
    Canceled,
    Error,
}

/// Everything the agent reports while a turn runs.
///
/// Events are delivered synchronously and in order from inside `run`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    AgentStarted,
    AgentFinished,
    TextDelta {
        text: String,
    },
    ToolCallStarted {
        tool_name: String,
        tool_call_id: String,
        /// The raw JSON arguments as the provider sent them.
        arguments: String,
    },
    ToolCallFinished {
        tool_name: String,
        tool_call_id: String,
        result: ToolResult,
    },
    ProviderUsage {
        usage: Usage,
        /// False when the provider reported no usage; `usage` is then zero.
        present: bool,
    },
    ProviderApiCall {
        provider: String,
        model: String,
        duration: std::time::Duration,
        status: ApiStatus,
    },
    AgentError {
        /// The `Display` text of the [`AgentError`] `run` is about to return.
        message: String,
    },
}

/// The event callback a caller passes to [`Agent::run`]. See
/// [`crate::provider::StreamSink`] for why the `Send` bound is target
/// dependent.
#[cfg(not(target_arch = "wasm32"))]
pub type EventSink<'a> = &'a mut (dyn FnMut(Event) + Send);
/// See the native definition above.
#[cfg(target_arch = "wasm32")]
pub type EventSink<'a> = &'a mut dyn FnMut(Event);

/// Turn settings and the two injected capabilities the core cannot provide
/// itself: the clock and the identifier generator. Keeping them here is what
/// lets `otto-core` build for `wasm32-unknown-unknown`.
pub struct Options {
    pub model: String,
    pub provider_name: String,
    pub system_prompt: String,
    pub thinking: String,
    /// Returns the current time. Called once per persisted message and twice
    /// per provider call, to measure its duration.
    pub now: Box<dyn Fn() -> DateTime<Utc> + Send + Sync>,
    /// Returns a fresh message identifier. Must not repeat within a session.
    pub new_id: Box<dyn Fn() -> String + Send + Sync>,
}

/// Why a turn stopped early.
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("user text is required")]
    EmptyUserText,
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error("persist {kind}: {source}")]
    Persist {
        /// What the agent was trying to store, for example `user message`.
        kind: &'static str,
        source: SessionError,
    },
    #[error("invalid provider response: {0}")]
    InvalidResponse(String),
}

/// Runs provider and tool turns against one session.
pub struct Agent<P, T, S> {
    provider: P,
    tools: T,
    session: S,
    options: Options,
}

impl<P: Provider, T: ToolExecutor, S: Session> Agent<P, T, S> {
    pub fn new(provider: P, tools: T, session: S, options: Options) -> Self {
        Self {
            provider,
            tools,
            session,
            options,
        }
    }

    /// The session this agent appends to.
    pub fn session(&self) -> &S {
        &self.session
    }

    /// The provider this agent calls.
    pub fn provider(&self) -> &P {
        &self.provider
    }

    /// Runs one turn: appends `user_text`, then alternates provider calls and
    /// tool calls until the model returns a message with no tool call.
    pub async fn run(
        &self,
        user_text: &str,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<(), AgentError> {
        // phase 4: a wake turn with a non-empty inbox runs with no user text.
        if trim_go_space(user_text).is_empty() {
            return fail(emit, AgentError::EmptyUserText);
        }
        emit(Event::AgentStarted);

        // phase 4: redact secrets out of the user text before it is stored.
        let user = Message {
            id: (self.options.new_id)(),
            role: Role::User,
            created_at: (self.options.now)(),
            blocks: vec![Block::text(user_text)],
            ..Message::default()
        };
        if let Err(source) = self.session.append(user).await {
            return fail(
                emit,
                AgentError::Persist {
                    kind: "user message",
                    source,
                },
            );
        }
        // phase 4: memory recall over the user text, then inbox delivery.

        loop {
            // phase 4: automatic compaction and context-overflow retry wrap
            // this request, and the tool-result overlay restores the live tool
            // text into the cloned messages.
            let request = Request {
                model: self.options.model.clone(),
                system_prompt: self.options.system_prompt.clone(),
                thinking: self.options.thinking.clone(),
                messages: self.session.messages(),
                tools: self.tools.definitions(),
            };

            let started = (self.options.now)();
            let outcome = {
                // phase 4: the redactor buffers deltas before they are emitted.
                let mut on_stream = |event: StreamEvent| {
                    if let StreamEvent::TextDelta { text } = event
                        && !text.is_empty()
                    {
                        emit(Event::TextDelta { text });
                    }
                };
                self.provider
                    .complete(&request, &mut on_stream, cancel)
                    .await
            };
            let duration = ((self.options.now)() - started)
                .to_std()
                .unwrap_or_default();
            let status = match &outcome {
                Ok(_) => ApiStatus::Ok,
                Err(ProviderError::Cancelled) => ApiStatus::Canceled,
                Err(_) if cancel.is_cancelled() => ApiStatus::Canceled,
                Err(_) => ApiStatus::Error,
            };
            emit(Event::ProviderApiCall {
                provider: self.options.provider_name.clone(),
                model: self.options.model.clone(),
                duration,
                status,
            });
            let response = match outcome {
                Ok(response) => response,
                Err(error) => return fail(emit, AgentError::Provider(error)),
            };

            // phase 4: redact the assistant message before it is inspected.
            let mut assistant = response.message;
            if assistant.id.is_empty() {
                assistant.id = (self.options.new_id)();
            }
            if assistant.role == Role::default() {
                assistant.role = Role::Assistant;
            }
            if assistant.created_at == zero_time() {
                assistant.created_at = (self.options.now)();
            }
            if assistant.role != Role::Assistant {
                return fail(
                    emit,
                    AgentError::InvalidResponse("assistant role is required".into()),
                );
            }
            if let Err(error) = assistant.validate() {
                return fail(emit, AgentError::InvalidResponse(error.to_string()));
            }

            let usage = assistant.usage;
            let tool_calls: Vec<Block> = assistant
                .blocks
                .iter()
                .filter(|block| block.block_type == BlockType::ToolCall)
                .cloned()
                .collect();
            if let Err(source) = self.session.append(assistant).await {
                return fail(
                    emit,
                    AgentError::Persist {
                        kind: "assistant message",
                        source,
                    },
                );
            }
            emit(Event::ProviderUsage {
                usage: usage.unwrap_or_default(),
                present: usage.is_some(),
            });

            let had_tool_call = !tool_calls.is_empty();
            // Tool calls run one at a time, so a frontend can reserve bounded
            // delivery for the terminal event of the single active call.
            for block in tool_calls {
                let arguments = block.arguments.clone().unwrap_or_else(empty_object);
                emit(Event::ToolCallStarted {
                    tool_name: block.tool_name.clone(),
                    tool_call_id: block.tool_call_id.clone(),
                    arguments: arguments.get().to_owned(),
                });
                let result = if cancel.is_cancelled() {
                    ToolResult::error(ProviderError::Cancelled.to_string())
                } else {
                    self.tools
                        .execute(&block.tool_name, &arguments, cancel)
                        .await
                };
                // phase 4: redact the result text, and remember the live text
                // in the tool-result overlay when it differs from the stored
                // text.
                let persisted_text = result.persisted_text().to_owned();
                emit(Event::ToolCallFinished {
                    tool_name: block.tool_name.clone(),
                    tool_call_id: block.tool_call_id.clone(),
                    result,
                });
                let stored = Message {
                    id: (self.options.new_id)(),
                    role: Role::Tool,
                    created_at: (self.options.now)(),
                    blocks: vec![Block {
                        block_type: BlockType::ToolResult,
                        text: persisted_text,
                        tool_call_id: block.tool_call_id.clone(),
                        tool_name: block.tool_name.clone(),
                        ..Block::default()
                    }],
                    ..Message::default()
                };
                // The Go loop persists tool results on a non-cancellable
                // context. `Session::append` is likewise not cancellable.
                if let Err(source) = self.session.append(stored).await {
                    return fail(
                        emit,
                        AgentError::Persist {
                            kind: "tool result",
                            source,
                        },
                    );
                }
            }

            if cancel.is_cancelled() {
                return fail(emit, AgentError::Provider(ProviderError::Cancelled));
            }
            if !had_tool_call {
                emit(Event::AgentFinished);
                return Ok(());
            }
            // phase 4: deliver inbox notifications before the next provider call.
        }
    }
}

/// Emits the terminal error event and returns the error, so every failure
/// path reports once.
fn fail(emit: EventSink<'_>, error: AgentError) -> Result<(), AgentError> {
    // phase 4: redact secrets out of the error text.
    emit(Event::AgentError {
        message: error.to_string(),
    });
    Err(error)
}

/// Trims exactly the bytes Go's agent trims: space, tab, newline, carriage
/// return.
fn trim_go_space(text: &str) -> &str {
    text.trim_matches(|character| matches!(character, ' ' | '\t' | '\n' | '\r'))
}

fn empty_object() -> Box<RawValue> {
    RawValue::from_string("{}".to_owned()).expect("valid JSON")
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use chrono::{DateTime, Utc};
    use serde_json::value::RawValue;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::model::{Block, BlockType, FinishReason, Message, Role, ToolDefinition, Usage};
    use crate::provider::{Provider, ProviderError, Request, Response, StreamEvent, StreamSink};
    use crate::session::{MemorySession, Session};
    use crate::tool::{ToolExecutor, ToolResult};

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
            _cancel: &CancellationToken,
        ) -> Result<Response, ProviderError> {
            self.requests
                .lock()
                .expect("requests")
                .push(request.clone());
            let turn = self.turns.lock().expect("turns").pop_front();
            let Some((events, response)) = turn else {
                return Err(ProviderError::Other("no scripted turn left".into()));
            };
            for event in events {
                emit(event);
            }
            Ok(response)
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
            cancel: &CancellationToken,
        ) -> Result<Response, ProviderError> {
            cancel.cancelled().await;
            Err(ProviderError::Cancelled)
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
            name: &str,
            arguments: &RawValue,
            _cancel: &CancellationToken,
        ) -> ToolResult {
            if name != "echo" {
                return ToolResult::unknown_tool(name);
            }
            ToolResult {
                content: arguments.get().to_owned(),
                persisted_content: None,
                is_error: false,
            }
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
        }
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
    async fn run_executes_a_tool_turn_then_a_text_turn() {
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
            provider: "fake".into(),
            model: "test-model".into(),
            duration: std::time::Duration::ZERO,
            status: ApiStatus::Ok,
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
                },
                Event::ToolCallStarted {
                    tool_name: "echo".into(),
                    tool_call_id: "call-1".into(),
                    arguments: r#"{"value":1}"#.into(),
                },
                Event::ToolCallFinished {
                    tool_name: "echo".into(),
                    tool_call_id: "call-1".into(),
                    result: ToolResult {
                        content: r#"{"value":1}"#.into(),
                        persisted_content: None,
                        is_error: false,
                    },
                },
                Event::TextDelta {
                    text: "done".into()
                },
                api_call,
                Event::ProviderUsage {
                    usage: Usage::default(),
                    present: false
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
                _name: &str,
                _arguments: &RawValue,
                _cancel: &CancellationToken,
            ) -> ToolResult {
                ToolResult {
                    content: "live".into(),
                    persisted_content: Some("stored".into()),
                    is_error: false,
                }
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
                    provider: "fake".into(),
                    model: "test-model".into(),
                    duration: std::time::Duration::ZERO,
                    status: ApiStatus::Canceled,
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
}
