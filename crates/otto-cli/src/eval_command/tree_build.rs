//! Builds the summary tree with the compressor model, in the order of `BuildSchedule`.

use std::sync::Arc;

use otto_core::provider::Provider;
use otto_core::view_tree::{
    BuildSchedule, LogMessage, NODE_MAXIMUM_BYTES, NodeAddress, NodeInput, SummaryTree, View,
    leaf_task, merge_task,
};
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use super::model_calls::{EvalEnvironment, TokenUsage};
use super::node_compression::{CompressionJob, CompressionResult, compress_node};
use super::output_files::OutputDir;

pub const TREE_FILE: &str = "tree.jsonl";
pub const CALLS_FILE: &str = "build_calls.jsonl";
/// Compressor context view limits, as in the gist.
pub const COMPRESSOR_VIEW_LOW_BYTES: usize = 16 * 1024;
pub const COMPRESSOR_VIEW_HIGH_BYTES: usize = 32 * 1024;

/// One line of `tree.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeRecord {
    pub id: u64,
    pub n: u64,
    pub text: String,
}

/// One compressor call, as written to `build.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallRecord {
    pub id: u64,
    pub n: u64,
    pub kind: String,
    pub model: String,
    pub attempts: u32,
    pub over_limit_replies: u32,
    pub tool_call_violations: u32,
    pub usage: TokenUsage,
}

pub struct BuildSettings {
    pub model: String,
    pub concurrency: usize,
    pub session_id: String,
}

/// What a build leaves behind.
pub struct BuildOutcome {
    pub tree: SummaryTree,
    /// Nodes that were already in `tree.jsonl`.
    pub resumed_nodes: usize,
    /// Nodes whose input fit in the limit and needed no model call, in this run.
    pub ready_nodes: usize,
}

/// The compressor's own view: built leaves in order, shrunk with the current message count.
struct CompressorContext {
    view: View,
    log_length: u64,
}

impl CompressorContext {
    fn refresh(&mut self, tree: &SummaryTree) {
        while self.view.covered() < self.log_length
            && tree.contains(NodeAddress::leaf(self.view.covered()))
        {
            let next = self.view.covered();
            self.view.append(next);
            self.view.shrink(next + 1, tree);
        }
        self.view.shrink(self.view.covered(), tree);
    }
}

struct BuildState<'a> {
    tree: SummaryTree,
    schedule: BuildSchedule,
    context: CompressorContext,
    out: &'a OutputDir,
}

impl BuildState<'_> {
    fn finish(&mut self, address: NodeAddress, text: String) -> Result<(), String> {
        self.out.append_jsonl(
            TREE_FILE,
            &NodeRecord {
                id: address.id,
                n: address.n,
                text: text.clone(),
            },
        )?;
        self.tree.insert(address, text)?;
        self.schedule.complete(address)?;
        self.context.refresh(&self.tree);
        Ok(())
    }
}

type JobResult = (CompressionJob, Result<CompressionResult, String>);

/// Builds every node of the log, resuming from `tree.jsonl` in `out`.
pub async fn build_tree<P: Provider + Clone + Send + Sync + 'static>(
    env: &Arc<EvalEnvironment<P>>,
    log: &[LogMessage],
    settings: &BuildSettings,
    out: &OutputDir,
) -> Result<BuildOutcome, String> {
    let log_length = log.len() as u64;
    let mut state = BuildState {
        tree: SummaryTree::new(),
        schedule: BuildSchedule::new(log_length, settings.concurrency),
        context: CompressorContext {
            view: View::new(COMPRESSOR_VIEW_LOW_BYTES, COMPRESSOR_VIEW_HIGH_BYTES),
            log_length,
        },
        out,
    };
    let mut existing: Vec<NodeRecord> = out.read_jsonl(TREE_FILE)?;
    existing.sort_by_key(|record| (record.n, record.id));
    let resumed_nodes = existing.len();
    for record in existing {
        let address = NodeAddress::new(record.id, record.n)?;
        state.tree.insert(address, record.text)?;
        state.schedule.complete(address)?;
    }
    state.context.refresh(&state.tree);

    let permits = Arc::new(Semaphore::new(settings.concurrency));
    let mut running: JoinSet<JobResult> = JoinSet::new();
    let mut ready_nodes = 0;
    loop {
        loop {
            let ready = state.schedule.take_ready();
            if ready.is_empty() {
                break;
            }
            for address in ready {
                let input = state.tree.node_input(log, address)?;
                if let NodeInput::Ready(text) = input {
                    state.finish(address, text)?;
                    ready_nodes += 1;
                    continue;
                }
                let context = state
                    .context
                    .view
                    .render_until(address.last_id(), &state.tree);
                let Some(job) = CompressionJob::from_input(input, context) else {
                    continue;
                };
                let (env, permits) = (Arc::clone(env), Arc::clone(&permits));
                let (model, session_id) = (settings.model.clone(), settings.session_id.clone());
                running.spawn(async move {
                    let _permit = permits.acquire_owned().await;
                    let result = compress_node(&env, &model, &session_id, &job).await;
                    (job, result)
                });
            }
        }
        if state.schedule.is_done() {
            break;
        }
        if env.cancel.is_cancelled() {
            return Err("build cancelled".into());
        }
        let Some(joined) = running.join_next().await else {
            return Err("build stopped with unbuilt nodes and no running call".into());
        };
        let (job, result) = joined.map_err(|error| format!("compressor task failed: {error}"))?;
        let result = result?;
        out.append_jsonl(
            CALLS_FILE,
            &CallRecord {
                id: job.address.id,
                n: job.address.n,
                kind: job.kind.to_string(),
                model: settings.model.clone(),
                attempts: result.attempts,
                over_limit_replies: result.over_limit_replies,
                tool_call_violations: result.tool_call_violations,
                usage: result.usage,
            },
        )?;
        state.finish(job.address, result.text)?;
    }
    Ok(BuildOutcome {
        tree: state.tree,
        resumed_nodes,
        ready_nodes,
    })
}

/// What `--dry-run` prints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DryRunEstimate {
    pub log_messages: usize,
    pub log_bytes: usize,
    pub leaf_calls: usize,
    pub merge_calls: usize,
    pub task_input_bytes: usize,
}

/// Counts the model calls a build needs. Leaf inputs are exact. A merge depends on the model's
/// replies, so the estimate assumes each replied node is exactly [`NODE_MAXIMUM_BYTES`] long.
pub fn estimate_build(log: &[LogMessage]) -> Result<DryRunEstimate, String> {
    let mut addresses = SummaryTree::addresses_for_log(log.len() as u64);
    addresses.sort_by_key(|address| (address.n, address.id));
    let mut tree = SummaryTree::new();
    let mut estimate = DryRunEstimate {
        log_messages: log.len(),
        log_bytes: log.iter().map(|message| message.text.len()).sum(),
        leaf_calls: 0,
        merge_calls: 0,
        task_input_bytes: 0,
    };
    let placeholder = "x".repeat(NODE_MAXIMUM_BYTES);
    for address in addresses {
        let text = match tree.node_input(log, address)? {
            NodeInput::Ready(text) => text,
            NodeInput::Leaf(task) => {
                estimate.leaf_calls += 1;
                estimate.task_input_bytes += leaf_task(address.id, task.kind, &task.text).len();
                placeholder.clone()
            }
            NodeInput::Merge(task) => {
                estimate.merge_calls += 1;
                estimate.task_input_bytes +=
                    merge_task(&task.first, &task.second, address.id, address.last_id()).len();
                placeholder.clone()
            }
        };
        tree.insert(address, text)?;
    }
    Ok(estimate)
}
