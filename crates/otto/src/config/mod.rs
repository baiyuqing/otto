//! Native filesystem and environment layer for configuration.
//!
//! Loading, in-place edits, and the default path; the pure schema, parsing, and
//! resolution logic lives in `otto_core::config`.
//!
//! ponytail: `default_path`, `load`, and `set_default_profile_file` each
//! have a private `_for_home`/`_impl` twin that takes the otherwise-implicit
//! `HOME` value or default path as an explicit argument. This workspace's
//! `unsafe_code` lint (denied by `make rust-lint`'s `-D warnings`) forbids the
//! `std::env::set_var` edition 2024 needs, so tests call the `_for_home`/
//! `_impl` twin directly instead of mutating the real environment.

use std::collections::HashMap;
pub mod layered;
use std::fs;
use std::fs::File as FsFile;
use std::io;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::fsops::{self, PosixPublish, Publish, PublishMode, PublishOps, PublishStep};
use chrono::Utc;
use nix::errno::Errno;
use nix::fcntl::{FcntlArg, fcntl};

use otto_core::config::{
    ConfigError, File, Overrides, PROVIDER_CHATGPT, PROVIDER_OPENAI_COMPATIBLE, SessionDefaults,
};

/// An error from the native config layer: either an I/O failure opening,
/// reading, or writing the file, or a [`ConfigError`] from otto-core's pure
/// parsing/resolution logic. Callers distinguish a missing file with
/// [`NativeConfigError::is_not_found`] and read everything else from
/// `Display`.
#[derive(Debug, thiserror::Error)]
pub enum NativeConfigError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Config(#[from] ConfigError),
}

impl NativeConfigError {
    /// Whether the failure was a missing file.
    pub fn is_not_found(&self) -> bool {
        matches!(self, NativeConfigError::Io(err) if err.kind() == io::ErrorKind::NotFound)
    }
}

/// The default config file path: `$HOME/.config/otto/config.toml`, or the
/// literal path `~/.config/otto/config.toml`, left unexpanded, if `HOME` is
/// unset or empty.
pub fn default_path() -> PathBuf {
    default_path_for_home(std::env::var("HOME").ok().as_deref())
}

fn default_path_for_home(home: Option<&str>) -> PathBuf {
    let base = match home {
        Some(home) if !home.is_empty() => home,
        _ => "~",
    };
    [base, ".config", "otto", "config.toml"].iter().collect()
}

/// Reads and parses `path`. Unlike [`load`], a missing file is always an error,
/// even at the default path.
pub fn load_required(path: &Path) -> Result<File, NativeConfigError> {
    let text = fs::read_to_string(path)?;
    Ok(otto_core::config::parse(&text)?)
}

/// Reads and parses `path`, treating a missing file at the default config path
/// as an empty [`File`] rather than an error.
pub fn load(path: &Path) -> Result<File, NativeConfigError> {
    load_impl(path, &default_path())
}

fn load_impl(path: &Path, default_path: &Path) -> Result<File, NativeConfigError> {
    load_with_bytes(path, default_path).map(|(_, file)| file)
}

