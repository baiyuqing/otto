//! `otto memory state|verify` and the `list` filters: the lifecycle state and
//! time fields of a record (#591).
//!
//! The record line is printed by `render_record_line`, which `list` and `show`
//! share and which prints the same fields as the `/memory` list line.

use std::io::Write;
use std::path::Path;

use chrono::{DateTime, Utc};
use otto_core::config::memory::MemoryRuntime;

use super::super::run::fail;
use super::{BACKEND_UNAVAILABLE, WORKING_DIRECTORY_INVALID, WORKSPACE_UNAVAILABLE};
use otto::app::management::{parse_time_or_age, review_deadline};
use otto::memory::{
    Record, RecordRef, RecordState, Scope, Service, SetStateRequest, VerifyRequest,
};

pub(super) const STATE_USAGE: &str = "usage: otto memory state <id> active|needs-review|retracted [--reason TEXT] | \
     state <id> superseded <replacement-id> [--reason TEXT] [--scope current|user|workspace] [--config PATH] [--cwd PATH]";
pub(super) const VERIFY_USAGE: &str = "usage: otto memory verify <id> [--review-in AGE] [--scope current|user|workspace] [--config PATH] [--cwd PATH]";
const REVIEW_IN_USAGE: &str = "--review-in requires an age such as 30d, 12h or 45m";
const STATE_VALUES: &str = "active, needs-review, superseded, retracted or all";

/// The filters of `otto memory list` beyond scope and paging.
#[derive(Default)]
pub(super) struct ListFilters {
    pub states: Vec<RecordState>,
    pub kinds: Vec<String>,
    pub labels: Vec<String>,
    pub updated_after: Option<DateTime<Utc>>,
    pub verified_after: Option<DateTime<Utc>>,
    pub unverified_only: bool,
    pub review_due_only: bool,
}

/// The lifecycle flags of every subcommand.
#[derive(Default)]
pub(super) struct LifecycleFlags {
    pub filters: ListFilters,
    pub reason: String,
    pub review_in: Option<DateTime<Utc>>,
}

impl LifecycleFlags {
    /// Whether `name` is a flag without a value.
    pub(super) fn is_switch(name: &str) -> bool {
        matches!(name, "unverified" | "review-due")
    }

    /// Whether `name` is a lifecycle flag that takes a value.
    pub(super) fn takes_value(name: &str) -> bool {
        matches!(
            name,
            "state"
                | "kind"
                | "label"
                | "updated-after"
                | "verified-after"
                | "reason"
                | "review-in"
        )
    }

    pub(super) fn set_switch(&mut self, name: &str) {
        match name {
            "unverified" => self.filters.unverified_only = true,
            _ => self.filters.review_due_only = true,
        }
    }

    /// Applies one `--name value` flag; the error is the diagnostic to print.
    pub(super) fn set_value(&mut self, name: &str, value: String) -> Result<(), String> {
        match name {
            "state" => {
                if value == "all" {
                    self.filters.states.extend(RecordState::ALL);
                } else {
                    let state = RecordState::parse(&value)
                        .ok_or_else(|| format!("unknown state {value:?}: use {STATE_VALUES}"))?;
                    self.filters.states.push(state);
                }
            }
            "kind" => self.filters.kinds.push(value),
            "label" => self.filters.labels.push(value),
            "updated-after" => self.filters.updated_after = Some(parse_time_flag(name, &value)?),
            "verified-after" => self.filters.verified_after = Some(parse_time_flag(name, &value)?),
            "reason" => self.reason = value,
            _ => self.review_in = Some(review_deadline(&value).ok_or(REVIEW_IN_USAGE)?),
        }
        self.filters.states.sort();
        self.filters.states.dedup();
        Ok(())
    }
}

fn parse_time_flag(name: &str, value: &str) -> Result<DateTime<Utc>, String> {
    parse_time_or_age(value, Utc::now()).ok_or_else(|| {
        format!(
            "--{name} requires an RFC 3339 time or an age such as 30d, 12h or 45m, got {value:?}"
        )
    })
}

/// The lifecycle subcommand a record id and its arguments were given to.
pub(super) enum LifecycleCommand {
    State {
        state: RecordState,
        superseded_by: String,
    },
    Verify,
}

