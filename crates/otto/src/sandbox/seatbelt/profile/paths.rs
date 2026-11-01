//! Path resolution, canonicalization and string-level path helpers.

use super::*;

/// Canonicalizes `path` against the real filesystem and classifies it.
pub(crate) fn resolve_path(path: &str) -> Result<ResolvedPath, ResolveError> {
    if !valid_path_text(path) || !is_absolute(path) {
        return Err(ResolveError::Rejected);
    }
    let canonical = canonical_filesystem_path(path)?;
    if !valid_path_text(&canonical) || !is_absolute(&canonical) || clean(&canonical) != canonical {
        return Err(ResolveError::Rejected);
    }
    let info = std::fs::symlink_metadata(&canonical).map_err(classify_io)?;
    if info.file_type().is_symlink() {
        return Err(ResolveError::Rejected);
    }
    let mut resolved = ResolvedPath {
        path: canonical,
        kind: PathKind::Special,
        executable: false,
    };
    if info.is_dir() {
        resolved.kind = PathKind::Directory;
    } else if info.is_file() {
        resolved.kind = PathKind::Regular;
        resolved.executable = info.permissions().mode() & 0o111 != 0;
    }
    Ok(resolved)
}

/// Resolves symlinks and then rewrites every component to the spelling the
/// directory actually stores, so a case-insensitive alias cannot smuggle a
/// different path text into the profile.
pub(crate) fn canonical_filesystem_path(path: &str) -> Result<String, ResolveError> {
    let resolved = std::fs::canonicalize(path).map_err(classify_io)?;
    let Some(resolved) = resolved.to_str() else {
        return Err(ResolveError::Rejected);
    };
    if !is_absolute(resolved) || clean(resolved) != resolved {
        return Err(ResolveError::Rejected);
    }
    let relative = resolved.trim_start_matches('/');
    if relative.is_empty() {
        return Ok("/".to_string());
    }
    let mut current = "/".to_string();
    for component in relative.split('/') {
        let requested = join(&current, component);
        let requested_info = std::fs::symlink_metadata(&requested).map_err(classify_io)?;
        let mut actual: Option<String> = None;
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(&current).map_err(classify_io)? {
            let entry = entry.map_err(classify_io)?;
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if name == component {
                actual = Some(name);
                break;
            }
            entries.push(entry);
        }
        if actual.is_none() {
            for entry in entries {
                let info = entry.metadata().map_err(classify_io)?;
                let Ok(name) = entry.file_name().into_string() else {
                    continue;
                };
                if info.dev() == requested_info.dev()
                    && info.ino() == requested_info.ino()
                    && actual.as_ref().is_none_or(|current| name < *current)
                {
                    actual = Some(name);
                }
            }
        }
        let Some(name) = actual else {
            return Err(ResolveError::Rejected);
        };
        current = join(&current, &name);
    }
    Ok(current)
}

fn classify_io(error: std::io::Error) -> ResolveError {
    if error.kind() == std::io::ErrorKind::NotFound {
        ResolveError::NotFound
    } else {
        ResolveError::Rejected
    }
}

pub(in crate::sandbox::seatbelt) fn valid_resolved(path: &ResolvedPath) -> bool {
    valid_path_text(&path.path) && is_absolute(&path.path) && clean(&path.path) == path.path
}

/// Rejects text that could break out of a quoted Scheme string or a comment:
/// empty, embedded NUL, any control character, and the line and paragraph
/// separators.
pub(crate) fn valid_path_text(path: &str) -> bool {
    !path.is_empty()
        && !path
            .chars()
            .any(|c| c == '\0' || c.is_control() || c == '\u{2028}' || c == '\u{2029}')
}

pub(crate) fn is_absolute(path: &str) -> bool {
    path.starts_with('/')
}

pub(crate) fn clean(path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    let rooted = path.starts_with('/');
    let mut out: Vec<&str> = Vec::new();
    let mut dotdot = 0usize;
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if out.len() > dotdot {
                    out.pop();
                } else if !rooted {
                    out.push("..");
                    dotdot += 1;
                }
            }
            other => out.push(other),
        }
    }
    let joined = out.join("/");
    if rooted {
        format!("/{joined}")
    } else if joined.is_empty() {
        ".".to_string()
    } else {
        joined
    }
}

pub(crate) fn dir(path: &str) -> String {
    match path.rfind('/') {
        Some(index) => clean(&path[..index + 1]),
        None => ".".to_string(),
    }
}

pub(crate) fn join(base: &str, element: &str) -> String {
    if base.is_empty() {
        return clean(element);
    }
    if element.is_empty() {
        return clean(base);
    }
    clean(&format!("{base}/{element}"))
}

/// Whether `child` is `parent` or lies beneath it.
///
/// For the cleaned, absolute paths this module works with, containment is
/// exactly a path-component prefix test, and a mixed absolute/relative pair is
/// not within either way.
pub(crate) fn path_within(parent: &str, child: &str) -> bool {
    if is_absolute(parent) != is_absolute(child) {
        return false;
    }
    let parent = clean(parent);
    let child = clean(child);
    if parent == child {
        return true;
    }
    if parent == "/" {
        return is_absolute(&child);
    }
    child.starts_with(&format!("{parent}/"))
}

pub(crate) fn paths_overlap(left: &str, right: &str) -> bool {
    path_within(left, right) || path_within(right, left)
}

pub(crate) fn depth(path: &str) -> usize {
    if path == "/" {
        return 0;
    }
    clean(path).matches('/').count()
}

/// Shallowest first, then lexical, so the rendered order never depends on
/// discovery order.
pub(crate) fn sort_paths(paths: &mut [String]) {
    paths.sort_by(|left, right| depth(left).cmp(&depth(right)).then_with(|| left.cmp(right)));
}