/// Reads and parses `path` like [`load_impl`], also returning the exact bytes
/// the parse came from.
///
/// Those bytes are what a later write passes as `replacing`, so that a change
/// another process made in between is detected instead of overwritten. Reading
/// the file a second time to obtain them would reopen that window, so every
/// read-modify-write keeps the bytes from this one read.
fn load_with_bytes(path: &Path, default_path: &Path) -> Result<(Vec<u8>, File), NativeConfigError> {
    match fs::read_to_string(path) {
        Ok(text) => {
            let file = otto_core::config::parse(&text)?;
            Ok((text.into_bytes(), file))
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound && path == default_path => {
            Ok((Vec::new(), File::default()))
        }
        Err(err) => Err(err.into()),
    }
}

/// How many replaced versions of the configuration [`write_bytes`] keeps in
/// the `backups` directory beside it.
const BACKUP_LIMIT: usize = 10;

static CONFIG_WRITE_LOCK: Mutex<()> = Mutex::new(());

/// Replaces `path` with `updated`, after copying the contents it replaces into
/// `backups` beside it.
///
/// The write is a compare-and-swap: `replacing` is the file's contents as the
/// caller read them (empty for a file that did not exist), and a mismatch
/// means another process wrote the file in between, so the write is refused
/// rather than overwriting that change. Several Otto processes can run at
/// once, and each one reads, edits, and writes the whole file, so without this
/// check the last writer would silently drop the others' edits.
///
/// The write is protected by a POSIX advisory lock on a sibling lock file, so
/// Otto processes serialize the compare/back-up/rename sequence. The lock file
/// is only the kernel lock's anchor: it may remain on disk, and a crashed
/// writer's lock is released when the kernel closes its file descriptor. The
/// compare-and-swap check remains necessary for editors or other processes that
/// do not take Otto's lock.
///
/// The write is atomic and never follows a symlink: a `0o600` temporary file
/// in the same directory is written, synced, and renamed over `path`, so an
/// interrupted write leaves the previous file intact. A backup that cannot be
/// written fails the whole call, because the previous version would otherwise
/// be lost exactly when it is needed.
pub(crate) fn write_bytes(
    path: &Path,
    replacing: &[u8],
    updated: &[u8],
) -> Result<(), &'static str> {
    write_bytes_with(path, replacing, updated, &PosixPublish)
}

fn write_bytes_with(
    path: &Path,
    replacing: &[u8],
    updated: &[u8],
    publish: &dyn PublishOps,
) -> Result<(), &'static str> {
    let _process_lock = CONFIG_WRITE_LOCK
        .lock()
        .map_err(|_| "cannot lock configuration")?;
    let _lock = lock_config(path)?;
    let current = match fs::read(path) {
        Ok(content) => Some(content),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(_) => return Err("cannot read current file"),
    };
    if current.as_deref().unwrap_or_default() != replacing {
        return Err("the configuration changed on disk; rerun to apply this change");
    }
    if fs::symlink_metadata(path).is_ok_and(|info| !info.file_type().is_file()) {
        return Err("configuration must be a regular file, not a symlink");
    }
    let directory = parent_directory(path);
    if fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)
        .is_err()
    {
        return Err("cannot create configuration directory");
    }
    if let Some(current) = current {
        back_up(directory, &current)?;
    }
    let suffix = crate::runtime::builder::random_id()
        .map_err(|_| "cannot create temporary configuration")?;
    let temp = format!(".otto-config-{suffix}");
    let (parent, name) =
        fsops::open_parent(path).map_err(|_| "cannot create temporary configuration")?;
    fsops::publish_file(
        &parent,
        &Publish {
            name,
            temp: &temp,
            permissions: 0o600,
            mode: PublishMode::Replace,
        },
        updated,
        publish,
    )
    .map_err(|error| match error.step {
        PublishStep::CreateTemp => "cannot create temporary configuration",
        PublishStep::Write | PublishStep::FileSync => "cannot write configuration",
        PublishStep::Rename => "cannot replace configuration",
        PublishStep::DirSync => "cannot sync configuration directory",
    })
}

struct ConfigLock {
    file: FsFile,
}

fn lock_config(path: &Path) -> Result<ConfigLock, &'static str> {
    let directory = parent_directory(path);
    if fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)
        .is_err()
    {
        return Err("cannot create configuration directory");
    }
    let lock_path = path.with_file_name(format!(
        "{}.lock",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("config.toml")
    ));
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(lock_path)
        .map_err(|_| "cannot lock configuration")?;
    let lock = libc::flock {
        l_type: libc::F_WRLCK as libc::c_short,
        l_whence: libc::SEEK_SET as libc::c_short,
        l_start: 0,
        l_len: 0,
        l_pid: 0,
    };
    loop {
        match fcntl(&file, FcntlArg::F_SETLKW(&lock)) {
            Ok(_) => return Ok(ConfigLock { file }),
            Err(Errno::EINTR) => continue,
            Err(_) => return Err("cannot lock configuration"),
        }
    }
}

