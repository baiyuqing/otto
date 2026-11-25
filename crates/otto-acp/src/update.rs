//! Maps otto wire events and stored history to ACP `session/update` payloads.
//!
//! Stateless and synchronous: every function takes its input by reference and
//! returns owned schema values. The live mapping (`event_update`) and the
//! replay mapping (`history_updates`) use the same tool kind, title and
//! content rules, so a tool call looks the same live and after `session/load`.

use agent_client_protocol_schema::v1::{
    ContentBlock, ContentChunk, Plan, PlanEntry, PlanEntryPriority, PlanEntryStatus, SessionUpdate,
    TextContent, ToolCall, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind,
};
use otto_core::agent::Event;
use otto_core::model::{BlockType, Message, OperationDisposition, Role};
use otto_core::session::{PlanStatus, PlanStep};
use otto_core::wire::events::{WireEvent, to_wire};
use serde_json::Value;

/// The ACP tool kind of an otto tool name.
pub fn tool_kind(tool_name: &str) -> ToolKind {
    match tool_name {
        "bash" => ToolKind::Execute,
        "read" => ToolKind::Read,
        "write" | "edit" => ToolKind::Edit,
        "ls" | "find" | "grep" | "memory_search" | "web_search" => ToolKind::Search,
        "web_fetch" => ToolKind::Fetch,
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
        "web_fetch" => "url",
        "web_search" => "query",
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

fn failed(is_error: bool, disposition: Option<OperationDisposition>) -> bool {
    is_error || disposition.is_some_and(|value| value != OperationDisposition::Succeeded)
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

/// The wire form of a local agent event, with the finished tool result's
/// content replaced by `persisted_text`, the text stored in the session, so
/// the live update equals what `session/load` replays later. Frames from
/// `otto serve` carry the live content and are mapped unchanged.
pub fn local_wire(event: &Event) -> WireEvent {
    let mut wire = to_wire(event);
    if let (Event::ToolCallFinished { result, .. }, Some(wire_result)) = (event, &mut wire.result) {
        wire_result.content = result.persisted_text().to_string();
    }
    wire
}

/// The update for one live event; `None` for events ACP has no counterpart for.
pub fn event_update(event: &WireEvent) -> Option<SessionUpdate> {
    match event.event_type.as_str() {
        "provider_retry"
            if event
                .retry
                .as_ref()
                .is_some_and(|retry| retry.interrupted_output) =>
        {
            Some(SessionUpdate::AgentMessageChunk(ContentChunk::new(
                text_block(&format!(
                    "\n\n[{}]\n\n",
                    otto_core::wire::transcript::INTERRUPTED_OUTPUT_NOTICE
                )),
            )))
        }
        "text_delta" => Some(SessionUpdate::AgentMessageChunk(ContentChunk::new(
            text_block(&event.text),
        ))),
        "reasoning_delta" => Some(SessionUpdate::AgentThoughtChunk(ContentChunk::new(
            text_block(&event.text),
        ))),
        "tool_call_started" => {
            let input = event
                .tool_args
                .as_ref()
                .map_or_else(|| Value::String(String::new()), |raw| raw_input(raw.get()));
            Some(tool_call(
                &event.tool_name,
                &event.tool_call_id,
                input,
                ToolCallStatus::InProgress,
            ))
        }
        "tool_call_finished" => event.result.as_ref().map(|result| {
            tool_result(
                &event.tool_call_id,
                &result.content,
                failed(result.is_error, result.disposition),
            )
        }),
        "plan_updated" => event.steps.as_deref().map(plan_update),
        _ => None,
    }
}

/// The ACP `plan` update for the whole session plan. Otto plans have no
/// priority, so every entry is `medium`; an empty plan clears the client's.
pub fn plan_update(steps: &[PlanStep]) -> SessionUpdate {
    SessionUpdate::Plan(Plan::new(
        steps
            .iter()
            .map(|step| {
                PlanEntry::new(
                    step.text.clone(),
                    PlanEntryPriority::Medium,
                    match step.status {
                        PlanStatus::Pending => PlanEntryStatus::Pending,
                        PlanStatus::InProgress => PlanEntryStatus::InProgress,
                        PlanStatus::Completed => PlanEntryStatus::Completed,
                    },
                )
            })
            .collect(),
    ))
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
                    failed(
                        block.is_error,
                        block
                            .operation_metadata
                            .as_ref()
                            .map(|meta| meta.disposition),
                    ),
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

    #[test]
    fn a_plan_update_becomes_an_acp_plan_with_every_step() {
        let steps = vec![
            PlanStep {
                text: "read".into(),
                status: PlanStatus::Completed,
            },
            PlanStep {
                text: "write".into(),
                status: PlanStatus::InProgress,
            },
            PlanStep {
                text: "ship".into(),
                status: PlanStatus::Pending,
            },
        ];
        let update = event_update(&local_wire(&Event::PlanUpdated {
            steps: steps.clone(),
        }))
        .expect("plan update");
        assert_eq!(
            to_json(&update),
            json!({
                "sessionUpdate": "plan",
                "entries": [
                    {"content": "read", "priority": "medium", "status": "completed"},
                    {"content": "write", "priority": "medium", "status": "in_progress"},
                    {"content": "ship", "priority": "medium", "status": "pending"}
                ]
            })
        );
        // A cleared plan clears the client's too.
        let cleared = event_update(&local_wire(&Event::PlanUpdated { steps: Vec::new() }))
            .expect("plan update");
        assert_eq!(
            to_json(&cleared),
            json!({"sessionUpdate": "plan", "entries": []})
        );
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
    fn interrupted_output_marks_a_boundary_in_acp_message_chunks() {
        let event = Event::ProviderRetry {
            operation_id: OperationId::new("recovery-op").unwrap(),
            attempt: 2,
            max_attempts: 7,
            delay: std::time::Duration::from_secs(1),
            reason: "connection interrupted".into(),
            interrupted_output: true,
        };
        let wire = local_wire(&event);
        assert!(wire.retry.as_ref().unwrap().interrupted_output);
        let update = to_json(&event_update(&wire).unwrap());
        assert_eq!(update["sessionUpdate"], "agent_message_chunk");
        assert!(
            update["content"]["text"]
                .as_str()
                .unwrap()
                .contains("Previous response interrupted")
        );
        let mut legacy = wire;
        legacy.retry.as_mut().unwrap().interrupted_output = false;
        assert!(event_update(&legacy).is_none());
    }

    #[test]
    fn text_and_reasoning_deltas_map_to_message_and_thought_chunks() {
        let text = event_update(&local_wire(&Event::TextDelta { text: "hi".into() })).unwrap();
        assert_eq!(
            to_json(&text),
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "hi"}})
        );
        let thought =
            event_update(&local_wire(&Event::ReasoningDelta { text: "hm".into() })).unwrap();
        assert_eq!(
            to_json(&thought)["sessionUpdate"],
            json!("agent_thought_chunk")
        );
        assert!(event_update(&local_wire(&Event::AgentStarted)).is_none());
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
            (
                "web_fetch",
                r#"{"url":"https://example.com/a"}"#,
                "fetch",
                "https://example.com/a",
            ),
            ("web_search", r#"{"query":"rust"}"#, "search", "rust"),
            ("mystery_tool", r#"{"url":"u"}"#, "other", "mystery_tool"),
        ];
        for (name, arguments, kind, title) in cases {
            let update = to_json(&event_update(&local_wire(&started(name, arguments))).unwrap());
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
        let update = to_json(&event_update(&local_wire(&started("bash", "{not json"))).unwrap());
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
            content_scope: Default::default(),
            is_error: false,
            outcome_override: None,
        };
        let update = to_json(&event_update(&local_wire(&finished(ok))).unwrap());
        assert_eq!(update["sessionUpdate"], "tool_call_update");
        assert_eq!(update["status"], "completed");
        assert_eq!(update["content"][0]["content"]["text"], "stored");
        let failed = ToolResult {
            content: "boom".into(),
            persisted_content: None,
            content_scope: Default::default(),
            is_error: true,
            outcome_override: None,
        };
        let update = to_json(&event_update(&local_wire(&finished(failed))).unwrap());
        assert_eq!(update["status"], "failed");
        assert_eq!(update["content"][0]["content"]["text"], "boom");
    }

    #[test]
    fn stopped_operations_are_failed_live_and_after_history_replay() {
        use otto_core::model::ToolResultMetadata;
        use otto_core::wire::events::WireToolResult;

        for disposition in [
            OperationDisposition::Cancelled,
            OperationDisposition::DeadlineExceeded,
            OperationDisposition::Interrupted,
            OperationDisposition::Error,
        ] {
            let event = WireEvent {
                event_type: "tool_call_finished".into(),
                tool_call_id: "call-1".into(),
                result: Some(WireToolResult {
                    content: "execution stopped".into(),
                    is_error: false,
                    disposition: Some(disposition),
                    ..WireToolResult::default()
                }),
                ..WireEvent::default()
            };
            let live = to_json(&event_update(&event).unwrap());
            assert_eq!(live["status"], "failed", "{disposition:?}");
            let mut block = Block::text("execution stopped");
            block.block_type = BlockType::ToolResult;
            block.tool_call_id = "call-1".into();
            block.operation_metadata = Some(ToolResultMetadata {
                operation_id: None,
                disposition,
                effect_certainty: EffectCertainty::Unknown,
                stop_reason: None,
            });
            let replay = history_updates(&[message(Role::Tool, vec![block])]);
            assert_eq!(to_json(&replay[0]), live, "{disposition:?}");
        }
        assert!(
            !failed(false, None),
            "legacy successful results remain completed"
        );
        assert!(!failed(false, Some(OperationDisposition::Succeeded)));
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
