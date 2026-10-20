//! The line-oriented frontend.
//!
//! `/sandbox reload` goes through [`Controller::reload_sandbox`], which the
//! composition root in [`super::run`] wires to the process sandbox switch, so
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

use super::controller::{Controller, PROFILE_SWITCH_UNAVAILABLE};
use crate::runtime::sandbox_config::{SandboxChange, parse_exclude_entry};

/// The longest line the REPL accepts.
pub const MAX_INPUT_BYTES: usize = 1 << 20;

const LOGO: &str = "     ____  __  __\n    / __ \\/ /_/ /____\n   / /_/ / __/ __/ __ \\\n   \\____/\\__/\\__/\\____/\n";

const HELP: &str = "/auto [manual|auto|yolo] show or set session approval mode\n/help     show commands\n/init     create a repository AGENTS.md guide\n/exit     exit Otto\n/new      start a new session\n/clear    start a new session\n/session  show session details\n/rename <name> rename current session\n/archive  archive current session and start a new one\n/model [profile] [--thinking LEVEL] [--save] show current model, or switch profiles\n/thinking [LEVEL] [--save] show or set reasoning effort\n/compact [focus] compact context\n/reflect [focus] write a retrospective and queue memory and skill proposals for review\n/reflect list | /reflect show <id> past reflection runs\n/sandbox [reload] show sandbox state, or apply the current [sandbox] configuration\n/sandbox allow <path> let sandboxed commands read a path\n/sandbox network allow|deny set sandboxed network access\n/sandbox exclude <entry> run matching commands outside the sandbox\n/approve <id> allow one exact elevated Bash command\n/approve <id> always run this exact command outside the sandbox from now on\n/memory list [--scope current|user|workspace|all] [--limit N] [--cursor CURSOR] | /memory show <id> | /memory search <query> | /memory forget <id> | /memory review <id> accept|reject\n/remember [--scope user|workspace] [--kind K] [--key K] <text>\n/skill [name] show skills or one skill; /skill set <name> enabled|disabled change a skill state\n/skill generated list skills reflection wrote; /skill revert <name> undo one\n/task     list sub-agent tasks\n/task <id> show a task's steps and result\n/task cancel <id> cancel a queued or running task\n/agents   list the latest 50 recorded sub-agent tasks, any session\n/timers   list this session's timers\n/timers cancel <id> cancel a timer\n/login [status] sign in to ChatGPT (or show status)\n/logout   sign out of ChatGPT\n/mcp      show configured MCP servers and their status\n/mcp login <server> sign in to an MCP server that uses OAuth\n";

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
            // Lines background reflection queued while a turn or command ran.
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

    /// One empty-text turn delivering the pending sub-agent notifications, and
    /// whether it ran. The leading newline keeps the output off the prompt
    /// marker.
    async fn wake(&mut self, cancel: &CancellationToken) -> Result<bool, Error> {
        let Self {
            controller,
            stdout,
            stderr,
            program_status,
        } = self;
        let wake = controller.prepare_wake().map_err(|message| Error::Turn {
            fatal: false,
            message,
        })?;
        let Some(wake) = wake else { return Ok(false) };
        if let Some(reporter) = program_status {
            reporter.report(&mut **stdout, crate::program_status::State::Working);
        }
        let _ = writeln!(stdout);
        let turn = cancel.child_token();
        let mut error_rendered = false;
        let result = {
            let mut sink = |event: Event| {
                if let Some(reporter) = program_status.as_ref() {
                    reporter.observe(&mut **stdout, &event);
                }
                if render_event(&mut **stdout, &mut **stderr, &event) {
                    error_rendered = true;
                }
            };
            wake.run(&mut sink, &turn).await
        };
        if let Some(reporter) = program_status {
            let canceled = cancel.is_cancelled()
                || result.as_ref().err().is_some_and(AgentError::is_cancelled);
            let state = crate::program_status::State::finished(result.is_ok(), canceled);
            reporter.report(&mut **stdout, state);
        };
        if let Err(error) = &result
            && !error_rendered
        {
            let _ = writeln!(stderr, "{error}");
        }
        let _ = writeln!(stdout);
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        match result {
            Ok(()) => Ok(true),
            Err(error) => Err(Error::Turn {
                fatal: is_fatal_persistence(&error),
                message: error.to_string(),
            }),
        }
    }

    /// Waits out every non-final task, then wakes until nothing is pending.
    /// Each wait and wake is bounded by `cancel`.
    async fn drain_tasks(&mut self, cancel: &CancellationToken) -> Result<(), Error> {
        let Some(tasks) = self.controller.subagent_tasks() else {
            return Ok(());
        };
        loop {
            for task in tasks.list() {
                if task.is_final() {
                    continue;
                }
                match tasks.wait(&task.id, cancel).await {
                    Ok(_) => {}
                    Err(TaskError::Canceled) => return Err(Error::Cancelled),
                    Err(error) => {
                        return Err(Error::Turn {
                            fatal: false,
                            message: error.to_string(),
                        });
                    }
                }
            }
            if !self.wake(cancel).await? {
                return Ok(());
            }
        }
    }

    async fn prompt(&mut self, line: &str, cancel: &CancellationToken) -> Result<(), Error> {
        let Self {
            controller,
            stdout,
            stderr,
            program_status,
        } = self;
        if let Some(reporter) = program_status {
            reporter.report(&mut **stdout, crate::program_status::State::Working);
        }
        let turn = cancel.child_token();
        let mut error_rendered = false;
        let result = {
            let mut sink = |event: Event| {
                if let Some(reporter) = program_status.as_ref() {
                    reporter.observe(&mut **stdout, &event);
                }
                if render_event(&mut **stdout, &mut **stderr, &event) {
                    error_rendered = true;
                }
            };
            controller.prompt(line, &mut sink, &turn).await
        };
        if let Some(reporter) = program_status {
            let canceled = cancel.is_cancelled()
                || result.as_ref().err().is_some_and(AgentError::is_cancelled);
            let state = crate::program_status::State::finished(result.is_ok(), canceled);
            reporter.report(&mut **stdout, state);
        }
        if let Err(error) = &result
            && !error_rendered
        {
            let _ = writeln!(stderr, "{error}");
        }
        let _ = writeln!(stdout);
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        match result {
            Ok(()) => Ok(()),
            Err(error) => Err(Error::Turn {
                fatal: is_fatal_persistence(&error),
                message: error.to_string(),
            }),
        }
    }

    /// Keep reading the trusted terminal while a prepared call waits. Ordinary
    /// work stays queued; approving never sends a replacement model prompt.
    async fn interactive_prompt(
        &mut self,
        line: &str,
        cancel: &CancellationToken,
        lines: &mut tokio::sync::mpsc::Receiver<Result<String, Error>>,
        queue: &mut std::collections::VecDeque<String>,
    ) -> Result<(), Error> {
        let controller = self.controller;
        if let Some(reporter) = &self.program_status {
            reporter.report(&mut *self.stdout, crate::program_status::State::Working);
        }
        let turn = cancel.child_token();
        let (send, mut events) = tokio::sync::mpsc::unbounded_channel();
        let mut sink = |event| {
            let _ = send.send(event);
        };
        let run = controller.prompt(line, &mut sink, &turn);
        tokio::pin!(run);
        let mut waiting = false;
        let mut input_closed = false;
        let mut input_error = None;
        let mut error_rendered = false;
        let result = loop {
            tokio::select! {
                result = &mut run => break result,
                Some(event) = events.recv() => {
                    if let Event::ApprovalChanged { request } = &event {
                        waiting = request.blocking && request.state == otto_core::approval::State::Pending;
                    }
                    if let Some(reporter) = &self.program_status {
                        reporter.observe(&mut *self.stdout, &event);
                    }
                    error_rendered |= render_event(&mut *self.stdout, &mut *self.stderr, &event);
                    let _ = self.stdout.flush();
                }
                input = lines.recv(), if waiting && !input_closed => {
                    let Some(input) = input else {
                        input_closed = true;
                        turn.cancel();
                        continue;
                    };
                    let input = match input {
                        Ok(input) => input,
                        Err(error) => {
                            input_error = Some(error);
                            input_closed = true;
                            turn.cancel();
                            continue;
                        }
                    };
                    let trimmed = input.trim_end();
                    if let Some(args) = trimmed.strip_prefix("/auto").filter(|args|args.is_empty() || args.starts_with(char::is_whitespace)) {
                        let line = self.controller.auto_mode_command(args).unwrap_or_else(|e|e);
                        let _ = writeln!(self.stdout,"{line}");
                        continue;
                    }
                    if let Some(args) = trimmed.strip_prefix("/permissions").filter(|args|args.is_empty() || args.starts_with(char::is_whitespace)) {
                        let line = self.controller.permissions_command(args).unwrap_or_else(|e|e);
                        let _ = writeln!(self.stdout,"{line}");
                        continue;
                    }
                    if trimmed == "/exit" || trimmed == "/stop" {
                        turn.cancel();
                        if trimmed == "/exit" { queue.push_front(input); }
                        continue;
                    }
                    let mut words = trimmed.splitn(3, char::is_whitespace);
                    let command = words.next().unwrap_or("");
                    let id = words.next().unwrap_or("");
                    let feedback = words.next().unwrap_or("").trim();
                    if matches!(command, "/approve" | "/deny" | "/withdraw") {
                        let scope = controller.approval_scope();
                        let service = controller.approval_service();
                        let decision = service.get(id, &scope.workspace, Some(&scope.session_id))
                            .and_then(|request| {
                                if command == "/withdraw" {
                                    service.withdraw(id, &request.scope, request.revision)
                                } else {
                                    service.decide(id, &request.scope, request.revision,
                                        command == "/approve", feedback,
                                        &crate::approval::Actor::local("repl"))
                                }
                            });
                        match decision {
                            Ok(request) => {
                                let _ = writeln!(self.stdout, "Approval {id}: {:?}.", request.state);
                            }
                            Err(error) => { let _ = writeln!(self.stderr, "{error}"); }
                        }
                    } else if !trimmed.is_empty() && !trimmed.starts_with('/') {
                        match controller.approval_message(trimmed, &turn).await {
                            Ok(Some(reply)) => {
                                let _ = writeln!(self.stdout, "{}", reply.text);
                                if reply.queued { queue.push_back(input); }
                            }
                            Ok(None) => queue.push_back(input),
                            Err(error) => { let _ = writeln!(self.stderr, "{error}"); }
                        }
                    } else if !trimmed.is_empty() {
                        queue.push_back(input);
                    }
                }
            }
        };
        while let Ok(event) = events.try_recv() {
            if let Some(reporter) = &self.program_status {
                reporter.observe(&mut *self.stdout, &event);
            }
            error_rendered |= render_event(&mut *self.stdout, &mut *self.stderr, &event);
        }
        if let Some(reporter) = &self.program_status {
            let canceled = cancel.is_cancelled()
                || result.as_ref().err().is_some_and(AgentError::is_cancelled);
            let state = crate::program_status::State::finished(result.is_ok(), canceled);
            reporter.report(&mut *self.stdout, state);
        }
        if let Err(error) = &result
            && !error_rendered
        {
            let _ = writeln!(self.stderr, "{error}");
        }
        let _ = writeln!(self.stdout);
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        if let Some(error) = input_error {
            return Err(error);
        }
        result.map_err(|error| Error::Turn {
            fatal: is_fatal_persistence(&error),
            message: error.to_string(),
        })
    }

    /// Dispatches one `/command`. Returns true when the loop should stop.
    async fn command(&mut self, command: &str, cancel: &CancellationToken) -> Result<bool, Error> {
        let outcome: Option<bool> = 'dispatch: {
            let Some((name, args)) = split_command(command) else {
                break 'dispatch None;
            };
            match name {
                "help" => {
                    if !args.is_empty() {
                        break 'dispatch None;
                    }
                    let _ = write!(self.stdout, "{HELP}");
                    let _ = writeln!(
                        self.stdout,
                        "/permissions [policy on_request|never | reviewer user|auto_review] inspect or set approval controls"
                    );
                    Some(false)
                }
                "init" => {
                    if !args.is_empty() {
                        break 'dispatch None;
                    }
                    self.prompt(otto_core::agent::INIT_PROMPT, cancel).await?;
                    Some(false)
                }
                "exit" => {
                    if !args.is_empty() {
                        break 'dispatch None;
                    }
                    Some(true)
                }
                "new" | "clear" => {
                    if !args.is_empty() {
                        break 'dispatch None;
                    }
                    self.controller
                        .new_session()
                        .await
                        .map_err(|message| Error::Command {
                            command: command.to_string(),
                            message,
                        })?;
                    self.print_session_id();
                    Some(false)
                }
                "archive" => {
                    if !args.is_empty() {
                        break 'dispatch None;
                    }
                    let result =
                        self.controller
                            .archive_current_session()
                            .await
                            .map_err(|message| Error::Command {
                                command: command.to_string(),
                                message,
                            })?;
                    let _ = writeln!(self.stdout, "Archived: {}", result.path);
                    self.print_session_id();
                    Some(false)
                }
                "session" => {
                    if !args.is_empty() {
                        break 'dispatch None;
                    }
                    let info = self.controller.info();
                    let _ = writeln!(
                        self.stdout,
                        "ID: {}\nPath: {}\nProvider: {}\nModel: {}\nThinking: {}\nSandbox: {}",
                        info.session_id,
                        info.session_path,
                        info.provider,
                        info.model,
                        display_thinking(&info.thinking),
                        info.sandbox.summary()
                    );
                    if !info.session_name.is_empty() {
                        let _ = writeln!(self.stdout, "Name: {}", info.session_name);
                    }
                    let reason = info.sandbox.reason_code();
                    if !reason.is_empty() {
                        let _ = writeln!(self.stdout, "Sandbox reason: {reason}");
                    }
                    Some(false)
                }
                "rename" => {
                    if args.is_empty() {
                        break 'dispatch None;
                    }
                    self.controller
                        .rename_session(args)
                        .map_err(|message| Error::Command {
                            command: command.to_string(),
                            message,
                        })?;
                    let _ = writeln!(self.stdout, "Renamed session: {args}");
                    Some(false)
                }
                "compact" => {
                    self.compact(args, cancel).await?;
                    Some(false)
                }
                "reflect" => {
                    match super::repl_commands::reflect_runs_report(self.controller, args) {
                        Some(text) => {
                            let _ = writeln!(self.stdout, "{text}");
                        }
                        None => self.reflect(args, cancel).await?,
                    }
                    Some(false)
                }
                "model" => {
                    self.model(args).await?;
                    Some(false)
                }
                "thinking" => {
                    self.thinking(args).await?;
                    Some(false)
                }
                "auto" => {
                    let line = self.controller.auto_mode_command(args).map_err(|message| {
                        Error::Command {
                            command: "/auto".into(),
                            message,
                        }
                    })?;
                    let _ = writeln!(self.stdout, "{line}");
                    Some(false)
                }
                "permissions" => {
                    let line = self
                        .controller
                        .permissions_command(args)
                        .map_err(|message| Error::Command {
                            command: "/permissions".into(),
                            message,
                        })?;
                    let _ = writeln!(self.stdout, "{line}");
                    Some(false)
                }
                "sandbox" => self.sandbox(args).await?.then_some(false),
                "approve" | "deny" | "withdraw" => {
                    let mut parts = args.trim().splitn(2, char::is_whitespace);
                    let shared_id = parts.next().unwrap_or("");
                    let feedback = parts.next().unwrap_or("").trim();
                    let scope = self.controller.approval_scope();
                    let service = self.controller.approval_service();
                    if let Ok(request) =
                        service.get(shared_id, &scope.workspace, Some(&scope.session_id))
                    {
                        if name == "approve" && feedback == "always" && !request.persistent {
                            return Err(Error::Command { command: "/approve".into(), message: "This request grants only one operation; persistent changes need their own review.".into() });
                        }
                        let decided = if name == "withdraw" {
                            service.withdraw(shared_id, &request.scope, request.revision)
                        } else {
                            service.decide(
                                shared_id,
                                &request.scope,
                                request.revision,
                                name == "approve",
                                feedback,
                                &crate::approval::Actor::local("repl"),
                            )
                        }
                        .map_err(|error| Error::Command {
                            command: format!("/{name}"),
                            message: error.to_string(),
                        })?;
                        let decided = service.finish_decision(decided).await.map_err(|error| {
                            Error::Command {
                                command: format!("/{name}"),
                                message: error.to_string(),
                            }
                        })?;
                        let _ = writeln!(
                            self.stdout,
                            "Approval {shared_id}: {:?}; application: {:?}. {}",
                            decided.state, decided.application, decided.application_error
                        );
                        break 'dispatch Some(false);
                    }
                    if name != "approve" {
                        return Err(Error::Command {
                            command: format!("/{name}"),
                            message: "approval not found".into(),
                        });
                    }
                    let (id, always) = match args.split_whitespace().collect::<Vec<_>>()[..] {
                        [id] => (id, false),
                        [id, "always"] => (id, true),
                        _ => break 'dispatch None,
                    };
                    let approved = if always {
                        self.controller.approve_bash_always(id).await
                    } else {
                        self.controller.approve_tool(id).await
                    };
                    let retry = approved.map_err(|message| Error::Command {
                        command: "/approve".to_string(),
                        message,
                    })?;
                    let _ = writeln!(self.stdout, "Approved {id}.");
                    self.prompt(&retry, cancel).await?;
                    Some(false)
                }
                "login" => {
                    super::login::repl_login(
                        self.controller,
                        &mut *self.stdout,
                        &mut *self.stderr,
                        args,
                        cancel,
                    )
                    .await?;
                    Some(false)
                }
                "logout" => {
                    if !args.is_empty() {
                        break 'dispatch None;
                    }
                    super::login::repl_logout(self.controller, &mut *self.stdout, cancel)?;
                    Some(false)
                }
                "mcp" => {
                    super::repl_commands::repl_mcp_command(
                        self.controller,
                        args,
                        &mut *self.stdout,
                        &mut *self.stderr,
                        cancel,
                    )
                    .await?;
                    Some(false)
                }
                "memory" => {
                    self.memory_command(args)?;
                    Some(false)
                }
                "remember" => {
                    self.remember_command(args)?;
                    Some(false)
                }
                "skill" => {
                    self.skill_command(args);
                    Some(false)
                }
                "tasks" => {
                    self.tasks_command();
                    Some(false)
                }
                "task" => {
                    self.task_command(args);
                    Some(false)
                }
                "agents" => {
                    if !args.is_empty() {
                        break 'dispatch None;
                    }
                    self.agents_command();
                    Some(false)
                }
                "timers" => {
                    self.timers_command(args);
                    Some(false)
                }
                _ => None,
            }
        };
        match outcome {
            Some(exit) => Ok(exit),
            None => {
                let _ = writeln!(self.stderr, "unknown command: {command}");
                Ok(false)
            }
        }
    }

    async fn model(&mut self, args: &str) -> Result<(), Error> {
        let unavailable = || Error::Command {
            command: "/model".to_string(),
            message: PROFILE_SWITCH_UNAVAILABLE.to_string(),
        };
        if !self.controller.dynamic_content() {
            return Err(unavailable());
        }
        let parsed = parse_model_args(args).map_err(|message| Error::Command {
            command: "/model".to_string(),
            message,
        })?;
        if parsed.profile.is_empty() {
            let info = self.controller.info();
            let _ = writeln!(
                self.stdout,
                "Current: profile {} (provider {}, model {}, thinking {})",
                info.profile,
                info.provider,
                info.model,
                display_thinking(&info.thinking)
            );
            let profiles = self.controller.profile_summaries();
            if profiles.is_empty() {
                let _ = writeln!(self.stdout, "No profiles configured.");
            } else {
                let list = profiles
                    .iter()
                    .map(|profile| {
                        format!(
                            "{} ({}/{}, thinking {})",
                            profile.name,
                            profile.provider,
                            profile.model,
                            display_thinking(&profile.thinking)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                let _ = writeln!(self.stdout, "Profiles: {list}");
            }
            return Ok(());
        }
        self.controller
            .switch_profile(&parsed.profile)
            .await
            .map_err(|message| Error::Command {
                command: "/model".to_string(),
                message,
            })?;
        if !parsed.thinking.is_empty() {
            self.controller
                .set_thinking(&parsed.thinking)
                .await
                .map_err(|message| Error::Command {
                    command: "/model".to_string(),
                    message,
                })?;
        }
        let saved = self.controller.set_default_profile(&parsed.profile);
        if parsed.save && !parsed.thinking.is_empty() {
            self.controller
                .save_profile_thinking(&parsed.thinking)
                .map_err(|message| Error::Command {
                    command: "/model".to_string(),
                    message,
                })?;
        }
        let info = self.controller.info();
        match saved {
            Ok(()) => {
                let _ = writeln!(
                    self.stdout,
                    "Switched to profile {} (provider {}, model {}, thinking {}). Set as default profile.",
                    info.profile,
                    info.provider,
                    info.model,
                    display_thinking(&info.thinking)
                );
            }
            Err(message) => {
                let _ = writeln!(
                    self.stdout,
                    "Switched to profile {} (provider {}, model {}, thinking {}), but the default profile was not saved: {}",
                    info.profile,
                    info.provider,
                    info.model,
                    display_thinking(&info.thinking),
                    message
                );
            }
        }
        if parsed.save && !parsed.thinking.is_empty() {
            let _ = writeln!(self.stdout, "Saved thinking to profile.");
        }
        if !info.session_id.is_empty() {
            let _ = writeln!(self.stdout, "Session: {}", info.session_id);
        }
        Ok(())
    }

    async fn thinking(&mut self, args: &str) -> Result<(), Error> {
        let parsed = parse_thinking_args(args).map_err(|message| Error::Command {
            command: "/thinking".to_string(),
            message,
        })?;
        if parsed.thinking.is_empty() {
            let info = self.controller.info();
            let _ = writeln!(
                self.stdout,
                "Thinking: {}",
                display_thinking(&info.thinking)
            );
            return Ok(());
        }
        self.controller
            .set_thinking(&parsed.thinking)
            .await
            .map_err(|message| Error::Command {
                command: "/thinking".to_string(),
                message,
            })?;
        if parsed.save {
            self.controller
                .save_profile_thinking(&parsed.thinking)
                .map_err(|message| Error::Command {
                    command: "/thinking".to_string(),
                    message,
                })?;
        }
        let info = self.controller.info();
        let _ = writeln!(
            self.stdout,
            "Thinking: {}",
            display_thinking(&info.thinking)
        );
        if parsed.save {
            let _ = writeln!(self.stdout, "Saved thinking to profile.");
        }
        Ok(())
    }

    /// False means "unknown command".
    ///
    /// `allow`, `network` and `exclude` write the `[sandbox]` table and reload it, the
    /// same amendment the TUI confirms through a picker; here the typed
    /// command is the confirmation.
    async fn sandbox(&mut self, args: &str) -> Result<bool, Error> {
        let (subcommand, rest) = match args.split_once(char::is_whitespace) {
            Some((subcommand, rest)) => (subcommand, rest.trim()),
            None => (args, ""),
        };
        match (subcommand, rest) {
            ("", _) => {
                self.print_sandbox(self.controller.sandbox_info());
                Ok(true)
            }
            ("reload", "") => {
                let info = self
                    .controller
                    .reload_sandbox()
                    .await
                    .map_err(sandbox_error)?;
                self.print_sandbox(info);
                Ok(true)
            }
            ("allow", path) => {
                let resolved = self
                    .controller
                    .resolve_sandbox_read_path(path)
                    .map_err(sandbox_error)?;
                let info = self
                    .controller
                    .amend_sandbox(SandboxChange::AllowReadPath(resolved))
                    .await
                    .map_err(sandbox_error)?;
                self.print_sandbox(info);
                Ok(true)
            }
            ("network", mode @ ("allow" | "deny")) => {
                let info = self
                    .controller
                    .amend_sandbox(SandboxChange::Network(mode.to_string()))
                    .await
                    .map_err(sandbox_error)?;
                self.print_sandbox(info);
                Ok(true)
            }
            ("exclude", entry) => {
                let entry = parse_exclude_entry(entry).map_err(sandbox_error)?;
                let info = self
                    .controller
                    .amend_sandbox(SandboxChange::ExcludeCommand(entry))
                    .await
                    .map_err(sandbox_error)?;
                self.print_sandbox(info);
                Ok(true)
            }
            _ => {
                let _ = writeln!(self.stderr, "unknown command: /sandbox {args}");
                Ok(true)
            }
        }
    }

    fn print_sandbox(&mut self, info: crate::runtime::info::SandboxInfo) {
        let _ = writeln!(self.stdout, "Sandbox: {}", info.summary());
        let reason = info.reason_code();
        if !reason.is_empty() {
            let _ = writeln!(self.stdout, "Sandbox reason: {reason}");
        }
    }

    /// Includes the checkpoint de-duplication that keeps an event and the
    /// returned result from rendering twice.
    async fn compact(&mut self, focus: &str, cancel: &CancellationToken) -> Result<(), Error> {
        let Self {
            controller,
            stdout,
            stderr,
            program_status,
        } = self;
        if let Some(reporter) = program_status {
            reporter.report(&mut **stdout, crate::program_status::State::Working);
        }
        let turn = cancel.child_token();
        let mut rendered_ids: Vec<String> = Vec::new();
        let mut rendered_noop_empty = false;
        let mut error_rendered = false;
        let result = {
            let mut sink = |event: Event| match &event {
                Event::CompactionCompleted { compaction } => {
                    if compaction.noop && compaction.checkpoint_id.is_empty() {
                        if rendered_noop_empty {
                            return;
                        }
                        rendered_noop_empty = true;
                    } else if !compaction.checkpoint_id.is_empty() {
                        if rendered_ids.contains(&compaction.checkpoint_id) {
                            return;
                        }
                        rendered_ids.push(compaction.checkpoint_id.clone());
                    }
                    let _ = write!(stdout, "{}", compaction_line(compaction));
                }
                _ => {
                    if render_event(&mut **stdout, &mut **stderr, &event) {
                        error_rendered = true;
                    }
                }
            };
            controller.compact(focus, &mut sink, &turn).await
        };
        if let Some(reporter) = program_status {
            let canceled = cancel.is_cancelled()
                || result.as_ref().err().is_some_and(AgentError::is_cancelled);
            let state = crate::program_status::State::finished(result.is_ok(), canceled);
            reporter.report(&mut **stdout, state);
        }
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                if error.is_cancelled() && !is_fatal_persistence(&error) {
                    return Ok(());
                }
                if !error_rendered {
                    let _ = writeln!(stderr, "{error}");
                }
                return Err(Error::Command {
                    command: "/compact".to_string(),
                    message: error.to_string(),
                });
            }
        };
        let already_rendered = if !result.checkpoint_id.is_empty() {
            rendered_ids.contains(&result.checkpoint_id)
        } else {
            result.noop && rendered_noop_empty
        };
        if !already_rendered {
            let _ = write!(stdout, "{}", compaction_line(&result));
        }
        Ok(())
    }

    /// Runs one reflection and prints its one-line report.
    async fn reflect(&mut self, focus: &str, cancel: &CancellationToken) -> Result<(), Error> {
        let turn = cancel.child_token();
        let result = self.controller.reflect(focus, &turn).await;
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        match result {
            Ok(report) => {
                let _ = writeln!(self.stdout, "{}", report.summary());
                Ok(())
            }
            Err(crate::reflection::Error::Cancelled) => Ok(()),
            Err(error) => Err(Error::Command {
                command: "/reflect".to_string(),
                message: error.to_string(),
            }),
        }
    }

    fn print_session_id(&mut self) {
        let id = self.controller.info().session_id;
        if !id.is_empty() {
            let _ = writeln!(self.stdout, "Session: {id}");
        }
    }
}