impl Drop for ConfigLock {
    fn drop(&mut self) {
        let lock = libc::flock {
            l_type: libc::F_UNLCK as libc::c_short,
            l_whence: libc::SEEK_SET as libc::c_short,
            l_start: 0,
            l_len: 0,
            l_pid: 0,
        };
        let _ = fcntl(&self.file, FcntlArg::F_SETLK(&lock));
    }
}

fn sync_directory(directory: &Path) -> Result<(), &'static str> {
    FsFile::open(directory)
        .and_then(|file| file.sync_all())
        .map_err(|_| "cannot sync configuration directory")
}

/// The directory `path` lives in: the working directory for a bare file name,
/// which [`Path::parent`] reports as an empty path rather than `None`.
fn parent_directory(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

/// Copies `current` into `<directory>/backups/config-<UTC timestamp>.toml`
/// and drops all but the most recent [`BACKUP_LIMIT`] of them.
///
/// The timestamp is millisecond-resolution UTC in a fixed-width basic format,
/// so the names sort chronologically; a name already taken (two writes within
/// the same millisecond) gets the [`backup_name`] suffix, which keeps them in
/// write order too.
fn back_up(directory: &Path, current: &[u8]) -> Result<(), &'static str> {
    let stamp = Utc::now().format("%Y%m%dT%H%M%S%.3fZ").to_string();
    back_up_with_stamp(directory, current, &stamp)
}

fn back_up_with_stamp(directory: &Path, current: &[u8], stamp: &str) -> Result<(), &'static str> {
    let backups = directory.join("backups");
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&backups)
        .map_err(|_| "cannot create configuration backup directory")?;
    // Pruning frees the oldest names. Never reuse those slots: a reused name
    // would sort before retained backups and immediately be pruned again.
    let mut attempt = (0..=99)
        .rev()
        .find(|attempt| backups.join(backup_name(stamp, *attempt)).exists())
        .map_or(0, |attempt| attempt + 1);
    if attempt > 99 {
        return Err("cannot write configuration backup");
    }
    let mut file = loop {
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(backups.join(backup_name(stamp, attempt)))
        {
            Ok(file) => break file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists && attempt < 99 => {
                attempt += 1;
            }
            Err(_) => return Err("cannot write configuration backup"),
        }
    };
    file.write_all(current)
        .and_then(|()| file.sync_all())
        .map_err(|_| "cannot write configuration backup")?;
    prune(&backups);
    Ok(())
}

/// The backup file name for `stamp`, and for `attempt` writes already
/// holding a name within that same millisecond.
///
/// The suffix separator sorts after the `.` of the extension and its counter
/// is zero-padded, so the names of one millisecond's writes sort in write
/// order, like every other pair. [`prune`] drops the oldest backups by that
/// order, so a separator sorting the other way would drop the newest.
fn backup_name(stamp: &str, attempt: u32) -> String {
    match attempt {
        0 => format!("config-{stamp}.toml"),
        taken => format!("config-{stamp}_{taken:02}.toml"),
    }
}

/// Removes all but the newest [`BACKUP_LIMIT`] backups, by name.
///
/// Best effort: a copy that cannot be removed is left behind rather than
/// failing a write whose backup already succeeded.
fn prune(backups: &Path) {
    let Ok(entries) = fs::read_dir(backups) else {
        return;
    };
    let mut names: Vec<_> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name())
        .filter(|name| {
            let name = name.to_string_lossy();
            name.starts_with("config-") && name.ends_with(".toml")
        })
        .collect();
    names.sort();
    let excess = names.len().saturating_sub(BACKUP_LIMIT);
    for name in names.into_iter().take(excess) {
        let _ = fs::remove_file(backups.join(name));
    }
}

fn io_error(message: &'static str) -> NativeConfigError {
    NativeConfigError::Io(io::Error::other(message))
}

/// Creates a new configuration file containing one default provider profile.
///
/// The destination must not exist, including as an empty file or symlink. The
/// final install is create-only, so a non-Otto writer that wins a race is never
/// overwritten.
pub(crate) fn create_initial_profile(
    path: &Path,
    profile: &str,
    provider: &str,
    model: &str,
    base_url: &str,
    api_key_env: &str,
) -> Result<(), String> {
    let content = initial_profile_content(profile, provider, model, base_url, api_key_env)?;
    create_bytes_if_absent(path, content.as_bytes()).map_err(str::to_string)
}

