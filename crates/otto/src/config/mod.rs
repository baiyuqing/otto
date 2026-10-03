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
use std::fs;
use std::fs::File as FsFile;
use std::io;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

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
    let suffix = crate::cli::runtime_builder::random_id()
        .map_err(|_| "cannot create temporary configuration")?;
    let temp = directory.join(format!(".otto-config-{suffix}"));
    let write = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)
        .map_err(|_| "cannot create temporary configuration")
        .and_then(|mut file| {
            file.write_all(updated)
                .and_then(|()| file.sync_all())
                .map_err(|_| "cannot write configuration")
        });
    let result = write
        .and_then(|()| fs::rename(&temp, path).map_err(|_| "cannot replace configuration"))
        .and_then(|()| sync_directory(directory));
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
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
    let backups = directory.join("backups");
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&backups)
        .map_err(|_| "cannot create configuration backup directory")?;
    let stamp = Utc::now().format("%Y%m%dT%H%M%S%.3fZ").to_string();
    let mut attempt = 0;
    let mut file = loop {
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(backups.join(backup_name(&stamp, attempt)))
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
    let suffix = crate::cli::runtime_builder::random_id()
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
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

/// The environment `otto_core::config::resolve` and `resolve_memory` may
/// consult for `file`: a fixed set of `OTTO_*` overrides plus `HOME`, and
/// each profile's `api_key_env`. Exactly these names are read, never the whole
/// process environment, so that resolution can never be influenced by an
/// unrelated or oversized environment, and so a key value never has to be
/// logged or matched against a name pattern to redact it.
pub fn resolution_environment(file: &File) -> HashMap<String, String> {
    const FIXED_KEYS: [&str; 7] = [
        "HOME",
        "OTTO_PROVIDER",
        "OTTO_PROFILE",
        "OTTO_MODEL",
        "OTTO_API_KEY",
        "OTTO_UI",
        "OTTO_TRACE",
    ];
    let mut environment = HashMap::with_capacity(FIXED_KEYS.len() + file.profiles.len());
    for key in FIXED_KEYS {
        if let Ok(value) = std::env::var(key) {
            environment.insert(key.to_string(), value);
        }
    }
    for profile in file.profiles.values() {
        if profile.api_key_env.is_empty() || environment.contains_key(&profile.api_key_env) {
            continue;
        }
        if let Ok(value) = std::env::var(&profile.api_key_env) {
            environment.insert(profile.api_key_env.clone(), value);
        }
    }
    environment
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Command, Stdio};
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    use super::*;

    fn write(dir: &Path, name: &str, content: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, content).expect("write fixture");
        path
    }

    fn backups(dir: &Path) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = match fs::read_dir(dir.join("backups")) {
            Ok(entries) => entries.map(|entry| entry.expect("entry").path()).collect(),
            Err(_) => Vec::new(),
        };
        paths.sort();
        paths
    }

    fn profile_file(name: &str) -> Vec<u8> {
        format!("default_profile = \"{name}\"\n").into_bytes()
    }

    #[test]
    fn create_initial_profile_refuses_an_existing_empty_file() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("config.toml");
        fs::write(&path, b"").expect("empty config");

        let error = create_initial_profile(&path, "default", PROVIDER_CHATGPT, "my-model", "", "")
            .expect_err("must not replace an existing config");

        assert_eq!(error, "configuration already exists; no changes made");
        assert_eq!(fs::read(&path).expect("read"), b"");
        assert!(backups(directory.path()).is_empty());
    }

    #[test]
    fn create_initial_profile_writes_only_non_secret_fields() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("config.toml");

        create_initial_profile(
            &path,
            "default",
            PROVIDER_OPENAI_COMPATIBLE,
            "cheap-model",
            "https://api.example/v1",
            "EXAMPLE_KEY",
        )
        .expect("create");

        let content = fs::read_to_string(&path).expect("read");
        assert!(
            content.contains("api_key_env = \"EXAMPLE_KEY\""),
            "{content}"
        );
        assert!(!content.contains("configured"), "{content}");
        assert!(backups(directory.path()).is_empty());
    }

    #[test]
    fn parent_directory_of_a_bare_file_name_is_the_working_directory() {
        assert_eq!(parent_directory(Path::new("config.toml")), Path::new("."));
        assert_eq!(
            parent_directory(Path::new("/home/u/.config/otto/config.toml")),
            Path::new("/home/u/.config/otto")
        );
    }

    #[test]
    fn write_refuses_a_write_when_the_file_changed_since_it_was_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        let concurrent = "default_profile = \"written by another otto\"\n";
        let path = write(dir.path(), "config.toml", concurrent);

        let error = write_bytes(&path, b"what this process read", &profile_file("mine"))
            .expect_err("stale write");

        assert!(error.to_string().contains("changed on disk"), "{error}");
        assert_eq!(fs::read_to_string(&path).expect("read"), concurrent);
        assert!(backups(dir.path()).is_empty());
    }

    #[test]
    fn write_creates_a_persistent_lock_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");

        write_bytes(&path, b"", &profile_file("first")).expect("write");

        let lock = dir.path().join("config.toml.lock");
        let mode = fs::metadata(&lock)
            .expect("lock metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "mode = {mode:#o}");
    }

    #[test]
    fn write_waits_for_the_configuration_lock() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let result = dir.path().join("result.txt");
        let held = lock_config(&path).expect("hold lock");
        let mut child = config_lock_child(&path, &result);

        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(300) {
            assert!(
                child.try_wait().expect("poll child").is_none(),
                "writer must wait for the lock"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        drop(held);
        let status = child.wait().expect("wait child");
        assert!(status.success(), "child status = {status}");
        assert_eq!(fs::read_to_string(&result).expect("result"), "ok");
        assert_eq!(fs::read(&path).expect("read"), profile_file("child"));
    }

    #[test]
    fn write_serializes_stale_writers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let first = dir.path().join("first.txt");
        let second = dir.path().join("second.txt");
        let mut first_child = config_lock_child(&path, &first);
        let mut second_child = config_lock_child(&path, &second);

        let first_status = first_child.wait().expect("first wait");
        let second_status = second_child.wait().expect("second wait");
        assert!(first_status.success(), "first status = {first_status}");
        assert!(second_status.success(), "second status = {second_status}");
        let mut results = [
            fs::read_to_string(&first).expect("first result"),
            fs::read_to_string(&second).expect("second result"),
        ];
        results.sort();
        assert_eq!(
            results,
            [
                "err:the configuration changed on disk; rerun to apply this change".to_string(),
                "ok".to_string(),
            ]
        );
        assert_eq!(fs::read(&path).expect("read"), profile_file("child"));
    }

    #[test]
    fn write_serializes_stale_threads_in_one_process() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let held = CONFIG_WRITE_LOCK.lock().expect("hold process lock");
        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let writer_path = path.clone();
        let writer = thread::spawn(move || {
            started_tx.send(()).expect("started");
            let result = write_bytes(&writer_path, b"", &profile_file("thread"));
            done_tx.send(result).expect("done");
        });

        started_rx.recv().expect("writer started");
        thread::sleep(Duration::from_millis(50));
        assert!(
            done_rx.try_recv().is_err(),
            "writer must wait for the process lock"
        );
        drop(held);
        done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("writer finished")
            .expect("write succeeds after unlock");
        writer.join().expect("writer thread");
        assert_eq!(fs::read(&path).expect("read"), profile_file("thread"));
    }

    fn config_lock_child(path: &Path, result: &Path) -> std::process::Child {
        Command::new(std::env::current_exe().expect("current exe"))
            .arg("--exact")
            .arg("config::tests::config_lock_child_entry")
            .arg("--nocapture")
            .env("OTTO_CONFIG_LOCK_CHILD_PATH", path)
            .env("OTTO_CONFIG_LOCK_CHILD_RESULT", result)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn child")
    }

    #[test]
    fn config_lock_child_entry() {
        let Some(path) = std::env::var_os("OTTO_CONFIG_LOCK_CHILD_PATH").map(PathBuf::from) else {
            return;
        };
        let result =
            PathBuf::from(std::env::var_os("OTTO_CONFIG_LOCK_CHILD_RESULT").expect("result path"));
        let text = match write_bytes(&path, b"", &profile_file("child")) {
            Ok(()) => "ok".to_string(),
            Err(error) => format!("err:{error}"),
        };
        fs::write(result, text).expect("write result");
    }

    #[test]
    fn write_backs_up_the_replaced_contents() {
        let dir = tempfile::tempdir().expect("tempdir");
        let original = "default_profile = \"old\"\n# a comment worth recovering\n";
        let path = write(dir.path(), "config.toml", original);

        write_bytes(&path, original.as_bytes(), &profile_file("new")).expect("write");

        let backups = backups(dir.path());
        assert_eq!(backups.len(), 1, "{backups:?}");
        assert_eq!(
            fs::read_to_string(&backups[0]).expect("read backup"),
            original
        );
        let mode = fs::metadata(&backups[0])
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "mode = {mode:#o}");
    }

    #[test]
    fn write_writes_no_backup_when_there_was_no_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");

        write_bytes(&path, b"", &profile_file("first")).expect("write");

        assert!(backups(dir.path()).is_empty());
    }

    /// Backups are pruned by name, so the names of writes that land in the
    /// same millisecond have to sort in write order like every other pair.
    #[test]
    fn write_keeps_the_newest_backups_of_a_shared_millisecond() {
        let dir = tempfile::tempdir().expect("tempdir");
        let directory = dir.path().join("backups");
        fs::create_dir_all(&directory).expect("create backups");
        let stamp = "20260923T101112.500Z";
        let written: Vec<String> = (0..13)
            .map(|attempt| {
                let name = backup_name(stamp, attempt);
                write(&directory, &name, &format!("p{attempt}"));
                name
            })
            .collect();

        prune(&directory);

        let kept: Vec<String> = backups(dir.path())
            .iter()
            .map(|path| {
                path.file_name()
                    .expect("file name")
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(
            kept,
            written[3..],
            "prune must drop the three oldest, not the newest"
        );
    }

    #[test]
    fn write_keeps_only_the_most_recent_backups() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        for index in 0..13 {
            let current = fs::read(&path).unwrap_or_default();
            write_bytes(&path, &current, &profile_file(&format!("p{index}"))).expect("write");
        }

        let backups = backups(dir.path());
        assert_eq!(backups.len(), 10, "{backups:?}");
        let oldest = fs::read_to_string(&backups[0]).expect("read oldest");
        let newest = fs::read_to_string(&backups[9]).expect("read newest");
        assert!(oldest.contains("\"p2\""), "{oldest}");
        assert!(newest.contains("\"p11\""), "{newest}");
    }

    #[test]
    fn set_default_profile_backs_up_the_replaced_contents() {
        let dir = tempfile::tempdir().expect("tempdir");
        let default = dir.path().join("unrelated-default.toml");
        let original = "default_profile = \"old\"\n[profiles.old]\nprovider = \"chatgpt\"\n[profiles.new]\nprovider = \"chatgpt\"\n";
        let path = write(dir.path(), "config.toml", original);

        set_default_profile_file_impl(&path, &default, "new").expect("set_default_profile_file");

        let backups = backups(dir.path());
        assert_eq!(backups.len(), 1, "{backups:?}");
        assert_eq!(
            fs::read_to_string(&backups[0]).expect("read backup"),
            original
        );
    }

    #[test]
    fn default_path_uses_home_dir() {
        assert_eq!(
            default_path_for_home(Some("/home/u")),
            PathBuf::from("/home/u/.config/otto/config.toml")
        );
    }

    #[test]
    fn default_path_falls_back_to_literal_tilde_when_home_is_absent_or_empty() {
        assert_eq!(
            default_path_for_home(None),
            PathBuf::from("~/.config/otto/config.toml")
        );
        assert_eq!(
            default_path_for_home(Some("")),
            PathBuf::from("~/.config/otto/config.toml")
        );
    }

    #[test]
    fn load_returns_empty_file_for_missing_default_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let default = dir.path().join("config.toml");
        let file = load_impl(&default, &default).expect("load");
        assert_eq!(file, File::default());
    }

    #[test]
    fn load_rejects_missing_explicit_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("missing.toml");
        let default = dir.path().join("elsewhere.toml");
        let err = load_impl(&path, &default).unwrap_err();
        assert!(err.is_not_found(), "{err}");
    }

    #[test]
    fn load_required_never_treats_missing_path_as_implicit_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let err = load_required(&path).unwrap_err();
        assert!(err.is_not_found(), "{err}");
    }

    #[test]
    fn load_required_decodes_without_consulting_default_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(
            dir.path(),
            "config.toml",
            "default_profile = \"local\"\n[profiles.local]\nprovider = \"openai-compatible\"\n",
        );
        let file = load_required(&path).expect("load_required");
        assert_eq!(file.default_profile, "local");
        assert_eq!(file.profiles["local"].provider, "openai-compatible");
    }

    #[test]
    fn load_rejects_unknown_fields() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(
            dir.path(),
            "config.toml",
            "default_profile = \"local\"\nunknown = true\n",
        );
        let err = load_required(&path).unwrap_err();
        assert!(err.to_string().contains("unknown"), "{err}");
    }

    #[test]
    fn write_creates_the_directory_and_restricts_permissions() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("config.toml");
        write_bytes(&path, b"", &profile_file("local")).expect("write");

        let reloaded = load_required(&path).expect("load_required");
        assert_eq!(reloaded.default_profile, "local");

        let mode = fs::metadata(&path).expect("metadata").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "mode = {mode:#o}");
    }

    #[test]
    fn set_default_profile_updates_existing_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        let default = dir.path().join("unrelated-default.toml");
        let path = write(
            dir.path(),
            "config.toml",
            "default_profile = \"old\"\n[profiles.old]\nprovider = \"openai-compatible\"\nmodel = \"old-model\"\nbase_url = \"https://old.example/v1\"\napi_key_env = \"OLD_KEY\"\n[profiles.new]\nprovider = \"chatgpt\"\nmodel = \"gpt-5-codex\"\n",
        );
        set_default_profile_file_impl(&path, &default, "new").expect("set_default_profile_file");

        let content = fs::read_to_string(&path).expect("read back");
        assert!(content.contains("default_profile = \"new\""));
        assert!(!content.contains("default_profile = \"old\""));

        let file = load_required(&path).expect("load_required");
        assert_eq!(file.default_profile, "new");
        assert_eq!(file.profiles["old"].model, "old-model");
        assert_eq!(file.profiles["new"].provider, "chatgpt");
    }

    #[test]
    fn set_default_profile_changes_only_the_value() {
        let dir = tempfile::tempdir().expect("tempdir");
        let default = dir.path().join("unrelated-default.toml");
        let original = "# settings\n\ndefault_profile = \"old\" # picked by /model\n\n# profiles below\n[profiles.old]\nprovider = \"chatgpt\"\n[profiles.new]\nprovider = \"chatgpt\"\n";
        let path = write(dir.path(), "config.toml", original);

        set_default_profile_file_impl(&path, &default, "new").expect("set_default_profile_file");

        assert_eq!(
            fs::read_to_string(&path).expect("read back"),
            original.replace("\"old\" #", "\"new\" #")
        );
    }

    #[test]
    fn set_profile_thinking_changes_only_the_value() {
        let dir = tempfile::tempdir().expect("tempdir");
        let original = "default_profile = \"a\"\n\n# the main profile\n[profiles.a]\nprovider = \"chatgpt\"\nmodel = \"m\" # fast\nthinking = \"low\"   # cheap\n\n# a second one\n[profiles.b]\nprovider = \"chatgpt\"\n\n# trailing note\n";
        let path = write(dir.path(), "config.toml", original);

        set_profile_thinking_file(&path, "a", "high").expect("set thinking");
        let replaced = original.replace("\"low\"   #", "\"high\"   #");
        assert_eq!(fs::read_to_string(&path).expect("read back"), replaced);

        set_profile_thinking_file(&path, "b", "max").expect("insert thinking");
        assert_eq!(
            fs::read_to_string(&path).expect("read back"),
            replaced.replace(
                "[profiles.b]\nprovider = \"chatgpt\"\n",
                "[profiles.b]\nprovider = \"chatgpt\"\nthinking = \"max\"\n"
            )
        );
    }

    #[test]
    fn set_default_profile_inserts_missing_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        let default = dir.path().join("unrelated-default.toml");
        let path = write(
            dir.path(),
            "config.toml",
            "[profiles.new]\nprovider = \"chatgpt\"\nmodel = \"gpt-5-codex\"\n",
        );
        set_default_profile_file_impl(&path, &default, "new").expect("set_default_profile_file");

        let content = fs::read_to_string(&path).expect("read back");
        assert!(
            content.starts_with("default_profile = \"new\"\n"),
            "{content:?}"
        );
    }

    #[test]
    fn set_default_profile_rejects_unknown_profile() {
        let dir = tempfile::tempdir().expect("tempdir");
        let default = dir.path().join("unrelated-default.toml");
        let path = write(
            dir.path(),
            "config.toml",
            "[profiles.known]\nprovider = \"chatgpt\"\nmodel = \"gpt-5-codex\"\n",
        );
        let err = set_default_profile_file_impl(&path, &default, "missing").unwrap_err();
        assert!(
            err.to_string().contains("profile \"missing\" not found"),
            "{err}"
        );
    }

    #[test]
    fn set_default_profile_rejects_empty_profile_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let default = dir.path().join("unrelated-default.toml");
        let path = dir.path().join("config.toml");
        let err = set_default_profile_file_impl(&path, &default, "").unwrap_err();
        assert!(err.to_string().contains("missing profile"), "{err}");
    }

    #[test]
    fn profile_change_preview_commits_a_redacted_byte_preserving_field_edit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let original = "# retained\ndefault_profile = \"local\"\n[profiles.local]\nprovider = \"openai-compatible\"\nmodel = \"old\" # comment\nbase_url = \"https://example.test/v1\"\napi_key_env = \"LOCAL_KEY\"\n";
        let path = write(dir.path(), "config.toml", original);
        let preview = preview_profile_change(
            &path,
            &ProfileChange::SetField {
                profile: "local".into(),
                field: ProfileField::Model,
                value: "new".into(),
            },
        )
        .expect("preview");

        assert_eq!(preview.original, original.as_bytes());
        assert_eq!(
            String::from_utf8(preview.updated.clone()).expect("utf8"),
            original.replace("\"old\" #", "\"new\" #")
        );
        assert_eq!(preview.metadata.operation, "set profile field");
        assert!(!preview.redacted_diff.contains("new"));
        commit_profile_change(&path, &preview).expect("commit");
        assert_eq!(fs::read(&path).expect("read"), preview.updated);
    }

    #[test]
    fn profile_change_rejects_invalid_names_and_provider_specific_fields() {
        let dir = tempfile::tempdir().expect("tempdir");
        let default = dir.path().join("unrelated-default.toml");
        let path = write(
            dir.path(),
            "config.toml",
            "default_profile = \"chat\"\n[profiles.chat]\nprovider = \"chatgpt\"\nmodel = \"gpt\"\n",
        );
        let invalid_name = preview_profile_change_impl(
            &path,
            &default,
            &ProfileChange::SetDefault {
                profile: "bad.name".into(),
            },
        )
        .expect_err("invalid name");
        assert!(invalid_name.to_string().contains("invalid profile name"));

        let chatgpt_base_url = preview_profile_change_impl(
            &path,
            &default,
            &ProfileChange::SetField {
                profile: "chat".into(),
                field: ProfileField::BaseUrl,
                value: "https://example.test/v1".into(),
            },
        )
        .expect_err("chatgpt base url");
        assert!(
            chatgpt_base_url
                .to_string()
                .contains("chatgpt does not support")
        );

        let missing_base_url = preview_profile_change_impl(
            &path,
            &default,
            &ProfileChange::Create {
                profile: "local".into(),
                provider: ProfileProvider::OpenAiCompatible,
                model: "model".into(),
                thinking: String::new(),
                base_url: None,
                api_key_env: None,
            },
        )
        .expect_err("openai-compatible needs base url");
        assert!(missing_base_url.to_string().contains("missing base_url"));
    }

    #[test]
    fn profile_change_rejects_default_and_last_profile_removal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let default = dir.path().join("unrelated-default.toml");
        let path = write(
            dir.path(),
            "config.toml",
            "default_profile = \"only\"\n[profiles.only]\nprovider = \"chatgpt\"\nmodel = \"gpt\"\n",
        );
        let last = preview_profile_change_impl(
            &path,
            &default,
            &ProfileChange::Remove {
                profile: "only".into(),
            },
        )
        .expect_err("last profile");
        assert!(last.to_string().contains("last profile"));

        fs::write(
            &path,
            "default_profile = \"only\"\n[profiles.only]\nprovider = \"chatgpt\"\nmodel = \"gpt\"\n[profiles.other]\nprovider = \"chatgpt\"\nmodel = \"gpt\"\n",
        )
        .expect("rewrite fixture");
        let default_profile = preview_profile_change_impl(
            &path,
            &default,
            &ProfileChange::Remove {
                profile: "only".into(),
            },
        )
        .expect_err("default profile");
        assert!(default_profile.to_string().contains("default profile"));
    }

    #[test]
    fn resolution_environment_reads_only_known_keys_present_in_process_env() {
        let mut file = File::default();
        file.profiles.insert(
            "local".into(),
            otto_core::config::Profile {
                api_key_env: "OTTO_TEST_NONEXISTENT_KEY_XYZ".into(),
                ..Default::default()
            },
        );
        let environment = resolution_environment(&file);
        // The fixed keys and the profile's api_key_env are looked up, but
        // nothing is inserted for a name that isn't actually set in this
        // process's environment.
        assert!(!environment.contains_key("OTTO_TEST_NONEXISTENT_KEY_XYZ"));
    }
}
