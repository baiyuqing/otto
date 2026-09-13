//! Classification of host environment variables for a sandboxed child.
//!
//! Port of Go's `internal/sandbox/environment.go`. Three questions are decided
//! here for every host variable: whether the child may see it, whether its
//! value must be redacted from transcripts, and whether the classification can
//! be proven complete.
//!
//! Ownership: [`resolve_environment`] copies everything it needs out of its
//! options, so an [`EnvironmentSnapshot`] shares no storage with the caller and
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
use crate::safetext::SecretCollector;
use crate::tool::gopath;
use crate::urlprivacy;

/// The per-cache subdirectories a confined child is pointed at. Created at mode
/// 0700 inside `PrivateDirectories::cache`.
const DERIVED_CACHE_NAMES: [&str; 6] = ["go-build", "go-mod", "npm", "pip", "uv", "xdg"];

/// What [`resolve_environment`] is given.
///
/// `host_entries` are raw `NAME=VALUE` byte strings as the host presents them,
/// because a real environment may hold bytes that are not UTF-8 and those
/// entries must be rejected rather than repaired. `provider_names` and
/// `allow_names` come from configuration, which is UTF-8 by construction, so
/// Go's UTF-8 check on those two is unreachable here and is not ported.
#[derive(Debug, Clone, Default)]
pub struct EnvironmentOptions {
    pub host_entries: Vec<Vec<u8>>,
    pub provider_names: Vec<String>,
    pub allow_names: Vec<String>,
    pub private_directories: Option<PrivateDirectories>,
}

/// The environment a sandboxed child will receive, plus the values that must
/// never appear in model-visible text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvironmentSnapshot {
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
    pub fn redactions_complete(&self) -> bool {
        self.redactions_complete
    }
}

/// A rejected environment, carrying the bounded redactions salvaged before the
/// rejection so the caller can still hide what it collected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RejectedEnvironment {
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

/// Parses `NAME=VALUE` entries into a map, last duplicate winning.
///
/// Rejects any entry that is not valid UTF-8, holds a NUL byte, lacks `=`, or
/// carries a name that is not a POSIX-ish identifier.
pub fn parse_environment(entries: &[Vec<u8>]) -> Result<BTreeMap<String, String>, Error> {
    let mut environment = BTreeMap::new();
    for entry in entries {
        let Ok(entry) = std::str::from_utf8(entry) else {
            return Err(Error::EnvironmentUnsafe);
        };
        if entry.contains('\0') {
            return Err(Error::EnvironmentUnsafe);
        }
        let Some((name, value)) = entry.split_once('=') else {
            return Err(Error::EnvironmentUnsafe);
        };
        if !super::valid_environment_name(name) {
            return Err(Error::EnvironmentUnsafe);
        }
        environment.insert(name.to_owned(), value.to_owned());
    }
    Ok(environment)
}