/// Renders one event. Returns true when it wrote an agent error, so the caller
/// does not print the same failure twice.
fn render_event(stdout: &mut dyn Write, stderr: &mut dyn Write, event: &Event) -> bool {
    match event {
        Event::ApprovalChanged { request } => {
            if request
                .decision
                .as_ref()
                .is_some_and(|d| d.transport == "auto-mode")
            {
                let _ = writeln!(
                    stdout,
                    "[AUTO] Approval {} automatically approved; application {:?}",
                    request.id, request.application
                );
            }
            let _ = writeln!(
                stdout,
                "\n[approval] {} ({}) {:?}\n{}",
                request.title, request.id, request.state, request.subject.details
            );
            if let Some(review) = &request.review {
                let _ = writeln!(
                    stdout,
                    "Review: {:?}{}",
                    review.status,
                    review
                        .assessment
                        .as_ref()
                        .map(|assessment| format!(" · {}", assessment.reason))
                        .unwrap_or_default()
                );
            }
            if request.is_human_pending() {
                let _ = writeln!(
                    stdout,
                    "Approve in Otto: /approve {}\nReject: /deny {} [feedback]\nWithdraw: /withdraw {}",
                    request.id, request.id, request.id
                );
            }
        }
        Event::TextDelta { text } => {
            let _ = write!(stdout, "{text}");
        }
        Event::ToolCallStarted {
            tool_name,
            tool_call_id,
            ..
        } => {
            let _ = writeln!(stdout, "\n[tool] {tool_name} ({tool_call_id})");
        }
        Event::ToolCallFinished {
            operation_id,
            tool_name,
            result,
            outcome,
            ..
        } => {
            let label = tool_outcome_label(outcome);
            if outcome.effect_certainty == EffectCertainty::Unknown {
                let _ = writeln!(
                    stdout,
                    "[tool warning] {label} (operation {operation_id}): {}",
                    first_line(&result.content)
                );
            } else {
                let _ = writeln!(
                    stdout,
                    "[tool result] {label} (operation {operation_id}): {}",
                    first_line(&result.content)
                );
            }
            // A bash approval request puts the /approve command, the command,
            // and the justification on their own lines after the first.
            if tool_name == "bash" && result.is_error {
                for line in result.content.lines().filter(|line| {
                    ["Approve in Otto: ", "Command: ", "Justification: "]
                        .iter()
                        .any(|prefix| line.starts_with(prefix))
                }) {
                    let _ = writeln!(stdout, "{line}");
                }
            }
        }
        Event::CompactionCompleted { compaction } => {
            let _ = write!(stdout, "{}", compaction_line(compaction));
        }
        Event::ProviderRetry {
            attempt,
            max_attempts,
            delay,
            reason,
            interrupted_output,
            ..
        } => {
            if *interrupted_output {
                let _ = writeln!(
                    stdout,
                    "\n[{}]",
                    otto_core::wire::transcript::INTERRUPTED_OUTPUT_NOTICE
                );
            }
            let _ = writeln!(
                stderr,
                "[provider retry] attempt {attempt}/{max_attempts} in {}ms: {}",
                delay.as_millis(),
                first_line(reason)
            );
        }
        Event::CompactionWarning { message } | Event::MemoryWarning { message } => {
            let _ = writeln!(stderr, "{message}");
        }
        Event::AgentError { message } => {
            let _ = writeln!(stderr, "{message}");
            return true;
        }
        Event::Notification { text, .. } => {
            let _ = writeln!(stdout, "\n{text}");
        }
        _ => {}
    }
    false
}

