//! A scripted provider and the environment the tests run the command in.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use otto_core::agent::redactor::Redactor;
use otto_core::config::resolve::CompactionRuntime;
use otto_core::model::{Block, BlockType, FinishReason, Message, Role, Usage};
use otto_core::operation::OperationControl;
use otto_core::provider::{Provider, ProviderSettlement, Request, Response, StreamSink};
use serde_json::value::RawValue;
use tokio_util::sync::CancellationToken;

use crate::eval_command::model_calls::EvalEnvironment;
use crate::eval_command::{Invocation, Roots, parse};

type Handler = dyn Fn(&Request) -> Response + Send + Sync;

/// Answers each request with `handler` and keeps every request.
#[derive(Clone)]
pub struct ScriptedProvider {
    handler: Arc<Handler>,
    requests: Arc<Mutex<Vec<Request>>>,
}

impl ScriptedProvider {
    pub fn new(handler: impl Fn(&Request) -> Response + Send + Sync + 'static) -> Self {
        Self {
            handler: Arc::new(handler),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// A provider that fails the test when it is called.
    pub fn never_called() -> Self {
        Self::new(|_| panic!("the provider must not be called"))
    }

    /// The reply the handler gives to `request`, without recording the request.
    pub fn reply(&self, request: &Request) -> Response {
        (self.handler)(request)
    }

    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl Provider for ScriptedProvider {
    async fn complete(
        &self,
        request: &Request,
        _emit: StreamSink<'_>,
        _control: &dyn OperationControl,
    ) -> ProviderSettlement {
        self.requests.lock().unwrap().push(request.clone());
        ProviderSettlement::succeeded((self.handler)(request), 1)
    }
}

fn usage() -> Option<Usage> {
    Some(Usage {
        input_tokens: 100,
        output_tokens: 10,
        cached_input_tokens: 40,
        ..Default::default()
    })
}

pub fn text_reply(text: &str) -> Response {
    Response {
        message: Message {
            role: Role::Assistant,
            finish_reason: Some(FinishReason::Stop),
            usage: usage(),
            blocks: vec![Block::text(text)],
            ..Message::default()
        },
        ..Response::default()
    }
}

pub fn tool_reply(name: &str, arguments: &str) -> Response {
    static NEXT_CALL: AtomicU64 = AtomicU64::new(1);
    Response {
        message: Message {
            role: Role::Assistant,
            finish_reason: Some(FinishReason::ToolCalls),
            usage: usage(),
            blocks: vec![Block {
                block_type: BlockType::ToolCall,
                tool_call_id: format!("call-{}", NEXT_CALL.fetch_add(1, Ordering::SeqCst)),
                tool_name: name.into(),
                arguments: Some(RawValue::from_string(arguments.to_owned()).unwrap()),
                ..Block::default()
            }],
            ..Message::default()
        },
        ..Response::default()
    }
}

/// The text of the first user message of the request.
pub fn first_user_text(request: &Request) -> String {
    request
        .messages
        .iter()
        .find(|message| message.role == Role::User)
        .map(Message::text)
        .unwrap_or_default()
}

/// The text of the newest user message.
pub fn last_user_text(request: &Request) -> String {
    request
        .messages
        .iter()
        .rev()
        .find(|message| message.role == Role::User)
        .map(Message::text)
        .unwrap_or_default()
}

pub fn user_message_count(request: &Request) -> usize {
    request
        .messages
        .iter()
        .filter(|message| message.role == Role::User)
        .count()
}

pub fn ends_with_tool_result(request: &Request) -> bool {
    request
        .messages
        .last()
        .is_some_and(|message| message.role == Role::Tool)
}

pub fn environment(
    provider: ScriptedProvider,
    redactor: Redactor,
) -> Arc<EvalEnvironment<ScriptedProvider>> {
    Arc::new(EvalEnvironment {
        provider,
        redactor,
        provider_name: "openai-compatible".into(),
        profile_model: "profile-model".into(),
        compressor_model: "compressor-model".into(),
        thinking: String::new(),
        compaction: CompactionRuntime {
            auto: false,
            context_window: 100_000,
            hard_input_window: 90_000,
            working_window: 80_000,
            max_output_tokens: 4_000,
            reserve_tokens: 10_000,
            keep_recent_tokens: 5_000,
        },
        cancel: CancellationToken::new(),
    })
}

pub fn invocation(arguments: &[&str]) -> Invocation {
    let arguments: Vec<String> = arguments
        .iter()
        .map(|argument| (*argument).to_string())
        .collect();
    parse(&arguments).expect("valid arguments")
}

pub fn roots(directory: &std::path::Path) -> Roots {
    Roots {
        session_root: directory.join("sessions"),
        workspace: directory.to_string_lossy().into_owned(),
        home: directory.to_string_lossy().into_owned(),
    }
}
