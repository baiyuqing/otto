//! Outcome classification and request or environment validation.

use super::*;

pub(super) fn successful(outcome: &Outcome) -> bool {
    outcome.code == 0 && !outcome.signaled && outcome.signal.is_empty()
}

pub(super) fn denied_result(outcome: &Outcome) -> bool {
    outcome.code > 0 && !outcome.signaled && outcome.signal.is_empty()
}

pub(super) fn check_cancelled(cancel: &CancellationToken) -> Result<(), Error> {
    if cancel.is_cancelled() {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

/// Whether a manager failure means the sandbox machinery broke rather than the
/// command. Cancellation and closure are ordinary; everything else is not.
pub(super) fn infrastructure_error(error: &Error) -> bool {
    !matches!(error, Error::Cancelled | Error::Closed)
}

/// Reduces a manager failure to the bounded set the caller may see.
pub(super) fn bounded_execution_error(result: Result<(), Error>) -> Result<(), Error> {
    match result {
        Ok(()) => Ok(()),
        Err(Error::Cancelled) => Err(Error::Cancelled),
        Err(Error::Closed) => Err(Error::Closed),
        Err(_) => Err(Error::unavailable(UnavailableReason::RuntimeFailure)),
    }
}

/// Whether `/usr/bin/sandbox-exec` is the canonical, root-owned, executable
/// regular file it must be. Anything else means the host cannot confine.
pub(super) fn valid_sandbox_exec() -> bool {
    use std::os::unix::fs::MetadataExt as _;
    use std::os::unix::fs::PermissionsExt as _;

    let Ok(canonical) = profile::canonical_filesystem_path(SANDBOX_EXEC_PATH) else {
        return false;
    };
    if canonical != SANDBOX_EXEC_PATH {
        return false;
    }
    std::fs::symlink_metadata(SANDBOX_EXEC_PATH).is_ok_and(|info| {
        info.is_file()
            && !info.file_type().is_symlink()
            && info.uid() == 0
            && info.permissions().mode() & 0o111 != 0
    })
}

/// Canonicalizes `path` and requires it to be an existing directory.
pub(super) fn resolve_directory(path: &str) -> Option<String> {
    match profile::resolve_path(path) {
        Ok(resolved)
            if profile::valid_resolved(&resolved)
                && resolved.kind == profile::PathKind::Directory =>
        {
            Some(resolved.path)
        }
        _ => None,
    }
}

/// The platform user cache directory on macOS.
pub(super) fn user_cache_directory() -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    if home.is_empty() || !std::path::Path::new(&home).is_absolute() {
        return None;
    }
    Some(format!("{home}/Library/Caches"))
}

/// `NAME=VALUE` entries with POSIX-ish names, no NUL and no duplicates.
pub(super) fn valid_environment(environment: &[String]) -> bool {
    let mut names = std::collections::HashSet::with_capacity(environment.len());
    for entry in environment {
        if entry.contains('\0') {
            return false;
        }
        let Some((name, _)) = entry.split_once('=') else {
            return false;
        };
        if !valid_environment_name(name) || !names.insert(name) {
            return false;
        }
    }
    true
}

fn valid_environment_name(name: &str) -> bool {
    let mut characters = name.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    if first != '_' && !first.is_ascii_alphabetic() {
        return false;
    }
    characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
}
