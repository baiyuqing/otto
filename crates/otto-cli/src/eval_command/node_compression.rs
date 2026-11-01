//! One compressor call: a fresh conversation that turns a leaf or merge task into node text.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use otto_core::provider::Provider;
use otto_core::session::MemorySession;
use otto_core::view_tree::{
    NODE_MAXIMUM_BYTES, NodeAddress, NodeInput, SYSTEM_PROMPT, leaf_task, merge_task, too_long_task,
};

use super::model_calls::{AgentSpec, EvalAgent, EvalEnvironment, TokenUsage};
use super::tool_executors::CompressorTools;

/// Attempts per node, the first reply included.
pub const MAXIMUM_ATTEMPTS: u32 = 5;
/// Provider calls one node may use, tool-call detours included.
const MAXIMUM_PROVIDER_CALLS: u32 = 16;

/// A node that needs a model call.
pub struct CompressionJob {
    pub address: NodeAddress,
    pub kind: &'static str,
    /// The leaf or merge task text.
    pub task: String,
    /// The compressor's context view up to the node's last message.
    pub context: String,
}

impl CompressionJob {
    /// `None` for a `Ready` input, which never calls the model.
    pub fn from_input(input: NodeInput, context: String) -> Option<Self> {
        match input {
            NodeInput::Ready(_) => None,
            NodeInput::Leaf(task) => Some(Self {
                address: task.address,
                kind: "leaf",
                task: leaf_task(task.address.id, task.kind, &task.text),
                context,
            }),
            NodeInput::Merge(task) => Some(Self {
                address: task.address,
                kind: "merge",
                task: merge_task(
                    &task.first,
                    &task.second,
                    task.address.id,
                    task.address.last_id(),
                ),
                context,
            }),
        }
    }

    /// The user message of the first attempt.
    pub fn first_message(&self) -> String {
        if self.context.is_empty() {
            return self.task.clone();
        }
        format!(
            "Context view of the messages before this task:\n{}\n{}",
            self.context, self.task
        )
    }
}

pub struct CompressionResult {
    pub text: String,
    pub attempts: u32,
    pub over_limit_replies: u32,
    pub tool_call_violations: u32,
    pub usage: TokenUsage,
}

/// Runs the compressor for `job` on `model`.
pub async fn compress_node<P: Provider + Clone + Send + Sync>(
    env: &EvalEnvironment<P>,
    model: &str,
    session_id: &str,
    job: &CompressionJob,
) -> Result<CompressionResult, String> {
    let violations = Arc::new(AtomicU32::new(0));
    let agent = EvalAgent::new(
        env,
        AgentSpec {
            model: model.to_string(),
            system_prompt: SYSTEM_PROMPT.to_string(),
            cache_key: format!("view-tree-eval-{session_id}-compressor"),
            max_provider_calls: MAXIMUM_PROVIDER_CALLS,
        },
        CompressorTools::new(Arc::clone(&violations)),
        MemorySession::new(),
    );
    let mut message = job.first_message();
    let mut best: Option<String> = None;
    let mut over_limit_replies = 0;
    let mut attempts = 0;
    while attempts < MAXIMUM_ATTEMPTS {
        attempts += 1;
        agent
            .run(&message)
            .await
            .map_err(|error| format!("compress node {}: {error}", job.address))?;
        let reply = agent.last_assistant_text().trim().to_string();
        if reply.is_empty() {
            return Err(format!(
                "compress node {}: the model returned no text",
                job.address
            ));
        }
        let fits = reply.len() <= NODE_MAXIMUM_BYTES;
        if best.as_ref().is_none_or(|kept| reply.len() < kept.len()) {
            best = Some(reply.clone());
        }
        if fits {
            break;
        }
        over_limit_replies += 1;
        message = too_long_task(reply.len(), prefix_at_boundary(&reply, NODE_MAXIMUM_BYTES));
    }
    Ok(CompressionResult {
        text: best.unwrap_or_default(),
        attempts,
        over_limit_replies,
        tool_call_violations: violations.load(Ordering::SeqCst),
        usage: agent.usage(),
    })
}

/// The longest prefix of `text` of at most `limit` bytes that ends on a character boundary.
pub fn prefix_at_boundary(text: &str, limit: usize) -> &str {
    let mut end = limit.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}
