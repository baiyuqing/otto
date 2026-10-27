//! Judges answers against the expected answer and classifies wrong view answers.

use otto_core::provider::Provider;
use otto_core::view_tree::{NodeAddress, SummaryTree};
use serde::{Deserialize, Serialize};

use super::model_calls::{EvalEnvironment, complete_text, parse_json_object};
use super::probe_generation::Probe;
use super::tool_executors::ToolCallRecord;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verdict {
    /// `correct`, `partial` or `wrong`.
    pub verdict: String,
    pub reason: String,
}

impl Verdict {
    pub fn is_wrong(&self) -> bool {
        self.verdict == "wrong"
    }
}

const JUDGE_SYSTEM: &str = "You grade an answer to a question about a recorded conversation against the expected \
answer. `correct`: the answer states the expected fact. `partial`: it states part of the expected fact, or the fact \
with a wrong detail. `wrong`: it lacks the fact, contradicts it, or is empty. Judge only against the expected answer. \
Reply with one JSON object and nothing else: {\"verdict\": \"correct\" or \"partial\" or \"wrong\", \"reason\": one sentence}.";

const MENTION_SYSTEM: &str = "You receive an expected answer and the summaries of a chain of nodes that cover one \
message. Decide whether any summary mentions the item that the expected answer names (the specific name, number, \
path, error or decision), even without the full detail. Reply with one JSON object and nothing else: \
{\"mentions\": true or false, \"reason\": one sentence}.";

/// Judges `answer` with the profile model. A reply that is not valid is a `wrong` verdict whose
/// reason says so.
pub async fn judge_answer<P: Provider + Clone + Send + Sync>(
    env: &EvalEnvironment<P>,
    probe: &Probe,
    answer: &str,
) -> Result<Verdict, String> {
    let request = format!(
        "Question: {}\nExpected answer: {}\nAnswer to grade: {}",
        probe.question, probe.expected, answer
    );
    let reply = complete_text(env, JUDGE_SYSTEM, &request).await?;
    let invalid = |reason: String| Verdict {
        verdict: "wrong".into(),
        reason: format!("judge reply not usable: {reason}"),
    };
    let value = match parse_json_object(&reply) {
        Ok(value) => value,
        Err(reason) => return Ok(invalid(reason)),
    };
    let verdict = value
        .get("verdict")
        .and_then(|field| field.as_str())
        .map(str::to_lowercase)
        .unwrap_or_default();
    if !matches!(verdict.as_str(), "correct" | "partial" | "wrong") {
        return Ok(invalid(format!("verdict {verdict:?}")));
    }
    let reason = value
        .get("reason")
        .and_then(|field| field.as_str())
        .unwrap_or_default()
        .to_string();
    Ok(Verdict { verdict, reason })
}

/// The nodes from the final view line that covers `source` down to the leaf of `source`.
pub fn chain_to_source(final_view: &[NodeAddress], source: u64) -> Vec<NodeAddress> {
    let Some(line) = final_view
        .iter()
        .find(|line| line.id <= source && source < line.end_id())
    else {
        return Vec::new();
    };
    let mut chain = vec![*line];
    let mut current = *line;
    while let Some((first, second)) = current.children() {
        current = if source < second.id { first } else { second };
        chain.push(current);
    }
    chain
}

/// Whether the model opened message `source` with `zoom(source, 1)`.
pub fn opened_source(calls: &[ToolCallRecord], source: u64) -> bool {
    calls.iter().any(|call| {
        call.tool == "zoom" && !call.error && call.id == Some(source) && call.n == Some(1)
    })
}

/// Whether a zoom returned a node that covers a source.
pub fn reached_source(calls: &[ToolCallRecord], sources: &[u64]) -> bool {
    calls.iter().any(|call| {
        call.tool == "zoom"
            && !call.error
            && matches!((call.id, call.n), (Some(id), Some(n)) if sources.iter().any(|source| id <= *source && *source < id + n))
    })
}

/// `missing`, `not_zoomed` or `other` for a wrong view answer.
pub async fn classify_failure<P: Provider + Clone + Send + Sync>(
    env: &EvalEnvironment<P>,
    tree: &SummaryTree,
    final_view: &[NodeAddress],
    probe: &Probe,
    calls: &[ToolCallRecord],
) -> Result<String, String> {
    let mut summaries = String::new();
    for source in &probe.sources {
        for node in chain_to_source(final_view, *source) {
            if let Some(text) = tree.get(node) {
                summaries.push_str(&format!("{node}|{}\n", text.replace('\n', " ")));
            }
        }
    }
    let request = format!(
        "Expected answer: {}\nSummaries:\n{summaries}",
        probe.expected
    );
    let reply = complete_text(env, MENTION_SYSTEM, &request).await?;
    let mentioned = parse_json_object(&reply)
        .ok()
        .and_then(|value| value.get("mentions").and_then(serde_json::Value::as_bool));
    Ok(match mentioned {
        Some(false) => "missing",
        Some(true)
            if !probe
                .sources
                .iter()
                .any(|source| opened_source(calls, *source)) =>
        {
            "not_zoomed"
        }
        _ => "other",
    }
    .to_string())
}
