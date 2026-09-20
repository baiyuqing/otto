//! Test doubles shared by the sub-agent unit tests.
//!
//! [`FakeProvider`] is safe to call concurrently, as several children may run
//! at once. Routes are matched in registration order and the first match wins;
//! a route's last step repeats once its earlier steps are used up.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::future::BoxFuture;
use otto_core::agent::redactor::Redactor;
use otto_core::model::{Block, BlockType, FinishReason, Message, Role, ToolDefinition, Usage};
use otto_core::provider::{Provider, ProviderError, Request, Response, StreamEvent, StreamSink};
use otto_core::tool::ToolResult;
use serde_json::value::RawValue;
use tokio_util::sync::CancellationToken;

use super::runner::{Config, OptionsTemplate};
use super::tasks::{Task, TaskStatus, Tasks};
use crate::tool::{Tool, definition};

/// One scripted response for a matched route.
#[derive(Clone)]
pub(crate) enum RouteStep {
    Reply(Response),
    Fail(String),
}

type MatchFn = Box<dyn Fn(&Request) -> bool + Send + Sync>;

/// A callback run before route resolution on every `complete` call, with that
/// call's cancellation token, so a test can block a child, count concurrent
/// calls, or record requests.
pub(crate) type Hook =
    Arc<dyn for<'a> Fn(&'a CancellationToken, &'a Request) -> BoxFuture<'a, ()> + Send + Sync>;

struct Route {
    matches: MatchFn,
    steps: VecDeque<RouteStep>,
}

#[derive(Default)]
struct FakeState {
    routes: Vec<Route>,
    calls: Vec<Request>,
    hook: Option<Hook>,
}

/// A scripted [`Provider`] for tests.
#[derive(Default)]
pub(crate) struct FakeProvider {
    state: Mutex<FakeState>,
}

impl FakeProvider {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Registers a route matching every request for which `matches` returns
    /// true, serving `steps` in order; the last step repeats once exhausted.
    pub(crate) fn add_route(
        &self,
        matches: impl Fn(&Request) -> bool + Send + Sync + 'static,
        steps: Vec<RouteStep>,
    ) {
        self.lock().routes.push(Route {
            matches: Box::new(matches),
            steps: steps.into(),
        });
    }

    pub(crate) fn set_hook(&self, hook: Hook) {
        self.lock().hook = Some(hook);
    }

    pub(crate) fn requests(&self) -> Vec<Request> {
        self.lock().calls.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FakeState> {
        self.state.lock().expect("the fake provider lock is intact")
    }
}

#[async_trait::async_trait]
impl Provider for FakeProvider {
    async fn complete(
        &self,
        request: &Request,
        emit: StreamSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<Response, ProviderError> {
        let hook = {
            let mut state = self.lock();
            state.calls.push(request.clone());
            state.hook.clone()
        };
        if let Some(hook) = hook {
            hook(cancel, request).await;
        }
        if cancel.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }

        let step = {
            let mut state = self.lock();
            let mut found = None;
            for route in &mut state.routes {
                if !(route.matches)(request) {
                    continue;
                }
                found = match route.steps.len() {
                    0 => Some(None),
                    1 => Some(Some(route.steps[0].clone())),
                    _ => Some(route.steps.pop_front()),
                };
                break;
            }
            match found {
                Some(step) => step,
                None => {
                    return Err(ProviderError::Other(format!(
                        "FakeProvider: no route for request (last user text: {:?})",
                        last_user_text(request)
                    )));
                }
            }
        };

        match step {
            // A matched route with no steps yields an empty response and no
            // error.
            None => Ok(Response::default()),
            Some(RouteStep::Fail(error)) => Err(ProviderError::Other(error)),
            Some(RouteStep::Reply(response)) => {
                // Each text block streams whole: the agent's text accounting
                // reads the stream, not the final response blocks.
                for block in &response.message.blocks {
                    if block.block_type == BlockType::Text && !block.text.is_empty() {
                        emit(StreamEvent::TextDelta {
                            text: block.text.clone(),
                        });
                    }
                }
                Ok(response)
            }
        }
    }
}

/// The text of the last user-role message, or `""` when there is none.
pub(crate) fn last_user_text(request: &Request) -> String {
    request
        .messages
        .iter()
        .rev()
        .find(|message| message.role == Role::User)
        .map(Message::text)
        .unwrap_or_default()
}

/// A route predicate matching requests whose last user-role message text
/// equals `text` exactly.
pub(crate) fn match_prompt(text: &str) -> impl Fn(&Request) -> bool + Send + Sync + 'static {
    let text = text.to_string();
    move |request| last_user_text(request) == text
}

