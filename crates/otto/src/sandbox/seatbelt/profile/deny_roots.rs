//! Resolution of required options and of the read policy: the denylist
//! entries and the `read_paths` exceptions.

use super::*;
use crate::sandbox::read_denylist::{BUILT_IN_DENY_PATHS, PRIVATE_ROOT_LEAF_PREFIX};

pub(super) fn resolve_required_options(
    options: &Options,
    resolve: &dyn Fn(&str) -> Result<ResolvedPath, ResolveError>,
) -> Result<ResolvedOptions, Rejected> {
    let workspace = resolve_required_directory(&options.workspace, resolve)?;
    let mut workspaces = Vec::with_capacity(options.workspaces.len() + 1);
    for path in options.workspaces.iter().chain(std::iter::once(&workspace)) {
        let resolved = resolve_required_directory(path, resolve)?;
        if !workspaces.contains(&resolved) {
            workspaces.push(resolved);
        }
    }
    let root = resolve_required_directory(&directory_text(&options.directories.root)?, resolve)?;
    let temp_directory =
        resolve_required_directory(&directory_text(&options.directories.temp)?, resolve)?;
    let cache_directory =
        resolve_required_directory(&directory_text(&options.directories.cache)?, resolve)?;
    let resolved_home = resolve_required_directory(&options.home, resolve)?;

    let shell = resolve(&options.shell).map_err(Rejected::from)?;
    if !valid_resolved(&shell) || shell.kind != PathKind::Regular || !shell.executable {
        return Err(Rejected);
    }

    if temp_directory != join(&root, "tmp")
        || cache_directory != join(&root, "cache")
        || paths_overlap(&workspace, &root)
        || workspaces
            .iter()
            .any(|workspace| paths_overlap(workspace, &root))
        || paths_overlap(&shell.path, &root)
    {
        return Err(Rejected);
    }
    let Some(network) = options.network else {
        return Err(Rejected);
    };

    Ok(ResolvedOptions {
        workspace,
        workspaces,
        directories: PrivateDirectories {
            root: root.into(),
            temp: temp_directory.into(),
            cache: cache_directory.into(),
        },
        shell: shell.path,
        home: resolved_home,
        network,
    })
}

fn resolve_required_directory(
    path: &str,
    resolve: &dyn Fn(&str) -> Result<ResolvedPath, ResolveError>,
) -> Result<String, Rejected> {
    let resolved = resolve(path).map_err(Rejected::from)?;
    if !valid_resolved(&resolved) || resolved.kind != PathKind::Directory {
        return Err(Rejected);
    }
    Ok(resolved.path)
}

/// The read policy of one profile: what is denied, which `read_paths`
/// exceptions are in effect and what was left out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ReadPolicy {
    /// The canonical user cache directory; every `otto-sandbox-*` child is
    /// denied.
    pub(super) cache_base: String,
    /// Denied subtrees: each entry in its written spelling and, when it exists
    /// and differs, its canonical spelling.
    pub(super) deny_paths: Vec<String>,
    /// Active `read_paths` exceptions, canonical, shallowest first.
    pub(super) exceptions: Vec<ResolvedPath>,
    pub(super) inert_read_paths: Vec<String>,
    pub(super) skipped_deny_paths: Vec<String>,
}

