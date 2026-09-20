//! The REPL's memory and sub-agent commands.
//!
//! Port of `internal/repl/memory.go` and `internal/repl/tasks.go`, plus the
//! `internal/app` helpers they call (`MemoryManagerAndScopes`,
//! `ParseRememberArgument`, `RenderMemorySearchResult`) and the
//! `Controller` memory accessors in `internal/app/controller.go`.
//!
//! Divergence from Go: `/memory review` reviews extraction candidates, and
//! automatic memory extraction is not ported, so the subcommand falls through
//! to the usage line instead of reaching a reviewer. The usage text still
//! names it, because it is Go's and the frontend output is pinned byte for
//! byte.
//!
//! [`repl_memory_command`] and [`repl_remember_command`] are free functions,
//! not [`Repl`] methods, so `tui::app`'s `/memory`/`/remember` dispatch can
//! call them directly against its own captured-output buffers; [`Repl`]'s
//! own `memory_command`/`remember_command` are thin wrappers over the same
//! two functions.

use std::fmt::Write as _;
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};

use chrono::Utc;
use otto_core::config::{McpAuth, McpTransport};
use tokio_util::sync::CancellationToken;

use crate::mcp::{Era, ServerState};
use crate::memory::{
    ForgetRequest, RecordRef, RememberRequest, Scope, SearchRequest, SearchResult, Service,
};
use crate::skill;
use crate::subagent::format::{task_line, task_steps};
use crate::subagent::tasks::Tasks;
use crate::tool::remind::{NO_TIMERS, timer_line};

use super::controller::Controller;
use super::login::{SharedWriter, browser_opener};
use super::repl::{Error, Repl};

/// Go's `app.MemorySearchLimit` and `app.MemorySearchTokenBudget`.
const SEARCH_LIMIT: usize = 20;
const SEARCH_TOKEN_BUDGET: usize = 4000;
/// Go's `app.MemoryDefaultKind`.
const DEFAULT_KIND: &str = "note";
/// Go's `app.MemoryUsage` and `app.RememberUsage`. `pub(crate)` so the TUI's
/// `tui::app` dispatch can assert on the exact usage text it reuses via
/// [`repl_memory_command`]/[`repl_remember_command`] instead of duplicating
/// the literal.
pub(crate) const MEMORY_USAGE: &str =
    "usage: /memory search <query> | /memory forget <id> | /memory review <id> accept|reject";
pub(crate) const REMEMBER_USAGE: &str =
    "usage: /remember [--scope user|workspace] [--kind K] [--key K] <text>";
/// Go's `app.ErrMemoryUnavailable`.
pub(crate) const MEMORY_UNAVAILABLE: &str = "memory is not available";
/// Go's `repl.tasksCommand` message when no runner carries a registry.
const SUBAGENTS_UNAVAILABLE: &str = "sub-agents are not available";
/// The `/task` argument forms, as the README and the user manual document
/// them. [`super::super::tui`] prints the same line for the same input.
pub(crate) const TASK_USAGE: &str = "usage: /task <id|name> | /task cancel <id|name>";
/// The `/timers` argument forms. Both frontends print it for anything else.
pub(crate) const TIMERS_USAGE: &str = "usage: /timers [cancel <id>]";
/// What `/timers` reports when the runner registers no timer tools, in the
/// shape [`SUBAGENTS_UNAVAILABLE`] uses for `/tasks`.
pub(crate) const TIMERS_UNAVAILABLE: &str = "timers are not available";
pub(crate) const SKILL_USAGE: &str = "usage: /skill <name>";
pub(crate) const MCP_USAGE: &str = "usage: /mcp | /mcp login <server>";

impl Controller {
    /// Port of `app.MemoryManagerAndScopes`: the bound service and its two
    /// scopes, or `None` when memory is unusable or the redaction boundary is
    /// closed.
    pub(crate) fn memory_manager(&self) -> Option<(Arc<Service>, Scope, Scope)> {
        let wiring = self.memory_wiring();
        match self.dynamic_content() && wiring.usable {
            true => Some((
                Arc::clone(&wiring.service),
                wiring.user_scope.clone(),
                wiring.workspace_scope.clone(),
            )),
            false => None,
        }
    }

    /// Port of Go's `taskOwner`: the active runner's task registry.
    ///
    /// The REPL reads the concrete registry rather than
    /// [`crate::app::Controller::tasks`], because that view carries the
    /// server's wire record: it omits `prompt`, which
    /// [`crate::subagent::format::task_label`] falls back to when a task has
    /// no description, and `wait`/`updates`, which the wake path needs.
    pub(crate) fn subagent_tasks(&self) -> Option<Arc<Tasks>> {
        self.current_runner()?.tasks.clone()
    }
}

