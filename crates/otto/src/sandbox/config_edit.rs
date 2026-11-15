//! Changes a running session makes to the `[sandbox]` table of the
//! configuration file, behind `/sandbox allow`, `/sandbox network`,
//! `/sandbox exclude` and the bash read-path approval.
//!
//! The interactive `otto sandbox setup` command, which writes the same table
//! from the terminal, stays in `otto_cli::sandbox_setup` and shares
//! [`quote`] from here.

use std::path::{Path, PathBuf};

use crate::hex::to_hex;
use otto_core::config::update_sandbox;

/// One `[sandbox]` change a frontend applies to the configuration file while a
/// session is running, behind `/sandbox allow`, `/sandbox network` and
/// `/sandbox exclude`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxChange {
    /// One absolute, symlink-free path added to `read_paths`.
    AllowReadPath(String),
    /// The `network` mode, `allow` or `deny`.
    Network(String),
    /// One `excluded_commands` entry, a command pattern that runs outside the
    /// sandbox.
    ExcludeCommand(String),
}

impl SandboxChange {
    /// The change as a frontend reports it.
    pub fn summary(&self) -> String {
        match self {
            Self::AllowReadPath(path) => format!("read path {}", quote(path)),
            Self::Network(mode) => format!("network {mode}"),
            Self::ExcludeCommand(entry) => format!("excluded command {}", quote(entry)),
        }
    }
}

impl SandboxChange {
    /// Whether the change widens what a sandboxed command may do, so a command
    /// that failed before it may now succeed.
    pub fn widens(&self) -> bool {
        match self {
            Self::AllowReadPath(_) | Self::ExcludeCommand(_) => true,
            Self::Network(mode) => mode == "allow",
        }
    }

    /// The prompt a frontend submits after a widening change so the model
    /// retries a command the sandbox refused; `None` for a change that does
    /// not widen. `sandbox_summary` is `SandboxInfo::summary()` after the
    /// reload.
    pub fn followup_prompt(&self, sandbox_summary: &str) -> Option<String> {
        self.widens().then(|| {
            format!(
                "The user changed the sandbox configuration: {} is now allowed. \
        Sandbox: {sandbox_summary}. If a command failed because of the sandbox, retry it now.",
                self.summary()
            )
        })
    }

    /// The sentence a frontend appends after saving an `excluded_commands`
    /// entry; `config_path` is the file that now holds it.
    pub(crate) fn exclude_saved_note(&self, config_path: &str) -> String {
        format!(
            "Saved {} in {config_path} and reloaded immediately (existing \
                    entries are not duplicated). Matching simple commands run outside the \
                    sandbox; other approval requirements still apply.",
            self.summary()
        )
    }
}

/// What one amendment replaced, so a caller whose reload failed can put the
/// previous configuration back.
#[derive(Debug, Clone)]
pub(crate) struct Amendment {
    original: Vec<u8>,
    updated: Vec<u8>,
}

/// Resolves a user-supplied read path into the absolute, symlink-free text the
/// `[sandbox]` table stores.
///
/// A relative path resolves against the process working directory, which is
/// the session workspace. The result is canonicalized here rather than at
/// sandbox-build time so the frontend can show what the grant will actually
/// cover, and so a path the profile would reject (one that does not exist, or
/// is neither a directory nor a regular file) is refused before it is written.
pub(crate) fn resolve_read_path(input: &str, home: &str) -> Result<String, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("a path is required".to_string());
    }
    let expanded = match trimmed.strip_prefix("~/") {
        Some(rest) => Path::new(home).join(rest),
        None => PathBuf::from(trimmed),
    };
    let canonical =
        std::fs::canonicalize(&expanded).map_err(|_| format!("no such path: {trimmed}"))?;
    let kind = std::fs::metadata(&canonical).map_err(|_| format!("no such path: {trimmed}"))?;
    if !kind.is_dir() && !kind.is_file() {
        return Err(format!(
            "a read path must be a directory or a regular file: {trimmed}"
        ));
    }
    match canonical.to_str() {
        Some(text) => Ok(text.to_string()),
        None => Err(format!("path is not valid UTF-8: {trimmed}")),
    }
}

