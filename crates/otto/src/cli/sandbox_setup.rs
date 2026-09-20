//! `otto sandbox setup`: the interactive sandbox permission editor.
//!
//! It is dispatched before the main flag set is parsed, because its argument
//! grammar is its own, and it builds no provider, session or controller: it
//! reads the configuration file, asks two questions, and rewrites only the
//! `[sandbox]` table through [`otto_core::config::update_sandbox`].
//!
//! Safety: the file is written by rename from a sibling temporary file, and
//! only when the bytes on disk still match what setup read. A configuration
//! path that is not a regular file is refused rather than followed, so a
//! symlink planted during the prompts cannot redirect the write.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use otto_core::config::{SandboxNetworkMode, resolve_sandbox, update_sandbox};
use tokio_util::sync::CancellationToken;

use crate::sandbox::{CommandExecutor, Request, Streams};

use super::run::{fail, sandbox_provider_environment_names};
use super::sandbox_runtime::{
    OpenOptions, SandboxRuntime, canonical_directory, open_sandbox_runtime, settings_from_config,
};

const USAGE: &str = "usage: otto sandbox setup [--config PATH] [--cwd PATH]";

/// How long the optional `check` step may take.
const CHECK_TIMEOUT: Duration = Duration::from_secs(15);

/// The probe `check` runs when GitHub CLI access was requested. The exit codes
/// are the vocabulary [`report_check`] reads.
const GH_PROBE: &str = r#"command -v gh >/dev/null || exit 20; test -d "$GH_CONFIG_DIR" && test -r "$GH_CONFIG_DIR" || exit 21; gh --version >/dev/null 2>&1 || exit 22"#;

/// What the subcommand's own flags resolved to.
struct Flags {
    config_path: String,
    cwd: String,
}

/// The two flags this command defines: `--name value` and `--name=value`, one
/// leading dash or two, with `-h`/`-help` asking for the usage line and
/// anything else rejected.
fn parse_flags(args: &[String]) -> Result<Option<Flags>, ()> {
    let mut flags = Flags {
        config_path: String::new(),
        cwd: ".".to_string(),
    };
    let mut index = 0;
    while index < args.len() {
        let argument = args[index].as_str();
        // Parsing stops at the first non-flag argument, which this command
        // rejects either way.
        if !argument.starts_with('-') || argument == "-" || argument == "--" {
            return Err(());
        }
        let (name, inline) = match argument.split_once('=') {
            Some((name, value)) => (name, Some(value.to_string())),
            None => (argument, None),
        };
        let name = name.trim_start_matches('-');
        if matches!(name, "h" | "help") {
            return Ok(None);
        }
        if !matches!(name, "config" | "cwd") {
            return Err(());
        }
        let value = match inline {
            Some(value) => value,
            None => {
                index += 1;
                args.get(index).cloned().ok_or(())?
            }
        };
        if name == "config" {
            flags.config_path = value;
        } else {
            flags.cwd = value;
        }
        index += 1;
    }
    Ok(Some(flags))
}