/// Renders and validates the complete non-secret configuration `otto setup`
/// proposes before a user confirms its creation.
pub(crate) fn initial_profile_content(
    profile: &str,
    provider: &str,
    model: &str,
    base_url: &str,
    api_key_env: &str,
) -> Result<String, String> {
    if !matches!(provider, PROVIDER_CHATGPT | PROVIDER_OPENAI_COMPATIBLE) {
        return Err("unsupported provider; no changes made".to_string());
    }
    let mut profile_table = toml::Table::new();
    profile_table.insert(
        "provider".to_string(),
        toml::Value::String(provider.to_string()),
    );
    profile_table.insert("model".to_string(), toml::Value::String(model.to_string()));
    if provider == PROVIDER_OPENAI_COMPATIBLE {
        profile_table.insert(
            "base_url".to_string(),
            toml::Value::String(base_url.to_string()),
        );
        profile_table.insert(
            "api_key_env".to_string(),
            toml::Value::String(api_key_env.to_string()),
        );
    }
    let mut profiles = toml::Table::new();
    profiles.insert(profile.to_string(), toml::Value::Table(profile_table));
    let mut root = toml::Table::new();
    root.insert(
        "default_profile".to_string(),
        toml::Value::String(profile.to_string()),
    );
    root.insert("profiles".to_string(), toml::Value::Table(profiles));
    let content = toml::to_string(&root).map_err(|_| "cannot create configuration".to_string())?;
    let parsed = otto_core::config::parse(&content)
        .map_err(|_| "cannot create configuration".to_string())?;
    let mut environment = HashMap::new();
    if provider == PROVIDER_OPENAI_COMPATIBLE {
        environment.insert(api_key_env.to_string(), "configured".to_string());
    }
    otto_core::config::resolve(
        &parsed,
        &environment,
        &SessionDefaults::default(),
        &Overrides::default(),
    )
    .map_err(|error| format!("invalid setup values: {error}"))?;
    Ok(content)
}

/// Atomically installs `contents` only if `path` does not exist. It never
/// replaces an existing filesystem object and keeps no backup because there is
/// no replaced configuration.
fn create_bytes_if_absent(path: &Path, contents: &[u8]) -> Result<(), &'static str> {
    let _process_lock = CONFIG_WRITE_LOCK
        .lock()
        .map_err(|_| "cannot lock configuration")?;
    let _lock = lock_config(path)?;
    if fs::symlink_metadata(path).is_ok() {
        return Err("configuration already exists; no changes made");
    }
    let directory = parent_directory(path);
    let suffix = crate::runtime::builder::random_id()
        .map_err(|_| "cannot create temporary configuration")?;
    let temp = directory.join(format!(".otto-config-{suffix}"));
    let result = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)
        .map_err(|_| "cannot create temporary configuration")
        .and_then(|mut file| {
            file.write_all(contents)
                .and_then(|()| file.sync_all())
                .map_err(|_| "cannot write configuration")
        })
        .and_then(|()| match fs::hard_link(&temp, path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                Err("configuration already exists; no changes made")
            }
            Err(_) => Err("cannot create configuration"),
        })
        .and_then(|()| fs::remove_file(&temp).map_err(|_| "cannot create configuration"))
        .and_then(|()| sync_directory(directory));
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// Adds or removes one exact name from `[skills].disabled`, preserving every
/// other configuration byte and using the normal CAS-backed atomic writer.
pub fn set_skill_disabled_file(
    path: &Path,
    name: &str,
    disabled: bool,
) -> Result<(), NativeConfigError> {
    if name.is_empty() {
        return Err(ConfigError::new("missing skill name").into());
    }
    let (original, mut file) = load_with_bytes(path, &default_path())?;
    if disabled {
        file.skills.disabled.insert(name.to_string());
    } else {
        file.skills.disabled.remove(name);
    }
    let values: Vec<toml::Value> = file
        .skills
        .disabled
        .iter()
        .map(|name| toml::Value::String(name.clone()))
        .collect();
    let value = toml::Value::Array(values.clone()).to_string();
    let content = String::from_utf8_lossy(&original);
    let updated = if content.contains("[skills]") {
        otto_core::config::edit::set_value(&content, &["skills"], "disabled", Some(&value))?
    } else {
        let mut table = toml::Table::new();
        table.insert("disabled".to_string(), toml::Value::Array(values));
        otto_core::config::edit::insert_table(&content, &["skills"], table)?
    };
    write_bytes(path, &original, updated.as_bytes()).map_err(io_error)
}

