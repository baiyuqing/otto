//! The provider wrapper of the evaluation: it enforces the provider call budget, sums usage
//! and logs the text of every request so a case can check what reached the model.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use otto_core::model::EffectCertainty;
use otto_core::operation::OperationControl;
use otto_core::provider::{
    Provider, ProviderCapabilities, ProviderError, ProviderSettlement, Request, StreamSink,
};

use crate::eval_command::model_calls::TokenUsage;

/// One request as the model received it, flattened to text for substring checks.
#[derive(Debug, Clone)]
pub struct CapturedRequest {
    /// The system prompt followed by every message text, newline separated.
    pub text: String,
    pub usage: TokenUsage,
}

/// The requests of a run in order. Cloning shares the log.
#[derive(Debug, Clone, Default)]
pub struct RequestLog(Arc<Mutex<Vec<CapturedRequest>>>);

impl RequestLog {
    pub fn len(&self) -> usize {
        self.0.lock().expect("request log").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The requests logged at or after `index`.
    pub fn since(&self, index: usize) -> Vec<CapturedRequest> {
        self.0
            .lock()
            .expect("request log")
            .iter()
            .skip(index)
            .cloned()
            .collect()
    }

    fn push(&self, request: CapturedRequest) {
        self.0.lock().expect("request log").push(request);
    }
}

/// Counts provider calls against `max_calls`, sums their usage and logs them.
#[derive(Clone)]
pub struct BudgetedProvider<P> {
    inner: P,
    max_calls: u32,
    calls: Arc<AtomicU32>,
    log: RequestLog,
}

impl<P> BudgetedProvider<P> {
    pub fn new(inner: P, max_calls: u32) -> (Self, RequestLog) {
        let log = RequestLog::default();
        (
            Self {
                inner,
                max_calls,
                calls: Arc::new(AtomicU32::new(0)),
                log: log.clone(),
            },
            log,
        )
    }

    /// Provider calls made so far, including refused ones.
    pub fn calls(&self) -> u32 {
        self.calls.load(Ordering::SeqCst)
    }
}

/// The text of `request` for substring checks: the system prompt, then each message's text.
pub fn request_text(request: &Request) -> String {
    let mut text = request.system_prompt.clone();
    for message in &request.messages {
        text.push('\n');
        text.push_str(&message.text());
    }
    text
}

#[async_trait::async_trait]
impl<P: Provider + Send + Sync> Provider for BudgetedProvider<P> {
    fn capabilities(&self) -> ProviderCapabilities {
        self.inner.capabilities()
    }

    async fn complete(
        &self,
        request: &Request,
        emit: StreamSink<'_>,
        control: &dyn OperationControl,
    ) -> ProviderSettlement {
        let number = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if number > self.max_calls {
            return ProviderSettlement::failed(
                ProviderError::Other(format!("provider call limit of {} reached", self.max_calls)),
                0,
                EffectCertainty::NotStarted,
            );
        }
        let settlement = self.inner.complete(request, emit, control).await;
        let mut usage = TokenUsage {
            provider_calls: 1,
            ..TokenUsage::default()
        };
        if let Ok(response) = &settlement.result
            && let Some(reported) = response.message.usage
        {
            usage.input_tokens = reported.input_tokens.max(0) as u64;
            usage.cached_tokens = reported.cached_input_tokens.max(0) as u64;
            usage.output_tokens = reported.output_tokens.max(0) as u64;
        }
        self.log.push(CapturedRequest {
            text: request_text(request),
            usage,
        });
        settlement
    }
}
