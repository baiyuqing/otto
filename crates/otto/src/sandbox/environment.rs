//! Classification of host environment variables for a sandboxed child.
//!
//! Three questions are decided here for every host variable: whether the child
//! may see it, whether its value must be redacted from transcripts, and whether
//! the classification can be proven complete.
//!
//! Ownership: `resolve_environment` copies everything it needs out of its
//! options, so an `EnvironmentSnapshot` shares no storage with the caller and
//! later mutation of the inputs cannot change it.
//!
//! Concurrency and cancellation: every function here is synchronous, does a
//! bounded amount of filesystem work, and holds no shared state. There is
//! nothing to cancel.
//!
//! Errors: the only failure is [`Error::EnvironmentUnsafe`], deliberately
//! detail-free, because the reason a variable was rejected can itself disclose
//! a credential. A rejection still carries the bounded redaction set salvaged
//! before the failure, so a caller that must abort can still redact what it
//! already saw.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::Path;

use super::{Error, PrivateDirectories};
use otto_core::safetext::SecretCollector;
use otto_platform::fsops::gopath;
use otto_platform::urlprivacy;

/// The per-cache subdirectories a confined child is pointed at. Created at mode
/// 0700 inside `PrivateDirectories::cache`.
const DERIVED_CACHE_NAMES: [&str; 7] = ["cargo", "go-build", "go-mod", "npm", "pip", "uv", "xdg"];

/// What [`resolve_environment`] is given.
///
/// `host_entries` are raw `NAME=VALUE` byte strings as the host presents them,
/// because a real environment may hold bytes that are not UTF-8 and those
/// entries must be rejected rather than repaired. `provider_names` and
/// `allow_names` come from configuration, which is UTF-8 by construction, so
/// they need no UTF-8 check.
#[derive(Debug, Clone, Default)]
pub(crate) struct EnvironmentOptions {
    pub host_entries: Vec<Vec<u8>>,
    pub provider_names: Vec<String>,
    pub allow_names: Vec<String>,
    pub private_directories: Option<PrivateDirectories>,
}

/// The environment a sandboxed child will receive, plus the values that must
/// never appear in model-visible text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct EnvironmentSnapshot {
    entries: Option<Vec<String>>,
    redactions: Vec<String>,
    redactions_complete: bool,
}

impl EnvironmentSnapshot {
    /// The child's `NAME=VALUE` entries, sorted by name. `None` when resolution
    /// failed: a rejected environment has no usable entries at all.
    pub fn entries(&self) -> Option<&[String]> {
        self.entries.as_deref()
    }

    /// The values a redactor must hide, longest first then lexically.
    pub fn redaction_values(&self) -> &[String] {
        &self.redactions
    }

    /// Whether the redaction set is provably complete. A `false` here means the
    /// caller must suppress the child's output rather than redact it.
    pub(crate) fn redactions_complete(&self) -> bool {
        self.redactions_complete
    }
}

/// A rejected environment, carrying the bounded redactions salvaged before the
/// rejection so the caller can still hide what it collected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RejectedEnvironment {
    snapshot: EnvironmentSnapshot,
}

impl RejectedEnvironment {
    /// The partial snapshot. Its `entries` is always `None`.
    pub fn snapshot(&self) -> &EnvironmentSnapshot {
        &self.snapshot
    }
}

impl std::fmt::Display for RejectedEnvironment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&Error::EnvironmentUnsafe, f)
    }
}

impl std::error::Error for RejectedEnvironment {}

impl From<RejectedEnvironment> for Error {
    fn from(_: RejectedEnvironment) -> Self {
        Error::EnvironmentUnsafe
    }
}

