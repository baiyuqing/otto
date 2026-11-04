//! Complete, fail-closed discovery for destructive target resolution.

use std::fs::Metadata;
use std::io::Read;
use std::path::Path;

use otto_core::session::{PiError, build_context, decode_pi_file};

use super::list::{
    open_session_file_read_only_no_follow_at, open_session_root_no_follow,
    open_workspace_session_directory_no_follow, same_list_candidate_metadata,
};
use super::store::{reject_oversized_session_file, validate_pi_header};
use crate::fsops::{self, Dir};
use crate::session::DeleteCandidate;

pub(super) fn list_for_delete(
    root: &Path,
    workspace: &str,
) -> Result<Vec<DeleteCandidate>, PiError> {
    let workspace = fsops::canonical_workspace(Path::new(workspace))?;
    let root_dir = open_session_root_no_follow(root)?;
    let Some((directory, directory_path)) =
        open_workspace_session_directory_no_follow(&root_dir, root, &workspace)?
    else {
        return Ok(Vec::new());
    };
    let mut candidates = Vec::new();
    scan_directory(
        &directory,
        &directory_path,
        &workspace,
        false,
        &mut candidates,
    )?;
    let archive = open_archive(&directory)?;
    if let Some(archive) = &archive {
        scan_directory(
            archive,
            &Path::new(&directory_path).join("archive").to_string_lossy(),
            &workspace,
            true,
            &mut candidates,
        )?;
    }
    // Ensure pinned descriptors still correspond to the paths handed to callers.
    let current_root = open_session_root_no_follow(root)?;
    verify_directory_identity(&root_dir, &current_root)?;
    let (current_directory, _) =
        open_workspace_session_directory_no_follow(&current_root, root, &workspace)?.ok_or_else(
            || PiError::invalid("session directory changed during deletion discovery"),
        )?;
    verify_directory_identity(&directory, &current_directory)?;
    match (archive, open_archive(&current_directory)?) {
        (Some(expected), Some(current)) => verify_directory_identity(&expected, &current)?,
        (None, None) => {}
        _ => {
            return Err(PiError::invalid(
                "archive directory changed during deletion discovery",
            ));
        }
    }
    candidates.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(candidates)
}

fn open_archive(directory: &Dir) -> Result<Option<Dir>, PiError> {
    match fsops::open_dir_at_no_follow(directory, "archive") {
        Ok(archive) => Ok(Some(archive)),
        Err(error) if fsops::is_enoent(&error) => Ok(None),
        Err(error) => Err(PiError::other(format!("open archive directory: {error}"))),
    }
}

fn directory_metadata(directory: &Dir) -> Result<Metadata, PiError> {
    fsops::open_at_no_follow(directory, ".", libc::O_RDONLY | libc::O_DIRECTORY)
        .and_then(|file| file.metadata())
        .map_err(|error| PiError::other(format!("stat session directory: {error}")))
}

fn verify_directory_identity(expected: &Dir, current: &Dir) -> Result<(), PiError> {
    if fsops::file_identity(&directory_metadata(expected)?)
        != fsops::file_identity(&directory_metadata(current)?)
    {
        return Err(PiError::invalid(
            "session directory identity changed during deletion discovery",
        ));
    }
    Ok(())
}

fn directory_names(directory: &Dir) -> Result<Vec<String>, PiError> {
    let mut names = fsops::read_directory_names(directory, usize::MAX)
        .map_err(|error| PiError::other(format!("read session directory for deletion: {error}")))?;
    names.sort();
    Ok(names)
}

fn scan_directory(
    directory: &Dir,
    directory_path: &str,
    workspace: &str,
    archived: bool,
    candidates: &mut Vec<DeleteCandidate>,
) -> Result<(), PiError> {
    let names = directory_names(directory)?;
    let mut inspected = Vec::new();
    for name in names.iter().filter(|name| name.ends_with(".jsonl")) {
        let (mut file, metadata, path) =
            open_session_file_read_only_no_follow_at(directory, directory_path, name)?;
        reject_oversized_session_file(&file)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|error| PiError::other(format!("read session for deletion: {error}")))?;
        // Unlike display listing, never discard an incomplete last record.
        let decoded = decode_pi_file(&bytes)?;
        validate_pi_header(&decoded.header)?;
        let leaf = decoded
            .entries
            .last()
            .map(|entry| entry.id.as_str())
            .unwrap_or("");
        let (resolved, warnings) = build_context(&decoded.entries, leaf)?;
        if !warnings.is_empty() {
            return Err(PiError::invalid(
                "session metadata has warnings during deletion discovery",
            ));
        }
        if !crate::workspace::owns_session(workspace, &decoded.header.cwd)? {
            return Err(PiError::invalid(
                "session workspace does not match deletion workspace",
            ));
        }
        candidates.push(DeleteCandidate {
            id: decoded.header.id,
            name: resolved.session_name,
            path,
            archived,
        });
        inspected.push((name, metadata));
    }
    if names != directory_names(directory)? {
        return Err(PiError::invalid(
            "session directory changed during deletion discovery",
        ));
    }
    for (name, expected) in inspected {
        let (_, current, _) =
            open_session_file_read_only_no_follow_at(directory, directory_path, name)?;
        if !same_list_candidate_metadata(&expected, &current) {
            return Err(PiError::invalid(
                "session identity or metadata changed during deletion discovery",
            ));
        }
    }
    Ok(())
}
