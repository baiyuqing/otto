//! Token estimates for a provider request.
//!
//! Port of `internal/agent/context_estimate.go`. The numbers are a cheap
//! byte-count heuristic, not a tokenizer: three bytes per token plus fixed
//! framing costs. They only have to be consistent with the Go agent, because
//! the compaction triggers are calibrated against them.
//!
//! Every addition saturates at [`i64::MAX`] instead of wrapping, so a hostile
//! transcript cannot make a huge context look small.

use crate::model::{Block, BlockType, Message, ToolDefinition};
use crate::provider::Request;
use crate::session::CompactionMetadata;

const REQUEST_FRAMING_TOKENS: i64 = 3;
const MESSAGE_FRAMING_TOKENS: i64 = 6;
const TEXT_BLOCK_FRAMING_TOKENS: i64 = 2;
const IMAGE_BLOCK_TOKENS: i64 = 2048;
const TOOL_CALL_FRAMING_TOKENS: i64 = 12;
const TOOL_RESULT_FRAMING_TOKENS: i64 = 8;
const TOOL_DEFINITION_FRAMING_TOKENS: i64 = 16;

/// Estimates the input tokens `request` will cost.
///
/// When the most recent assistant message inside the active checkpoint
/// reported its own input-token count, that count anchors the estimate and
/// only the messages after it are counted. Otherwise everything is counted
/// from the system prompt down.
pub fn estimate_request(request: &Request, latest: Option<&CompactionMetadata>) -> i64 {
    if let Some((anchor, prompt_tokens)) = request_usage_anchor(&request.messages, latest) {
        let mut total = prompt_tokens;
        for message in &request.messages[anchor..] {
            total = saturating_add(total, estimate_message(message));
        }
        return total;
    }

    let mut total = REQUEST_FRAMING_TOKENS;
    total = saturating_add(total, estimate_string(&request.system_prompt));
    for message in &request.messages {
        total = saturating_add(total, estimate_message(message));
    }
    for definition in &request.tools {
        total = saturating_add(total, estimate_tool_definition(definition));
    }
    total
}

/// Estimates one message, framing included.
pub fn estimate_message(message: &Message) -> i64 {
    let mut total = MESSAGE_FRAMING_TOKENS;
    for block in &message.blocks {
        total = saturating_add(total, estimate_block(block));
    }
    total
}

fn estimate_block(block: &Block) -> i64 {
    match block.block_type {
        BlockType::Text => saturating_add(TEXT_BLOCK_FRAMING_TOKENS, estimate_string(&block.text)),
        BlockType::Image => IMAGE_BLOCK_TOKENS,
        BlockType::ToolCall => {
            let mut total = TOOL_CALL_FRAMING_TOKENS;
            total = saturating_add(total, estimate_string(&block.tool_call_id));
            total = saturating_add(total, estimate_string(&block.tool_name));
            let arguments = block.arguments.as_ref().map_or("", |raw| raw.get());
            saturating_add(total, estimate_string(arguments))
        }
        BlockType::ToolResult => {
            let mut total = TOOL_RESULT_FRAMING_TOKENS;
            total = saturating_add(total, estimate_string(&block.text));
            total = saturating_add(total, estimate_string(&block.tool_call_id));
            saturating_add(total, estimate_string(&block.tool_name))
        }
        // Go's switch has no default arm, so an unknown block type costs
        // nothing.
        BlockType::Other(_) => 0,
    }
}

/// Three bytes per token, rounded up, and zero for an empty string.
fn estimate_string(value: &str) -> i64 {
    if value.is_empty() {
        return 0;
    }
    1 + (value.len() as i64 - 1) / 3
}

fn estimate_tool_definition(definition: &ToolDefinition) -> i64 {
    let mut total = TOOL_DEFINITION_FRAMING_TOKENS;
    total = saturating_add(total, estimate_string(&definition.name));
    total = saturating_add(total, estimate_string(&definition.description));
    // Go marshals `Parameters` and skips the cost on a marshal error. A
    // `RawValue` is already the marshalled form, and `None` marshals to
    // `null`, which Go also counts.
    let schema = definition
        .parameters
        .as_ref()
        .map_or("null", |raw| raw.get());
    saturating_add(total, estimate_string(schema))
}

/// Finds the newest assistant message inside the active checkpoint that
/// reported an input-token count, returning its index and that count.
fn request_usage_anchor(
    messages: &[Message],
    latest: Option<&CompactionMetadata>,
) -> Option<(usize, i64)> {
    let start = match latest {
        Some(latest) => {
            if latest.first_post_checkpoint_message_id.is_empty() {
                return None;
            }
            messages
                .iter()
                .position(|message| message.id == latest.first_post_checkpoint_message_id)?
        }
        None => 0,
    };
    for index in (start..messages.len()).rev() {
        let message = &messages[index];
        if message.role != crate::model::Role::Assistant {
            continue;
        }
        match message.usage {
            Some(usage) if usage.input_tokens > 0 => return Some((index, usage.input_tokens)),
            _ => continue,
        }
    }
    None
}

