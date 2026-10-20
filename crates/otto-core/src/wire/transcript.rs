//! The browser transcript reducer.
//!
//! `from_history` renders stored session history and `reduce` folds one live
//! turn event into the rendered list. Both take JSON text rather than decoded
//! values so provider argument JSON keeps its original key order.

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::model::{EffectCertainty, OperationDisposition, OperationStopReason};

use super::events::WireEvent;

/// One rendered transcript entry.
///
/// `isError` is camelCase because the TypeScript view component reads it
/// under that name; the rest of the wire is snake_case.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Item {
    User {
        text: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        created_at: String,
    },
    Image {
        data: String,
        mime_type: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        created_at: String,
    },
    Assistant {
        text: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        created_at: String,
    },
    /// Model reasoning that precedes an assistant reply.
    Reasoning {
        text: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        created_at: String,
    },
    Tool {
        id: String,
        name: String,
        args: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<String>,
        #[serde(rename = "isError", default, skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        operation_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        disposition: Option<OperationDisposition>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effect_certainty: Option<EffectCertainty>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stop_reason: Option<OperationStopReason>,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        created_at: String,
    },
    Notice {
        text: String,
    },
    Error {
        text: String,
    },
}

/// One stored message, decoded only as far as the transcript needs.
///
/// Every field defaults, so a message the server grows a field on still
/// renders.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct HistoryMessage {
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub blocks: Vec<HistoryBlock>,
    #[serde(default)]
    pub display: bool,
    #[serde(default)]
    pub created_at: String,
}

/// One block of a stored message.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct HistoryBlock {
    #[serde(rename = "type", default)]
    pub block_type: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub data: String,
    #[serde(default)]
    pub mime_type: String,
    #[serde(default)]
    pub tool_call_id: String,
    #[serde(default)]
    pub tool_name: String,
    #[serde(default)]
    pub arguments: Option<Box<RawValue>>,
    #[serde(default)]
    pub is_error: bool,
    #[serde(default)]
    pub operation_id: Option<String>,
    #[serde(default)]
    pub disposition: Option<OperationDisposition>,
    #[serde(default)]
    pub effect_certainty: Option<EffectCertainty>,
    #[serde(default)]
    pub stop_reason: Option<OperationStopReason>,
}

fn message_created_at(message: &HistoryMessage) -> String {
    let created_at = message.created_at.trim();
    if created_at.is_empty() || created_at == crate::model::zero_time().to_rfc3339() {
        String::new()
    } else {
        created_at.to_string()
    }
}

/// Renders tool arguments the way `JSON.stringify(args, null, 2)` does:
/// a JSON string becomes its own contents, `null` and absent become empty,
/// and anything else is pretty-printed with two-space indentation.
pub fn format_args(args: Option<&RawValue>) -> String {
    let Some(raw) = args else {
        return String::new();
    };
    let text = raw.get().trim();
    if text == "null" {
        return String::new();
    }
    if text.starts_with('"') {
        return serde_json::from_str::<String>(text).unwrap_or_else(|_| text.to_string());
    }
    format_json(text, true)
}

/// Formats arguments as compact JSON, preserving member order and number literals.
/// Strings stay quoted, with literal backslashes and controls still escaped.
pub fn format_args_compact(args: Option<&RawValue>) -> String {
    args.map_or_else(String::new, |raw| format_json(raw.get().trim(), false))
}

/// Reformats valid JSON while keeping member order and number literals.
/// String tokens are decoded and re-encoded for display only.
fn format_json(src: &str, pretty: bool) -> String {
    let bytes = src.as_bytes();
    let mut out = String::with_capacity(src.len() + src.len() / 4 + 8);
    let mut depth = 0usize;
    let mut index = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b'"' => {
                let start = index;
                index += 1;
                while index < bytes.len() {
                    match bytes[index] {
                        b'\\' => index += 2,
                        b'"' => {
                            index += 1;
                            break;
                        }
                        _ => index += 1,
                    }
                }
                let token = &src[start..index.min(bytes.len())];
                match serde_json::from_str::<String>(token) {
                    Ok(decoded) => out.push_str(
                        &serde_json::to_string(&decoded).expect("a string always encodes"),
                    ),
                    Err(_) => out.push_str(token),
                }
            }
            b' ' | b'\n' | b'\t' | b'\r' => index += 1,
            open @ (b'{' | b'[') => {
                let mut ahead = index + 1;
                while ahead < bytes.len() && bytes[ahead].is_ascii_whitespace() {
                    ahead += 1;
                }
                if ahead < bytes.len() && (bytes[ahead] == b'}' || bytes[ahead] == b']') {
                    out.push(open as char);
                    out.push(bytes[ahead] as char);
                    index = ahead + 1;
                    continue;
                }
                depth += 1;
                out.push(open as char);
                if pretty {
                    newline(&mut out, depth);
                }
                index += 1;
            }
            close @ (b'}' | b']') => {
                depth = depth.saturating_sub(1);
                if pretty {
                    newline(&mut out, depth);
                }
                out.push(close as char);
                index += 1;
            }
            b',' => {
                out.push(',');
                if pretty {
                    newline(&mut out, depth);
                }
                index += 1;
            }
            b':' => {
                out.push_str(if pretty { ": " } else { ":" });
                index += 1;
            }
            other => {
                out.push(other as char);
                index += 1;
            }
        }
    }
    out
}