/// Decides the child's environment and the redaction set for it.
///
/// A name is *ordinary* (passed through), *automatic* (withheld unless
/// explicitly allowed, value always redacted), or *non-restorable* (withheld
/// unconditionally, value always redacted). When `private_directories` is set,
/// the per-session `HOME`, `TMPDIR` and cache variables replace whatever the
/// host had, and the derived cache directories are created at mode 0700.
///
/// Returns [`RejectedEnvironment`] when any input is unsafe. That path never
/// creates directories: the bounds are enforced first.
pub fn resolve_environment(
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
        for private_path in [
            &directories.root,
            &directories.home,
            &directories.temp,
            &directories.cache,
        ] {
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
            ("HOME", display_path(&directories.home)),
            ("TMPDIR", display_path(&directories.temp)),
            ("TMP", display_path(&directories.temp)),
            ("TEMP", display_path(&directories.temp)),
            ("XDG_CACHE_HOME", cache("xdg")),
            ("GOCACHE", cache("go-build")),
            ("GOMODCACHE", cache("go-mod")),
            ("NPM_CONFIG_CACHE", cache("npm")),
            ("PIP_CACHE_DIR", cache("pip")),
            ("UV_CACHE_DIR", cache("uv")),
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

/// Verifies the four base directories and creates the derived cache
/// directories, all owner-only and none reached through a symlink.
fn prepare_private_environment(directories: &PrivateDirectories) -> Result<(), Error> {
    for path in [
        &directories.root,
        &directories.home,
        &directories.temp,
        &directories.cache,
    ] {
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

/// Go's `securePrivateDirectoryInfo`: a directory, permissions exactly 0700,
/// no setuid/setgid/sticky bit, owned by the effective user.
fn secure_private_directory_info(info: &fs::Metadata) -> bool {
    let mode = info.mode();
    info.is_dir() && mode & 0o7777 == 0o700 && info.uid() == nix::unistd::Uid::effective().as_raw()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn entries(list: &[&str]) -> Vec<Vec<u8>> {
        list.iter().map(|entry| entry.as_bytes().to_vec()).collect()
    }

    fn options(host: &[&str]) -> EnvironmentOptions {
        EnvironmentOptions {
            host_entries: entries(host),
            ..EnvironmentOptions::default()
        }
    }

    /// The resolved snapshot as a name/value map, checking on the way that no
    /// entry is malformed or duplicated.
    #[track_caller]
    fn snapshot_environment(snapshot: &EnvironmentSnapshot) -> BTreeMap<&str, &str> {
        let mut environment = BTreeMap::new();
        for entry in snapshot.entries().expect("a resolved snapshot has entries") {
            let (name, value) = entry.split_once('=').expect("entry has a separator");
            assert!(!name.is_empty(), "entry has an empty name");
            assert!(
                environment.insert(name, value).is_none(),
                "duplicate name {name:?}"
            );
        }
        environment
    }

    #[track_caller]
    fn assert_environment_names(snapshot: &EnvironmentSnapshot, want: &[&str]) {
        let got: Vec<&str> = snapshot_environment(snapshot).into_keys().collect();
        assert_eq!(got, want);
    }

    #[track_caller]
    fn assert_redactions_contain(got: &[String], want: &[&str]) {
        for value in want {
            assert!(
                got.iter().any(|candidate| candidate == value),
                "redactions omitted {value:?}"
            );
        }
    }

    #[track_caller]
    fn assert_redactions_sorted(values: &[String]) {
        assert!(
            values.windows(2).all(|pair| {
                pair[0].len() > pair[1].len()
                    || (pair[0].len() == pair[1].len() && pair[0] <= pair[1])
            }),
            "redactions are not sorted longest-first then lexically: {values:?}"
        );
    }

    /// Go's `newPrivateDirectories`: four owner-only directories under one
    /// temporary root, returned with the guard that keeps them alive.
    fn new_private_directories() -> (tempfile::TempDir, PrivateDirectories) {
        let temp = tempfile::tempdir().expect("temporary directory");
        let root = temp.path().join("private");
        let directories = PrivateDirectories {
            home: root.join("home"),
            temp: root.join("tmp"),
            cache: root.join("cache"),
            root,
        };
        for path in [
            &directories.root,
            &directories.home,
            &directories.temp,
            &directories.cache,
        ] {
            fs::DirBuilder::new()
                .mode(0o700)
                .create(path)
                .expect("create private directory");
        }
        (temp, directories)
    }

    #[track_caller]
    fn assert_unsafe_resolution(options: &EnvironmentOptions) {
        let rejection = resolve_environment(options).expect_err("resolution should be rejected");
        assert_eq!(rejection.to_string(), "sandbox environment is unsafe");
        assert_eq!(Error::from(rejection), Error::EnvironmentUnsafe);
    }

    fn sensitive_entries(count: usize) -> Vec<Vec<u8>> {
        (0..count)
            .map(|index| {
                format!("VALUE_{index:03}_TOKEN=unique-sensitive-value-{index:03}").into_bytes()
            })
            .collect()
    }

    #[test]
    fn parse_environment_is_deterministic_and_the_last_duplicate_wins() {
        let input = entries(&["ZETA=first", "ALPHA=alpha", "ZETA=last", "EMPTY="]);
        let want = BTreeMap::from([
            ("ALPHA".to_owned(), "alpha".to_owned()),
            ("EMPTY".to_owned(), String::new()),
            ("ZETA".to_owned(), "last".to_owned()),
        ]);
        assert_eq!(parse_environment(&input).unwrap(), want);
        assert_eq!(parse_environment(&input).unwrap(), want);
    }

    #[test]
    fn parse_environment_rejects_malformed_entries() {
        let cases: [&[u8]; 8] = [
            b"MALFORMED",
            b"=value",
            b"1INVALID=value",
            b"INVALID-NAME=value",
            b"INVALID\0NAME=value",
            b"VALID=unsafe\0value",
            b"N\xff=x",
            b"VALUE=\xff",
        ];
        for case in cases {
            let error = parse_environment(&[case.to_vec()]).expect_err("malformed entry");
            assert_eq!(error, Error::EnvironmentUnsafe);
            assert_eq!(error.to_string(), "sandbox environment is unsafe");
        }
    }

    #[test]
    fn a_failed_resolution_retains_bounded_redactions_from_every_valid_entry() {
        let (_guard, directories) = new_private_directories();
        let rejection = resolve_environment(&EnvironmentOptions {
            host_entries: entries(&[
                "AWS_SECRET_ACCESS_KEY=first-aws-secret",
                "HTTPS_PROXY=http://raw%20user:raw%2Fpass@[::1]:8443/path",
                "BROKEN",
                "AWS_SECRET_ACCESS_KEY=second-aws-secret",
            ]),
            private_directories: Some(directories.clone()),
            ..EnvironmentOptions::default()
        })
        .expect_err("a malformed entry rejects the environment");
        let snapshot = rejection.snapshot();
        assert!(snapshot.entries().is_none());
        assert_redactions_contain(
            snapshot.redaction_values(),
            &[
                "first-aws-secret",
                "second-aws-secret",
                "raw%20user:raw%2Fpass",
                "raw%20user",
                "raw%2Fpass",
                "raw user:raw/pass",
                "raw user",
                "raw/pass",
                &display_path(&directories.root),
                &display_path(&directories.home),
                &display_path(&directories.temp),
                &display_path(&directories.cache),
            ],
        );
        assert_redactions_sorted(snapshot.redaction_values());
    }

    #[test]
    fn provider_credentials_are_removed() {
        let snapshot = resolve_environment(&EnvironmentOptions {
            host_entries: entries(&[
                "OTTO_API_KEY=otto-sensitive-value",
                "otto_api_key=lower-otto-sensitive-value",
                "selected_key=selected-sensitive-value",
                "CONFIGURED_KEY=configured-sensitive-value",
                "ORDINARY=preserved",
            ]),
            provider_names: vec!["SELECTED_KEY".into(), "CONFIGURED_KEY".into()],
            allow_names: vec![
                "OTTO_API_KEY".into(),
                "otto_api_key".into(),
                "selected_key".into(),
                "CONFIGURED_KEY".into(),
            ],
            private_directories: None,
        })
        .unwrap();
        assert_environment_names(&snapshot, &["ORDINARY"]);
        assert_redactions_contain(
            snapshot.redaction_values(),
            &[
                "otto-sensitive-value",
                "lower-otto-sensitive-value",
                "selected-sensitive-value",
                "configured-sensitive-value",
            ],
        );
    }

    #[test]
    fn loader_and_shell_injection_variables_are_never_restored() {
        let names = [
            "DYLD_INSERT_LIBRARIES",
            "dyld_custom",
            "LD_PRELOAD",
            "ld_library_path",
            "BASH_ENV",
            "env",
            "ZDOTDIR",
            "PROMPT_COMMAND",
            "CDPATH",
            "SHELLOPTS",
            "BASHOPTS",
        ];
        let mut host: Vec<Vec<u8>> = names
            .iter()
            .enumerate()
            .map(|(index, name)| format!("{name}=classified-value-{index:02}").into_bytes())
            .collect();
        host.push(b"AUTHORS=preserved".to_vec());

        let snapshot = resolve_environment(&EnvironmentOptions {
            host_entries: host,
            allow_names: names.iter().map(|name| (*name).to_owned()).collect(),
            ..EnvironmentOptions::default()
        })
        .unwrap();
        assert_environment_names(&snapshot, &["AUTHORS"]);
        assert_eq!(snapshot.redaction_values().len(), names.len());
    }

    #[test]
    fn internal_and_control_variables_are_never_restored() {
        let names = [
            "OTTO_SANDBOX",
            "otto_sandbox_profile_path",
            "SSH_AUTH_SOCK",
            "docker_host",
            "CONTAINER_HOST",
        ];
        let mut host: Vec<Vec<u8>> = Vec::new();
        let mut original_values: Vec<String> = Vec::new();
        for (index, name) in names.iter().enumerate() {
            let value = format!("non-restorable-value-{index:02}");
            host.push(format!("{name}={value}").into_bytes());
            original_values.push(value);
        }
        host.push(b"SANDBOX_PROFILE=preserved".to_vec());

        let snapshot = resolve_environment(&EnvironmentOptions {
            host_entries: host,
            allow_names: names.iter().map(|name| (*name).to_owned()).collect(),
            ..EnvironmentOptions::default()
        })
        .unwrap();
        assert_environment_names(&snapshot, &["SANDBOX_PROFILE"]);
        let wanted: Vec<&str> = original_values.iter().map(String::as_str).collect();
        assert_redactions_contain(snapshot.redaction_values(), &wanted);
    }

    #[test]
    fn sensitive_suffixes_are_classified_case_insensitively() {
        let names = [
            "BUILD_TOKEN",
            "build_secret",
            "DATABASE_PASSWORD",
            "database_passwd",
            "SERVICE_API_KEY",
            "service_access_key",
            "signing_private_key",
            "APP_CREDENTIAL",
            "app_credentials",
        ];
        let mut host: Vec<Vec<u8>> = names
            .iter()
            .enumerate()
            .map(|(index, name)| format!("{name}=suffix-sensitive-{index:02}").into_bytes())
            .collect();
        host.push(b"AUTHORS=preserved".to_vec());
        host.push(b"PASSWORD_POLICY=preserved".to_vec());

        let snapshot = resolve_environment(&EnvironmentOptions {
            host_entries: host,
            ..EnvironmentOptions::default()
        })
        .unwrap();
        assert_environment_names(&snapshot, &["AUTHORS", "PASSWORD_POLICY"]);
        assert_eq!(snapshot.redaction_values().len(), names.len());
    }

    #[test]
    fn fixed_credential_names_are_classified() {
        let names = [
            "AWS_ACCESS_KEY_ID",
            "azure_client_id",
            "GCP_SERVICE_ACCOUNT_KEY",
            "github_pat",
            "GITLAB_CI_JOB_JWT",
            "NPM_CONFIG__AUTH",
            "REGISTRY_AUTH_FILE",
            "DOCKER_AUTH_CONFIG",
            "CI_JOB_JWT",
            "AUTHORIZATION",
            "http_cookie",
            "SSH_AUTH_SOCK",
            "DOCKER_HOST",
            "CONTAINER_HOST",
        ];
        let mut host: Vec<Vec<u8>> = names
            .iter()
            .enumerate()
            .map(|(index, name)| format!("{name}=fixed-sensitive-{index:02}").into_bytes())
            .collect();
        host.push(b"AUTHORS=preserved".to_vec());

        let snapshot = resolve_environment(&EnvironmentOptions {
            host_entries: host,
            ..EnvironmentOptions::default()
        })
        .unwrap();
        assert_environment_names(&snapshot, &["AUTHORS"]);
        assert_eq!(snapshot.redaction_values().len(), names.len());
    }

    #[test]
    fn an_exact_allow_restores_only_automatic_non_provider_names() {
        let snapshot = resolve_environment(&EnvironmentOptions {
            host_entries: entries(&[
                "PROJECT_TOKEN=restored-sensitive-value",
                "project_token=case-sensitive-not-restored",
                "NPM_CONFIG__AUTH=restored-fixed-value",
                "PROVIDER_TOKEN=provider-sensitive-value",
                "BASH_ENV=shell-sensitive-value",
                "ORDINARY=preserved",
            ]),
            provider_names: vec!["PROVIDER_TOKEN".into()],
            allow_names: vec![
                "PROJECT_TOKEN".into(),
                "NPM_CONFIG__AUTH".into(),
                "PROVIDER_TOKEN".into(),
                "BASH_ENV".into(),
            ],
            private_directories: None,
        })
        .unwrap();
        assert_environment_names(
            &snapshot,
            &["NPM_CONFIG__AUTH", "ORDINARY", "PROJECT_TOKEN"],
        );
        assert_redactions_contain(
            snapshot.redaction_values(),
            &[
                "restored-sensitive-value",
                "case-sensitive-not-restored",
                "restored-fixed-value",
                "provider-sensitive-value",
                "shell-sensitive-value",
            ],
        );
    }

    #[test]
    fn proxy_userinfo_is_collected_without_rewriting_the_proxy() {
        let proxy = "http://raw%20user:raw%2Fpass@[2001:db8::1]:8443/path";
        let second = "socks5://solo%20user@[::1]:9000";
        let network_path = "//network%20user:network%2Fpass@[2001:db8::2]:9443/path@ignored";
        let snapshot = resolve_environment(&options(&[
            &format!("HTTPS_PROXY={proxy}"),
            &format!("all_proxy={second}"),
            &format!("http_proxy={network_path}"),
        ]))
        .unwrap();
        let environment = snapshot_environment(&snapshot);
        assert_eq!(environment["HTTPS_PROXY"], proxy);
        assert_eq!(environment["all_proxy"], second);
        assert_eq!(environment["http_proxy"], network_path);
        assert_redactions_contain(
            snapshot.redaction_values(),
            &[
                "raw%20user:raw%2Fpass",
                "raw user:raw/pass",
                "raw user",
                "raw/pass",
                "solo%20user",
                "solo user",
                "network%20user:network%2Fpass",
                "network user:network/pass",
            ],
        );
        assert_redactions_sorted(snapshot.redaction_values());
    }

    #[test]
    fn malformed_proxy_authorities_are_extracted_conservatively() {
        let cases: &[(&str, &[&str])] = &[
            (
                "http:///proxy-user:proxy-pass@example.test",
                &["proxy-user:proxy-pass", "proxy-user", "proxy-pass"],
            ),
            (
                r"http:\\proxy%20user:proxy%2Fpass@example.test\path",
                &[
                    "proxy%20user:proxy%2Fpass",
                    "proxy%20user",
                    "proxy%2Fpass",
                    "proxy user:proxy/pass",
                    "proxy user",
                    "proxy/pass",
                ],
            ),
            (
                "proxy%20user:proxy%2Fpass@example.test/path",
                &[
                    "proxy%20user:proxy%2Fpass",
                    "proxy%20user",
                    "proxy%2Fpass",
                    "proxy user:proxy/pass",
                    "proxy user",
                    "proxy/pass",
                ],
            ),
            (
                "ht!tp://odd%20user:odd%2Fpass@example.test/path",
                &[
                    "odd%20user:odd%2Fpass",
                    "odd%20user",
                    "odd%2Fpass",
                    "odd user:odd/pass",
                    "odd user",
                    "odd/pass",
                ],
            ),
            (
                "http:odd%20user:odd%2Fpass@example.test/path",
                &[
                    "http:odd%20user:odd%2Fpass",
                    "odd%20user:odd%2Fpass",
                    "odd%20user",
                    "odd%2Fpass",
                    "odd user",
                    "odd/pass",
                ],
            ),
            (
                "http://[::1/path/proxy%20user:proxy%2Fpass@example.test",
                &[
                    "proxy%20user:proxy%2Fpass",
                    "proxy%20user",
                    "proxy%2Fpass",
                    "proxy user",
                    "proxy/pass",
                ],
            ),
            (
                "http://bad%zz:pass%2Fword@[::1]:8080/path?next=@ignored#@ignored",
                &["bad%zz:pass%2Fword", "bad%zz", "pass%2Fword", "pass/word"],
            ),
            (
                "http://user%20name:bad%zz@[2001:db8::1]:8080/path",
                &["user%20name:bad%zz", "user%20name", "bad%zz", "user name"],
            ),
            (
                "http://bad%zz:pass%2Fword@[::1]:8080/path/user:other@example.test",
                &["bad%zz:pass%2Fword", "bad%zz", "pass%2Fword", "pass/word"],
            ),
            (
                "http:///real%20user:real%2Fpass@proxy/path%20user:path%2Fpass@example",
                &[
                    "real%20user:real%2Fpass",
                    "real user:real/pass",
                    "path%20user:path%2Fpass",
                    "path user:path/pass",
                ],
            ),
            (
                "///raw%20user:raw%2Fpass@host/path@ignored",
                &["raw%20user:raw%2Fpass", "raw user:raw/pass", "path"],
            ),
        ];
        for (proxy, want) in cases {
            let Err(rejection) = resolve_environment(&options(&[&format!("HTTPS_PROXY={proxy}")]))
            else {
                panic!("{proxy} should be rejected");
            };
            let snapshot = rejection.snapshot();
            assert!(snapshot.entries().is_none(), "{proxy}");
            assert!(!snapshot.redactions_complete(), "{proxy}");
            assert_redactions_contain(snapshot.redaction_values(), want);
        }
    }

    #[test]
    fn percent_decoded_invalid_utf8_proxy_userinfo_is_canonicalized() {
        let proxy = "http://user%FF:pass%C0%AF@[::1]:8080/path";
        let snapshot = resolve_environment(&options(&[&format!("HTTPS_PROXY={proxy}")])).unwrap();
        assert_eq!(snapshot_environment(&snapshot)["HTTPS_PROXY"], proxy);
        assert_redactions_contain(
            snapshot.redaction_values(),
            &[
                "user%FF:pass%C0%AF",
                "user%FF",
                "pass%C0%AF",
                "user\u{fffd}:pass\u{fffd}\u{fffd}",
                "user\u{fffd}",
                "pass\u{fffd}\u{fffd}",
            ],
        );
    }

    #[test]
    fn a_valid_proxy_path_query_or_fragment_at_is_not_userinfo() {
        let proxy = "http://[2001:db8::1]:8080/path/user:pass@example.test?next=user:pass@example.test#user:pass@example.test";
        let snapshot = resolve_environment(&options(&[&format!("HTTPS_PROXY={proxy}")])).unwrap();
        assert_eq!(snapshot_environment(&snapshot)["HTTPS_PROXY"], proxy);
        assert!(snapshot.redaction_values().is_empty());
    }

    #[test]
    fn a_malformed_proxy_path_at_is_incomplete_without_inventing_userinfo() {
        let proxy = "http://[2001:db8::1]:8080/path/user:pass@example.test?broken=%zz";
        let rejection =
            resolve_environment(&options(&[&format!("HTTPS_PROXY={proxy}")])).unwrap_err();
        assert!(rejection.snapshot().entries().is_none());
        assert!(!rejection.snapshot().redactions_complete());
        assert!(rejection.snapshot().redaction_values().is_empty());
    }

    #[test]
    fn proxy_values_without_a_literal_at_are_preserved() {
        for value in [
            "http://example.test/path%zz",
            "http:///example.test/path",
            r"http:\\example.test\path",
            "example.test:8080",
            "//example.test:8443",
            "http://:8080",
            "http://example.test:99999",
        ] {
            let Ok(snapshot) = resolve_environment(&options(&[&format!("HTTPS_PROXY={value}")]))
            else {
                panic!("{value} was rejected");
            };
            assert!(snapshot.redactions_complete(), "{value}");
            assert_eq!(snapshot_environment(&snapshot)["HTTPS_PROXY"], value);
        }
    }

    #[test]
    fn no_proxy_bypass_lists_are_preserved_case_insensitively() {
        let host = [
            "NO_PROXY=localhost,127.0.0.1,.example.test,[::1],*",
            "no_proxy=user:pass@example.test,service.local",
        ];
        let snapshot = resolve_environment(&options(&host)).unwrap();
        assert!(snapshot.redactions_complete());
        assert_eq!(snapshot.entries().unwrap(), host);
        assert!(snapshot.redaction_values().is_empty());
    }

    #[test]
    fn private_paths_are_rewritten_and_caches_created() {
        let (_guard, directories) = new_private_directories();
        let snapshot = resolve_environment(&EnvironmentOptions {
            host_entries: entries(&[
                "HOME=/host/home",
                "TMPDIR=/host/tmpdir",
                "TMP=/host/tmp",
                "TEMP=/host/temp",
                "XDG_CACHE_HOME=/host/xdg",
                "GOCACHE=/host/go-build",
                "GOMODCACHE=/host/go-mod",
                "NPM_CONFIG_CACHE=/host/npm",
                "PIP_CACHE_DIR=/host/pip",
                "UV_CACHE_DIR=/host/uv",
                "home=preserved-lowercase",
                "HOME_SUFFIX=preserved-suffix",
            ]),
            private_directories: Some(directories.clone()),
            ..EnvironmentOptions::default()
        })
        .unwrap();

        let cache = |name: &str| display_path(&directories.cache.join(name));
        let want = BTreeMap::from([
            ("HOME", display_path(&directories.home)),
            ("TMPDIR", display_path(&directories.temp)),
            ("TMP", display_path(&directories.temp)),
            ("TEMP", display_path(&directories.temp)),
            ("XDG_CACHE_HOME", cache("xdg")),
            ("GOCACHE", cache("go-build")),
            ("GOMODCACHE", cache("go-mod")),
            ("NPM_CONFIG_CACHE", cache("npm")),
            ("PIP_CACHE_DIR", cache("pip")),
            ("UV_CACHE_DIR", cache("uv")),
            ("home", "preserved-lowercase".to_owned()),
            ("HOME_SUFFIX", "preserved-suffix".to_owned()),
        ]);
        let got: BTreeMap<&str, String> = snapshot_environment(&snapshot)
            .into_iter()
            .map(|(name, value)| (name, value.to_owned()))
            .collect();
        assert_eq!(got, want);

        for name in DERIVED_CACHE_NAMES {
            let info = fs::symlink_metadata(directories.cache.join(name)).unwrap();
            assert!(info.is_dir() && !info.file_type().is_symlink(), "{name}");
            assert_eq!(info.mode() & 0o7777, 0o700, "{name}");
        }
        assert_redactions_contain(
            snapshot.redaction_values(),
            &[
                &display_path(&directories.root),
                &display_path(&directories.home),
                &display_path(&directories.temp),
                &display_path(&directories.cache),
            ],
        );
        assert_redactions_sorted(snapshot.redaction_values());
    }

    #[test]
    fn direct_mode_preserves_host_paths() {
        let host = [
            "HOME=/host/home",
            "TMPDIR=/host/tmpdir",
            "TMP=/host/tmp",
            "TEMP=/host/temp",
            "XDG_CACHE_HOME=/host/xdg",
            "GOCACHE=/host/go-build",
            "GOMODCACHE=/host/go-mod",
            "NPM_CONFIG_CACHE=/host/npm",
            "PIP_CACHE_DIR=/host/pip",
            "UV_CACHE_DIR=/host/uv",
        ];
        let snapshot = resolve_environment(&options(&host)).unwrap();
        let mut sorted = host.to_vec();
        sorted.sort_unstable();
        assert_eq!(snapshot.entries().unwrap(), sorted);
        assert!(snapshot.redaction_values().is_empty());
    }

    #[test]
    fn unsafe_private_directories_are_rejected() {
        let (_guard, directories) = new_private_directories();
        fs::remove_dir(&directories.cache).unwrap();
        std::os::unix::fs::symlink(tempfile::tempdir().unwrap().path(), &directories.cache)
            .unwrap();
        assert_unsafe_resolution(&EnvironmentOptions {
            private_directories: Some(directories),
            ..EnvironmentOptions::default()
        });

        let (_guard, directories) = new_private_directories();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), directories.cache.join("xdg")).unwrap();
        assert_unsafe_resolution(&EnvironmentOptions {
            private_directories: Some(directories),
            ..EnvironmentOptions::default()
        });

        let (_guard, directories) = new_private_directories();
        fs::set_permissions(&directories.cache, fs::Permissions::from_mode(0o750)).unwrap();
        assert_unsafe_resolution(&EnvironmentOptions {
            private_directories: Some(directories),
            ..EnvironmentOptions::default()
        });

        let (_guard, directories) = new_private_directories();
        fs::DirBuilder::new()
            .mode(0o755)
            .create(directories.cache.join("xdg"))
            .unwrap();
        assert_unsafe_resolution(&EnvironmentOptions {
            private_directories: Some(directories),
            ..EnvironmentOptions::default()
        });
    }

    #[test]
    fn json_string_escape_forms_expand_before_the_bounds() {
        let raw = r"prefix\u003csuffix";
        let snapshot = resolve_environment(&options(&[&format!("ESCAPED_TOKEN={raw}")])).unwrap();
        assert_redactions_contain(snapshot.redaction_values(), &[raw, "prefix<suffix"]);

        let mut host = sensitive_entries(511);
        host.push(format!("ESCAPED_TOKEN={raw}").into_bytes());
        let rejection = resolve_environment(&EnvironmentOptions {
            host_entries: host,
            ..EnvironmentOptions::default()
        })
        .unwrap_err();
        assert!(!rejection.snapshot().redactions_complete());
    }

    #[test]
    fn the_sensitive_value_count_bound_is_enforced() {
        let snapshot = resolve_environment(&EnvironmentOptions {
            host_entries: sensitive_entries(512),
            ..EnvironmentOptions::default()
        })
        .unwrap();
        assert_eq!(snapshot.redaction_values().len(), 512);

        assert_unsafe_resolution(&EnvironmentOptions {
            host_entries: sensitive_entries(513),
            ..EnvironmentOptions::default()
        });
    }

    #[test]
    fn incomplete_redaction_snapshots_are_marked() {
        let complete = resolve_environment(&EnvironmentOptions {
            host_entries: sensitive_entries(512),
            ..EnvironmentOptions::default()
        })
        .unwrap();
        assert!(complete.redactions_complete());

        let overflow = resolve_environment(&EnvironmentOptions {
            host_entries: sensitive_entries(513),
            ..EnvironmentOptions::default()
        })
        .unwrap_err();
        assert!(!overflow.snapshot().redactions_complete());
        assert!(
            !overflow
                .snapshot()
                .redaction_values()
                .iter()
                .any(|value| value == "unique-sensitive-value-512")
        );

        let over_budget = resolve_environment(&options(&[&format!(
            "LARGE_TOKEN={}",
            "z".repeat((1 << 20) + 1)
        )]))
        .unwrap_err();
        assert!(!over_budget.snapshot().redactions_complete());

        let malformed = resolve_environment(&options(&["BROKEN"])).unwrap_err();
        assert!(!malformed.snapshot().redactions_complete());

        let malformed_proxy =
            resolve_environment(&options(&["HTTPS_PROXY=http:///user:pass@example.test"]))
                .unwrap_err();
        assert!(!malformed_proxy.snapshot().redactions_complete());
    }

    #[test]
    fn the_sensitive_byte_bound_is_enforced_before_private_paths() {
        let at_limit = "x".repeat(1 << 20);
        let snapshot =
            resolve_environment(&options(&[&format!("LARGE_TOKEN={at_limit}")])).unwrap();
        assert_eq!(snapshot.redaction_values().len(), 1);
        assert_eq!(snapshot.redaction_values()[0].len(), 1 << 20);

        let (_guard, directories) = new_private_directories();
        assert_unsafe_resolution(&EnvironmentOptions {
            host_entries: entries(&[&format!("LARGE_TOKEN={}", "y".repeat((1 << 20) + 1))]),
            private_directories: Some(directories.clone()),
            ..EnvironmentOptions::default()
        });
        for name in DERIVED_CACHE_NAMES {
            assert!(
                fs::symlink_metadata(directories.cache.join(name)).is_err(),
                "derived cache {name} was created before the bound passed"
            );
        }
    }

    #[test]
    fn private_path_redactions_stay_inside_the_sensitive_byte_bound() {
        let directories = PrivateDirectories {
            root: PathBuf::from("r".repeat(1 << 20)),
            home: PathBuf::from("/known/private/home"),
            temp: PathBuf::from("/known/private/temp"),
            cache: PathBuf::from("/known/private/cache"),
        };
        let rejection = resolve_environment(&EnvironmentOptions {
            private_directories: Some(directories),
            ..EnvironmentOptions::default()
        })
        .unwrap_err();
        assert!(rejection.snapshot().entries().is_none());
        let total: usize = rejection
            .snapshot()
            .redaction_values()
            .iter()
            .map(String::len)
            .sum();
        assert!(total <= 1 << 20, "private redaction bytes = {total}");
    }

    #[test]
    fn malformed_repeated_proxy_prefixes_stay_bounded() {
        let mut proxy = String::from("http:///");
        for index in 0..600 {
            proxy.push_str(&format!("u{index:03}:p{index:03}@"));
        }
        proxy.push_str("example.test");
        let rejection =
            resolve_environment(&options(&[&format!("HTTPS_PROXY={proxy}")])).unwrap_err();
        assert!(!rejection.snapshot().redactions_complete());
        assert!(rejection.snapshot().redaction_values().len() <= 512);
        assert!(
            !rejection
                .snapshot()
                .redaction_values()
                .iter()
                .any(|value| value == "u000:p000@u001:p001")
        );
    }

    #[test]
    fn resolution_returns_sorted_entries_and_redactions() {
        let snapshot = resolve_environment(&EnvironmentOptions {
            host_entries: entries(&[
                "ZED=ordinary",
                "ALPHA_TOKEN=longest-sensitive-value",
                "BETA_SECRET=aa",
                "GAMMA_PASSWORD=zz",
            ]),
            allow_names: vec!["ALPHA_TOKEN".into()],
            ..EnvironmentOptions::default()
        })
        .unwrap();
        assert_eq!(
            snapshot.entries().unwrap(),
            ["ALPHA_TOKEN=longest-sensitive-value", "ZED=ordinary"]
        );
        assert_eq!(
            snapshot.redaction_values(),
            ["longest-sensitive-value", "aa", "zz"]
        );

        let empty = resolve_environment(&options(&[])).unwrap();
        assert_eq!(empty.entries(), Some(&[][..]));
        assert!(empty.redaction_values().is_empty());
    }

    #[test]
    fn invalid_option_names_are_rejected() {
        assert_unsafe_resolution(&EnvironmentOptions {
            provider_names: vec!["INVALID-NAME".into()],
            ..EnvironmentOptions::default()
        });
        assert_unsafe_resolution(&EnvironmentOptions {
            allow_names: vec!["*_TOKEN".into()],
            ..EnvironmentOptions::default()
        });
        assert_unsafe_resolution(&EnvironmentOptions {
            allow_names: vec!["DUPLICATE".into(), "DUPLICATE".into()],
            ..EnvironmentOptions::default()
        });
    }
}
