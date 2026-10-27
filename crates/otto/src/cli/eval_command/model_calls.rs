//! Provider access of the evaluation: one place that builds `Agent`s over the shared provider
//! client, so redaction and the egress guard apply to every call, and that meters usage.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use chrono::Utc;
use otto_core::agent::oneshot::TextRequest;
use otto_core::agent::redactor::Redactor;
use otto_core::agent::{Agent, Event, Options};
use otto_core::config::resolve::CompactionRuntime;
use otto_core::model::{Message, Role};
use otto_core::operation::OperationControl;
use otto_core::provider::{Provider, ProviderError, ProviderSettlement, Request, StreamSink};
use otto_core::session::{MemorySession, Session};
use otto_core::tool::ToolExecutor;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use super::tool_executors::NoTools;

/// What the evaluation needs from the runtime: the provider, the redactor and the settings that
/// the cost model reads.
pub struct EvalEnvironment<P> {
    pub provider: P,
    pub redactor: Redactor,
    pub provider_name: String,
    /// The profile model: probe generation, validation, answers and judging.
    pub profile_model: String,
    /// The compressor model unless `--model` overrides it.
    pub compressor_model: String,
    pub thinking: String,
    pub compaction: CompactionRuntime,
    pub cancel: CancellationToken,
}

/// Provider usage summed over the calls of one agent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub provider_calls: u64,
    pub input_tokens: u64,
    pub cached_tokens: u64,
    pub output_tokens: u64,
}

impl TokenUsage {
    pub fn add(&mut self, other: &Self) {
        self.provider_calls += other.provider_calls;
        self.input_tokens += other.input_tokens;
        self.cached_tokens += other.cached_tokens;
        self.output_tokens += other.output_tokens;
    }
}

/// Counts provider calls and sums their usage; refuses calls beyond `max_calls`.
#[derive(Default)]
pub struct Meter {
    max_calls: u32,
    calls: AtomicU32,
    input_tokens: AtomicU64,
    cached_tokens: AtomicU64,
    output_tokens: AtomicU64,
}

impl Meter {
    fn usage(&self) -> TokenUsage {
        TokenUsage {
            provider_calls: u64::from(self.calls.load(Ordering::SeqCst))
                .min(u64::from(self.max_calls)),
            input_tokens: self.input_tokens.load(Ordering::SeqCst),
            cached_tokens: self.cached_tokens.load(Ordering::SeqCst),
            output_tokens: self.output_tokens.load(Ordering::SeqCst),
        }
    }
}

/// A provider that forwards to `inner` through a [`Meter`].
#[derive(Clone)]
pub struct MeteredProvider<P> {
    inner: P,
    meter: Arc<Meter>,
}

#[async_trait::async_trait]
impl<P: Provider + Send + Sync> Provider for MeteredProvider<P> {
    fn capabilities(&self) -> otto_core::provider::ProviderCapabilities {
        self.inner.capabilities()
    }

    async fn complete(
        &self,
        request: &Request,
        emit: StreamSink<'_>,
        control: &dyn OperationControl,
    ) -> ProviderSettlement {
        let number = self.meter.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if number > self.meter.max_calls {
            return ProviderSettlement::failed(
                ProviderError::Other(format!(
                    "provider call limit of {} reached",
                    self.meter.max_calls
                )),
                0,
                otto_core::model::EffectCertainty::NotStarted,
            );
        }
        let settlement = self.inner.complete(request, emit, control).await;
        if let Ok(response) = &settlement.result
            && let Some(usage) = response.message.usage
        {
            self.meter
                .input_tokens
                .fetch_add(usage.input_tokens.max(0) as u64, Ordering::SeqCst);
            self.meter
                .cached_tokens
                .fetch_add(usage.cached_input_tokens.max(0) as u64, Ordering::SeqCst);
            self.meter
                .output_tokens
                .fetch_add(usage.output_tokens.max(0) as u64, Ordering::SeqCst);
        }
        settlement
    }
}

/// Model, prompt and call limit of one agent.
pub struct AgentSpec {
    pub model: String,
    pub system_prompt: String,
    pub cache_key: String,
    pub max_provider_calls: u32,
}

