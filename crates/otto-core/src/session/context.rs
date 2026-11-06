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
use super::memory_recall::{MEMORY_RECALL_CUSTOM_TYPE, TurnMemoryReferences, decode_memory_recall};
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
use super::session_types::{Header, RuntimeMetadata, Snapshot, Warning};
use super::turn::{InterruptedTurn, TURN_CUSTOM_TYPE, apply_turn_record, decode_turn_record};
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
    /// Every valid `otto.memory_recall` record on the active path, oldest
    /// first, anchored to the nearest preceding user message entry; see
    /// [`crate::session::memory_recall`].
    pub memory_references: Vec<TurnMemoryReferences>,
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
            memory_references: Vec::new(),
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
    let mut last_user_message_id: Option<String> = None;
    for entry in &path {
        match entry.type_name.as_str() {
            "message" => {
                let message = entry
                    .message
                    .as_ref()
                    .ok_or_else(|| PiError::invalid("message payload is required"))?;
                if message.role == "user" {
                    last_user_message_id = Some(entry.id.clone());
                }
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
                } else if custom.custom_type == MEMORY_RECALL_CUSTOM_TYPE {
                    let raw = custom
                        .data
                        .as_deref()
                        .map(RawValue::get)
                        .unwrap_or_default();
                    match (decode_memory_recall(raw), last_user_message_id.as_ref()) {
                        (Some(record), Some(user_message_id)) => {
                            resolved.memory_references.push(TurnMemoryReferences {
                                user_message_id: user_message_id.clone(),
                                records: record.records,
                            });
                        }
                        (None, _) => collector.add(format!(
                            "ignored malformed {MEMORY_RECALL_CUSTOM_TYPE} entry {}",
                            entry.id
                        )),
                        (Some(_), None) => collector.add(format!(
                            "ignored {MEMORY_RECALL_CUSTOM_TYPE} entry {} before any user message",
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

fn to_raw_value<T: serde::Serialize>(value: &T) -> Result<Box<RawValue>, PiError> {
    let encoded = serde_json::to_string(value)
        .map_err(|error| PiError::other(format!("encode context message content: {error}")))?;
    RawValue::from_string(encoded)
        .map_err(|error| PiError::other(format!("encode context message content: {error}")))
}

mod entries;
mod history_messages;
mod messages;
mod snapshot_preview;
mod tool_results;

pub use entries::*;
pub use history_messages::*;
pub use messages::*;
pub use snapshot_preview::*;
pub use tool_results::*;

#[cfg(test)]
mod tests;
