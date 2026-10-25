//! Discovery of linked Git worktrees admitted to one sandbox profile.
//!
//! `git worktree list --porcelain` is the source of candidate directories, not
//! the authority for admitting them. The listing is built from
//! `.git/worktrees/<id>/gitdir` files, and a sandboxed command can write those
//! files, so a forged entry can name any directory such as `$HOME/.ssh`.
//!
//! A candidate is admitted only when its own `.git` entry links back to the
//! common Git directory of the selected workspace:
//!
//! - `.git` is a regular file `gitdir: <path>` and `<path>` canonicalizes to a
//!   directory directly under `<common dir>/worktrees/`; or
//! - `.git` is a directory that canonicalizes to the common Git directory
//!   (the main worktree, when the selected workspace is a linked worktree).
//!
//! A symlinked `.git`, a missing `.git`, a `bare` record and a `prunable`
//! record are never admitted. Any failure to run Git or to resolve the common
//! Git directory admits only the canonical selected workspace.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const GIT_BINARY: &str = "/usr/bin/git";
const MAX_GITFILE_BYTES: u64 = 4096;
const GITFILE_PREFIX: &str = "gitdir: ";

#[cfg(test)]
mod tests;

/// One record of `git worktree list --porcelain` output.
#[derive(Debug, PartialEq, Eq)]
struct WorktreeRecord {
    path: String,
    bare: bool,
    prunable: bool,
}

/// Canonical worktree roots registered with `workspace`'s Git common directory.
///
/// The returned list is sorted, deduplicated and always includes the canonical
/// `workspace`; it is empty only when `workspace` cannot be canonicalized.
/// Candidates come from Git's listing and are admitted only after
/// `linked_to_common_dir` confirms the link from the candidate's own `.git`.
pub fn managed(workspace: &Path) -> Vec<String> {
    let Ok(workspace) = std::fs::canonicalize(workspace) else {
        return Vec::new();
    };
    let mut roots = vec![workspace.to_string_lossy().into_owned()];
    let Some(common_dir) = common_git_dir(&workspace) else {
        return roots;
    };
    let Some(listing) = git_stdout(&workspace, &["worktree", "list", "--porcelain"]) else {
        return roots;
    };
    for record in parse_worktree_list(&listing) {
        if record.bare || record.prunable {
            continue;
        }
        let Ok(candidate) = std::fs::canonicalize(&record.path) else {
            continue;
        };
        if candidate == workspace {
            continue;
        }
        if linked_to_common_dir(&candidate, &common_dir) {
            roots.push(candidate.to_string_lossy().into_owned());
        }
    }
    roots.sort();
    roots.dedup();
    roots
}

/// Runs Git in `directory` without hooks or fsmonitor and returns its stdout.
fn git_stdout(directory: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new(GIT_BINARY)
        .current_dir(directory)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
        ])
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Canonical absolute common Git directory of the repository containing `workspace`.
fn common_git_dir(workspace: &Path) -> Option<PathBuf> {
    let stdout = git_stdout(
        workspace,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    let reported = PathBuf::from(stdout.trim());
    if reported.as_os_str().is_empty() {
        return None;
    }
    let absolute = if reported.is_absolute() {
        reported
    } else {
        workspace.join(reported)
    };
    std::fs::canonicalize(absolute).ok()
}

/// Splits porcelain output into records. Does not touch the filesystem.
fn parse_worktree_list(output: &str) -> Vec<WorktreeRecord> {
    let mut records = Vec::new();
    let mut current: Option<WorktreeRecord> = None;
    for line in output.lines() {
        if line.is_empty() {
            records.extend(current.take());
        } else if let Some(path) = line.strip_prefix("worktree ") {
            records.extend(current.take());
            current = Some(WorktreeRecord {
                path: path.to_string(),
                bare: false,
                prunable: false,
            });
        } else if let Some(record) = current.as_mut() {
            if line == "bare" {
                record.bare = true;
            } else if line == "prunable" || line.starts_with("prunable ") {
                record.prunable = true;
            }
        }
    }
    records.extend(current);
    records
}

/// True when `worktree_root`'s own `.git` entry links to `common_dir`.
///
/// Both arguments must be canonical. `.git` must be a regular file or a
/// directory; a symlink or a missing entry is rejected.
fn linked_to_common_dir(worktree_root: &Path, common_dir: &Path) -> bool {
    let git_entry = worktree_root.join(".git");
    let Ok(metadata) = std::fs::symlink_metadata(&git_entry) else {
        return false;
    };
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        false
    } else if file_type.is_dir() {
        std::fs::canonicalize(&git_entry).is_ok_and(|resolved| resolved == common_dir)
    } else if file_type.is_file() {
        metadata.len() <= MAX_GITFILE_BYTES
            && gitfile_targets_private_dir(&git_entry, worktree_root, common_dir)
    } else {
        false
    }
}

/// True when the `gitdir: <path>` file names a directory under `<common_dir>/worktrees`.
fn gitfile_targets_private_dir(gitfile: &Path, worktree_root: &Path, common_dir: &Path) -> bool {
    let Ok(content) = std::fs::read_to_string(gitfile) else {
        return false;
    };
    let Some(target) = content.strip_prefix(GITFILE_PREFIX) else {
        return false;
    };
    let target = PathBuf::from(target.trim_end());
    let target = if target.is_absolute() {
        target
    } else {
        worktree_root.join(target)
    };
    let Ok(private_dir) = std::fs::canonicalize(target) else {
        return false;
    };
    let Ok(worktrees_dir) = std::fs::canonicalize(common_dir.join("worktrees")) else {
        return false;
    };
    private_dir.is_dir() && private_dir.parent() == Some(worktrees_dir.as_path())
}