/// `/timers` and `/timers cancel <id>` for both frontends: the `Ok` text is
/// the listing or the confirmation, the `Err` text a usage line or a failed
/// cancel. A free function, not a [`Repl`] method, so `tui::app` renders the
/// same text from its own buffers.
pub(crate) fn timers_report(controller: &Controller, args: &str) -> Result<String, String> {
    let Some(reminders) = controller.reminders() else {
        return Err(TIMERS_UNAVAILABLE.to_string());
    };
    match args.split_whitespace().collect::<Vec<_>>()[..] {
        [] => {
            let items = reminders.list();
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
        ["cancel", id] => reminders
            .cancel(id)
            .map(|item| format!("canceled {}: {}", item.id, item.message)),
        _ => Err(TIMERS_USAGE.to_string()),
    }
}

/// Port of `app.SplitFirstToken`.
fn split_first_token(value: &str) -> (&str, &str) {
    let value = value.trim();
    match value.find(char::is_whitespace) {
        Some(index) => (&value[..index], value[index..].trim()),
        None => (value, ""),
    }
}

/// Port of `app.ParseRememberArgument`: the `--scope`/`--kind`/`--key` flags
/// and the trailing free text.
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

/// Port of `app.RenderMemorySearchResult`.
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

pub(crate) fn skills_report(controller: &Controller) -> String {
    let catalog = controller.skills();
    if catalog.is_empty() {
        return "No skills found.".to_string();
    }
    let mut out = "Available skills:".to_string();
    for skill in catalog.skills() {
        let _ = write!(out, "\n- {}: {}", skill.name, skill.description);
    }
    out
}

pub(crate) fn skill_report(controller: &Controller, args: &str) -> String {
    let fields: Vec<&str> = args.split_whitespace().collect();
    if fields.len() != 1 {
        return SKILL_USAGE.to_string();
    }
    let name = fields[0];
    let catalog = controller.skills();
    let Some(found) = catalog.lookup(name) else {
        return format!("unknown skill: {name}");
    };
    match skill::load(found) {
        Ok(body) => format!(
            "Skill: {}\nLocation: {}\nDescription: {}\n\n{}",
            found.name,
            found.directory.display(),
            found.description,
            body.trim_end()
        ),
        Err(error) => format!("skill {name}: {error}"),
    }
}

/// Port of `/mcp`: one line per configured server, in configuration order.
pub(crate) fn mcp_report(controller: &Controller) -> String {
    format_mcp_report(&controller.mcp())
}

/// The formatting `mcp_report` applies to `Controller::mcp`'s status rows.
/// A free function so the per-state line format has a test that does not
/// need a runner with a real MCP server behind it.
fn format_mcp_report(servers: &[crate::mcp::ServerStatus]) -> String {
    if servers.is_empty() {
        return "No MCP servers configured.".to_string();
    }
    let mut out = "MCP servers:".to_string();
    for server in servers {
        let era = match &server.era {
            Some(Era::Modern) => "modern".to_string(),
            Some(Era::Legacy(version)) => format!("legacy {version}"),
            None => "-".to_string(),
        };
        let state = match &server.state {
            ServerState::Connected { tools } => format!("connected ({tools} tools)"),
            ServerState::Disabled => "disabled".to_string(),
            ServerState::NeedsLogin => "needs login".to_string(),
            ServerState::Failed(message) => format!("failed: {message}"),
        };
        let _ = write!(
            out,
            "\n- {}: {} ({}, {})",
            server.name, state, server.transport, era
        );
    }
    out
}

/// Runs the OAuth flow for one configured HTTP server and persists its
/// token. Port of `/mcp login <server>`.
async fn mcp_login(
    controller: &Controller,
    name: &str,
    stdout: &mut (dyn Write + Send),
    stderr: &mut (dyn Write + Send),
    cancel: &CancellationToken,
) -> Result<(), Error> {
    if !controller.dynamic_content() {
        let _ = writeln!(stdout, "MCP sign-in is unavailable in this session");
        return Ok(());
    }
    let builder = controller.builder();
    let Some(server) = builder
        .mcp
        .servers
        .iter()
        .find(|server| server.name == name)
    else {
        let _ = writeln!(stderr, "unknown MCP server: {name}");
        return Ok(());
    };
    let McpTransport::Http {
        url,
        auth,
        oauth_client_id,
        oauth_scopes,
        ..
    } = &server.transport
    else {
        let _ = writeln!(
            stderr,
            "{name} is a stdio server; MCP login only applies to HTTP servers with OAuth"
        );
        return Ok(());
    };
    if *auth != McpAuth::OAuth {
        let _ = writeln!(stderr, "{name} does not use OAuth; no login is required");
        return Ok(());
    }
    let token_path = crate::mcp::oauth::token_path(Path::new(&builder.home), name);
    let shared: SharedWriter<'_> = Mutex::new(stdout);
    let opener = browser_opener(&shared);
    let request = crate::mcp::oauth::LoginRequest {
        server: name,
        url,
        client_id: oauth_client_id.as_deref(),
        scopes: oauth_scopes,
        ports: &crate::auth::oauth::LOOPBACK_PORTS,
        token_path: &token_path,
    };
    let result = crate::mcp::oauth::login(request, cancel, &opener).await;
    drop(opener);
    let stdout = shared
        .into_inner()
        .unwrap_or_else(|poison| poison.into_inner());
    match result {
        Ok(()) => {
            let _ = writeln!(
                stdout,
                "Signed in to {name}. Restart Otto to use the new credentials."
            );
            Ok(())
        }
        Err(error) => Err(command_error("/mcp login", error)),
    }
}

/// Port of `/mcp` / `/mcp login <server>`. A free function, rather than a
/// `Repl` method, for the same reason as [`repl_memory_command`].
pub(crate) async fn repl_mcp_command(
    controller: &Controller,
    args: &str,
    stdout: &mut (dyn Write + Send),
    stderr: &mut (dyn Write + Send),
    cancel: &CancellationToken,
) -> Result<(), Error> {
    let fields: Vec<&str> = args.split_whitespace().collect();
    match fields.as_slice() {
        [] => {
            let _ = writeln!(stdout, "{}", mcp_report(controller));
            Ok(())
        }
        ["login", name] => mcp_login(controller, name, stdout, stderr, cancel).await,
        _ => {
            let _ = writeln!(stderr, "{MCP_USAGE}");
            Ok(())
        }
    }
}

fn command_error(command: &str, message: impl std::fmt::Display) -> Error {
    Error::Command {
        command: command.to_string(),
        message: message.to_string(),
    }
}

/// Port of `REPL.memoryCommand`. A free function, rather than a `Repl`
/// method, so `tui::app`'s `/memory` dispatch can reuse it against its own
/// captured-output buffers instead of a line-oriented `Repl`'s stdout/
/// stderr.
pub(crate) fn repl_memory_command(
    controller: &Controller,
    args: &str,
    stdout: &mut (dyn Write + Send),
    stderr: &mut (dyn Write + Send),
) -> Result<(), Error> {
    let Some((service, user_scope, workspace_scope)) = controller.memory_manager() else {
        return Err(command_error("/memory", MEMORY_UNAVAILABLE));
    };
    let fields: Vec<&str> = args.split_whitespace().collect();
    let Some((subcommand, rest)) = fields.split_first() else {
        let _ = writeln!(stderr, "{MEMORY_USAGE}");
        return Ok(());
    };
    let scopes = vec![user_scope, workspace_scope];

    match *subcommand {
        "search" => {
            let result = service
                .search(&SearchRequest {
                    query: rest.join(" "),
                    scopes,
                    limit: SEARCH_LIMIT,
                    token_budget: SEARCH_TOKEN_BUDGET,
                    now: Utc::now(),
                    ..SearchRequest::default()
                })
                .map_err(|error| command_error("/memory search", error))?;
            let _ = writeln!(stdout, "{}", render_search_result(&result));
        }
        "forget" if rest.len() == 1 => {
            let id = rest[0];
            let mut last_error = None;
            for scope in scopes {
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
                let result = service
                    .forget(&ForgetRequest {
                        reference,
                        expected_revision: record.revision,
                        purge_backups: false,
                        confirm_purge: false,
                    })
                    .map_err(|error| command_error("/memory forget", error))?;
                let _ = writeln!(
                    stdout,
                    "forgot {} (revision {})",
                    result.tombstone.id, record.revision
                );
                return Ok(());
            }
            let reason = last_error
                .map(|error| error.to_string())
                .unwrap_or_else(|| "not found".to_string());
            return Err(command_error(
                "/memory forget",
                format!("record {id} not found: {reason}"),
            ));
        }
        _ => {
            let _ = writeln!(stderr, "{MEMORY_USAGE}");
        }
    }
    Ok(())
}

/// Port of `REPL.rememberCommand`. See [`repl_memory_command`] for why this
/// is a free function.
pub(crate) fn repl_remember_command(
    controller: &Controller,
    args: &str,
    stdout: &mut (dyn Write + Send),
    stderr: &mut (dyn Write + Send),
) -> Result<(), Error> {
    let Some((service, user_scope, workspace_scope)) = controller.memory_manager() else {
        return Err(command_error("/remember", MEMORY_UNAVAILABLE));
    };
    let (scope_flag, kind, key, text) = parse_remember_argument(args);
    if text.is_empty() {
        let _ = writeln!(stderr, "{REMEMBER_USAGE}");
        return Ok(());
    }
    let scope = match scope_flag {
        "" | "workspace" => workspace_scope,
        "user" => user_scope,
        other => {
            let _ = writeln!(stderr, "unknown scope {other:?}, want user or workspace");
            return Ok(());
        }
    };
    let record = service
        .remember(&RememberRequest {
            scope,
            kind: kind.to_string(),
            key: key.to_string(),
            text: text.to_string(),
            ..RememberRequest::default()
        })
        .map_err(|error| command_error("/remember", error))?;
    let _ = writeln!(
        stdout,
        "remembered {} (scope={}/{} kind={})",
        record.id, record.scope.namespace, record.scope.id, record.kind
    );
    Ok(())
}

impl Repl<'_> {
    /// Port of `REPL.memoryCommand`.
    pub(super) fn memory_command(&mut self, args: &str) -> Result<(), Error> {
        repl_memory_command(self.controller, args, &mut *self.stdout, &mut *self.stderr)
    }

    /// Port of `REPL.rememberCommand`.
    pub(super) fn remember_command(&mut self, args: &str) -> Result<(), Error> {
        repl_remember_command(self.controller, args, &mut *self.stdout, &mut *self.stderr)
    }

    pub(super) fn skills_command(&mut self) {
        let _ = writeln!(self.stdout, "{}", skills_report(self.controller));
    }

    pub(super) fn skill_command(&mut self, args: &str) {
        let text = skill_report(self.controller, args);
        if text == SKILL_USAGE || text.starts_with("unknown skill:") || text.starts_with("skill ") {
            let _ = writeln!(self.stderr, "{text}");
        } else {
            let _ = writeln!(self.stdout, "{text}");
        }
    }

    /// Port of `REPL.tasksCommand`.
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

    /// Port of `REPL.taskCommand`.
    pub(super) fn task_command(&mut self, args: &str) {
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
mod tests {
    use super::*;
    use crate::cli::testutil;
    use crate::subagent::tasks::{Task, TaskStatus};
    use otto_core::config::memory::MemoryRuntime;
    use std::path::Path;
    use tokio_util::sync::CancellationToken;

    #[derive(Clone, Default)]
    struct Buffer(Arc<std::sync::Mutex<Vec<u8>>>);

    impl Buffer {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().expect("buffer").clone()).expect("utf-8")
        }
    }

    impl std::io::Write for Buffer {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("buffer").extend_from_slice(data);
            Ok(data.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A controller whose builder carries a usable SQLite memory service.
    async fn controller_with_memory(
        workspace: &Path,
        sessions: &Path,
        store_path: &Path,
    ) -> Controller {
        let mut builder = testutil::builder(workspace, sessions);
        let runtime = MemoryRuntime {
            enabled: true,
            backend: "sqlite".into(),
            sqlite_path: store_path.to_string_lossy().into_owned(),
            ..MemoryRuntime::default()
        };
        let (service, user_scope, usable) =
            super::super::wiring::open_memory_service(&runtime, &[], &mut Vec::new())
                .expect("open memory service");
        assert!(usable, "the test store must be usable");
        builder.memory = super::super::wiring::MemoryWiring {
            service,
            usable,
            user_scope,
            workspace_scope: super::super::wiring::workspace_memory_scope(
                &runtime,
                &workspace.to_string_lossy(),
            )
            .expect("workspace scope"),
            recall_limit: 8,
            recall_token_budget: 1000,
        };
        let runtime = testutil::initial_runtime(&builder);
        let session = builder.create_session(&runtime).expect("session");
        let runner = builder
            .build_runner(&session, &runtime)
            .await
            .expect("runner");
        let info = builder.runtime_info(&runtime);
        Controller::new(builder, true, session, runner, info)
    }

    /// Feeds `input` to a REPL on `controller` and returns its two streams.
    async fn session(input: &str, controller: &Controller) -> (String, String) {
        let stdout = Buffer::default();
        let stderr = Buffer::default();
        let mut repl = Repl::new(
            controller,
            Box::new(stdout.clone()),
            Box::new(stderr.clone()),
        );
        repl.run(
            std::io::Cursor::new(input.as_bytes().to_vec()),
            &CancellationToken::new(),
        )
        .await
        .expect("run");
        (stdout.text(), stderr.text())
    }

    #[test]
    fn remember_arguments_parse_flags_and_trailing_text() {
        assert_eq!(
            parse_remember_argument("--scope user --kind fact --key nickname call me Bai"),
            ("user", "fact", "nickname", "call me Bai")
        );
        assert_eq!(
            parse_remember_argument("just some text"),
            ("", "note", "", "just some text")
        );
    }

    #[test]
    fn search_results_render_as_plain_text() {
        assert_eq!(
            render_search_result(&SearchResult::default()),
            "no matching records"
        );
        let record = crate::memory::Record {
            id: "rec-1".into(),
            scope: Scope {
                namespace: "workspace".into(),
                id: "w1".into(),
            },
            kind: "preference".into(),
            key: "editor".into(),
            text: "uses vim".into(),
            revision: 1,
            ..crate::memory::Record::default()
        };
        assert_eq!(
            render_search_result(&SearchResult {
                records: vec![record],
                ..SearchResult::default()
            }),
            "1 records:\nid=rec-1 scope=workspace/w1 kind=preference key=editor revision=1 text=uses vim"
        );
    }

    #[test]
    fn an_empty_server_list_reports_none_configured() {
        assert_eq!(format_mcp_report(&[]), "No MCP servers configured.");
    }

    #[test]
    fn each_server_state_and_era_renders_its_own_line() {
        use crate::mcp::ServerStatus;

        let servers = vec![
            ServerStatus {
                name: "docs".to_string(),
                transport: "http",
                era: Some(Era::Modern),
                state: ServerState::Connected { tools: 3 },
            },
            ServerStatus {
                name: "legacy-tool".to_string(),
                transport: "http",
                era: Some(Era::Legacy("2025-11-25".to_string())),
                state: ServerState::NeedsLogin,
            },
            ServerStatus {
                name: "shell".to_string(),
                transport: "stdio",
                era: None,
                state: ServerState::Disabled,
            },
            ServerStatus {
                name: "broken".to_string(),
                transport: "stdio",
                era: None,
                state: ServerState::Failed("spawn failed: not found".to_string()),
            },
        ];

        assert_eq!(
            format_mcp_report(&servers),
            "MCP servers:\n\
             - docs: connected (3 tools) (http, modern)\n\
             - legacy-tool: needs login (http, legacy 2025-11-25)\n\
             - shell: disabled (stdio, -)\n\
             - broken: failed: spawn failed: not found (stdio, -)"
        );
    }

    /// A controller whose builder carries the given MCP servers, each
    /// reachable through `Controller::builder().mcp.servers` for `/mcp
    /// login`'s validation without a real connection ever being attempted
    /// (servers are `enabled: false`, so `connect_mcp` reports them as
    /// `Disabled` instead of dialing out).
    async fn controller_with_mcp_servers(
        workspace: &Path,
        sessions: &Path,
        servers: Vec<otto_core::config::McpServerRuntime>,
        dynamic_content: bool,
    ) -> Controller {
        let mut builder = testutil::builder(workspace, sessions);
        builder.mcp = otto_core::config::McpRuntime {
            enabled: true,
            call_timeout_secs: 5,
            connect_timeout_secs: 1,
            servers,
        };
        let runtime = testutil::initial_runtime(&builder);
        let session = builder.create_session(&runtime).expect("session");
        let runner = builder
            .build_runner(&session, &runtime)
            .await
            .expect("runner");
        let info = builder.runtime_info(&runtime);
        Controller::new(builder, dynamic_content, session, runner, info)
    }

    fn stdio_server(name: &str) -> otto_core::config::McpServerRuntime {
        otto_core::config::McpServerRuntime {
            name: name.to_string(),
            enabled: false,
            transport: McpTransport::Stdio {
                command: "otto-mcp-test-nonexistent-command".to_string(),
                args: Vec::new(),
                env: Vec::new(),
                cwd: ".".to_string(),
            },
            secrets: Vec::new(),
        }
    }

    fn http_server(name: &str, auth: McpAuth) -> otto_core::config::McpServerRuntime {
        otto_core::config::McpServerRuntime {
            name: name.to_string(),
            enabled: false,
            transport: McpTransport::Http {
                url: "https://mcp.example.com".to_string(),
                headers: Vec::new(),
                auth,
                oauth_client_id: None,
                oauth_scopes: Vec::new(),
            },
            secrets: Vec::new(),
        }
    }

    #[tokio::test]
    async fn mcp_with_no_servers_reports_none_configured() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;

        let (stdout, stderr) = session("/mcp\n/exit\n", &controller).await;
        assert!(stderr.is_empty(), "{stderr}");
        assert!(stdout.contains("No MCP servers configured."), "{stdout}");
    }

    #[tokio::test]
    async fn mcp_with_malformed_arguments_prints_usage_on_stderr() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;

        for input in ["/mcp bogus\n/exit\n", "/mcp login\n/exit\n"] {
            let (_, stderr) = session(input, &controller).await;
            assert!(stderr.contains(MCP_USAGE), "{input:?} -> {stderr}");
        }
    }

    #[tokio::test]
    async fn mcp_login_for_an_unknown_server_reports_it_on_stderr() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = controller_with_mcp_servers(
            workspace.path(),
            sessions.path(),
            vec![stdio_server("shell")],
            true,
        )
        .await;

        let (_, stderr) = session("/mcp login ghost\n/exit\n", &controller).await;
        assert!(stderr.contains("unknown MCP server: ghost"), "{stderr}");
    }

    #[tokio::test]
    async fn mcp_login_for_a_stdio_server_reports_it_on_stderr() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = controller_with_mcp_servers(
            workspace.path(),
            sessions.path(),
            vec![stdio_server("shell")],
            true,
        )
        .await;

        let (_, stderr) = session("/mcp login shell\n/exit\n", &controller).await;
        assert!(
            stderr.contains(
                "shell is a stdio server; MCP login only applies to HTTP servers with OAuth"
            ),
            "{stderr}"
        );
    }

    #[tokio::test]
    async fn mcp_login_for_a_non_oauth_http_server_reports_no_login_required() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = controller_with_mcp_servers(
            workspace.path(),
            sessions.path(),
            vec![http_server("docs", McpAuth::None)],
            true,
        )
        .await;

        let (_, stderr) = session("/mcp login docs\n/exit\n", &controller).await;
        assert!(
            stderr.contains("docs does not use OAuth; no login is required"),
            "{stderr}"
        );
    }

    #[tokio::test]
    async fn mcp_login_is_unavailable_without_dynamic_content() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = controller_with_mcp_servers(
            workspace.path(),
            sessions.path(),
            vec![http_server("docs", McpAuth::OAuth)],
            false,
        )
        .await;

        let (stdout, _) = session("/mcp login docs\n/exit\n", &controller).await;
        assert!(
            stdout.contains("MCP sign-in is unavailable in this session"),
            "{stdout}"
        );
    }

    #[tokio::test]
    async fn the_help_text_lists_the_memory_and_task_commands() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;

        let (stdout, _) = session("/help\n/exit\n", &controller).await;

        for command in ["/memory", "/remember", "/tasks", "/task ", "/mcp"] {
            assert!(stdout.contains(command), "{command} missing from {stdout}");
        }
    }

    #[tokio::test]
    async fn memory_commands_without_a_service_are_command_errors() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;

        for (input, command) in [
            ("/memory search vim\n", "/memory"),
            ("/remember prefers dark mode\n", "/remember"),
        ] {
            let mut repl = Repl::new(
                &controller,
                Box::new(Buffer::default()),
                Box::new(Buffer::default()),
            );
            let error = repl
                .run(
                    std::io::Cursor::new(input.as_bytes().to_vec()),
                    &CancellationToken::new(),
                )
                .await
                .expect_err("command error");
            assert!(
                super::super::repl::is_command_error(&error, command),
                "{error:?}"
            );
            assert_eq!(error.to_string(), MEMORY_UNAVAILABLE);
        }
    }

    #[tokio::test]
    async fn memory_and_remember_round_trip_through_the_store() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let store = tempfile::tempdir().expect("store");
        let controller = controller_with_memory(
            workspace.path(),
            sessions.path(),
            &store.path().join("m.db"),
        )
        .await;

        let (stdout, stderr) = session(
            "/memory\n/remember --scope user\n/remember --scope bogus text\n\
             /remember --kind preference --key editor vim\n/memory search vim\n/exit\n",
            &controller,
        )
        .await;

        assert!(stderr.contains(MEMORY_USAGE), "{stderr}");
        assert!(stderr.contains(REMEMBER_USAGE), "{stderr}");
        assert!(
            stderr.contains("unknown scope \"bogus\", want user or workspace"),
            "{stderr}"
        );
        assert!(stdout.contains("remembered "), "{stdout}");
        assert!(stdout.contains("kind=preference"), "{stdout}");
        assert!(stdout.contains("1 records:"), "{stdout}");
        assert!(stdout.contains("text=vim"), "{stdout}");
    }

    #[tokio::test]
    async fn forget_resolves_the_revision_and_reports_a_missing_record() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let store = tempfile::tempdir().expect("store");
        let controller = controller_with_memory(
            workspace.path(),
            sessions.path(),
            &store.path().join("m.db"),
        )
        .await;
        let (service, _, workspace_scope) = controller.memory_manager().expect("memory");
        let record = service
            .remember(&RememberRequest {
                scope: workspace_scope,
                kind: "note".into(),
                text: "vim".into(),
                ..RememberRequest::default()
            })
            .expect("remember");

        let (stdout, _) = session(
            &format!("/memory forget {}\n/exit\n", record.id),
            &controller,
        )
        .await;
        assert!(
            stdout.contains(&format!("forgot {} (revision 1)", record.id)),
            "{stdout}"
        );

        let mut repl = Repl::new(
            &controller,
            Box::new(Buffer::default()),
            Box::new(Buffer::default()),
        );
        let error = repl
            .run(
                std::io::Cursor::new(b"/memory forget missing\n".to_vec()),
                &CancellationToken::new(),
            )
            .await
            .expect_err("command error");
        assert!(
            super::super::repl::is_command_error(&error, "/memory forget"),
            "{error:?}"
        );
        assert!(error.to_string().contains("not found"), "{error}");
    }

    #[tokio::test]
    async fn the_task_commands_report_an_empty_registry_and_reject_bad_arguments() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;

        let (stdout, stderr) = session(
            "/tasks\n/task\n/task cancel\n/task nope\n/exit\n",
            &controller,
        )
        .await;

        assert!(
            controller.subagent_tasks().is_some(),
            "sub-agents must be wired"
        );
        assert!(stdout.contains("no tasks in this session"), "{stdout}");
        assert!(
            stderr.contains("usage: /task <id|name> | /task cancel <id|name>"),
            "{stderr}"
        );
        assert!(stderr.contains("unknown task: nope"), "{stderr}");
    }

    #[tokio::test]
    async fn timers_lists_cancels_and_reports_bad_input() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let reminders = controller.reminders().expect("timer registry");
        reminders
            .schedule(std::time::Duration::from_secs(90), "check the build".into())
            .expect("schedule");

        let (stdout, stderr) = session(
            "/timers
/timers cancel r1
/timers
/timers cancel r1
/timers nope
/exit
",
            &controller,
        )
        .await;

        assert!(stdout.contains("check the build"), "{stdout}");
        assert!(stdout.contains("in 1m"), "{stdout}");
        assert!(stdout.contains("canceled r1"), "{stdout}");
        assert!(stdout.contains("no timers in this session"), "{stdout}");
        assert!(stderr.contains("unknown timer: r1"), "{stderr}");
        assert!(stderr.contains("usage: /timers [cancel <id>]"), "{stderr}");
        assert!(reminders.list().is_empty());
    }

    /// Ports Go's `/task <id>` detail rendering without a provider: the
    /// registry is written directly, as `internal/repl/tasks_test.go` does
    /// through its fake task view.
    #[tokio::test]
    async fn task_detail_prints_the_line_model_and_result() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let tasks = controller.subagent_tasks().expect("sub-agent tasks");
        let task = tasks
            .add(
                Task {
                    description: "check the tests".into(),
                    model: "gpt-alpha".into(),
                    ..Task::default()
                },
                None,
                None,
            )
            .expect("add task");
        tasks.finish(&task.id, TaskStatus::Succeeded, Utc::now(), "done", "");

        let (stdout, _) =
            session(&format!("/tasks\n/task {}\n/exit\n", task.id), &controller).await;

        assert!(stdout.contains(&task.id), "{stdout}");
        assert!(stdout.contains("model: gpt-alpha"), "{stdout}");
        assert!(stdout.contains("result: done"), "{stdout}");
    }

    #[tokio::test]
    async fn cancelling_a_queued_task_reports_it() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let tasks = controller.subagent_tasks().expect("sub-agent tasks");
        let task = tasks
            .add(
                Task {
                    description: "cancel me".into(),
                    ..Task::default()
                },
                None,
                None,
            )
            .expect("add task");

        let (stdout, stderr) = session(
            &format!("/task cancel {}\n/task cancel missing\n/exit\n", task.id),
            &controller,
        )
        .await;

        assert!(
            stdout.contains(&format!("canceled {}", task.id)),
            "{stdout}"
        );
        assert!(!stderr.is_empty(), "an unknown id must report an error");
    }

    /// Port of `TestTasksCommandListsTasks`. The column spellings themselves
    /// are `subagent::format`'s tests; what the REPL owns is one line per
    /// task, in registry order.
    #[tokio::test]
    async fn the_task_list_prints_one_line_per_task() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let tasks = controller.subagent_tasks().expect("sub-agent tasks");

        let started = Utc::now() - chrono::TimeDelta::seconds(42);
        let reviewed = tasks
            .add(
                Task {
                    description: "review the diff".into(),
                    prompt: "review please".into(),
                    ..Task::default()
                },
                None,
                None,
            )
            .expect("add task");
        tasks.mark_running(&reviewed.id, started);
        for _ in 0..7 {
            tasks.record_tool_call(&reviewed.id, "read");
        }
        tasks.record_provider_step(
            &reviewed.id,
            otto_core::model::Usage {
                input_tokens: 10000,
                output_tokens: 2310,
                ..otto_core::model::Usage::default()
            },
            "",
            true,
        );
        tasks.finish(
            &reviewed.id,
            TaskStatus::Succeeded,
            started + chrono::TimeDelta::seconds(42),
            "",
            "",
        );

        // Canceled while still queued: no start time, so the elapsed column
        // must read "0s" rather than the span from the zero instant.
        let aborted = tasks
            .add(
                Task {
                    description: "abort early".into(),
                    ..Task::default()
                },
                None,
                None,
            )
            .expect("add task");
        tasks.finish(&aborted.id, TaskStatus::Canceled, Utc::now(), "", "");

        let (stdout, stderr) = session("/tasks\n/exit\n", &controller).await;

        for want in [
            "succeeded",
            "42s",
            "7 tools",
            "12,310 tokens",
            "review the diff",
        ] {
            assert!(stdout.contains(want), "stdout missing {want:?}: {stdout}");
        }
        let line = stdout
            .lines()
            .find(|line| line.starts_with(&aborted.id))
            .unwrap_or_else(|| panic!("no line for {}: {stdout}", aborted.id));
        assert!(line.contains("canceled"), "{line:?}");
        assert!(line.contains("0s"), "{line:?}");
        assert_eq!(stderr, "");
    }

    /// Port of `TestTaskCommandShowsStepsAndResult`. The step spellings are
    /// `subagent::format`'s tests; what the REPL owns is the order of the
    /// task line, the model line, the steps and the result.
    #[tokio::test]
    async fn task_detail_prints_the_child_steps_between_the_model_and_the_result() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let tasks = controller.subagent_tasks().expect("sub-agent tasks");

        let history = vec![
            testutil::user("review please"),
            otto_core::model::Message {
                role: otto_core::model::Role::Assistant,
                blocks: vec![otto_core::model::Block {
                    block_type: otto_core::model::BlockType::ToolCall,
                    tool_name: "read".into(),
                    arguments: Some(
                        serde_json::value::RawValue::from_string(r#"{"path":"main.go"}"#.into())
                            .expect("valid JSON"),
                    ),
                    ..otto_core::model::Block::default()
                }],
                ..otto_core::model::Message::default()
            },
            otto_core::model::Message {
                role: otto_core::model::Role::Tool,
                blocks: vec![otto_core::model::Block {
                    block_type: otto_core::model::BlockType::ToolResult,
                    text: "package main".into(),
                    ..otto_core::model::Block::default()
                }],
                ..otto_core::model::Message::default()
            },
            otto_core::model::Message {
                role: otto_core::model::Role::Assistant,
                blocks: vec![otto_core::model::Block {
                    block_type: otto_core::model::BlockType::Text,
                    text: "looks fine".into(),
                    ..otto_core::model::Block::default()
                }],
                ..otto_core::model::Message::default()
            },
        ];
        let task = tasks
            .add(
                Task {
                    description: "review".into(),
                    model: "gpt-test-model".into(),
                    ..Task::default()
                },
                None,
                Some(Arc::new(move || history.clone())),
            )
            .expect("add task");
        tasks.finish(
            &task.id,
            TaskStatus::Succeeded,
            Utc::now(),
            "no issues found",
            "",
        );

        let (stdout, _) = session(&format!("/task {}\n/exit\n", task.id), &controller).await;

        let lines: Vec<&str> = stdout.lines().collect();
        let model_index = lines
            .iter()
            .position(|line| *line == "model: gpt-test-model")
            .unwrap_or_else(|| panic!("no model line: {stdout}"));
        assert!(model_index > 0, "{stdout}");
        assert!(lines[model_index - 1].contains(&task.id), "{stdout}");
        assert!(
            stdout.contains("\u{2192} read {\"path\":\"main.go\"}"),
            "{stdout}"
        );
        assert!(stdout.contains("assistant: looks fine"), "{stdout}");
        assert!(stdout.contains("result: no issues found"), "{stdout}");
        assert!(
            !stdout.contains("review please"),
            "the delegated prompt is not a step: {stdout}"
        );
        assert!(
            !stdout.contains("package main"),
            "tool results are not steps: {stdout}"
        );
    }

    /// Port of `TestTaskCommandByName`: both `/task` and `/task cancel`
    /// resolve a task name as well as an id.
    #[tokio::test]
    async fn the_task_commands_resolve_a_task_name() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let tasks = controller.subagent_tasks().expect("sub-agent tasks");
        let canceled = Arc::new(CancellationToken::new());
        let task = tasks
            .add(
                Task {
                    name: "lint-check".into(),
                    description: "review".into(),
                    ..Task::default()
                },
                Some((*canceled).clone()),
                None,
            )
            .expect("add task");

        let (stdout, stderr) = session(
            "/task lint-check\n/task cancel lint-check\n/exit\n",
            &controller,
        )
        .await;

        assert!(stdout.contains(&task.id), "{stdout}");
        assert!(stdout.contains("canceled lint-check"), "{stdout}");
        assert!(canceled.is_cancelled(), "the cancel token must fire");
        assert_eq!(stderr, "");
    }

    /// Port of `TestTasksCommandWithoutTaskLister`: with `[agents]` off the
    /// runner registers no registry, and both commands say so.
    #[tokio::test]
    async fn the_task_commands_without_a_registry_report_it_once_each() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let mut builder = testutil::builder(workspace.path(), sessions.path());
        builder.config.agents.enabled = Some(false);
        let runtime = testutil::initial_runtime(&builder);
        let session_store = builder.create_session(&runtime).expect("session");
        let runner = builder
            .build_runner(&session_store, &runtime)
            .await
            .expect("runner");
        let info = builder.runtime_info(&runtime);
        let controller = Controller::new(builder, true, session_store, runner, info);
        assert!(controller.subagent_tasks().is_none(), "agents are disabled");

        let (_, stderr) = session("/tasks\n/task t1\n/exit\n", &controller).await;

        assert_eq!(stderr.matches(SUBAGENTS_UNAVAILABLE).count(), 2, "{stderr}");
    }
}
