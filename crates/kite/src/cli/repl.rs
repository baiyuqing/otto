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

use kite_core::agent::{AgentError, CompactionResult, Event};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::subagent::tasks::{TaskError, Tasks};

use super::controller::{Controller, PROFILE_SWITCH_UNAVAILABLE};
use super::sandbox_setup::SandboxChange;

/// The longest line the REPL accepts.
pub const MAX_INPUT_BYTES: usize = 1 << 20;

const LOGO: &str = "     ____  __  __\n    / __ \\/ /_/ /____\n   / /_/ / __/ __/ __ \\\n   \\____/\\__/\\__/\\____/\n";

const HELP: &str = "/help     show commands\n/exit     exit Kite\n/new      start a new session\n/clear    start a new session\n/session  show session details\n/rename <name> rename current session\n/archive  archive current session and start a new one\n/model [profile] [--thinking LEVEL] [--save] show current model, or switch profiles\n/thinking [LEVEL] [--save] show or set reasoning effort\n/compact [focus] compact context\n/sandbox [reload] show sandbox state, or apply the current [sandbox] configuration\n/sandbox allow <path> let sandboxed commands read a path\n/sandbox network allow|deny set sandboxed network access\n/approve <id> allow one exact elevated Bash command\n/memory search <query> | /memory forget <id> | /memory review <id> accept|reject\n/remember [--scope user|workspace] [--kind K] [--key K] <text>\n/skills   list available skills\n/skill <name> show a skill\n/tasks    list sub-agent tasks\n/task <id> show a task's steps and result\n/task cancel <id> cancel a queued or running task\n/timers   list this session's timers\n/timers cancel <id> cancel a timer\n/login [status] sign in to ChatGPT (or show status)\n/logout   sign out of ChatGPT\n/mcp      show configured MCP servers and their status\n/mcp login <server> sign in to an MCP server that uses OAuth\n";

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

        let mut lines = spawn_reader(input);
        let mut updates: Option<(Arc<Tasks>, watch::Receiver<u64>)> = None;
        loop {
            let _ = write!(self.stdout, "{}", crate::tui::gutter::USER_MARK);
            let _ = self.stdout.flush();
            // The receiver is kept across iterations rather than re-subscribed:
            // subscribing now would mark a signal raised during the last turn
            // as already seen.
            match self.controller.subagent_tasks() {
                Some(tasks) => {
                    if updates
                        .as_ref()
                        .is_none_or(|(held, _)| !Arc::ptr_eq(held, &tasks))
                    {
                        let receiver = tasks.updates();
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
                tokio::select! {
                    _ = cancel.cancelled() => return Err(Error::Cancelled),
                    open = signal => Err(open),
                    line = lines.recv() => Ok(line),
                }
            };
            let line = match read {
                Ok(line) => line,
                Err(false) => {
                    updates = None;
                    continue;
                }
                Err(true) => match self.wake(cancel).await {
                    Ok(_) | Err(Error::Turn { fatal: false, .. }) => continue,
                    Err(error) => return Err(error),
                },
            };
            let Some(line) = line else { return Ok(()) };
            let line = line?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if trimmed.starts_with('/') {
                if self.command(trimmed, cancel).await? {
                    return Ok(());
                }
                continue;
            }
            match self.prompt(&line, cancel).await {
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
        } = self;
        let wake = controller.prepare_wake().map_err(|message| Error::Turn {
            fatal: false,
            message,
        })?;
        let Some(wake) = wake else { return Ok(false) };
        let _ = writeln!(stdout);
        let turn = cancel.child_token();
        let mut error_rendered = false;
        let result = {
            let mut sink = |event: Event| {
                if render_event(&mut **stdout, &mut **stderr, &event) {
                    error_rendered = true;
                }
            };
            wake.run(&mut sink, &turn).await
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
        } = self;
        let turn = cancel.child_token();
        let mut error_rendered = false;
        let result = {
            let mut sink = |event: Event| {
                if render_event(&mut **stdout, &mut **stderr, &event) {
                    error_rendered = true;
                }
            };
            controller.prompt(line, &mut sink, &turn).await
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
            Ok(()) => Ok(()),
            Err(error) => Err(Error::Turn {
                fatal: is_fatal_persistence(&error),
                message: error.to_string(),
            }),
        }
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
                "model" => {
                    self.model(args).await?;
                    Some(false)
                }
                "thinking" => {
                    self.thinking(args).await?;
                    Some(false)
                }
                "sandbox" => self.sandbox(args).await?.then_some(false),
                "approve" => {
                    if args.is_empty() || args.contains(char::is_whitespace) {
                        break 'dispatch None;
                    }
                    let retry =
                        self.controller
                            .approve_bash(args)
                            .map_err(|message| Error::Command {
                                command: "/approve".to_string(),
                                message,
                            })?;
                    let _ = writeln!(self.stdout, "Approved {args} for one command.");
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
                "skills" => {
                    if !args.is_empty() {
                        break 'dispatch None;
                    }
                    self.skills_command();
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
    /// `allow` and `network` write the `[sandbox]` table and reload it, the
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
            _ => {
                let _ = writeln!(self.stderr, "unknown command: /sandbox {args}");
                Ok(true)
            }
        }
    }

    fn print_sandbox(&mut self, info: crate::cli::info::SandboxInfo) {
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
        } = self;
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
        Event::ToolCallFinished { result, .. } => {
            let _ = writeln!(stdout, "[tool result] {}", first_line(&result.content));
        }
        Event::CompactionCompleted { compaction } => {
            let _ = write!(stdout, "{}", compaction_line(compaction));
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

fn compaction_line(result: &CompactionResult) -> String {
    if result.noop {
        return "\n[context] no-op\n".to_string();
    }
    let before = format_token_count(result.tokens_before);
    if result.estimated_tokens_after > 0 {
        return format!(
            "\n[context] compacted {before} \u{2192} {} tokens\n",
            format_token_count(result.estimated_tokens_after)
        );
    }
    format!("\n[context] compacted {before} tokens\n")
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
/// on `SessionError` would be the upgrade, in `kite-core`.
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
mod tests {
    use std::path::Path;

    use super::*;
    use crate::cli::controller::SANDBOX_RELOAD_UNAVAILABLE;
    use crate::cli::info::SandboxNetwork;
    use crate::cli::runtime_builder::Runner;
    use crate::cli::testutil::{self, controller, user};
    use crate::subagent::tasks::Tasks;
    use kite_core::agent::inbox::Notification;
    use kite_core::agent::{CompactionResult, Event};
    use kite_core::model::{Block, BlockType, FinishReason, Message, Role};
    use kite_core::provider::{
        Provider, ProviderError, Request as ProviderRequest, Response as ProviderResponse,
        StreamEvent, StreamSink,
    };
    use kite_core::session::Session;
    use kite_core::tool::ToolResult;
    use std::io::{Cursor, Read};
    use std::sync::{Arc, Condvar, Mutex};

    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl Buffer {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().expect("buffer").clone()).expect("utf-8")
        }
    }

    impl Write for Buffer {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("buffer").extend_from_slice(data);
            Ok(data.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    async fn session(input: &str, controller: &Controller) -> (String, String, Result<(), Error>) {
        let stdout = Buffer::default();
        let stderr = Buffer::default();
        let mut repl = Repl::new(
            controller,
            Box::new(stdout.clone()),
            Box::new(stderr.clone()),
        );
        let result = repl
            .run(
                Cursor::new(input.as_bytes().to_vec()),
                &CancellationToken::new(),
            )
            .await;
        (stdout.text(), stderr.text(), result)
    }

    struct ExitThenBlock {
        first: bool,
        release: Arc<(Mutex<bool>, Condvar)>,
    }

    impl ExitThenBlock {
        fn new(release: Arc<(Mutex<bool>, Condvar)>) -> Self {
            Self {
                first: true,
                release,
            }
        }
    }

    impl Read for ExitThenBlock {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            if self.first {
                self.first = false;
                let line = b"/exit\n";
                buffer[..line.len()].copy_from_slice(line);
                return Ok(line.len());
            }
            let (lock, signal) = &*self.release;
            let mut released = lock.lock().expect("release lock");
            while !*released {
                released = signal.wait(released).expect("release lock");
            }
            Ok(0)
        }
    }

    #[test]
    fn exit_does_not_wait_for_the_next_blocking_stdin_read_on_runtime_drop() {
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let thread_release = Arc::clone(&release);
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("runtime");
            runtime.block_on(async move {
                let workspace = tempfile::tempdir().expect("workspace");
                let sessions = tempfile::tempdir().expect("sessions");
                let controller = controller(workspace.path(), sessions.path()).await;
                let stdout = Buffer::default();
                let stderr = Buffer::default();
                let mut repl = Repl::new(&controller, Box::new(stdout), Box::new(stderr));
                repl.run(
                    std::io::BufReader::new(ExitThenBlock::new(thread_release)),
                    &CancellationToken::new(),
                )
                .await
                .expect("/exit returns");
            });
            drop(runtime);
            let _ = done_tx.send(());
        });

        let finished = done_rx.recv_timeout(std::time::Duration::from_millis(500));
        {
            let (lock, signal) = &*release;
            *lock.lock().expect("release lock") = true;
            signal.notify_all();
        }
        worker.join().expect("runtime thread");
        assert!(
            finished.is_ok(),
            "runtime shutdown waited for a REPL stdin read that was still blocked"
        );
    }

    #[tokio::test]
    async fn the_banner_reports_the_session_and_the_sandbox() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = controller(workspace.path(), sessions.path()).await;

        let (stdout, stderr, result) = session("/exit\n", &controller).await;

        assert!(result.is_ok(), "{result:?}");
        assert!(stdout.starts_with("     ____  __  __"), "{stdout}");
        assert!(
            stdout.contains(&format!("Session: {}\n", controller.info().session_id)),
            "{stdout}"
        );
        assert!(
            stdout.contains(&format!(
                "Sandbox: {}\n",
                controller.sandbox_info().summary()
            )),
            "{stdout}"
        );
        assert!(stdout.contains(crate::tui::gutter::USER_MARK), "{stdout}");
        assert_eq!(stderr, "");
    }

    #[tokio::test]
    async fn blank_lines_are_skipped_and_unknown_commands_are_reported() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = controller(workspace.path(), sessions.path()).await;

        let (stdout, stderr, result) = session("\n   \n/unknown\n/exit\n", &controller).await;

        assert!(result.is_ok(), "{result:?}");
        assert_eq!(stderr, "unknown command: /unknown\n");
        assert_eq!(stdout.matches(crate::tui::gutter::USER_MARK).count(), 4);
    }

    #[tokio::test]
    async fn approve_fails_closed_when_temporary_elevation_is_unavailable() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = controller(workspace.path(), sessions.path()).await;

        let (_, _, result) = session("/approve approval-1\n", &controller).await;

        let error = result.expect_err("approval must fail");
        assert!(is_command_error(&error, "/approve"), "{error:?}");
        assert_eq!(error.to_string(), "temporary elevation is unavailable");
    }

    #[tokio::test]
    async fn skill_commands_list_and_show_discovered_skills() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        testutil::write_skill(
            workspace.path(),
            "rust-helper",
            "Rust guidance",
            "Use small focused Rust changes.",
        );
        let controller = controller(workspace.path(), sessions.path()).await;

        let (stdout, stderr, result) = session(
            "/skills\n/skill rust-helper\n/skill missing\n/skill\n/exit\n",
            &controller,
        )
        .await;

        assert!(result.is_ok(), "{result:?}");
        assert!(
            stdout.contains("Available skills:\n- rust-helper: Rust guidance"),
            "{stdout}"
        );
        assert!(stdout.contains("Skill: rust-helper"), "{stdout}");
        assert!(stdout.contains("Description: Rust guidance"), "{stdout}");
        assert!(
            stdout.contains("Use small focused Rust changes."),
            "{stdout}"
        );
        assert!(stderr.contains("unknown skill: missing"), "{stderr}");
        assert!(
            stderr.contains(crate::cli::repl_commands::SKILL_USAGE),
            "{stderr}"
        );
    }

    #[tokio::test]
    async fn help_and_session_describe_the_ported_commands_and_the_session() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = controller(workspace.path(), sessions.path()).await;
        controller
            .current_session()
            .append(user("hello"))
            .await
            .expect("append");
        let info = controller.info();

        let (stdout, stderr, result) = session("/help\n/session\n/exit\n", &controller).await;

        assert!(result.is_ok(), "{result:?}");
        assert_eq!(stderr, "");
        for expected in [
            "/help     show commands",
            "/exit     exit Kite",
            "/new      start a new session",
            "/session  show session details",
            "/rename <name> rename current session",
            "/archive  archive current session and start a new one",
            "/model [profile]",
            "/compact [focus] compact context",
            "/sandbox [reload]",
            "/approve <id>",
            "/skills   list available skills",
            "/skill <name> show a skill",
        ] {
            assert!(
                stdout.contains(expected),
                "{expected} missing from {stdout}"
            );
        }
        assert!(
            stdout.contains(&format!(
                "ID: {}\nPath: {}\nProvider: openai-compatible\nModel: gpt-alpha\nThinking: default\nSandbox: {}\n",
                info.session_id,
                info.session_path,
                info.sandbox.summary()
            )),
            "{stdout}"
        );
    }

    #[tokio::test]
    async fn renaming_prints_the_new_name_and_an_empty_name_is_unknown() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = controller(workspace.path(), sessions.path()).await;

        let (stdout, stderr, result) =
            session("/rename  release notes \n/rename   \n/exit\n", &controller).await;

        assert!(result.is_ok(), "{result:?}");
        assert!(
            stdout.contains("Renamed session: release notes\n"),
            "{stdout}"
        );
        assert_eq!(stderr, "unknown command: /rename\n");
        assert_eq!(controller.info().session_name, "release notes");
    }

    #[tokio::test]
    async fn a_new_session_reports_a_different_id() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = controller(workspace.path(), sessions.path()).await;
        let before = controller.info().session_id;

        let (stdout, _, result) = session("/new\n/exit\n", &controller).await;

        assert!(result.is_ok(), "{result:?}");
        let after = controller.info().session_id;
        assert_ne!(after, before);
        assert!(stdout.contains(&format!("Session: {after}\n")), "{stdout}");
    }

    #[tokio::test]
    async fn clear_starts_a_fresh_session_like_new() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = controller(workspace.path(), sessions.path()).await;
        let before = controller.info().session_id;

        let (stdout, stderr, result) = session("/clear\n/exit\n", &controller).await;

        assert!(result.is_ok(), "{result:?}");
        assert_eq!(stderr, "");
        let after = controller.info().session_id;
        assert_ne!(after, before);
        assert!(stdout.contains(&format!("Session: {after}\n")), "{stdout}");
    }

    #[tokio::test]
    async fn archiving_prints_the_path_and_the_replacement_session() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = controller(workspace.path(), sessions.path()).await;
        controller
            .current_session()
            .append(user("hello"))
            .await
            .expect("append");
        let before = controller.info();

        let (stdout, stderr, result) = session("/archive\n/exit\n", &controller).await;

        assert!(result.is_ok(), "{result:?}");
        assert_eq!(stderr, "");
        assert!(stdout.contains("Archived: "), "{stdout}");
        assert!(!Path::new(&before.session_path).exists());
        assert!(
            stdout.contains(&format!("Session: {}\n", controller.info().session_id)),
            "{stdout}"
        );
    }

    #[tokio::test]
    async fn archiving_a_session_without_a_file_is_a_command_error() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = controller(workspace.path(), sessions.path()).await;

        let (_, _, result) = session("/archive\n", &controller).await;

        let error = result.expect_err("archive without a file");
        assert!(is_command_error(&error, "/archive"), "{error:?}");
        assert!(error.to_string().contains("persistence"), "{error}");
    }

    #[tokio::test]
    async fn model_lists_profiles_and_switches_to_one() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = controller(workspace.path(), sessions.path()).await;
        std::fs::write(
            controller.config_path(),
            "default_profile = \"alpha\"\n\n[profiles.alpha]\n\n[profiles.beta]\n",
        )
        .expect("write config");

        let (stdout, stderr, result) = session("/model\n/model beta\n/exit\n", &controller).await;

        assert!(result.is_ok(), "{result:?}");
        assert_eq!(stderr, "");
        assert!(
            stdout
                .contains("Current: profile alpha (provider openai-compatible, model gpt-alpha, thinking default)\n"),
            "{stdout}"
        );
        assert!(stdout.contains("Profiles: alpha (openai-compatible/gpt-alpha, thinking default), beta (openai-compatible/gpt-beta, thinking default)\n"), "{stdout}");
        assert!(
            stdout.contains(
                "Switched to profile beta (provider openai-compatible, model gpt-beta, thinking default). Set as default profile.\n"
            ),
            "{stdout}"
        );
        assert_eq!(controller.info().model, "gpt-beta");
    }

    #[tokio::test]
    async fn an_unknown_profile_is_a_command_error() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = controller(workspace.path(), sessions.path()).await;

        let (_, _, result) = session("/model missing\n", &controller).await;

        let error = result.expect_err("unknown profile");
        assert!(is_command_error(&error, "/model"), "{error:?}");
    }

    #[tokio::test]
    async fn sandbox_prints_the_state_and_rejects_an_unknown_subcommand() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = controller(workspace.path(), sessions.path()).await;

        let (stdout, stderr, result) =
            session("/sandbox\n/sandbox bogus\n/exit\n", &controller).await;

        assert!(result.is_ok(), "{result:?}");
        assert!(
            stdout.contains(&format!(
                "Sandbox: {}\n",
                controller.sandbox_info().summary()
            )),
            "{stdout}"
        );
        assert_eq!(stderr, "unknown command: /sandbox bogus\n");
    }

    async fn reloading_controller(
        workspace: &Path,
        sessions: &Path,
        failure: Option<&str>,
    ) -> (Controller, Arc<Mutex<usize>>) {
        let (control, calls) = testutil::FakeSandbox::new(
            testutil::seatbelt_info(SandboxNetwork::Allowed),
            testutil::seatbelt_info(SandboxNetwork::Denied),
            failure,
        );
        let controller = controller(workspace, sessions)
            .await
            .with_sandbox_control(control);
        (controller, calls)
    }

    #[tokio::test]
    async fn sandbox_shows_the_current_state_without_reloading() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let (controller, calls) =
            reloading_controller(workspace.path(), sessions.path(), None).await;

        let (stdout, stderr, result) = session("/sandbox\n/exit\n", &controller).await;

        assert!(result.is_ok(), "{result:?}");
        assert!(
            stdout.contains("Sandbox: seatbelt · workspace-write · network allowed\n"),
            "{stdout}"
        );
        assert_eq!(stderr, "");
        assert_eq!(*calls.lock().expect("calls"), 0);
    }

    #[tokio::test]
    async fn sandbox_reload_reports_the_new_state() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let (controller, calls) =
            reloading_controller(workspace.path(), sessions.path(), None).await;

        let (stdout, _, result) = session("/sandbox reload\n/exit\n", &controller).await;

        assert!(result.is_ok(), "{result:?}");
        assert_eq!(*calls.lock().expect("calls"), 1);
        assert!(stdout.contains("network denied"), "{stdout}");
    }

    #[tokio::test]
    async fn sandbox_reload_reports_a_failure() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let (controller, _) = reloading_controller(
            workspace.path(),
            sessions.path(),
            Some("sandbox reload failed: self-test-failed"),
        )
        .await;

        let (_, _, result) = session("/sandbox reload\n", &controller).await;

        let error = result.expect_err("reload");
        assert!(is_command_error(&error, "/sandbox"), "{error:?}");
        assert_eq!(error.to_string(), "sandbox reload failed: self-test-failed");
    }

    /// The CLI builds no control when bash never came up.
    #[tokio::test]
    async fn sandbox_reload_without_a_control_is_reported() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = controller(workspace.path(), sessions.path()).await;

        let (_, _, result) = session("/sandbox reload\n", &controller).await;

        let error = result.expect_err("reload");
        assert!(is_command_error(&error, "/sandbox"), "{error:?}");
        assert_eq!(error.to_string(), SANDBOX_RELOAD_UNAVAILABLE);
    }

    #[tokio::test]
    async fn sandbox_allow_writes_the_read_path_and_reloads() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let granted = workspace.path().join("cache");
        std::fs::create_dir(&granted).expect("cache");
        let (controller, calls) =
            reloading_controller(workspace.path(), sessions.path(), None).await;

        let (stdout, stderr, result) = session(
            &format!("/sandbox allow {}\n/exit\n", granted.display()),
            &controller,
        )
        .await;

        assert!(result.is_ok(), "{result:?}");
        assert_eq!(stderr, "");
        assert_eq!(*calls.lock().expect("calls"), 1);
        assert!(stdout.contains("network denied"), "{stdout}");
        let written =
            std::fs::read_to_string(workspace.path().join("config.toml")).expect("config");
        assert!(written.contains("read_paths"), "{written}");
        assert!(written.contains("cache"), "{written}");
    }

    #[tokio::test]
    async fn sandbox_allow_rejects_a_path_that_does_not_exist() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let (controller, calls) =
            reloading_controller(workspace.path(), sessions.path(), None).await;

        let (_, _, result) = session("/sandbox allow ~/missing\n", &controller).await;

        let error = result.expect_err("allow");
        assert!(is_command_error(&error, "/sandbox"), "{error:?}");
        assert_eq!(error.to_string(), "no such path: ~/missing");
        assert_eq!(*calls.lock().expect("calls"), 0);
        assert!(!workspace.path().join("config.toml").exists());
    }

    #[tokio::test]
    async fn sandbox_network_sets_the_mode_and_reports_an_unknown_one() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let (controller, calls) =
            reloading_controller(workspace.path(), sessions.path(), None).await;

        let (stdout, stderr, result) = session(
            "/sandbox network deny\n/sandbox network sometimes\n/exit\n",
            &controller,
        )
        .await;

        assert!(result.is_ok(), "{result:?}");
        assert_eq!(*calls.lock().expect("calls"), 1);
        assert!(stdout.contains("network denied"), "{stdout}");
        assert_eq!(stderr, "unknown command: /sandbox network sometimes\n");
        let written =
            std::fs::read_to_string(workspace.path().join("config.toml")).expect("config");
        assert!(written.contains("network = 'deny'"), "{written}");
    }

    #[tokio::test]
    async fn end_of_input_ends_the_loop() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = controller(workspace.path(), sessions.path()).await;

        let (_, _, result) = session("", &controller).await;

        assert!(result.is_ok(), "{result:?}");
    }

    #[tokio::test]
    async fn a_line_over_one_mebibyte_ends_the_loop_with_an_error() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = controller(workspace.path(), sessions.path()).await;
        let line = "x".repeat(MAX_INPUT_BYTES + 1);

        let (_, _, result) = session(&format!("{line}\n"), &controller).await;

        let error = result.expect_err("oversized line");
        assert!(error.to_string().contains("input line too long"), "{error}");
    }

    #[test]
    fn every_event_renders_to_the_expected_line() {
        let mut stdout = Buffer::default();
        let mut stderr = Buffer::default();
        let events = [
            Event::TextDelta {
                text: "done".to_string(),
            },
            Event::ToolCallStarted {
                tool_name: "read".to_string(),
                tool_call_id: "call-1".to_string(),
                arguments: String::new(),
            },
            Event::ToolCallFinished {
                tool_name: "read".to_string(),
                tool_call_id: "call-1".to_string(),
                result: ToolResult {
                    content: "read README.md\nfull output must not render".to_string(),
                    ..ToolResult::default()
                },
            },
            Event::Notification {
                task_id: "t-1".to_string(),
                text: "[task-notification] done".to_string(),
                usage: Default::default(),
                present: false,
            },
        ];
        let mut rendered_error = false;
        for event in events {
            rendered_error |= render_event(&mut stdout, &mut stderr, &event);
        }
        assert!(!rendered_error);
        assert_eq!(
            stdout.text(),
            "done\n[tool] read (call-1)\n[tool result] read README.md\n\n[task-notification] done\n"
        );
        assert_eq!(stderr.text(), "");

        let mut stdout = Buffer::default();
        let mut stderr = Buffer::default();
        assert!(render_event(
            &mut stdout,
            &mut stderr,
            &Event::AgentError {
                message: "provider unavailable".to_string(),
            }
        ));
        assert_eq!(stderr.text(), "provider unavailable\n");
        assert_eq!(stdout.text(), "");
    }

    #[test]
    fn compaction_lines_match_the_expected_text() {
        assert_eq!(
            compaction_line(&CompactionResult {
                noop: true,
                ..CompactionResult::default()
            }),
            "\n[context] no-op\n"
        );
        assert_eq!(
            compaction_line(&CompactionResult {
                tokens_before: 12_000,
                estimated_tokens_after: 800,
                ..CompactionResult::default()
            }),
            "\n[context] compacted 12k → 800 tokens\n"
        );
        assert_eq!(
            compaction_line(&CompactionResult {
                tokens_before: 999,
                ..CompactionResult::default()
            }),
            "\n[context] compacted 999 tokens\n"
        );
    }

    #[test]
    fn commands_split_on_the_first_space() {
        assert_eq!(split_command("/model beta"), Some(("model", "beta")));
        assert_eq!(split_command("/model\tbeta  "), Some(("model", "beta")));
        assert_eq!(split_command("/exit"), Some(("exit", "")));
        assert_eq!(split_command("/"), None);
        assert_eq!(split_command("model"), None);
    }
    // ---- sub-agent wake turns ----

    /// One provider call per turn. The `cli` tests have no backend seam, so the
    /// script sits one layer down, at the provider, the way `server`'s tests
    /// script theirs. `reply` gets the 1-based call index.
    struct ScriptedProvider {
        reply: Box<dyn Fn(usize) -> Result<String, String> + Send + Sync>,
        /// The role of each call's last request message: `User` for a prompt
        /// turn, `Context` for a wake turn's delivered notification.
        roles: Mutex<Vec<Role>>,
        calls: tokio::sync::watch::Sender<usize>,
    }

    impl ScriptedProvider {
        fn new(
            reply: impl Fn(usize) -> Result<String, String> + Send + Sync + 'static,
        ) -> Arc<Self> {
            Arc::new(Self {
                reply: Box::new(reply),
                roles: Mutex::new(Vec::new()),
                calls: tokio::sync::watch::channel(0).0,
            })
        }

        fn roles(&self) -> Vec<Role> {
            self.roles.lock().expect("roles").clone()
        }

        fn calls(&self) -> usize {
            *self.calls.borrow()
        }

        /// Resolves once `count` calls have started.
        async fn wait_calls(&self, count: usize) {
            let mut receiver = self.calls.subscribe();
            receiver
                .wait_for(|seen| *seen >= count)
                .await
                .expect("sender");
        }
    }

    #[async_trait::async_trait]
    impl Provider for ScriptedProvider {
        async fn complete(
            &self,
            request: &ProviderRequest,
            emit: StreamSink<'_>,
            _cancel: &CancellationToken,
        ) -> Result<ProviderResponse, ProviderError> {
            let call = {
                let mut roles = self.roles.lock().expect("roles");
                roles.push(
                    request
                        .messages
                        .last()
                        .map(|message| message.role.clone())
                        .unwrap_or(Role::User),
                );
                roles.len()
            };
            self.calls.send_modify(|seen| *seen = call);
            let text = (self.reply)(call).map_err(ProviderError::Other)?;
            emit(StreamEvent::TextDelta { text: text.clone() });
            Ok(ProviderResponse {
                message: Message {
                    role: Role::Assistant,
                    finish_reason: Some(FinishReason::Stop),
                    blocks: vec![Block {
                        block_type: BlockType::Text,
                        text,
                        ..Block::default()
                    }],
                    ..Message::default()
                },
            })
        }
    }

    /// Stdin that stays open until the test drops `sender`.
    struct Pipe(std::sync::mpsc::Receiver<()>);

    impl Read for Pipe {
        fn read(&mut self, _buffer: &mut [u8]) -> std::io::Result<usize> {
            // The sender is never used to send: dropping it reports EOF.
            let _ = self.0.recv();
            Ok(0)
        }
    }

    fn scripted_controller(
        workspace_root: &Path,
        session_root: &Path,
        provider: Arc<ScriptedProvider>,
        tasks: Arc<Tasks>,
    ) -> Controller {
        let builder = crate::cli::testutil::builder(workspace_root, session_root);
        let runtime = crate::cli::testutil::initial_runtime(&builder);
        let session = builder.create_session(&runtime).expect("session");
        let info = builder.runtime_info(&runtime);
        let runner = Runner::scripted(
            session.clone(),
            provider as Arc<dyn Provider + Send + Sync>,
            tasks,
        );
        Controller::new(builder, true, session, runner, info)
    }

    fn repl_buffers<'a>(controller: &'a Controller) -> (Repl<'a>, Buffer, Buffer) {
        let stdout = Buffer::default();
        let stderr = Buffer::default();
        let repl = Repl::new(
            controller,
            Box::new(stdout.clone()),
            Box::new(stderr.clone()),
        );
        (repl, stdout, stderr)
    }

    #[tokio::test]
    async fn a_notification_renders_during_a_one_shot_turn() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let tasks = Arc::new(Tasks::new());
        tasks.notifications().push(Notification {
            task_id: "t1".to_string(),
            text: "[task-notification] task t1 (explorer) succeeded · 1s · 1 tool call\nall good"
                .to_string(),
            ..Notification::default()
        });
        let provider = ScriptedProvider::new(|_| Ok("ok".to_string()));
        let controller = scripted_controller(
            workspace.path(),
            sessions.path(),
            Arc::clone(&provider),
            Arc::clone(&tasks),
        );
        let (mut repl, stdout, _stderr) = repl_buffers(&controller);

        repl.run_once("go", &CancellationToken::new())
            .await
            .expect("run once");

        assert!(
            stdout.text().contains(
                "\n[task-notification] task t1 (explorer) succeeded · 1s · 1 tool call\nall good\n"
            ),
            "{}",
            stdout.text()
        );
    }

    #[tokio::test]
    async fn the_loop_wakes_only_when_a_notification_is_pending() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let tasks = Arc::new(Tasks::new());
        let provider = ScriptedProvider::new(|_| Ok("woke up".to_string()));
        let controller = scripted_controller(
            workspace.path(),
            sessions.path(),
            Arc::clone(&provider),
            Arc::clone(&tasks),
        );
        let (mut repl, stdout, _stderr) = repl_buffers(&controller);
        let (sender, receiver) = std::sync::mpsc::channel::<()>();
        let cancel = CancellationToken::new();

        let driver = async {
            // A registry signal with nothing pending must not wake a turn.
            tasks
                .add(crate::subagent::tasks::Task::default(), None, None)
                .expect("add");
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            assert_eq!(
                provider.calls(),
                0,
                "a wake turn ran before any notification was pending"
            );
            tasks.notifications().push(Notification {
                task_id: "t1".to_string(),
                text: "[task-notification] task t1 succeeded".to_string(),
                ..Notification::default()
            });
            tokio::time::timeout(std::time::Duration::from_secs(2), provider.wait_calls(1))
                .await
                .expect("the pending notification did not trigger a wake turn");
            drop(sender);
        };
        let (result, ()) = tokio::join!(
            repl.run(std::io::BufReader::new(Pipe(receiver)), &cancel),
            driver
        );

        result.expect("run");
        assert_eq!(
            provider.roles(),
            vec![Role::Context],
            "want exactly one wake turn, whose last request message is the notification"
        );
        assert!(stdout.text().contains("\nwoke up"), "{}", stdout.text());
    }

    #[tokio::test]
    async fn a_one_shot_run_waits_for_a_running_task_and_then_wakes() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let tasks = Arc::new(Tasks::new());
        let added = tasks
            .add(crate::subagent::tasks::Task::default(), None, None)
            .expect("add");
        tasks.mark_running(&added.id, chrono::Utc::now());
        let child = Arc::clone(&tasks);
        let id = added.id.clone();
        let provider = ScriptedProvider::new(move |call| {
            if call > 1 {
                return Ok("reported".to_string());
            }
            let child = Arc::clone(&child);
            let id = id.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                // Pushed before the final update, as the sub-agent runner
                // does: the wait unblocks on the update and the drain must
                // then find the notification already pending.
                child.notifications().push(Notification {
                    task_id: id.clone(),
                    text: format!("[task-notification] task {id} succeeded\nchild done"),
                    ..Notification::default()
                });
                child.finish(
                    &id,
                    crate::subagent::tasks::TaskStatus::Succeeded,
                    chrono::Utc::now(),
                    "child done",
                    "",
                );
            });
            Ok("started".to_string())
        });
        let controller = scripted_controller(
            workspace.path(),
            sessions.path(),
            Arc::clone(&provider),
            Arc::clone(&tasks),
        );
        let (mut repl, stdout, _stderr) = repl_buffers(&controller);

        repl.run_once("go", &CancellationToken::new())
            .await
            .expect("run once");

        assert_eq!(
            provider.roles(),
            vec![Role::User, Role::Context],
            "want the initial turn and one wake turn"
        );
        assert!(stdout.text().contains("reported"), "{}", stdout.text());
    }

    #[tokio::test]
    async fn a_one_shot_run_returns_the_wake_turns_error() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let tasks = Arc::new(Tasks::new());
        let pushed = Arc::clone(&tasks);
        let provider = ScriptedProvider::new(move |call| {
            if call > 1 {
                return Err("wake provider failed".to_string());
            }
            // Pushed inside the first turn, after the agent drained the
            // inbox, so the notification is still pending when it returns.
            pushed.notifications().push(Notification {
                task_id: "t1".to_string(),
                kind: Some(kite_core::agent::inbox::NotificationKind::TaskReport),
                text: "progress".to_string(),
                ..Notification::default()
            });
            Ok("ok".to_string())
        });
        let controller = scripted_controller(
            workspace.path(),
            sessions.path(),
            Arc::clone(&provider),
            Arc::clone(&tasks),
        );
        let (mut repl, _stdout, _stderr) = repl_buffers(&controller);

        let error = repl
            .run_once("inspect", &CancellationToken::new())
            .await
            .expect_err("the wake turn's error");

        assert!(
            error.to_string().contains("wake provider failed"),
            "{error}"
        );
    }
}
