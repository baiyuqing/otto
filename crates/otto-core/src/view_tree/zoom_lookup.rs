//! The `zoom` and `date` tools over a built tree and the log.

use serde_json::value::RawValue;

use super::log_messages::LogMessage;
use super::node_address::NodeAddress;
use super::summary_tree::{SummaryTree, render_line};
use crate::model::ToolDefinition;

/// Opens node `id+n`.
///
/// For `n` > 1, returns the two child lines `id+n|text`, one per line. For `n` = 1, returns
/// message `id` whole as `kind: text`. Errors name the problem: `n` not a power of two or
/// below 1, `id` not a multiple of `n`, a range outside the log, or a node not built.
pub fn zoom(tree: &SummaryTree, log: &[LogMessage], id: u64, n: u64) -> Result<String, String> {
    let address = NodeAddress::new(id, n)?;
    if address.end_id() > log.len() as u64 {
        return Err(format!(
            "{address} is outside the log: the log has {} messages (ids 0 to {})",
            log.len(),
            log.len().saturating_sub(1)
        ));
    }
    let Some((first, second)) = address.children() else {
        let message = &log[id as usize];
        return Ok(format!("{}: {}", message.kind.name(), message.text));
    };
    let mut lines = String::new();
    for child in [first, second] {
        let text = tree
            .get(child)
            .ok_or_else(|| format!("node {child} is not built"))?;
        lines.push_str(&render_line(child, text));
    }
    Ok(lines)
}

/// Returns the timestamp of the session entry that message `id` came from.
pub fn date(log: &[LogMessage], id: u64) -> Result<String, String> {
    log.get(id as usize)
        .map(|message| message.timestamp.clone())
        .ok_or_else(|| {
            format!(
                "message {id} is outside the log: the log has {} messages",
                log.len()
            )
        })
}

/// Tool definitions of `zoom` and `date`.
pub fn tool_definitions() -> Vec<ToolDefinition> {
    let schema =
        |json: &str| RawValue::from_string(json.to_owned()).expect("tool schema is valid JSON");
    vec![
        ToolDefinition {
            name: "zoom".into(),
            description: "Open view node id+n. n > 1 returns its two child lines `id+n|text`; \
                          n = 1 returns message id whole. n must be a power of two (1, 2, 4, ...) \
                          and id a multiple of n inside the log; otherwise the result is an error \
                          text that names the problem."
                .into(),
            parameters: Some(schema(concat!(
                r#"{"type":"object","properties":{"#,
                r#""id":{"type":"integer","minimum":0,"description":"First message id of the node"},"#,
                r#""n":{"type":"integer","minimum":1,"description":"Number of messages, a power of two"}},"#,
                r#""required":["id","n"],"additionalProperties":false}"#,
            ))),
        },
        ToolDefinition {
            name: "date".into(),
            description: "Return the RFC 3339 time of the session entry that holds message id. \
                          An id outside the log returns an error text."
                .into(),
            parameters: Some(schema(
                r#"{"type":"object","properties":{"id":{"type":"integer","minimum":0,"description":"Message id"}},"required":["id"],"additionalProperties":false}"#,
            )),
        },
    ]
}