/// Runs `otto sandbox ...` and returns its exit code.
pub async fn run(
    args: &[String],
    stdin: &mut (dyn BufRead + Send),
    stdout: &mut (dyn Write + Send),
    stderr: &mut (dyn Write + Send),
    host_entries: &[Vec<u8>],
    lookup: &HashMap<String, String>,
    cancel: &CancellationToken,
) -> i32 {
    if args.first().map(String::as_str) != Some("setup") {
        return fail(stderr, USAGE);
    }
    let flags = match parse_flags(&args[1..]) {
        Ok(Some(flags)) => flags,
        Ok(None) => {
            let _ = writeln!(stdout, "{USAGE}");
            return 0;
        }
        Err(()) => return fail(stderr, USAGE),
    };
    let Ok(home) = super::run::resolve_home_for(lookup) else {
        return fail(stderr, "cannot resolve home directory");
    };
    let path: PathBuf = match flags.config_path.is_empty() {
        true => [&home, ".config", "otto", "config.toml"].iter().collect(),
        false => PathBuf::from(&flags.config_path),
    };
    let Ok(path) = std::path::absolute(&path) else {
        return fail(stderr, "invalid configuration path");
    };
    let original = match std::fs::read(&path) {
        Ok(content) => Some(content),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => return fail(stderr, "cannot read configuration"),
    };
    let file = match &original {
        None => otto_core::config::File::default(),
        Some(_) => match crate::config::load_required(&path) {
            Ok(file) => file,
            Err(_) => return fail(stderr, "configuration is invalid; no changes made"),
        },
    };
    let original = original.unwrap_or_default();
    let Ok(workspace) = canonical_directory(Path::new(&flags.cwd)) else {
        return fail(stderr, "workspace is unavailable");
    };
    let workspace = workspace.to_string_lossy().into_owned();
    let Ok(settings) = resolve_sandbox(&file.sandbox, None) else {
        return fail(stderr, "sandbox configuration is invalid");
    };

    let _ = write!(
        stdout,
        "Sandbox setup for {}\nShell commands can modify the whole workspace. Home files are hidden unless explicitly allowed.\nExisting extra permissions are retained and shown below. Setup enables Seatbelt even if sandbox was off.\n",
        quote(&workspace)
    );
    let Some(network) = ask(
        stdin,
        stdout,
        "Allow network access?",
        settings.network == SandboxNetworkMode::Allow,
    ) else {
        return 0;
    };
    let Some(github) = ask(stdin, stdout, "Add GitHub CLI access?", false) else {
        return 0;
    };

    let network_mode = if network { "allow" } else { "deny" };
    let mut proposed = file.sandbox.clone();
    proposed.driver = Some("auto".to_string());
    proposed.network = Some(network_mode.to_string());
    let mut gh_dir = String::new();
    if github {
        let configured = lookup.get("GH_CONFIG_DIR").cloned().unwrap_or_default();
        let candidate: PathBuf = match configured.is_empty() {
            true => [&home, ".config", "gh"].iter().collect(),
            false => PathBuf::from(&configured),
        };
        if !candidate.is_absolute() {
            return fail(
                stderr,
                "GH_CONFIG_DIR must be an absolute directory; no changes made",
            );
        }
        let Ok(resolved) = canonical_directory(&candidate) else {
            return fail(
                stderr,
                "GitHub configuration directory is missing; run gh auth login outside Otto, then retry (set GH_CONFIG_DIR for a custom location)",
            );
        };
        gh_dir = resolved.to_string_lossy().into_owned();
        if !proposed.read_paths.contains(&gh_dir) {
            proposed.read_paths.push(gh_dir.clone());
        }
        if !proposed
            .allow_env
            .iter()
            .any(|name| name == "GH_CONFIG_DIR")
        {
            proposed.allow_env.push("GH_CONFIG_DIR".to_string());
        }
        let _ = writeln!(
            stdout,
            "GitHub CLI access may expose saved GitHub credentials to shell commands. Network access allows those commands to send data externally."
        );
    }
    let updated = match update_sandbox(&original, &proposed) {
        Ok(updated) => updated,
        Err(error) => return fail(stderr, &error.to_string()),
    };
    let _ = write!(
        stdout,
        "\nConfiguration: {} (applies to future Otto processes using this file)\nDriver: auto (Seatbelt)\nNetwork: {network_mode}\nRead paths: {}\nAllowed environment names: {}\n",
        quote(&path.to_string_lossy()),
        quote_list(&proposed.read_paths),
        quote_list(&proposed.allow_env)
    );
    let launch = |stdout: &mut (dyn Write + Send)| {
        if github {
            let _ = writeln!(
                stdout,
                "Start Otto with: GH_CONFIG_DIR={} otto --config {} --cwd {}",
                shell_quote(&gh_dir),
                shell_quote(&path.to_string_lossy()),
                shell_quote(&workspace)
            );
        }
    };
    launch(stdout);

    loop {
        let _ = write!(stdout, "[check / save / cancel] (cancel): ");
        let Some(answer) = read_line(stdin) else {
            return 0;
        };
        match answer.as_str() {
            "" | "cancel" => {
                let _ = writeln!(stdout, "Cancelled; configuration unchanged.");
                return 0;
            }
            "check" => {
                let mut check_entries = host_entries.to_vec();
                if github {
                    check_entries.retain(|entry| !entry.starts_with(b"GH_CONFIG_DIR="));
                    check_entries.push(format!("GH_CONFIG_DIR={gh_dir}").into_bytes());
                }
                // This error is ignored: the same table already resolved above,
                // so only a caller-visible change could fail here.
                let Ok(resolved) = resolve_sandbox(&proposed, None) else {
                    continue;
                };
                let options = OpenOptions {
                    settings: settings_from_config(&resolved),
                    workspace: workspace.clone(),
                    shell: "/bin/bash".to_string(),
                    home: home.clone(),
                    host_entries: check_entries,
                    provider_names: sandbox_provider_environment_names(&file, ""),
                };
                check(
                    |options, cancel| async move { open_sandbox_runtime(&options, &cancel).await },
                    options,
                    github,
                    stdout,
                    cancel,
                )
                .await;
            }
            "save" => {
                if let Err(message) = save(&path, &original, &updated) {
                    return fail(stderr, &format!("cannot save configuration: {message}"));
                }
                let _ = writeln!(stdout, "Saved. Restart Otto to apply these permissions.");
                launch(stdout);
                return 0;
            }
            _ => {
                let _ = writeln!(stdout, "Enter check, save, or cancel.");
            }
        }
    }
}

