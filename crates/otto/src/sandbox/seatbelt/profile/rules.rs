//! Rendering of the read, write, network and shell rule sections.

use super::*;

pub(super) fn render_read_rules(
    metadata: &[String],
    writable: &[String],
    roots: &[ResolvedPath],
) -> Result<String, Rejected> {
    let mut builder = String::new();
    builder.push_str("; OTTO-DYNAMIC-READ-BEGIN\n");
    builder.push_str("; OTTO-DYNAMIC-METADATA-BEGIN\n");
    let literals: Vec<Filter> = metadata
        .iter()
        .map(|path| Filter::Literal(path.clone()))
        .collect();
    write_rule(&mut builder, "allow", "file-read-metadata", &literals, None)?;
    builder.push_str("; OTTO-DYNAMIC-METADATA-END\n");
    builder.push_str("; OTTO-DYNAMIC-READ-DATA-BEGIN\n");
    let mut filters: Vec<Filter> = Vec::with_capacity(writable.len() + roots.len());
    for path in writable {
        filters.push(Filter::Subpath(path.clone()));
    }
    for root in roots {
        filters.push(if root.kind == PathKind::Directory {
            Filter::Subpath(root.path.clone())
        } else {
            Filter::Literal(root.path.clone())
        });
    }
    write_rule(&mut builder, "allow", "file-read*", &filters, None)?;
    builder.push_str("; OTTO-DYNAMIC-READ-DATA-END\n");
    builder.push_str("; OTTO-DYNAMIC-READ-END");
    Ok(builder)
}

pub(super) fn render_write_rules(
    writable: &[String],
    workspaces: &[String],
    deny_message: &str,
) -> Result<String, Rejected> {
    let mut builder = String::new();
    builder.push_str("; OTTO-DYNAMIC-WRITE-BEGIN\n");
    let filters: Vec<Filter> = writable
        .iter()
        .map(|path| Filter::Subpath(path.clone()))
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
        .flat_map(|workspace| protected_workspace_filters(workspace))
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

/// The write-denied filters of one workspace: the fixed git paths, then the
/// git directory program settings below `.git/modules` and `.git/worktrees`,
/// then the workspace identity files.
fn protected_workspace_filters(workspace: &str) -> Vec<Filter> {
    let literal_or_subpath = |(name, subpath): &(&str, bool)| {
        let path = join(workspace, name);
        if *subpath {
            Filter::Subpath(path)
        } else {
            Filter::Literal(path)
        }
    };
    let mut filters: Vec<Filter> = crate::sandbox::GIT_READ_ONLY
        .iter()
        .map(literal_or_subpath)
        .collect();
    filters.extend(GIT_DIR_PROGRAM_SETTINGS.iter().map(|(prefix, pattern)| {
        Filter::SubpathMatching {
            root: join(workspace, prefix),
            pattern,
        }
    }));
    filters.extend(
        crate::workspace::READ_ONLY_PATHS
            .iter()
            .map(literal_or_subpath),
    );
    filters
}

pub(super) fn render_network_rules(network: NetworkMode) -> String {
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

pub(super) fn render_shell_rule(shell: &str) -> Result<String, Rejected> {
    let mut builder = String::new();
    builder.push_str("; OTTO-DYNAMIC-SHELL-BEGIN\n");
    write_rule(
        &mut builder,
        "allow",
        "file-read*",
        &[Filter::Literal(shell.to_string())],
        None,
    )?;
    builder.push_str("; OTTO-DYNAMIC-SHELL-END");
    Ok(builder)
}

/// Files below a git directory that git reads as program settings or hooks
/// outside the sandbox: `(prefix, regex)`. `prefix` is relative to the
/// workspace and becomes a `subpath`; `regex` is a constant Seatbelt regex
/// matched against the full path, and holds no workspace text.
///
/// - `.git/modules`, at any depth: files named `config`, `config.worktree` and
///   `commondir`, and every `hooks` directory with everything below it. These
///   belong to submodule git directories, whose names may contain `/`.
/// - `.git/worktrees`, at any depth: files named `config.worktree`.
///
/// `gitdir` and `commondir` below `.git/worktrees` stay writable because
/// `git worktree add` creates them, and nested `<dir>/.git` repositories are
/// not covered.
pub(super) const GIT_DIR_PROGRAM_SETTINGS: [(&str, &str); 3] = [
    (".git/modules", r"/(config|config\.worktree|commondir)$"),
    (".git/modules", r"/hooks(/|$)"),
    (".git/worktrees", r"/config\.worktree$"),
];

#[derive(Debug, Clone, PartialEq, Eq)]
enum Filter {
    /// `(literal "<path>")`
    Literal(String),
    /// `(subpath "<path>")`
    Subpath(String),
    /// `(require-all (subpath "<root>") (regex #"<pattern>"))`. The pattern is
    /// a constant, so no untrusted text is ever embedded in a regex.
    SubpathMatching { root: String, pattern: &'static str },
}

impl Filter {
    fn render(&self) -> Result<String, Rejected> {
        Ok(match self {
            Self::Literal(path) => format!("(literal {})", string_literal(path)?),
            Self::Subpath(path) => format!("(subpath {})", string_literal(path)?),
            Self::SubpathMatching { root, pattern } => {
                debug_assert!(!pattern.contains(['"', '\n']));
                format!(
                    "(require-all (subpath {}) (regex #\"{pattern}\"))",
                    string_literal(root)?
                )
            }
        })
    }
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
        builder.push_str("  ");
        builder.push_str(&filter.render()?);
        builder.push('\n');
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
pub(super) fn string_literal(value: &str) -> Result<String, Rejected> {
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
