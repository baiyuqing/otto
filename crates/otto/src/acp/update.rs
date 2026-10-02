//! Maps otto events and stored history to ACP `session/update` payloads.
//!
//! Stateless and synchronous: every function takes its input by reference and
//! returns owned schema values. The live mapping (`event_update`) and the
//! replay mapping (`history_updates`) use the same tool kind, title and
//! content rules, so a tool call looks the same live and after `session/load`.

use agent_client_protocol_schema::v1::{
    ContentBlock, ContentChunk, SessionUpdate, TextContent, ToolCall, ToolCallStatus,
    ToolCallUpdate, ToolCallUpdateFields, ToolKind,
};
use otto_core::agent::Event;
use otto_core::model::{BlockType, Message, Role};
use serde_json::Value;

/// The ACP tool kind of an otto tool name.
pub fn tool_kind(tool_name: &str) -> ToolKind {
    match tool_name {
        "bash" => ToolKind::Execute,
        "read" => ToolKind::Read,
        "write" | "edit" => ToolKind::Edit,
        "ls" | "find" | "grep" | "memory_search" => ToolKind::Search,
        _ => ToolKind::Other,
    }
}

/// The title of a tool call: the argument a user recognizes the call by, else
/// the tool name.
pub fn tool_title(tool_name: &str, raw_input: &Value) -> String {
    let key = match tool_name {
        "bash" => "command",
        "read" | "write" | "edit" | "ls" => "path",
        "find" | "grep" => "pattern",
        _ => return tool_name.to_string(),
    };
    match raw_input.get(key).and_then(Value::as_str) {
        Some(value) if !value.is_empty() => value.to_string(),
        _ => tool_name.to_string(),
    }
}

/// The arguments as parsed JSON, or as a JSON string when they do not parse.
fn raw_input(arguments: &str) -> Value {
    serde_json::from_str(arguments).unwrap_or_else(|_| Value::String(arguments.to_string()))
}

fn text_block(text: &str) -> ContentBlock {
    ContentBlock::Text(TextContent::new(text))
}

fn status(is_error: bool) -> ToolCallStatus {
    if is_error {
        ToolCallStatus::Failed
    } else {
        ToolCallStatus::Completed
    }
}

fn tool_call(tool_name: &str, id: &str, input: Value, status: ToolCallStatus) -> SessionUpdate {
    SessionUpdate::ToolCall(
        ToolCall::new(id.to_string(), tool_title(tool_name, &input))
            .kind(tool_kind(tool_name))
            .status(status)
            .raw_input(input),
    )
}

fn tool_result(id: &str, text: &str, is_error: bool) -> SessionUpdate {
    SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
        id.to_string(),
        ToolCallUpdateFields::new()
            .status(status(is_error))
            .content(vec![text_block(text).into()]),
    ))
}

/// The update for one live event; `None` for events ACP has no counterpart for.
///
/// The finished result uses `persisted_text`, the text stored in the session,
/// so the live update equals what `session/load` replays later.
pub fn event_update(event: &Event) -> Option<SessionUpdate> {
    match event {
        Event::TextDelta { text } => Some(SessionUpdate::AgentMessageChunk(ContentChunk::new(
            text_block(text),
        ))),
        Event::ReasoningDelta { text } => Some(SessionUpdate::AgentThoughtChunk(
            ContentChunk::new(text_block(text)),
        )),
        Event::ToolCallStarted {
            tool_name,
            tool_call_id,
            arguments,
            ..
        } => Some(tool_call(
            tool_name,
            tool_call_id,
            raw_input(arguments),
            ToolCallStatus::InProgress,
        )),
        Event::ToolCallFinished {
            tool_call_id,
            result,
            ..
        } => Some(tool_result(
            tool_call_id,
            result.persisted_text(),
            result.is_error,
        )),
        _ => None,
    }
}

/// One update per stored block, in history order.
pub fn history_updates(history: &[Message]) -> Vec<SessionUpdate> {
    let mut updates = Vec::new();
    for message in history {
        for block in &message.blocks {
            let update = match (&message.role, &block.block_type) {
                (Role::User, BlockType::Text) => Some(SessionUpdate::UserMessageChunk(
                    ContentChunk::new(text_block(&block.text)),
                )),
                (Role::Assistant, BlockType::Text) => Some(SessionUpdate::AgentMessageChunk(
                    ContentChunk::new(text_block(&block.text)),
                )),
                (Role::Assistant, BlockType::Reasoning) => Some(SessionUpdate::AgentThoughtChunk(
                    ContentChunk::new(text_block(&block.text)),
                )),
                (Role::Assistant, BlockType::ToolCall) => {
                    let input = block
                        .arguments
                        .as_ref()
                        .map_or(Value::Null, |raw| raw_input(raw.get()));
                    Some(tool_call(
                        &block.tool_name,
                        &block.tool_call_id,
                        input,
                        ToolCallStatus::Pending,
                    ))
                }
                (Role::Tool, BlockType::ToolResult) => Some(tool_result(
                    &block.tool_call_id,
                    &block.text,
                    block.is_error,
                )),
                _ => None,
            };
            updates.extend(update);
        }
    }
    updates
}

#[cfg(test)]
mod tests {
    use super::*;
    use otto_core::model::{
        Block, EffectCertainty, OperationDisposition, OperationId, OperationOutcome,
    };
    use otto_core::tool::ToolResult;
    use serde_json::json;

    fn to_json(update: &SessionUpdate) -> Value {
        serde_json::to_value(update).expect("update serializes")
    }