/// Prompts until the answer parses, returning `None` at end of input.
fn ask(
    stdin: &mut dyn BufRead,
    stdout: &mut (dyn Write + Send),
    prompt: &str,
    default_yes: bool,
) -> Option<bool> {
    let suffix = if default_yes { " [Y/n]: " } else { " [y/N]: " };
    loop {
        let _ = write!(stdout, "{prompt}{suffix}");
        match read_line(stdin)?.as_str() {
            "" => return Some(default_yes),
            "y" | "yes" => return Some(true),
            "n" | "no" => return Some(false),
            _ => {
                let _ = writeln!(stdout, "Enter yes or no.");
            }
        }
    }
}

/// One lowercased, trimmed line, or `None` at end of input.
fn read_line(stdin: &mut dyn BufRead) -> Option<String> {
    let mut line = String::new();
    match stdin.read_line(&mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(line.trim().to_lowercase()),
    }
}

/// Opens the proposed sandbox and reports whether a command runs inside it.
///
/// `open` is a parameter because the real opener starts Seatbelt, which no
/// offline test may depend on.
async fn check<F, Fut>(
    open: F,
    options: OpenOptions,
    github: bool,
    stdout: &mut (dyn Write + Send),
    cancel: &CancellationToken,
) where
    F: FnOnce(OpenOptions, CancellationToken) -> Fut,
    Fut: Future<Output = SandboxRuntime>,
{
    // The whole check is bounded by a child token a timer cancels, which the
    // executor honours the same way it honours process cancellation.
    let deadline = cancel.child_token();
    let timer = {
        let deadline = deadline.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = tokio::time::sleep(CHECK_TIMEOUT) => deadline.cancel(),
                _ = deadline.cancelled() => {}
            }
        })
    };

    let workspace = PathBuf::from(&options.workspace);
    let runtime = open(options, deadline.clone()).await;
    report_check(&runtime, &workspace, github, stdout, &deadline).await;
    if runtime.close().is_err() {
        let _ = writeln!(stdout, "Sandbox cleanup failed.");
    }

    deadline.cancel();
    let _ = timer.await;
}

