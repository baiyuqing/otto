//! The line-oriented frontend.
//!
//! `/sandbox reload` goes through [`Controller::reload_sandbox`], which the
//! composition root in `otto_cli::run` wires to the process sandbox switch, so
//! it re-points bash without restarting.
//!
//! Input: one blocking reader task feeds a bounded channel, so a parent
//! cancellation is observed while the loop is idle waiting for a line. A line
//! longer than [`MAX_INPUT_BYTES`] ends the loop with an error.
//!
//! Sub-agent wake turns: the loop also selects on the task registry's update
//! signal and runs an empty-text turn whenever a notification is pending, and
//! [`Repl::run_once`] drains the registry before returning.

use std::io::{BufRead, Read, Write};
use std::sync::Arc;

use otto_core::agent::{AgentError, CompactionResult, Event};
use otto_core::model::{EffectCertainty, OperationDisposition, OperationOutcome};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::subagent::tasks::TaskError;

use super::controller::{Controller, MODEL_SWITCH_UNAVAILABLE};
use crate::runtime::sandbox_config::{SandboxChange, parse_exclude_entry};

mod command;
mod compaction;
mod prompt;
mod render;
mod settings;
use compaction::*;
use render::*;
use settings::*;

/// The longest line the REPL accepts.
pub const MAX_INPUT_BYTES: usize = 1 << 20;

const LOGO: &str = "     ____  __  __\n    / __ \\/ /_/ /____\n   / /_/ / __/ __/ __ \\\n   \\____/\\__/\\__/\\____/\n";

const HELP: &str = concat!(
    "Help\n",
    "/help     show commands\n",
    "\n",
    "Setup\n",
    "/init     create a repository AGENTS.md guide\n",
    "/login [status] sign in to ChatGPT (or show status)\n",
    "/logout   sign out of ChatGPT\n",
    "/mcp      show configured MCP servers and their status\n",
    "/mcp login <server> sign in to an MCP server that uses OAuth\n",
    "/skill [name] show skills or one skill; /skill set <name> enabled|disabled change a skill state\n",
    "\n",
    "During a task\n",
    "/model [provider/model_id] [--thinking LEVEL] [--save] show current model, or switch models\n",
    "/thinking [LEVEL] [--save] show or set reasoning effort\n",
    "/compact [focus] compact context\n",
    "\n",
    "Approvals and sandbox\n",
    "/permissions [manual|auto|yolo | policy on_request|never | reviewer user|auto_review] show or set approval controls\n",
    "/approve <id> allow one exact elevated Bash command\n",
    "/approve <id> always run this exact command outside the sandbox from now on\n",
    "/sandbox [reload] show sandbox state, or apply the current [sandbox] configuration\n",
    "/sandbox allow <path> let sandboxed commands read a path\n",
    "/sandbox network allow|deny set sandboxed network access\n",
    "/sandbox exclude <entry> run matching commands outside the sandbox\n",
    "\n",
    "Memory\n",
    "/memory list [--scope current|user|workspace|all] [--limit N] [--cursor CURSOR] | /memory show <id> | /memory search <query> | /memory forget <id> | /memory review <id> accept|reject\n",
    "/memory history <id> | /memory diff <id> <from> [<to>] | /memory restore <id> <revision>\n",
    "/remember [--scope user|workspace] [--kind K] [--key K] <text>\n",
    "\n",
    "Tasks\n",
    "/task     list sub-agent tasks\n",
    "/task <id> show a task's steps and result\n",
    "/task cancel <id> cancel a queued or running task\n",
    "/agents   list the latest 50 recorded sub-agent tasks, any session\n",
    "/timers   list this session's timers\n",
    "/timers cancel <id> cancel a timer\n",
    "\n",
    "Sessions\n",
    "/new      start a new session\n",
    "/clear    start a new session\n",
    "/rename <name> rename current session\n",
    "/archive  archive current session and start a new one\n",
    "/delete   permanently delete current session; /delete confirm is irreversible\n",
    "/session  show session details\n",
    "\n",
    "Exit\n",
    "/exit     exit Otto\n",
);

/// Why the loop stopped.
#[derive(Debug)]
pub enum Error {
    /// The process context was cancelled.
    Cancelled,
    /// The input could not be read, or a line exceeded the limit.
    Input(String),
    /// The command that failed and its message.
    Command { command: String, message: String },
    /// A turn failed. `fatal` marks a fatal persistence failure, the only turn
    /// failure that ends the loop.
    Turn { message: String, fatal: bool },
}

impl std::fmt::Display for Error {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => write!(formatter, "context canceled"),
            Self::Input(message) => write!(formatter, "{message}"),
            Self::Command { message, .. } => write!(formatter, "{message}"),
            Self::Turn { message, .. } => write!(formatter, "{message}"),
        }
    }
}

impl std::error::Error for Error {}

pub fn is_command_error(error: &Error, command: &str) -> bool {
    matches!(error, Error::Command { command: name, .. } if name == command)
}

