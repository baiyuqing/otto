//! Native filesystem and environment layer for configuration.
//!
//! Port of the file- and environment-touching half of `internal/config`
//! (`config.go`'s `Load`, `LoadRequired`, `Save`, `DefaultPath`,
//! `SetDefaultProfile`, and `main.go`'s `configEnvironment`). The pure
//! schema, parsing, and resolution logic lives in `otto_core::config` and is
//! reused here unchanged.
//!
//! ponytail: `default_path`, `load`, and `set_default_profile_file` each
//! have a private `_for_home`/`_impl` twin that takes the otherwise-implicit
//! `HOME` value or default path as an explicit argument. Go's tests reach
//! these paths with `t.Setenv("HOME", ...)`; this workspace's `unsafe_code`
//! lint (denied by `make rust-lint`'s `-D warnings`) forbids the
//! `std::env::set_var` edition-2024 needs for the same trick, so tests call
//! the `_for_home`/`_impl` twin directly instead of mutating real env.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use otto_core::config::{ConfigError, File};

/// An error from the native config layer: either an I/O failure opening,
/// reading, or writing the file, or a [`ConfigError`] from otto-core's pure
/// parsing/resolution logic. Mirrors Go's plain `error`, which callers there
/// inspect with `os.IsNotExist` or a substring check on `Error()`;
/// [`NativeConfigError::is_not_found`] covers the former, and `Display`
/// (via `thiserror`) covers the latter.
#[derive(Debug, thiserror::Error)]
pub enum NativeConfigError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Config(#[from] ConfigError),
}

impl NativeConfigError {
    /// Mirrors Go's `os.IsNotExist(err)`.
    pub fn is_not_found(&self) -> bool {
        matches!(self, NativeConfigError::Io(err) if err.kind() == io::ErrorKind::NotFound)
    }
}

/// The default config file path: `$HOME/.config/otto/config.toml`, or the
/// literal path `~/.config/otto/config.toml` if `HOME` is unset or empty.
/// Port of Go's `DefaultPath`, including its unexpanded `~` fallback (Go's
/// `os.UserHomeDir()` reads `$HOME` on macOS, same as here).
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

/// Reads and parses `path`. Unlike [`load`], a missing file is always an
/// error, even at the default path. Port of Go's `LoadRequired`.
pub fn load_required(path: &Path) -> Result<File, NativeConfigError> {
    let text = fs::read_to_string(path)?;
    Ok(otto_core::config::parse(&text)?)
}

/// Reads and parses `path`, treating a missing file at the default config
/// path as an empty [`File`] rather than an error. Port of Go's `Load`.
pub fn load(path: &Path) -> Result<File, NativeConfigError> {
    load_impl(path, &default_path())
}

fn load_impl(path: &Path, default_path: &Path) -> Result<File, NativeConfigError> {
    match load_required(path) {
        Err(err) if err.is_not_found() && path == default_path => Ok(File::default()),
        other => other,
    }
}

/// Serializes and writes `file` to `path`, creating parent directories and
/// restricting the file to owner read/write (`0o600`). Port of Go's `Save`.
pub fn save(path: &Path, file: &File) -> Result<(), NativeConfigError> {
    let text = otto_core::config::to_toml_string(file)?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(path, text)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

/// Rewrites `path`'s `default_profile` line to name `profile`, after
/// checking the profile exists in the file. Port of Go's `SetDefaultProfile`.
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
    let file = load_impl(path, default_path)?;
    if !file.profiles.contains_key(profile) {
        return Err(ConfigError::new(format!("profile {profile:?} not found")).into());
    }
    match fs::read_to_string(path) {
        Ok(content) => {
            let updated = otto_core::config::set_default_profile(&content, profile);
            if let Some(dir) = path.parent() {
                fs::create_dir_all(dir)?;
            }
            fs::write(path, &updated)?;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
            Ok(())
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound && path == default_path => {
            let mut file = file;
            file.default_profile = profile.to_string();
            save(path, &file)
        }
        Err(err) => Err(err.into()),
    }
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
    let mut file = load_impl(path, &default_path())?;
    let Some(entry) = file.profiles.get_mut(profile) else {
        return Err(ConfigError::new(format!("profile {profile:?} not found")).into());
    };
    entry.thinking = thinking.to_string();
    save(path, &file)
}

/// The environment `otto_core::config::resolve` and `resolve_memory` may
/// consult for `file`: a fixed set of `OTTO_*` overrides plus `HOME`, and
/// each profile's `api_key_env`. Port of `cmd/otto/main.go`'s
/// `configEnvironment`, which reads exactly these names (never the whole
/// process environment) so that resolution can never be influenced by an
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
    use super::*;

    fn write(dir: &Path, name: &str, content: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, content).expect("write fixture");
        path
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
    fn save_round_trips_and_restricts_permissions() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("config.toml");
        let file = File {
            default_profile: "local".into(),
            ..File::default()
        };
        save(&path, &file).expect("save");

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
