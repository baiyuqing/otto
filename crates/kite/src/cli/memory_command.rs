//! The standalone `kite memory status|forget <id>` CLI.
//!
//! It is dispatched before the main flag set is parsed, because its argument
//! grammar is its own, and it builds only the memory service: no provider,
//! session, or controller.
//!
//! It folds the credentials captured by `kite login` into the secret set before
//! deciding whether redaction is complete, so a token never reaches the status
//! output.

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;

use kite_core::config::memory::MemoryRuntime;
use kite_core::config::resolve_memory;

use super::boundary::{self, BoundaryInputs};
use super::run::fail;
use crate::memory::{ForgetRequest, RecordRef, Scope, Service};

const STORE_UNAVAILABLE_WARNING: &str =
    "warning: memory store unavailable, continuing without memory";
const CONFIG_UNAVAILABLE: &str = "memory configuration is invalid or unavailable";
const BACKEND_UNAVAILABLE: &str = "memory backend is unavailable";
const COMMAND_UNAVAILABLE: &str = "memory command is unavailable";
const WORKING_DIRECTORY_INVALID: &str = "working directory is invalid or unavailable";
const WORKSPACE_UNAVAILABLE: &str = "memory workspace is invalid or unavailable";

/// What the subcommand's own flags resolved to.
struct Flags {
    config_path: String,
    explicit_config: bool,
    cwd: String,
}

/// Parses `--config PATH` and `--cwd PATH` in either `--name value` or
/// `--name=value` form.
fn parse_flags(args: &[String]) -> Result<Flags, ()> {
    let mut flags = Flags {
        config_path: String::new(),
        explicit_config: false,
        cwd: ".".into(),
    };
    let mut index = 0;
    while index < args.len() {
        let argument = args[index].as_str();
        let (name, inline) = match argument.split_once('=') {
            Some((name, value)) => (name, Some(value.to_string())),
            None => (argument, None),
        };
        let name = name.trim_start_matches('-');
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
            flags.explicit_config = !value.is_empty();
            flags.config_path = value;
        } else {
            flags.cwd = value;
        }
        index += 1;
    }
    Ok(flags)
}

/// Runs `kite memory ...` and returns its exit code.
pub fn run(
    args: &[String],
    stdout: &mut (dyn Write + Send),
    stderr: &mut (dyn Write + Send),
    lookup: &HashMap<String, String>,
) -> i32 {
    let Some(subcommand) = args.first().cloned() else {
        return fail(
            stderr,
            "usage: kite memory status|forget <id> [--config PATH] [--cwd PATH]",
        );
    };
    let mut rest = &args[1..];

    let mut record_id = String::new();
    if subcommand == "forget" {
        match rest.first() {
            Some(first) if !first.starts_with('-') => {
                record_id = first.clone();
                rest = &rest[1..];
            }
            _ => {
                return fail(
                    stderr,
                    "usage: kite memory forget <id> [--config PATH] [--cwd PATH]",
                );
            }
        }
    }
    let Ok(flags) = parse_flags(rest) else {
        return 2;
    };

    let home = match super::run::resolve_home_for(lookup) {
        Ok(home) => home,
        Err(message) => return fail(stderr, &message),
    };
    let Ok((_, config_file)) =
        super::run::load_config_for(&flags.config_path, flags.explicit_config, &home)
    else {
        return fail(
            stderr,
            "load config: configuration is invalid or unavailable",
        );
    };
    // The captured credentials ride in on `sandbox_secrets`, which the boundary
    // collects first.
    let captured_auth =
        super::login::capture_auth_credentials(&crate::auth::path_for_home(Path::new(&home)));
    let mut environment = super::run::config_environment_for(&config_file, lookup);
    environment.insert("HOME".to_string(), home);
    let Ok(memory_config) = resolve_memory(&config_file, &environment) else {
        return fail(stderr, CONFIG_UNAVAILABLE);
    };

    let (secret_values, complete) = boundary::boundary_secret_values(
        &BoundaryInputs {
            sandbox_secrets: &captured_auth.redaction_values,
            sandbox_secrets_complete: captured_auth.complete,
            config: &config_file,
            environment: &environment,
            overrides_base_url: "",
        },
        None,
    );
    if !complete {
        return fail(stderr, COMMAND_UNAVAILABLE);
    }

    match subcommand.as_str() {
        "status" => status(&memory_config, &secret_values, stdout, stderr),
        "forget" => {
            let Ok(workspace) = super::sandbox_runtime::canonical_directory(Path::new(&flags.cwd))
            else {
                return fail(stderr, &format!("resolve cwd: {WORKING_DIRECTORY_INVALID}"));
            };
            forget(
                &memory_config,
                &secret_values,
                &workspace.to_string_lossy(),
                &record_id,
                stdout,
                stderr,
            )
        }
        other => fail(stderr, &format!("unknown memory subcommand {other:?}")),
    }
}

