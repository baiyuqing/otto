//! The REPL's memory and sub-agent commands.
//!
//! `/memory review` lists pending candidates (queued by the model's `remember`
//! tool or any other non-human writer) and `/memory review <id> accept|reject`
//! decides one. Like `/memory forget`, it is a human command.
//!
//! [`repl_memory_command`] and [`repl_remember_command`] are free functions,
//! not [`Repl`] methods, so `tui::app`'s `/memory`/`/remember` dispatch can
//! call them directly against its own captured-output buffers; [`Repl`]'s own
//! `memory_command`/`remember_command` are thin wrappers over the same two
//! functions.

use std::fmt::Write as _;
use std::io::Write;
use std::path::Path;
#[cfg(test)]
use std::sync::Arc;
use std::sync::Mutex;

use chrono::Utc;
use otto_core::config::{McpAuth, McpTransport};
use tokio_util::sync::CancellationToken;

use crate::mcp::{Era, ServerState};
#[cfg(test)]
use crate::memory::{RememberRequest, Scope};
use crate::memory::{ReviewDecision, SearchResult};
use crate::schedule::remind::{NO_TIMERS, timer_line};
use crate::skill;
use crate::subagent::format::{first_runes, one_line, task_line, task_steps};
use crate::subagent::record::{self, TaskRow};

use super::controller::Controller;
use super::login::{SharedWriter, browser_opener};
use super::repl::{Error, Repl};

mod mcp_reports;
pub(crate) use mcp_reports::*;

#[cfg(test)]
const DEFAULT_KIND: &str = "note";
/// `pub(crate)` so the TUI's `tui::app` dispatch can assert on the exact usage
/// text it reuses via [`repl_memory_command`]/[`repl_remember_command`] instead
/// of duplicating the literal.
#[cfg(test)]
pub(crate) const MEMORY_USAGE: &str = "usage: /memory list [--scope current|user|workspace|all] [--limit N] \
    [--cursor CURSOR] [--state active|needs-review|superseded|retracted|all] \
    [--kind K] [--label L] [--updated-after TIME|AGE] \
    [--verified-after TIME|AGE] [--unverified] [--review-due] | \
    /memory show <id> | /memory search <query> | \
    /memory forget <id> | /memory history <id> | \
    /memory diff <id> <from> [<to>] | /memory restore <id> <revision> | \
    /memory state <id> active|needs-review|retracted [--reason TEXT] | \
    /memory state <id> superseded <replacement-id> [--reason TEXT] | \
    /memory verify <id> [--review-in AGE] | \
    /memory propose <id> <evidence text> | \
    /memory review <id> accept|reject";
#[cfg(test)]
pub(crate) const REMEMBER_USAGE: &str =
    "usage: /remember [--scope user|workspace] [--kind K] [--key K] [--review-in AGE] <text>";
#[cfg(test)]
pub(crate) const MEMORY_UNAVAILABLE: &str = "memory is not available";
/// The message printed when no runner carries a registry.
const SUBAGENTS_UNAVAILABLE: &str = "sub-agents are not available";
/// The `/task` argument forms, as the README and the user manual document
/// them. [`super::super::tui`] prints the same line for the same input.
pub(crate) const TASK_USAGE: &str = "usage: /task [<id|name> | cancel <id|name>]";
/// The `/timers` argument forms. Both frontends print it for anything else.
/// What `/timers` reports when the runner registers no timer tools, in the
/// shape [`SUBAGENTS_UNAVAILABLE`] uses for `/tasks`.
pub(crate) const SKILL_USAGE: &str =
    "usage: /skill | /skill <name> | /skill set <name> enabled|disabled";
pub(crate) const MCP_USAGE: &str = "usage: /mcp | /mcp login <server>";

