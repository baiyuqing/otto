//! Resolution of a Pi v3 entry tree into Otto's message list, and the
//! model-to-entry direction used when appending.
//!
//! Nothing here touches the filesystem, the clock, or randomness, so the whole
//! module builds for `wasm32-unknown-unknown`.
//!
//! Ownership: every function takes borrowed input and returns owned values.
//! Concurrency: no shared state. Errors: every failure is a [`PiError`],
//! classified by [`PiError::kind`].

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::LazyLock;

use chrono::{DateTime, SecondsFormat, Utc};
use regex::Regex;
use serde_json::value::RawValue;

use crate::model::{
    Block, BlockType, ContextMetadata, EffectCertainty, FinishReason, Message,
    OperationDisposition, OperationStopReason, Role, ToolResultMetadata, Usage, zero_time,
};

use super::compaction::{
    compaction_aware_path, compaction_has_retained_tail, safe_context_token_count,
};
use super::operation::{
    DecodedOperationFact, OPERATION_CUSTOM_TYPE, OperationLedger, decode_operation_raw,
};
use super::pi::{
    PiContentBlock, PiContextEdit, PiCustomMessage, PiEntry, PiMessage, PiOttoDetails, PiUsage,
    decode_pi_otto_details, encode_pi_otto_details,
};
use super::remote_compaction::{
    REMOTE_COMPACTION_CUSTOM_TYPE, RemoteCompaction, decode_remote_compaction,
};
use super::turn::{InterruptedTurn, TURN_CUSTOM_TYPE, apply_turn_record, decode_turn_record};
use super::types::{Header, RuntimeMetadata, Snapshot, Warning};
use super::{PiError, PiErrorKind};

/// Context type written for a compaction checkpoint summary.
pub const COMPACTION_CONTEXT_TYPE: &str = "compaction";
/// Context type written for a branch summary.
pub const BRANCH_CONTEXT_TYPE: &str = "branch_summary";
/// Custom entry type carrying Otto's runtime metadata.
pub const OTTO_RUNTIME_CUSTOM_TYPE: &str = "otto.runtime";
/// Custom entry type carrying the tool names the session's latest run sent;
/// written at run start when absent or different.
pub const CAPABILITIES_CUSTOM_TYPE: &str = "otto.capabilities";

const MAX_CONTEXT_WARNINGS: usize = 32;
const MAX_WARNING_TYPE_BYTES: usize = 32;

// Otto's own context message types, used for sub-agent notifications. They
// round-trip without the "[Custom context: ...]" decoration applied to custom
// types written by other Pi-compatible tools.
const TASK_NOTIFICATION_CONTEXT_TYPE: &str = "task_notification";
const PARENT_MESSAGE_CONTEXT_TYPE: &str = "parent_message";
/// Context type of the one-time tool-set change notice.
pub const CAPABILITY_UPDATE_CONTEXT_TYPE: &str = "capability_update";

static ENTRY_ID_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new("^[0-9a-f]{8}$").expect("entry id pattern compiles"));

/// True when `id` is a well-formed Pi entry id: eight lowercase hex digits.
pub fn is_pi_entry_id(id: &str) -> bool {
    ENTRY_ID_PATTERN.is_match(id)
}

/// Everything a frontend needs after replaying one session's active branch.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedContext {
    /// The unmodified active transcript, used for history, recovery, and
    /// sequencing checks.
    pub messages: Vec<Message>,
    /// The active transcript after Pi `context_edit` projection, used only
    /// for provider context and compaction.
    pub model_messages: Vec<Message>,
    /// The newest valid provider-managed checkpoint on the active path.
    pub remote_compaction: Option<RemoteCompaction>,
    /// The model projection strictly after `remote_compaction`. Empty when a
    /// checkpoint is the leaf; equal to `model_messages` when none is active.
    pub post_remote_compaction_messages: Vec<Message>,
    pub runtime: RuntimeMetadata,
    pub usage: Usage,
    pub usage_present: bool,
    pub session_name: String,
    pub thinking_level: String,
    pub operation_ledger: OperationLedger,
    /// The latest `otto.capabilities` entry on the active path, or `None`
    /// when there is none or it is malformed.
    pub observed_capabilities: Option<Capabilities>,
    /// The newest `otto.turn` start on the active path without a matching
    /// end; see [`crate::session::turn`].
    pub interrupted_turn: Option<InterruptedTurn>,
    /// The steps of the latest valid `otto.plan` record on the active path;
    /// see [`crate::session::plan`].
    pub plan: Vec<crate::session::plan::PlanStep>,
    /// Every input id delivered on the active path, including messages a
    /// compaction checkpoint has since summarized away, so an input is
    /// recognised as delivered for as long as its entry is on the path.
    pub input_ids: HashSet<String>,
}

impl Default for ResolvedContext {
    fn default() -> Self {
        Self {
            messages: Vec::new(),
            model_messages: Vec::new(),
            remote_compaction: None,
            post_remote_compaction_messages: Vec::new(),
            runtime: RuntimeMetadata::default(),
            usage: Usage::default(),
            usage_present: false,
            session_name: String::new(),
            thinking_level: "off".into(),
            operation_ledger: OperationLedger::default(),
            observed_capabilities: None,
            interrupted_turn: None,
            plan: Vec::new(),
            input_ids: HashSet::new(),
        }
    }
}

/// The input ids on the whole active `path`: those of the context
/// `messages` plus those of entries before the latest compaction checkpoint.
/// Entries a checkpoint summarized were valid when appended; one that no
/// longer decodes contributes no id rather than failing the resolution.
fn path_input_ids(
    path: &[PiEntry],
    context_path: &[PiEntry],
    messages: &[Message],
) -> HashSet<String> {
    let in_context: HashSet<&str> = context_path.iter().map(|entry| entry.id.as_str()).collect();
    let summarized = path
        .iter()
        .filter(|entry| !in_context.contains(entry.id.as_str()))
        .flat_map(|entry| pi_entry_to_context_messages(entry).unwrap_or_default());
    messages
        .iter()
        .cloned()
        .chain(summarized)
        .filter(|message| !message.input_id.is_empty())
        .map(|message| message.input_id)
        .collect()
}

/// Entry lookup built once per resolution: id to entry and id to file position.
pub struct ContextEntryIndex {
    by_id: HashMap<String, PiEntry>,
    indexes: HashMap<String, usize>,
}

#[derive(Default)]
struct WarningCollector {
    warnings: Vec<Warning>,
    omitted: bool,
}

impl WarningCollector {
    fn add(&mut self, message: String) {
        if self.omitted {
            return;
        }
        if self.warnings.len() < MAX_CONTEXT_WARNINGS {
            self.warnings.push(Warning::new(message));
            return;
        }
        self.warnings[MAX_CONTEXT_WARNINGS - 1] =
            Warning::new("additional session warnings omitted");
        self.omitted = true;
    }
}

