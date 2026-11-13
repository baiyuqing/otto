//! Provider access of the evaluation: one place that builds `Agent`s over the shared provider
//! client, so redaction and the egress guard apply to every call, and that meters usage.

use chrono::Utc;
use otto_core::agent::oneshot::TextRequest;
use otto_core::agent::redactor::Redactor;
use otto_core::agent::{Agent, Event, Options};
use otto_core::config::resolve::CompactionRuntime;
use otto_core::provider::Provider;
use otto_core::session::MemorySession;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use super::tool_executors::NoTools;

/// What the evaluation needs from the runtime: the provider, the redactor and the model and
/// compaction settings.
pub struct EvalEnvironment<P> {
    pub provider: P,
    pub redactor: Redactor,
    pub provider_name: String,
    /// The profile model: task answers and judging.
    pub profile_model: String,
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
