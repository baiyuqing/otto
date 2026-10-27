//! Answers each kept probe twice (view and baseline), judges both and classifies view failures.

use std::sync::Arc;

use otto_core::model::Message;
use otto_core::provider::Provider;
use otto_core::session::MemorySession;
use otto_core::view_tree::{LogMessage, NodeAddress, SYSTEM_PROMPT, SummaryTree, render_line};
use serde::{Deserialize, Serialize};

use super::answer_judging::{
    Verdict, classify_failure, judge_answer, opened_source, reached_source,
};
use super::model_calls::{AgentSpec, EvalAgent, EvalEnvironment, TokenUsage, seeded_session};
use super::output_files::OutputDir;
use super::probe_generation::Probe;
use super::tool_executors::{NoTools, ToolCallRecord, TreeTools};

pub const ANSWERS_FILE: &str = "answers.jsonl";

const BASELINE_SYSTEM: &str = "Answer the question about the conversation above in one or two sentences. If the \
conversation does not contain the answer, say so.";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AnswerRecord {
    pub text: String,
    /// The error that stopped the answer, such as the step limit; empty when none.
    #[serde(default)]
    pub error: String,
    pub usage: TokenUsage,
}

/// Everything recorded for one probe.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeResult {
    pub probe: Probe,
    pub view_answer: AnswerRecord,
    pub view_verdict: Verdict,
    pub zoom_calls: Vec<ToolCallRecord>,
    pub reached: bool,
    pub opened_source: bool,
    /// `missing`, `not_zoomed` or `other` for a wrong view answer; empty otherwise.
    pub failure_class: String,
    pub baseline_answer: AnswerRecord,
    pub baseline_verdict: Verdict,
}

/// Inputs shared by every probe.
pub struct AnsweringContext {
    pub tree: Arc<SummaryTree>,
    pub log: Arc<Vec<LogMessage>>,
    pub final_view: Vec<NodeAddress>,
    pub baseline_messages: Vec<Message>,
    pub session_id: String,
    pub max_steps: u32,
}

impl AnsweringContext {
    fn rendered_view(&self) -> String {
        self.final_view
            .iter()
            .filter_map(|line| self.tree.get(*line).map(|text| render_line(*line, text)))
            .collect()
    }
}

fn answer_record(text: String, error: Result<(), String>, usage: TokenUsage) -> AnswerRecord {
    AnswerRecord {
        text,
        error: error.err().unwrap_or_default(),
        usage,
    }
}

async fn view_answer<P: Provider + Clone + Send + Sync>(
    env: &EvalEnvironment<P>,
    context: &AnsweringContext,
    probe: &Probe,
) -> (AnswerRecord, Vec<ToolCallRecord>) {
    let tools = TreeTools::new(Arc::clone(&context.tree), Arc::clone(&context.log));
    let recorded_calls = tools.recorded_calls();
    let agent = EvalAgent::new(
        env,
        AgentSpec {
            model: env.profile_model.clone(),
            system_prompt: SYSTEM_PROMPT.to_string(),
            cache_key: format!("view-tree-eval-{}", context.session_id),
            max_provider_calls: context.max_steps,
        },
        tools,
        MemorySession::new(),
    );
    let message = format!(
        "Conversation view:\n{}\nQuestion: {}\nAnswer in one or two sentences.",
        context.rendered_view(),
        probe.question
    );
    let result = agent.run(&message).await;
    let calls = recorded_calls
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    (
        answer_record(agent.last_assistant_text(), result, agent.usage()),
        calls,
    )
}

async fn baseline_answer<P: Provider + Clone + Send + Sync>(
    env: &EvalEnvironment<P>,
    context: &AnsweringContext,
    probe: &Probe,
) -> Result<AnswerRecord, String> {
    let session = seeded_session(&context.baseline_messages).await?;
    let agent = EvalAgent::new(
        env,
        AgentSpec {
            model: env.profile_model.clone(),
            system_prompt: BASELINE_SYSTEM.to_string(),
            cache_key: format!("view-tree-eval-{}-baseline", context.session_id),
            max_provider_calls: 4,
        },
        NoTools,
        session,
    );
    let result = agent.run(&probe.question).await;
    Ok(answer_record(
        agent.last_assistant_text(),
        result,
        agent.usage(),
    ))
}

async fn answer_probe<P: Provider + Clone + Send + Sync>(
    env: &EvalEnvironment<P>,
    context: &AnsweringContext,
    probe: &Probe,
) -> Result<ProbeResult, String> {
    let (view_answer, zoom_calls) = view_answer(env, context, probe).await;
    let baseline_answer = baseline_answer(env, context, probe).await?;
    let view_verdict = judge_answer(env, probe, &view_answer.text).await?;
    let baseline_verdict = judge_answer(env, probe, &baseline_answer.text).await?;
    let failure_class = if view_verdict.is_wrong() {
        classify_failure(env, &context.tree, &context.final_view, probe, &zoom_calls).await?
    } else {
        String::new()
    };
    Ok(ProbeResult {
        reached: reached_source(&zoom_calls, &probe.sources),
        opened_source: probe
            .sources
            .iter()
            .any(|source| opened_source(&zoom_calls, *source)),
        probe: probe.clone(),
        view_answer,
        view_verdict,
        zoom_calls,
        failure_class,
        baseline_answer,
        baseline_verdict,
    })
}

/// Answers the kept probes that `answers.jsonl` does not hold yet, appending each result, and
/// returns the results of all kept probes in probe order.
pub async fn run_probes<P: Provider + Clone + Send + Sync>(
    env: &EvalEnvironment<P>,
    context: &AnsweringContext,
    probes: &[Probe],
    out: &OutputDir,
) -> Result<Vec<ProbeResult>, String> {
    let mut results: Vec<ProbeResult> = out.read_jsonl(ANSWERS_FILE)?;
    for probe in probes.iter().filter(|probe| !probe.dropped) {
        if results.iter().any(|result| result.probe.id == probe.id) {
            continue;
        }
        if env.cancel.is_cancelled() {
            return Err("run cancelled".into());
        }
        let result = answer_probe(env, context, probe)
            .await
            .map_err(|error| format!("probe {}: {error}", probe.id))?;
        out.append_jsonl(ANSWERS_FILE, &result)?;
        results.push(result);
    }
    let order: Vec<&str> = probes.iter().map(|probe| probe.id.as_str()).collect();
    results.retain(|result| order.contains(&result.probe.id.as_str()));
    results.sort_by_key(|result| order.iter().position(|id| *id == result.probe.id));
    Ok(results)
}
