//! Optional extension boundary; core never launches commands.
use crate::model::{Block, BlockType, Message, Role};
use crate::operation::OperationControl;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    pub decision: Option<Decision>,
    pub reason: Option<String>,
    pub context: Option<String>,
    pub arguments: Option<Value>,
    pub content: Option<String>,
    /// Text shown to the user as a notification; never sent to the model.
    pub notice: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Allow,
    Deny,
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch="wasm32", async_trait::async_trait(?Send))]
pub trait Hooks: Send + Sync {
    async fn run(
        &self,
        event: &str,
        data: Value,
        control: &dyn OperationControl,
    ) -> Result<Response, String>;
}

#[derive(Default)]
pub struct State {
    pub start: std::sync::OnceLock<Result<Response, String>>,
    pub starting: std::sync::atomic::AtomicBool,
    /// Set once the `session_start` notice has been shown for this session.
    pub start_notified: std::sync::atomic::AtomicBool,
    pub start_done: tokio_util::sync::CancellationToken,
    pub closed: tokio_util::sync::CancellationToken,
    pub end_requested: std::sync::atomic::AtomicBool,
    pub ended: std::sync::atomic::AtomicBool,
    pub end_done: tokio_util::sync::CancellationToken,
}

impl State {
    pub fn claim_start(&self) -> bool {
        !self
            .starting
            .swap(true, std::sync::atomic::Ordering::SeqCst)
    }
    pub fn complete_start(&self, result: Result<Response, String>) {
        let _ = self.start.set(result);
        self.start_done.cancel();
    }
    pub async fn wait_start(&self) -> Result<Response, String> {
        self.start_done.cancelled().await;
        self.start
            .get()
            .cloned()
            .unwrap_or_else(|| Err("session hook initialization failed".into()))
    }
}

impl<P: crate::provider::Provider, T: crate::tool::ToolExecutor, S: crate::session::Session>
    super::Agent<P, T, S>
{
    pub async fn start_hooks(&self, control: &dyn OperationControl) -> Result<Response, String> {
        let state = &self.options.hook_state;
        if let Some(result) = state.start.get() {
            return result.clone();
        }
        if !state.claim_start() {
            return state.wait_start().await;
        }
        let result = self
            .hook("session_start", serde_json::json!({}), control)
            .await;
        state.complete_start(result.clone());
        result
    }
    /// Appends to the session and, on success, records the message for the
    /// `turn_end` hook when a hooked run is active.
    pub(super) async fn append_message(
        &self,
        message: Message,
    ) -> Result<(), crate::session::SessionError> {
        let recording = self
            .hook_turn_messages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some();
        let recorded = recording.then(|| message.clone());
        self.session.append(message).await?;
        if let Some(recorded) = recorded {
            self.record_turn_message(recorded);
        }
        Ok(())
    }

    pub(super) fn record_turn_message(&self, message: Message) {
        if let Some(messages) = self
            .hook_turn_messages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_mut()
        {
            messages.push(message);
        }
    }

    pub(super) async fn hook(
        &self,
        event: &str,
        hook_payload: Value,
        control: &dyn OperationControl,
    ) -> Result<Response, String> {
        if !self.redactor.allows_dynamic_content() {
            return Ok(Response::default());
        }
        let mut hook_payload = hook_payload;
        if let Some(object) = hook_payload.as_object_mut() {
            object.insert(
                "turn_id".into(),
                self.hook_turn_id
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone()
                    .into(),
            );
        }
        match &self.options.hooks {
            Some(hooks) => hooks.run(event, hook_payload, control).await,
            None => Ok(Response::default()),
        }
    }

    pub(super) fn hook_error(
        &self,
        message: String,
        control: &dyn OperationControl,
    ) -> super::AgentError {
        match control.admission_stop_reason() {
            Some(crate::model::OperationStopReason::Deadline) => {
                super::AgentError::Provider(crate::provider::ProviderError::DeadlineExceeded)
            }
            Some(_) => super::AgentError::Provider(crate::provider::ProviderError::Cancelled),
            None => super::AgentError::Other(message),
        }
    }

    pub(super) fn hook_warning(&self, emit: super::EventSink<'_>, message: &str) {
        emit(super::Event::Notification {
            kind: None,
            task_id: String::new(),
            text: self.redactor.redact_string(&format!("[hook] {message}")),
            usage: Default::default(),
            present: false,
            cached_input_tokens_present: false,
        });
    }

    /// Shows a hook response `notice` as a notification; absent notices emit nothing.
    pub(super) fn hook_notice(&self, emit: super::EventSink<'_>, response: &Response) {
        if let Some(text) = &response.notice {
            self.hook_warning(emit, text);
        }
    }

    pub fn claim_end_hooks(&self) -> bool {
        !self
            .options
            .hook_state
            .end_requested
            .swap(true, std::sync::atomic::Ordering::SeqCst)
    }
    pub async fn wait_end_hooks(&self) {
        self.options.hook_state.end_done.cancelled().await;
    }

    /// Called by the session owner on graceful close, never on runner replacement.
    pub async fn end_hooks(&self) {
        let state = &self.options.hook_state;
        state
            .end_requested
            .store(true, std::sync::atomic::Ordering::SeqCst);
        if state.ended.swap(true, std::sync::atomic::Ordering::SeqCst) {
            state.end_done.cancelled().await;
            return;
        }
        if self.options.hooks.is_some() && state.start.get().is_some() {
            let token = tokio_util::sync::CancellationToken::new();
            let _ = self
                .hook("session_end", serde_json::json!({}), &token)
                .await;
        }
        state.end_done.cancel();
    }
}

/// Converts messages to the `turn_end` `turn` JSON: text, tool call and tool
/// result blocks only; messages without such blocks are skipped.
pub(super) fn turn_json(messages: &[Message]) -> Value {
    let messages: Vec<Value> = messages
        .iter()
        .filter_map(|message| {
            let role = match message.role {
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::Tool => "tool",
                Role::Context => "context",
                Role::Other(_) => return None,
            };
            let blocks: Vec<Value> = message.blocks.iter().filter_map(turn_block_json).collect();
            (!blocks.is_empty()).then(|| serde_json::json!({"role": role, "blocks": blocks}))
        })
        .collect();
    serde_json::json!({ "messages": messages })
}

fn turn_block_json(block: &Block) -> Option<Value> {
    match block.block_type {
        BlockType::Text => Some(serde_json::json!({"type":"text","text":block.text})),
        BlockType::ToolCall => {
            // Absent or unparseable arguments become an empty object.
            let arguments = block
                .arguments
                .as_ref()
                .and_then(|raw| serde_json::from_str(raw.get()).ok())
                .filter(Value::is_object)
                .unwrap_or_else(|| Value::Object(Default::default()));
            Some(
                serde_json::json!({"type":"tool_call","id":block.tool_call_id,"name":block.tool_name,"arguments":arguments}),
            )
        }
        BlockType::ToolResult => Some(serde_json::json!({
            "type":"tool_result","id":block.tool_call_id,"name":block.tool_name,"text":block.text,"is_error":block.is_error
        })),
        _ => None,
    }
}