/// Replays the entry tree from `leaf_id` back to its root and returns the
/// messages, runtime and token totals in force there.
///
/// A tool call the history left unanswered with more history after it is
/// repaired here, in memory, and warned about; see
/// [`repair_interrupted_tool_calls`] for why the file cannot hold that
/// repair and why a trailing one is left alone.
///
/// Warnings describe entries that were skipped, re-rooted or repaired; they
/// are never fatal. An error means the session cannot be represented at all,
/// and no warnings are returned with it.
pub fn build_context(
    entries: &[PiEntry],
    leaf_id: &str,
) -> Result<(ResolvedContext, Vec<Warning>), PiError> {
    let (index, warnings) = index_context_entries(entries)?;
    let mut collector = WarningCollector {
        warnings,
        omitted: false,
    };

    let path = active_context_path(entries, leaf_id, &index)?;

    let mut resolved = ResolvedContext::default();
    let mut latest_runtime: Option<RuntimeMetadata> = None;
    let mut latest_model_change: Option<RuntimeMetadata> = None;
    let mut latest_assistant: Option<RuntimeMetadata> = None;
    let mut remote_compaction_entry_index = None;
    for entry in &path {
        match entry.type_name.as_str() {
            "message" => {
                let message = entry
                    .message
                    .as_ref()
                    .ok_or_else(|| PiError::invalid("message payload is required"))?;
                if message.role == "assistant" {
                    latest_assistant = Some(RuntimeMetadata {
                        profile: String::new(),
                        provider: message.provider.clone(),
                        model: message.model.clone(),
                    });
                    if let Some(usage) = pi_assistant_usage_to_model(message)? {
                        resolved.usage = add_resolved_usage(resolved.usage, Some(&usage));
                        resolved.usage_present = true;
                    }
                }
            }
            "model_change" => {
                let change = entry
                    .model_change
                    .as_ref()
                    .ok_or_else(|| PiError::invalid("model_change payload is required"))?;
                latest_model_change = Some(RuntimeMetadata {
                    profile: String::new(),
                    provider: change.provider.clone(),
                    model: change.model_id.clone(),
                });
            }
            "thinking_level_change" => {
                let change = entry
                    .thinking_level_change
                    .as_ref()
                    .ok_or_else(|| PiError::invalid("thinking_level_change payload is required"))?;
                resolved.thinking_level = change.thinking_level.clone();
            }
            "compaction" => {
                let compaction = entry
                    .compaction
                    .as_ref()
                    .ok_or_else(|| PiError::invalid("compaction payload is required"))?;
                // The newest checkpoint kind wins. A later local summary must
                // not leave an older provider-owned opaque prefix active.
                resolved.remote_compaction = None;
                remote_compaction_entry_index = None;
                let usage = optional_pi_usage_to_model(compaction.usage.as_ref())?;
                resolved.usage = add_resolved_usage(resolved.usage, usage.as_ref());
                if compaction.usage.is_some() {
                    resolved.usage_present = true;
                }
            }
            "branch_summary" => {
                let summary = entry
                    .branch_summary
                    .as_ref()
                    .ok_or_else(|| PiError::invalid("branch_summary payload is required"))?;
                let usage = optional_pi_usage_to_model(summary.usage.as_ref())?;
                resolved.usage = add_resolved_usage(resolved.usage, usage.as_ref());
                if summary.usage.is_some() {
                    resolved.usage_present = true;
                }
            }
            "custom_message" => {
                let custom = entry
                    .custom_message
                    .as_ref()
                    .ok_or_else(|| PiError::invalid("custom_message payload is required"))?;
                // A notification (such as a finished sub-agent task) reports
                // its own token cost in the Otto extension, and that cost
                // belongs to this session's totals the same way an assistant
                // round trip's does.
                if let Some(details) = decode_pi_otto_details(custom.details.as_deref())?
                    && let Some(usage) = details.usage
                {
                    resolved.usage = add_resolved_usage(resolved.usage, Some(&usage));
                    resolved.usage_present = true;
                }
            }
            "custom" => {
                let custom = entry
                    .custom
                    .as_ref()
                    .ok_or_else(|| PiError::invalid("custom payload is required"))?;
                if custom.custom_type == OTTO_RUNTIME_CUSTOM_TYPE {
                    latest_runtime = Some(decode_runtime_metadata(custom.data.as_deref())?);
                } else if custom.custom_type == CAPABILITIES_CUSTOM_TYPE {
                    let raw = custom.data.as_deref().map(RawValue::get);
                    resolved.observed_capabilities = decode_capabilities(raw.unwrap_or_default());
                    if resolved.observed_capabilities.is_none() {
                        collector.add(format!(
                            "ignored malformed {CAPABILITIES_CUSTOM_TYPE} entry {}",
                            entry.id
                        ));
                    }
                } else if custom.custom_type == TURN_CUSTOM_TYPE {
                    let raw = custom.data.as_deref().map(RawValue::get);
                    match decode_turn_record(raw.unwrap_or_default()) {
                        Some(record) => apply_turn_record(&mut resolved.interrupted_turn, &record),
                        None => collector.add(format!(
                            "ignored malformed {TURN_CUSTOM_TYPE} entry {}",
                            entry.id
                        )),
                    }
                } else if custom.custom_type == crate::session::plan::PLAN_CUSTOM_TYPE {
                    let raw = custom.data.as_deref().map(RawValue::get);
                    match crate::session::plan::decode_plan(raw.unwrap_or_default()) {
                        Some(steps) => resolved.plan = steps,
                        None => collector.add(format!(
                            "ignored malformed {} entry {}",
                            crate::session::plan::PLAN_CUSTOM_TYPE,
                            entry.id
                        )),
                    }
                } else if custom.custom_type == REMOTE_COMPACTION_CUSTOM_TYPE {
                    let raw = custom
                        .data
                        .as_deref()
                        .map(RawValue::get)
                        .unwrap_or_default();
                    match decode_remote_compaction(raw) {
                        Ok(checkpoint) => {
                            resolved.remote_compaction = Some(checkpoint);
                            remote_compaction_entry_index = index.indexes.get(&entry.id).copied();
                        }
                        Err(_) => collector.add(format!(
                            "ignored malformed {REMOTE_COMPACTION_CUSTOM_TYPE} entry {}",
                            entry.id
                        )),
                    }
                } else if custom.custom_type == OPERATION_CUSTOM_TYPE {
                    match decode_operation_raw(custom.data.as_deref()).map_err(|error| {
                        PiError::invalid(format!("invalid otto.operation fact: {error}"))
                    })? {
                        DecodedOperationFact::Fact(fact) => {
                            let warning_count = resolved.operation_ledger.warnings().len();
                            resolved.operation_ledger.fold_history([fact]);
                            for warning in &resolved.operation_ledger.warnings()[warning_count..] {
                                collector.add(warning.clone());
                            }
                        }
                        DecodedOperationFact::Unsupported { .. } => {}
                    }
                }
            }
            "session_info" => {
                let info = entry
                    .session_info
                    .as_ref()
                    .ok_or_else(|| PiError::invalid("session_info payload is required"))?;
                resolved.session_name = info.name.clone().unwrap_or_default();
            }
            other => {
                if !known_pi_entry_type(other) {
                    collector.add(format!(
                        "ignored unknown active session entry {} of type {}",
                        entry.id,
                        sanitize_warning_type(other)
                    ));
                }
            }
        }
    }

    if let Some(runtime) = latest_runtime.or(latest_model_change).or(latest_assistant) {
        resolved.runtime = runtime;
    }

    let context_path = compaction_aware_path(&path)?;
    for entry in &context_path {
        resolved
            .messages
            .extend(pi_entry_to_context_messages(entry)?);
    }
    for repair in repair_interrupted_tool_calls(&mut resolved.messages, &resolved.operation_ledger)
    {
        collector.add(repair);
    }
    pending_tool_calls(&resolved.messages)?;
    resolved.input_ids = path_input_ids(&path, &context_path, &resolved.messages);

    let edits = context_edits(&path, &context_path, &mut collector);
    for entry in context_path {
        let entry_id = entry.id.clone();
        let Some(projected) = project_context_edit(entry, edits.get(&entry_id))? else {
            continue;
        };
        let projected_messages = pi_entry_to_context_messages(&projected)?;
        resolved.model_messages.extend(projected_messages.clone());
        let is_after_remote = remote_compaction_entry_index.is_some_and(|checkpoint_index| {
            index
                .indexes
                .get(&entry_id)
                .is_some_and(|entry_index| *entry_index > checkpoint_index)
        });
        if is_after_remote {
            resolved
                .post_remote_compaction_messages
                .extend(projected_messages);
        }
    }
    if resolved.remote_compaction.is_none() {
        resolved.post_remote_compaction_messages = resolved.model_messages.clone();
    }
    Ok((resolved, collector.warnings))
}

