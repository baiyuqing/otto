//! The line-oriented frontend.
//!
//! Port of `internal/repl`. The banner, prompt marker, command output and
//! event rendering are byte-identical to the Go REPL's for the same inputs.
//!
//! Not ported in this phase, and answered with a "not yet ported" line on
//! stderr: `/memory`, `/remember`, `/tasks`, `/task`.
//! Sub-agent wake turns and the task drain in `RunOnce` go with them. Go's
//! `/sandbox reload` needs the sandbox reloader, which is also a later phase,
//! so it reports Go's `ErrSandboxReloadUnavailable` text.
//!
//! Input: one blocking reader task feeds a bounded channel, so a parent
//! cancellation is observed while the loop is idle waiting for a line. A line
//! longer than [`MAX_INPUT_BYTES`] ends the loop with an error, as Go's
//! scanner limit does.

use std::io::{BufRead, Read, Write};

use otto_core::agent::{AgentError, CompactionResult, Event};
use tokio_util::sync::CancellationToken;

use super::controller::{Controller, PROFILE_SWITCH_UNAVAILABLE, SANDBOX_RELOAD_UNAVAILABLE};

/// The longest line the REPL accepts, matching Go's `maxInputBytes`.
pub const MAX_INPUT_BYTES: usize = 1 << 20;

const LOGO: &str = "     ____  __  __\n    / __ \\/ /_/ /____\n   / /_/ / __/ __/ __ \\\n   \\____/\\__/\\__/\\____/\n";

const HELP: &str = "/help     show commands\n/exit     exit Otto\n/new      start a new session\n/session  show session details\n/rename <name> rename current session\n/archive  archive current session and start a new one\n/model [profile] show current model, or switch profiles in a fresh session\n/compact [focus] compact context\n/sandbox [reload] show sandbox state, or apply the current [sandbox] configuration\n/login [status] sign in to ChatGPT (or show status)\n/logout   sign out of ChatGPT\n";

/// Commands `internal/repl` has that this phase does not.
const UNPORTED: [&str; 4] = ["memory", "remember", "tasks", "task"];

/// Why the loop stopped. Port of the error values `Run` returns.
#[derive(Debug)]
pub enum Error {
    /// The process context was cancelled. Go returns `ctx.Err()`.
    Cancelled,
    /// The input could not be read, or a line exceeded the limit.
    Input(String),
    /// Port of `commandError`: the command that failed and its message.
    Command { command: String, message: String },
    /// A turn failed. `fatal` marks Go's `session.ErrFatalPersistence`, the
    /// only turn failure that ends the loop.
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

/// Port of `repl.IsCommandError`.
pub fn is_command_error(error: &Error, command: &str) -> bool {
    matches!(error, Error::Command { command: name, .. } if name == command)
}

pub struct Repl<'a> {
    controller: &'a Controller,
    stdout: Box<dyn Write + Send + 'a>,
    stderr: Box<dyn Write + Send + 'a>,
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

