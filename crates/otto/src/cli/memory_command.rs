//! The standalone `otto memory status|list|show <id>|forget <id>` CLI.
//!
//! It is dispatched before the main flag set is parsed, because its argument
//! grammar is its own, and it builds only the memory service: no provider,
//! session, or controller.
//!
//! It folds the credentials captured by `otto login` into the secret set before
//! deciding whether redaction is complete, so a token never reaches the status
//! output.

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;

use otto_core::config::memory::MemoryRuntime;
use otto_core::config::resolve_memory;

use super::run::fail;
use crate::memory::{ForgetRequest, ListRequest, MAX_PAGE_SIZE, RecordRef, Scope, Service};
use crate::runtime::boundary::{self, BoundaryInputs};

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
    scope: String,
    limit: usize,
    cursor: String,
}

/// Parses `--config PATH` and `--cwd PATH` in either `--name value` or
/// `--name=value` form.
fn parse_flags(args: &[String]) -> Result<Flags, ()> {
    let mut flags = Flags {
        config_path: String::new(),
        explicit_config: false,
        cwd: ".".into(),
        scope: "current".into(),
        limit: 20,
        cursor: String::new(),
    };
    let mut index = 0;
    while index < args.len() {
        let argument = args[index].as_str();
        let (name, inline) = match argument.split_once('=') {
            Some((name, value)) => (name, Some(value.to_string())),
            None => (argument, None),
        };
        let name = name.trim_start_matches('-');
        if !matches!(name, "config" | "cwd" | "scope" | "limit" | "cursor") {
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
        } else if name == "cwd" {
            flags.cwd = value;
        } else if name == "scope" {
            if !matches!(value.as_str(), "current" | "user" | "workspace" | "all") {
                return Err(());
            }
            flags.scope = value;
        } else if name == "limit" {
            flags.limit = value.parse().map_err(|_| ())?;
            if !(1..=MAX_PAGE_SIZE).contains(&flags.limit) {
                return Err(());
            }
        } else {
            flags.cursor = value;
        }
        index += 1;
    }
    Ok(flags)
}

/// Runs `otto memory ...` and returns its exit code.
pub(crate) fn startup_configuration(
    args: &[String],
    home: &str,
) -> Result<otto_core::config::File, ()> {
    let (command, mut rest) = args.split_first().ok_or(())?;
    if matches!(command.as_str(), "forget" | "show") {
        let (_, flags) = rest.split_first().ok_or(())?;
        rest = flags;
    }
    let flags = parse_flags(rest)?;
    super::run::load_config_for(&flags.config_path, flags.explicit_config, home)
        .map(|(_, file)| file)
        .map_err(|_| ())
}

/// Whether the memory directory that `config` resolves to already holds a
/// store. Then `otto memory` needs no storage route authority: the routes only
/// locate a database to import.
pub(crate) fn has_store(
    config: &otto_core::config::File,
    lookup: &HashMap<String, String>,
    home: &str,
) -> bool {
    let mut environment = super::run::config_environment_for(config, lookup);
    environment.insert("HOME".to_string(), home.to_string());
    resolve_memory(config, &environment)
        .is_ok_and(|memory| crate::memory::files::has_identity(Path::new(&memory.dir)))
}

pub fn run(
    args: &[String],
    stdout: &mut (dyn Write + Send),
    stderr: &mut (dyn Write + Send),
    lookup: &HashMap<String, String>,
) -> i32 {
    run_with_storage(
        args,
        stdout,
        stderr,
        lookup,
        &crate::runtime::storage::Storage::default(),
    )
}