fn tool_outcome_label(outcome: &OperationOutcome) -> &'static str {
    match outcome.effect_certainty {
        EffectCertainty::Unknown => "Outcome unknown — check effects before retrying",
        EffectCertainty::NotStarted => "Did not run",
        EffectCertainty::KnownNoEffect => "Known no effect",
        EffectCertainty::Completed => match outcome.disposition {
            OperationDisposition::Succeeded => "Completed",
            OperationDisposition::Error => "Failed with a known result",
            _ => "Completed",
        },
    }
}

fn compaction_line(result: &CompactionResult) -> String {
    if result.noop {
        return "\n[context] no-op\n".to_string();
    }
    match (result.tokens_before, result.estimated_tokens_after) {
        (Some(before), Some(after)) => format!(
            "\n[context] compacted {} \u{2192} {} tokens\n",
            format_token_count(before),
            format_token_count(after)
        ),
        (Some(before), None) => {
            format!(
                "\n[context] compacted {} tokens\n",
                format_token_count(before)
            )
        }
        (None, _) => "\n[context] compacted\n".into(),
    }
}

fn format_token_count(tokens: i64) -> String {
    if tokens < 1000 {
        return tokens.to_string();
    }
    format!("{}k", tokens / 1000)
}

fn display_thinking(thinking: &str) -> &str {
    if thinking.is_empty() {
        "default"
    } else {
        thinking
    }
}