/// Decides the child's environment and the redaction set for it.
///
/// A name is *ordinary* (passed through), *automatic* (withheld unless
/// explicitly allowed, value always redacted), or *non-restorable* (withheld
/// unconditionally, value always redacted). When `private_directories` is set,
/// the per-session `TMPDIR`, `ZDOTDIR`, zsh's `TMPPREFIX` and cache variables
/// replace whatever the host had, `HOME` keeps the host value, and the derived
/// cache directories are created at mode 0700.
///
/// Returns [`RejectedEnvironment`] when any input is unsafe. That path never
/// creates directories: the bounds are enforced first.
pub(crate) fn resolve_environment(
    options: &EnvironmentOptions,
) -> Result<EnvironmentSnapshot, RejectedEnvironment> {
    let mut collector = SecretCollector::new();
    let mut unsafe_input = false;
    let mut redactions_complete = true;

    let mut provider_names: HashSet<String> = HashSet::new();
    for name in &options.provider_names {
        if name.is_empty() {
            continue;
        }
        if !super::valid_environment_name(name) {
            unsafe_input = true;
            redactions_complete = false;
            continue;
        }
        provider_names.insert(name.to_uppercase());
    }
    let mut allow_names: HashSet<String> = HashSet::new();
    for name in &options.allow_names {
        if !super::valid_environment_name(name) || !allow_names.insert(name.clone()) {
            unsafe_input = true;
        }
    }

    let mut host: BTreeMap<String, String> = BTreeMap::new();
    for entry in &options.host_entries {
        let Some(entry) = std::str::from_utf8(entry)
            .ok()
            .filter(|e| !e.contains('\0'))
        else {
            unsafe_input = true;
            redactions_complete = false;
            continue;
        };
        let Some((name, value)) = entry
            .split_once('=')
            .filter(|(name, _)| super::valid_environment_name(name))
        else {
            unsafe_input = true;
            redactions_complete = false;
            continue;
        };

        let (proxy_values, proxy_ambiguous) = proxy_userinfo_redactions(name, value);
        if proxy_ambiguous {
            unsafe_input = true;
            redactions_complete = false;
        }
        for proxy_value in &proxy_values {
            if !collector.add_form(proxy_value) {
                unsafe_input = true;
                redactions_complete = false;
                break;
            }
        }
        if classify_environment_name(name, &provider_names) != Classification::Ordinary
            && !collector.add(value)
        {
            unsafe_input = true;
            redactions_complete = false;
        }
        host.insert(name.to_owned(), value.to_owned());
    }

    let directories = options.private_directories.clone();
    if let Some(directories) = &directories {
        for private_path in [&directories.root, &directories.temp, &directories.cache] {
            if !add_private_environment_redaction(&mut collector, private_path) {
                unsafe_input = true;
                redactions_complete = false;
            }
        }
    }
    if unsafe_input {
        return Err(rejected(&collector, redactions_complete));
    }

    let mut resolved: BTreeMap<String, String> = BTreeMap::new();
    for (name, value) in &host {
        let classification = classify_environment_name(name, &provider_names);
        if classification == Classification::Ordinary
            || (classification == Classification::Automatic && allow_names.contains(name))
        {
            resolved.insert(name.clone(), value.clone());
        }
    }

    if let Some(directories) = &directories {
        if prepare_private_environment(directories).is_err() {
            return Err(rejected(&collector, redactions_complete));
        }
        let cache = |name: &str| {
            display_path(&gopath::path_from(gopath::join(&[
                gopath::bytes(&directories.cache),
                name.as_bytes(),
            ])))
        };
        for (name, value) in [
            // `HOME` is not replaced: the host value reaches the child. zsh
            // reads its startup files from `ZDOTDIR`, so pointing it at the
            // private `tmp` directory keeps `~/.zshenv`, `~/.zprofile` and
            // `~/.zshrc` from being sourced.
            ("ZDOTDIR", display_path(&directories.temp)),
            ("TMPDIR", display_path(&directories.temp)),
            ("TMP", display_path(&directories.temp)),
            ("TEMP", display_path(&directories.temp)),
            // zsh names here-document and `=(...)` temporary files after
            // `TMPPREFIX`, not `TMPDIR`; its default `/tmp/zsh` is outside the
            // sandbox's writable set, so every heredoc would fail.
            ("TMPPREFIX", display_path(&directories.temp.join("zsh"))),
            ("XDG_CACHE_HOME", cache("xdg")),
            ("GOCACHE", cache("go-build")),
            ("GOMODCACHE", cache("go-mod")),
            ("NPM_CONFIG_CACHE", cache("npm")),
            ("PIP_CACHE_DIR", cache("pip")),
            ("UV_CACHE_DIR", cache("uv")),
            // Cargo would otherwise read `~/.cargo/config.toml` and
            // `~/.cargo/credentials.toml` through the real `HOME`.
            ("CARGO_HOME", cache("cargo")),
        ] {
            resolved.insert(name.to_owned(), value);
        }
    }

    Ok(EnvironmentSnapshot {
        entries: Some(
            resolved
                .iter()
                .map(|(name, value)| format!("{name}={value}"))
                .collect(),
        ),
        redactions: collector.values(),
        redactions_complete: true,
    })
}

