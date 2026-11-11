//! Writes and validates probe questions with the profile model.

use otto_core::provider::Provider;
use otto_core::view_tree::{LogKind, LogMessage, NodeAddress};
use serde::{Deserialize, Serialize};

use super::model_calls::{EvalEnvironment, complete_text, parse_json_object};
use super::node_compression::prefix_at_boundary;
use super::output_files::OutputDir;

pub const PROBES_FILE: &str = "probes.jsonl";
/// A view line must cover at least this many messages for its messages to be candidates.
pub const MINIMUM_LINE_MESSAGES: u64 = 8;
/// Messages of context on each side of the source in the generation prompt.
const NEIGHBOUR_MESSAGES: u64 = 2;
const NEIGHBOUR_TEXT_BYTES: usize = 3000;
const SOURCE_TEXT_BYTES: usize = 6000;
const MINIMUM_SOURCE_BYTES: usize = 20;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Probe {
    pub id: String,
    pub question: String,
    pub expected: String,
    pub sources: Vec<u64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dropped: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

const GENERATION_SYSTEM: &str = "You write one test question about a recorded conversation between a user and a coding \
agent. You receive the target message and its neighbours. Write one question whose answer is stated in the target \
message only, for a reader who has the conversation in compressed form and must look the detail up. The question must \
be self-contained, must not contain the answer, and must ask for a specific fact (a name, number, path, error text, \
decision or reason). Reply with one JSON object and nothing else: {\"question\": string, \"expected\": string}. \
`expected` is the answer in one sentence.";

const VALIDATION_SYSTEM: &str = "You check a test question about a recorded conversation. You receive the source \
messages, the question and the expected answer. Reply `drop` when the question cannot be answered from the sources, \
when the expected answer is not stated in the sources, or when the question already contains the answer; otherwise \
reply `keep`. Reply with one JSON object and nothing else: {\"verdict\": \"keep\" or \"drop\", \"reason\": string}.";

/// Message ids that may be probe sources: inside a final view line of at least
/// [`MINIMUM_LINE_MESSAGES`] messages, and a user message, an assistant message or a tool result
/// whose text holds an error. At most `count` ids, spread evenly over the log.
pub fn candidate_sources(log: &[LogMessage], final_view: &[NodeAddress], count: usize) -> Vec<u64> {
    let mut candidates = Vec::new();
    for line in final_view
        .iter()
        .filter(|line| line.n >= MINIMUM_LINE_MESSAGES)
    {
        for id in line.id..line.end_id() {
            let Some(message) = log.get(id as usize) else {
                continue;
            };
            let qualifies = match message.kind {
                LogKind::User | LogKind::Assistant => true,
                LogKind::ToolResult => message.text.to_lowercase().contains("error"),
                LogKind::ToolCall | LogKind::Context => false,
            };
            if qualifies && message.text.len() >= MINIMUM_SOURCE_BYTES {
                candidates.push(id);
            }
        }
    }
    candidates.sort_unstable();
    candidates.dedup();
    let take = count.min(candidates.len());
    (0..take)
        .map(|index| candidates[index * candidates.len() / take])
        .collect()
}

fn render_message(message: &LogMessage, limit: usize) -> String {
    let text = prefix_at_boundary(&message.text, limit);
    let cut = if text.len() < message.text.len() {
        " [cut]"
    } else {
        ""
    };
    format!("[{}] {}: {text}{cut}", message.id, message.kind.name())
}

fn generation_request(log: &[LogMessage], source: u64) -> String {
    let first = source.saturating_sub(NEIGHBOUR_MESSAGES);
    let last = (source + NEIGHBOUR_MESSAGES).min(log.len() as u64 - 1);
    let mut text = format!("Target message id: {source}\n\n");
    for id in first..=last {
        text.push_str(&render_message(&log[id as usize], NEIGHBOUR_TEXT_BYTES));
        text.push_str("\n\n");
    }
    text
}

fn string_field(value: &serde_json::Value, name: &str) -> Option<String> {
    value
        .get(name)?
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

/// Generates and validates probes for the candidate sources, appending each to `probes.jsonl`.
pub async fn generate_probes<P: Provider + Clone + Send + Sync>(
    env: &EvalEnvironment<P>,
    log: &[LogMessage],
    final_view: &[NodeAddress],
    count: usize,
    out: &OutputDir,
) -> Result<Vec<Probe>, String> {
    let sources = candidate_sources(log, final_view, count);
    if sources.is_empty() {
        return Err(format!(
            "no probe candidates: no final view line covers {MINIMUM_LINE_MESSAGES} or more messages; \
             rebuild with smaller --view-low and --view-high"
        ));
    }
    out.write_jsonl::<Probe>(PROBES_FILE, &[])?;
    let mut probes = Vec::new();
    for (index, source) in sources.into_iter().enumerate() {
        let mut probe = Probe {
            id: format!("p{:03}", index + 1),
            question: String::new(),
            expected: String::new(),
            sources: vec![source],
            dropped: false,
            reason: None,
        };
        match write_and_validate(env, log, &mut probe).await {
            Ok(None) => {}
            Ok(Some(reason)) => {
                probe.dropped = true;
                probe.reason = Some(reason);
            }
            Err(error) => return Err(format!("probe {}: {error}", probe.id)),
        }
        out.append_jsonl(PROBES_FILE, &probe)?;
        probes.push(probe);
    }
    Ok(probes)
}

/// `Ok(None)` keeps the probe; `Ok(Some(reason))` drops it. `Err` is a provider failure.
async fn write_and_validate<P: Provider + Clone + Send + Sync>(
    env: &EvalEnvironment<P>,
    log: &[LogMessage],
    probe: &mut Probe,
) -> Result<Option<String>, String> {
    let source = probe.sources[0];
    let reply = complete_text(env, GENERATION_SYSTEM, &generation_request(log, source)).await?;
    let written = match parse_json_object(&reply) {
        Ok(value) => value,
        Err(reason) => return Ok(Some(format!("generation: {reason}"))),
    };
    let (Some(question), Some(expected)) = (
        string_field(&written, "question"),
        string_field(&written, "expected"),
    ) else {
        return Ok(Some(
            "generation: the reply lacks a non-empty question and expected answer".into(),
        ));
    };
    probe.question = question;
    probe.expected = expected;
    let request = format!(
        "Source messages:\n{}\n\nQuestion: {}\nExpected answer: {}",
        render_message(&log[source as usize], SOURCE_TEXT_BYTES),
        probe.question,
        probe.expected
    );
    let reply = complete_text(env, VALIDATION_SYSTEM, &request).await?;
    let checked = match parse_json_object(&reply) {
        Ok(value) => value,
        Err(reason) => return Ok(Some(format!("validation: {reason}"))),
    };
    let reason = string_field(&checked, "reason").unwrap_or_default();
    Ok(match string_field(&checked, "verdict").as_deref() {
        Some("keep") => None,
        Some("drop") => Some(format!("validation: {reason}")),
        _ => Some("validation: the verdict is neither keep nor drop".into()),
    })
}
