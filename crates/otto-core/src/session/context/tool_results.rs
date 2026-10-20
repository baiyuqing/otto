//! Repair of interrupted tool calls and the stand-in results for calls without a recorded result.

use super::*;

/// Text for the first call in a run of calls a session never recorded a
/// result for. That call may have started running before the session
/// stopped, so its effects are unknown rather than absent.
pub const MAY_HAVE_RUN_TOOL_RESULT_TEXT: &str = "tool result missing from prior session: the \
    session stopped while this call was running or before it started, so it may have run fully, \
    partly, or not at all; check its effects before repeating it";

/// Text for every call after the first in such a run. Otto runs the tool
/// calls of one assistant message one at a time, so a call after the one
/// that was interrupted never started.
pub const NOT_EXECUTED_TOOL_RESULT_TEXT: &str = "tool result missing from prior session: this \
    call was not executed, because the session stopped at an earlier call in the same assistant \
    message";

/// Builds the stand-in results for one run of unanswered tool calls from a
/// single assistant message, in call order. The first call may have run
/// before the session stopped; every later call never started, because Otto
/// runs one call at a time and appends each result before the next call
/// starts. Both are error results, so the model reads each call as failed
/// rather than as answered.
pub fn missing_tool_results(calls: &[Block]) -> Vec<Block> {
    calls
        .iter()
        .enumerate()
        .map(|(index, call)| {
            let text = if index == 0 {
                MAY_HAVE_RUN_TOOL_RESULT_TEXT
            } else {
                NOT_EXECUTED_TOOL_RESULT_TEXT
            };
            Block {
                block_type: BlockType::ToolResult,
                text: text.into(),
                tool_call_id: call.tool_call_id.clone(),
                tool_name: call.tool_name.clone(),
                is_error: true,
                operation_metadata: Some(ToolResultMetadata {
                    operation_id: None,
                    disposition: OperationDisposition::Interrupted,
                    effect_certainty: if index == 0 {
                        EffectCertainty::Unknown
                    } else {
                        EffectCertainty::NotStarted
                    },
                    stop_reason: Some(OperationStopReason::ProcessLost),
                }),
                ..Block::default()
            }
        })
        .collect()
}

/// Splices a stand-in result in for every tool call this history leaves
/// unanswered *before its end*, and returns one description per repair.
///
/// A run of calls that reaches the end is left alone: that is a session
/// interrupted mid-turn, and the native store answers it durably when it
/// opens the file. A run with history after it cannot be answered in the
/// file at all, because Pi history is append-only and a result belongs
/// directly after its call, so it is repaired here instead, on every
/// resolve, and the file keeps the gap. Without this the whole session stops
/// resolving, which is one interrupted call poisoning every later request.
///
/// Damage no repair can describe -- a duplicate call id, a result with no
/// call, a tool message carrying something else -- is left untouched for
/// [`pending_tool_calls`] to reject.
pub(super) fn repair_interrupted_tool_calls(
    messages: &mut Vec<Message>,
    operation_ledger: &OperationLedger,
) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut pending: Vec<Block> = Vec::new();
    let mut anchor = Message::default();
    let mut splices: Vec<(usize, Vec<Message>)> = Vec::new();
    let mut repairs = Vec::new();

    for (index, message) in messages.iter().enumerate() {
        if message.role == Role::Tool {
            for block in &message.blocks {
                if block.block_type != BlockType::ToolResult {
                    return Vec::new();
                }
                let answered = pending.iter().position(|call| {
                    call.tool_call_id == block.tool_call_id && call.tool_name == block.tool_name
                });
                let Some(answered) = answered else {
                    return Vec::new();
                };
                pending.remove(answered);
            }
            continue;
        }
        if !pending.is_empty() {
            let (stand_ins, warnings) =
                stand_in_results(&anchor, std::mem::take(&mut pending), operation_ledger);
            splices.push((index, stand_ins));
            repairs.extend(warnings);
        }
        if message.role != Role::Assistant {
            continue;
        }
        for block in &message.blocks {
            if block.block_type != BlockType::ToolCall {
                continue;
            }
            if !seen.insert(block.tool_call_id.clone()) {
                return Vec::new();
            }
            pending.push(block.clone());
        }
        anchor = message.clone();
    }

    for (_, stand_ins) in &splices {
        for message in stand_ins {
            for block in &message.blocks {
                repairs.push(format!(
                    "repaired dangling tool call {} in memory; the stored history keeps the gap",
                    block.tool_call_id
                ));
            }
        }
    }
    // Late to early, so an earlier splice does not move a later index.
    for (index, stand_ins) in splices.into_iter().rev() {
        messages.splice(index..index, stand_ins);
    }
    repairs
}

