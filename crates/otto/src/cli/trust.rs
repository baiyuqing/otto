//! `otto trust <dir>`: records a directory as trusted in `config.toml`.
//!
//! Dispatched before the main flag set is parsed, like `otto sandbox setup`
//! and `otto mcp`, because its argument grammar is its own. The table is
//! appended to the file as text through `otto_core::config::edit`, not written
//! through a schema round trip, so existing comments and formatting survive; [`otto_core::config::projects`]
//! is only the schema `otto serve`'s admission reads back. The write goes
//! through [`crate::config::write_bytes`], so it takes a backup and refuses
//! when another process changed the file in between.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use super::run::fail;
use super::sandbox_runtime::canonical_directory;

const USAGE: &str = "usage: otto trust <dir> [--config PATH]";

struct Flags {
    dir: String,
    config_path: String,
}

/// `otto trust <dir>` and `otto trust <dir> --config PATH`. `<dir>` is the
/// only positional argument; `--config` may appear before or after it.
fn parse_flags(args: &[String]) -> Result<Option<Flags>, ()> {
    let mut dir: Option<String> = None;
    let mut config_path = String::new();
    let mut index = 0;
    while index < args.len() {
        let argument = args[index].as_str();
        if matches!(argument, "-h" | "-help" | "--help") {
            return Ok(None);
        }
        if argument.starts_with('-') && argument != "-" {
            let (name, inline) = match argument.split_once('=') {
                Some((name, value)) => (name, Some(value.to_string())),
                None => (argument, None),
            };
            if !matches!(name.trim_start_matches('-'), "config") {
                return Err(());
            }
            let value = match inline {
                Some(value) => value,
                None => {
                    index += 1;
                    args.get(index).cloned().ok_or(())?
                }
            };
            config_path = value;
            index += 1;
            continue;
        }
        if dir.is_some() {
            return Err(());
        }
        dir = Some(argument.to_string());
        index += 1;
    }
    let dir = dir.ok_or(())?;
    Ok(Some(Flags { dir, config_path }))
}

/// Runs `otto trust <dir>` and returns its exit code.
pub fn run(
    args: &[String],
    stdout: &mut (dyn Write + Send),
    stderr: &mut (dyn Write + Send),
    lookup: &HashMap<String, String>,
) -> i32 {
    let flags = match parse_flags(args) {
        Ok(Some(flags)) => flags,
        Ok(None) => {
            let _ = writeln!(stdout, "{USAGE}");
            return 0;
        }
        Err(()) => return fail(stderr, USAGE),
    };
    let path = match flags.config_path.is_empty() {
        true => {
            let Ok(home) = super::run::resolve_home_for(lookup) else {
                return fail(stderr, "cannot resolve home directory");
            };
            let path: PathBuf = [&home, ".config", "otto", "config.toml"].iter().collect();
            path
        }
        false => PathBuf::from(&flags.config_path),
    };
    let Ok(path) = std::path::absolute(&path) else {
        return fail(stderr, "invalid configuration path");
    };
    match trust_directory(&path, Path::new(&flags.dir)) {
        Ok(canonical) => {
            let _ = writeln!(stdout, "Trusted {canonical}.");
            0
        }
        Err(message) => fail(stderr, &message),
    }
}

/// Records `dir` as trusted in the config file at `config_path` (absolute)
/// and returns its canonical path. Shared by `otto trust` and `otto serve`'s
/// `POST /v1/workspaces` with `trust: true`. Nothing is written when `dir` is
/// already listed under `[projects]`.
pub(crate) fn trust_directory(config_path: &Path, dir: &Path) -> Result<String, String> {
    let Ok(canonical) = canonical_directory(dir) else {
        return Err(format!("not a directory: {}", dir.display()));
    };
    let canonical = canonical.to_string_lossy().into_owned();
    let original = match std::fs::read(config_path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(_) => return Err("cannot read configuration".to_string()),
    };
    if let Some(updated) = compute_update(&original, &canonical)? {
        crate::config::write_bytes(config_path, &original, &updated)
            .map_err(|message| format!("cannot save configuration: {message}"))?;
    }
    Ok(canonical)
}

