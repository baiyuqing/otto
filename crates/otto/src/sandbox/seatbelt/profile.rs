//! Generation of the Seatbelt profile for one session.
//!
//! [`generate`] renders the four dynamic sections of [`super::TEMPLATE`] from a
//! session's workspace, private state directories, shell and reviewed read
//! roots.
//!
//! Ownership: every function here is pure with respect to the caller's data;
//! the only side effects are the filesystem reads used to canonicalize paths.
//!
//! Concurrency: no shared state, so generation is safe from any thread. It
//! performs blocking filesystem work and belongs on a blocking task.
//!
//! Errors: every rejection is the single [`Rejected`] value. The generated
//! profile reaches a child process description and the failure text reaches the
//! model, so a rejection never says which path failed.
//!
//! Paths are handled as `String` rather than `Path`: [`valid_path_text`]
//! already requires valid UTF-8, and the rules are rendered as text.

use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::{MetadataExt, PermissionsExt};

use crate::sandbox::{NetworkMode, PrivateDirectories};

/// At most this many dynamic read roots may reach the profile.
pub(crate) const MAX_DYNAMIC_ROOTS: usize = 128;
/// The dynamic read roots may contribute at most this many bytes of path text.
pub(crate) const MAX_DYNAMIC_PATH_BYTES: usize = 32 * 1024;

pub(crate) const READ_MARKER: &str = "@@OTTO_PROFILE_READ_RULES@@";
pub(crate) const WRITE_MARKER: &str = "@@OTTO_PROFILE_WRITE_RULES@@";
pub(crate) const NETWORK_MARKER: &str = "@@OTTO_PROFILE_NETWORK_RULES@@";
pub(crate) const SHELL_MARKER: &str = "@@OTTO_PROFILE_SHELL_RULE@@";
/// Replaced at every template deny with `(with message "<tag>")`.
pub(crate) const DENY_MESSAGE_MARKER: &str = "@@OTTO_DENY_MESSAGE@@";

/// Directories that are read-only for every session when they exist.
pub(crate) const REVIEWED_AUTOMATIC_PATHS: &[&str] = &[
    "/bin",
    "/sbin",
    "/usr/bin",
    "/usr/sbin",
    "/usr/lib",
    "/usr/libexec",
    "/usr/share",
    "/usr/include",
    "/System",
    "/Library/Apple",
    "/Library/Developer",
    "/opt/homebrew/bin",
    "/opt/homebrew/sbin",
    "/opt/homebrew/lib",
    "/opt/homebrew/share",
    "/opt/homebrew/include",
    "/opt/homebrew/Cellar",
    "/opt/homebrew/opt",
    "/usr/local/bin",
    "/usr/local/sbin",
    "/usr/local/lib",
    "/usr/local/share",
    "/usr/local/include",
    "/usr/local/Cellar",
    "/usr/local/opt",
    "/usr/local/Homebrew",
];

/// Individual runtime files that are read-only for every session.
pub(crate) const REVIEWED_RUNTIME_FILES: &[&str] = &[
    "/private/etc/hosts",
    "/private/etc/protocols",
    "/private/etc/resolv.conf",
    "/private/etc/services",
    "/private/etc/ssl/cert.pem",
];

/// The single rejection value. It carries no detail on purpose: the text is
/// reachable by the model through the bash tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("seatbelt profile rejected")]
pub(crate) struct Rejected;

/// Why a path could not be resolved.
///
/// `NotFound` is the only recoverable case: an automatic root that does not
/// exist on this host is skipped rather than rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResolveError {
    NotFound,
    Rejected,
}

impl From<ResolveError> for Rejected {
    fn from(_: ResolveError) -> Self {
        Self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum PathKind {
    Directory,
    Regular,
    Special,
}

/// One canonical path together with what the filesystem says it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedPath {
    pub(crate) path: String,
    pub(crate) kind: PathKind,
    pub(crate) executable: bool,
}

/// One entry of the reviewed fixed list, with the kind it must have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AutomaticPath {
    pub(crate) path: String,
    pub(crate) kind: PathKind,
}