/// One stand-in tool message per unanswered `call`, carrying ids derived from
/// the message that made the calls the way a compaction's retained tail
/// derives its own.
fn stand_in_results(
    anchor: &Message,
    calls: Vec<Block>,
    operation_ledger: &OperationLedger,
) -> (Vec<Message>, Vec<String>) {
    let mut warnings = Vec::new();
    let messages = missing_tool_results(&calls)
        .into_iter()
        .enumerate()
        .map(|(index, mut block)| {
            if let Some(record) = operation_ledger.operation_for_tool_call(&block.tool_call_id) {
                block.operation_metadata = Some(if record.corrupt {
                    warnings.push(format!(
                        "operation history for dangling tool call {} is corrupt; projected effect certainty as unknown",
                        block.tool_call_id
                    ));
                    interrupted_tool_result_metadata(Some(record.operation_id.clone()))
                } else if let Some(outcome) = record.terminal.as_ref() {
                    ToolResultMetadata {
                        operation_id: Some(record.operation_id.clone()),
                        disposition: outcome.disposition,
                        effect_certainty: outcome.effect_certainty,
                        stop_reason: outcome.stop_reason,
                    }
                } else {
                    interrupted_tool_result_metadata(Some(record.operation_id.clone()))
                });
            }
            Message {
                id: format!("{}-repair-{index}", anchor.id),
                role: Role::Tool,
                created_at: anchor.created_at,
                blocks: vec![block],
                ..Message::default()
            }
        })
        .collect();
    (messages, warnings)
}

fn interrupted_tool_result_metadata(
    operation_id: Option<crate::model::OperationId>,
) -> ToolResultMetadata {
    ToolResultMetadata {
        operation_id,
        disposition: OperationDisposition::Interrupted,
        effect_certainty: EffectCertainty::Unknown,
        stop_reason: Some(OperationStopReason::ProcessLost),
    }
}

/// Walks a message list and returns the tool calls that are still unanswered.
/// Any ordering violation is an error, so this doubles as the sequence check.
pub fn pending_tool_calls(messages: &[Message]) -> Result<Vec<Block>, PiError> {
    let mut pending: HashMap<String, Block> = HashMap::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut order: Vec<String> = Vec::new();
    for message in messages {
        match message.role {
            Role::Assistant => {
                if !pending.is_empty() {
                    return Err(PiError::invalid(
                        "unresolved tool calls must be followed by tool results",
                    ));
                }
                for block in &message.blocks {
                    if block.block_type != BlockType::ToolCall {
                        continue;
                    }
                    if !seen.insert(block.tool_call_id.clone()) {
                        return Err(PiError::invalid("duplicate tool-call id"));
                    }
                    pending.insert(block.tool_call_id.clone(), block.clone());
                    order.push(block.tool_call_id.clone());
                }
            }
            Role::Tool => {
                if message.blocks.is_empty() {
                    return Err(PiError::invalid("tool message must contain a tool result"));
                }
                for block in &message.blocks {
                    if block.block_type != BlockType::ToolResult {
                        return Err(PiError::invalid("tool message contains a non-result block"));
                    }
                    let call = pending
                        .get(&block.tool_call_id)
                        .ok_or_else(|| PiError::invalid("tool result has no pending call"))?;
                    if call.tool_name != block.tool_name {
                        return Err(PiError::invalid(
                            "tool result name does not match pending call",
                        ));
                    }
                    pending.remove(&block.tool_call_id);
                }
            }
            _ => {
                if !pending.is_empty() {
                    return Err(PiError::invalid(
                        "unresolved tool calls must be followed by tool results",
                    ));
                }
            }
        }
    }
    Ok(order
        .into_iter()
        .filter_map(|id| pending.remove(&id))
        .collect())
}

/// True when `arguments` is a JSON object, the only tool-call argument shape
/// Otto persists.
pub fn valid_tool_arguments(arguments: Option<&RawValue>) -> bool {
    let Some(arguments) = arguments else {
        return false;
    };
    let trimmed = arguments.get().trim();
    trimmed.starts_with('{')
        && trimmed.ends_with('}')
        && serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(trimmed).is_ok()
}