/// The bytes to write for `canonical`, or `None` when it is already trusted
/// and nothing needs to change.
///
/// `original` is parsed rather than assumed empty so a directory already
/// present under `[projects]` — trusted or not — is never appended a second
/// time, which would otherwise produce a duplicate TOML table. The appended
/// table is checked to parse as that one project, so a `projects` written as
/// an inline table is refused instead of turned into an invalid file.
fn compute_update(original: &[u8], canonical: &str) -> Result<Option<Vec<u8>>, String> {
    let text = String::from_utf8_lossy(original);
    let file = otto_core::config::parse(&text)
        .map_err(|error| format!("invalid configuration: {error}"))?;
    if file.projects.contains_key(canonical) {
        return Ok(None);
    }
    let mut body = toml::Table::new();
    body.insert("trust_level".into(), "trusted".into());
    let updated = otto_core::config::edit::insert_table(&text, &["projects", canonical], body)
        .map_err(|error| error.to_string())?;
    Ok(Some(updated.into_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn words(args: &[&str]) -> Vec<String> {
        args.iter().map(|value| (*value).to_owned()).collect()
    }

    fn run_cli(home: &Path, args: &[&str]) -> (i32, String, String) {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut lookup = HashMap::new();
        lookup.insert("HOME".to_string(), home.to_string_lossy().into_owned());
        let code = run(&words(args), &mut stdout, &mut stderr, &lookup);
        (
            code,
            String::from_utf8(stdout).expect("stdout"),
            String::from_utf8(stderr).expect("stderr"),
        )
    }

    #[test]
    fn appends_the_table_and_a_second_run_writes_nothing() {
        let home = TempDir::new().expect("home");
        let dir = TempDir::new().expect("dir");
        let config = home.path().join(".config/otto/config.toml");
        std::fs::create_dir_all(config.parent().expect("parent")).expect("mkdir");
        std::fs::write(
            &config,
            "# a comment worth keeping\ndefault_profile = \"x\"\n",
        )
        .expect("write config");
        let canonical = std::fs::canonicalize(dir.path()).expect("canonical");

        let (code, stdout, stderr) = run_cli(home.path(), &[&dir.path().to_string_lossy()]);
        assert_eq!(code, 0, "{stderr}");
        assert!(stdout.contains("Trusted"), "{stdout}");
        let text = std::fs::read_to_string(&config).expect("read back");
        assert!(text.contains("# a comment worth keeping"), "{text}");
        assert!(text.contains("default_profile = \"x\""), "{text}");
        assert!(
            text.contains(&format!(
                "[projects.\"{}\"]\ntrust_level = \"trusted\"",
                canonical.display()
            )),
            "{text}"
        );

        let after_first = std::fs::read_to_string(&config).expect("read back");
        let (code, _, stderr) = run_cli(home.path(), &[&dir.path().to_string_lossy()]);
        assert_eq!(code, 0, "{stderr}");
        let after_second = std::fs::read_to_string(&config).expect("read back");
        assert_eq!(after_first, after_second, "a second run changed the file");
    }

    #[test]
    fn a_missing_path_or_a_file_fails_and_writes_nothing() {
        let home = TempDir::new().expect("home");
        let config = home.path().join(".config/otto/config.toml");
        std::fs::create_dir_all(config.parent().expect("parent")).expect("mkdir");
        std::fs::write(&config, "default_profile = \"x\"\n").expect("write config");
        let before = std::fs::read_to_string(&config).expect("read");

        let missing = home.path().join("does-not-exist");
        let (code, _, stderr) = run_cli(home.path(), &[&missing.to_string_lossy()]);
        assert_ne!(code, 0);
        assert!(stderr.contains("not a directory"), "{stderr}");

        let file = home.path().join("a-file");
        std::fs::write(&file, b"x").expect("write file");
        let (code, _, stderr) = run_cli(home.path(), &[&file.to_string_lossy()]);
        assert_ne!(code, 0);
        assert!(stderr.contains("not a directory"), "{stderr}");

        assert_eq!(std::fs::read_to_string(&config).expect("read"), before);
    }

    #[test]
    fn refuses_a_concurrent_edit_using_the_existing_compare_and_swap() {
        let directory = TempDir::new().expect("dir");
        let trusted = TempDir::new().expect("trusted");
        let path = directory.path().join("config.toml");
        let original = b"default_profile = \"x\"\n".to_vec();
        std::fs::write(&path, &original).expect("write");
        let canonical = std::fs::canonicalize(trusted.path())
            .expect("canonical")
            .to_string_lossy()
            .into_owned();
        let updated = compute_update(&original, &canonical)
            .expect("compute")
            .expect("some update");

        std::fs::write(&path, b"default_profile = \"changed by another otto\"\n").expect("write");

        assert_eq!(
            crate::config::write_bytes(&path, &original, &updated),
            Err("the configuration changed on disk; rerun to apply this change")
        );
    }

    #[test]
    fn already_trusted_directory_computes_no_update() {
        let trusted = TempDir::new().expect("trusted");
        let canonical = std::fs::canonicalize(trusted.path())
            .expect("canonical")
            .to_string_lossy()
            .into_owned();
        let original = format!("[projects.{canonical:?}]\ntrust_level = \"trusted\"\n");
        assert_eq!(
            compute_update(original.as_bytes(), &canonical).expect("compute"),
            None
        );
    }

    #[test]
    fn a_path_with_control_characters_still_writes_valid_toml() {
        let canonical = "/tmp/a\nb\t\"c\\d\u{7f}";
        let updated = compute_update(b"", canonical)
            .expect("compute")
            .expect("an update");
        let file = otto_core::config::parse(&String::from_utf8(updated).expect("utf-8"))
            .expect("the written file parses");
        assert!(file.projects.contains_key(canonical));
    }

    #[test]
    fn an_inline_projects_table_is_refused_instead_of_written_invalid() {
        let err = compute_update(b"projects = {}\n", "/tmp/whatever").unwrap_err();
        assert!(err.contains("configuration was not changed"), "{err}");
    }

    #[test]
    fn an_invalid_existing_configuration_is_reported_and_nothing_is_written() {
        let err = compute_update(b"not = valid = toml", "/tmp/whatever").unwrap_err();
        assert!(err.contains("invalid"), "{err}");
    }

    #[test]
    fn unknown_flag_and_missing_directory_argument_are_rejected() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run(
            &words(&["--nope"]),
            &mut stdout,
            &mut stderr,
            &HashMap::new(),
        );
        assert_eq!(code, 1);
        let stderr_text = String::from_utf8_lossy(&stderr).into_owned();
        assert!(stderr_text.contains(USAGE), "{stderr_text}");

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run(&words(&[]), &mut stdout, &mut stderr, &HashMap::new());
        assert_eq!(code, 1);
        let stderr_text = String::from_utf8_lossy(&stderr).into_owned();
        assert!(stderr_text.contains(USAGE), "{stderr_text}");
    }

    #[test]
    fn help_flag_prints_usage_and_succeeds() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run(&words(&["-h"]), &mut stdout, &mut stderr, &HashMap::new());
        assert_eq!(code, 0);
        assert_eq!(String::from_utf8_lossy(&stdout), format!("{USAGE}\n"));
        assert!(stderr.is_empty());
    }
}