    /// The interactive loop. Port of `REPL.Run`.
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
        loop {
            let _ = write!(self.stdout, "> ");
            let _ = self.stdout.flush();
            let line = tokio::select! {
                _ = cancel.cancelled() => return Err(Error::Cancelled),
                line = lines.recv() => line,
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

    /// One prompt with the same rendering as [`Repl::run`], without the
    /// banner or the prompt marker. Port of `REPL.RunOnce`; the sub-agent
    /// task drain is a later phase.
    pub async fn run_once(
        &mut self,
        prompt: &str,
        cancel: &CancellationToken,
    ) -> Result<(), Error> {
        self.prompt(prompt, cancel).await
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
                "new" => {
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
                        "ID: {}\nPath: {}\nProvider: {}\nModel: {}\nSandbox: {}",
                        info.session_id,
                        info.session_path,
                        info.provider,
                        info.model,
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
                "sandbox" => self.sandbox(args)?.then_some(false),
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
                _ if UNPORTED.contains(&name) => {
                    let _ = writeln!(self.stderr, "/{name} is not yet ported");
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

    /// Port of `modelCommand`.
    async fn model(&mut self, args: &str) -> Result<(), Error> {
        let unavailable = || Error::Command {
            command: "/model".to_string(),
            message: PROFILE_SWITCH_UNAVAILABLE.to_string(),
        };
        if !self.controller.dynamic_content() {
            return Err(unavailable());
        }
        if args.is_empty() {
            let info = self.controller.info();
            let _ = writeln!(
                self.stdout,
                "Current: profile {} (provider {}, model {})",
                info.profile, info.provider, info.model
            );
            let profiles = self.controller.profiles();
            if profiles.is_empty() {
                let _ = writeln!(self.stdout, "No profiles configured.");
            } else {
                let _ = writeln!(self.stdout, "Profiles: {}", profiles.join(", "));
            }
            return Ok(());
        }
        self.controller
            .switch_profile(args)
            .await
            .map_err(|message| Error::Command {
                command: "/model".to_string(),
                message,
            })?;
        let saved = self.controller.set_default_profile(args);
        let info = self.controller.info();
        match saved {
            Ok(()) => {
                let _ = writeln!(
                    self.stdout,
                    "Switched to profile {} (provider {}, model {}). Set as default profile.",
                    info.profile, info.provider, info.model
                );
            }
            Err(message) => {
                let _ = writeln!(
                    self.stdout,
                    "Switched to profile {} (provider {}, model {}), but the default profile was not saved: {}",
                    info.profile, info.provider, info.model, message
                );
            }
        }
        if !info.session_id.is_empty() {
            let _ = writeln!(self.stdout, "Session: {}", info.session_id);
        }
        Ok(())
    }

    /// Port of `sandboxCommand`. False means "unknown command".
    fn sandbox(&mut self, args: &str) -> Result<bool, Error> {
        match args {
            "" => {
                let info = self.controller.sandbox_info();
                let _ = writeln!(self.stdout, "Sandbox: {}", info.summary());
                let reason = info.reason_code();
                if !reason.is_empty() {
                    let _ = writeln!(self.stdout, "Sandbox reason: {reason}");
                }
                Ok(true)
            }
            "reload" => Err(Error::Command {
                command: "/sandbox".to_string(),
                message: SANDBOX_RELOAD_UNAVAILABLE.to_string(),
            }),
            _ => {
                let _ = writeln!(self.stderr, "unknown command: /sandbox {args}");
                Ok(true)
            }
        }
    }

    /// Port of `REPL.compact`, including the checkpoint de-duplication that
    /// keeps an event and the returned result from rendering twice.
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

/// Renders one event. Returns true when it wrote an agent error, so the
/// caller does not print the same failure twice. Port of `renderEvent`.
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

/// Port of `compactionLine`.
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

/// Port of `formatTokenCount`.
fn format_token_count(tokens: i64) -> String {
    if tokens < 1000 {
        return tokens.to_string();
    }
    format!("{}k", tokens / 1000)
}

/// Port of `splitCommand`: the name and the trimmed remainder, or `None` when
/// the line is not a command.
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

/// Go's `errors.Is(err, session.ErrFatalPersistence)`.
///
/// ponytail: the kind does not survive `SessionError::Persist(String)`, so
/// this matches the sentinel text the store prefixes onto the message. A
/// typed flag on `SessionError` would be the upgrade, in `otto-core`.
pub(crate) fn is_fatal_persistence(error: &AgentError) -> bool {
    matches!(error, AgentError::Persist { source, .. }
        if source.to_string().starts_with("fatal session persistence failure"))
}

/// Reads lines on a blocking task so the loop can wait on cancellation at the
/// same time. The channel holds one line, which is Go's scan/ack handshake.
fn spawn_reader<R: BufRead + Send + 'static>(
    mut input: R,
) -> tokio::sync::mpsc::Receiver<Result<String, Error>> {
    let (sender, receiver) = tokio::sync::mpsc::channel(1);
    tokio::task::spawn_blocking(move || {
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
    use crate::cli::testutil::{controller, user};
    use otto_core::agent::{CompactionResult, Event};
    use otto_core::session::Session;
    use otto_core::tool::ToolResult;
    use std::io::Cursor;
    use std::sync::{Arc, Mutex};

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
        assert!(stdout.contains("> "), "{stdout}");
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
        assert_eq!(stdout.matches("> ").count(), 4);
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
            "/exit     exit Otto",
            "/new      start a new session",
            "/session  show session details",
            "/rename <name> rename current session",
            "/archive  archive current session and start a new one",
            "/model [profile]",
            "/compact [focus] compact context",
            "/sandbox [reload]",
        ] {
            assert!(
                stdout.contains(expected),
                "{expected} missing from {stdout}"
            );
        }
        assert!(
            stdout.contains(&format!(
                "ID: {}\nPath: {}\nProvider: openai-compatible\nModel: gpt-alpha\nSandbox: {}\n",
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
                .contains("Current: profile alpha (provider openai-compatible, model gpt-alpha)\n"),
            "{stdout}"
        );
        assert!(stdout.contains("Profiles: alpha, beta\n"), "{stdout}");
        assert!(
            stdout.contains(
                "Switched to profile beta (provider openai-compatible, model gpt-beta). Set as default profile.\n"
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

    #[tokio::test]
    async fn sandbox_reload_reports_that_it_is_unavailable() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = controller(workspace.path(), sessions.path()).await;

        let (_, _, result) = session("/sandbox reload\n", &controller).await;

        let error = result.expect_err("reload");
        assert!(is_command_error(&error, "/sandbox"), "{error:?}");
        assert_eq!(error.to_string(), SANDBOX_RELOAD_UNAVAILABLE);
    }

    #[tokio::test]
    async fn commands_from_later_phases_report_that_they_are_not_ported() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = controller(workspace.path(), sessions.path()).await;

        let (_, stderr, result) = session(
            "/memory search x\n/remember note\n/tasks\n/task 1\n/exit\n",
            &controller,
        )
        .await;

        assert!(result.is_ok(), "{result:?}");
        for command in ["/memory", "/remember", "/tasks", "/task"] {
            assert!(
                stderr.contains(&format!("{command} is not yet ported\n")),
                "{command} missing from {stderr}"
            );
        }
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
    fn events_render_the_way_the_go_frontend_renders_them() {
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
    fn compaction_lines_match_the_go_text() {
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
}