fn newline(out: &mut String, depth: usize) {
    out.push('\n');
    for _ in 0..depth {
        out.push_str("  ");
    }
}

/// Renders stored session history from the JSON array the
/// `/v1/sessions/{id}/history` route returns.
///
/// Tool results are matched to their call by id. Context messages appear only
/// when the server marked them `display`.
pub fn from_history(messages_json: &str) -> Result<Vec<Item>, serde_json::Error> {
    let messages: Vec<HistoryMessage> = serde_json::from_str(messages_json)?;
    Ok(from_history_messages(&messages))
}

/// The decoded form of [`from_history`], for callers that already hold
/// messages.
pub fn from_history_messages(messages: &[HistoryMessage]) -> Vec<Item> {
    let mut items: Vec<Item> = Vec::new();
    // call id -> index into items, so a later tool_result updates in place.
    let mut tools: Vec<(String, usize)> = Vec::new();
    for message in messages {
        for block in &message.blocks {
            match block.block_type.as_str() {
                "tool_result" => {
                    let found = tools
                        .iter()
                        .find(|(id, _)| !block.tool_call_id.is_empty() && *id == block.tool_call_id)
                        .map(|(_, index)| *index);
                    if let Some(index) = found
                        && let Item::Tool {
                            result,
                            is_error,
                            operation_id,
                            disposition,
                            effect_certainty,
                            stop_reason,
                            ..
                        } = &mut items[index]
                    {
                        *result = Some(block.text.clone());
                        *is_error = Some(block.is_error);
                        *operation_id = block.operation_id.clone();
                        *disposition = block.disposition;
                        *effect_certainty = block.effect_certainty;
                        *stop_reason = block.stop_reason;
                    }
                }
                "tool_call" => {
                    tools.push((block.tool_call_id.clone(), items.len()));
                    items.push(Item::Tool {
                        id: block.tool_call_id.clone(),
                        name: block.tool_name.clone(),
                        args: format_args(block.arguments.as_deref()),
                        result: None,
                        is_error: None,
                        operation_id: None,
                        disposition: None,
                        effect_certainty: None,
                        stop_reason: None,
                        created_at: message_created_at(message),
                    });
                }
                "reasoning" => {
                    if message.role == "assistant" && !block.text.is_empty() {
                        items.push(Item::Reasoning {
                            text: block.text.clone(),
                            created_at: message_created_at(message),
                        });
                    }
                }
                "image" if message.role == "user" => {
                    items.push(Item::Image {
                        data: block.data.clone(),
                        mime_type: block.mime_type.clone(),
                        created_at: message_created_at(message),
                    });
                }
                _ => {
                    if block.text.is_empty() {
                        continue;
                    }
                    match message.role.as_str() {
                        "user" => items.push(Item::User {
                            text: block.text.clone(),
                            created_at: message_created_at(message),
                        }),
                        "assistant" => items.push(Item::Assistant {
                            text: block.text.clone(),
                            created_at: message_created_at(message),
                        }),
                        "context" if message.display => items.push(Item::Notice {
                            text: block.text.clone(),
                        }),
                        _ => {}
                    }
                }
            }
        }
    }
    items
}

/// How many characters of a tool call's raw arguments the phase shows.
const PHASE_ARGS_LIMIT: usize = 60;

/// The turn phase `event` starts, or `None` when the event leaves the phase
/// unchanged. Both frontends show the result through [`status_line`].
pub fn phase(event: &WireEvent) -> Option<String> {
    phase_at(event, 0)
}