    fn started(name: &str, arguments: &str) -> Event {
        Event::ToolCallStarted {
            operation_id: OperationId::new("op-1").unwrap(),
            attempt: 1,
            tool_name: name.into(),
            tool_call_id: "call-1".into(),
            arguments: arguments.into(),
        }
    }

    #[test]
    fn text_and_reasoning_deltas_map_to_message_and_thought_chunks() {
        let text = event_update(&Event::TextDelta { text: "hi".into() }).unwrap();
        assert_eq!(
            to_json(&text),
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "hi"}})
        );
        let thought = event_update(&Event::ReasoningDelta { text: "hm".into() }).unwrap();
        assert_eq!(
            to_json(&thought)["sessionUpdate"],
            json!("agent_thought_chunk")
        );
        assert!(event_update(&Event::AgentStarted).is_none());
    }

    #[test]
    fn tool_kind_and_title_follow_the_mapping_table() {
        let cases = [
            ("bash", r#"{"command":"ls -l"}"#, "execute", "ls -l"),
            ("read", r#"{"path":"a.txt"}"#, "read", "a.txt"),
            ("write", r#"{"path":"b.txt"}"#, "edit", "b.txt"),
            ("edit", r#"{"path":"c.txt"}"#, "edit", "c.txt"),
            ("ls", r#"{"path":"d"}"#, "search", "d"),
            ("find", r#"{"pattern":"*.rs"}"#, "search", "*.rs"),
            ("grep", r#"{"pattern":"todo"}"#, "search", "todo"),
            (
                "memory_search",
                r#"{"query":"x"}"#,
                "search",
                "memory_search",
            ),
            ("web_fetch", r#"{"url":"u"}"#, "other", "web_fetch"),
        ];
        for (name, arguments, kind, title) in cases {
            let update = to_json(&event_update(&started(name, arguments)).unwrap());
            assert_eq!(update["sessionUpdate"], "tool_call", "{name}");
            assert_eq!(update["toolCallId"], "call-1", "{name}");
            // The schema omits a default `kind` ("other") from the wire.
            assert_eq!(update["kind"].as_str().unwrap_or("other"), kind, "{name}");
            assert_eq!(update["title"], title, "{name}");
            assert_eq!(update["status"], "in_progress", "{name}");
            assert_eq!(
                update["rawInput"],
                serde_json::from_str::<Value>(arguments).unwrap(),
                "{name}"
            );
        }
    }

    #[test]
    fn unparsable_arguments_are_sent_as_a_json_string() {
        let update = to_json(&event_update(&started("bash", "{not json")).unwrap());
        assert_eq!(update["rawInput"], json!("{not json"));
        assert_eq!(update["title"], "bash");
    }

    #[test]
    fn finished_result_carries_status_and_persisted_text() {
        let finished = |result: ToolResult| Event::ToolCallFinished {
            operation_id: OperationId::new("op-1").unwrap(),
            attempt: 1,
            tool_name: "read".into(),
            tool_call_id: "call-1".into(),
            result,
            outcome: OperationOutcome {
                disposition: OperationDisposition::Succeeded,
                effect_certainty: EffectCertainty::Completed,
                stop_reason: None,
            },
        };
        let ok = ToolResult {
            content: "live".into(),
            persisted_content: Some("stored".into()),
            is_error: false,
            outcome_override: None,
        };
        let update = to_json(&event_update(&finished(ok)).unwrap());
        assert_eq!(update["sessionUpdate"], "tool_call_update");
        assert_eq!(update["status"], "completed");
        assert_eq!(update["content"][0]["content"]["text"], "stored");
        let failed = ToolResult {
            content: "boom".into(),
            persisted_content: None,
            is_error: true,
            outcome_override: None,
        };
        let update = to_json(&event_update(&finished(failed)).unwrap());
        assert_eq!(update["status"], "failed");
        assert_eq!(update["content"][0]["content"]["text"], "boom");
    }

    fn message(role: Role, blocks: Vec<Block>) -> Message {
        Message {
            role,
            blocks,
            ..Message::default()
        }
    }

    #[test]
    fn history_replays_one_update_per_block_and_skips_other_roles() {
        let mut call = Block::text("");
        call.block_type = BlockType::ToolCall;
        call.tool_call_id = "call-1".into();
        call.tool_name = "read".into();
        call.arguments =
            Some(serde_json::value::RawValue::from_string(r#"{"path":"a"}"#.into()).unwrap());
        let mut result = Block::text("contents");
        result.block_type = BlockType::ToolResult;
        result.tool_call_id = "call-1".into();
        result.is_error = true;
        let history = vec![
            message(Role::Context, vec![Block::text("summary")]),
            message(Role::User, vec![Block::text("question")]),
            message(
                Role::Assistant,
                vec![Block::reasoning("thinking"), Block::text("answer"), call],
            ),
            message(Role::Tool, vec![result]),
            message(Role::User, vec![Block::image("AA", "image/png")]),
        ];
        let kinds: Vec<Value> = history_updates(&history)
            .iter()
            .map(|update| to_json(update)["sessionUpdate"].clone())
            .collect();
        assert_eq!(
            kinds,
            [
                "user_message_chunk",
                "agent_thought_chunk",
                "agent_message_chunk",
                "tool_call",
                "tool_call_update"
            ]
        );
        let updates = history_updates(&history);
        let call = to_json(&updates[3]);
        // The schema omits the default status ("pending") from the wire.
        assert!(call["status"].is_null() || call["status"] == "pending");
        assert_eq!(call["rawInput"], json!({"path": "a"}));
        let result = to_json(&updates[4]);
        assert_eq!(result["status"], "failed");
        assert_eq!(result["content"][0]["content"]["text"], "contents");
    }
}