fn status(
    config: &MemoryRuntime,
    secret_values: &[String],
    stdout: &mut (dyn Write + Send),
    stderr: &mut (dyn Write + Send),
) -> i32 {
    let mut warning = Vec::new();
    let Ok((service, _, usable)) =
        super::wiring::open_memory_service(config, secret_values, &mut warning)
    else {
        return fail(stderr, BACKEND_UNAVAILABLE);
    };
    let redactor = kite_core::agent::redactor::Redactor::with_completeness(secret_values, true);
    let _ = writeln!(stdout, "enabled: {}", config.enabled);
    let _ = writeln!(stdout, "backend: {}", config.backend);
    let _ = writeln!(
        stdout,
        "path: {}",
        redactor.redact_string(&config.sqlite_path)
    );
    let _ = writeln!(stdout, "usable: {usable}");
    if !String::from_utf8_lossy(&warning).trim().is_empty() {
        let _ = writeln!(stdout, "{STORE_UNAVAILABLE_WARNING}");
    }
    let _ = service.close();
    0
}

fn forget(
    config: &MemoryRuntime,
    secret_values: &[String],
    workspace_path: &str,
    id: &str,
    stdout: &mut (dyn Write + Send),
    stderr: &mut (dyn Write + Send),
) -> i32 {
    let mut warning = Vec::new();
    let Ok((service, user_scope, usable)) =
        super::wiring::open_memory_service(config, secret_values, &mut warning)
    else {
        return fail(stderr, BACKEND_UNAVAILABLE);
    };
    let code = forget_record(
        &service,
        config,
        user_scope,
        workspace_path,
        id,
        usable,
        stdout,
        stderr,
    );
    let _ = service.close();
    code
}

