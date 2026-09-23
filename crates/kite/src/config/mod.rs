//! Native filesystem and environment layer for configuration.
//!
//! Loading, saving, and the default path; the pure schema, parsing, and
//! resolution logic lives in `kite_core::config`.
//!
//! ponytail: `default_path`, `load`, and `set_default_profile_file` each
//! have a private `_for_home`/`_impl` twin that takes the otherwise-implicit
//! `HOME` value or default path as an explicit argument. This workspace's
//! `unsafe_code` lint (denied by `make rust-lint`'s `-D warnings`) forbids the
//! `std::env::set_var` edition 2024 needs, so tests call the `_for_home`/
//! `_impl` twin directly instead of mutating the real environment.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use chrono::Utc;

use kite_core::config::{ConfigError, File};

/// An error from the native config layer: either an I/O failure opening,
/// reading, or writing the file, or a [`ConfigError`] from kite-core's pure
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

/// The default config file path: `$HOME/.config/kite/config.toml`, or the
/// literal path `~/.config/kite/config.toml`, left unexpanded, if `HOME` is
/// unset or empty.
pub fn default_path() -> PathBuf {
    default_path_for_home(std::env::var("HOME").ok().as_deref())
}

fn default_path_for_home(home: Option<&str>) -> PathBuf {
    let base = match home {
        Some(home) if !home.is_empty() => home,
        _ => "~",
    };
    [base, ".config", "kite", "config.toml"].iter().collect()
}

/// Reads and parses `path`. Unlike [`load`], a missing file is always an error,
/// even at the default path.
pub fn load_required(path: &Path) -> Result<File, NativeConfigError> {
    let text = fs::read_to_string(path)?;
    Ok(kite_core::config::parse(&text)?)
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
            let file = kite_core::config::parse(&text)?;
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

/// Serializes and writes `file` to `path` through [`write_bytes`], replacing
/// `replacing` — the bytes the caller read `file` from, empty for a file that
/// did not exist.
///
/// Serialization is a full round trip through the schema, so comments and
/// formatting in a hand-edited file are not preserved; the backup
/// [`write_bytes`] takes is what the previous contents can be recovered from.
pub fn save(path: &Path, file: &File, replacing: &[u8]) -> Result<(), NativeConfigError> {
    let text = kite_core::config::to_toml_string(file)?;
    write_bytes(path, replacing, text.as_bytes()).map_err(io_error)
}

/// Replaces `path` with `updated`, after copying the contents it replaces into
/// `backups` beside it.
///
/// The write is a compare-and-swap: `replacing` is the file's contents as the
/// caller read them (empty for a file that did not exist), and a mismatch
/// means another process wrote the file in between, so the write is refused
/// rather than overwriting that change. Several Kite processes can run at
/// once, and each one reads, edits, and writes the whole file, so without this
/// check the last writer would silently drop the others' edits. The check is
/// not atomic against the rename below — a write landing inside that
/// millisecond-scale window is still lost — but it turns the common case
/// (two commands seconds apart) from silent loss into a reported error.
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
    let temp = directory.join(format!(".kite-config-{suffix}"));
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
    let result =
        write.and_then(|()| fs::rename(&temp, path).map_err(|_| "cannot replace configuration"));
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
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
/// the same millisecond) gets a `-N` suffix, which orders arbitrarily within
/// that millisecond and correctly against every other one.
fn back_up(directory: &Path, current: &[u8]) -> Result<(), &'static str> {
    let backups = directory.join("backups");
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&backups)
        .map_err(|_| "cannot create configuration backup directory")?;
    let stamp = Utc::now().format("%Y%m%dT%H%M%S%.3fZ");
    let mut attempt = 0;
    let mut file = loop {
        let name = match attempt {
            0 => format!("config-{stamp}.toml"),
            taken => format!("config-{stamp}-{taken}.toml"),
        };
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(backups.join(name))
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

/// Rewrites `path`'s `default_profile` line to name `profile`, after checking
/// the profile exists in the file.
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
    let (original, mut file) = load_with_bytes(path, default_path)?;
    if !file.profiles.contains_key(profile) {
        return Err(ConfigError::new(format!("profile {profile:?} not found")).into());
    }
    if original.is_empty() {
        file.default_profile = profile.to_string();
        return save(path, &file, &original);
    }
    let content = String::from_utf8_lossy(&original);
    let updated = kite_core::config::set_default_profile(&content, profile);
    write_bytes(path, &original, updated.as_bytes()).map_err(io_error)
}

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
    let (original, mut file) = load_with_bytes(path, &default_path())?;
    let Some(entry) = file.profiles.get_mut(profile) else {
        return Err(ConfigError::new(format!("profile {profile:?} not found")).into());
    };
    entry.thinking = thinking.to_string();
    save(path, &file, &original)
}

