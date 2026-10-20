//! Converts the `turn_end` hook `turn` value into memory observations.

use otto_core::memory::{
    Content, MAX_BATCH_BYTES, MAX_CONTENT_BYTES, MAX_ITEMS, Observation, SourceRef, Trust,
};
use serde_json::{Value, json};

/// One observation per text, tool_call and tool_result block. Text over
/// `MAX_CONTENT_BYTES` is split at UTF-8 character boundaries; `event_id` is
/// `<turn_id>:<message>:<block>:<part>`. Text blocks lose the control
/// characters `Observation::validate` rejects (all but newline, carriage
/// return and tab; tool JSON escapes them already). Parts that still fail it,
/// such as blank text, are left out, because one invalid observation fails the
/// whole request.
pub(super) fn turn_observations(turn_id: &str, turn: &Value) -> Vec<Observation> {
    let mut observations = Vec::new();
    let messages = turn["messages"].as_array().into_iter().flatten();
    for (message_index, message) in messages.enumerate() {
        let message_role = message["role"].as_str().unwrap_or_default();
        let blocks = message["blocks"].as_array().into_iter().flatten();
        for (block_index, block) in blocks.enumerate() {
            let (role, content) = match block["type"].as_str() {
                Some("text") => {
                    let role = if message_role == "tool" { "tool_result" } else { message_role };
                    let text = block["text"].as_str().unwrap_or_default();
                    let text = text
                        .chars()
                        .filter(|c| !c.is_control() || matches!(c, '\n' | '\r' | '\t'))
                        .collect();
                    (role, text)
                }
                Some("tool_call") => (
                    "tool_call",
                    json!({"id": block["id"], "name": block["name"], "arguments": block["arguments"]})
                        .to_string(),
                ),
                Some("tool_result") => (
                    "tool_result",
                    json!({
                        "id": block["id"], "name": block["name"],
                        "text": block["text"], "is_error": block["is_error"]
                    })
                    .to_string(),
                ),
                _ => continue,
            };
            let trust = if role == "tool_result" {
                Trust::External
            } else {
                Trust::Local
            };
            for (part_index, part) in text_parts(&content).into_iter().enumerate() {
                let event_id = format!("{turn_id}:{message_index}:{block_index}:{part_index}");
                let observation = Observation {
                    event_id: event_id.clone(),
                    content: Content::Text(part.to_owned()),
                    source: SourceRef {
                        id: event_id,
                        role: role.to_owned(),
                        occurred_at: None,
                        trust,
                    },
                };
                if observation.validate().is_ok() {
                    observations.push(observation);
                }
            }
        }
    }
    observations
}

/// Consecutive parts of at most `MAX_CONTENT_BYTES` bytes, split at character
/// boundaries. Empty text gives no parts.
fn text_parts(text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let mut end = rest.len().min(MAX_CONTENT_BYTES);
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        let (part, tail) = rest.split_at(end);
        parts.push(part);
        rest = tail;
    }
    parts
}

/// Consecutive batches of at most `MAX_ITEMS` observations and
/// `MAX_BATCH_BYTES` bytes of content, in order.
pub(super) fn observation_batches(observations: Vec<Observation>) -> Vec<Vec<Observation>> {
    let mut batches: Vec<Vec<Observation>> = Vec::new();
    let mut batch_bytes = 0;
    for observation in observations {
        let bytes = observation.content.byte_len();
        match batches.last_mut() {
            Some(batch) if batch.len() < MAX_ITEMS && batch_bytes + bytes <= MAX_BATCH_BYTES => {
                batch.push(observation);
                batch_bytes += bytes;
            }
            _ => {
                batches.push(vec![observation]);
                batch_bytes = bytes;
            }
        }
    }
    batches
}