/// Sets `path`'s `default_profile` to `profile`, after checking the profile
/// exists in the file, changing only that value.
pub fn set_default_profile_file(path: &Path, profile: &str) -> Result<(), NativeConfigError> {
    set_default_profile_file_impl(path, &default_path(), profile)
}

fn set_default_profile_file_impl(
    path: &Path,
    default_path: &Path,
    profile: &str,
) -> Result<(), NativeConfigError> {
    if profile.is_empty() {
        return Err(ConfigError::new("missing profile").into());
    }
    let (original, file) = load_with_bytes(path, default_path)?;
    if !file.profiles.contains_key(profile) {
        return Err(ConfigError::new(format!("profile {profile:?} not found")).into());
    }
    let content = String::from_utf8_lossy(&original);
    let updated = otto_core::config::set_default_profile(&content, profile)?;
    write_bytes(path, &original, updated.as_bytes()).map_err(io_error)
}

/// Sets the `thinking` key of `path`'s `[profiles.<profile>]` table, or
/// removes it when `thinking` is empty, changing only that statement.
pub fn set_profile_thinking_file(
    path: &Path,
    profile: &str,
    thinking: &str,
) -> Result<(), NativeConfigError> {
    if profile.is_empty() {
        return Err(ConfigError::new("missing profile").into());
    }
    if !matches!(thinking, "" | "low" | "medium" | "high" | "xhigh" | "max") {
        return Err(ConfigError::new(
            "invalid thinking: must be one of low, medium, high, xhigh, max",
        )
        .into());
    }
    let (original, file) = load_with_bytes(path, &default_path())?;
    if !file.profiles.contains_key(profile) {
        return Err(ConfigError::new(format!("profile {profile:?} not found")).into());
    }
    let content = String::from_utf8_lossy(&original);
    let value = (!thinking.is_empty()).then(|| format!("{thinking:?}"));
    let updated = otto_core::config::edit::set_value(
        &content,
        &["profiles", profile],
        "thinking",
        value.as_deref(),
    )?;
    write_bytes(path, &original, updated.as_bytes()).map_err(io_error)
}

/// A provider permitted in a profile change. This intentionally has no
/// catch-all string variant: profile management never accepts other providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ProfileProvider {
    OpenAiCompatible,
    ChatGpt,
}

impl ProfileProvider {
    fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiCompatible => PROVIDER_OPENAI_COMPATIBLE,
            Self::ChatGpt => PROVIDER_CHATGPT,
        }
    }
}

/// One editable, non-secret profile field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ProfileField {
    Model,
    Thinking,
    BaseUrl,
    ApiKeyEnv,
}

impl ProfileField {
    fn key(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::Thinking => "thinking",
            Self::BaseUrl => "base_url",
            Self::ApiKeyEnv => "api_key_env",
        }
    }
}

/// A typed, non-secret change to the profile portion of `config.toml`.
///
/// `api_key_env` is an environment-variable *name*, never an API-key value.
/// There is deliberately no generic TOML edit operation.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ProfileChange {
    SetDefault {
        profile: String,
    },
    Create {
        profile: String,
        provider: ProfileProvider,
        model: String,
        thinking: String,
        base_url: Option<String>,
        api_key_env: Option<String>,
    },
    SetField {
        profile: String,
        field: ProfileField,
        value: String,
    },
    Remove {
        profile: String,
    },
}

/// Safe, human-readable information about a profile change. It intentionally
/// contains no configured values (including the API-key environment name).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileChangeMetadata {
    pub operation: &'static str,
    pub profile: String,
    pub field: Option<ProfileField>,
}

