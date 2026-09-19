//! The browser transcript reducer.
//!
//! Port of `ui/src/transcript.ts`. `from_history` renders stored session
//! history and `reduce` folds one live turn event into the rendered list.
//! Both take JSON text rather than decoded values so provider argument JSON
//! keeps its original key order.

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

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
    },
    Assistant {
        text: String,
    },
    Tool {
        id: String,
        name: String,
        args: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<String>,
        #[serde(rename = "isError", default, skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
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
}

/// One block of a stored message.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct HistoryBlock {
    #[serde(rename = "type", default)]
    pub block_type: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub tool_call_id: String,
    #[serde(default)]
    pub tool_name: String,
    #[serde(default)]
    pub arguments: Option<Box<RawValue>>,
    #[serde(default)]
    pub is_error: bool,
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
    pretty_json(text)
}

/// Reformats valid, compact JSON text with two-space indentation while
/// keeping member order. Scalars pass through verbatim, so number literals
/// and string escapes are not renormalized.
fn pretty_json(src: &str) -> String {
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
                out.push_str(&src[start..index.min(bytes.len())]);
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
                newline(&mut out, depth);
                index += 1;
            }
            close @ (b'}' | b']') => {
                depth = depth.saturating_sub(1);
                newline(&mut out, depth);
                out.push(close as char);
                index += 1;
            }
            b',' => {
                out.push(',');
                newline(&mut out, depth);
                index += 1;
            }
            b':' => {
                out.push_str(": ");
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
                            result, is_error, ..
                        } = &mut items[index]
                    {
                        *result = Some(block.text.clone());
                        *is_error = Some(block.is_error);
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
                    });
                }
                "image" if message.role == "user" => {
                    items.push(Item::User {
                        text: "[image]".into(),
                    });
                }
                _ => {
                    if block.text.is_empty() {
                        continue;
                    }
                    match message.role.as_str() {
                        "user" => items.push(Item::User {
                            text: block.text.clone(),
                        }),
                        "assistant" => items.push(Item::Assistant {
                            text: block.text.clone(),
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

/// Applies one turn event, given as the SSE frame's `data` field, and returns
/// the next transcript. `items` is never modified.
pub fn reduce_json(items: &[Item], event_json: &str) -> Result<Vec<Item>, serde_json::Error> {
    let event: WireEvent = serde_json::from_str(event_json)?;
    Ok(reduce(items, &event))
}

/// The decoded form of [`reduce_json`].
pub fn reduce(items: &[Item], event: &WireEvent) -> Vec<Item> {
    match event.event_type.as_str() {
        "text_delta" => {
            if event.text.is_empty() {
                return items.to_vec();
            }
            let mut next = items.to_vec();
            if let Some(Item::Assistant { text }) = next.last_mut() {
                text.push_str(&event.text);
            } else {
                next.push(Item::Assistant {
                    text: event.text.clone(),
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
            let mut next = items.to_vec();
            let found = next.iter().rposition(
                |item| matches!(item, Item::Tool { id, .. } if *id == event.tool_call_id),
            );
            match found {
                Some(index) => {
                    if let Item::Tool {
                        result, is_error, ..
                    } = &mut next[index]
                    {
                        *result = Some(content);
                        *is_error = Some(errored);
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
                text: format!(
                    "Context compacted: {} → ~{} tokens ({})",
                    compaction.tokens_before, compaction.estimated_tokens_after, compaction.reason
                ),
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
mod tests {
    use super::*;

    fn apply(events: &[&str]) -> Vec<Item> {
        let mut items = Vec::new();
        for event in events {
            items = reduce_json(&items, event).expect("event decodes");
        }
        items
    }

    fn tool(
        id: &str,
        name: &str,
        args: &str,
        result: Option<&str>,
        is_error: Option<bool>,
    ) -> Item {
        Item::Tool {
            id: id.into(),
            name: name.into(),
            args: args.into(),
            result: result.map(str::to_string),
            is_error,
        }
    }

    #[test]
    fn merges_text_deltas_into_one_assistant_item() {
        let items = apply(&[
            r#"{"type":"agent_started"}"#,
            r#"{"type":"text_delta","text":"hel"}"#,
            r#"{"type":"text_delta","text":"lo"}"#,
            r#"{"type":"agent_finished"}"#,
        ]);
        assert_eq!(
            items,
            vec![Item::Assistant {
                text: "hello".into()
            }]
        );
    }

    #[test]
    fn starts_a_new_assistant_item_after_a_tool_call() {
        let items = apply(&[
            r#"{"type":"text_delta","text":"first"}"#,
            r#"{"type":"tool_call_started","tool_call_id":"c1","tool_name":"bash","tool_args":{"command":"ls"}}"#,
            r#"{"type":"tool_call_finished","tool_call_id":"c1","tool_name":"bash","result":{"content":"a\nb","is_error":false}}"#,
            r#"{"type":"text_delta","text":"second"}"#,
        ]);
        assert_eq!(
            items,
            vec![
                Item::Assistant {
                    text: "first".into()
                },
                tool(
                    "c1",
                    "bash",
                    "{\n  \"command\": \"ls\"\n}",
                    Some("a\nb"),
                    Some(false)
                ),
                Item::Assistant {
                    text: "second".into()
                },
            ]
        );
    }

    #[test]
    fn records_an_unmatched_tool_call_finished_as_its_own_item() {
        let items = apply(&[
            r#"{"type":"tool_call_finished","tool_call_id":"c9","tool_name":"read","result":{"content":"x","is_error":true}}"#,
        ]);
        assert_eq!(items, vec![tool("c9", "read", "", Some("x"), Some(true))]);
    }

    #[test]
    fn does_not_mutate_the_previous_transcript() {
        let before = vec![Item::Assistant { text: "a".into() }];
        let after = reduce_json(&before, r#"{"type":"text_delta","text":"b"}"#).expect("decodes");
        assert_eq!(before, vec![Item::Assistant { text: "a".into() }]);
        assert_eq!(after, vec![Item::Assistant { text: "ab".into() }]);
    }

    #[test]
    fn adds_a_notice_for_a_real_compaction_and_nothing_for_a_noop() {
        let done = r#"{"type":"compaction_completed","compaction":{"reason":"threshold","tokens_before":900,"estimated_tokens_after":300,"automatic":true,"noop":false}}"#;
        assert_eq!(
            apply(&[done]),
            vec![Item::Notice {
                text: "Context compacted: 900 → ~300 tokens (threshold)".into(),
            }]
        );
        let noop = r#"{"type":"compaction_completed","compaction":{"reason":"threshold","tokens_before":900,"estimated_tokens_after":300,"automatic":true,"noop":true}}"#;
        assert_eq!(apply(&[noop]), vec![]);
    }

    #[test]
    fn turns_agent_error_into_an_error_item() {
        assert_eq!(
            apply(&[r#"{"type":"agent_error","error":"provider: 500"}"#]),
            vec![Item::Error {
                text: "provider: 500".into(),
            }]
        );
    }

    #[test]
    fn notification_without_text_names_the_task() {
        assert_eq!(
            apply(&[r#"{"type":"notification","task_id":"t1"}"#]),
            vec![Item::Notice {
                text: "Task t1 finished".into(),
            }]
        );
    }

    #[test]
    fn warnings_fall_back_to_the_error_field_then_the_type() {
        assert_eq!(
            apply(&[r#"{"type":"memory_warning","error":"recall failed"}"#]),
            vec![Item::Notice {
                text: "recall failed".into(),
            }]
        );
        assert_eq!(
            apply(&[r#"{"type":"compaction_warning"}"#]),
            vec![Item::Notice {
                text: "compaction_warning".into(),
            }]
        );
    }

    #[test]
    fn pairs_tool_results_and_shows_only_display_context() {
        let history = r#"[
          {"id":"1","role":"user","created_at":"","blocks":[{"type":"text","text":"list files"}]},
          {"id":"2","role":"assistant","created_at":"","blocks":[
            {"type":"text","text":"Running ls."},
            {"type":"tool_call","tool_call_id":"c1","tool_name":"bash","arguments":{"command":"ls"}}]},
          {"id":"3","role":"tool","created_at":"","blocks":[
            {"type":"tool_result","tool_call_id":"c1","tool_name":"bash","text":"a.go","is_error":false}]},
          {"id":"4","role":"context","created_at":"","context_type":"memory","blocks":[{"type":"text","text":"hidden recall"}]},
          {"id":"5","role":"context","created_at":"","display":true,"blocks":[{"type":"text","text":"Task t1 finished"}]},
          {"id":"6","role":"assistant","created_at":"","blocks":[{"type":"text","text":"One file."}]}
        ]"#;
        assert_eq!(
            from_history(history).expect("history decodes"),
            vec![
                Item::User {
                    text: "list files".into()
                },
                Item::Assistant {
                    text: "Running ls.".into()
                },
                tool(
                    "c1",
                    "bash",
                    "{\n  \"command\": \"ls\"\n}",
                    Some("a.go"),
                    Some(false)
                ),
                Item::Notice {
                    text: "Task t1 finished".into()
                },
                Item::Assistant {
                    text: "One file.".into()
                },
            ]
        );
    }

    #[test]
    fn format_args_matches_json_stringify_with_two_space_indent() {
        let cases = [
            ("null", ""),
            (r#""plain text""#, "plain text"),
            ("{}", "{}"),
            ("[]", "[]"),
            (r#"{"b":1,"a":2}"#, "{\n  \"b\": 1,\n  \"a\": 2\n}"),
            (r#"[1,2]"#, "[\n  1,\n  2\n]"),
            (
                r#"{"a":{"b":[1,{"c":null}]}}"#,
                "{\n  \"a\": {\n    \"b\": [\n      1,\n      {\n        \"c\": null\n      }\n    ]\n  }\n}",
            ),
            (r#"{"s":"a\"b,c:d"}"#, "{\n  \"s\": \"a\\\"b,c:d\"\n}"),
        ];
        for (input, want) in cases {
            let raw = RawValue::from_string(input.to_string()).expect("valid json");
            assert_eq!(format_args(Some(&raw)), want, "input {input}");
        }
        assert_eq!(format_args(None), "");
    }

    #[test]
    fn items_serialize_with_the_typescript_field_names() {
        let json = serde_json::to_string(&tool("c1", "bash", "a", Some("out"), Some(true)))
            .expect("item serializes");
        assert_eq!(
            json,
            r#"{"kind":"tool","id":"c1","name":"bash","args":"a","result":"out","isError":true}"#
        );
        assert_eq!(
            serde_json::to_string(&tool("c1", "bash", "a", None, None)).expect("item serializes"),
            r#"{"kind":"tool","id":"c1","name":"bash","args":"a"}"#
        );
    }
}
