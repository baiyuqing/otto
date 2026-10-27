//! Full active-branch message history, including messages a compaction replaced.

use super::*;

/// One message of the active branch's full history with the session entry it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryMessage {
    /// Id of the `message` entry that holds this message.
    pub entry_id: String,
    /// RFC 3339 timestamp of that entry.
    pub timestamp: String,
    /// The converted message.
    pub message: Message,
}

/// Returns every `message` entry on the active branch ending at `leaf_id`, oldest first.
///
/// Unlike [`build_context`], messages before the newest compaction checkpoint are kept.
/// Compaction, branch-summary, context-edit, custom and other non-message entries
/// contribute nothing, and context edits are not applied: the messages are as first written.
pub fn active_branch_history(
    entries: &[PiEntry],
    leaf_id: &str,
) -> Result<Vec<HistoryMessage>, PiError> {
    let (index, _) = index_context_entries(entries)?;
    let path = active_context_path(entries, leaf_id, &index)?;
    let mut history = Vec::new();
    for entry in path.iter().filter(|entry| entry.type_name == "message") {
        for message in pi_entry_to_context_messages(entry)? {
            history.push(HistoryMessage {
                entry_id: entry.id.clone(),
                timestamp: entry.timestamp.clone(),
                message,
            });
        }
    }
    Ok(history)
}