fn rejected(collector: &SecretCollector, complete: bool) -> RejectedEnvironment {
    RejectedEnvironment {
        snapshot: EnvironmentSnapshot {
            entries: None,
            redactions: collector.values(),
            redactions_complete: complete,
        },
    }
}

/// A path as the child will see it. Non-UTF-8 paths are already refused by
/// [`valid_private_directory_path`], so the lossy conversion never fires on a
/// path that reaches the child.
fn display_path(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// How a variable name is treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Classification {
    /// Passed through to the child untouched.
    Ordinary,
    /// Withheld unless explicitly allowed by exact name; value always redacted.
    Automatic,
    /// Never given to the child, whatever the allow list says.
    NonRestorable,
}

fn classify_environment_name(name: &str, provider_names: &HashSet<String>) -> Classification {
    let upper = name.to_uppercase();
    if upper == "OTTO_API_KEY"
        || provider_names.contains(&upper)
        || upper.starts_with("DYLD_")
        || upper.starts_with("LD_")
        || is_non_restorable_environment_name(&upper)
    {
        return Classification::NonRestorable;
    }
    if has_sensitive_environment_suffix(&upper) || is_fixed_sensitive_environment_name(&upper) {
        return Classification::Automatic;
    }
    Classification::Ordinary
}

fn is_non_restorable_environment_name(upper: &str) -> bool {
    if upper == "OTTO_SERVER_TOKEN" {
        return true;
    }
    upper == "OTTO_SANDBOX"
        || upper.starts_with("OTTO_SANDBOX_")
        || matches!(
            upper,
            "BASH_ENV"
                | "ENV"
                | "ZDOTDIR"
                | "PROMPT_COMMAND"
                | "CDPATH"
                | "SHELLOPTS"
                | "BASHOPTS"
                | "SSH_AUTH_SOCK"
                | "DOCKER_HOST"
                | "CONTAINER_HOST"
        )
}

fn has_sensitive_environment_suffix(upper: &str) -> bool {
    [
        "_TOKEN",
        "_SECRET",
        "_PASSWORD",
        "_PASSWD",
        "_API_KEY",
        "_ACCESS_KEY",
        "_PRIVATE_KEY",
        "_CREDENTIAL",
        "_CREDENTIALS",
    ]
    .iter()
    .any(|suffix| upper.ends_with(suffix))
}