fn context_edits<'a>(
    path: &'a [PiEntry],
    context_path: &[PiEntry],
    collector: &mut WarningCollector,
) -> HashMap<String, &'a PiContextEdit> {
    let editable: HashMap<&str, &PiEntry> = path
        .iter()
        .filter(|entry| context_entry_is_editable(entry))
        .map(|entry| (entry.id.as_str(), entry))
        .collect();
    let retained_ids: HashSet<&str> = context_path.iter().map(|entry| entry.id.as_str()).collect();
    let mut edits = HashMap::new();
    for entry in path {
        if entry.type_name != "context_edit" {
            continue;
        }
        let Some(edit) = entry.context_edit.as_ref() else {
            collector.add(format!("ignored malformed context_edit entry {}", entry.id));
            continue;
        };
        if !is_pi_entry_id(&edit.target_id) || !editable.contains_key(edit.target_id.as_str()) {
            collector.add(format!(
                "ignored context_edit entry {} with an unsupported target",
                entry.id
            ));
            continue;
        }
        if !retained_ids.contains(edit.target_id.as_str()) {
            collector.add(format!(
                "ignored context_edit entry {} because its target is not retained after compaction",
                entry.id
            ));
            continue;
        }
        edits.insert(edit.target_id.clone(), edit);
    }
    edits
}

fn context_entry_is_editable(entry: &PiEntry) -> bool {
    match entry.type_name.as_str() {
        "custom_message" => true,
        "message" => matches!(
            entry.message.as_deref(),
            Some(message)
                if matches!(message.role.as_str(), "user" | "custom")
                    || (message.role == "assistant"
                        && !message
                            .content_blocks
                            .iter()
                            .any(|block| block.type_name == "toolCall"))
        ),
        _ => false,
    }
}

fn project_context_edit(
    mut entry: PiEntry,
    edit: Option<&&PiContextEdit>,
) -> Result<Option<PiEntry>, PiError> {
    let Some(edit) = edit else {
        return Ok((entry.type_name != "context_edit").then_some(entry));
    };
    let Some(_replacement) = edit.replacement.as_ref() else {
        return Ok(None);
    };
    match entry.type_name.as_str() {
        "message" => {
            let message = entry
                .message
                .as_mut()
                .ok_or_else(|| PiError::invalid("message payload is required"))?;
            message.content = edit.replacement_content.clone();
            message.content_text = edit.replacement_text.clone();
            message.content_blocks = edit.replacement_blocks.clone();
        }
        "custom_message" => {
            let custom = entry
                .custom_message
                .as_mut()
                .ok_or_else(|| PiError::invalid("custom_message payload is required"))?;
            custom.content = edit.replacement_content.clone();
            custom.content_text = edit.replacement_text.clone();
            custom.content_blocks = edit.replacement_blocks.clone();
        }
        _ => return Ok(Some(entry)),
    }
    entry.raw.clear();
    Ok(Some(entry))
}

pub fn index_context_entries(
    entries: &[PiEntry],
) -> Result<(ContextEntryIndex, Vec<Warning>), PiError> {
    let mut index = ContextEntryIndex {
        by_id: HashMap::with_capacity(entries.len()),
        indexes: HashMap::with_capacity(entries.len()),
    };
    let mut collector = WarningCollector::default();
    for (position, entry) in entries.iter().enumerate() {
        validate_context_entry_base(entry)
            .map_err(|error| error.context(format!("session entry {}", position + 1)))?;
        if index.by_id.contains_key(&entry.id) {
            return Err(PiError::invalid("duplicate entry id")
                .context(format!("session entry {}", position + 1)));
        }
        index.by_id.insert(entry.id.clone(), entry.clone());
        index.indexes.insert(entry.id.clone(), position);
    }

    let mut root_count = 0usize;
    for (position, entry) in entries.iter().enumerate() {
        let Some(parent_id) = entry.parent_id.as_ref() else {
            root_count += 1;
            continue;
        };
        let Some(parent_position) = index.indexes.get(parent_id).copied() else {
            root_count += 1;
            collector.add(format!(
                "session entry {} has missing parent {parent_id}; treating entry as a root",
                entry.id
            ));
            continue;
        };
        if *parent_id == entry.id {
            return Err(
                PiError::invalid("parent cycle").context(format!("session entry {}", position + 1))
            );
        }
        if parent_position >= position {
            return Err(
                PiError::invalid("parent id does not reference a prior entry")
                    .context(format!("session entry {}", position + 1)),
            );
        }
    }
    if root_count > 1 {
        collector.add("session contains multiple roots".into());
    }
    Ok((index, collector.warnings))
}

fn validate_context_entry_base(entry: &PiEntry) -> Result<(), PiError> {
    if entry.type_name.trim().is_empty() {
        return Err(PiError::invalid("entry type is required"));
    }
    if !is_pi_entry_id(&entry.id) {
        return Err(PiError::invalid(
            "entry id must be eight lowercase hexadecimal characters",
        ));
    }
    if parse_rfc3339(&entry.timestamp).is_none() {
        return Err(PiError::invalid("entry timestamp is invalid"));
    }
    if let Some(parent_id) = entry.parent_id.as_ref()
        && !is_pi_entry_id(parent_id)
    {
        return Err(PiError::invalid(
            "parent id must be eight lowercase hexadecimal characters",
        ));
    }
    Ok(())
}

pub fn active_context_path(
    entries: &[PiEntry],
    leaf_id: &str,
    index: &ContextEntryIndex,
) -> Result<Vec<PiEntry>, PiError> {
    if entries.is_empty() {
        if !leaf_id.is_empty() {
            return Err(PiError::invalid("active leaf does not exist"));
        }
        return Ok(Vec::new());
    }
    let mut current = index
        .by_id
        .get(leaf_id)
        .ok_or_else(|| PiError::invalid("active leaf does not exist"))?;
    let mut visited = HashSet::new();
    let mut path = Vec::new();
    loop {
        if !visited.insert(current.id.clone()) {
            return Err(PiError::invalid("active branch contains a cycle"));
        }
        path.push(current.clone());
        let Some(parent_id) = current.parent_id.as_ref() else {
            break;
        };
        let Some(parent) = index.by_id.get(parent_id) else {
            break;
        };
        current = parent;
    }
    path.reverse();
    Ok(path)
}