pub(super) fn resolve_read_policy(
    options: &Options,
    resolved: &ResolvedOptions,
    writable: &[String],
    resolve: &dyn Fn(&str) -> Result<ResolvedPath, ResolveError>,
) -> Result<ReadPolicy, Rejected> {
    let private_root = directory_text(&resolved.directories.root)?;
    let cache_base = dir(&private_root);
    let mut deny_paths: Vec<String> = Vec::new();
    let mut skipped_deny_paths: Vec<String> = Vec::new();

    for relative in BUILT_IN_DENY_PATHS {
        let spellings = deny_spellings(&join(&resolved.home, relative), resolve)?;
        if contains_writable_root(&spellings, writable) {
            skipped_deny_paths.push(format!("~/{relative}"));
        } else {
            push_unique(&mut deny_paths, spellings);
        }
    }
    for configured in &options.deny_read_paths {
        let spellings = deny_spellings(&expand_read_path(configured, &resolved.home)?, resolve)?;
        // The sandbox cannot run without reading its writable roots, so a
        // configured entry that covers one makes the sandbox unavailable
        // instead of silently narrower.
        if contains_writable_root(&spellings, writable) {
            return Err(Rejected);
        }
        push_unique(&mut deny_paths, spellings);
    }

    let mut exceptions: Vec<ResolvedPath> = Vec::new();
    let mut inert_read_paths: Vec<String> = Vec::new();
    for configured in &options.read_paths {
        let expanded = expand_read_path(configured, &resolved.home)?;
        let candidate = match resolve(&expanded) {
            Ok(candidate) => candidate,
            // Configured paths can disappear when a linked worktree is
            // removed. They grant no access while absent, rather than making
            // the whole sandbox unavailable.
            Err(ResolveError::NotFound) => continue,
            Err(ResolveError::Rejected) => return Err(Rejected),
        };
        if !valid_resolved(&candidate)
            || !matches!(candidate.kind, PathKind::Directory | PathKind::Regular)
            || paths_overlap(&candidate.path, &private_root)
            || overlaps_private_roots(&candidate.path, &cache_base)
        {
            return Err(Rejected);
        }
        if deny_paths
            .iter()
            .any(|denied| path_within(denied, &candidate.path))
        {
            if !exceptions
                .iter()
                .any(|existing| existing.path == candidate.path)
            {
                exceptions.push(candidate);
            }
        } else if !inert_read_paths.contains(configured) {
            inert_read_paths.push(configured.clone());
        }
    }
    exceptions.sort_by(|left, right| {
        depth(&left.path)
            .cmp(&depth(&right.path))
            .then_with(|| left.path.cmp(&right.path))
    });
    sort_paths(&mut deny_paths);

    let policy = ReadPolicy {
        cache_base,
        deny_paths,
        exceptions,
        inert_read_paths,
        skipped_deny_paths,
    };
    if !within_path_bytes(&policy, &resolved.directories) {
        return Err(Rejected);
    }
    Ok(policy)
}

/// The written spelling of a denylist entry and, when the entry exists and
/// canonicalizes to a different path, that spelling too. An entry that does not
/// exist, or cannot be resolved, is denied by its written spelling alone.
fn deny_spellings(
    written: &str,
    resolve: &dyn Fn(&str) -> Result<ResolvedPath, ResolveError>,
) -> Result<Vec<String>, Rejected> {
    if !valid_path_text(written) || !is_absolute(written) {
        return Err(Rejected);
    }
    let mut spellings = vec![clean(written)];
    if let Ok(canonical) = resolve(written)
        && valid_resolved(&canonical)
        && !spellings.contains(&canonical.path)
    {
        spellings.push(canonical.path);
    }
    Ok(spellings)
}

fn contains_writable_root(spellings: &[String], writable: &[String]) -> bool {
    spellings
        .iter()
        .any(|entry| writable.iter().any(|root| path_within(entry, root)))
}

fn push_unique(deny_paths: &mut Vec<String>, spellings: Vec<String>) {
    for spelling in spellings {
        if !deny_paths.contains(&spelling) {
            deny_paths.push(spelling);
        }
    }
}

/// Whether `candidate` is, contains or lies inside an `otto-sandbox-*` private
/// root below `cache_base`. A `read_paths` exception there would expose
/// another session's state or the profile of this one.
fn overlaps_private_roots(candidate: &str, cache_base: &str) -> bool {
    if path_within(candidate, cache_base) {
        return true;
    }
    let Some(below) = candidate
        .strip_prefix(cache_base)
        .and_then(|below| below.strip_prefix('/'))
    else {
        return false;
    };
    below
        .split('/')
        .next()
        .is_some_and(|leaf| leaf.starts_with(PRIVATE_ROOT_LEAF_PREFIX))
}

/// The path text of the rendered read rules stays within
/// [`MAX_DYNAMIC_PATH_BYTES`].
fn within_path_bytes(policy: &ReadPolicy, directories: &PrivateDirectories) -> bool {
    let mut bytes = policy.cache_base.len()
        + directories.temp.as_os_str().len()
        + directories.cache.as_os_str().len();
    let texts = policy
        .deny_paths
        .iter()
        .map(String::len)
        .chain(policy.exceptions.iter().map(|root| root.path.len()));
    for length in texts {
        // The same subtraction in `usize` would underflow, so it is rearranged.
        if length + bytes > MAX_DYNAMIC_PATH_BYTES {
            return false;
        }
        bytes += length;
    }
    true
}

fn expand_read_path(path: &str, home: &str) -> Result<String, Rejected> {
    if !valid_path_text(path) {
        return Err(Rejected);
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return Ok(join(home, rest));
    }
    if !is_absolute(path) {
        return Err(Rejected);
    }
    Ok(path.to_string())
}