/// Recomputes a retry countdown using time since its event arrived.
pub fn phase_at(event: &WireEvent, elapsed_ms: u64) -> Option<String> {
    let phase = match event.event_type.as_str() {
        "agent_started" | "tool_call_finished" | "compaction_completed" => {
            "waiting for model".to_string()
        }
        "reasoning_delta" => "reasoning".to_string(),
        "text_delta" => "responding".to_string(),
        "compaction_started" => "compacting".to_string(),
        "tool_call_started" => {
            let raw = format_args_compact(event.tool_args.as_deref());
            let flat: String = raw
                .chars()
                .map(|c| if c.is_whitespace() { ' ' } else { c })
                .collect();
            let preview = if flat.chars().count() > PHASE_ARGS_LIMIT {
                let cut: String = flat.chars().take(PHASE_ARGS_LIMIT - 1).collect();
                format!("{cut}…")
            } else {
                flat
            };
            format!("running {} {preview}", event.tool_name)
                .trim_end()
                .to_string()
        }
        "provider_retry" => {
            let retry = event.retry.clone().unwrap_or_default();
            let remaining = retry.delay_ms.saturating_sub(elapsed_ms);
            let progress = if remaining == 0 {
                "requesting".to_owned()
            } else {
                format!("waiting {}s", remaining.div_ceil(1000))
            };
            format!(
                "retry {}/{} after {}, {progress}",
                retry.attempt.saturating_sub(1),
                retry.max_attempts.saturating_sub(1),
                retry.reason
            )
        }
        _ => return None,
    };
    Some(phase)
}

/// The in-flight turn's status: `<phase> · <phase seconds>s · turn <turn seconds>s`.
pub fn status_line(phase: &str, phase_secs: u64, turn_secs: u64) -> String {
    format!("{phase} · {phase_secs}s · turn {turn_secs}s")
}

/// Applies one turn event, given as the SSE frame's `data` field, and returns
/// the next transcript. `items` is never modified.
pub fn reduce_json(items: &[Item], event_json: &str) -> Result<Vec<Item>, serde_json::Error> {
    let event: WireEvent = serde_json::from_str(event_json)?;
    Ok(reduce(items, &event))
}

/// Marks the boundary between an interrupted attempt and its replacement.
pub const INTERRUPTED_OUTPUT_NOTICE: &str =
    "Previous response interrupted; retrying this model step. Completed tools are preserved.";

