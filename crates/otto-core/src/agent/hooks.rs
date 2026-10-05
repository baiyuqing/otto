//! Optional extension boundary; core never launches commands.
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
    pub(super) async fn hook(
        &self,
        event: &str,
        data: Value,
        control: &dyn OperationControl,
    ) -> Result<Response, String> {
        if !self.redactor.allows_dynamic_content() {
            return Ok(Response::default());
        }
        let mut data = data;
        if let Some(object) = data.as_object_mut() {
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
            Some(hooks) => hooks.run(event, data, control).await,
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
        });
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