/// Content-only rendering shared by the REPL and TUI management presenters.
pub fn format_management_reply(reply: &crate::app::management::Reply) -> String {
    use crate::app::management::Reply;
    match reply {
        Reply::Tasks(rows) => {
            if rows.is_empty() {
                "No tasks.".into()
            } else {
                rows.iter()
                    .map(|t| format!("{} [{}] {}", t.id, t.status.as_str(), t.description))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        }
        Reply::Task(t) => format!(
            "{} [{}] {}\nResult: {}\nError: {}\nTranscript: {}",
            t.id,
            t.status.as_str(),
            t.description,
            t.result,
            t.error,
            t.session_path
        ),
        Reply::TaskCancelled(id) => format!("Cancelled task: {id}"),

        Reply::MemoryList(page) => render_list_result(&page.records, &page.next_cursor),
        Reply::MemoryRecord(record) => render_list_result(std::slice::from_ref(record), ""),
        Reply::MemorySearch(result) => render_search_result(result),
        Reply::MemoryCandidates(rows) => render_pending_candidates(rows),
        Reply::MemoryCandidatePage(page) => render_pending_candidates(&page.candidates),
        Reply::MemoryForgotten {
            tombstone,
            previous_revision,
            cleanup,
        } => {
            let mut text = format!("forgot {} (revision {})", tombstone.id, previous_revision);
            for entry in cleanup {
                text.push_str(&format!(
                    "\n  {}: {} ({})",
                    entry.status.as_str(),
                    entry.copy,
                    entry.detail
                ));
            }
            text
        }
        Reply::MemoryHistory(history) => crate::memory::history::render_history(history),
        Reply::MemoryDiff { from, to } => crate::memory::history::render_comparison(from, to),
        Reply::MemoryRestored(record) => format!(
            "restored {} as revision {} (from revision {})",
            record.id,
            record.revision,
            record
                .metadata
                .get(crate::memory::RESTORED_FROM_REVISION_METADATA)
                .map(String::as_str)
                .unwrap_or("?")
        ),
        Reply::MemoryStateChanged(record) => {
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
            text
        }
        Reply::MemoryProposed(outcome) => {
            use crate::memory::proposals::Outcome;
            match outcome {
                Outcome::NoChange { explanation } => format!("no change proposed: {explanation}"),
                Outcome::Proposed(candidate) => format!(
                    "proposed {} of {} as candidate {} (review with /memory review {} accept|reject): {}",
                    candidate.action.as_str(),
                    candidate.target_id,
                    candidate.id,
                    candidate.id,
                    candidate.reason
                ),
                Outcome::Rejected(reason) => format!("proposal rejected: {reason}"),
                Outcome::Unavailable(reason) => format!("proposal unavailable: {reason}"),
            }
        }
        Reply::MemoryVerified(record) => format!(
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
        ),
        Reply::MemoryReviewed(decision, result) => {
            let id = &result.candidate.id;
            match (decision, &result.record, &result.tombstone) {
                (ReviewDecision::Reject, _, _) => format!("rejected {id}"),
                (_, Some(r), _) => {
                    format!("accepted {id} as record {} (revision {})", r.id, r.revision)
                }
                (_, None, Some(t)) => format!("accepted {id}: forgot {}", t.id),
                _ => format!("accepted {id}"),
            }
        }
        Reply::Remembered(record) => format!(
            "remembered {} (scope={}/{} kind={})",
            record.id, record.scope.namespace, record.scope.id, record.kind
        ),
        Reply::SkillManager(inventory) => {
            let rows = inventory
                .skills
                .iter()
                .map(|row| {
                    format!(
                        "{} [preference: {}] {}",
                        row.skill.name,
                        if row.enabled { "enabled" } else { "disabled" },
                        row.skill.description
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            if inventory.global_enabled {
                rows
            } else {
                format!(
                    "Skills globally disabled. Set [skills].enabled = true in Otto config and restart Otto; restarting alone will not enable skills.\n{rows}"
                )
            }
        }
        Reply::SkillInfo(info) => format!(
            "Skill: {}\nDescription: {}\nLocation: {}\nSource: {}\n{}\n\n{}",
            info.skill.name,
            info.skill.description,
            info.skill.directory.display(),
            info.skill.path.display(),
            info.contract_check.as_deref().unwrap_or(""),
            info.body
        ),
        Reply::Skills(catalog) => {
            if catalog.is_empty() {
                "No skills found.".into()
            } else {
                let mut s = "Available skills:".to_string();
                for item in catalog.skills() {
                    s.push_str(&skill_list_line(item));
                }
                s
            }
        }
        Reply::MemoryServiceResult { text: message, .. }
        | Reply::SkillSet(message)
        | Reply::Notice(message)
        | Reply::Usage(message) => message.clone(),
        Reply::SkillDetail {
            skill: found,
            body,
            contract_check,
        } => {
            let contract = contract_check
                .as_ref()
                .map(|v| format!("\n{v}"))
                .unwrap_or_default();
            format!(
                "Skill: {}\nLocation: {}\nDescription: {}{}\n\n{}",
                found.name,
                found.directory.display(),
                found.description,
                contract,
                body.trim_end()
            )
        }
        Reply::McpStatus(rows) => format_mcp_report(rows),
        Reply::Timers(items) => {
            if items.is_empty() {
                NO_TIMERS.into()
            } else {
                let now = Utc::now();
                items
                    .iter()
                    .map(|i| timer_line(i, now))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        }
        Reply::TimerCancelled(item) => format!("canceled {}: {}", item.id, item.message),
    }
}

/// One recorded task row in the layout `/agents` prints: parent session, task
/// id, agent name, status, created time, then the same description-or-prompt
/// label [`task_label`](super::super::subagent::format::task_label) builds for
/// [`task_line`]. Trailing padding is trimmed.
fn agent_row_line(row: &TaskRow) -> String {
    let name = if row.agent.is_empty() {
        "(default)"
    } else {
        &row.agent
    };
    let label = if row.description.is_empty() {
        first_runes(&one_line(&row.prompt), 60)
    } else {
        row.description.clone()
    };
    let line = format!(
        "{:<26} {:<4} {:<10} {:<11} {:<20}  {label}",
        row.parent_session, row.task_id, name, row.status, row.created_at
    );
    line.trim_end_matches(' ').to_string()
}

/// `/timers` and `/timers cancel <id>` for both frontends: the `Ok` text is
/// the listing or the confirmation, the `Err` text a usage line or a failed
/// cancel. A free function, not a [`Repl`] method, so `tui::app` renders the
/// same text from its own buffers.
pub(crate) fn timers_report(controller: &Controller, args: &str) -> Result<String, String> {
    use crate::app::management::{Reply, Request};
    let request = Request::parse("timers", args)?;
    match controller.management(request)? {
        Reply::Timers(items) => {
            if items.is_empty() {
                return Ok(NO_TIMERS.to_string());
            }
            let now = Utc::now();
            Ok(items
                .iter()
                .map(|item| timer_line(item, now))
                .collect::<Vec<_>>()
                .join("\n"))
        }
        Reply::TimerCancelled(item) => Ok(format!("canceled {}: {}", item.id, item.message)),
        _ => unreachable!("timer request returned a different management reply"),
    }
}

#[cfg(test)]
fn split_first_token(value: &str) -> (&str, &str) {
    let value = value.trim();
    match value.find(char::is_whitespace) {
        Some(index) => (&value[..index], value[index..].trim()),
        None => (value, ""),
    }
}

/// The `--scope`/`--kind`/`--key` flags and the trailing free text.
#[cfg(test)]
fn parse_remember_argument(argument: &str) -> (&str, &str, &str, &str) {
    let (mut scope, mut kind, mut key) = ("", DEFAULT_KIND, "");
    let mut remaining = argument;
    loop {
        let trimmed = remaining.trim();
        if let Some(rest) = trimmed.strip_prefix("--scope ") {
            (scope, remaining) = split_first_token(rest);
        } else if let Some(rest) = trimmed.strip_prefix("--kind ") {
            (kind, remaining) = split_first_token(rest);
        } else if let Some(rest) = trimmed.strip_prefix("--key ") {
            (key, remaining) = split_first_token(rest);
        } else {
            return (scope, kind, key, trimmed);
        }
    }
}

fn render_search_result(result: &SearchResult) -> String {
    if result.records.is_empty() {
        return "no matching records".to_string();
    }
    let mut content = format!("{} records:\n", result.records.len());
    for record in &result.records {
        let _ = writeln!(
            content,
            "id={} scope={}/{} kind={} key={} revision={} text={}",
            record.id,
            record.scope.namespace,
            record.scope.id,
            record.kind,
            record.key,
            record.revision,
            record.text
        );
    }
    content.trim_end_matches('\n').to_string()
}

fn render_list_result(records: &[crate::memory::Record], next_cursor: &str) -> String {
    if records.is_empty() {
        return "no records".to_string();
    }
    let mut content = format!("{} records:\n", records.len());
    for record in records {
        let _ = writeln!(
            content,
            "id={} scope={}/{} kind={} key={} revision={} state={}{}{} created_at={} updated_at={} \
             verified_at={} review_at={} text={}",
            record.id,
            record.scope.namespace,
            record.scope.id,
            record.kind,
            record.key,
            record.revision,
            record.state.as_str(),
            if record.superseded_by.is_empty() {
                String::new()
            } else {
                format!(" superseded_by={}", record.superseded_by)
            },
            if record.state_reason.is_empty() {
                String::new()
            } else {
                format!(" state_reason={}", record.state_reason)
            },
            record.created_at,
            record.updated_at,
            record
                .verified_at
                .map(|time| time.to_string())
                .unwrap_or_default(),
            record
                .review_at
                .map(|time| time.to_string())
                .unwrap_or_default(),
            record.text
        );
    }
    if !next_cursor.is_empty() {
        let _ = writeln!(content, "next_cursor={next_cursor}");
    }
    content.trim_end_matches('\n').to_string()
}

/// Renders one compact catalog row. Bare `/skill` deliberately lists every
/// available name, but leaves potentially long descriptions and the
/// per-skill contract-check lookup to `/skill <name>` so the TUI does not
/// retain a large, expensive-to-render transcript entry.
fn skill_list_line(skill: &skill::Skill) -> String {
    let contract = if skill.contract.is_some() {
        " [contract]"
    } else {
        ""
    };
    format!("\n- {}{}", skill.name, contract)
}

pub(crate) fn skill_report(controller: &Controller, args: &str) -> String {
    use crate::app::management::Request;
    let request = match Request::parse("skill", args) {
        Ok(r) => r,
        Err(_) => return SKILL_USAGE.to_string(),
    };
    controller
        .management(request)
        .map(|reply| format_management_reply(&reply))
        .unwrap_or_else(|e| e)
}

fn command_error(command: &str, message: impl std::fmt::Display) -> Error {
    Error::Command {
        command: command.to_string(),
        message: message.to_string(),
    }
}

/// A free function, rather than a `Repl` method, so `tui::app`'s `/memory`
/// dispatch can reuse it against its own captured-output buffers instead of a
/// line-oriented `Repl`'s stdout/ stderr.
pub(crate) fn repl_memory_command(
    controller: &Controller,
    args: &str,
    stdout: &mut (dyn Write + Send),
    stderr: &mut (dyn Write + Send),
) -> Result<(), Error> {
    match controller.memory_command("memory", args) {
        Ok(crate::app::management::Reply::Usage(message)) => {
            let _ = writeln!(stderr, "{message}");
            Ok(())
        }
        Ok(reply) => {
            let _ = writeln!(stdout, "{}", format_management_reply(&reply));
            Ok(())
        }
        Err(error) if error.kind == crate::app::management::ManagementErrorKind::Invalid => {
            let _ = writeln!(stderr, "{}", error.message);
            Ok(())
        }
        Err(error) => Err(command_error("/memory", error.message)),
    }
}

fn render_pending_candidates(candidates: &[crate::memory::Candidate]) -> String {
    if candidates.is_empty() {
        return "no pending candidates".to_string();
    }
    let mut out = format!("{} pending candidates:", candidates.len());
    for candidate in candidates {
        let proposed = &candidate.proposed;
        let origin = proposed.source.origin.map_or("unknown", |o| o.as_str());
        let _ = write!(
            out,
            "\nid={} scope={}/{} action={} kind={} key={} confidence={} origin={} text={} reason={}",
            candidate.id,
            proposed.scope.namespace,
            proposed.scope.id,
            candidate.action.as_str(),
            proposed.kind,
            proposed.key,
            proposed.confidence,
            origin,
            first_runes(&one_line(&proposed.text), 120),
            first_runes(&one_line(&candidate.reason), 120),
        );
        if let Some(line) = crate::memory::advice::advice_line(proposed) {
            let _ = write!(out, "\n  {line}");
        }
    }
    out
}

/// See [`repl_memory_command`] for why this is a free function.
pub(crate) fn repl_remember_command(
    controller: &Controller,
    args: &str,
    stdout: &mut (dyn Write + Send),
    stderr: &mut (dyn Write + Send),
) -> Result<(), Error> {
    match controller.memory_command("remember", args) {
        Ok(crate::app::management::Reply::Usage(message)) => {
            let _ = writeln!(stderr, "{message}");
            Ok(())
        }
        Ok(reply) => {
            let _ = writeln!(stdout, "{}", format_management_reply(&reply));
            Ok(())
        }
        Err(error) if error.kind == crate::app::management::ManagementErrorKind::Invalid => {
            let _ = writeln!(stderr, "{}", error.message);
            Ok(())
        }
        Err(error) => Err(command_error("/remember", error.message)),
    }
}

impl Repl<'_> {
    pub(super) fn memory_command(&mut self, args: &str) -> Result<(), Error> {
        repl_memory_command(self.controller, args, &mut *self.stdout, &mut *self.stderr)
    }

    pub(super) fn remember_command(&mut self, args: &str) -> Result<(), Error> {
        repl_remember_command(self.controller, args, &mut *self.stdout, &mut *self.stderr)
    }

    pub(super) fn skill_command(&mut self, args: &str) {
        let text = skill_report(self.controller, args);
        if text == SKILL_USAGE || text.starts_with("unknown skill:") || text.starts_with("skill ") {
            let _ = writeln!(self.stderr, "{text}");
        } else {
            let _ = writeln!(self.stdout, "{text}");
        }
    }

    pub(super) fn tasks_command(&mut self) {
        let Some(tasks) = self.controller.subagent_tasks() else {
            let _ = writeln!(self.stderr, "{SUBAGENTS_UNAVAILABLE}");
            return;
        };
        let list = tasks.list();
        if list.is_empty() {
            let _ = writeln!(self.stdout, "no tasks in this session");
            return;
        }
        let now = Utc::now();
        for task in &list {
            let _ = writeln!(self.stdout, "{}", task_line(task, now));
        }
    }

    /// `/agents`: the latest 50 recorded sub-agent tasks, from `tasks.db`,
    /// across every session and process. `None` on the builder's recorder
    /// (no `~/.otto/tasks.db`, or it failed to open) prints the same "no
    /// recorded tasks" line as an empty result, since [`Builder::tasks_list`]
    /// already degrades that way.
    pub(super) fn agents_command(&mut self) {
        let query = record::ListQuery {
            limit: Some(50),
            ..Default::default()
        };
        match self.controller.builder().tasks_list(&query) {
            Ok(result) if result.tasks.is_empty() => {
                let _ = writeln!(self.stdout, "no recorded tasks");
            }
            Ok(result) => {
                for row in &result.tasks {
                    let _ = writeln!(self.stdout, "{}", agent_row_line(row));
                }
            }
            Err(error) => {
                let _ = writeln!(self.stderr, "{error}");
            }
        }
    }

    /// `/timers`: the session's outstanding timers, or one cancelled.
    pub(super) fn timers_command(&mut self, args: &str) {
        match timers_report(self.controller, args) {
            Ok(text) => {
                let _ = writeln!(self.stdout, "{text}");
            }
            Err(text) => {
                let _ = writeln!(self.stderr, "{text}");
            }
        }
    }

    pub(super) fn task_command(&mut self, args: &str) {
        if args.trim().is_empty() {
            self.tasks_command();
            return;
        }
        let Some(tasks) = self.controller.subagent_tasks() else {
            let _ = writeln!(self.stderr, "{SUBAGENTS_UNAVAILABLE}");
            return;
        };
        let fields: Vec<&str> = args.split_whitespace().collect();
        if fields.len() == 2 && fields[0] == "cancel" {
            match tasks.cancel(fields[1]) {
                Ok(()) => {
                    let _ = writeln!(self.stdout, "canceled {}", fields[1]);
                }
                Err(error) => {
                    let _ = writeln!(self.stderr, "{error}");
                }
            }
            return;
        }
        if fields.len() != 1 || fields[0] == "cancel" {
            let _ = writeln!(self.stderr, "{TASK_USAGE}");
            return;
        }
        let id = fields[0];
        let Some(task) = tasks.get(id) else {
            let _ = writeln!(self.stderr, "unknown task: {id}");
            return;
        };
        let _ = writeln!(self.stdout, "{}", task_line(&task, Utc::now()));
        if !task.model.is_empty() {
            let _ = writeln!(self.stdout, "model: {}", task.model);
        }
        if !task.session_path.is_empty() {
            let _ = writeln!(self.stdout, "transcript: {}", task.session_path);
        }
        if let Some(history) = tasks.history(id)
            && !history.is_empty()
        {
            let _ = write!(self.stdout, "{}", task_steps(&history));
        }
        if task.is_final() {
            match task.error.is_empty() {
                false => {
                    let _ = writeln!(self.stdout, "error: {}", task.error);
                }
                true => {
                    let _ = writeln!(self.stdout, "result: {}", task.result);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