/// An agent over a fresh or seeded in-memory session. Automatic compaction is off.
pub struct EvalAgent<P, T> {
    agent: Agent<MeteredProvider<P>, T, MemorySession>,
    meter: Arc<Meter>,
    redactor: Redactor,
    cancel: CancellationToken,
}

impl<P: Provider + Clone + Send + Sync, T: ToolExecutor> EvalAgent<P, T> {
    pub fn new(
        env: &EvalEnvironment<P>,
        spec: AgentSpec,
        tools: T,
        session: MemorySession,
    ) -> Self {
        let meter = Arc::new(Meter {
            max_calls: spec.max_provider_calls,
            ..Meter::default()
        });
        let counter = AtomicU64::new(0);
        let options = Options {
            model: spec.model,
            provider_name: env.provider_name.clone(),
            thinking: env.thinking.clone(),
            system_prompt: spec.system_prompt,
            cache_key: spec.cache_key,
            now: Box::new(Utc::now),
            new_id: Box::new(move || {
                format!(
                    "{:032x}",
                    counter.fetch_add(1, Ordering::SeqCst) + 1_000_000
                )
            }),
            ..Options::default()
        };
        let provider = MeteredProvider {
            inner: env.provider.clone(),
            meter: Arc::clone(&meter),
        };
        Self {
            agent: Agent::with_redactor(provider, tools, session, options, env.redactor.clone()),
            meter,
            redactor: env.redactor.clone(),
            cancel: env.cancel.clone(),
        }
    }

    /// Runs one user message through the loop. The error text is redacted.
    pub async fn run(&self, user_text: &str) -> Result<(), String> {
        let mut sink = |_event: Event| {};
        self.agent
            .run(user_text, &mut sink, &self.cancel)
            .await
            .map_err(|error| self.redactor.redact_string(&error.to_string()))
    }

    /// The text of the newest assistant message that has text, or empty.
    pub fn last_assistant_text(&self) -> String {
        self.agent
            .session()
            .messages()
            .iter()
            .rev()
            .filter(|message| message.role == Role::Assistant)
            .map(Message::text)
            .find(|text| !text.trim().is_empty())
            .unwrap_or_default()
    }

    pub fn usage(&self) -> TokenUsage {
        self.meter.usage()
    }
}

/// A session that holds `messages` already.
pub async fn seeded_session(messages: &[Message]) -> Result<MemorySession, String> {
    let session = MemorySession::new();
    for message in messages {
        session
            .append(message.clone())
            .await
            .map_err(|error| format!("seed baseline context: {error}"))?;
    }
    Ok(session)
}

/// Largest reply accepted from a tool-less completion.
const COMPLETION_MAXIMUM_BYTES: usize = 64 * 1024;

/// One tool-less completion on the profile model. Returns the reply text.
pub async fn complete_text<P: Provider + Clone + Send + Sync>(
    env: &EvalEnvironment<P>,
    system_prompt: &str,
    user_text: &str,
) -> Result<String, String> {
    let options = Options {
        model: env.profile_model.clone(),
        provider_name: env.provider_name.clone(),
        thinking: env.thinking.clone(),
        now: Box::new(Utc::now),
        ..Options::default()
    };
    let agent = Agent::with_redactor(
        env.provider.clone(),
        NoTools,
        MemorySession::new(),
        options,
        env.redactor.clone(),
    );
    let request = TextRequest {
        system_prompt,
        user_text,
        maximum_bytes: COMPLETION_MAXIMUM_BYTES,
    };
    let mut sink = |_event: Event| {};
    agent
        .complete_text(&request, &mut sink, &env.cancel)
        .await
        .map(|response| response.text)
        .map_err(|error| env.redactor.redact_string(&error.to_string()))
}

/// Parses the first JSON object in a model reply.
pub fn parse_json_object(reply: &str) -> Result<serde_json::Value, String> {
    let (Some(start), Some(end)) = (reply.find('{'), reply.rfind('}')) else {
        return Err("the reply holds no JSON object".into());
    };
    if end < start {
        return Err("the reply holds no JSON object".into());
    }
    match serde_json::from_str::<serde_json::Value>(&reply[start..=end]) {
        Ok(value) if value.is_object() => Ok(value),
        Ok(_) => Err("the reply JSON is not an object".into()),
        Err(error) => Err(format!("the reply is not valid JSON: {error}")),
    }
}