pub struct Repl<'a> {
    pub(super) controller: &'a Controller,
    pub(super) stdout: Box<dyn Write + Send + 'a>,
    pub(super) stderr: Box<dyn Write + Send + 'a>,
    program_status: Option<crate::program_status::Reporter>,
    delete_confirmation: Option<String>,
}

impl<'a> Repl<'a> {
    pub fn new(
        controller: &'a Controller,
        stdout: Box<dyn Write + Send + 'a>,
        stderr: Box<dyn Write + Send + 'a>,
    ) -> Self {
        Self {
            controller,
            stdout,
            stderr,
            program_status: None,
            delete_confirmation: None,
        }
    }

    /// Enables OSC 7501 for a frontend that established stdin and stdout are
    /// terminals. Tests and embedded callers keep using [`Self::new`].
    pub fn with_program_status(
        controller: &'a Controller,
        mut stdout: Box<dyn Write + Send + 'a>,
        stderr: Box<dyn Write + Send + 'a>,
    ) -> Self {
        let reporter = crate::program_status::Reporter::default();
        reporter.report(&mut *stdout, crate::program_status::State::Idle);
        Self {
            controller,
            stdout,
            stderr,
            program_status: Some(reporter),
            delete_confirmation: None,
        }
    }

    /// The interactive loop.
    pub async fn run<R: BufRead + Send + 'static>(
        &mut self,
        input: R,
        cancel: &CancellationToken,
    ) -> Result<(), Error> {
        let _ = write!(self.stdout, "{LOGO}");
        let info = self.controller.info();
        if !info.session_id.is_empty() {
            let _ = writeln!(self.stdout, "Session: {}", info.session_id);
        }
        let _ = writeln!(self.stdout, "Sandbox: {}", info.sandbox.summary());
        self.print_sandbox_read_notes(&info.sandbox);

        let mut approval_queue = std::collections::VecDeque::<String>::new();
        let mut lines = spawn_reader(input);
        let mut updates: Option<(Arc<dyn otto_core::mailbox::Mailbox>, watch::Receiver<u64>)> =
            None;
        let mut notices = self.controller.notices_changed();
        let mut memory_review = self.controller.memory_review_available_changed();
        let mut memory_review_notice = false;
        let mut shown_approvals = std::collections::BTreeSet::new();
        let mut approval_tick = tokio::time::interval(std::time::Duration::from_millis(500));
        loop {
            if self.controller.pending_approval().is_none()
                && let Some(line) = approval_queue.pop_front()
            {
                if line.starts_with('/') {
                    if self.command(line.trim_end(), cancel).await? {
                        return Ok(());
                    }
                } else {
                    self.interactive_prompt(&line, cancel, &mut lines, &mut approval_queue)
                        .await?;
                }
                continue;
            }
            if memory_review_notice {
                let _ = writeln!(self.stdout, "{}", crate::app::MEMORY_REVIEW_AVAILABLE);
                memory_review_notice = false;
            }
            // Lines background operations queued while a turn or command ran.
            for line in self.controller.take_notices() {
                let _ = writeln!(self.stdout, "{line}");
            }
            let _ = write!(self.stdout, "{}", crate::tui::gutter::USER_MARK);
            let _ = self.stdout.flush();
            // The receiver is kept across iterations rather than re-subscribed:
            // subscribing now would mark a signal raised during the last turn
            // as already seen.
            match self.controller.notification_mailbox() {
                Some(tasks) => {
                    if updates
                        .as_ref()
                        .is_none_or(|(held, _)| !Arc::ptr_eq(held, &tasks))
                    {
                        let receiver =
                            crate::runtime::workflow_notifications::updates(tasks.as_ref());
                        updates = Some((tasks, receiver));
                    }
                }
                None => updates = None,
            }
            // `Ok(line)` is a read; `Err(open)` is a registry signal, where
            // `open == false` means the session was replaced and the next
            // iteration re-reads the registry.
            let read = {
                let signal = async {
                    match updates.as_mut() {
                        Some((_, receiver)) => receiver.changed().await.is_ok(),
                        None => std::future::pending().await,
                    }
                };
                let approval_signal = async {
                    loop {
                        approval_tick.tick().await;
                        if let Ok(snapshot) = self.controller.approval_snapshot() {
                            shown_approvals.retain(|id| {
                                snapshot.requests.iter().any(|request| &request.id == id)
                            });
                            let added: Vec<_> = snapshot
                                .requests
                                .into_iter()
                                .filter(|request| shown_approvals.insert(request.id.clone()))
                                .collect();
                            if !added.is_empty() {
                                return added;
                            }
                        }
                    }
                };
                tokio::select! {
                    _ = cancel.cancelled() => return Err(Error::Cancelled),
                    requests = approval_signal => {
                        for request in requests {
                            let event = Event::ApprovalChanged { request: Box::new(request) };
                            if let Some(reporter) = &self.program_status {
                                reporter.observe(&mut *self.stdout, &event);
                            }
                            render_event(&mut *self.stdout, &mut *self.stderr, &event);
                        }
                        Err(None)
                    },
                    open = signal => Err(Some(open)),
                    // The top of the loop prints it and shows the prompt again.
                    _ = notices.changed() => Err(None),
                    _ = memory_review.changed() => {
                        memory_review_notice = true;
                        Err(None)
                    }
                    line = lines.recv() => Ok(line),
                }
            };
            let line = match read {
                Ok(line) => line,
                Err(None) => {
                    let _ = writeln!(self.stdout);
                    continue;
                }
                Err(Some(false)) => {
                    updates = None;
                    continue;
                }
                Err(Some(true)) => match self.wake(cancel).await {
                    Ok(_) | Err(Error::Turn { fatal: false, .. }) => continue,
                    Err(error) => return Err(error),
                },
            };
            let Some(line) = line else { return Ok(()) };
            let line = line?;
            let line = line.trim_end();
            if line.is_empty() {
                continue;
            }
            if line.starts_with('/') {
                if self.command(line, cancel).await? {
                    return Ok(());
                }
                continue;
            }
            match self.controller.approval_message(line, cancel).await {
                Ok(Some(reply)) => {
                    let _ = writeln!(self.stdout, "{}", reply.text);
                    if reply.queued {
                        approval_queue.push_back(line.to_owned());
                    }
                    continue;
                }
                Ok(None) => {}
                Err(message) => {
                    let _ = writeln!(self.stderr, "{message}");
                    continue;
                }
            }
            match self
                .interactive_prompt(line, cancel, &mut lines, &mut approval_queue)
                .await
            {
                Ok(()) => {}
                Err(Error::Turn { fatal: false, .. }) => {}
                Err(error) => return Err(error),
            }
        }
    }

    /// One prompt with the same rendering as [`Repl::run`], without the banner
    /// or the prompt marker. After the turn it waits out any sub-agent task
    /// still running and wakes until nothing is pending, so a one-shot `-p` run
    /// does not exit with children in flight.
    pub async fn run_once(
        &mut self,
        prompt: &str,
        cancel: &CancellationToken,
    ) -> Result<(), Error> {
        self.prompt(prompt, cancel).await?;
        self.drain_tasks(cancel).await
    }
}