/// The body of [`check`], split out so the close above always runs.
async fn report_check(
    runtime: &SandboxRuntime,
    workspace: &Path,
    github: bool,
    stdout: &mut (dyn Write + Send),
    cancel: &CancellationToken,
) {
    let Some(executor) = runtime
        .executor
        .as_ref()
        .filter(|_| runtime.info.bash_available)
    else {
        let _ = writeln!(
            stdout,
            "Sandbox startup failed ({}). Check read paths and Seatbelt availability; permissions were not widened.",
            runtime.info.reason.as_str()
        );
        return;
    };
    let command = if github { GH_PROBE } else { "true" };
    let (status, result) = executor
        .execute(
            Request {
                argv: vec![
                    "/bin/bash".to_string(),
                    "-c".to_string(),
                    command.to_string(),
                ],
                dir: workspace.to_path_buf(),
                env: runtime.environment.clone().unwrap_or_default(),
            },
            Streams {
                stdout: &mut std::io::sink(),
                stderr: &mut std::io::sink(),
            },
            cancel,
        )
        .await;
    if result.is_err() {
        let _ = writeln!(stdout, "Sandbox check could not execute or timed out.");
        return;
    }
    let message = match status.code {
        0 => {
            "Sandbox check passed. GitHub authentication and network connectivity were not tested."
        }
        20 => "GitHub CLI is not on the sandbox PATH. Install gh or check PATH.",
        21 => "GitHub configuration directory is not readable inside the sandbox.",
        _ => {
            "Command failed inside the sandbox. This does not by itself establish a permission or authentication problem."
        }
    };
    let _ = writeln!(stdout, "{message}");
}