fn is_fixed_sensitive_environment_name(upper: &str) -> bool {
    matches!(
        upper,
        "TOKEN"
            | "SECRET"
            | "PASSWORD"
            | "PASSWD"
            | "API_KEY"
            | "ACCESS_KEY"
            | "PRIVATE_KEY"
            | "CREDENTIAL"
            | "CREDENTIALS"
            | "AWS_ACCESS_KEY_ID"
            | "AWS_SECRET_KEY"
            | "AWS_SHARED_CREDENTIALS_FILE"
            | "AWS_WEB_IDENTITY_TOKEN_FILE"
            | "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI"
            | "AWS_CONTAINER_CREDENTIALS_FULL_URI"
            | "AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE"
            | "AZURE_CLIENT_ID"
            | "AZURE_TENANT_ID"
            | "AZURE_CLIENT_CERTIFICATE_PATH"
            | "AZURE_FEDERATED_TOKEN_FILE"
            | "ARM_CLIENT_ID"
            | "ARM_TENANT_ID"
            | "ARM_CLIENT_CERTIFICATE_PATH"
            | "ARM_OIDC_REQUEST_TOKEN"
            | "ARM_OIDC_REQUEST_URL"
            | "MSI_ENDPOINT"
            | "IDENTITY_ENDPOINT"
            | "IDENTITY_HEADER"
            | "IMDS_ENDPOINT"
            | "GOOGLE_APPLICATION_CREDENTIALS"
            | "GOOGLE_APPLICATION_CREDENTIALS_JSON"
            | "GOOGLE_CREDENTIALS"
            | "GCP_CREDENTIALS"
            | "GCP_SERVICE_ACCOUNT"
            | "GCP_SERVICE_ACCOUNT_KEY"
            | "GCLOUD_SERVICE_KEY"
            | "CLOUDSDK_AUTH_CREDENTIAL_FILE_OVERRIDE"
            | "GITHUB_PAT"
            | "GITLAB_CI_JOB_JWT"
            | "GITLAB_CI_JOB_JWT_V2"
            | "NPM_CONFIG__AUTH"
            | "NPM_CONFIG__AUTHTOKEN"
            | "NPM_CONFIG_AUTH"
            | "NPM_CONFIG_AUTHTOKEN"
            | "REGISTRY_AUTH"
            | "REGISTRY_AUTH_FILE"
            | "CONTAINER_AUTH_FILE"
            | "DOCKER_AUTH_CONFIG"
            | "DOCKER_CONFIG"
            | "DOCKER_CERT_PATH"
            | "DOCKER_HOST"
            | "CONTAINER_HOST"
            | "CI_JOB_JWT"
            | "CI_JOB_JWT_V2"
            | "CI_DEPLOY_USER"
            | "SYSTEM_ACCESSTOKEN"
            | "ACTIONS_ID_TOKEN_REQUEST_URL"
            | "BUILDKITE_AGENT_META_DATA_AWS_ROLE_ARN"
            | "AUTH"
            | "AUTHORIZATION"
            | "HTTP_AUTHORIZATION"
            | "PROXY_AUTHORIZATION"
            | "COOKIE"
            | "COOKIES"
            | "HTTP_COOKIE"
            | "SET_COOKIE"
            | "SSH_AUTH_SOCK"
    )
}

fn add_private_environment_redaction(collector: &mut SecretCollector, path: &Path) -> bool {
    if path.as_os_str().is_empty() {
        return true;
    }
    let Some(text) = path.to_str() else {
        return false;
    };
    if text.contains('\0') {
        return false;
    }
    collector.add(text)
}

/// Userinfo forms hidden inside a proxy setting, plus whether extraction was
/// ambiguous. Bypass lists (`NO_PROXY`) are never scanned: their `@` is a host
/// pattern, not a credential.
fn proxy_userinfo_redactions(name: &str, value: &str) -> (Vec<String>, bool) {
    if value.is_empty() || !is_proxy_environment_name(name) || name.eq_ignore_ascii_case("NO_PROXY")
    {
        return (Vec::new(), false);
    }
    urlprivacy::userinfo_forms(value.as_bytes())
}

fn is_proxy_environment_name(name: &str) -> bool {
    let upper = name.to_uppercase();
    upper == "PROXY" || upper.ends_with("_PROXY")
}