/// The environment `kite_core::config::resolve` and `resolve_memory` may
/// consult for `file`: a fixed set of `KITE_*` overrides plus `HOME`, and
/// each profile's `api_key_env`. Exactly these names are read, never the whole
/// process environment, so that resolution can never be influenced by an
/// unrelated or oversized environment, and so a key value never has to be
/// logged or matched against a name pattern to redact it.
pub fn resolution_environment(file: &File) -> HashMap<String, String> {
    const FIXED_KEYS: [&str; 7] = [
        "HOME",
        "KITE_PROVIDER",
        "KITE_PROFILE",
        "KITE_MODEL",
        "KITE_API_KEY",
        "KITE_UI",
        "KITE_TRACE",
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

    fn profile_file(name: &str) -> File {
        File {
            default_profile: name.into(),
            ..File::default()
        }
    }

    #[test]
    fn parent_directory_of_a_bare_file_name_is_the_working_directory() {
        assert_eq!(parent_directory(Path::new("config.toml")), Path::new("."));
        assert_eq!(
            parent_directory(Path::new("/home/u/.config/kite/config.toml")),
            Path::new("/home/u/.config/kite")
        );
    }

    #[test]
    fn save_refuses_a_write_when_the_file_changed_since_it_was_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        let concurrent = "default_profile = \"written by another kite\"\n";
        let path = write(dir.path(), "config.toml", concurrent);

        let error =
            save(&path, &profile_file("mine"), b"what this process read").expect_err("stale write");

        assert!(error.to_string().contains("changed on disk"), "{error}");
        assert_eq!(fs::read_to_string(&path).expect("read"), concurrent);
        assert!(backups(dir.path()).is_empty());
    }

    #[test]
    fn save_backs_up_the_replaced_contents() {
        let dir = tempfile::tempdir().expect("tempdir");
        let original = "default_profile = \"old\"\n# a comment worth recovering\n";
        let path = write(dir.path(), "config.toml", original);

        save(&path, &profile_file("new"), original.as_bytes()).expect("save");

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
    fn save_writes_no_backup_when_there_was_no_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");

        save(&path, &profile_file("first"), b"").expect("save");

        assert!(backups(dir.path()).is_empty());
    }

    #[test]
    fn save_keeps_only_the_most_recent_backups() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        for index in 0..13 {
            let current = fs::read(&path).unwrap_or_default();
            save(&path, &profile_file(&format!("p{index}")), &current).expect("save");
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
            PathBuf::from("/home/u/.config/kite/config.toml")
        );
    }

    #[test]
    fn default_path_falls_back_to_literal_tilde_when_home_is_absent_or_empty() {
        assert_eq!(
            default_path_for_home(None),
            PathBuf::from("~/.config/kite/config.toml")
        );
        assert_eq!(
            default_path_for_home(Some("")),
            PathBuf::from("~/.config/kite/config.toml")
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
    fn save_round_trips_and_restricts_permissions() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("config.toml");
        let file = File {
            default_profile: "local".into(),
            ..File::default()
        };
        save(&path, &file, b"").expect("save");

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
    fn resolution_environment_reads_only_known_keys_present_in_process_env() {
        let mut file = File::default();
        file.profiles.insert(
            "local".into(),
            kite_core::config::Profile {
                api_key_env: "KITE_TEST_NONEXISTENT_KEY_XYZ".into(),
                ..Default::default()
            },
        );
        let environment = resolution_environment(&file);
        // The fixed keys and the profile's api_key_env are looked up, but
        // nothing is inserted for a name that isn't actually set in this
        // process's environment.
        assert!(!environment.contains_key("KITE_TEST_NONEXISTENT_KEY_XYZ"));
    }
}
