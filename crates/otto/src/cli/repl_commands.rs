//! The REPL's memory and sub-agent commands.
//!
//! `/memory review` lists pending candidates (queued by the model's `remember`
//! tool or any other non-human writer) and `/memory review <id> accept|reject`
//! decides one. Like `/memory forget`, it is a human command.
//!
//! `repl_memory_command` and `repl_remember_command` are free functions,
//! not [`Repl`] methods, so `tui::app`'s `/memory`/`/remember` dispatch can
//! call them directly against its own captured-output buffers; [`Repl`]'s own
//! `memory_command`/`remember_command` are thin wrappers over the same two
//! functions.

use std::io::Write;
use std::path::Path;
#[cfg(test)]
use std::sync::Arc;
use std::sync::Mutex;

use chrono::Utc;
use otto_core::config::{McpAuth, McpTransport};
use tokio_util::sync::CancellationToken;

#[cfg(test)]
use crate::memory::RememberRequest;
use crate::schedule::remind::{NO_TIMERS, timer_line};
use crate::subagent::format::{first_runes, one_line, task_line, task_steps};
use crate::subagent::record::{self, TaskRow};

use crate::app::management::{format_management_reply, format_mcp_report};

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
