//! Native filesystem and environment layer for configuration.
//!
//! Loading, in-place edits, and the default path; the pure schema, parsing, and
//! resolution logic lives in `otto_core::config`.
//!
//! ponytail: `default_path`, `load`, and `set_default_model_file` each
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
    ConfigError, File, Overrides, PROVIDER_CHATGPT, PROVIDER_CLAUDE, PROVIDER_OPENAI_COMPATIBLE,
    SessionDefaults,
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
pub(crate) fn default_path() -> PathBuf {
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
pub fn write_bytes(path: &Path, replacing: &[u8], updated: &[u8]) -> Result<(), &'static str> {
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
    let suffix = crate::random::random_id().map_err(|_| "cannot create temporary configuration")?;
    let temp_name = format!(".otto-config-{suffix}");
    let (parent, name) =
        fsops::open_parent(path).map_err(|_| "cannot create temporary configuration")?;
    fsops::publish_file(
        &parent,
        &Publish {
            name,
            temp: &temp_name,
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

/// Creates a new configuration file that selects one default model.
///
/// The destination must not exist, including as an empty file or symlink. The
/// final install is create-only, so a non-Otto writer that wins a race is never
/// overwritten.
pub fn create_initial_config(
    path: &Path,
    provider: &str,
    model_id: &str,
    base_url: &str,
    api_key_env: &str,
) -> Result<(), String> {
    let content = initial_config_content(provider, model_id, base_url, api_key_env)?;
    create_bytes_if_absent(path, content.as_bytes()).map_err(str::to_string)
}

/// Renders and validates the complete non-secret configuration `otto setup`
/// proposes before a user confirms its creation: `default_model`, and a
/// provider table that names the model as an inline entry.
pub fn initial_config_content(
    provider: &str,
    model_id: &str,
    base_url: &str,
    api_key_env: &str,
) -> Result<String, String> {
    if !matches!(
        provider,
        PROVIDER_CHATGPT | PROVIDER_OPENAI_COMPATIBLE | PROVIDER_CLAUDE
    ) {
        return Err("unsupported provider; no changes made".to_string());
    }
    let quote = |value: &str| toml::Value::String(value.to_string()).to_string();
    let claude_default_url = otto_core::config::providers::CLAUDE_DEFAULT_BASE_URL;
    let claude_default_key = otto_core::config::providers::CLAUDE_DEFAULT_API_KEY_ENV;
    let mut content = format!(
        "default_model = {}\n\n[providers.{provider}]\ntype = {}\n",
        quote(&format!("{provider}/{model_id}")),
        quote(provider)
    );
    if provider == PROVIDER_OPENAI_COMPATIBLE
        || (provider == PROVIDER_CLAUDE && base_url != claude_default_url)
    {
        content.push_str(&format!("base_url = {}\n", quote(base_url)));
    }
    if provider == PROVIDER_OPENAI_COMPATIBLE
        || (provider == PROVIDER_CLAUDE && api_key_env != claude_default_key)
    {
        content.push_str(&format!("api_key_env = {}\n", quote(api_key_env)));
    }
    content.push_str(&format!("models.{} = {{}}\n", quote(model_id)));
    let parsed = otto_core::config::parse(&content)
        .map_err(|error| format!("invalid setup values: {error}"))?;
    let mut environment = HashMap::new();
    if matches!(provider, PROVIDER_OPENAI_COMPATIBLE | PROVIDER_CLAUDE) {
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
    let suffix = crate::random::random_id().map_err(|_| "cannot create temporary configuration")?;
    let temp_path = directory.join(format!(".otto-config-{suffix}"));
    let result = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp_path)
        .map_err(|_| "cannot create temporary configuration")
        .and_then(|mut file| {
            file.write_all(contents)
                .and_then(|()| file.sync_all())
                .map_err(|_| "cannot write configuration")
        })
        .and_then(|()| match fs::hard_link(&temp_path, path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                Err("configuration already exists; no changes made")
            }
            Err(_) => Err("cannot create configuration"),
        })
        .and_then(|()| fs::remove_file(&temp_path).map_err(|_| "cannot create configuration"))
        .and_then(|()| sync_directory(directory));
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
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

/// Sets `path`'s `default_model` to the selector `model`, after checking its
/// provider exists in the file, changing only that value.
pub(crate) fn set_default_model_file(path: &Path, model: &str) -> Result<(), NativeConfigError> {
    set_default_model_file_impl(path, &default_path(), model)
}

fn set_default_model_file_impl(
    path: &Path,
    default_path: &Path,
    model: &str,
) -> Result<(), NativeConfigError> {
    let change = ModelChange::SetDefaultModel {
        model: model.to_string(),
    };
    let preview = preview_model_change_impl(path, default_path, &change).map_err(native_error)?;
    commit_model_change(path, &preview)
}

/// Sets the `thinking` key of the model table `model` (a selector), creating
/// the table when absent, or removes the key when `thinking` is empty.
pub(crate) fn set_model_thinking_file(
    path: &Path,
    model: &str,
    thinking: &str,
) -> Result<(), NativeConfigError> {
    let change = ModelChange::SetModelField {
        model: model.to_string(),
        field: ModelField::Thinking,
        value: thinking.to_string(),
    };
    let preview =
        preview_model_change_impl(path, &default_path(), &change).map_err(native_error)?;
    commit_model_change(path, &preview)
}

fn native_error(error: ModelChangeError) -> NativeConfigError {
    match error {
        ModelChangeError::Native(error) => error,
        ModelChangeError::InUse(message) | ModelChangeError::LegacyProfiles(message) => {
            ConfigError::new(message).into()
        }
    }
}

mod model_change;
mod model_listing;

pub use model_change::*;
pub use model_listing::configured_model_selectors;

#[cfg(test)]
mod model_change_tests;
#[cfg(test)]
mod tests;
