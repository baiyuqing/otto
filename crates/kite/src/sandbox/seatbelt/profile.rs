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

pub(crate) const READ_MARKER: &str = "@@KITE_PROFILE_READ_RULES@@";
pub(crate) const WRITE_MARKER: &str = "@@KITE_PROFILE_WRITE_RULES@@";
pub(crate) const NETWORK_MARKER: &str = "@@KITE_PROFILE_NETWORK_RULES@@";
pub(crate) const SHELL_MARKER: &str = "@@KITE_PROFILE_SHELL_RULE@@";

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
    let writable = vec![
        resolved.workspace.clone(),
        directory_text(&resolved.directories.home)?,
        directory_text(&resolved.directories.temp)?,
        directory_text(&resolved.directories.cache)?,
    ];
    let roots = collapse_read_roots(roots, &writable);
    if !roots_within_limits(&roots) {
        return Err(Rejected);
    }

    let mut writable = writable;
    sort_paths(&mut writable);
    let metadata = metadata_ancestors(&writable, &roots, &resolved.shell);

    let read_rules = render_read_rules(&metadata, &writable, &roots)?;
    let write_rules = render_write_rules(&writable)?;
    let network_rules = render_network_rules(resolved.network);
    let shell_rule = render_shell_rule(&resolved.shell)?;

    let template = super::TEMPLATE;
    for marker in [READ_MARKER, WRITE_MARKER, NETWORK_MARKER, SHELL_MARKER] {
        if template.matches(marker).count() != 1 {
            return Err(Rejected);
        }
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
        || paths_overlap(&shell.path, &root)
    {
        return Err(Rejected);
    }
    let Some(network) = options.network else {
        return Err(Rejected);
    };

    Ok(ResolvedOptions {
        workspace,
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
        let candidate = (dependencies.resolve)(&expanded).map_err(Rejected::from)?;
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
    builder.push_str("; KITE-DYNAMIC-READ-BEGIN\n");
    builder.push_str("; KITE-DYNAMIC-METADATA-BEGIN\n");
    let literals: Vec<Filter> = metadata
        .iter()
        .map(|path| Filter {
            kind: FilterKind::Literal,
            path: path.clone(),
        })
        .collect();
    write_rule(&mut builder, "file-read-metadata", &literals)?;
    builder.push_str("; KITE-DYNAMIC-METADATA-END\n");
    builder.push_str("; KITE-DYNAMIC-READ-DATA-BEGIN\n");
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
    write_rule(&mut builder, "file-read*", &filters)?;
    builder.push_str("; KITE-DYNAMIC-READ-DATA-END\n");
    builder.push_str("; KITE-DYNAMIC-READ-END");
    Ok(builder)
}

fn render_write_rules(writable: &[String]) -> Result<String, Rejected> {
    let mut builder = String::new();
    builder.push_str("; KITE-DYNAMIC-WRITE-BEGIN\n");
    let filters: Vec<Filter> = writable
        .iter()
        .map(|path| Filter {
            kind: FilterKind::Subpath,
            path: path.clone(),
        })
        .collect();
    write_rule(&mut builder, "file-write*", &filters)?;
    builder.push_str("; KITE-DYNAMIC-WRITE-END");
    Ok(builder)
}

fn render_network_rules(network: NetworkMode) -> String {
    match network {
        NetworkMode::Deny => "; KITE-DYNAMIC-NETWORK-BEGIN\n; KITE-DYNAMIC-NETWORK-END".to_string(),
        NetworkMode::Allow => [
            "; KITE-DYNAMIC-NETWORK-BEGIN",
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
            "; KITE-DYNAMIC-NETWORK-END",
        ]
        .join("\n"),
    }
}

fn render_shell_rule(shell: &str) -> Result<String, Rejected> {
    let mut builder = String::new();
    builder.push_str("; KITE-DYNAMIC-SHELL-BEGIN\n");
    write_rule(
        &mut builder,
        "file-read*",
        &[Filter {
            kind: FilterKind::Literal,
            path: shell.to_string(),
        }],
    )?;
    builder.push_str("; KITE-DYNAMIC-SHELL-END");
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

/// Writes one `(allow …)` form. An empty filter list is a rejection: an
/// operation with no filter would allow it unconditionally.
fn write_rule(builder: &mut String, operation: &str, filters: &[Filter]) -> Result<(), Rejected> {
    if filters.is_empty() {
        return Err(Rejected);
    }
    builder.push_str("(allow ");
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
mod tests {
    use super::*;
    use crate::sandbox::seatbelt::state;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    /// [`Dependencies`] borrows its callbacks, so the fixture owns them and
    /// hands out a borrowing view.
    /// The fixture's owned resolver, matching [`Dependencies::resolve`].
    type ResolveFn = dyn Fn(&str) -> Result<ResolvedPath, ResolveError>;

    struct Deps {
        resolve: Box<ResolveFn>,
        fixed_paths: Vec<AutomaticPath>,
        developer_root: Box<dyn Fn() -> Result<Option<String>, Rejected>>,
    }

    impl Deps {
        fn new() -> Self {
            Self {
                resolve: Box::new(resolve_path),
                fixed_paths: Vec::new(),
                developer_root: Box::new(|| Ok(None)),
            }
        }

        fn view(&self) -> Dependencies<'_> {
            Dependencies {
                resolve: self.resolve.as_ref(),
                fixed_paths: self.fixed_paths.clone(),
                developer_root: self.developer_root.as_ref(),
            }
        }

        /// Layers one lookup over the current resolver. `None` falls through to
        /// the resolver that was installed before.
        fn override_resolve<F>(&mut self, lookup: F)
        where
            F: Fn(&str) -> Option<Result<ResolvedPath, ResolveError>> + 'static,
        {
            let base = std::mem::replace(&mut self.resolve, Box::new(resolve_path));
            self.resolve = Box::new(move |path| lookup(path).unwrap_or_else(|| base(path)));
        }

        fn with_synthetic_directories(&mut self, paths: &[String]) {
            let mut resolved: HashMap<String, ResolvedPath> = HashMap::new();
            for path in paths {
                resolved.insert(
                    path.clone(),
                    ResolvedPath {
                        path: path.clone(),
                        kind: PathKind::Directory,
                        executable: false,
                    },
                );
            }
            self.override_resolve(move |path| resolved.get(path).cloned().map(Ok));
        }

        fn developer_root_is(&mut self, root: String) {
            self.developer_root = Box::new(move || Ok(Some(root.clone())));
        }
    }

    /// Dropping it closes the private state before the temporary tree is
    /// removed, which is what `t.Cleanup` does there.
    struct Fixture {
        temp: Option<tempfile::TempDir>,
        base: String,
        state: Option<state::State>,
        options: Options,
        deps: Deps,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let result = self.state.take().expect("state").close();
            drop(self.temp.take());
            if !std::thread::panicking() {
                assert!(result.is_ok(), "state close failed: {result:?}");
            }
        }
    }

    impl Fixture {
        fn state(&self) -> &state::State {
            self.state.as_ref().expect("state")
        }

        fn render(&self) -> String {
            generate_with(&self.options, &self.deps.view()).expect("generate profile")
        }

        fn generate(&self) -> Result<String, Rejected> {
            generate_with(&self.options, &self.deps.view())
        }

        fn assert_rejected(&self, what: &str) {
            match self.generate() {
                Err(Rejected) => {}
                Ok(profile) => panic!("{what} generated {} bytes", profile.len()),
            }
        }

        fn directory(&self, path: &std::path::Path) -> String {
            path.to_str().expect("utf-8 directory").to_string()
        }
    }

    fn new_fixture() -> Fixture {
        let temp = tempfile::tempdir().expect("temp dir");
        let base = canonical(temp.path().to_str().expect("utf-8 temp dir"));
        let workspace = make_directory(&join(&base, "workspace"));
        let cache_base = make_directory(&join(&base, "user-cache"));
        let private_state = state::create(&workspace, &cache_base).expect("create private state");
        let home = make_directory(&join(&base, "resolved-home"));
        let shell = make_file(&join(&join(&base, "shells"), "shell"), 0o700);
        let options = Options {
            workspace,
            directories: private_state.directories.clone(),
            shell,
            home,
            host_entries: vec!["PATH=".to_string()],
            read_paths: Vec::new(),
            network: Some(NetworkMode::Deny),
        };
        Fixture {
            temp: Some(temp),
            base,
            state: Some(private_state),
            options,
            deps: Deps::new(),
        }
    }

    fn canonical(path: &str) -> String {
        std::fs::canonicalize(path)
            .expect("canonicalize")
            .to_str()
            .expect("utf-8 path")
            .to_string()
    }

    fn make_directory(path: &str) -> String {
        std::fs::create_dir_all(path).expect("create directory");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .expect("chmod directory");
        canonical(path)
    }

    fn make_file(path: &str, mode: u32) -> String {
        std::fs::create_dir_all(dir(path)).expect("create parent");
        std::fs::write(path, b"fixture").expect("write file");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod file");
        canonical(path)
    }

    fn symlink(target: &str, link: &str) {
        std::os::unix::fs::symlink(target, link).expect("symlink");
    }

    fn section<'a>(profile: &'a str, name: &str) -> &'a str {
        let begin = format!("; KITE-DYNAMIC-{name}-BEGIN");
        let end = format!("; KITE-DYNAMIC-{name}-END");
        let start = profile
            .find(&begin)
            .unwrap_or_else(|| panic!("profile section {name} not found"));
        let finish = profile
            .find(&end)
            .unwrap_or_else(|| panic!("profile section {name} not found"));
        assert!(finish >= start, "profile section {name} is inverted");
        &profile[start..finish + end.len()]
    }

    fn quote(path: &str) -> String {
        format!("\"{}\"", path.replace('\\', "\\\\").replace('"', "\\\""))
    }

    fn filter(kind: &str, path: &str) -> String {
        format!("({kind} {})", quote(path))
    }

    fn count(haystack: &str, needle: &str) -> usize {
        haystack.matches(needle).count()
    }

    fn base_name(path: &str) -> &str {
        path.rsplit('/').next().unwrap_or(path)
    }

    #[test]
    fn profile_is_closed_by_default_and_limits_private_writes() {
        let mut fixture = new_fixture();
        let external_directory = make_directory(&join(&join(&fixture.base, "external"), "tools"));
        let external_file = make_file(
            &join(&join(&fixture.base, "external"), "runtime.conf"),
            0o600,
        );
        fixture.options.read_paths = vec![external_directory.clone(), external_file.clone()];

        let profile = fixture.render();
        assert!(
            profile.starts_with("(version 1)\n(deny default)\n"),
            "profile does not begin closed by default:\n{profile}"
        );
        for unfiltered in [
            "(allow file-read*)",
            "(allow file-write*)",
            "(allow network-outbound)",
            "(allow network-inbound)",
        ] {
            assert!(
                !profile.contains(unfiltered),
                "profile contains unfiltered grant {unfiltered}"
            );
        }

        let read_section = section(&profile, "READ");
        let write_section = section(&profile, "WRITE");
        for path in [
            fixture.options.workspace.clone(),
            fixture.directory(&fixture.options.directories.home),
            fixture.directory(&fixture.options.directories.temp),
            fixture.directory(&fixture.options.directories.cache),
        ] {
            let grant = filter("subpath", &path);
            assert!(
                read_section.contains(&grant) && write_section.contains(&grant),
                "writable path {} is not read/write",
                base_name(&path)
            );
        }
        for path in [&external_directory, &external_file] {
            assert!(
                !write_section.contains(&quote(path)),
                "external path {} appears in write rules",
                base_name(path)
            );
        }
        assert!(
            read_section.contains(&filter("subpath", &external_directory))
                && read_section.contains(&filter("literal", &external_file)),
            "external directory/file did not become read-only subpath/literal grants"
        );

        let root = fixture.directory(&fixture.options.directories.root);
        assert!(
            !read_section.contains(&filter("subpath", &root))
                && !write_section.contains(&filter("subpath", &root)),
            "private state parent received a subpath grant"
        );
        assert!(
            !profile.contains(&fixture.state().profiles)
                && !profile.contains(&fixture.state().profile_path),
            "profiles directory or generated profile leaked into child policy"
        );
    }

    #[test]
    fn rejects_every_external_root_overlapping_private_state() {
        type Mutate = fn(&mut Fixture);
        let cases: [(&str, Mutate); 5] = [
            ("explicit ancestor", |fixture| {
                fixture.options.read_paths = vec![fixture.state().root_parent.clone()];
            }),
            ("explicit profiles child", |fixture| {
                fixture.options.read_paths = vec![fixture.state().profiles.clone()];
            }),
            ("fixed automatic ancestor", |fixture| {
                fixture.deps.fixed_paths = vec![AutomaticPath {
                    path: fixture.state().root_parent.clone(),
                    kind: PathKind::Directory,
                }];
            }),
            ("PATH automatic ancestor", |fixture| {
                fixture.options.host_entries =
                    vec![format!("PATH={}", fixture.state().root_parent)];
            }),
            ("developer root ancestor", |fixture| {
                let root = fixture.state().root_parent.clone();
                fixture.deps.developer_root_is(root);
            }),
        ];
        for (name, mutate) in cases {
            let mut fixture = new_fixture();
            mutate(&mut fixture);
            fixture.assert_rejected(name);
        }
    }

    #[test]
    fn rejects_case_alias_containing_private_state() {
        let mut fixture = new_fixture();
        let parent = fixture.state().root_parent.clone();
        let alias = join(&dir(&parent), &base_name(&parent).to_uppercase());
        if std::fs::metadata(&alias).is_err() {
            println!("skipped: the test volume is case-sensitive");
            return;
        }
        fixture.options.read_paths = vec![alias];
        fixture.assert_rejected("a differently-cased state ancestor");
    }

    #[test]
    fn network_rules_are_ip_only() {
        let mut fixture = new_fixture();
        fixture.options.network = Some(NetworkMode::Allow);
        let allow = fixture.render();
        assert!(
            allow.contains("(remote ip)") && allow.contains("(local ip)"),
            "NetworkAllow is missing filtered remote/local IP grants"
        );
        const MDNS_LOOKUP: &str =
            "(allow mach-lookup\n  (global-name \"com.apple.mDNSResponder\"))";
        let allow_network = section(&allow, "NETWORK");
        assert!(
            count(&allow, MDNS_LOOKUP) == 1 && allow_network.contains(MDNS_LOOKUP),
            "NetworkAllow does not contain exactly one marker-scoped mDNS broker lookup"
        );
        const RESOLVER_SOCKET: &str =
            "(remote unix-socket (path \"/private/var/run/mDNSResponder\"))";
        assert!(
            count(&allow, RESOLVER_SOCKET) == 1 && allow_network.contains(RESOLVER_SOCKET),
            "NetworkAllow does not contain exactly one marker-scoped resolver socket grant"
        );
        for forbidden in [
            "(allow network-outbound)",
            "(allow network-inbound)",
            "(allow network-bind)",
            "(remote unix-socket)",
            "(local unix-socket)",
        ] {
            assert!(
                !allow.contains(forbidden),
                "NetworkAllow contains unfiltered or Unix-socket grant {forbidden}"
            );
        }

        fixture.options.network = Some(NetworkMode::Deny);
        let deny = fixture.render();
        assert!(
            !deny.contains("(remote ip)")
                && !deny.contains("(local ip)")
                && !deny.contains("allow network-")
                && !deny.contains("com.apple.mDNSResponder"),
            "NetworkDeny emitted an IP or mDNS broker grant"
        );

        // [`NetworkMode`] has exactly two variants, so the only unset network
        // state is `None`, which is what this rejection guards.
        fixture.options.network = None;
        fixture.assert_rejected("an unset network mode");
    }

    #[test]
    fn grants_the_exact_page_size_compat_sysctl() {
        let profile = new_fixture().render();
        assert!(
            profile.contains("(sysctl-name \"hw.pagesize_compat\")"),
            "profile lacks the exact Darwin page-size compatibility sysctl"
        );
        assert!(
            !profile.contains("sysctl-name-prefix") && !profile.contains("(allow sysctl-read)"),
            "runtime compatibility broadened the exact sysctl allowlist"
        );
    }

    #[test]
    fn denies_system_volumes_read_aliases() {
        let profile = new_fixture().render();
        const CARVE_OUT: &str = "(deny file-read*\n  (subpath \"/System/Volumes\"))";
        assert_eq!(
            count(&profile, CARVE_OUT),
            1,
            "profile does not contain exactly one /System/Volumes read carve-out"
        );
        assert!(
            !profile.contains("(allow file-read*\n  (subpath \"/System/Volumes\"))")
                && !profile.contains("(allow file-read-data\n  (subpath \"/System/Volumes\"))"),
            "profile grants the /System/Volumes alias subtree"
        );
    }

    #[test]
    fn grants_only_the_root_inode_for_process_cwd_resolution() {
        let profile = new_fixture().render();
        assert!(
            profile.contains("(allow file-read-data\n  (literal \"/\"))"),
            "profile lacks the exact root-inode data operation required by process cwd resolution"
        );
        assert!(
            !profile.contains("(allow file-read*\n  (literal \"/\"))")
                && !profile.contains("(subpath \"/\")"),
            "process cwd compatibility broadened the exact root operation or path"
        );
    }

    #[test]
    fn denies_exact_escape_broker_executables() {
        let profile = new_fixture().render();
        for executable in [
            "/usr/bin/open",
            "/usr/bin/osascript",
            "/usr/bin/sandbox-exec",
        ] {
            assert!(
                profile.contains(&filter("literal", executable)),
                "profile lacks exact process-exec denial for {executable}"
            );
        }
        assert!(
            profile.contains("(deny process-exec")
                && !profile.contains("(deny process-exec\n  (subpath \"/usr/bin\"))"),
            "escape-broker process denial is absent or broadened to /usr/bin"
        );
    }

    #[test]
    fn explicitly_denies_apple_events_and_launch_services() {
        let profile = new_fixture().render();
        for rule in ["(deny appleevent-send)", "(deny lsopen)"] {
            assert!(
                profile.contains(rule),
                "profile lacks explicit escape-broker denial {rule}"
            );
        }
        for rule in ["(allow appleevent-send", "(allow lsopen"] {
            assert!(
                !profile.contains(rule),
                "profile grants escape-broker operation {rule}"
            );
        }
    }

    #[test]
    fn template_has_only_reviewed_ipc_and_process_rules() {
        let profile = new_fixture().render();
        for required in [
            "(allow process-fork)",
            "(allow process-exec)",
            "(allow signal (target same-sandbox))",
            "(sysctl-name ",
            "com.apple.system.opendirectoryd.libinfo",
        ] {
            assert!(
                profile.contains(required),
                "profile is missing reviewed rule {required}"
            );
        }
        let lowered = profile.to_lowercase();
        for forbidden in [
            "(allow appleevent",
            "(allow lsopen",
            "launchservices",
            "com.apple.lsd",
            "com.apple.coreservices",
            "com.apple.securityd",
            "com.apple.tccd",
            "com.apple.pboard",
            "process-info",
            "(target self)",
            "mach-task",
            "ipc-posix",
            "sysctl-name-prefix",
            "(with no-sandbox)",
        ] {
            assert!(
                !lowered.contains(forbidden),
                "profile contains forbidden IPC/process rule {forbidden}"
            );
        }
    }

    #[test]
    fn automatic_path_entries_skip_broad_and_sensitive_anchors() {
        let mut fixture = new_fixture();
        let broad: Vec<String> = [
            "/",
            "/Users",
            fixture.options.home.as_str(),
            "/Applications",
            "/Library",
            "/Network",
            "/Volumes",
            "/dev",
            "/private",
            "/private/etc",
            "/private/tmp",
            "/private/var",
            "/usr",
            "/opt",
            "/opt/homebrew",
            "/usr/local",
            "/opt/homebrew/etc",
            "/opt/homebrew/etc/private",
            "/opt/homebrew/var",
            "/usr/local/etc",
            "/usr/local/var/cache",
        ]
        .iter()
        .map(|path| (*path).to_string())
        .collect();
        let narrow: Vec<String> = vec![
            "/opt/homebrew/bin".to_string(),
            "/usr/local/bin".to_string(),
            "/Applications/Xcode.app/Contents/Developer/usr/bin".to_string(),
            "/Library/Developer/CommandLineTools/usr/bin".to_string(),
            join(&join(&fixture.options.home, ".local"), "bin"),
        ];
        let mut all = broad.clone();
        all.extend(narrow.iter().cloned());
        fixture.deps.with_synthetic_directories(&all);
        fixture.options.host_entries =
            vec!["LANG=C".to_string(), format!("PATH={}", all.join(":"))];

        let profile = fixture.render();
        let read_section = section(&profile, "READ").to_string();
        let write_section = section(&profile, "WRITE").to_string();
        for path in &broad {
            assert!(
                !read_section.contains(&filter("subpath", path)),
                "broad/sensitive PATH entry {path} was granted"
            );
        }
        for path in &narrow {
            assert!(
                read_section.contains(&filter("subpath", path)),
                "narrow PATH entry {path} was not granted"
            );
            assert!(
                !write_section.contains(&quote(path)),
                "narrow PATH entry {path} was writable"
            );
        }
    }

    #[test]
    fn automatic_fixed_roots_require_declared_types() {
        let fixture = new_fixture();
        let regular_target = make_file(
            &join(&join(&fixture.base, "automatic-types"), "regular"),
            0o600,
        );
        let directory_target =
            make_directory(&join(&join(&fixture.base, "automatic-types"), "directory"));
        drop(fixture);

        for (name, source, expected, resolved) in [
            (
                "fixed directory resolved to regular file",
                "/usr/bin",
                PathKind::Directory,
                ResolvedPath {
                    path: regular_target,
                    kind: PathKind::Regular,
                    executable: false,
                },
            ),
            (
                "exact runtime file resolved to directory",
                "/private/etc/hosts",
                PathKind::Regular,
                ResolvedPath {
                    path: directory_target,
                    kind: PathKind::Directory,
                    executable: false,
                },
            ),
        ] {
            let mut fixture = new_fixture();
            fixture.deps.fixed_paths = vec![AutomaticPath {
                path: source.to_string(),
                kind: expected,
            }];
            let source = source.to_string();
            fixture
                .deps
                .override_resolve(move |path| (path == source).then(|| Ok(resolved.clone())));
            fixture.assert_rejected(name);
        }
    }

    #[test]
    fn canonical_automatic_targets_cannot_grant_forbidden_anchors() {
        for class in ["fixed", "developer", "PATH"] {
            let mut fixture = new_fixture();
            let forbidden_target = if class == "PATH" {
                fixture.options.home.clone()
            } else {
                make_directory(&join(&join(&fixture.options.home, "sensitive"), class))
            };
            let alias = join(&fixture.base, &format!("automatic-alias-{class}"));
            symlink(&forbidden_target, &alias);
            match class {
                "fixed" => {
                    fixture.deps.fixed_paths = vec![AutomaticPath {
                        path: alias.clone(),
                        kind: PathKind::Directory,
                    }];
                }
                "developer" => fixture.deps.developer_root_is(alias.clone()),
                _ => fixture.options.host_entries = vec![format!("PATH={alias}")],
            }

            let profile = fixture.render();
            let read_data = section(&profile, "READ-DATA");
            assert!(
                !read_data.contains(&quote(&forbidden_target))
                    && !read_data.contains(&quote(&alias)),
                "canonical automatic {class} target inside the user home was granted"
            );
        }
    }

    #[test]
    fn canonical_automatic_symlinks_to_broad_anchor_are_skipped() {
        for class in ["fixed", "developer", "PATH"] {
            let mut fixture = new_fixture();
            let alias = join(&fixture.base, &format!("broad-root-alias-{class}"));
            symlink("/", &alias);
            match class {
                "fixed" => {
                    fixture.deps.fixed_paths = vec![AutomaticPath {
                        path: alias.clone(),
                        kind: PathKind::Directory,
                    }];
                }
                "developer" => fixture.deps.developer_root_is(alias.clone()),
                _ => fixture.options.host_entries = vec![format!("PATH={alias}")],
            }

            let profile = fixture.render();
            assert!(
                !section(&profile, "READ-DATA").contains(&filter("subpath", "/")),
                "canonical automatic {class} symlink target granted the filesystem root"
            );
        }
    }

    #[test]
    fn configured_shell_is_one_canonical_executable_literal() {
        let mut fixture = new_fixture();
        let shell_target = make_file(&join(&join(&fixture.base, "shells"), "real shell"), 0o700);
        let shell_alias = join(&fixture.base, "shell alias");
        symlink(&shell_target, &shell_alias);
        fixture.options.shell = shell_alias.clone();

        let profile = fixture.render();
        let shell_section = section(&profile, "SHELL");
        assert_eq!(
            count(shell_section, &filter("literal", &shell_target)),
            1,
            "canonical shell is not exactly one literal grant:\n{shell_section}"
        );
        assert!(
            !shell_section.contains(&shell_alias)
                && !shell_section.contains(&filter("subpath", &dir(&shell_target))),
            "shell alias or shell parent received a grant"
        );
    }

    #[test]
    fn rejects_configured_shell_overlapping_private_state() {
        for name in ["state root executable", "profiles executable"] {
            let mut fixture = new_fixture();
            let parent = if name == "state root executable" {
                fixture.directory(&fixture.options.directories.root)
            } else {
                fixture.state().profiles.clone()
            };
            fixture.options.shell = make_file(&join(&parent, "private-shell"), 0o700);
            fixture.assert_rejected(name);
        }
    }

    #[test]
    fn rejects_invalid_configured_shell() {
        let fixture = new_fixture();
        let non_executable = make_file(
            &join(&join(&fixture.base, "shells"), "not-executable"),
            0o600,
        );
        let directory = make_directory(&join(&join(&fixture.base, "shells"), "directory"));
        let missing = join(&fixture.base, "missing");
        drop(fixture);

        // `String` cannot hold invalid UTF-8, so a non-UTF-8 path is
        // unrepresentable rather than rejected. Rust's `String` cannot hold
        // invalid UTF-8, so that input is unrepresentable and the rejection it
        // exercises is enforced by the type instead.
        for shell in [
            "relative-shell".to_string(),
            non_executable,
            directory,
            missing,
            "bad\nname".to_string(),
        ] {
            let mut fixture = new_fixture();
            fixture.options.shell = shell.clone();
            fixture.assert_rejected(&format!("invalid shell {shell:?}"));
        }

        let mut fixture = new_fixture();
        fixture.options.shell = "/injected/special".to_string();
        fixture.deps.override_resolve(|path| {
            (path == "/injected/special").then(|| {
                Ok(ResolvedPath {
                    path: "/injected/special".to_string(),
                    kind: PathKind::Special,
                    executable: true,
                })
            })
        });
        fixture.assert_rejected("a special-file shell");
    }

    #[test]
    fn expands_only_resolved_home_tilde_prefix() {
        let mut fixture = new_fixture();
        let tool_directory =
            make_directory(&join(&join(&fixture.options.home, "tool roots"), "bin"));
        fixture.options.read_paths = vec!["~/tool roots/bin".to_string()];
        let profile = fixture.render();
        assert!(
            section(&profile, "READ").contains(&filter("subpath", &tool_directory)),
            "~/ was not expanded from the resolved home"
        );

        for syntax in [
            "$KITE_ROOT",
            "${KITE_ROOT}",
            "$(whoami)",
            "`whoami`",
            "*.tools",
        ] {
            let mut fixture = new_fixture();
            let literal = join(&fixture.base, syntax);
            fixture.options.read_paths = vec![literal.clone()];
            let seen = Arc::new(Mutex::new(String::new()));
            let recorder = seen.clone();
            let target = literal.clone();
            fixture.deps.override_resolve(move |path| {
                if path == target {
                    *recorder.lock().expect("seen") = path.to_string();
                    return Some(Err(ResolveError::NotFound));
                }
                None
            });
            fixture.assert_rejected(&format!("shell syntax {syntax}"));
            assert_eq!(
                *seen.lock().expect("seen"),
                literal,
                "shell syntax was transformed instead of checked literally"
            );
        }
    }

    #[test]
    fn canonicalizes_read_paths_and_rejects_special_or_missing_entries() {
        let mut fixture = new_fixture();
        let directory = make_directory(&join(&join(&fixture.base, "canonical"), "directory"));
        let file = make_file(&join(&join(&fixture.base, "canonical"), "file"), 0o600);
        let directory_alias = join(&fixture.base, "directory-alias");
        let file_alias = join(&fixture.base, "file-alias");
        symlink(&directory, &directory_alias);
        symlink(&file, &file_alias);
        fixture.options.read_paths = vec![directory_alias.clone(), file_alias.clone()];
        let profile = fixture.render();
        let read_section = section(&profile, "READ");
        assert!(
            read_section.contains(&filter("subpath", &directory))
                && read_section.contains(&filter("literal", &file))
                && !read_section.contains(&directory_alias)
                && !read_section.contains(&file_alias),
            "read path aliases were not canonicalized to typed grants"
        );

        let base = fixture.base.clone();
        drop(fixture);
        let cases: Vec<(&str, String, Result<ResolvedPath, ResolveError>)> = vec![
            (
                "relative",
                "relative/path".to_string(),
                Err(ResolveError::Rejected),
            ),
            (
                "missing",
                join(&base, "missing"),
                Err(ResolveError::NotFound),
            ),
            (
                "special",
                "/injected/special".to_string(),
                Ok(ResolvedPath {
                    path: "/injected/special".to_string(),
                    kind: PathKind::Special,
                    executable: false,
                }),
            ),
        ];
        for (name, path, resolved) in cases {
            let mut fixture = new_fixture();
            fixture.options.read_paths = vec![path.clone()];
            let target = path.clone();
            fixture
                .deps
                .override_resolve(move |candidate| (candidate == target).then(|| resolved.clone()));
            fixture.assert_rejected(&format!("invalid read path {name}"));
        }
    }

    #[test]
    fn escapes_dynamic_literals_without_injection() {
        let mut fixture = new_fixture();
        let name = "tools 空格 \\\" ) (allow network-outbound) ; #";
        let path = make_directory(&join(&fixture.base, name));
        fixture.options.read_paths = vec![path.clone()];

        let profile = fixture.render();
        assert!(
            profile.contains(&filter("subpath", &path)),
            "spaces, Unicode, quotes, backslashes, or profile punctuation were not safely represented"
        );
        assert!(
            count(&profile, "(deny default)") == 1
                && !profile.contains("(allow network-outbound)\n"),
            "dynamic path injected an SBPL form"
        );

        // `String` cannot hold invalid UTF-8, so a non-UTF-8 value is
        // unrepresentable rather than rejected. Rust's `String` cannot hold
        // invalid UTF-8, so that input is unrepresentable.
        let base = fixture.base.clone();
        drop(fixture);
        let invalid = [
            join(&base, "line\nbreak"),
            join(&base, "tab\tcontrol"),
            join(&base, "line\u{2028}separator"),
            join(&base, "paragraph\u{2029}separator"),
            join(&base, "nul\0x"),
        ];
        for path in invalid {
            let mut fixture = new_fixture();
            fixture.options.read_paths = vec![path.clone()];
            fixture
                .deps
                .with_synthetic_directories(std::slice::from_ref(&path));
            let error = match fixture.generate() {
                Err(error) => error,
                Ok(profile) => panic!("control/invalid path generated {} bytes", profile.len()),
            };
            let text = error.to_string();
            assert!(
                text.len() <= 128 && !text.contains(&fixture.base) && !text.contains("allow"),
                "profile error is not bounded and source-free: {text:?}"
            );
        }
    }

    #[test]
    fn marker_like_path_text_renders_without_template_reprocessing() {
        let mut fixture = new_fixture();
        let markers = [READ_MARKER, WRITE_MARKER, NETWORK_MARKER, SHELL_MARKER];
        let marker_name = markers.join("-");
        let read_directory =
            make_directory(&join(&join(&fixture.base, "marker-paths"), &marker_name));
        let shell = make_file(
            &join(&join(&fixture.base, "marker-shells"), &marker_name),
            0o700,
        );
        fixture.options.read_paths = vec![read_directory.clone()];
        fixture.options.shell = shell.clone();

        let first = fixture.render();
        let second = fixture.render();
        assert_eq!(
            first, second,
            "marker-like path text rendered nondeterministically"
        );
        assert!(
            section(&first, "READ-DATA").contains(&filter("subpath", &read_directory))
                && section(&first, "SHELL").contains(&filter("literal", &shell)),
            "marker-like filenames were not rendered as escaped literal path text"
        );
        for marker in markers {
            assert_eq!(
                count(&first, marker),
                2,
                "marker-like text {marker} does not appear exactly twice"
            );
        }
        for name in ["READ", "WRITE", "NETWORK", "SHELL"] {
            assert!(
                count(&first, &format!("; KITE-DYNAMIC-{name}-BEGIN")) == 1
                    && count(&first, &format!("; KITE-DYNAMIC-{name}-END")) == 1,
                "marker-like text changed the {name} section structure"
            );
        }
        assert!(
            count(&first, "(version 1)") == 1
                && count(&first, "(deny default)") == 1
                && !first.contains("(allow network-outbound)\n"),
            "marker-like path text injected or changed fixed profile forms"
        );
    }

    #[test]
    fn ancestor_metadata_does_not_grant_ancestor_contents() {
        let mut fixture = new_fixture();
        let ancestor = make_directory(&join(&fixture.base, "metadata-only"));
        let approved = make_directory(&join(&join(&ancestor, "approved"), "bin"));
        fixture.options.read_paths = vec![approved.clone()];
        let profile = fixture.render();

        let metadata = section(&profile, "METADATA");
        let read_data = section(&profile, "READ-DATA");
        assert!(
            metadata.contains(&filter("literal", &ancestor)),
            "approved root ancestor lacks traversal metadata"
        );
        assert!(
            !read_data.contains(&quote(&ancestor))
                && read_data.contains(&filter("subpath", &approved)),
            "ancestor content was granted or approved root content was omitted"
        );
    }

    #[test]
    fn dynamic_root_limits_apply_after_deterministic_collapse() {
        let mut fixture = new_fixture();
        let paths: Vec<String> = (0..129)
            .map(|index| format!("/dynamic/tool-{index:03}"))
            .collect();
        fixture.options.read_paths = paths.clone();
        fixture.deps.with_synthetic_directories(&paths);
        fixture.assert_rejected("129 effective roots");
        drop(fixture);

        let mut fixture = new_fixture();
        let paths: Vec<String> = ["a", "b", "c"]
            .iter()
            .map(|letter| format!("/dynamic/{letter}-{}", letter.repeat(11_000)))
            .collect();
        fixture.options.read_paths = paths.clone();
        fixture.deps.with_synthetic_directories(&paths);
        fixture.assert_rejected("oversized roots");
        drop(fixture);

        let mut fixture = new_fixture();
        let parent = "/dynamic/tools".to_string();
        let mut paths = vec![parent.clone(), parent.clone()];
        for index in 0..129 {
            paths.push(format!("{parent}/tool-{index:03}/bin"));
        }
        fixture.options.read_paths = paths.clone();
        fixture.deps.with_synthetic_directories(&paths);
        let profile = fixture.render();
        let read_data = section(&profile, "READ-DATA");
        assert!(
            count(read_data, &filter("subpath", &parent)) == 1 && !read_data.contains("tool-000"),
            "nested/duplicate roots were not collapsed to their parent"
        );
        drop(fixture);

        let mut fixture = new_fixture();
        let paths: Vec<String> = (0..128)
            .map(|index| format!("/dynamic-{index:03}/bin"))
            .collect();
        fixture.options.read_paths = paths.clone();
        fixture.deps.with_synthetic_directories(&paths);
        let _ = fixture.render();
    }

    #[test]
    fn equivalent_semantic_inputs_render_byte_identically() {
        let fixture = new_fixture();
        let parent = make_directory(&join(&join(&fixture.base, "deterministic"), "tools"));
        let child = make_directory(&join(&join(&parent, "nested"), "bin"));
        let alias = join(&fixture.base, "deterministic-alias");
        symlink(&parent, &alias);
        let path_one = make_directory(&join(&fixture.base, "path-one"));
        let path_two = make_directory(&join(&fixture.base, "path-two"));

        let mut first_options = fixture.options.clone();
        first_options.read_paths = vec![child.clone(), alias, parent.clone(), child];
        first_options.host_entries = vec![format!("PATH={path_one}:{path_two}")];
        let first_deps = Deps {
            resolve: Box::new(resolve_path),
            fixed_paths: vec![
                AutomaticPath {
                    path: path_two.clone(),
                    kind: PathKind::Directory,
                },
                AutomaticPath {
                    path: path_one.clone(),
                    kind: PathKind::Directory,
                },
            ],
            developer_root: Box::new(|| Ok(None)),
        };

        let mut second_options = fixture.options.clone();
        second_options.read_paths = vec![parent];
        second_options.host_entries =
            vec!["LANG=C".to_string(), format!("PATH={path_two}:{path_one}")];
        let second_deps = Deps {
            resolve: Box::new(resolve_path),
            fixed_paths: vec![
                AutomaticPath {
                    path: path_one.clone(),
                    kind: PathKind::Directory,
                },
                AutomaticPath {
                    path: path_two,
                    kind: PathKind::Directory,
                },
                AutomaticPath {
                    path: path_one,
                    kind: PathKind::Directory,
                },
            ],
            developer_root: Box::new(|| Ok(None)),
        };

        let first = generate_with(&first_options, &first_deps.view()).expect("first profile");
        let second = generate_with(&second_options, &second_deps.view()).expect("second profile");
        assert_eq!(
            first, second,
            "semantically equal roots rendered different profile bytes"
        );
    }

    #[test]
    fn grants_etc_symlink_traversal_metadata() {
        let profile = new_fixture().render();
        const EXACT_METADATA_RULE: &str = "(allow file-read-metadata\n  (literal \"/etc\"))";
        assert!(
            profile.contains(EXACT_METADATA_RULE),
            "profile lacks the exact metadata-only /etc symlink traversal grant"
        );
        for parent in ["/etc", "/private/etc"] {
            assert!(
                !profile.contains(&filter("subpath", parent)),
                "/etc traversal compatibility granted sensitive parent subtree {parent}"
            );
        }
        assert!(
            !profile.contains(&format!(
                "(allow file-read*\n  {})",
                filter("literal", "/etc")
            )),
            "/etc symlink traversal grant is not metadata-only"
        );
    }

    #[test]
    fn network_allow_grants_certificate_trust_broker() {
        let mut fixture = new_fixture();
        fixture.options.network = Some(NetworkMode::Allow);
        let allow = fixture.render();
        const TRUST_LOOKUP: &str = "(allow mach-lookup\n  (global-name \"com.apple.trustd\")\n  (global-name \"com.apple.trustd.agent\"))";
        let allow_network = section(&allow, "NETWORK");
        assert!(
            count(&allow, TRUST_LOOKUP) == 1 && allow_network.contains(TRUST_LOOKUP),
            "NetworkAllow does not contain exactly one marker-scoped trustd broker lookup"
        );

        fixture.options.network = Some(NetworkMode::Deny);
        let deny = fixture.render();
        assert!(
            !deny.contains("com.apple.trustd"),
            "NetworkDeny emitted a trustd broker grant"
        );
    }

    #[test]
    fn xcode_selector_uses_only_exact_read_and_traversal_metadata() {
        let profile = new_fixture().render();
        const EXACT_METADATA_RULE: &str = "(allow file-read-metadata\n  (literal \"/var\")\n  (literal \"/var/select\")\n  (literal \"/private/var/select\")\n  (literal \"/var/select/developer_dir\")\n  (literal \"/private/var/select/developer_dir\"))";
        assert!(
            profile.contains(EXACT_METADATA_RULE),
            "Git/xcrun compatibility lacks the reviewed metadata-only selector rule"
        );
        for selector in [
            "/var/select/developer_dir",
            "/private/var/select/developer_dir",
        ] {
            assert_eq!(
                count(&profile, &filter("literal", selector)),
                1,
                "Git/xcrun compatibility lacks one exact selector grant for {selector}"
            );
        }
        for ancestor in ["/var", "/var/select", "/private/var/select"] {
            assert!(
                profile.contains(&filter("literal", ancestor)),
                "developer selector lacks exact traversal metadata for {ancestor}"
            );
        }
        for parent in ["/var", "/var/select", "/private/var", "/private/var/select"] {
            assert!(
                !profile.contains(&filter("subpath", parent)),
                "developer selector compatibility granted sensitive parent subtree {parent}"
            );
        }
    }

    #[test]
    fn bin_sh_selector_is_one_exact_read_only_runtime_file() {
        let profile = new_fixture().render();
        const SELECTOR: &str = "/private/var/select/sh";
        assert!(
            count(&profile, &filter("literal", SELECTOR)) == 1
                && profile.contains(&format!(
                    "(allow file-read-metadata\n  {}",
                    filter("literal", SELECTOR)
                )),
            "/bin/sh compatibility lacks one exact metadata-only shell-selector grant"
        );
        assert!(
            !profile.contains(&format!(
                "(allow file-read*\n  {}",
                filter("literal", SELECTOR)
            )) && !profile.contains(&filter("subpath", "/private/var/select"))
                && !profile.contains(&filter("subpath", "/private/var")),
            "/bin/sh selector compatibility granted a writable/sensitive parent subtree"
        );
    }

    #[test]
    fn reviewed_automatic_roots_are_narrow() {
        for required in [
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
            "/opt/homebrew/lib",
            "/opt/homebrew/Cellar",
            "/opt/homebrew/opt",
            "/usr/local/bin",
            "/usr/local/lib",
            "/usr/local/Homebrew",
        ] {
            assert!(
                REVIEWED_AUTOMATIC_PATHS.contains(&required),
                "reviewed automatic roots omit {required}"
            );
        }
        for forbidden in [
            "/",
            "/Users",
            "/Applications",
            "/Library",
            "/Network",
            "/Volumes",
            "/dev",
            "/private",
            "/private/etc",
            "/private/tmp",
            "/private/var",
            "/usr",
            "/opt",
            "/opt/homebrew",
            "/opt/homebrew/etc",
            "/opt/homebrew/var",
            "/usr/local",
            "/usr/local/etc",
            "/usr/local/var",
        ] {
            assert!(
                !REVIEWED_AUTOMATIC_PATHS.contains(&forbidden),
                "reviewed automatic roots contain broad/sensitive {forbidden}"
            );
        }
        for path in REVIEWED_RUNTIME_FILES {
            assert!(
                path.starts_with("/private/etc/") && *path != "/private/etc",
                "runtime compatibility path is not one exact /private/etc file: {path}"
            );
        }
    }

    #[test]
    fn embedded_template_has_four_unique_markers() {
        for marker in [READ_MARKER, WRITE_MARKER, NETWORK_MARKER, SHELL_MARKER] {
            assert_eq!(
                count(super::super::TEMPLATE, marker),
                1,
                "template marker {marker} does not appear exactly once"
            );
        }
    }

    #[test]
    fn discovery_errors_remain_bounded() {
        let mut fixture = new_fixture();
        let private_diagnostic = join(&fixture.base, "private-discovery-value");
        fixture.deps.developer_root = Box::new(|| Err(Rejected));
        let error = match fixture.generate() {
            Err(error) => error,
            Ok(profile) => panic!("discovery failure generated {} bytes", profile.len()),
        };
        let text = error.to_string();
        assert!(
            text.len() <= 128
                && !text.contains(&private_diagnostic)
                && !text.contains(&fixture.base),
            "profile error leaks discovery details: {text:?}"
        );
    }
}