/// The entry of `/sandbox exclude <entry>`: the rest of the line, trimmed,
/// with one pair of matching single or double quotes removed.
pub(crate) fn parse_exclude_entry(rest: &str) -> Result<String, String> {
    let trimmed = rest.trim();
    let entry = ['\'', '"']
        .iter()
        .find_map(|quote| {
            trimmed
                .strip_prefix(*quote)
                .and_then(|inner| inner.strip_suffix(*quote))
        })
        .unwrap_or(trimmed)
        .trim();
    if entry.is_empty() {
        return Err(
            "usage: /sandbox exclude <entry>, for example /sandbox exclude gh *".to_string(),
        );
    }
    Ok(entry.to_string())
}

/// Applies `change` to the `[sandbox]` table at `path` and writes the file.
///
/// The write goes through the same [`crate::config::write_bytes`] the
/// interactive setup uses, so a concurrent edit and a non-regular file are
/// both refused and the replaced contents are backed up. A change that
/// would not resolve is rejected before anything is written, and a read path
/// already present is left alone rather than repeated.
pub(crate) fn amend_sandbox_config(
    path: &Path,
    change: &SandboxChange,
) -> Result<Amendment, String> {
    amend_sandbox_config_checked(path, change, None)
}

pub(crate) fn amend_sandbox_config_checked(
    path: &Path,
    change: &SandboxChange,
    expected: Option<&str>,
) -> Result<Amendment, String> {
    let original = match std::fs::read(path) {
        Ok(content) => Some(content),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => return Err("cannot read configuration".to_string()),
    };
    let file = match &original {
        None => otto_core::config::File::default(),
        Some(_) => crate::config::load_required(path)
            .map_err(|_| "configuration is invalid; no changes made".to_string())?,
    };
    let original = original.unwrap_or_default();
    if let Some(expected) = expected {
        use sha2::Digest;
        if to_hex(&sha2::Sha256::digest(&original)) != expected {
            return Err("configuration changed after approval; request approval again".into());
        }
    }

    let mut proposed = file.sandbox.clone();
    match change {
        SandboxChange::AllowReadPath(allowed) => {
            if !proposed.read_paths.contains(allowed) {
                proposed.read_paths.push(allowed.clone());
            }
        }
        SandboxChange::Network(mode) => proposed.network = Some(mode.clone()),
        SandboxChange::ExcludeCommand(entry) => {
            if !proposed.excluded_commands.contains(entry) {
                proposed.excluded_commands.push(entry.clone());
            }
        }
    }
    let updated = update_sandbox(&original, &proposed).map_err(|error| match change {
        SandboxChange::ExcludeCommand(entry)
            if error.to_string() == "invalid sandbox excluded_commands" =>
        {
            format!(
            "invalid excluded command {}: use one simple command, or a program name followed by ' *'",
                quote(entry)
            )
        }
        _ => error.to_string(),
    })?;
    crate::config::write_bytes(path, &original, &updated).map_err(str::to_string)?;
    Ok(Amendment { original, updated })
}

/// Puts back what [`amend_sandbox_config`] replaced, refusing to undo an
/// amendment something else has already written over.
///
/// A configuration file the amendment created is emptied rather than removed,
/// which resolves to the same defaults.
pub(crate) fn revert_sandbox_config(path: &Path, amendment: &Amendment) -> Result<(), String> {
    crate::config::write_bytes(path, &amendment.updated, &amendment.original)
        .map_err(str::to_string)
}