/// Verifies the three base directories and creates the derived cache
/// directories, all owner-only and none reached through a symlink.
fn prepare_private_environment(directories: &PrivateDirectories) -> Result<(), Error> {
    for path in [&directories.root, &directories.temp, &directories.cache] {
        if !valid_private_directory_path(path) || verify_private_directory(path).is_err() {
            return Err(Error::EnvironmentUnsafe);
        }
    }
    for name in DERIVED_CACHE_NAMES {
        let path = gopath::path_from(gopath::join(&[
            gopath::bytes(&directories.cache),
            name.as_bytes(),
        ]));
        ensure_private_directory(&path)?;
    }
    Ok(())
}

fn valid_private_directory_path(path: &Path) -> bool {
    let Some(text) = path.to_str() else {
        return false;
    };
    !text.is_empty()
        && !text.contains('\0')
        && path.is_absolute()
        && gopath::clean(text.as_bytes()) == text.as_bytes()
}

/// Creates `path` at mode 0700 if absent, then proves the directory that is
/// there now is the one just created, is owned by this user, and is not a
/// symlink. `mkdir` is subject to the umask, so the mode is re-applied through
/// the open descriptor rather than trusted from creation.
fn ensure_private_directory(path: &Path) -> Result<(), Error> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            return verify_private_directory(path);
        }
        Err(_) => return Err(Error::EnvironmentUnsafe),
        Ok(()) => {}
    }

    let entry_info = fs::symlink_metadata(path).map_err(|_| Error::EnvironmentUnsafe)?;
    if entry_info.file_type().is_symlink() || !entry_info.is_dir() {
        return Err(Error::EnvironmentUnsafe);
    }
    let file = fs::File::open(path).map_err(|_| Error::EnvironmentUnsafe)?;
    let opened_info = file.metadata().map_err(|_| Error::EnvironmentUnsafe)?;
    if !same_file(&entry_info, &opened_info) {
        return Err(Error::EnvironmentUnsafe);
    }
    file.set_permissions(fs::Permissions::from_mode(0o700))
        .map_err(|_| Error::EnvironmentUnsafe)?;
    let opened_info = file.metadata().map_err(|_| Error::EnvironmentUnsafe)?;
    if !secure_private_directory_info(&opened_info) {
        return Err(Error::EnvironmentUnsafe);
    }
    let final_info = fs::symlink_metadata(path).map_err(|_| Error::EnvironmentUnsafe)?;
    if !same_file(&opened_info, &final_info) || final_info.file_type().is_symlink() {
        return Err(Error::EnvironmentUnsafe);
    }
    Ok(())
}

/// Proves an existing directory is owner-only, owned by this user, and reached
/// without traversing a symlink.
fn verify_private_directory(path: &Path) -> Result<(), Error> {
    let entry_info = fs::symlink_metadata(path).map_err(|_| Error::EnvironmentUnsafe)?;
    if entry_info.file_type().is_symlink() || !secure_private_directory_info(&entry_info) {
        return Err(Error::EnvironmentUnsafe);
    }
    let file = fs::File::open(path).map_err(|_| Error::EnvironmentUnsafe)?;
    let opened_info = file.metadata().map_err(|_| Error::EnvironmentUnsafe)?;
    if !same_file(&entry_info, &opened_info) || !secure_private_directory_info(&opened_info) {
        return Err(Error::EnvironmentUnsafe);
    }
    Ok(())
}

fn same_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino()
}

/// What a secure private directory must be: a directory, permissions exactly
/// 0700, no setuid/setgid/sticky bit, owned by the effective user.
fn secure_private_directory_info(info: &fs::Metadata) -> bool {
    let mode = info.mode();
    info.is_dir() && mode & 0o7777 == 0o700 && info.uid() == nix::unistd::Uid::effective().as_raw()
}

#[cfg(test)]
mod tests;