/// Parses the positional arguments of `state` and `verify`; `Ok(None)` for any
/// other subcommand, `Err` carries the usage message.
pub(super) fn parse_lifecycle_command(
    subcommand: &str,
    positionals: &[String],
) -> Result<Option<LifecycleCommand>, String> {
    match (subcommand, positionals) {
        ("verify", [_]) => Ok(Some(LifecycleCommand::Verify)),
        ("verify", _) => Err(VERIFY_USAGE.into()),
        ("state", [_, state]) => {
            let state = parse_target_state(state)?;
            if state == RecordState::Superseded {
                return Err(format!(
                    "state superseded requires the replacement record id\n{STATE_USAGE}"
                ));
            }
            Ok(Some(LifecycleCommand::State {
                state,
                superseded_by: String::new(),
            }))
        }
        ("state", [_, state, replacement]) => {
            if parse_target_state(state)? != RecordState::Superseded {
                return Err(format!(
                    "only state superseded takes a replacement record id\n{STATE_USAGE}"
                ));
            }
            Ok(Some(LifecycleCommand::State {
                state: RecordState::Superseded,
                superseded_by: replacement.clone(),
            }))
        }
        ("state", _) => Err(STATE_USAGE.into()),
        _ => Ok(None),
    }
}

fn parse_target_state(value: &str) -> Result<RecordState, String> {
    RecordState::parse(value).ok_or_else(|| format!("unknown state {value:?}: use {STATE_VALUES}"))
}

/// One record as a `key=value` line, with the fields of the `/memory` list line.
pub(super) fn render_record_line(record: &Record) -> String {
    let mut line = format!(
        "id={} scope={}/{} kind={} key={} revision={} state={}",
        record.id,
        record.scope.namespace,
        record.scope.id,
        record.kind,
        record.key,
        record.revision,
        record.state.as_str()
    );
    if !record.superseded_by.is_empty() {
        line.push_str(&format!(" superseded_by={}", record.superseded_by));
    }
    if !record.state_reason.is_empty() {
        line.push_str(&format!(" state_reason={}", record.state_reason));
    }
    let optional_time =
        |time: Option<DateTime<Utc>>| time.map(|time| time.to_string()).unwrap_or_default();
    line.push_str(&format!(
        " created_at={} updated_at={} verified_at={} review_at={} text={}",
        record.created_at,
        record.updated_at,
        optional_time(record.verified_at),
        optional_time(record.review_at),
        record.text
    ));
    line
}

/// What a lifecycle subcommand needs to find its record.
pub(super) struct LifecycleInput<'a> {
    pub storage: &'a otto::runtime::storage::Storage,
    pub config: &'a MemoryRuntime,
    pub secret_values: &'a [String],
    pub cwd: &'a str,
    pub scope: &'a str,
    pub id: &'a str,
    pub reason: &'a str,
    pub review_in: Option<DateTime<Utc>>,
    pub command: LifecycleCommand,
}

pub(super) fn run_lifecycle(
    input: LifecycleInput<'_>,
    stdout: &mut (dyn Write + Send),
    stderr: &mut (dyn Write + Send),
) -> i32 {
    if input.scope == "all" {
        return fail(
            stderr,
            "state and verify require --scope current, user, or workspace",
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
    let code = if !usable {
        fail(stderr, "memory is not usable")
    } else {
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
                apply(&service, scopes, &input, stdout, stderr)
            }
            Err(_) => fail(stderr, WORKSPACE_UNAVAILABLE),
        }
    };
    let _ = service.close();
    code
}

fn apply(
    service: &Service,
    scopes: Vec<Scope>,
    input: &LifecycleInput<'_>,
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
        LifecycleCommand::State {
            state,
            superseded_by,
        } => match service.set_state(&SetStateRequest {
            reference,
            expected_revision: current.revision,
            state: *state,
            superseded_by: superseded_by.clone(),
            reason: input.reason.to_string(),
        }) {
            Ok(record) => {
                let mut text = format!(
                    "state of {} is now {} (revision {})",
                    record.id,
                    record.state.as_str(),
                    record.revision
                );
                if !record.superseded_by.is_empty() {
                    text.push_str(&format!(" superseded_by={}", record.superseded_by));
                }
                if !record.state_reason.is_empty() {
                    text.push_str(&format!(" reason={}", record.state_reason));
                }
                let _ = writeln!(stdout, "{text}");
                0
            }
            Err(error) => fail(stderr, &format!("set state of {id}: {error}")),
        },
        LifecycleCommand::Verify => match service.verify(&VerifyRequest {
            reference,
            expected_revision: current.revision,
            review_at: input.review_in,
        }) {
            Ok(record) => {
                let _ = writeln!(
                    stdout,
                    "verified {} (revision {}) verified_at={} review_at={}",
                    record.id,
                    record.revision,
                    record
                        .verified_at
                        .map(|time| time.to_string())
                        .unwrap_or_default(),
                    record
                        .review_at
                        .map(|time| time.to_string())
                        .unwrap_or_else(|| "none".into())
                );
                0
            }
            Err(error) => fail(stderr, &format!("verify {id}: {error}")),
        },
    }
}