/// `value` as a double-quoted TOML basic string.
pub fn quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{7}' => out.push_str("\\a"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{b}' => out.push_str("\\v"),
            ch if (ch as u32) < 0x20 || ch as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", ch as u32));
            }
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const PRESERVED: &str = "# preserved\n[profiles.demo]\nmodel = 'example'\n";

    #[test]
    fn resolve_read_path_expands_home_and_rejects_what_the_profile_would() {
        let home = TempDir::new().expect("home");
        let home_text = home.path().to_string_lossy().into_owned();
        let cache = home.path().join("cache");
        std::fs::create_dir(&cache).expect("cache");
        let canonical = std::fs::canonicalize(&cache).expect("canonical");

        assert_eq!(
            resolve_read_path("~/cache", &home_text),
            Ok(canonical.to_string_lossy().into_owned())
        );
        assert_eq!(
            resolve_read_path(&cache.to_string_lossy(), &home_text),
            Ok(canonical.to_string_lossy().into_owned())
        );
        assert!(resolve_read_path("  ", &home_text).is_err());
        assert!(resolve_read_path("~/missing", &home_text).is_err());
    }

    #[test]
    fn amending_adds_a_read_path_once_and_preserves_other_tables() {
        let home = TempDir::new().expect("home");
        let path = home.path().join("config.toml");
        std::fs::write(&path, PRESERVED).expect("write config");
        let cache = home.path().join("cache");
        std::fs::create_dir(&cache).expect("cache");
        let resolved =
            resolve_read_path("~/cache", &home.path().to_string_lossy()).expect("resolve");
        let change = SandboxChange::AllowReadPath(resolved.clone());

        amend_sandbox_config(&path, &change).expect("amend");
        amend_sandbox_config(&path, &change).expect("amend again");

        let text = std::fs::read_to_string(&path).expect("read");
        assert!(
            text.contains(&format!("read_paths = ['{resolved}']")),
            "{text}"
        );
        assert!(text.contains("[profiles.demo]"), "{text}");
        assert_eq!(text.matches(resolved.as_str()).count(), 1, "{text}");
    }

    #[test]
    fn amending_sets_the_network_mode_and_reverting_restores_the_bytes() {
        let home = TempDir::new().expect("home");
        let path = home.path().join("config.toml");
        std::fs::write(&path, PRESERVED).expect("write config");

        let amendment = amend_sandbox_config(&path, &SandboxChange::Network("deny".to_string()))
            .expect("amend");
        assert!(
            std::fs::read_to_string(&path)
                .expect("read")
                .contains("network = 'deny'")
        );

        revert_sandbox_config(&path, &amendment).expect("revert");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), PRESERVED);
    }

    #[test]
    fn amending_refuses_an_invalid_change_without_touching_the_file() {
        let home = TempDir::new().expect("home");
        let path = home.path().join("config.toml");
        std::fs::write(&path, PRESERVED).expect("write config");

        let error = amend_sandbox_config(&path, &SandboxChange::Network("maybe".to_string()))
            .expect_err("invalid network");

        assert!(error.contains("invalid sandbox network"), "{error}");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), PRESERVED);
    }

    #[test]
    fn excluding_a_command_appends_it_once() {
        let home = TempDir::new().expect("home");
        let path = home.path().join("config.toml");
        std::fs::write(&path, PRESERVED).expect("write config");
        let change = SandboxChange::ExcludeCommand("acme-cli *".to_string());

        amend_sandbox_config(&path, &change).expect("amend");
        let first = std::fs::read(&path).expect("read");
        amend_sandbox_config(&path, &change).expect("amend again");

        assert_eq!(std::fs::read(&path).expect("read"), first);
        let text = String::from_utf8(first).expect("utf8");
        assert!(
            text.contains("excluded_commands = ['acme-cli *']"),
            "{text}"
        );
        assert_eq!(change.summary(), "excluded command \"acme-cli *\"");
    }

    #[test]
    fn excluding_an_invalid_entry_is_refused_without_touching_the_file() {
        let home = TempDir::new().expect("home");
        let path = home.path().join("config.toml");
        std::fs::write(&path, PRESERVED).expect("write config");

        let error = amend_sandbox_config(&path, &SandboxChange::ExcludeCommand("a; b".to_string()))
            .expect_err("invalid entry");

        assert!(
            error.contains("invalid excluded command \"a; b\""),
            "{error}"
        );
        assert_eq!(std::fs::read_to_string(&path).expect("read"), PRESERVED);
    }

    #[test]
    fn the_exclude_entry_drops_one_pair_of_matching_quotes() {
        assert_eq!(
            parse_exclude_entry("  acme-cli *  "),
            Ok("acme-cli *".to_string())
        );
        assert_eq!(
            parse_exclude_entry("'acme-cli *'"),
            Ok("acme-cli *".to_string())
        );
        assert_eq!(
            parse_exclude_entry("\"acme-cli *\""),
            Ok("acme-cli *".to_string())
        );
        assert_eq!(
            parse_exclude_entry("'acme-cli *\""),
            Ok("'acme-cli *\"".to_string())
        );
        assert!(parse_exclude_entry("").is_err());
        assert!(parse_exclude_entry("''").is_err());
    }
}
