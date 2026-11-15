//! Process environment capture, home and configuration loading.

use super::*;

pub(super) fn environment_snapshot(
    host_entries: &[Vec<u8>],
    provider_names: Vec<String>,
) -> EnvironmentSnapshot {
    let options = EnvironmentOptions {
        host_entries: host_entries.to_vec(),
        provider_names,
        ..EnvironmentOptions::default()
    };
    match resolve_environment(&options) {
        Ok(snapshot) => snapshot,
        Err(rejected) => rejected.snapshot().clone(),
    }
}

/// Bounds the snapshot before anything reads it.
pub fn capture_environment(entries: Vec<Vec<u8>>) -> Result<Vec<Vec<u8>>, String> {
    if entries.len() > MAX_CAPTURED_ENVIRONMENT_ENTRIES {
        return Err(ENVIRONMENT_SNAPSHOT_TOO_LARGE.to_string());
    }
    let mut total = 0usize;
    for entry in &entries {
        if entry.len() > MAX_CAPTURED_ENVIRONMENT_BYTES - total {
            return Err(ENVIRONMENT_SNAPSHOT_TOO_LARGE.to_string());
        }
        total += entry.len();
    }
    Ok(entries)
}

/// Parses `KEY=VALUE` entries, skipping every malformed one and refusing an
/// oversized set outright.
pub fn environment_lookup(entries: &[Vec<u8>]) -> Result<EnvironmentLookup, String> {
    let mut parsed: EnvironmentLookup = HashMap::new();
    let mut total = 0usize;
    for entry in entries {
        if entry.len() > MAX_LOOKUP_ENVIRONMENT_ENTRY_BYTES || entry.contains(&0) {
            continue;
        }
        let Ok(entry) = std::str::from_utf8(entry) else {
            continue;
        };
        let Some((name, value)) = entry.split_once('=') else {
            continue;
        };
        if name.len() > MAX_LOOKUP_ENVIRONMENT_NAME_BYTES || !valid_environment_name(name) {
            continue;
        }
        let previous = parsed.get(name);
        if previous.is_none() && parsed.len() >= MAX_LOOKUP_ENVIRONMENT_ENTRIES {
            return Err(ENVIRONMENT_SNAPSHOT_TOO_LARGE.to_string());
        }
        let mut next_total = total + name.len() + value.len();
        if let Some(previous) = previous {
            next_total -= name.len() + previous.len();
        }
        if next_total > MAX_LOOKUP_ENVIRONMENT_BYTES {
            return Err(ENVIRONMENT_SNAPSHOT_TOO_LARGE.to_string());
        }
        parsed.insert(name.to_string(), value.to_string());
        total = next_total;
    }
    Ok(parsed)
}

fn valid_environment_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    if !(first == b'_' || first.is_ascii_alphabetic()) {
        return false;
    }
    bytes.all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
}

/// `$HOME`, else the passwd entry, made absolute.
pub fn resolve_home_for(lookup: &EnvironmentLookup) -> Result<String, String> {
    resolve_home(lookup)
}

/// The `otto memory` flag grammar names its own config path, so it cannot
/// build a `CliOptions`.
pub fn load_config_for(
    config_path: &str,
    explicit: bool,
    home: &str,
) -> Result<(String, File), String> {
    let path = if config_path.is_empty() {
        PathBuf::from(home).join(".config/otto/config.toml")
    } else {
        PathBuf::from(config_path)
    };
    let file = crate::config::layered::load_user(Path::new(home), &path, explicit)?;
    Ok((path.display().to_string(), file))
}

pub fn config_environment_for(file: &File, lookup: &EnvironmentLookup) -> HashMap<String, String> {
    config_environment(file, lookup)
}

pub fn resolve_home(lookup: &EnvironmentLookup) -> Result<String, String> {
    const FAILED: &str = "resolve home directory";
    let mut home = lookup.get("HOME").cloned().unwrap_or_default();
    if home.is_empty() {
        home = current_user_home().ok_or_else(|| FAILED.to_string())?;
    }
    if home.is_empty() {
        return Err(FAILED.to_string());
    }
    let path = PathBuf::from(&home);
    let absolute = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .map_err(|_| FAILED.to_string())?
            .join(path)
    };
    Ok(absolute.to_string_lossy().into_owned())
}

/// The current user's home directory from the passwd database.
fn current_user_home() -> Option<String> {
    let user = nix::unistd::User::from_uid(nix::unistd::getuid()).ok()??;
    let home = user.dir.to_string_lossy().into_owned();
    (!home.is_empty()).then_some(home)
}

/// Opens `~/.otto/schedule.db`. Without it the process runs with no schedule
/// store and no delivery worker, after one warning.
pub fn open_schedule(
    storage: &crate::runtime::storage::Storage,
    home: &str,
    stderr: &mut (dyn Write + Send),
) -> Option<Arc<dyn crate::schedule::ScheduleStore>> {
    match storage.schedule(&Path::new(home).join(".otto/schedule.db")) {
        Ok(store) => Some(store),
        Err(_) => {
            let _ = writeln!(
                stderr,
                "warning: schedule store unavailable, continuing without scheduled deliveries"
            );
            None
        }
    }
}

/// A missing file at an implicit path is an empty one.
pub(super) fn load_config(
    config_path: &str,
    explicit: bool,
    cwd: &str,
    home: &str,
) -> Result<(String, File), String> {
    let path: PathBuf = if config_path.is_empty() {
        [home, ".config", "otto", "config.toml"].iter().collect()
    } else {
        PathBuf::from(config_path)
    };
    let workspace = if cwd.is_empty() {
        Path::new(".")
    } else {
        Path::new(cwd)
    };
    let loaded = crate::config::layered::load(Path::new(home), &path, explicit, workspace)?;
    Ok((path.to_string_lossy().into_owned(), loaded.file))
}

/// Every fixed key is always present, even when the process has no such
/// variable: the empty string is inserted, and resolution distinguishes
/// "absent" from "empty" nowhere. The map also holds each provider's
/// `api_key_env`, Claude workspace bindings and the `web_search` key names.
pub fn config_environment(file: &File, lookup: &EnvironmentLookup) -> HashMap<String, String> {
    const FIXED_KEYS: [&str; 6] = [
        "HOME",
        "OTTO_PROVIDER",
        "OTTO_MODEL",
        "OTTO_API_KEY",
        "OTTO_UI",
        "OTTO_TRACE",
    ];
    use crate::tool::web::{BRAVE_KEY_ENV, GOOGLE_ENGINE_ENV, GOOGLE_KEY_ENV};
    let mut keys: BTreeSet<&str> = FIXED_KEYS.into_iter().collect();
    keys.extend([
        GOOGLE_KEY_ENV,
        GOOGLE_ENGINE_ENV,
        BRAVE_KEY_ENV,
        "ANTHROPIC_API_KEY",
    ]);
    for provider in file.providers.values() {
        if !provider.api_key_env.is_empty() {
            keys.insert(provider.api_key_env.as_str());
        }
        if !provider.workspace_id_env.is_empty() {
            keys.insert(provider.workspace_id_env.as_str());
        }
    }
    let credential_variables = file.plugins.credential_variables();
    keys.extend(credential_variables.iter().map(String::as_str));
    keys.extend(["PATH", "TMPDIR", "LANG", "TERM"]);
    keys.into_iter()
        .map(|key| {
            (
                key.to_string(),
                lookup.get(key).cloned().unwrap_or_default(),
            )
        })
        .collect()
}