/// The name and the trimmed remainder, or `None` when the line is not a
/// command.
fn split_command(command: &str) -> Option<(&str, &str)> {
    let body = command.strip_prefix('/')?;
    if body.is_empty() {
        return None;
    }
    match body.find(char::is_whitespace) {
        Some(index) => Some((&body[..index], body[index..].trim())),
        None => Some((body, "")),
    }
}

fn first_line(content: &str) -> &str {
    match content.find('\n') {
        Some(index) => &content[..index],
        None => content,
    }
}

pub(crate) fn is_fatal_persistence(error: &AgentError) -> bool {
    matches!(error, AgentError::Persist { source, .. }
        if source.to_string().starts_with("fatal session persistence failure"))
}

/// Reads lines on a blocking OS thread so the loop can wait on cancellation at
/// the same time. This deliberately avoids [`tokio::task::spawn_blocking`]:
/// after `/exit` or Ctrl-C, terminal stdin may stay blocked in `read` until the
/// user types another line, and Tokio waits for tracked blocking tasks during
/// runtime shutdown. A plain thread is not joined by Tokio, so process exit can
/// continue once the REPL loop has returned.
fn spawn_reader<R: BufRead + Send + 'static>(
    mut input: R,
) -> tokio::sync::mpsc::Receiver<Result<String, Error>> {
    let (sender, receiver) = tokio::sync::mpsc::channel(1);
    std::thread::spawn(move || {
        loop {
            let mut buffer = Vec::new();
            // One byte past the limit plus the newline, so an over-long line
            // is detected without reading it all into memory.
            let read = match (&mut input)
                .take(MAX_INPUT_BYTES as u64 + 2)
                .read_until(b'\n', &mut buffer)
            {
                Ok(read) => read,
                Err(error) => {
                    let _ = sender.blocking_send(Err(Error::Input(error.to_string())));
                    return;
                }
            };
            if read == 0 {
                return;
            }
            if buffer.last() == Some(&b'\n') {
                buffer.pop();
                if buffer.last() == Some(&b'\r') {
                    buffer.pop();
                }
            }
            if buffer.len() > MAX_INPUT_BYTES {
                let _ = sender.blocking_send(Err(Error::Input(format!(
                    "input line too long: maximum is {MAX_INPUT_BYTES} bytes"
                ))));
                return;
            }
            let line = String::from_utf8_lossy(&buffer).into_owned();
            if sender.blocking_send(Ok(line)).is_err() {
                return;
            }
        }
    });
    receiver
}

#[cfg(test)]
mod tests;
