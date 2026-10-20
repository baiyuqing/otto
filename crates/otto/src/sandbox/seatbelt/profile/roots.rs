//! Resolution of required options and the reviewed read-root set.

use super::*;

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
    let home_directory =
        resolve_required_directory(&directory_text(&options.directories.home)?, resolve)?;
    let temp_directory =
        resolve_required_directory(&directory_text(&options.directories.temp)?, resolve)?;
    let cache_directory =
        resolve_required_directory(&directory_text(&options.directories.cache)?, resolve)?;
    let resolved_home = resolve_required_directory(&options.home, resolve)?;

    let shell = resolve(&options.shell).map_err(Rejected::from)?;
    if !valid_resolved(&shell) || shell.kind != PathKind::Regular || !shell.executable {
        return Err(Rejected);
    }

    if home_directory != join(&root, "home")
        || temp_directory != join(&root, "tmp")
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
            home: home_directory.into(),
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

pub(super) fn discover_read_roots(
    options: &Options,
    resolved: &ResolvedOptions,
    dependencies: &Dependencies<'_>,
) -> Result<Vec<ResolvedPath>, Rejected> {
    let private_root = directory_text(&resolved.directories.root)?;
    let mut roots =
        Vec::with_capacity(dependencies.fixed_paths.len() + options.read_paths.len() + 8);

    for automatic in &dependencies.fixed_paths {
        let Some(candidate) =
            resolve_automatic_path(&automatic.path, automatic.kind, dependencies.resolve)?
        else {
            continue;
        };
        if exact_broad_anchor(&candidate.path, &resolved.home) {
            continue;
        }
        if paths_overlap(&candidate.path, &private_root) {
            return Err(Rejected);
        }
        if !safe_fixed_root(automatic, &candidate.path, &resolved.home) {
            continue;
        }
        roots.push(candidate);
    }

    if let Some(developer_root) = (dependencies.developer_root)()? {
        let candidate = (dependencies.resolve)(&developer_root).map_err(Rejected::from)?;
        if !valid_resolved(&candidate) || candidate.kind != PathKind::Directory {
            return Err(Rejected);
        }
        if !exact_broad_anchor(&candidate.path, &resolved.home) {
            if paths_overlap(&candidate.path, &private_root) {
                return Err(Rejected);
            }
            if safe_automatic_directory_root(&candidate.path, &resolved.home, false) {
                roots.push(candidate);
            }
        }
    }

    for path in path_entries(&options.host_entries)? {
        if path.is_empty() || !is_absolute(&path) {
            continue;
        }
        let Some(candidate) =
            resolve_automatic_path(&path, PathKind::Directory, dependencies.resolve)?
        else {
            continue;
        };
        if exact_broad_anchor(&candidate.path, &resolved.home) {
            continue;
        }
        if paths_overlap(&candidate.path, &private_root) {
            return Err(Rejected);
        }
        if !safe_automatic_directory_root(&candidate.path, &resolved.home, true) {
            continue;
        }
        roots.push(candidate);
    }

    for configured in &options.read_paths {
        let expanded = expand_read_path(configured, &resolved.home)?;
        let candidate = match (dependencies.resolve)(&expanded) {
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
        {
            return Err(Rejected);
        }
        roots.push(candidate);
    }
    Ok(roots)
}

/// Resolves one entry of the reviewed fixed list. `Ok(None)` means the path
/// simply does not exist on this host.
fn resolve_automatic_path(
    path: &str,
    expected: PathKind,
    resolve: &dyn Fn(&str) -> Result<ResolvedPath, ResolveError>,
) -> Result<Option<ResolvedPath>, Rejected> {
    if !valid_path_text(path)
        || !is_absolute(path)
        || !matches!(expected, PathKind::Directory | PathKind::Regular)
    {
        return Err(Rejected);
    }
    match resolve(path) {
        Err(ResolveError::NotFound) => Ok(None),
        Err(ResolveError::Rejected) => Err(Rejected),
        Ok(resolved) if valid_resolved(&resolved) && resolved.kind == expected => {
            Ok(Some(resolved))
        }
        Ok(_) => Err(Rejected),
    }
}

/// The entries of the host's single `PATH`. A duplicate `PATH` is a rejection;
/// a missing or empty one yields no entries.
fn path_entries(host_entries: &[String]) -> Result<Vec<String>, Rejected> {
    let mut value = None;
    for entry in host_entries {
        let Some(entry) = entry.strip_prefix("PATH=") else {
            continue;
        };
        if value.is_some() {
            return Err(Rejected);
        }
        value = Some(entry);
    }
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    if value.is_empty() {
        return Ok(Vec::new());
    }
    if value.contains('\0') {
        return Err(Rejected);
    }
    Ok(value.split(':').map(str::to_string).collect())
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

/// Roots so broad that granting them would defeat confinement.
fn exact_broad_anchor(path: &str, home: &str) -> bool {
    path == home
        || matches!(
            path,
            "/" | "/Users"
                | "/Applications"
                | "/Library"
                | "/Network"
                | "/Volumes"
                | "/dev"
                | "/private"
                | "/private/etc"
                | "/private/tmp"
                | "/private/var"
                | "/usr"
                | "/opt"
                | "/opt/homebrew"
                | "/usr/local"
        )
}

fn safe_automatic_directory_root(path: &str, home: &str, allow_home_descendant: bool) -> bool {
    if exact_broad_anchor(path, home) {
        return false;
    }
    if path_within(home, path) {
        return allow_home_descendant && path != home;
    }
    ![
        "/Users",
        "/Network",
        "/Volumes",
        "/dev",
        "/private/etc",
        "/private/tmp",
        "/private/var",
        "/opt/homebrew/etc",
        "/opt/homebrew/var",
        "/usr/local/etc",
        "/usr/local/var",
    ]
    .iter()
    .any(|parent| path_within(parent, path))
}

fn safe_fixed_root(automatic: &AutomaticPath, canonical: &str, home: &str) -> bool {
    if automatic.kind == PathKind::Directory {
        return safe_automatic_directory_root(canonical, home, false);
    }
    if automatic.kind != PathKind::Regular
        || exact_broad_anchor(canonical, home)
        || path_within(home, canonical)
        || [
            "/Users",
            "/Network",
            "/Volumes",
            "/dev",
            "/private/tmp",
            "/opt/homebrew/etc",
            "/opt/homebrew/var",
            "/usr/local/etc",
            "/usr/local/var",
        ]
        .iter()
        .any(|parent| path_within(parent, canonical))
    {
        return false;
    }
    if path_within("/private/etc", canonical) {
        return automatic.path.starts_with("/private/etc/");
    }
    if path_within("/private/var", canonical) {
        return automatic.path == "/private/etc/resolv.conf"
            && canonical == "/private/var/run/resolv.conf";
    }
    true
}

/// Deduplicates roots, drops any already covered by a writable root, and keeps
/// only the shallowest of each nested chain. Ordering is by depth then path so
/// the result is independent of discovery order.
pub(super) fn collapse_read_roots(
    roots: Vec<ResolvedPath>,
    writable: &[String],
) -> Vec<ResolvedPath> {
    let mut by_path: BTreeMap<String, ResolvedPath> = BTreeMap::new();
    for root in roots {
        match by_path.get(&root.path) {
            Some(existing) if existing.kind != root.kind => {}
            _ => {
                by_path.insert(root.path.clone(), root);
            }
        }
    }
    let mut unique: Vec<ResolvedPath> = by_path
        .into_values()
        .filter(|root| {
            !writable
                .iter()
                .any(|parent| path_within(parent, &root.path))
        })
        .collect();
    unique.sort_by(|left, right| {
        depth(&left.path)
            .cmp(&depth(&right.path))
            .then_with(|| left.path.cmp(&right.path))
    });

    let mut collapsed: Vec<ResolvedPath> = Vec::with_capacity(unique.len());
    for candidate in unique {
        let contained = collapsed.iter().any(|parent| {
            parent.kind == PathKind::Directory && path_within(&parent.path, &candidate.path)
        });
        if !contained {
            collapsed.push(candidate);
        }
    }
    collapsed
}

pub(super) fn roots_within_limits(roots: &[ResolvedPath]) -> bool {
    if roots.len() > MAX_DYNAMIC_ROOTS {
        return false;
    }
    let mut bytes = 0usize;
    for root in roots {
        // The same subtraction in `usize` would underflow, so it is rearranged.
        if root.path.len() + bytes > MAX_DYNAMIC_PATH_BYTES {
            return false;
        }
        bytes += root.path.len();
    }
    true
}

/// Every ancestor directory of every granted path, so the child can traverse
/// to them without being able to list their contents.
pub(super) fn metadata_ancestors(
    writable: &[String],
    roots: &[ResolvedPath],
    shell: &str,
) -> Vec<String> {
    let mut metadata: BTreeSet<String> = BTreeSet::new();
    let mut add = |path: &str| {
        let mut ancestor = dir(path);
        loop {
            metadata.insert(ancestor.clone());
            let next = dir(&ancestor);
            if next == ancestor {
                break;
            }
            ancestor = next;
        }
    };
    for path in writable {
        add(path);
    }
    for root in roots {
        add(&root.path);
    }
    add(shell);

    let mut paths: Vec<String> = metadata.into_iter().collect();
    sort_paths(&mut paths);
    paths
}