#[allow(clippy::too_many_arguments)]
fn forget_record(
    service: &Service,
    config: &MemoryRuntime,
    user_scope: Scope,
    workspace_path: &str,
    id: &str,
    usable: bool,
    stdout: &mut (dyn Write + Send),
    stderr: &mut (dyn Write + Send),
) -> i32 {
    if !usable {
        return fail(stderr, "memory is not usable");
    }
    let Ok(workspace_scope) = super::wiring::workspace_memory_scope(config, workspace_path) else {
        return fail(stderr, WORKSPACE_UNAVAILABLE);
    };

    let mut last_error = None;
    for scope in [user_scope, workspace_scope] {
        let reference = RecordRef {
            scope,
            id: id.to_string(),
        };
        let record = match service.get(&reference) {
            Ok(record) => record,
            Err(error) => {
                last_error = Some(error);
                continue;
            }
        };
        let result = match service.forget(&ForgetRequest {
            reference,
            expected_revision: record.revision,
            purge_backups: false,
            confirm_purge: false,
        }) {
            Ok(result) => result,
            Err(error) => return fail(stderr, &error.to_string()),
        };
        let _ = writeln!(
            stdout,
            "forgot {} (revision {})",
            result.tombstone.id, record.revision
        );
        return 0;
    }
    let reason = last_error
        .map(|error| error.to_string())
        .unwrap_or_else(|| "not found".to_string());
    fail(stderr, &format!("record {id} not found: {reason}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::RememberRequest;
    use std::collections::HashMap;

    /// Writes a config file enabling memory at `db_path` and returns its path.
    fn memory_config(directory: &Path, db_path: &str) -> String {
        let path = directory.join("kite.toml");
        std::fs::write(
            &path,
            format!("[memory]\nenabled = true\n[memory.sqlite]\npath = \"{db_path}\"\n"),
        )
        .expect("write config");
        path.to_string_lossy().into_owned()
    }

    fn lookup(home: &Path) -> HashMap<String, String> {
        HashMap::from([("HOME".to_string(), home.to_string_lossy().into_owned())])
    }

    /// Runs `kite memory ...` and returns its exit code, stdout and stderr.
    fn memory(args: &[&str], home: &Path) -> (i32, String, String) {
        let arguments: Vec<String> = args.iter().map(|value| value.to_string()).collect();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run(&arguments, &mut stdout, &mut stderr, &lookup(home));
        (
            code,
            String::from_utf8_lossy(&stdout).into_owned(),
            String::from_utf8_lossy(&stderr).into_owned(),
        )
    }

    #[test]
    fn status_reports_configured_memory() {
        let home = tempfile::tempdir().expect("home");
        let workspace = tempfile::tempdir().expect("workspace");
        let store = tempfile::tempdir().expect("store");
        let db_path = store.path().join("memory").join("memory.db");
        let db_path = db_path.to_string_lossy().into_owned();
        let config = memory_config(home.path(), &db_path);

        let (code, stdout, stderr) = memory(
            &[
                "status",
                "--config",
                &config,
                "--cwd",
                &workspace.path().to_string_lossy(),
            ],
            home.path(),
        );
        assert_eq!(code, 0, "stderr = {stderr}");
        for want in ["enabled: true", "backend: sqlite", &db_path, "usable: true"] {
            assert!(stdout.contains(want), "stdout = {stdout:?}, want {want:?}");
        }
    }

    /// A captured token must not survive into the status output.
    #[test]
    fn status_redacts_captured_chatgpt_credentials() {
        let home = tempfile::tempdir().expect("home");
        let workspace = tempfile::tempdir().expect("workspace");
        let store = tempfile::tempdir().expect("store");
        // Not a credential: a fixture value placed where the status output
        // prints it, so an uncollected token is visible in the assertion.
        let token = "captured-chatgpt-token-fixture";
        let auth_path = crate::auth::path_for_home(home.path());
        std::fs::create_dir_all(auth_path.parent().expect("auth directory"))
            .expect("create auth directory");
        std::fs::write(&auth_path, format!(r#"{{"access_token":"{token}"}}"#))
            .expect("write credentials");
        let db_path = store
            .path()
            .join(token)
            .join("memory.db")
            .to_string_lossy()
            .into_owned();
        let config = memory_config(home.path(), &db_path);

        let (code, stdout, stderr) = memory(
            &[
                "status",
                "--config",
                &config,
                "--cwd",
                &workspace.path().to_string_lossy(),
            ],
            home.path(),
        );

        assert_eq!(code, 0, "stderr = {stderr}");
        assert!(
            !stdout.contains(token),
            "stdout leaked the token: {stdout:?}"
        );
        assert!(stdout.contains("usable: true"), "stdout = {stdout:?}");
    }

    #[test]
    fn forget_removes_a_record_from_the_workspace_scope() {
        let home = tempfile::tempdir().expect("home");
        let workspace = tempfile::tempdir().expect("workspace");
        let workspace_path = super::super::sandbox_runtime::canonical_directory(workspace.path())
            .expect("canonical workspace");
        let store = tempfile::tempdir().expect("store");
        let db_path = store
            .path()
            .join("memory")
            .join("memory.db")
            .to_string_lossy()
            .into_owned();
        let config = memory_config(home.path(), &db_path);

        let runtime = MemoryRuntime {
            enabled: true,
            backend: "sqlite".into(),
            sqlite_path: db_path,
            ..MemoryRuntime::default()
        };
        let (service, _, usable) =
            super::super::wiring::open_memory_service(&runtime, &[], &mut Vec::new())
                .expect("open memory service");
        assert!(usable, "the store must be usable");
        let scope = super::super::wiring::workspace_memory_scope(
            &runtime,
            &workspace_path.to_string_lossy(),
        )
        .expect("workspace scope");
        let record = service
            .remember(&RememberRequest {
                scope: scope.clone(),
                kind: "preference".into(),
                key: "editor".into(),
                text: "vim".into(),
                ..RememberRequest::default()
            })
            .expect("remember");
        service.close().expect("close");

        let (code, stdout, stderr) = memory(
            &[
                "forget",
                &record.id,
                "--config",
                &config,
                "--cwd",
                &workspace_path.to_string_lossy(),
            ],
            home.path(),
        );
        assert_eq!(code, 0, "stderr = {stderr}");
        assert!(stdout.contains(&record.id), "stdout = {stdout:?}");

        let (reopened, _, usable) =
            super::super::wiring::open_memory_service(&runtime, &[], &mut Vec::new())
                .expect("reopen memory service");
        assert!(usable, "the reopened store must be usable");
        let reference = RecordRef {
            scope,
            id: record.id,
        };
        assert!(
            reopened.get(&reference).is_err(),
            "the forgotten record must be gone"
        );
        reopened.close().expect("close");
    }

    #[test]
    fn forget_without_an_id_reports_usage() {
        let home = tempfile::tempdir().expect("home");
        let config = memory_config(home.path(), "/unused/memory.db");
        let (code, _, stderr) = memory(&["forget", "--config", &config], home.path());
        assert_ne!(code, 0);
        assert!(stderr.contains("usage"), "stderr = {stderr:?}");
    }

    #[test]
    fn no_subcommand_reports_usage() {
        let home = tempfile::tempdir().expect("home");
        let (code, _, stderr) = memory(&[], home.path());
        assert_eq!(code, 1);
        assert_eq!(
            stderr,
            "kite: usage: kite memory status|forget <id> [--config PATH] [--cwd PATH]\n"
        );
    }

    #[test]
    fn an_unknown_subcommand_fails() {
        let home = tempfile::tempdir().expect("home");
        let (code, _, stderr) = memory(&["bogus"], home.path());
        assert_ne!(code, 0);
        assert!(stderr.contains("bogus"), "stderr = {stderr:?}");
    }

    #[test]
    fn config_and_resolve_failures_use_fixed_diagnostics() {
        let home = tempfile::tempdir().expect("home");
        let workspace = tempfile::tempdir().expect("workspace");
        let cwd = workspace.path().to_string_lossy().into_owned();
        let missing = home
            .path()
            .join("config-secret")
            .join("missing.toml")
            .to_string_lossy()
            .into_owned();
        let invalid = home.path().join("invalid-memory.toml");
        std::fs::write(
            &invalid,
            "[memory]\n[memory.sqlite]\nbusy_timeout = \"busy-timeout-secret\"\n",
        )
        .expect("write config");
        let invalid = invalid.to_string_lossy().into_owned();

        for (config, want, avoid) in [
            (
                &missing,
                "kite: load config: configuration is invalid or unavailable\n",
                "config-secret",
            ),
            (
                &invalid,
                "kite: memory configuration is invalid or unavailable\n",
                "busy-timeout-secret",
            ),
        ] {
            let (code, _, stderr) =
                memory(&["status", "--config", config, "--cwd", &cwd], home.path());
            assert_eq!((code, stderr.as_str()), (1, want));
            assert!(!stderr.contains(avoid), "stderr leaked {avoid:?}");
        }
    }

    #[test]
    fn an_unusable_working_directory_uses_a_fixed_diagnostic() {
        let home = tempfile::tempdir().expect("home");
        let config = memory_config(home.path(), "/unused/memory.db");
        let cwd = home
            .path()
            .join("cwd-secret-does-not-exist")
            .to_string_lossy()
            .into_owned();
        let (code, _, stderr) = memory(
            &["forget", "rec-1", "--config", &config, "--cwd", &cwd],
            home.path(),
        );
        assert_eq!(
            (code, stderr.as_str()),
            (
                1,
                "kite: resolve cwd: working directory is invalid or unavailable\n"
            )
        );
        assert!(!stderr.contains("cwd-secret"), "stderr = {stderr:?}");
    }

    #[test]
    fn a_failing_open_uses_a_fixed_diagnostic() {
        let home = tempfile::tempdir().expect("home");
        let workspace = tempfile::tempdir().expect("workspace");
        let config = home.path().join("encrypted.toml");
        std::fs::write(
            &config,
            "[memory]\nenabled = true\nrequire_encryption = true\n",
        )
        .expect("write config");
        let (code, _, stderr) = memory(
            &[
                "status",
                "--config",
                &config.to_string_lossy(),
                "--cwd",
                &workspace.path().to_string_lossy(),
            ],
            home.path(),
        );
        assert_eq!(
            (code, stderr.as_str()),
            (1, "kite: memory backend is unavailable\n")
        );
    }

    /// The status writer is called directly with a path that cannot be opened
    /// and a secret value that covers the directory holding it.
    #[test]
    fn status_redacts_the_configured_path_and_reports_the_warning() {
        let home = tempfile::tempdir().expect("home");
        let secret = home.path().join("acct-secret");
        std::fs::write(&secret, b"not a directory").expect("write blocker");
        let secret = secret.to_string_lossy().into_owned();
        let runtime = MemoryRuntime {
            enabled: true,
            backend: "sqlite".into(),
            sqlite_path: format!("{secret}/memory.db"),
            ..MemoryRuntime::default()
        };

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = status(
            &runtime,
            std::slice::from_ref(&secret),
            &mut stdout,
            &mut stderr,
        );
        let stdout = String::from_utf8_lossy(&stdout).into_owned();
        assert_eq!(code, 0, "stderr = {:?}", String::from_utf8_lossy(&stderr));
        assert!(
            !stdout.contains(&secret),
            "stdout leaked the path: {stdout:?}"
        );
        for want in [
            "enabled: true",
            "backend: sqlite",
            "path: ",
            "memory.db",
            "usable: false",
            STORE_UNAVAILABLE_WARNING,
        ] {
            assert!(stdout.contains(want), "stdout = {stdout:?}, want {want:?}");
        }
    }
}