/// Converts one active-path entry into the messages it contributes. Entry
/// types that carry no message (labels, runtime changes) contribute none.
pub fn pi_entry_to_context_messages(entry: &PiEntry) -> Result<Vec<Message>, PiError> {
    let created_at = parse_rfc3339(&entry.timestamp)
        .ok_or_else(|| PiError::invalid("entry timestamp is invalid"))?;
    match entry.type_name.as_str() {
        "message" => {
            let wire = entry
                .message
                .as_ref()
                .ok_or_else(|| PiError::invalid("message payload is required"))?;
            Ok(vec![pi_message_to_context_message(
                wire, &entry.id, created_at,
            )?])
        }
        "compaction" => {
            let compaction = entry
                .compaction
                .as_ref()
                .ok_or_else(|| PiError::invalid("compaction payload is required"))?;
            let usage = optional_pi_usage_to_model(compaction.usage.as_ref())?;
            let mut summary = new_context_message(
                &entry.id,
                COMPACTION_CONTEXT_TYPE,
                true,
                format!("[Compaction summary]\n{}", compaction.summary),
                created_at,
                usage,
            );
            summary.context_tokens_before = safe_context_token_count(compaction.tokens_before);
            let mut messages = vec![summary];
            if compaction_has_retained_tail(compaction) {
                for (index, wire) in compaction
                    .retained_tail
                    .as_deref()
                    .unwrap_or_default()
                    .iter()
                    .enumerate()
                {
                    messages.push(pi_message_to_context_message(
                        wire,
                        &format!("{}-tail-{index}", entry.id),
                        DateTime::from_timestamp_millis(wire.timestamp).unwrap_or_else(zero_time),
                    )?);
                }
            }
            Ok(messages)
        }
        "branch_summary" => {
            let branch = entry
                .branch_summary
                .as_ref()
                .ok_or_else(|| PiError::invalid("branch_summary payload is required"))?;
            let usage = optional_pi_usage_to_model(branch.usage.as_ref())?;
            Ok(vec![new_context_message(
                &entry.id,
                BRANCH_CONTEXT_TYPE,
                true,
                format!("[Branch summary]\n{}", branch.summary),
                created_at,
                usage,
            )])
        }
        "custom_message" => {
            let custom = entry
                .custom_message
                .as_ref()
                .ok_or_else(|| PiError::invalid("custom_message payload is required"))?;
            let text = pi_context_text(custom.content_text.as_deref(), &custom.content_blocks)?;
            let mut message = new_context_message(
                &entry.id,
                &custom.custom_type,
                custom.display,
                custom_context_text(&custom.custom_type, &text),
                created_at,
                None,
            );
            if let Some(details) = decode_pi_otto_details(custom.details.as_deref())? {
                message.input_id = details.input_id;
                message.usage = details.usage;
                if !details.task_id.is_empty() {
                    message.context_metadata = Some(ContextMetadata {
                        task_id: details.task_id,
                    });
                }
            }
            Ok(vec![message])
        }
        _ => Ok(Vec::new()),
    }
}

fn pi_message_to_context_message(
    wire: &PiMessage,
    id: &str,
    created_at: DateTime<Utc>,
) -> Result<Message, PiError> {
    let mut message = Message {
        id: id.to_owned(),
        created_at,
        ..Message::default()
    };
    match wire.role.as_str() {
        "user" => {
            let blocks = pi_context_text_and_tool_blocks(wire, Role::User)?;
            if blocks.is_empty() {
                return Err(PiError::invalid("user message content is required"));
            }
            message.role = Role::User;
            message.blocks = blocks;
            if let Some(details) = decode_pi_otto_details(wire.details.as_deref())? {
                message.input_id = details.input_id;
            }
        }
        "assistant" => {
            let blocks = pi_context_text_and_tool_blocks(wire, Role::Assistant)?;
            let finish_reason = pi_stop_reason_to_model(&wire.stop_reason)?;
            validate_assistant_tool_finish(&blocks, Some(finish_reason.clone()))?;
            message.usage = pi_assistant_usage_to_model(wire)?;
            message.role = Role::Assistant;
            message.blocks = blocks;
            message.finish_reason = Some(finish_reason);
        }
        "toolResult" => {
            let text = pi_context_text(wire.content_text.as_deref(), &wire.content_blocks)?;
            if wire.tool_call_id.trim().is_empty()
                || wire.tool_name.trim().is_empty()
                || wire.is_error.is_none()
            {
                return Err(PiError::invalid("tool-result message is malformed"));
            }
            let operation_metadata =
                decode_pi_otto_details(wire.details.as_deref())?.and_then(|details| {
                    match (details.disposition, details.effect_certainty) {
                        (Some(disposition), Some(effect_certainty)) => Some(ToolResultMetadata {
                            operation_id: details.operation_id,
                            disposition,
                            effect_certainty,
                            stop_reason: details.stop_reason,
                        }),
                        _ => None,
                    }
                });
            message.role = Role::Tool;
            message.blocks = vec![Block {
                block_type: BlockType::ToolResult,
                text,
                data: String::new(),
                mime_type: String::new(),
                tool_call_id: wire.tool_call_id.clone(),
                tool_name: wire.tool_name.clone(),
                arguments: None,
                is_error: wire.is_error.unwrap_or(false),
                operation_metadata,
                ..Block::default()
            }];
        }
        "custom" => {
            let text = pi_context_text(wire.content_text.as_deref(), &wire.content_blocks)?;
            return Ok(new_context_message(
                id,
                &wire.custom_type,
                wire.display.unwrap_or(false),
                custom_context_text(&wire.custom_type, &text),
                created_at,
                None,
            ));
        }
        "branchSummary" => {
            return Ok(new_context_message(
                id,
                BRANCH_CONTEXT_TYPE,
                true,
                format!("[Branch summary]\n{}", wire.summary),
                created_at,
                None,
            ));
        }
        "compactionSummary" => {
            let mut summary = new_context_message(
                id,
                COMPACTION_CONTEXT_TYPE,
                true,
                format!("[Compaction summary]\n{}", wire.summary),
                created_at,
                None,
            );
            if let Some(tokens_before) = wire.tokens_before {
                summary.context_tokens_before = safe_context_token_count(tokens_before);
            }
            return Ok(summary);
        }
        _ => {
            return Err(PiError::new(
                PiErrorKind::UnsupportedContent,
                "Pi message role is not supported by Otto",
            ));
        }
    }
    Ok(message)
}

