//! Reads the session under evaluation: the pre-compaction history of its active branch and
//! the context a normal turn would send now.

use std::path::{Path, PathBuf};

use otto_core::model::Message;
use otto_core::session::{active_branch_history, build_context, decode_pi_file};
use otto_core::view_tree::{LogMessage, log_from_history};

/// A session file read without taking its writer lease.
#[derive(Debug)]
pub struct SessionInput {
    pub session_id: String,
    pub path: PathBuf,
    pub log: Vec<LogMessage>,
    /// `ResolvedContext::model_messages` at the last entry: what a turn would send now.
    pub baseline_messages: Vec<Message>,
}

/// Resolves `--session`: an existing file path, or a session id under the workspace's session
/// directory.
pub fn resolve_session_path(
    reference: &str,
    session_root: &Path,
    workspace: &str,
) -> Result<PathBuf, String> {
    let as_path = Path::new(reference);
    if as_path.is_file() {
        return Ok(as_path.to_path_buf());
    }
    if !otto::session::is_session_id(reference) {
        return Err(format!(
            "session {reference:?} is neither an existing file nor a 32-character session id"
        ));
    }
    let directory = otto::session::session_directory(session_root, workspace)
        .map_err(|error| error.to_string())?;
    let path = directory.join(format!("{reference}.jsonl"));
    if !path.is_file() {
        return Err(format!(
            "session {reference} has no JSONL file at {}; only the JSONL session backend is supported, \
             so a SQLite-routed session cannot be evaluated",
            path.display()
        ));
    }
    Ok(path)
}

/// Reads and decodes the session file at `path`.
pub fn load_session(path: &Path) -> Result<SessionInput, String> {
    let bytes = std::fs::read(path)
        .map_err(|error| format!("read session file {}: {error}", path.display()))?;
    session_from_bytes(&bytes, path)
}

/// Decodes the bytes of a session file; `path` is only recorded.
pub fn session_from_bytes(bytes: &[u8], path: &Path) -> Result<SessionInput, String> {
    let file = decode_pi_file(bytes)
        .map_err(|error| format!("decode session file {}: {error}", path.display()))?;
    let Some(leaf) = file.entries.last() else {
        return Err(format!("session file {} has no entries", path.display()));
    };
    let leaf_id = leaf.id.clone();
    let history = active_branch_history(&file.entries, &leaf_id)
        .map_err(|error| format!("read session history: {error}"))?;
    let log = log_from_history(&history);
    if log.is_empty() {
        return Err(format!(
            "session file {} has no messages to evaluate",
            path.display()
        ));
    }
    let (context, _warnings) = build_context(&file.entries, &leaf_id)
        .map_err(|error| format!("build session context: {error}"))?;
    Ok(SessionInput {
        session_id: file.header.id.clone(),
        path: path.to_path_buf(),
        log,
        baseline_messages: context.model_messages,
    })
}