/// What the caller asks the profile to permit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Options {
    pub(crate) workspace: String,
    /// Canonical Git-linked worktree roots, including `workspace`.
    pub(crate) workspaces: Vec<String>,
    pub(crate) directories: PrivateDirectories,
    pub(crate) shell: String,
    pub(crate) home: String,
    /// The host environment, only read for its single `PATH` entry.
    pub(crate) host_entries: Vec<String>,
    pub(crate) read_paths: Vec<String>,
    pub(crate) network: Option<NetworkMode>,
}

/// The filesystem facts generation depends on.
pub(crate) struct Dependencies<'a> {
    pub(crate) resolve: &'a dyn Fn(&str) -> Result<ResolvedPath, ResolveError>,
    pub(crate) fixed_paths: Vec<AutomaticPath>,
    pub(crate) developer_root: &'a dyn Fn() -> Result<Option<String>, Rejected>,
}

/// The required options once each has been canonicalized and cross-checked.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedOptions {
    workspace: String,
    workspaces: Vec<String>,
    directories: PrivateDirectories,
    shell: String,
    home: String,
    network: NetworkMode,
}

/// Renders the profile for `options` against the real filesystem.
pub(crate) fn generate(options: &Options) -> Result<String, Rejected> {
    let mut fixed_paths =
        Vec::with_capacity(REVIEWED_AUTOMATIC_PATHS.len() + REVIEWED_RUNTIME_FILES.len());
    for path in REVIEWED_AUTOMATIC_PATHS {
        fixed_paths.push(AutomaticPath {
            path: (*path).to_string(),
            kind: PathKind::Directory,
        });
    }
    for path in REVIEWED_RUNTIME_FILES {
        fixed_paths.push(AutomaticPath {
            path: (*path).to_string(),
            kind: PathKind::Regular,
        });
    }
    generate_with(
        options,
        &Dependencies {
            resolve: &resolve_path,
            fixed_paths,
            developer_root: &discover_developer_root,
        },
    )
}

/// Renders the profile against injected filesystem facts.
pub(crate) fn generate_with(
    options: &Options,
    dependencies: &Dependencies<'_>,
) -> Result<String, Rejected> {
    let resolved = resolve_required_options(options, dependencies.resolve)?;
    let roots = discover_read_roots(options, &resolved, dependencies)?;
    let mut writable = resolved.workspaces.clone();
    writable.extend([
        directory_text(&resolved.directories.home)?,
        directory_text(&resolved.directories.temp)?,
        directory_text(&resolved.directories.cache)?,
    ]);
    let roots = collapse_read_roots(roots, &writable);
    if !roots_within_limits(&roots) {
        return Err(Rejected);
    }

    sort_paths(&mut writable);
    let metadata = metadata_ancestors(&writable, &roots, &resolved.shell);

    let read_rules = render_read_rules(&metadata, &writable, &roots)?;
    let tag = options
        .directories
        .root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(Rejected)?;
    let deny_message = format!("(with message {})", string_literal(tag)?);
    let write_rules = render_write_rules(&writable, &resolved.workspaces, &deny_message)?;
    let network_rules = render_network_rules(resolved.network);
    let shell_rule = render_shell_rule(&resolved.shell)?;

    let template = super::TEMPLATE;
    for marker in [READ_MARKER, WRITE_MARKER, NETWORK_MARKER, SHELL_MARKER] {
        if template.matches(marker).count() != 1 {
            return Err(Rejected);
        }
    }
    if template.matches(DENY_MESSAGE_MARKER).count() != 5 {
        return Err(Rejected);
    }
    // One left-to-right pass over the template, so generated text is never
    // rescanned for markers.
    Ok(replace_markers(
        template,
        &[
            (READ_MARKER, read_rules.as_str()),
            (WRITE_MARKER, write_rules.as_str()),
            (NETWORK_MARKER, network_rules.as_str()),
            (SHELL_MARKER, shell_rule.as_str()),
            (DENY_MESSAGE_MARKER, deny_message.as_str()),
        ],
    ))
}