/// Adds two estimates, clamping at [`i64::MAX`] and ignoring non-positive
/// deltas. Port of `saturatingEstimateAdd`.
pub fn saturating_add(total: i64, delta: i64) -> i64 {
    if total == i64::MAX || delta <= 0 {
        return total;
    }
    total.saturating_add(delta)
}

/// Sums [`estimate_message`] over a slice, saturating.
pub fn estimate_messages(messages: &[Message]) -> i64 {
    messages.iter().fold(0, |total, message| {
        saturating_add(total, estimate_message(message))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Role, Usage};
    use serde_json::value::RawValue;

    /// The Go test's independent restatement of the formula.
    fn formula(value: &str) -> i64 {
        (value.len() as i64 + 2) / 3
    }

    /// The Go test's independent restatement of the per-message sum.
    fn message_formula(message: &Message) -> i64 {
        let mut want = 6;
        for block in &message.blocks {
            want += match block.block_type {
                BlockType::Text => 2 + formula(&block.text),
                BlockType::Image => IMAGE_BLOCK_TOKENS,
                BlockType::ToolCall => {
                    12 + formula(&block.tool_call_id)
                        + formula(&block.tool_name)
                        + formula(block.arguments.as_ref().map_or("", |raw| raw.get()))
                }
                BlockType::ToolResult => {
                    8 + formula(&block.text)
                        + formula(&block.tool_call_id)
                        + formula(&block.tool_name)
                }
                BlockType::Other(_) => 0,
            };
        }
        want
    }

    fn raw(json: &str) -> Option<Box<RawValue>> {
        Some(RawValue::from_string(json.to_owned()).expect("valid JSON"))
    }

    fn text_message(id: &str, role: Role, text: &str) -> Message {
        Message {
            id: id.to_owned(),
            role,
            blocks: vec![Block::text(text)],
            ..Message::default()
        }
    }

    fn with_usage(mut message: Message, input: i64, cached: i64) -> Message {
        message.usage = Some(Usage {
            input_tokens: input,
            output_tokens: 0,
            cached_input_tokens: cached,
        });
        message
    }

    #[test]
    fn estimate_string_uses_the_utf8_ceiling_table() {
        let code = "func main() {\n\tprintln(\"hi\")\n}\n";
        for (value, want) in [
            ("", 0),
            ("abc", 1),
            ("abcd", 2),
            ("你好", 2),
            ("🙂", 2),
            (code, formula(code)),
        ] {
            assert_eq!(estimate_string(value), want, "estimate_string({value:?})");
        }
    }

    #[test]
    fn estimate_message_uses_exact_framing() {
        let user = Message {
            role: Role::User,
            blocks: vec![Block::text("hello")],
            ..Message::default()
        };
        assert_eq!(estimate_message(&user), 6 + 2 + formula("hello"));

        let assistant = Message {
            role: Role::Assistant,
            blocks: vec![
                Block::text("done"),
                Block {
                    block_type: BlockType::ToolCall,
                    tool_call_id: "call-1".into(),
                    tool_name: "read".into(),
                    arguments: raw(r#"{"path":"README.md"}"#),
                    ..Block::default()
                },
            ],
            ..Message::default()
        };
        assert_eq!(
            estimate_message(&assistant),
            6 + (2 + formula("done"))
                + (12 + formula("call-1") + formula("read") + formula(r#"{"path":"README.md"}"#))
        );

        let tool = Message {
            role: Role::Tool,
            blocks: vec![Block {
                block_type: BlockType::ToolResult,
                text: "contents".into(),
                tool_call_id: "call-1".into(),
                tool_name: "read".into(),
                is_error: true,
                ..Block::default()
            }],
            ..Message::default()
        };
        assert_eq!(
            estimate_message(&tool),
            6 + 8 + formula("contents") + formula("call-1") + formula("read")
        );

        let summary = Message {
            role: Role::Context,
            blocks: vec![Block::text("[Compaction summary]\nkeep this")],
            ..Message::default()
        };
        assert_eq!(
            estimate_message(&summary),
            6 + 2 + formula("[Compaction summary]\nkeep this")
        );
    }

    #[test]
    fn estimate_request_falls_back_to_stable_system_messages_and_tools() {
        // The two schemas differ only in key order. Both serialize through a
        // sorted map, so the estimate cannot depend on the author's ordering.
        let schema_a = serde_json::json!({
            "type": "object",
            "properties": {"path": {"type": "string"}, "mode": {"enum": ["r", "w"]}},
            "required": ["path"],
        });
        let schema_b = serde_json::json!({
            "required": ["path"],
            "properties": {"mode": {"enum": ["r", "w"]}, "path": {"type": "string"}},
            "type": "object",
        });
        let serialized = serde_json::to_string(&schema_a).expect("encodes");
        assert_eq!(
            serialized,
            serde_json::to_string(&schema_b).expect("encodes")
        );

        let messages = vec![
            text_message("", Role::User, "inspect src"),
            text_message("", Role::Assistant, "working"),
        ];
        let tool = |schema: &serde_json::Value| ToolDefinition {
            name: "read".into(),
            description: "Read a file".into(),
            parameters: raw(&serde_json::to_string(schema).expect("encodes")),
        };
        let request_a = Request {
            system_prompt: "system prompt".into(),
            messages: messages.clone(),
            tools: vec![tool(&schema_a)],
            ..Request::default()
        };
        let request_b = Request {
            tools: vec![tool(&schema_b)],
            ..request_a.clone()
        };

        let want = 3
            + formula("system prompt")
            + (6 + 2 + formula("inspect src"))
            + (6 + 2 + formula("working"))
            + (16 + formula("read") + formula("Read a file") + formula(&serialized));
        assert_eq!(estimate_request(&request_a, None), want);
        assert_eq!(estimate_request(&request_b, None), want);
    }

    #[test]
    fn estimate_request_uses_the_prompt_usage_anchor_without_double_counting_cache() {
        let request = Request {
            messages: vec![
                with_usage(text_message("a", Role::Assistant, "abc"), 100, 80),
                text_message("b", Role::User, "next"),
            ],
            ..Request::default()
        };
        let want =
            100 + estimate_message(&request.messages[0]) + estimate_message(&request.messages[1]);
        assert_eq!(estimate_request(&request, None), want);
    }

    #[test]
    fn estimate_request_ignores_context_usage_anchors() {
        let request = Request {
            system_prompt: "system".into(),
            messages: vec![
                with_usage(
                    text_message("", Role::Context, "[Compaction summary]\nsummary"),
                    400,
                    200,
                ),
                text_message("", Role::User, "next"),
            ],
            ..Request::default()
        };
        let want = 3
            + formula("system")
            + message_formula(&request.messages[0])
            + message_formula(&request.messages[1]);
        assert_eq!(estimate_request(&request, None), want);
    }

    #[test]
    fn a_checkpoint_without_a_post_checkpoint_message_permits_no_anchor() {
        let request = Request {
            system_prompt: "system".into(),
            messages: vec![
                with_usage(text_message("a", Role::Assistant, "retained"), 250, 180),
                text_message("b", Role::User, "next"),
            ],
            ..Request::default()
        };
        let want = 3
            + formula("system")
            + message_formula(&request.messages[0])
            + message_formula(&request.messages[1]);
        assert_eq!(
            estimate_request(&request, Some(&CompactionMetadata::default())),
            want
        );
    }

    #[test]
    fn the_anchor_search_starts_at_the_checkpoint_floor() {
        let request = Request {
            messages: vec![
                with_usage(
                    text_message("old-assistant", Role::Assistant, "retained tail"),
                    500,
                    300,
                ),
                text_message("post-user", Role::User, "continue"),
                with_usage(
                    text_message("post-assistant", Role::Assistant, "current"),
                    70,
                    20,
                ),
                text_message("tail", Role::User, "next"),
            ],
            ..Request::default()
        };
        let latest = CompactionMetadata {
            first_post_checkpoint_message_id: "post-user".into(),
            ..CompactionMetadata::default()
        };
        let want =
            70 + estimate_message(&request.messages[2]) + estimate_message(&request.messages[3]);
        assert_eq!(estimate_request(&request, Some(&latest)), want);
    }

    #[test]
    fn a_checkpoint_floor_without_an_eligible_anchor_falls_back() {
        let request = Request {
            system_prompt: "system".into(),
            messages: vec![
                with_usage(
                    text_message("old-assistant", Role::Assistant, "retained tail"),
                    500,
                    300,
                ),
                text_message("post-user", Role::User, "continue"),
                text_message("tail", Role::User, "next"),
            ],
            ..Request::default()
        };
        let latest = CompactionMetadata {
            first_post_checkpoint_message_id: "post-user".into(),
            ..CompactionMetadata::default()
        };
        let want = 3
            + formula("system")
            + message_formula(&request.messages[0])
            + message_formula(&request.messages[1])
            + message_formula(&request.messages[2]);
        assert_eq!(estimate_request(&request, Some(&latest)), want);
    }

    #[test]
    fn estimate_request_saturates_at_the_integer_ceiling() {
        let request = Request {
            messages: vec![
                with_usage(
                    text_message("a", Role::Assistant, "abc"),
                    i64::MAX,
                    i64::MAX,
                ),
                text_message("b", Role::User, "next"),
            ],
            ..Request::default()
        };
        assert_eq!(estimate_request(&request, None), i64::MAX);
    }
}