/// The result a UI/server presents before confirmation.
///
/// `original` is the exact file snapshot read for the preview; `updated` is
/// the exact replacement proposed from it. `commit_profile_change` uses these
/// bytes with the normal compare-and-swap writer, so a stale preview cannot
/// overwrite another change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileChangePreview {
    pub original: Vec<u8>,
    pub updated: Vec<u8>,
    pub metadata: ProfileChangeMetadata,
    /// A value-redacted description suitable for direct display.
    pub redacted_diff: String,
}

/// Previews a typed profile change without writing it.
pub fn preview_profile_change(
    path: &Path,
    change: &ProfileChange,
) -> Result<ProfileChangePreview, NativeConfigError> {
    preview_profile_change_impl(path, &default_path(), change)
}

fn preview_profile_change_impl(
    path: &Path,
    default_path: &Path,
    change: &ProfileChange,
) -> Result<ProfileChangePreview, NativeConfigError> {
    let (original, file) = load_with_bytes(path, default_path)?;
    let content = String::from_utf8_lossy(&original);
    let (updated, metadata) = match change {
        ProfileChange::SetDefault { profile } => {
            validate_profile_name(profile)?;
            let profile_config = file
                .profiles
                .get(profile)
                .ok_or_else(|| ConfigError::new(format!("profile {profile:?} not found")))?;
            validate_provider(&profile_config.provider)?;
            (
                otto_core::config::set_default_profile(&content, profile)?,
                ProfileChangeMetadata {
                    operation: "set default profile",
                    profile: profile.clone(),
                    field: None,
                },
            )
        }
        ProfileChange::Create {
            profile,
            provider,
            model,
            thinking,
            base_url,
            api_key_env,
        } => {
            validate_profile_name(profile)?;
            if file.profiles.contains_key(profile) {
                return Err(ConfigError::new(format!("profile {profile:?} already exists")).into());
            }
            validate_model(model)?;
            validate_thinking(thinking)?;
            validate_create_fields(*provider, base_url.as_deref(), api_key_env.as_deref())?;
            let mut table = toml::Table::new();
            table.insert(
                "provider".into(),
                toml::Value::String(provider.as_str().into()),
            );
            table.insert("model".into(), toml::Value::String(model.clone()));
            if !thinking.is_empty() {
                table.insert("thinking".into(), toml::Value::String(thinking.clone()));
            }
            if let Some(base_url) = base_url {
                table.insert("base_url".into(), toml::Value::String(base_url.clone()));
            }
            if let Some(api_key_env) = api_key_env {
                table.insert(
                    "api_key_env".into(),
                    toml::Value::String(api_key_env.clone()),
                );
            }
            (
                otto_core::config::edit::insert_table(&content, &["profiles", profile], table)?,
                ProfileChangeMetadata {
                    operation: "create profile",
                    profile: profile.clone(),
                    field: None,
                },
            )
        }
        ProfileChange::SetField {
            profile,
            field,
            value,
        } => {
            validate_profile_name(profile)?;
            let profile_config = file
                .profiles
                .get(profile)
                .ok_or_else(|| ConfigError::new(format!("profile {profile:?} not found")))?;
            validate_field(*field, value, &profile_config.provider)?;
            let value = toml::Value::String(value.clone()).to_string();
            (
                otto_core::config::edit::set_value(
                    &content,
                    &["profiles", profile],
                    field.key(),
                    Some(&value),
                )?,
                ProfileChangeMetadata {
                    operation: "set profile field",
                    profile: profile.clone(),
                    field: Some(*field),
                },
            )
        }
        ProfileChange::Remove { profile } => {
            validate_profile_name(profile)?;
            let profile_config = file
                .profiles
                .get(profile)
                .ok_or_else(|| ConfigError::new(format!("profile {profile:?} not found")))?;
            validate_provider(&profile_config.provider)?;
            if file.profiles.len() == 1 {
                return Err(ConfigError::new("cannot remove the last profile").into());
            }
            if file.default_profile == *profile {
                return Err(ConfigError::new("cannot remove the default profile").into());
            }
            (
                otto_core::config::edit::remove_table(&content, &["profiles", profile])?,
                ProfileChangeMetadata {
                    operation: "remove profile",
                    profile: profile.clone(),
                    field: None,
                },
            )
        }
    };
    let redacted_diff = match metadata.field {
        Some(field) => format!(
            "{} {:?} for profile {:?}: <redacted>",
            metadata.operation, field, metadata.profile
        ),
        None => format!("{} {:?}", metadata.operation, metadata.profile),
    };
    Ok(ProfileChangePreview {
        original,
        updated: updated.into_bytes(),
        metadata,
        redacted_diff,
    })
}

