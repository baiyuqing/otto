//! `otto memory history|diff|restore`: the revision history of one record.
//!
//! The output text comes from `otto::memory::history`, which `/memory` shares.

use std::io::Write;
use std::path::Path;

use otto_core::config::memory::MemoryRuntime;

use super::super::run::fail;
use super::{BACKEND_UNAVAILABLE, WORKING_DIRECTORY_INVALID, WORKSPACE_UNAVAILABLE};
use otto::memory::history::{render_comparison, render_history};
use otto::memory::{Record, RecordRef, RestoreRequest, Service};

/// The history subcommand a record id and revision numbers were given to.
pub(super) enum HistoryCommand {
    History,
    Diff { from: u64, to: Option<u64> },
    Restore { revision: u64 },
}

/// What a history subcommand needs to find its record.
pub(super) struct HistoryInput<'a> {
    pub storage: &'a otto::runtime::storage::Storage,
    pub config: &'a MemoryRuntime,
    pub secret_values: &'a [String],
    pub cwd: &'a str,
    pub scope: &'a str,
    pub id: &'a str,
    pub command: HistoryCommand,
}

/// Parses a revision number argument; a revision starts at 1.
pub(super) fn parse_revision(argument: &str) -> Option<u64> {
    argument
        .parse::<u64>()
        .ok()
        .filter(|revision| *revision >= 1)
}

pub(super) fn run_history(
    input: HistoryInput<'_>,
    stdout: &mut (dyn Write + Send),
    stderr: &mut (dyn Write + Send),
) -> i32 {
    if input.scope == "all" {
        return fail(
            stderr,
            "history, diff and restore require --scope current, user, or workspace",
        );
    }
    let Ok(workspace) = otto::runtime::sandbox_runtime::canonical_directory(Path::new(input.cwd))
    else {
        return fail(stderr, &format!("resolve cwd: {WORKING_DIRECTORY_INVALID}"));
    };
    let mut warning = Vec::new();
    let Ok((service, user_scope, usable)) = otto::runtime::wiring::open_memory_service_with_storage(
        input.storage,
        input.config,
        input.secret_values,
        &mut warning,
    ) else {
        return fail(stderr, BACKEND_UNAVAILABLE);
    };
    let code = if usable {
        match otto::runtime::wiring::workspace_memory_scope(
            input.config,
            &workspace.to_string_lossy(),
        ) {
            Ok(workspace_scope) => {
                let scopes = match input.scope {
                    "user" => vec![user_scope],
                    "workspace" => vec![workspace_scope],
                    _ => vec![user_scope, workspace_scope],
                };
                run_with_service(&service, scopes, &input, stdout, stderr)
            }
            Err(_) => fail(stderr, WORKSPACE_UNAVAILABLE),
        }
    } else {
        fail(stderr, "memory is not usable")
    };
    let _ = service.close();
    code
}

fn run_with_service(
    service: &Service,
    scopes: Vec<otto::memory::Scope>,
    input: &HistoryInput<'_>,
    stdout: &mut (dyn Write + Send),
    stderr: &mut (dyn Write + Send),
) -> i32 {
    let id = input.id;
    let found = scopes.into_iter().find_map(|scope| {
        let reference = RecordRef {
            scope,
            id: id.to_string(),
        };
        service
            .get(&reference)
            .ok()
            .map(|record| (reference, record))
    });
    let Some((reference, current)) = found else {
        return fail(stderr, &format!("record {id} not found"));
    };
    match &input.command {
        HistoryCommand::History => match service.history(&reference) {
            Ok(history) => {
                let _ = writeln!(stdout, "{}", render_history(&history));
                0
            }
            Err(error) => fail(stderr, &error.to_string()),
        },
        HistoryCommand::Diff { from, to } => {
            let history = match service.history(&reference) {
                Ok(history) => history,
                Err(error) => return fail(stderr, &error.to_string()),
            };
            let to = to.unwrap_or(current.revision);
            let find = |revision: u64| -> Option<&Record> {
                history
                    .revisions
                    .iter()
                    .find(|record| record.revision == revision)
            };
            for revision in [*from, to] {
                if find(revision).is_none() {
                    return fail(stderr, &format!("record {id} has no revision {revision}"));
                }
            }
            if let (Some(from_record), Some(to_record)) = (find(*from), find(to)) {
                let _ = writeln!(stdout, "{}", render_comparison(from_record, to_record));
            }
            0
        }
        HistoryCommand::Restore { revision } => {
            match service.restore(&RestoreRequest {
                reference,
                revision: *revision,
                expected_revision: current.revision,
            }) {
                Ok(record) => {
                    let _ = writeln!(
                        stdout,
                        "restored {} as revision {} (from revision {})",
                        record.id, record.revision, revision
                    );
                    0
                }
                Err(error) => fail(
                    stderr,
                    &format!("restore revision {revision} of {id}: {error}"),
                ),
            }
        }
    }
}
