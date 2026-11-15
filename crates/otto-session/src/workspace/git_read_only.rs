//! The write refusal for `.otto` identity files and git metadata.
//!
//! [`refuse_git_read_only`] runs inside the `write` and `edit` tools, outside
//! the bash sandbox, so it applies with or without `--sandbox off`. Git runs
//! hooks and programs named in configuration files outside the sandbox, so a
//! file that git reads for those settings must not be writable by a model.
//!
//! Refused, after case folding and removal of `.` components:
//!
//! - the root paths in `GIT_READ_ONLY` and
//!   `crate::workspace::READ_ONLY_PATHS`;
//! - at any depth, a path whose last component is `.git`, which is the git
//!   directory or the gitfile of a nested repository or submodule checkout;
//! - at any depth, a path below a `.git` component whose last component is in
//!   `GIT_DIR_READ_ONLY_FILES`, or that has a `GIT_DIR_HOOKS` component.
//!   This covers `.git/modules/<name>/config`, `.git/modules/<name>/hooks/*`,
//!   `.git/worktrees/<id>/commondir`, `.git/worktrees/<id>/gitdir` and
//!   `<dir>/.git/config`.
//!
//! The Seatbelt profile enforces a narrower list for bash: the root
//! `GIT_READ_ONLY` entries, `config`, `config.worktree`, `commondir` and
//! `hooks` below `.git/modules`, and `config.worktree` below `.git/worktrees`.
//! The file tools refuse more because they never need those paths.
//!
//! The function is pure: it performs no filesystem access and keeps no state.

use std::io;
use std::path::Path;

/// File names that are read-only anywhere below a `.git` directory.
///
/// - `config` and `config.worktree` hold program settings such as
///   `core.fsmonitor` and `core.sshCommand`, which git executes.
/// - `commondir` and `gitdir` redirect a linked worktree to another git
///   directory, which can then hold hooks and configuration.
pub(crate) const GIT_DIR_READ_ONLY_FILES: [&str; 4] =
    ["config", "config.worktree", "commondir", "gitdir"];

/// Directory name below a `.git` directory whose files git executes as hooks.
pub(crate) const GIT_DIR_HOOKS: &str = "hooks";

const GIT_DIR_NAME: &str = ".git";

/// Refuses a write to the root-relative path made of `components` when it is
/// protected, with a `PermissionDenied` error naming `path`. APFS is
/// case-insensitive by default and also treats the Kelvin sign (U+212A) as `k`
/// and long s (U+017F) as `s`, so each component is folded with
/// `to_uppercase().to_lowercase()`, which maps both.
pub fn refuse_git_read_only<'a>(
    components: impl IntoIterator<Item = &'a std::ffi::OsStr>,
    path: &Path,
) -> io::Result<()> {
    let parts: Vec<String> = components
        .into_iter()
        .filter(|part| *part != ".")
        .map(|part| part.to_string_lossy().to_uppercase().to_lowercase())
        .collect();
    if crate::workspace::READ_ONLY_PATHS
        .iter()
        .any(|(name, _)| parts.iter().map(String::as_str).eq(name.split('/')))
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} is read-only: workspace identity is managed by Otto",
                path.display()
            ),
        ));
    }
    if is_protected_git_path(&parts) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} is read-only: git runs hooks and configuration programs outside the sandbox",
                path.display()
            ),
        ));
    }
    Ok(())
}

/// Workspace paths that stay read-only because git runs hooks and the
/// configuration's program settings outside the sandbox: `(suffix, subpath)`,
/// where `subpath` also covers everything below the path. The Seatbelt profile
/// (through `crate::workspace::git_read_only::GIT_READ_ONLY`) and the `write`/`edit` tools both
/// enforce this one list.
pub const GIT_READ_ONLY: [(&str, bool); 5] = [
    (".git", false),
    (".git/config", false),
    (".git/config.worktree", false),
    (".git/commondir", false),
    (".git/hooks", true),
];

/// Reports whether the folded components name protected git metadata.
fn is_protected_git_path(parts: &[String]) -> bool {
    for (name, subpath) in GIT_READ_ONLY {
        let protected: Vec<&str> = name.split('/').collect();
        let hit = if subpath {
            parts.len() >= protected.len() && parts[..protected.len()] == protected[..]
        } else {
            parts == protected
        };
        if hit {
            return true;
        }
    }
    if parts.last().is_some_and(|last| last == GIT_DIR_NAME) {
        return true;
    }
    parts
        .iter()
        .position(|part| part == GIT_DIR_NAME)
        .is_some_and(|git_index| {
            let inside = &parts[git_index + 1..];
            inside
                .last()
                .is_some_and(|last| GIT_DIR_READ_ONLY_FILES.contains(&last.as_str()))
                || inside.iter().any(|part| part == GIT_DIR_HOOKS)
        })
}

#[cfg(test)]
mod tests;