/// Commits a previously generated preview through the normal atomic,
/// CAS-backed configuration writer.
pub fn commit_profile_change(
    path: &Path,
    preview: &ProfileChangePreview,
) -> Result<(), NativeConfigError> {
    write_bytes(path, &preview.original, &preview.updated).map_err(io_error)
}

fn validate_profile_name(name: &str) -> Result<(), ConfigError> {
    let mut chars = name.chars();
    if !matches!(chars.next(), Some(ch) if ch.is_ascii_alphanumeric())
        || !chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
    {
        return Err(ConfigError::new(
            "invalid profile name: use ASCII letters, digits, _ or -, starting with a letter or digit",
        ));
    }
    Ok(())
}

fn validate_model(model: &str) -> Result<(), ConfigError> {
    if model.is_empty() {
        Err(ConfigError::new("missing model"))
    } else {
        Ok(())
    }
}

fn validate_thinking(thinking: &str) -> Result<(), ConfigError> {
    if matches!(thinking, "" | "low" | "medium" | "high" | "xhigh" | "max") {
        Ok(())
    } else {
        Err(ConfigError::new(
            "invalid thinking: must be one of low, medium, high, xhigh, max",
        ))
    }
}

fn validate_env_name(name: &str) -> Result<(), ConfigError> {
    let mut chars = name.chars();
    if !matches!(chars.next(), Some(ch) if ch.is_ascii_alphabetic() || ch == '_')
        || !chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
    {
        return Err(ConfigError::new(
            "invalid api_key_env: use an environment variable name",
        ));
    }
    Ok(())
}

fn validate_create_fields(
    provider: ProfileProvider,
    base_url: Option<&str>,
    api_key_env: Option<&str>,
) -> Result<(), ConfigError> {
    match provider {
        ProfileProvider::OpenAiCompatible => {
            if base_url.is_none_or(str::is_empty) {
                return Err(ConfigError::new("missing base_url"));
            }
            if let Some(name) = api_key_env {
                validate_env_name(name)?;
            }
        }
        ProfileProvider::ChatGpt if base_url.is_some() || api_key_env.is_some() => {
            return Err(ConfigError::new(
                "chatgpt does not support base_url or api_key_env",
            ));
        }
        ProfileProvider::ChatGpt => {}
    }
    Ok(())
}

fn validate_provider(provider: &str) -> Result<(), ConfigError> {
    if matches!(provider, PROVIDER_OPENAI_COMPATIBLE | PROVIDER_CHATGPT) {
        Ok(())
    } else {
        Err(ConfigError::new(format!(
            "unsupported provider {provider:?}"
        )))
    }
}

fn validate_field(field: ProfileField, value: &str, provider: &str) -> Result<(), ConfigError> {
    validate_provider(provider)?;
    match field {
        ProfileField::Model => validate_model(value),
        ProfileField::Thinking => validate_thinking(value),
        ProfileField::BaseUrl => {
            if provider == PROVIDER_CHATGPT {
                Err(ConfigError::new(
                    "chatgpt does not support base_url or api_key_env",
                ))
            } else if value.is_empty() {
                Err(ConfigError::new("missing base_url"))
            } else {
                Ok(())
            }
        }
        ProfileField::ApiKeyEnv => {
            if provider == PROVIDER_CHATGPT {
                Err(ConfigError::new(
                    "chatgpt does not support base_url or api_key_env",
                ))
            } else {
                validate_env_name(value)
            }
        }
    }
}

#[cfg(test)]
mod tests;