/// The decoded form of [`reduce_json`].
pub fn reduce(items: &[Item], event: &WireEvent) -> Vec<Item> {
    match event.event_type.as_str() {
        "provider_retry"
            if event
                .retry
                .as_ref()
                .is_some_and(|retry| retry.interrupted_output) =>
        {
            let mut next = items.to_vec();
            next.push(Item::Notice {
                text: INTERRUPTED_OUTPUT_NOTICE.into(),
            });
            next
        }
        "reasoning_delta" => {
            if event.text.is_empty() {
                return items.to_vec();
            }
            let mut next = items.to_vec();
            if let Some(Item::Reasoning { text, .. }) = next.last_mut() {
                text.push_str(&event.text);
            } else {
                next.push(Item::Reasoning {
                    text: event.text.clone(),
                    created_at: String::new(),
                });
            }
            next
        }
        "text_delta" => {
            if event.text.is_empty() {
                return items.to_vec();
            }
            let mut next = items.to_vec();
            if let Some(Item::Assistant { text, .. }) = next.last_mut() {
                text.push_str(&event.text);
            } else {
                next.push(Item::Assistant {
                    text: event.text.clone(),
                    created_at: String::new(),
                });
            }
            next
        }
        "tool_call_started" => {
            let mut next = items.to_vec();
            next.push(Item::Tool {
                id: event.tool_call_id.clone(),
                name: event.tool_name.clone(),
                args: format_args(event.tool_args.as_deref()),
                result: None,
                is_error: None,
                operation_id: (!event.operation_id.is_empty()).then(|| event.operation_id.clone()),
                disposition: None,
                effect_certainty: None,
                stop_reason: None,
                created_at: String::new(),
            });
            next
        }
        "tool_call_finished" => {
            let content = event
                .result
                .as_ref()
                .map(|result| result.content.clone())
                .unwrap_or_default();
            let errored = event
                .result
                .as_ref()
                .map(|result| result.is_error)
                .unwrap_or(false);
            let operation_id = event
                .result
                .as_ref()
                .and_then(|result| {
                    (!result.operation_id.is_empty()).then(|| result.operation_id.clone())
                })
                .or_else(|| (!event.operation_id.is_empty()).then(|| event.operation_id.clone()));
            let disposition = event.result.as_ref().and_then(|result| result.disposition);
            let effect_certainty = event
                .result
                .as_ref()
                .and_then(|result| result.effect_certainty);
            let stop_reason = event.result.as_ref().and_then(|result| result.stop_reason);
            let mut next = items.to_vec();
            let found = next.iter().rposition(
                |item| matches!(item, Item::Tool { id, .. } if *id == event.tool_call_id),
            );
            match found {
                Some(index) => {
                    if let Item::Tool {
                        result,
                        is_error,
                        operation_id: item_operation_id,
                        disposition: item_disposition,
                        effect_certainty: item_effect_certainty,
                        stop_reason: item_stop_reason,
                        ..
                    } = &mut next[index]
                    {
                        *result = Some(content);
                        *is_error = Some(errored);
                        if operation_id.is_some() {
                            *item_operation_id = operation_id;
                        }
                        *item_disposition = disposition;
                        *item_effect_certainty = effect_certainty;
                        *item_stop_reason = stop_reason;
                    }
                }
                // Seen when attaching mid-turn with ?after=N past the start
                // event.
                None => next.push(Item::Tool {
                    id: event.tool_call_id.clone(),
                    name: event.tool_name.clone(),
                    args: String::new(),
                    result: Some(content),
                    is_error: Some(errored),
                    operation_id,
                    disposition,
                    effect_certainty,
                    stop_reason,
                    created_at: String::new(),
                }),
            }
            next
        }
        "compaction_completed" => {
            let Some(compaction) = event.compaction.as_ref() else {
                return items.to_vec();
            };
            if compaction.noop {
                return items.to_vec();
            }
            let mut next = items.to_vec();
            next.push(Item::Notice {
                text: match (compaction.tokens_before, compaction.estimated_tokens_after) {
                    (Some(before), Some(after)) => format!(
                        "Context compacted: {before} → ~{after} tokens ({})",
                        compaction.reason
                    ),
                    (Some(before), None) => format!(
                        "Context compacted: {before} tokens before ({})",
                        compaction.reason
                    ),
                    (None, _) => format!("Context compacted ({})", compaction.reason),
                },
            });
            next
        }
        "notification" => {
            let mut next = items.to_vec();
            let text = if event.text.is_empty() {
                format!("Task {} finished", event.task_id)
            } else {
                event.text.clone()
            };
            next.push(Item::Notice { text });
            next
        }
        kind @ ("compaction_warning" | "memory_warning") => {
            let mut next = items.to_vec();
            let text = first_non_empty(&[&event.text, &event.error, kind]);
            next.push(Item::Notice { text });
            next
        }
        "agent_error" => {
            let mut next = items.to_vec();
            let text = first_non_empty(&[&event.error, &event.text, "agent error"]);
            next.push(Item::Error { text });
            next
        }
        // The image of a user_message is added by the client that holds it.
        "user_message" => {
            if event.text.is_empty() {
                return items.to_vec();
            }
            let mut next = items.to_vec();
            next.push(Item::User {
                text: event.text.clone(),
                created_at: String::new(),
            });
            next
        }
        // An agent failure already added its agent_error item just before;
        // turn_end adds one only for failures outside the agent loop.
        "turn_end" => {
            if event.status != "error" || matches!(items.last(), Some(Item::Error { .. })) {
                return items.to_vec();
            }
            let mut next = items.to_vec();
            let text = first_non_empty(&[&event.error, "turn failed"]);
            next.push(Item::Error { text });
            next
        }
        "approval_changed" => {
            let Some(request) = &event.approval else {
                return items.to_vec();
            };
            let automatic = request
                .decision
                .as_ref()
                .is_some_and(|d| d.transport == "auto-mode");
            let text = if request.state == crate::approval::State::Pending {
                format!(
                    "Approval requested: {} ({})\n{}\nReason: {}",
                    request.title, request.id, request.subject.details, request.reason
                )
            } else if automatic {
                format!(
                    "Approval {}: automatically approved · application {:?}{}",
                    request.id,
                    request.application,
                    if request.application_error.is_empty() {
                        String::new()
                    } else {
                        format!("\n{}", request.application_error)
                    }
                )
            } else {
                format!(
                    "Approval {}: {:?} · application {:?}",
                    request.id, request.state, request.application
                )
            };
            let mut next = items.to_vec();
            next.push(Item::Notice { text });
            next
        }
        "approval_requested" => {
            let mut text = format!(
                "Approval {} requested for: {}",
                event.approval_id, event.command
            );
            if !event.read_path.is_empty() {
                text.push_str(&format!("\nPermanently allow reading: {}\nSaved to read_paths; commands stay sandboxed.", event.read_path));
            }
            if !event.justification.is_empty() {
                text.push_str(&format!("\nReason: {}", event.justification));
            }
            text.push_str(&format!(
                "\n/approve {id} or /deny {id}",
                id = event.approval_id
            ));
            let mut next = items.to_vec();
            next.push(Item::Notice { text });
            next
        }
        "approval_decided" => {
            let mut next = items.to_vec();
            next.push(Item::Notice {
                text: format!("Approval {}: {}", event.approval_id, event.decision),
            });
            next
        }
        // agent_started, agent_finished, provider_usage, compaction_planned
        // and compaction_started carry nothing the transcript shows.
        _ => items.to_vec(),
    }
}

fn first_non_empty(candidates: &[&str]) -> String {
    candidates
        .iter()
        .find(|value| !value.is_empty())
        .unwrap_or(&"")
        .to_string()
}

#[cfg(test)]
mod tests;
