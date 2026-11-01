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

mod paths;
mod roots;
mod rules;
pub(crate) use paths::*;
use roots::*;
use rules::*;

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
    /// `[sandbox] deny_read_paths`: additions to the built-in read denylist.
    pub(crate) deny_read_paths: Vec<String>,
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

#[cfg(test)]
mod tests;
