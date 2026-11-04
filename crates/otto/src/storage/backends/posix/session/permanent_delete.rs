//! Irreversible, descriptor-confined session cleanup. Archive semantics stay separate.
use super::{
    list::{
        inspect_opened_session, open_session_root_no_follow,
        open_workspace_session_directory_no_follow,
    },
    prepared::validate_listed_candidate_path,
};
use crate::{fsops, storage::files::JournalFile};
use otto_core::session::PiError;
use std::{fs::Metadata, path::Path};

pub(crate) fn delete_open_file(
    root: &Path,
    workspace: &str,
    path: &Path,
    file: &mut dyn JournalFile,
    metadata: &Metadata,
) -> Result<(), PiError> {
    let (_, workspace, basename, _) = validate_listed_candidate_path(root, workspace, path)?;
    let root_dir = open_session_root_no_follow(root)?;
    let (directory, _) = open_workspace_session_directory_no_follow(&root_dir, root, &workspace)?
        .ok_or_else(|| PiError::invalid("session directory no longer exists"))?;
    let (directory, basename) = if let Some(basename) = basename.strip_prefix("archive/") {
        let archive = fsops::open_dir_at_no_follow(&directory, "archive").map_err(|error| {
            PiError::other(format!("open archive directory for deletion: {error}"))
        })?;
        (archive, basename.to_owned())
    } else {
        (directory, basename)
    };
    let (info, _) = inspect_opened_session(&path.to_string_lossy(), file, metadata)?;
    if !crate::workspace::owns_session(&workspace, &info.cwd)? {
        return Err(PiError::invalid(
            "session workspace does not match expected workspace",
        ));
    }
    let current = fsops::open_at_no_follow(&directory, &basename, libc::O_RDONLY)
        .and_then(|file| file.metadata())
        .map_err(|error| PiError::other(format!("verify session identity: {error}")))?;
    if fsops::file_identity(metadata) != fsops::file_identity(&current) {
        return Err(PiError::invalid(
            "session path identity changed before permanent delete",
        ));
    }
    let stem = basename
        .strip_suffix(".jsonl")
        .ok_or_else(|| PiError::invalid("session must be JSONL"))?;
    let mut names = vec![stem.to_string()];
    for suffix in [
        "reminders.json",
        "inbox.json",
        "inputs.json",
        "followups.json",
        "turns.json",
    ] {
        names.push(format!("{stem}.{suffix}"));
    }
    // Keep the locked journal as the retry anchor until every sidecar is gone.
    for name in &names {
        fsops::remove_tree_at_no_follow(&directory, name)
            .map_err(|error| PiError::other(format!("permanent delete {name}: {error}")))?;
    }
    fsops::unlink_at(&directory, &basename)
        .map_err(|error| PiError::other(format!("permanent delete journal: {error}")))?;
    let lease_name = format!("{stem}.lease");
    fsops::remove_tree_at_no_follow(&directory, &lease_name)
        .map_err(|error| PiError::other(format!("permanent delete lease: {error}")))?;
    names.push(lease_name);
    names.push(basename);
    fsops::sync_dir(&directory)
        .map_err(|error| PiError::other(format!("sync permanent delete: {error}")))?;
    for name in names {
        if fsops::exists_at_no_follow(&directory, &name)
            .map_err(|error| PiError::other(format!("verify permanent delete: {error}")))?
        {
            return Err(PiError::other(format!(
                "permanent delete incomplete: {name} remains"
            )));
        }
    }
    Ok(())
}