fn pi_context_text_and_tool_blocks(message: &PiMessage, role: Role) -> Result<Vec<Block>, PiError> {
    if let Some(text) = message.content_text.as_ref() {
        return Ok(vec![Block::text(text.clone())]);
    }
    let mut blocks = Vec::with_capacity(message.content_blocks.len());
    for block in &message.content_blocks {
        match block.type_name.as_str() {
            "text" => {
                if !block.text_signature.is_empty() {
                    return Err(PiError::new(
                        PiErrorKind::UnsupportedContent,
                        "provider-specific text signatures are not supported",
                    ));
                }
                blocks.push(Block::text(block.text.clone()));
            }
            "toolCall" => {
                if role != Role::Assistant {
                    return Err(PiError::new(
                        PiErrorKind::UnsupportedContent,
                        "tool call is incompatible with message role",
                    ));
                }
                if block.id.trim().is_empty()
                    || block.name.trim().is_empty()
                    || !valid_tool_arguments(block.arguments.as_deref())
                {
                    return Err(PiError::new(
                        PiErrorKind::UnsupportedContent,
                        "tool-call shape cannot be represented",
                    ));
                }
                if !block.thought_signature.is_empty() || !block.namespace.is_empty() {
                    return Err(PiError::new(
                        PiErrorKind::UnsupportedContent,
                        "provider-specific tool-call content is not supported",
                    ));
                }
                blocks.push(Block {
                    block_type: BlockType::ToolCall,
                    text: String::new(),
                    data: String::new(),
                    mime_type: String::new(),
                    tool_call_id: block.id.clone(),
                    tool_name: block.name.clone(),
                    arguments: block.arguments.clone(),
                    is_error: false,
                    operation_metadata: None,
                    ..Block::default()
                });
            }
            "image" => {
                if role != Role::User {
                    return Err(PiError::new(
                        PiErrorKind::UnsupportedContent,
                        "image content is incompatible with message role",
                    ));
                }
                let image = Block::image(block.data.clone(), block.mime_type.clone());
                image
                    .validate()
                    .map_err(|_| PiError::invalid("image content is malformed"))?;
                blocks.push(image);
            }
            // Otto writes unsigned assistant reasoning. Signed or redacted
            // thinking from another writer cannot be represented.
            "thinking" => {
                if role != Role::Assistant
                    || block.thinking.is_empty()
                    || !block.thinking_signature.is_empty()
                    || block.redacted.is_some()
                {
                    return Err(PiError::new(
                        PiErrorKind::UnsupportedContent,
                        "Pi message content is not supported by Otto",
                    ));
                }
                blocks.push(Block::reasoning(block.thinking.clone()));
            }
            _ => {
                return Err(PiError::new(
                    PiErrorKind::UnsupportedContent,
                    "Pi message content type is unsupported",
                ));
            }
        }
    }
    Ok(blocks)
}

fn pi_context_text(
    content_text: Option<&str>,
    blocks: &[PiContentBlock],
) -> Result<String, PiError> {
    if let Some(text) = content_text {
        return Ok(text.to_owned());
    }
    let mut text = String::new();
    for block in blocks {
        if block.type_name != "text" || !block.text_signature.is_empty() {
            return Err(PiError::new(
                PiErrorKind::UnsupportedContent,
                "context content must be plain text",
            ));
        }
        text.push_str(&block.text);
    }
    Ok(text)
}

/// Builds the `Role::Context` message every summary and notification uses.
pub fn new_context_message(
    id: &str,
    context_type: &str,
    display: bool,
    text: String,
    created_at: DateTime<Utc>,
    usage: Option<Usage>,
) -> Message {
    Message {
        id: id.to_owned(),
        role: Role::Context,
        blocks: vec![Block::text(text)],
        created_at,
        usage,
        context_type: context_type.to_owned(),
        display,
        ..Message::default()
    }
}

fn custom_context_text(custom_type: &str, text: &str) -> String {
    if custom_type == TASK_NOTIFICATION_CONTEXT_TYPE
        || custom_type == PARENT_MESSAGE_CONTEXT_TYPE
        || custom_type == CAPABILITY_UPDATE_CONTEXT_TYPE
    {
        return text.to_owned();
    }
    format!("[Custom context: {custom_type}]\n{text}")
}

pub(crate) fn optional_pi_usage_to_model(
    usage: Option<&PiUsage>,
) -> Result<Option<Usage>, PiError> {
    let Some(usage) = usage else {
        return Ok(None);
    };
    match pi_usage_to_model(Some(usage))? {
        Some(converted) => Ok(Some(converted)),
        None => Ok(Some(Usage::default())),
    }
}

fn pi_assistant_usage_to_model(wire: &PiMessage) -> Result<Option<Usage>, PiError> {
    if let Some(usage) = pi_usage_to_model(wire.usage.as_ref())? {
        return Ok(Some(usage));
    }
    if let Some(details) = decode_pi_otto_details(wire.details.as_deref())?
        && details.usage_present
    {
        return Ok(Some(Usage::default()));
    }
    Ok(None)
}

/// Adds `usage` into `total`, saturating instead of overflowing.
pub fn add_resolved_usage(mut total: Usage, usage: Option<&Usage>) -> Usage {
    let Some(usage) = usage else {
        return total;
    };
    total.input_tokens = saturating_usage_add(total.input_tokens, usage.input_tokens);
    total.output_tokens = saturating_usage_add(total.output_tokens, usage.output_tokens);
    total.cached_input_tokens =
        saturating_usage_add(total.cached_input_tokens, usage.cached_input_tokens);
    total
}

fn saturating_usage_add(total: i64, delta: i64) -> i64 {
    if delta <= 0 {
        return total;
    }
    total.saturating_add(delta)
}

fn known_pi_entry_type(entry_type: &str) -> bool {
    matches!(
        entry_type,
        "message"
            | "model_change"
            | "thinking_level_change"
            | "compaction"
            | "branch_summary"
            | "custom"
            | "custom_message"
            | "context_edit"
            | "label"
            | "session_info"
    )
}

fn sanitize_warning_type(entry_type: &str) -> String {
    let mut sanitized = String::new();
    for character in entry_type.chars() {
        if sanitized.len() >= MAX_WARNING_TYPE_BYTES {
            break;
        }
        if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
            sanitized.push(character);
        } else {
            sanitized.push('?');
        }
    }
    if sanitized.is_empty() {
        return "unknown".into();
    }
    sanitized
}

/// The capability state recorded in an `otto.capabilities` custom entry.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Capabilities {
    /// Sorted, deduplicated tool names.
    pub tools: Vec<String>,
    /// Digest per tool name of its description and parameters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definitions: Option<BTreeMap<String, String>>,
    /// The Otto package version that wrote the record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// Decodes `otto.capabilities` data, sorting and deduplicating the tool
/// names; `None` when the data has another shape. Fields absent from older
/// records decode as `None` and unknown fields are ignored.
pub fn decode_capabilities(data: &str) -> Option<Capabilities> {
    let mut capabilities = serde_json::from_str::<Capabilities>(data).ok()?;
    capabilities.tools.sort();
    capabilities.tools.dedup();
    Some(capabilities)
}

/// Encodes a [`Capabilities`] value as `otto.capabilities` data.
pub fn encode_capabilities(capabilities: &Capabilities) -> String {
    serde_json::to_string(capabilities).expect("capabilities serialize")
}