#[derive(Default)]
struct ParsedThinkingArgs {
    thinking: String,
    save: bool,
}

#[derive(Default)]
struct ParsedModelArgs {
    profile: String,
    thinking: String,
    save: bool,
}

fn parse_thinking_args(args: &str) -> Result<ParsedThinkingArgs, String> {
    let mut parsed = ParsedThinkingArgs::default();
    for part in args.split_whitespace() {
        if part == "--save" {
            parsed.save = true;
        } else if parsed.thinking.is_empty() {
            parsed.thinking = part.to_string();
        } else {
            return Err("usage: /thinking [LEVEL] [--save]".to_string());
        }
    }
    Ok(parsed)
}

fn parse_model_args(args: &str) -> Result<ParsedModelArgs, String> {
    let mut parsed = ParsedModelArgs::default();
    let mut parts = args.split_whitespace();
    while let Some(part) = parts.next() {
        match part {
            "--save" => parsed.save = true,
            "--thinking" => {
                let Some(level) = parts.next() else {
                    return Err("--thinking requires a level".to_string());
                };
                parsed.thinking = level.to_string();
            }
            value if parsed.profile.is_empty() => parsed.profile = value.to_string(),
            _ => return Err("usage: /model [profile] [--thinking LEVEL] [--save]".to_string()),
        }
    }
    Ok(parsed)
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

/// Whether the error is the store's fatal-persistence failure.
///
/// ponytail: the kind does not survive `SessionError::Persist(String)`, so this
/// matches the sentinel text the store prefixes onto the message. A typed flag
/// on `SessionError` would be the upgrade, in `otto-core`.
/// A `/sandbox` failure in the shape the REPL reports command failures.
fn sandbox_error(message: String) -> Error {
    Error::Command {
        command: "/sandbox".to_string(),
        message,
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