/// A catch-all route predicate.
pub(crate) fn match_any(_request: &Request) -> bool {
    true
}

/// A response carrying one assistant text block, no tool calls, and `usage`.
pub(crate) fn assistant_text(text: &str, usage: Usage) -> RouteStep {
    RouteStep::Reply(Response {
        message: Message {
            role: Role::Assistant,
            finish_reason: Some(FinishReason::Stop),
            usage: Some(usage),
            blocks: vec![Block::text(text)],
            ..Message::default()
        },
    })
}

/// A response carrying one tool-call block and no text, so the agent loop
/// executes the call and makes a further provider request.
pub(crate) fn assistant_tool_call(
    call_id: &str,
    tool_name: &str,
    arguments: &str,
    usage: Usage,
) -> RouteStep {
    RouteStep::Reply(Response {
        message: Message {
            role: Role::Assistant,
            finish_reason: Some(FinishReason::ToolCalls),
            usage: Some(usage),
            blocks: vec![Block {
                block_type: BlockType::ToolCall,
                tool_call_id: call_id.to_string(),
                tool_name: tool_name.to_string(),
                arguments: Some(raw(arguments)),
                ..Block::default()
            }],
            ..Message::default()
        },
    })
}

pub(crate) fn raw(json: &str) -> Box<RawValue> {
    RawValue::from_string(json.to_owned()).expect("test JSON is valid")
}

/// A minimal [`Tool`] returning a fixed result and counting its calls.
pub(crate) struct StubTool {
    name: String,
    result: ToolResult,
    calls: Arc<AtomicUsize>,
}

impl StubTool {
    pub(crate) fn new(name: &str, result: ToolResult) -> Self {
        Self {
            name: name.to_string(),
            result,
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// A handle a test keeps after the tool is moved into the runner.
    pub(crate) fn counter(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.calls)
    }

    pub(crate) fn boxed(self) -> Box<dyn Tool + Send + Sync> {
        Box::new(self)
    }
}

#[async_trait::async_trait]
impl Tool for StubTool {
    fn definition(&self) -> ToolDefinition {
        definition(
            &self.name,
            "test stub tool",
            serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {},
            }),
        )
    }

    async fn execute(&self, _arguments: &RawValue, _cancel: &CancellationToken) -> ToolResult {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.result.clone()
    }
}

pub(crate) fn stub(name: &str) -> Box<dyn Tool + Send + Sync> {
    StubTool::new(name, ToolResult::default()).boxed()
}

/// A `Config::prompt_for` stand-in whose output names the child's tools, so
/// tests can assert against it directly.
pub(crate) fn test_prompt_for(definitions: &[ToolDefinition]) -> String {
    format!("PARENT PROMPT tools={}", tool_names(definitions).join(","))
}

pub(crate) fn tool_names(definitions: &[ToolDefinition]) -> Vec<String> {
    definitions
        .iter()
        .map(|definition| definition.name.clone())
        .collect()
}

/// A [`Config`] wired to `provider` with a no-op redactor and `tools` as the
/// parent tool set. Callers override fields as needed.
pub(crate) fn test_config(
    provider: &Arc<FakeProvider>,
    tasks: &Arc<Tasks>,
    tools: Vec<Box<dyn Tool + Send + Sync>>,
) -> Config {
    Config {
        provider: Arc::clone(provider) as Arc<dyn Provider + Send + Sync>,
        tools,
        redaction_values: Vec::new(),
        redaction_complete: true,
        template: OptionsTemplate::default(),
        prompt_for: Arc::new(test_prompt_for),
        tasks: Arc::clone(tasks),
        catalog: super::Catalog::default(),
        parent_session: None,
        max_parallel: 4,
        max_output_bytes: 16384,
        usage: None,
    }
}

/// The values a parent redactor holds, for a [`Config`] that must redact.
pub(crate) fn redaction(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_string()).collect()
}

/// A redactor equivalent to the one a runner builds from `values`, for
/// asserting on redacted forms.
pub(crate) fn redactor(values: &[&str]) -> Redactor {
    Redactor::with_completeness(&redaction(values), true)
}

/// Polls until `id` reaches `status`, or fails after five seconds.
pub(crate) async fn wait_status(tasks: &Tasks, id: &str, status: TaskStatus) -> Task {
    for _ in 0..500 {
        if let Some(task) = tasks.get(id)
            && task.status == status
        {
            return task;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!(
        "task {id} did not reach status {status} in time: {:?}",
        tasks.get(id)
    );
}