/// Decodes an `otto.runtime` custom entry's `data` field.
pub fn decode_runtime_metadata(raw: Option<&RawValue>) -> Result<RuntimeMetadata, PiError> {
    let bytes = raw.map_or(&b""[..], |value| value.get().as_bytes());
    let object = super::codec::decode_object(bytes, "otto.runtime data")?;
    let provider = super::codec::required_string(&object, "provider", "otto.runtime data.provider")
        .unwrap_or_default();
    if provider.trim().is_empty() {
        return Err(PiError::invalid("otto.runtime provider is required"));
    }
    let model = super::codec::required_string(&object, "model", "otto.runtime data.model")
        .unwrap_or_default();
    if model.trim().is_empty() {
        return Err(PiError::invalid("otto.runtime model is required"));
    }
    let profile =
        super::codec::optional_string(&object, "profile", "otto.runtime data.profile", false)?;
    Ok(RuntimeMetadata {
        profile: profile.unwrap_or_default(),
        provider,
        model,
    })
}

// ---------------------------------------------------------------------------
// model -> Pi
// ---------------------------------------------------------------------------

/// Encodes one Otto message as the Pi entry that will be appended, and returns
/// the message as it will be replayed from disk (its id is the entry id).
pub fn model_message_to_pi_entry(
    message: &Message,
    entry_id: &str,
    parent_id: Option<&str>,
    header: &Header,
) -> Result<(PiEntry, Message), PiError> {
    if message.role == Role::Context {
        return model_context_message_to_pi_entry(message, entry_id, parent_id);
    }
    let timestamp = format_persisted_timestamp(message.created_at, "message")?;
    let content = model_blocks_to_pi_content(message.role.clone(), &message.blocks)?;
    let (_, content_blocks) = super::codec::decode_content(&content, "message content", false)?;
    let mut wire = PiMessage {
        role: role_wire_name(message.role.clone()).to_owned(),
        content: Some(content),
        content_blocks,
        timestamp: message.created_at.timestamp_millis(),
        ..PiMessage::default()
    };

    match message.role {
        Role::User => {
            if message.finish_reason.is_some() || message.usage.is_some() {
                return Err(PiError::invalid(
                    "user message contains assistant-only metadata",
                ));
            }
            if !message.input_id.is_empty() {
                if !crate::model::is_input_id(&message.input_id) {
                    return Err(PiError::invalid("invalid input id"));
                }
                wire.details = Some(encode_pi_otto_details(&PiOttoDetails {
                    input_id: message.input_id.clone(),
                    ..PiOttoDetails::default()
                })?);
            }
        }
        Role::Assistant => {
            let stop_reason = model_finish_reason_to_pi(message.finish_reason.clone())?;
            validate_assistant_tool_finish(&message.blocks, message.finish_reason.clone())?;
            let usage = model_usage_to_pi(message.usage.as_ref())?;
            if message.usage == Some(Usage::default()) {
                wire.details = Some(encode_pi_otto_details(&PiOttoDetails {
                    usage_present: true,
                    ..PiOttoDetails::default()
                })?);
            }
            wire.api = "openai-completions".into();
            wire.provider = header.provider.clone();
            wire.model = header.model.clone();
            wire.usage = usage;
            wire.stop_reason = stop_reason;
            if message.finish_reason == Some(FinishReason::Unknown) {
                wire.error_message =
                    "assistant response ended with an unknown finish reason".into();
            }
        }
        Role::Tool => {
            if message.finish_reason.is_some()
                || message.usage.is_some()
                || message.blocks.len() != 1
            {
                return Err(PiError::invalid("tool result message is malformed"));
            }
            let block = &message.blocks[0];
            if block.block_type != BlockType::ToolResult
                || block.tool_call_id.trim().is_empty()
                || block.tool_name.trim().is_empty()
            {
                return Err(PiError::invalid("tool result id and name are required"));
            }
            wire.role = "toolResult".into();
            wire.tool_call_id = block.tool_call_id.clone();
            wire.tool_name = block.tool_name.clone();
            wire.is_error = Some(block.is_error);
            if let Some(metadata) = block.operation_metadata.as_ref() {
                metadata.validate().map_err(|error| {
                    PiError::invalid(format!("invalid operation metadata: {error}"))
                })?;
                wire.details = Some(encode_pi_otto_details(&PiOttoDetails {
                    operation_id: metadata.operation_id.clone(),
                    disposition: Some(metadata.disposition),
                    effect_certainty: Some(metadata.effect_certainty),
                    stop_reason: metadata.stop_reason,
                    ..PiOttoDetails::default()
                })?);
            }
        }
        _ => return Err(PiError::invalid("unsupported message role")),
    }

    let entry = PiEntry {
        type_name: "message".into(),
        id: entry_id.to_owned(),
        parent_id: parent_id.map(str::to_owned),
        timestamp,
        message: Some(Box::new(wire)),
        ..PiEntry::default()
    };
    let mut persisted = message.clone();
    persisted.id = entry_id.to_owned();
    Ok((entry, persisted))
}

/// Encodes a `Role::Context` message as a Pi v3 `custom_message` entry.
/// `compaction` and `branch_summary` are reserved: those context messages are
/// anchored to a checkpoint and must not reach this path.
fn model_context_message_to_pi_entry(
    message: &Message,
    entry_id: &str,
    parent_id: Option<&str>,
) -> Result<(PiEntry, Message), PiError> {
    let timestamp = format_persisted_timestamp(message.created_at, "context message")?;
    if message.context_type.is_empty() {
        return Err(PiError::invalid("context message type is required"));
    }
    if message.context_type == COMPACTION_CONTEXT_TYPE
        || message.context_type == BRANCH_CONTEXT_TYPE
    {
        return Err(PiError::invalid(format!(
            "context type {:?} is reserved",
            message.context_type
        )));
    }
    if message.finish_reason.is_some() {
        return Err(PiError::invalid(
            "context message contains assistant-only metadata",
        ));
    }
    if message.blocks.is_empty() {
        return Err(PiError::invalid("context message content is required"));
    }
    let mut content = String::new();
    for block in &message.blocks {
        if block.block_type != BlockType::Text
            || !block.tool_call_id.is_empty()
            || !block.tool_name.is_empty()
            || block.arguments.is_some()
            || block.is_error
        {
            return Err(PiError::invalid(
                "context message must contain only text blocks",
            ));
        }
        content.push_str(&block.text);
    }
    let content_json = to_raw_value(&content)?;

    let mut custom = PiCustomMessage {
        custom_type: message.context_type.clone(),
        content: Some(content_json),
        display: message.display,
        content_text: Some(content.clone()),
        ..PiCustomMessage::default()
    };
    if !message.input_id.is_empty() && !crate::model::is_input_id(&message.input_id) {
        return Err(PiError::invalid("invalid input id"));
    }
    if message.context_metadata.is_some() || !message.input_id.is_empty() || message.usage.is_some()
    {
        custom.details = Some(encode_pi_otto_details(&PiOttoDetails {
            task_id: message
                .context_metadata
                .as_ref()
                .map(|metadata| metadata.task_id.clone())
                .unwrap_or_default(),
            input_id: message.input_id.clone(),
            // `piCustomMessage` has no usage field in the Pi v3 format, so a
            // notification's token cost (such as a finished sub-agent task's)
            // travels in the Otto extension instead of being dropped.
            usage: message.usage,
            ..PiOttoDetails::default()
        })?);
    }
    let entry = PiEntry {
        type_name: "custom_message".into(),
        id: entry_id.to_owned(),
        parent_id: parent_id.map(str::to_owned),
        timestamp,
        custom_message: Some(Box::new(custom)),
        ..PiEntry::default()
    };
    let persisted = Message {
        id: entry_id.to_owned(),
        role: Role::Context,
        blocks: vec![Block::text(content)],
        created_at: message.created_at,
        context_type: message.context_type.clone(),
        display: message.display,
        context_metadata: message.context_metadata.clone(),
        input_id: message.input_id.clone(),
        usage: message.usage,
        ..Message::default()
    };
    Ok((entry, persisted))
}