/// Replaces the configuration file, refusing anything that would lose a
/// concurrent edit or follow a symlink.
fn save(path: &Path, original: &[u8], updated: &[u8]) -> Result<(), &'static str> {
    let current = match std::fs::read(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(_) => return Err("cannot read current file"),
    };
    if current != original {
        return Err("configuration changed during setup; rerun setup");
    }
    if std::fs::symlink_metadata(path).is_ok_and(|info| !info.file_type().is_file()) {
        return Err("configuration must be a regular file, not a symlink");
    }
    let directory = path.parent().unwrap_or(Path::new("."));
    if std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)
        .is_err()
    {
        return Err("cannot create configuration directory");
    }
    let suffix =
        super::runtime_builder::random_id().map_err(|_| "cannot create temporary configuration")?;
    let temp = directory.join(format!(".otto-config-{suffix}"));
    let write = std::fs::OpenOptions::new()
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
        .and_then(|()| std::fs::rename(&temp, path).map_err(|_| "cannot replace configuration"));
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn quote(value: &str) -> String {
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

/// Quoted values, space separated inside brackets.
fn quote_list(values: &[String]) -> String {
    let quoted: Vec<String> = values.iter().map(|value| quote(value)).collect();
    format!("[{}]", quoted.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::{DriverMode, NetworkMode, Settings};
    use std::io::Cursor;
    use tempfile::TempDir;

    const PRESERVED: &str = "# preserved\n[profiles.demo]\nmodel = 'example'\n";

    /// Drives the setup command through the process entry point so the dispatch
    /// in [`super::super::run::run`] is covered too.
    #[tokio::test]
    async fn setup_writes_only_on_save() {
        for (name, input, saved) in [
            ("save", "n\ny\nsave\n", true),
            ("cancel", "y\ny\ncancel\n", false),
            ("eof", "y\ny\n", false),
        ] {
            let home = TempDir::new().expect("home");
            let gh = home.path().join(".config/gh");
            std::fs::create_dir_all(&gh).expect("gh dir");
            let path = home.path().join("config.toml");
            std::fs::write(&path, PRESERVED).expect("write config");

            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            let code = super::super::run::run(
                &[
                    "sandbox".to_string(),
                    "setup".to_string(),
                    "--config".to_string(),
                    path.to_string_lossy().into_owned(),
                ],
                Box::new(Cursor::new(input.as_bytes().to_vec())),
                &mut stdout,
                &mut stderr,
                vec![format!("HOME={}", home.path().display()).into_bytes()],
                false,
                &CancellationToken::new(),
            )
            .await;
            assert_eq!(code, 0, "{name}: {}", String::from_utf8_lossy(&stderr));

            let data = std::fs::read_to_string(&path).expect("read back");
            if !saved {
                assert_eq!(data, PRESERVED, "{name}: changed on cancel");
                continue;
            }
            let file = crate::config::load_required(&path).expect("load");
            assert_eq!(file.sandbox.network.as_deref(), Some("deny"), "{name}");
            assert_eq!(
                file.sandbox.read_paths,
                vec![
                    std::fs::canonicalize(&gh)
                        .expect("canonical gh")
                        .to_string_lossy()
                        .into_owned()
                ],
                "{name}"
            );
            let out = String::from_utf8_lossy(&stdout).into_owned();
            assert!(out.contains("GH_CONFIG_DIR="), "{name}: {out}");
            assert!(data.contains(PRESERVED), "{name}: {data}");
        }
    }

    #[tokio::test]
    async fn setup_rejects_a_bad_command_line() {
        for args in [vec![], vec!["install"], vec!["setup", "--bogus", "x"]] {
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            let code = run(
                &args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>(),
                &mut Cursor::new(Vec::new()),
                &mut stdout,
                &mut stderr,
                &[],
                &HashMap::new(),
                &CancellationToken::new(),
            )
            .await;
            assert_eq!(code, 1, "{args:?}");
            assert_eq!(String::from_utf8_lossy(&stderr), format!("otto: {USAGE}\n"));
        }
    }

    #[tokio::test]
    async fn setup_prints_the_usage_line_for_help() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run(
            &["setup".to_string(), "-h".to_string()],
            &mut Cursor::new(Vec::new()),
            &mut stdout,
            &mut stderr,
            &[],
            &HashMap::new(),
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(code, 0);
        assert_eq!(String::from_utf8_lossy(&stdout), format!("{USAGE}\n"));
        assert!(stderr.is_empty());
    }

    /// Writes a `gh` shim whose `--version` exit code is `code`, and returns
    /// the directory to put on the sandbox `PATH`.
    fn gh_shim(home: &TempDir, code: i32) -> String {
        let bin = home.path().join("bin");
        std::fs::create_dir_all(&bin).expect("bin");
        let script = bin.join("gh");
        std::fs::write(&script, format!("#!/bin/sh\nexit {code}\n")).expect("shim");
        let mut mode = std::fs::metadata(&script).expect("stat").permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut mode, 0o755);
        std::fs::set_permissions(&script, mode).expect("chmod");
        bin.to_string_lossy().into_owned()
    }

    /// Open options for an unconfined runtime: the `off` driver runs the
    /// command directly, so these cases exercise the real probe script
    /// offline instead of a stubbed exit code.
    fn check_options(home: &TempDir, path: &str, config_dir: &str) -> OpenOptions {
        let workspace = std::fs::canonicalize(home.path())
            .expect("canonical home")
            .to_string_lossy()
            .into_owned();
        OpenOptions {
            settings: Settings {
                driver: DriverMode::Off,
                network: Some(NetworkMode::Allow),
                read_paths: Vec::new(),
                allow_env: vec!["GH_CONFIG_DIR".to_string()],
            },
            workspace: workspace.clone(),
            shell: "/bin/bash".to_string(),
            home: workspace,
            host_entries: vec![
                format!("PATH={path}").into_bytes(),
                format!("GH_CONFIG_DIR={config_dir}").into_bytes(),
            ],
            provider_names: vec!["OTTO_API_KEY".to_string()],
        }
    }

    async fn run_check(options: OpenOptions, github: bool) -> String {
        let mut stdout = Vec::new();
        check(
            |options, cancel| async move { open_sandbox_runtime(&options, &cancel).await },
            options,
            github,
            &mut stdout,
            &CancellationToken::new(),
        )
        .await;
        String::from_utf8_lossy(&stdout).into_owned()
    }

    /// The unconfined driver runs the real probe against a `gh` shim, which
    /// pins the script and the exit codes together.
    #[tokio::test]
    async fn check_reports_a_startup_failure() {
        let home = TempDir::new().expect("home");
        let mut options = check_options(&home, "/usr/bin:/bin", home.path().to_str().unwrap());
        options.workspace = home.path().join("missing").to_string_lossy().into_owned();
        assert!(
            run_check(options, true)
                .await
                .contains("Sandbox startup failed"),
            "expected a startup failure"
        );
    }

    #[tokio::test]
    async fn check_reports_success() {
        let home = TempDir::new().expect("home");
        let bin = gh_shim(&home, 0);
        let config_dir = std::fs::canonicalize(home.path()).expect("canonical");
        let options = check_options(
            &home,
            &format!("{bin}:/usr/bin:/bin"),
            &config_dir.to_string_lossy(),
        );
        assert!(
            run_check(options, true)
                .await
                .contains("authentication and network connectivity were not tested")
        );
    }

    #[tokio::test]
    async fn check_reports_a_missing_github_cli() {
        let home = TempDir::new().expect("home");
        let config_dir = std::fs::canonicalize(home.path()).expect("canonical");
        // An empty PATH cannot resolve `gh`, so the probe stops at exit 20.
        let options = check_options(&home, "", &config_dir.to_string_lossy());
        assert!(
            run_check(options, true)
                .await
                .contains("not on the sandbox PATH"),
            "expected the missing-gh message"
        );
    }

    #[tokio::test]
    async fn check_reports_an_unreadable_configuration_directory() {
        let home = TempDir::new().expect("home");
        let bin = gh_shim(&home, 0);
        let missing = home.path().join("missing");
        let options = check_options(
            &home,
            &format!("{bin}:/usr/bin:/bin"),
            &missing.to_string_lossy(),
        );
        assert!(
            run_check(options, true)
                .await
                .contains("not readable inside the sandbox")
        );
    }

    #[tokio::test]
    async fn check_reports_any_other_failure() {
        let home = TempDir::new().expect("home");
        let bin = gh_shim(&home, 1);
        let config_dir = std::fs::canonicalize(home.path()).expect("canonical");
        let options = check_options(
            &home,
            &format!("{bin}:/usr/bin:/bin"),
            &config_dir.to_string_lossy(),
        );
        assert!(
            run_check(options, true)
                .await
                .contains("does not by itself establish")
        );
    }

    #[test]
    fn save_refuses_a_concurrent_edit_and_a_symlink() {
        let directory = TempDir::new().expect("dir");
        let path = directory.path().join("config.toml");
        std::fs::write(&path, b"changed").expect("write");
        assert_eq!(
            save(&path, b"old", b"new"),
            Err("configuration changed during setup; rerun setup")
        );
        let link = directory.path().join("link");
        std::os::unix::fs::symlink(&path, &link).expect("symlink");
        assert_eq!(
            save(&link, b"changed", b"new"),
            Err("configuration must be a regular file, not a symlink")
        );
        assert_eq!(std::fs::read(&path).expect("read"), b"changed");
    }

    #[test]
    fn quoting_helpers_render_paths_lists_and_shell_words() {
        assert_eq!(quote("/tmp/a b"), "\"/tmp/a b\"");
        assert_eq!(quote("say \"hi\"\n"), "\"say \\\"hi\\\"\\n\"");
        assert_eq!(quote_list(&[]), "[]");
        assert_eq!(
            quote_list(&["/a".to_string(), "/b".to_string()]),
            "[\"/a\" \"/b\"]"
        );
        assert_eq!(shell_quote("it's"), "'it'\"'\"'s'");
    }
}
