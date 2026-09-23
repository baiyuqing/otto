//! Listing and inspecting session files.
//!
//! Ownership: every function opens and closes its own descriptors. Concurrency:
//! stateless, safe to call from any thread. Errors: a candidate that cannot be
//! read is counted in [`ListResult::skipped`] and never surfaced; only a
//! failure of the listing itself is returned.

use std::fs::{File, Metadata};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use kite_core::session::context::{last_user_text, preview_text};
use kite_core::session::{ListResult, PiError, SessionInfo, Warning, build_context};

use super::fsops::{self, Dir};
use super::store::{decode_pi_file_read_only, reject_oversized_session_file, validate_pi_header};

/// The most sessions one [`list`] call may return.
pub const MAX_LIST_SESSIONS: usize = 20;

/// The directory a workspace's sessions live in: the root plus the workspace
/// key. The directory need not exist.
pub fn session_directory(root: &Path, workspace: &str) -> Result<PathBuf, PiError> {
    Ok(root.join(fsops::workspace_key(Path::new(workspace))?))
}

/// Reads one session file's listing row without opening a store.
pub fn inspect(path: &Path) -> Result<(SessionInfo, Vec<Warning>), PiError> {
    let (mut file, metadata) = open_session_file_read_only_no_follow(path)?;
    inspect_opened_session(&path.to_string_lossy(), &mut file, &metadata)
}

/// The workspace's sessions, newest first, capped at `limit`.
///
/// `current_path` marks the row for the session already open, by file
/// identity when it can be stat'ed and by canonical path otherwise. A
/// candidate that cannot be opened, changed between the two opens, belongs to
/// a different workspace, or shares a header id with a row already listed
/// (kept from the newest-modified file) is counted as skipped.
pub fn list(
    root: &Path,
    workspace: &str,
    current_path: &str,
    limit: usize,
) -> Result<ListResult, PiError> {
    if !(1..=MAX_LIST_SESSIONS).contains(&limit) {
        return Err(PiError::other(format!(
            "list limit must be between 1 and {MAX_LIST_SESSIONS}"
        )));
    }
    let workspace_canonical = fsops::canonical_workspace(Path::new(workspace))?;
    let root_dir = open_session_root_no_follow(root)?;
    let Some((session_dir, directory)) =
        open_workspace_session_directory_no_follow(&root_dir, root, &workspace_canonical)?
    else {
        return Ok(ListResult::default());
    };

    let mut result = ListResult::default();
    let mut candidates: Vec<(String, String, Metadata)> = Vec::new();
    for name in read_directory_names(&directory)? {
        if !name.ends_with(".jsonl") {
            continue;
        }
        match open_session_file_read_only_no_follow_at(&session_dir, &directory, &name) {
            Ok((_, metadata, path)) => candidates.push((name, path, metadata)),
            Err(_) => result.skipped = increment_skipped(result.skipped),
        }
    }

    candidates.sort_by(|left, right| {
        let (left_time, right_time) = (modified_time(&left.2), modified_time(&right.2));
        right_time
            .cmp(&left_time)
            .then_with(|| right.1.cmp(&left.1))
    });

    let current_canonical = (!current_path.is_empty())
        .then(|| fsops::canonical_workspace(Path::new(current_path)))
        .transpose()?;
    let current_metadata = std::fs::metadata(current_path).ok();

    let mut sessions = Vec::new();
    let mut seen_ids = std::collections::HashSet::new();
    for (name, _, expected) in &candidates {
        if sessions.len() == limit {
            break;
        }
        let Ok((mut file, metadata, path)) =
            open_session_file_read_only_no_follow_at(&session_dir, &directory, name)
        else {
            result.skipped = increment_skipped(result.skipped);
            continue;
        };
        if !same_list_candidate_metadata(expected, &metadata) {
            result.skipped = increment_skipped(result.skipped);
            continue;
        }
        let Ok((mut info, _)) = inspect_opened_session(&path, &mut file, &metadata) else {
            result.skipped = increment_skipped(result.skipped);
            continue;
        };
        match fsops::canonical_workspace(Path::new(&info.cwd)) {
            Ok(candidate) if candidate == workspace_canonical => {}
            _ => {
                result.skipped = increment_skipped(result.skipped);
                continue;
            }
        }
        if !seen_ids.insert(info.id.clone()) {
            result.skipped = increment_skipped(result.skipped);
            continue;
        }
        if let Some(current_canonical) = current_canonical.as_deref() {
            info.current = match current_metadata.as_ref() {
                Some(current) => fsops::file_identity(&metadata) == fsops::file_identity(current),
                None => match fsops::canonical_workspace(Path::new(&path)) {
                    Ok(candidate) => candidate == current_canonical,
                    Err(_) => {
                        result.skipped = increment_skipped(result.skipped);
                        continue;
                    }
                },
            };
        }
        sessions.push(info);
    }
    result.sessions = sessions;
    Ok(result)
}

/// The names in `directory`, unsorted.
///
/// The directory is read by path rather than through the no-follow descriptor.
/// Names are only a work list: every file is still opened with `openat` on
/// that descriptor, so a directory swapped between the two steps yields opens
/// that fail and rows that are skipped.
fn read_directory_names(directory: &str) -> Result<Vec<String>, PiError> {
    let entries = std::fs::read_dir(directory)
        .map_err(|error| PiError::other(format!("read session directory: {error}")))?;
    let mut names = Vec::new();
    for entry in entries {
        let entry =
            entry.map_err(|error| PiError::other(format!("read session directory: {error}")))?;
        names.push(entry.file_name().to_string_lossy().into_owned());
    }
    Ok(names)
}