fn role_wire_name(role: Role) -> &'static str {
    match role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
        Role::Context => "context",
        Role::Other(_) => "",
    }
}

fn model_blocks_to_pi_content(role: Role, blocks: &[Block]) -> Result<Box<RawValue>, PiError> {
    if blocks.is_empty() && role != Role::Assistant {
        return Err(PiError::invalid("message content is required"));
    }
    let mut content = Vec::with_capacity(blocks.len());
    for block in blocks {
        match block.block_type {
            BlockType::Text => {
                if role != Role::User && role != Role::Assistant {
                    return Err(PiError::invalid(
                        "text content is incompatible with message role",
                    ));
                }
                if !block.tool_call_id.is_empty()
                    || !block.tool_name.is_empty()
                    || block.arguments.is_some()
                    || block.is_error
                {
                    return Err(PiError::invalid("text block contains incompatible fields"));
                }
                content.push(PiContentBlock {
                    type_name: "text".into(),
                    text: block.text.clone(),
                    ..PiContentBlock::default()
                });
            }
            BlockType::Image => {
                if role != Role::User {
                    return Err(PiError::invalid(
                        "image content is incompatible with message role",
                    ));
                }
                content.push(PiContentBlock {
                    type_name: "image".into(),
                    data: block.data.clone(),
                    mime_type: block.mime_type.clone(),
                    ..PiContentBlock::default()
                });
            }
            BlockType::ToolCall => {
                if role != Role::Assistant
                    || block.tool_call_id.trim().is_empty()
                    || block.tool_name.trim().is_empty()
                {
                    return Err(PiError::invalid(
                        "assistant tool-call id and name are required",
                    ));
                }
                if !valid_tool_arguments(block.arguments.as_deref()) {
                    return Err(PiError::invalid(
                        "tool-call arguments must be a JSON object",
                    ));
                }
                if !block.text.is_empty() || block.is_error {
                    return Err(PiError::invalid(
                        "tool-call block contains incompatible fields",
                    ));
                }
                content.push(PiContentBlock {
                    type_name: "toolCall".into(),
                    id: block.tool_call_id.clone(),
                    name: block.tool_name.clone(),
                    arguments: block.arguments.clone(),
                    ..PiContentBlock::default()
                });
            }
            BlockType::ToolResult => {
                if role != Role::Tool || blocks.len() != 1 {
                    return Err(PiError::invalid(
                        "tool-result block is incompatible with message role",
                    ));
                }
                content.push(PiContentBlock {
                    type_name: "text".into(),
                    text: block.text.clone(),
                    ..PiContentBlock::default()
                });
            }
            BlockType::Reasoning => {
                if role != Role::Assistant {
                    return Err(PiError::invalid(
                        "reasoning content is incompatible with message role",
                    ));
                }
                content.push(PiContentBlock {
                    type_name: "thinking".into(),
                    thinking: block.text.clone(),
                    ..PiContentBlock::default()
                });
            }
            BlockType::Other(_) => {
                return Err(PiError::invalid("unsupported message block type"));
            }
        }
    }
    marshal_pi_content(&content)
}