/// Replacement scans left to right: the earliest match in the remaining input
/// wins and the replacement is never reconsidered.
fn replace_markers(template: &str, pairs: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    'outer: while !rest.is_empty() {
        let mut best: Option<(usize, usize, &str)> = None;
        for (marker, replacement) in pairs {
            if let Some(index) = rest.find(marker)
                && best.is_none_or(|(current, _, _)| index < current)
            {
                best = Some((index, marker.len(), replacement));
            }
        }
        match best {
            Some((index, length, replacement)) => {
                out.push_str(&rest[..index]);
                out.push_str(replacement);
                rest = &rest[index + length..];
            }
            None => break 'outer,
        }
    }
    out.push_str(rest);
    out
}

fn directory_text(path: &std::path::Path) -> Result<String, Rejected> {
    path.to_str().map(str::to_string).ok_or(Rejected)
}

fn resolve_required_options(
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

fn discover_read_roots(
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
fn collapse_read_roots(roots: Vec<ResolvedPath>, writable: &[String]) -> Vec<ResolvedPath> {
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

fn roots_within_limits(roots: &[ResolvedPath]) -> bool {
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
fn metadata_ancestors(writable: &[String], roots: &[ResolvedPath], shell: &str) -> Vec<String> {
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

fn render_read_rules(
    metadata: &[String],
    writable: &[String],
    roots: &[ResolvedPath],
) -> Result<String, Rejected> {
    let mut builder = String::new();
    builder.push_str("; OTTO-DYNAMIC-READ-BEGIN\n");
    builder.push_str("; OTTO-DYNAMIC-METADATA-BEGIN\n");
    let literals: Vec<Filter> = metadata
        .iter()
        .map(|path| Filter {
            kind: FilterKind::Literal,
            path: path.clone(),
        })
        .collect();
    write_rule(&mut builder, "allow", "file-read-metadata", &literals, None)?;
    builder.push_str("; OTTO-DYNAMIC-METADATA-END\n");
    builder.push_str("; OTTO-DYNAMIC-READ-DATA-BEGIN\n");
    let mut filters: Vec<Filter> = Vec::with_capacity(writable.len() + roots.len());
    for path in writable {
        filters.push(Filter {
            kind: FilterKind::Subpath,
            path: path.clone(),
        });
    }
    for root in roots {
        filters.push(Filter {
            kind: if root.kind == PathKind::Directory {
                FilterKind::Subpath
            } else {
                FilterKind::Literal
            },
            path: root.path.clone(),
        });
    }
    write_rule(&mut builder, "allow", "file-read*", &filters, None)?;
    builder.push_str("; OTTO-DYNAMIC-READ-DATA-END\n");
    builder.push_str("; OTTO-DYNAMIC-READ-END");
    Ok(builder)
}

fn render_write_rules(
    writable: &[String],
    workspaces: &[String],
    deny_message: &str,
) -> Result<String, Rejected> {
    let mut builder = String::new();
    builder.push_str("; OTTO-DYNAMIC-WRITE-BEGIN\n");
    let filters: Vec<Filter> = writable
        .iter()
        .map(|path| Filter {
            kind: FilterKind::Subpath,
            path: path.clone(),
        })
        .collect();
    write_rule(&mut builder, "allow", "file-write*", &filters, None)?;
    // Git runs hooks and the configuration's program settings outside the
    // sandbox, so these workspace paths are read-only. Seatbelt applies the
    // last matching rule: the deny must follow the workspace allow above (a
    // probe on macOS 27.2 showed the same denies placed before it had no
    // effect). The rules are emitted whether or not the paths exist, and no
    // setting turns them off.
    let protected: Vec<Filter> = workspaces
        .iter()
        .flat_map(|workspace| {
            crate::sandbox::GIT_READ_ONLY
                .iter()
                .chain(crate::workspace::READ_ONLY_PATHS.iter())
                .map(move |(name, subpath)| Filter {
                    kind: if *subpath {
                        FilterKind::Subpath
                    } else {
                        FilterKind::Literal
                    },
                    path: join(workspace, name),
                })
        })
        .collect();
    write_rule(
        &mut builder,
        "deny",
        "file-write*",
        &protected,
        Some(deny_message),
    )?;
    builder.push_str("; OTTO-DYNAMIC-WRITE-END");
    Ok(builder)
}

fn render_network_rules(network: NetworkMode) -> String {
    match network {
        NetworkMode::Deny => "; OTTO-DYNAMIC-NETWORK-BEGIN\n; OTTO-DYNAMIC-NETWORK-END".to_string(),
        NetworkMode::Allow => [
            "; OTTO-DYNAMIC-NETWORK-BEGIN",
            "(allow mach-lookup",
            "  (global-name \"com.apple.mDNSResponder\"))",
            // getaddrinfo on current macOS connects this exact resolver socket
            // directly (observed via sandbox-exec bisection on macOS 26), in
            // addition to the mDNSResponder mach broker above; without this
            // literal exception, network = "allow" cannot resolve hostnames.
            "(allow network-outbound",
            "  (remote ip)",
            "  (remote unix-socket (path \"/private/var/run/mDNSResponder\")))",
            // A TLS client inside the sandbox evaluates server certificates
            // through the Security framework, which brokers to trustd. Without
            // these exact services it fails with an opaque trust error such as
            // "x509: OSStatus -26276" even though the CA bundle is readable.
            "(allow mach-lookup",
            "  (global-name \"com.apple.trustd\")",
            "  (global-name \"com.apple.trustd.agent\"))",
            "(allow network-bind",
            "  (local ip))",
            "(allow network-inbound",
            "  (local ip))",
            "; OTTO-DYNAMIC-NETWORK-END",
        ]
        .join("\n"),
    }
}

fn render_shell_rule(shell: &str) -> Result<String, Rejected> {
    let mut builder = String::new();
    builder.push_str("; OTTO-DYNAMIC-SHELL-BEGIN\n");
    write_rule(
        &mut builder,
        "allow",
        "file-read*",
        &[Filter {
            kind: FilterKind::Literal,
            path: shell.to_string(),
        }],
        None,
    )?;
    builder.push_str("; OTTO-DYNAMIC-SHELL-END");
    Ok(builder)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FilterKind {
    Literal,
    Subpath,
}

impl FilterKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Literal => "literal",
            Self::Subpath => "subpath",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Filter {
    kind: FilterKind,
    path: String,
}

/// Writes one `(allow …)` or `(deny …)` form. An empty filter list is a
/// rejection: an operation with no filter would apply to everything.
fn write_rule(
    builder: &mut String,
    action: &str,
    operation: &str,
    filters: &[Filter],
    message: Option<&str>,
) -> Result<(), Rejected> {
    if filters.is_empty() {
        return Err(Rejected);
    }
    builder.push('(');
    builder.push_str(action);
    builder.push(' ');
    builder.push_str(operation);
    builder.push('\n');
    for filter in filters {
        let literal = string_literal(&filter.path)?;
        builder.push_str("  (");
        builder.push_str(filter.kind.as_str());
        builder.push(' ');
        builder.push_str(&literal);
        builder.push_str(")\n");
    }
    if let Some(message) = message {
        builder.push_str("  ");
        builder.push_str(message);
        builder.push('\n');
    }
    builder.push_str(")\n");
    Ok(())
}

/// Quotes `value` as a Scheme string. Only `\` and `"` need escaping, and
/// [`valid_path_text`] has already excluded every character that could end a
/// line or a comment.
fn string_literal(value: &str) -> Result<String, Rejected> {
    if !valid_path_text(value) {
        return Err(Rejected);
    }
    let mut builder = String::with_capacity(value.len() + 2);
    builder.push('"');
    for character in value.chars() {
        if character == '\\' || character == '"' {
            builder.push('\\');
        }
        builder.push(character);
    }
    builder.push('"');
    Ok(builder)
}

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

/// The active developer directory, if `xcode-select` has recorded one.
pub(crate) fn discover_developer_root() -> Result<Option<String>, Rejected> {
    match std::fs::canonicalize("/var/db/xcode_select_link") {
        Ok(root) => root.to_str().map(str::to_string).map(Some).ok_or(Rejected),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(Rejected),
    }
}

pub(super) fn valid_resolved(path: &ResolvedPath) -> bool {
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

#[cfg(test)]
mod tests;