/// Opens the session root, refusing a symlink or a non-directory.
pub(crate) fn open_session_root_no_follow(root: &Path) -> Result<Dir, PiError> {
    fsops::open_dir_no_follow(root).map_err(|error| {
        if fsops::is_eloop(&error) {
            PiError::invalid("session root is a symlink")
        } else if fsops::is_enotdir(&error) {
            PiError::invalid("session root is not a directory")
        } else {
            PiError::other(format!("open session root: {error}"))
        }
    })
}

/// Opens the workspace's session directory under `root_dir`. `Ok(None)` means
/// the workspace has no sessions yet.
pub(crate) fn open_workspace_session_directory_no_follow(
    root_dir: &Dir,
    root: &Path,
    workspace_canonical: &str,
) -> Result<Option<(Dir, String)>, PiError> {
    let key = fsops::workspace_key_of_canonical(workspace_canonical);
    let path = root.join(&key).to_string_lossy().into_owned();
    match fsops::open_dir_at_no_follow(root_dir, &key) {
        Ok(dir) => Ok(Some((dir, path))),
        Err(error) if fsops::is_enoent(&error) => Ok(None),
        Err(error) if fsops::is_eloop(&error) => {
            Err(PiError::invalid("session directory is a symlink"))
        }
        Err(error) if fsops::is_enotdir(&error) => {
            Err(PiError::invalid("session directory is not a directory"))
        }
        Err(error) => Err(PiError::other(format!("open session directory: {error}"))),
    }
}

/// Opens one session file read-only, refusing a symlink or a non-regular file.
pub(crate) fn open_session_file_read_only_no_follow(
    path: &Path,
) -> Result<(File, Metadata), PiError> {
    let file = fsops::open_no_follow(path, libc::O_RDONLY).map_err(|error| {
        if fsops::is_eloop(&error) {
            PiError::invalid("session file is a symlink")
        } else {
            PiError::other(format!("open session file: {error}"))
        }
    })?;
    let metadata = regular_file_metadata(&file)?;
    Ok((file, metadata))
}

pub(crate) fn open_session_file_read_only_no_follow_at(
    directory: &Dir,
    directory_path: &str,
    name: &str,
) -> Result<(File, Metadata, String), PiError> {
    let path = Path::new(directory_path)
        .join(name)
        .to_string_lossy()
        .into_owned();
    let file = fsops::open_at_no_follow(directory, name, libc::O_RDONLY).map_err(|error| {
        if fsops::is_eloop(&error) {
            PiError::invalid("session file is a symlink")
        } else {
            PiError::other(format!("open session file: {error}"))
        }
    })?;
    let metadata = regular_file_metadata(&file)?;
    Ok((file, metadata, path))
}

fn regular_file_metadata(file: &File) -> Result<Metadata, PiError> {
    let metadata = file
        .metadata()
        .map_err(|error| PiError::other(format!("stat session file: {error}")))?;
    if !metadata.is_file() {
        return Err(PiError::invalid("session file is not a regular file"));
    }
    Ok(metadata)
}

/// Builds the listing row for an already-opened session file.
pub(crate) fn inspect_opened_session(
    path: &str,
    file: &mut File,
    metadata: &Metadata,
) -> Result<(SessionInfo, Vec<Warning>), PiError> {
    reject_oversized_session_file(file)?;
    let decoded = decode_pi_file_read_only(file)?;
    let created_at = validate_pi_header(&decoded.header)?;
    let leaf_id = decoded
        .entries
        .last()
        .map(|entry| entry.id.clone())
        .unwrap_or_default();
    let (resolved, warnings) = build_context(&decoded.entries, &leaf_id)?;
    let preview = preview_text(&last_user_text(&resolved.messages));
    let name = match preview_text(&resolved.session_name) {
        name if name.is_empty() => preview.clone(),
        name => name,
    };
    Ok((
        SessionInfo {
            path: path.to_owned(),
            id: decoded.header.id.clone(),
            cwd: decoded.header.cwd.clone(),
            name,
            created: created_at,
            modified: modified_time(metadata),
            message_count: resolved.messages.len() as i64,
            last_user_text: preview,
            profile: resolved.runtime.profile,
            provider: resolved.runtime.provider,
            model: resolved.runtime.model,
            thinking: pi_level_to_thinking(&resolved.thinking_level)?,
            current: false,
        },
        warnings,
    ))
}

fn pi_level_to_thinking(thinking: &str) -> Result<String, PiError> {
    match thinking {
        "" | "off" => Ok(String::new()),
        "low" | "medium" | "high" | "xhigh" | "max" => Ok(thinking.to_string()),
        _ => Err(PiError::invalid(
            "invalid thinking level: must be one of off, low, medium, high, xhigh, max",
        )),
    }
}

/// The modification time, or the zero time when the platform has none.
pub(crate) fn modified_time(metadata: &Metadata) -> DateTime<Utc> {
    metadata
        .modified()
        .ok()
        .map(DateTime::<Utc>::from)
        .unwrap_or_else(kite_core::model::zero_time)
}

/// Identity, mode, size and mtime all match.
pub(crate) fn same_list_candidate_metadata(expected: &Metadata, current: &Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    fsops::file_identity(expected) == fsops::file_identity(current)
        && expected.mode() == current.mode()
        && expected.len() == current.len()
        && modified_time(expected) == modified_time(current)
}

fn increment_skipped(count: i64) -> i64 {
    count.saturating_add(1)
}