/// Serializes content blocks for storage. `text` is always written, even when
/// empty, so the strict decoder accepts the record; `skip_serializing_if`
/// alone would drop it and make empty tool output fail the append-time
/// self-check.
fn marshal_pi_content(content: &[PiContentBlock]) -> Result<Box<RawValue>, PiError> {
    let mut wire = String::from("[");
    for (index, block) in content.iter().enumerate() {
        if index > 0 {
            wire.push(',');
        }
        if block.type_name == "text" {
            let text = serde_json::to_string(&block.text)
                .map_err(|error| PiError::other(format!("encode Pi message content: {error}")))?;
            let _ = write!(wire, r#"{{"type":"text","text":{text}}}"#);
            continue;
        }
        let encoded = serde_json::to_string(block)
            .map_err(|error| PiError::other(format!("encode Pi message content: {error}")))?;
        wire.push_str(&encoded);
    }
    wire.push(']');
    RawValue::from_string(wire)
        .map_err(|error| PiError::other(format!("encode Pi message content: {error}")))
}

fn model_finish_reason_to_pi(reason: Option<FinishReason>) -> Result<String, PiError> {
    match reason {
        Some(FinishReason::Stop) => Ok("stop".into()),
        Some(FinishReason::ToolCalls) => Ok("toolUse".into()),
        Some(FinishReason::Length) => Ok("length".into()),
        Some(FinishReason::Unknown) => Ok("error".into()),
        _ => Err(PiError::invalid("unsupported assistant finish reason")),
    }
}

/// Converts Otto usage into the Pi wire shape. `None` becomes an all-zero usage
/// object.
pub fn model_usage_to_pi(usage: Option<&Usage>) -> Result<Option<PiUsage>, PiError> {
    let Some(usage) = usage else {
        return Ok(Some(PiUsage::default()));
    };
    usage
        .validate()
        .map_err(|error| PiError::invalid(error.0))?;
    if usage.output_tokens > i64::MAX - usage.input_tokens {
        return Err(PiError::invalid("usage token total overflows"));
    }
    Ok(Some(PiUsage {
        input: usage.input_tokens - usage.cached_input_tokens,
        output: usage.output_tokens,
        cache_read: usage.cached_input_tokens,
        cache_write: 0,
        total_tokens: usage.input_tokens + usage.output_tokens,
        ..PiUsage::default()
    }))
}

fn pi_stop_reason_to_model(reason: &str) -> Result<FinishReason, PiError> {
    match reason {
        "stop" => Ok(FinishReason::Stop),
        "toolUse" => Ok(FinishReason::ToolCalls),
        "length" => Ok(FinishReason::Length),
        "error" | "aborted" => Ok(FinishReason::Unknown),
        "pending" => Err(PiError::invalid(
            "pending assistant message cannot be persisted",
        )),
        _ => Err(PiError::invalid("unsupported Pi assistant stop reason")),
    }
}

/// Normalizes legacy usage: prompt input is `input + cacheRead + cacheWrite`,
/// and an all-zero usage is reported as absent so the explicit-zero marker in
/// `details.otto.usagePresent` can tell the two apart.
pub fn pi_usage_to_model(usage: Option<&PiUsage>) -> Result<Option<Usage>, PiError> {
    let Some(usage) = usage else {
        return Err(PiError::invalid("assistant usage is required"));
    };
    if usage.input < 0
        || usage.output < 0
        || usage.cache_read < 0
        || usage.cache_write < 0
        || usage.total_tokens < 0
    {
        return Err(PiError::invalid(
            "assistant usage is outside the supported range",
        ));
    }
    let prompt_input = usage
        .input
        .checked_add(usage.cache_read)
        .and_then(|sum| sum.checked_add(usage.cache_write))
        .ok_or_else(|| PiError::invalid("assistant usage is outside the supported range"))?;
    if prompt_input == 0 && usage.output == 0 && usage.cache_read == 0 && usage.cache_write == 0 {
        return Ok(None);
    }
    Ok(Some(Usage {
        input_tokens: prompt_input,
        output_tokens: usage.output,
        cached_input_tokens: usage.cache_read,
    }))
}

/// Rejects an assistant message whose tool calls and finish reason disagree.
pub fn validate_assistant_tool_finish(
    blocks: &[Block],
    reason: Option<FinishReason>,
) -> Result<(), PiError> {
    let has_tool_call = blocks
        .iter()
        .any(|block| block.block_type == BlockType::ToolCall);
    if has_tool_call && reason != Some(FinishReason::ToolCalls) {
        return Err(PiError::invalid(
            "assistant tool calls require tool_calls finish reason",
        ));
    }
    if !has_tool_call && reason == Some(FinishReason::ToolCalls) {
        return Err(PiError::invalid(
            "tool_calls finish reason requires a tool call",
        ));
    }
    Ok(())
}

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
fn repair_interrupted_tool_calls(
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

// ---------------------------------------------------------------------------
// timestamps
// ---------------------------------------------------------------------------

/// Parses the RFC 3339 timestamps stored sessions use, with nanosecond
/// precision.
pub fn parse_rfc3339(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value).ok().map(Into::into)
}

/// Formats as RFC 3339 with nanosecond precision: fractional seconds are
/// written only when nonzero, with trailing zeros removed.
pub fn format_rfc3339_nano(timestamp: DateTime<Utc>) -> String {
    let formatted = timestamp.to_rfc3339_opts(SecondsFormat::Nanos, true);
    let Some((head, rest)) = formatted.split_once('.') else {
        return formatted;
    };
    let (fraction, suffix) = rest.split_at(rest.len() - 1);
    let trimmed = fraction.trim_end_matches('0');
    if trimmed.is_empty() {
        format!("{head}{suffix}")
    } else {
        format!("{head}.{trimmed}{suffix}")
    }
}

/// Formats a timestamp for persistence, rejecting the zero value and anything
/// that does not survive a format/parse round trip.
pub fn format_persisted_timestamp(
    timestamp: DateTime<Utc>,
    subject: &str,
) -> Result<String, PiError> {
    if timestamp == zero_time() {
        return Err(PiError::invalid(format!("{subject} timestamp is required")));
    }
    let formatted = format_rfc3339_nano(timestamp);
    match parse_rfc3339(&formatted) {
        Some(round_tripped)
            if round_tripped == timestamp && format_rfc3339_nano(round_tripped) == formatted =>
        {
            Ok(formatted)
        }
        _ => Err(PiError::invalid(format!(
            "{subject} timestamp is outside the RFC3339Nano range"
        ))),
    }
}

// ---------------------------------------------------------------------------
// snapshot and preview
// ---------------------------------------------------------------------------

/// Builds the display snapshot from a session's resolved state.
pub fn snapshot_from_state(
    aggregate_usage: Usage,
    aggregate_usage_present: bool,
    messages: &[Message],
    latest_compaction: Option<&super::types::CompactionMetadata>,
) -> Snapshot {
    let (context_input_tokens, present, pending) =
        latest_context_input_tokens(messages, latest_compaction);
    Snapshot {
        aggregate_usage,
        aggregate_usage_present,
        context_input_tokens,
        context_input_tokens_present: present,
        context_input_tokens_pending: pending,
    }
}

fn latest_context_input_tokens(
    messages: &[Message],
    latest_compaction: Option<&super::types::CompactionMetadata>,
) -> (i64, bool, bool) {
    let mut start = 0usize;
    if let Some(compaction) = latest_compaction {
        if compaction.first_post_checkpoint_message_id.is_empty() {
            return (0, false, true);
        }
        match messages
            .iter()
            .position(|message| message.id == compaction.first_post_checkpoint_message_id)
        {
            Some(index) => start = index,
            None => return (0, false, true),
        }
    }
    for message in messages[start..].iter().rev() {
        if message.role != Role::Assistant {
            continue;
        }
        return match message.usage {
            Some(usage) if usage.input_tokens > 0 => (usage.input_tokens, true, false),
            _ => (0, false, false),
        };
    }
    if latest_compaction.is_some() {
        return (0, false, true);
    }
    (0, false, false)
}

const MAX_SESSION_PREVIEW_RUNES: usize = 120;

/// The text of the most recent user message, or empty when there is none.
pub fn last_user_text(messages: &[Message]) -> String {
    messages
        .iter()
        .rev()
        .find(|message| message.role == Role::User)
        .map(Message::text)
        .unwrap_or_default()
}

/// Collapses whitespace, escapes control characters and truncates to 120
/// display units, appending "..." when anything was dropped.
pub fn preview_text(text: &str) -> String {
    let mut builder = String::with_capacity(MAX_SESSION_PREVIEW_RUNES * 6 + 3);
    let mut used = 0usize;
    let mut wrote_content = false;
    let mut pending_space = false;
    let mut truncated = false;

    for character in text.chars() {
        if character.is_whitespace() {
            pending_space = wrote_content;
            continue;
        }
        let width = preview_rune_width(character);
        let needed = width + usize::from(pending_space);
        if used + needed > MAX_SESSION_PREVIEW_RUNES {
            truncated = true;
            break;
        }
        if pending_space {
            builder.push(' ');
            used += 1;
            pending_space = false;
        }
        write_preview_rune(&mut builder, character);
        used += width;
        wrote_content = true;
    }

    if !wrote_content {
        return String::new();
    }
    if truncated {
        builder.push_str("...");
    }
    builder
}

fn preview_rune_width(character: char) -> usize {
    let value = character as u32;
    if is_go_control(character) {
        if value < 0x100 {
            return 4;
        }
        return 2 + hex_width(value).max(4);
    }
    1
}

fn write_preview_rune(builder: &mut String, character: char) {
    let value = character as u32;
    if is_go_control(character) {
        if value < 0x100 {
            let _ = write!(builder, "\\x{value:02x}");
        } else {
            let width = hex_width(value).max(4);
            let _ = write!(builder, "\\u{value:0width$x}");
        }
        return;
    }
    builder.push(character);
}

/// The C0 and C1 control ranges only.
fn is_go_control(character: char) -> bool {
    let value = character as u32;
    value < 0x20 || (0x7f..=0x9f).contains(&value)
}

fn hex_width(mut value: u32) -> usize {
    let mut width = 1;
    while value >= 16 {
        value /= 16;
        width += 1;
    }
    width
}

fn to_raw_value<T: serde::Serialize>(value: &T) -> Result<Box<RawValue>, PiError> {
    let encoded = serde_json::to_string(value)
        .map_err(|error| PiError::other(format!("encode context message content: {error}")))?;
    RawValue::from_string(encoded)
        .map_err(|error| PiError::other(format!("encode context message content: {error}")))
}

#[cfg(test)]
mod tests;