/// Runs the memory command using the process-wide storage routes.
pub fn run_with_storage(
    args: &[String],
    stdout: &mut (dyn Write + Send),
    stderr: &mut (dyn Write + Send),
    lookup: &HashMap<String, String>,
    storage: &crate::runtime::storage::Storage,
) -> i32 {
    let Some(subcommand) = args.first().cloned() else {
        return fail(
            stderr,
            "usage: otto memory status|list|show <id>|forget <id> [--scope current|user|workspace|all] [--limit N] [--cursor CURSOR] [--config PATH] [--cwd PATH]",
        );
    };
    let mut rest = &args[1..];

    let mut record_id = String::new();
    if matches!(subcommand.as_str(), "forget" | "show") {
        match rest.first() {
            Some(first) if !first.starts_with('-') => {
                record_id = first.clone();
                rest = &rest[1..];
            }
            _ => {
                return fail(
                    stderr,
                    &format!(
                        "usage: otto memory {subcommand} <id> [--scope current|user|workspace] [--config PATH] [--cwd PATH]"
                    ),
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
    if let Some(package) = memory_config.plugin_package() {
        return fail(
            stderr,
            &format!(
                "memory source plugin:{package} does not support otto memory; the plugin manages its records"
            ),
        );
    }
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
        "status" => status(
            storage,
            &memory_config,
            &secret_values,
            &flags.cwd,
            stdout,
            stderr,
        ),
        "list" => list(ListInput {
            storage,
            config: &memory_config,
            secret_values: &secret_values,
            cwd: &flags.cwd,
            scope: &flags.scope,
            limit: flags.limit,
            cursor: &flags.cursor,
            stdout,
            stderr,
        }),
        "show" => show(
            storage,
            &memory_config,
            &secret_values,
            &flags.cwd,
            &flags.scope,
            &record_id,
            stdout,
            stderr,
        ),
        "forget" => {
            let Ok(workspace) =
                crate::runtime::sandbox_runtime::canonical_directory(Path::new(&flags.cwd))
            else {
                return fail(stderr, &format!("resolve cwd: {WORKING_DIRECTORY_INVALID}"));
            };
            forget(
                storage,
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
    storage: &crate::runtime::storage::Storage,
    config: &MemoryRuntime,
    secret_values: &[String],
    cwd: &str,
    stdout: &mut (dyn Write + Send),
    stderr: &mut (dyn Write + Send),
) -> i32 {
    let mut warning = Vec::new();
    let Ok((service, user_scope, usable)) =
        crate::runtime::wiring::open_memory_service_with_storage(
            storage,
            config,
            secret_values,
            &mut warning,
        )
    else {
        return fail(stderr, BACKEND_UNAVAILABLE);
    };
    let redactor = otto_core::agent::redactor::Redactor::with_completeness(secret_values, true);
    let _ = writeln!(stdout, "enabled: {}", config.enabled);
    let _ = writeln!(stdout, "backend: {}", config.backend);
    let _ = writeln!(stdout, "path: {}", redactor.redact_string(&config.dir));
    let _ = writeln!(stdout, "usable: {usable}");
    if usable {
        // The scopes `/memory review` lists: the user and the current workspace.
        let standing = service.standing_usage(&user_scope);
        let pending = crate::runtime::sandbox_runtime::canonical_directory(Path::new(cwd))
            .ok()
            .and_then(|workspace| {
                crate::runtime::wiring::workspace_memory_scope(config, &workspace.to_string_lossy())
                    .ok()
            })
            .and_then(|workspace_scope| {
                service.pending_summary(&[user_scope, workspace_scope]).ok()
            });
        match pending {
            Some(summary) => {
                let _ = writeln!(stdout, "pending: {}", summary.count);
            }
            None => {
                let _ = writeln!(stdout, "pending: unavailable");
            }
        }
        match standing {
            Ok(usage) => {
                let _ = write!(
                    stdout,
                    "standing: {} records, {}/{} tokens",
                    usage.records, usage.tokens, usage.budget
                );
                if usage.omitted > 0 {
                    let _ = write!(stdout, ", {} not injected (over budget)", usage.omitted);
                }
                let _ = writeln!(stdout);
            }
            Err(_) => {
                let _ = writeln!(stdout, "standing: unavailable");
            }
        }
    }
    let warning = String::from_utf8_lossy(&warning);
    for skipped in warning
        .lines()
        .filter_map(|line| line.strip_prefix(crate::runtime::wiring::SKIPPED_FILE_PREFIX))
    {
        let _ = writeln!(stdout, "skipped: {}", redactor.redact_string(skipped));
    }
    if !usable {
        let _ = writeln!(stdout, "{STORE_UNAVAILABLE_WARNING}");
    }
    let _ = service.close();
    0
}

struct ListInput<'a> {
    storage: &'a crate::runtime::storage::Storage,
    config: &'a MemoryRuntime,
    secret_values: &'a [String],
    cwd: &'a str,
    scope: &'a str,
    limit: usize,
    cursor: &'a str,
    stdout: &'a mut (dyn Write + Send),
    stderr: &'a mut (dyn Write + Send),
}

fn list(input: ListInput<'_>) -> i32 {
    let ListInput {
        storage,
        config,
        secret_values,
        cwd,
        scope,
        limit,
        cursor,
        stdout,
        stderr,
    } = input;
    let Ok(workspace) = crate::runtime::sandbox_runtime::canonical_directory(Path::new(cwd)) else {
        return fail(stderr, &format!("resolve cwd: {WORKING_DIRECTORY_INVALID}"));
    };
    let mut warning = Vec::new();
    let Ok((service, user_scope, usable)) =
        crate::runtime::wiring::open_memory_service_with_storage(
            storage,
            config,
            secret_values,
            &mut warning,
        )
    else {
        return fail(stderr, BACKEND_UNAVAILABLE);
    };
    if !usable {
        let _ = service.close();
        return fail(stderr, "memory is not usable");
    }
    let workspace_scope = match crate::runtime::wiring::workspace_memory_scope(
        config,
        &workspace.to_string_lossy(),
    ) {
        Ok(scope) => scope,
        Err(_) => {
            let _ = service.close();
            return fail(stderr, WORKSPACE_UNAVAILABLE);
        }
    };
    let (all_scopes, scopes) = match scope {
        "current" => (false, vec![user_scope, workspace_scope]),
        "user" => (false, vec![user_scope]),
        "workspace" => (false, vec![workspace_scope]),
        "all" => (true, Vec::new()),
        _ => return fail(stderr, "invalid memory scope"),
    };
    let result = service.list(&ListRequest {
        all_scopes,
        scopes,
        kinds: Vec::new(),
        labels: Vec::new(),
        limit,
        cursor: cursor.to_string(),
        now: chrono::Utc::now(),
        include_expired: true,
    });
    let _ = service.close();
    match result {
        Ok(page) => {
            for record in page.records {
                let _ = writeln!(
                    stdout,
                    "id={} scope={}/{} kind={} key={} revision={} created_at={} updated_at={} text={}",
                    record.id,
                    record.scope.namespace,
                    record.scope.id,
                    record.kind,
                    record.key,
                    record.revision,
                    record.created_at,
                    record.updated_at,
                    record.text
                );
            }
            if !page.next_cursor.is_empty() {
                let _ = writeln!(stdout, "next_cursor={}", page.next_cursor);
            }
            0
        }
        Err(error) => fail(stderr, &error.to_string()),
    }
}

#[allow(clippy::too_many_arguments)]
fn show(
    storage: &crate::runtime::storage::Storage,
    config: &MemoryRuntime,
    secret_values: &[String],
    cwd: &str,
    scope: &str,
    id: &str,
    stdout: &mut (dyn Write + Send),
    stderr: &mut (dyn Write + Send),
) -> i32 {
    if scope == "all" {
        return fail(stderr, "show requires --scope current, user, or workspace");
    }
    let Ok(workspace) = crate::runtime::sandbox_runtime::canonical_directory(Path::new(cwd)) else {
        return fail(stderr, &format!("resolve cwd: {WORKING_DIRECTORY_INVALID}"));
    };
    let mut warning = Vec::new();
    let Ok((service, user_scope, usable)) =
        crate::runtime::wiring::open_memory_service_with_storage(
            storage,
            config,
            secret_values,
            &mut warning,
        )
    else {
        return fail(stderr, BACKEND_UNAVAILABLE);
    };
    if !usable {
        let _ = service.close();
        return fail(stderr, "memory is not usable");
    }
    let workspace_scope = match crate::runtime::wiring::workspace_memory_scope(
        config,
        &workspace.to_string_lossy(),
    ) {
        Ok(scope) => scope,
        Err(_) => return fail(stderr, WORKSPACE_UNAVAILABLE),
    };
    let scopes = match scope {
        "current" => vec![user_scope, workspace_scope],
        "user" => vec![user_scope],
        "workspace" => vec![workspace_scope],
        _ => unreachable!("scope was parsed"),
    };
    let record = scopes.into_iter().find_map(|scope| {
        service
            .get(&RecordRef {
                scope,
                id: id.to_string(),
            })
            .ok()
    });
    let _ = service.close();
    match record {
        Some(record) => {
            let _ = writeln!(
                stdout,
                "id={} scope={}/{} kind={} key={} revision={} created_at={} updated_at={} text={}",
                record.id,
                record.scope.namespace,
                record.scope.id,
                record.kind,
                record.key,
                record.revision,
                record.created_at,
                record.updated_at,
                record.text
            );
            0
        }
        None => fail(stderr, &format!("record {id} not found")),
    }
}

fn forget(
    storage: &crate::runtime::storage::Storage,
    config: &MemoryRuntime,
    secret_values: &[String],
    workspace_path: &str,
    id: &str,
    stdout: &mut (dyn Write + Send),
    stderr: &mut (dyn Write + Send),
) -> i32 {
    let mut warning = Vec::new();
    let Ok((service, user_scope, usable)) =
        crate::runtime::wiring::open_memory_service_with_storage(
            storage,
            config,
            secret_values,
            &mut warning,
        )
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
    let Ok(workspace_scope) =
        crate::runtime::wiring::workspace_memory_scope(config, workspace_path)
    else {
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
mod tests;
