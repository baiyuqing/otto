//! The cross-process sub-agent task recorder and reader.
//!
//! `~/.otto/tasks.db` is one SQLite table, `tasks`, keyed by
//! `(parent_session, task_id)`. Every otto process on the machine that runs
//! sub-agents writes its own rows to the same file; SQLite serialises the
//! writes, and no row is ever written by two processes, so there is no
//! conflict to resolve.
//!
//! Ownership: [`Store`] owns the connection. [`TaskContext`] is the part of a
//! row that is fixed for the life of one [`crate::subagent::tasks::Tasks`]
//! registry (the parent session, its workspace, and the owning process); the
//! rest comes from the [`Task`] snapshot passed to [`Recorder::upsert`].
//!
//! Concurrency: one mutex guards the connection, matching
//! [`crate::usage::Store`]. Liveness (queued/running vs. `interrupted`) is
//! computed on every read, from the pid and process start time stored in the
//! row, and is never written back.
//!
//! Errors: [`Recorder::upsert`] never fails its caller. A write error
//! disables the store for the rest of the process without output, like
//! `usage.db`'s collector: stderr output would overwrite the TUI screen. [`Store::open`] fails closed: an
//! unreadable file or an unknown `user_version` is an [`Error`], and the
//! composition root that calls it decides to run without a recorder.
//!
//! `os_process_start_time` reads process metadata through raw `libc` calls
//! (`proc_pidinfo` on macOS, `/proc` on Linux); no safe wrapper for either is
//! already a workspace dependency.

#![allow(unsafe_code)]

use std::fmt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{Connection, OpenFlags, params};
use serde::Serialize;

use super::tasks::Task;

/// The schema version this build writes and expects to read.
const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS tasks (
    parent_session TEXT NOT NULL,
    task_id TEXT NOT NULL,
    workspace TEXT NOT NULL,
    parent_session_path TEXT NOT NULL,
    name TEXT NOT NULL,
    agent TEXT NOT NULL,
    description TEXT NOT NULL,
    model TEXT NOT NULL,
    context TEXT NOT NULL,
    prompt TEXT NOT NULL,
    status TEXT NOT NULL,
    created_at TEXT NOT NULL,
    started_at TEXT NOT NULL,
    finished_at TEXT NOT NULL,
    steps INTEGER NOT NULL,
    tool_calls INTEGER NOT NULL,
    last_tool TEXT NOT NULL,
    input_tokens INTEGER NOT NULL,
    output_tokens INTEGER NOT NULL,
    cached_tokens INTEGER NOT NULL,
    result TEXT NOT NULL,
    error TEXT NOT NULL,
    session_path TEXT NOT NULL,
    pid INTEGER NOT NULL,
    process_started_at TEXT NOT NULL,
    PRIMARY KEY (parent_session, task_id)
) STRICT;
CREATE INDEX IF NOT EXISTS tasks_created_at ON tasks(created_at);
CREATE INDEX IF NOT EXISTS tasks_workspace ON tasks(workspace, created_at);
"#;

const COLUMNS: &str = "parent_session, task_id, workspace, parent_session_path, name, agent, \
description, model, context, prompt, status, created_at, started_at, finished_at, steps, \
tool_calls, last_tool, input_tokens, output_tokens, cached_tokens, result, error, session_path, \
pid, process_started_at";

/// The first 64 KiB of `prompt`, `result` and `error` are kept; the rest is
/// dropped, on a character boundary.
const MAX_TEXT_BYTES: usize = 64 * 1024;

/// A storage failure carries no SQLite text, paths, or row values across the
/// adapter boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Error;

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("tasks store unavailable")
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

/// The part of a row that is fixed for the life of one task registry: the
/// parent session it belongs to and the process that owns it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TaskContext {
    /// The parent session id, or `memory:<pid>:<process start RFC 3339>` for
    /// an in-memory parent.
    pub parent_session: String,
    pub parent_session_path: String,
    pub workspace: String,
    pub pid: i64,
    pub process_started_at: String,
}

/// Records one task's row after every lifecycle change. Implementations must
/// not fail their caller; [`Store`] disables itself on a write error instead.
pub trait Recorder: Send + Sync {
    fn upsert(&self, context: &TaskContext, task: &Task);
}

/// One row as read back, in the shape the server API serialises. `status` is
/// the displayed status (it can be `interrupted`); `cancelable` is not here
/// because only the server, which knows which sessions it owns, can compute
/// it.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct TaskRow {
    pub parent_session: String,
    pub task_id: String,
    pub workspace: String,
    pub parent_session_path: String,
    pub name: String,
    pub agent: String,
    pub description: String,
    pub model: String,
    pub context: String,
    pub prompt: String,
    pub status: String,
    pub created_at: String,
    pub started_at: String,
    pub finished_at: String,
    pub steps: i64,
    pub tool_calls: i64,
    pub last_tool: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cached_tokens: i64,
    pub result: String,
    pub error: String,
    pub session_path: String,
}

/// A `GET /v1/tasks` query. `status` is one of the stored statuses or
/// `interrupted`; an unrecognised value matches no rows.
#[derive(Debug, Clone, Default)]
pub struct ListQuery {
    pub status: Option<String>,
    pub workspace: Option<String>,
    /// Defaults to 100, clamped to 500, when `None`.
    pub limit: Option<u32>,
    /// An exclusive `created_at` cursor.
    pub before: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ListResult {
    pub tasks: Vec<TaskRow>,
    /// Empty when there are no older rows.
    pub next_before: String,
}

/// One row exactly as stored, before liveness is applied.
struct Row {
    parent_session: String,
    task_id: String,
    workspace: String,
    parent_session_path: String,
    name: String,
    agent: String,
    description: String,
    model: String,
    context: String,
    prompt: String,
    status: String,
    created_at: String,
    started_at: String,
    finished_at: String,
    steps: i64,
    tool_calls: i64,
    last_tool: String,
    input_tokens: i64,
    output_tokens: i64,
    cached_tokens: i64,
    result: String,
    error: String,
    session_path: String,
    pid: i64,
    process_started_at: String,
}

impl Row {
    fn from_sql(row: &rusqlite::Row) -> rusqlite::Result<Self> {
        Ok(Self {
            parent_session: row.get(0)?,
            task_id: row.get(1)?,
            workspace: row.get(2)?,
            parent_session_path: row.get(3)?,
            name: row.get(4)?,
            agent: row.get(5)?,
            description: row.get(6)?,
            model: row.get(7)?,
            context: row.get(8)?,
            prompt: row.get(9)?,
            status: row.get(10)?,
            created_at: row.get(11)?,
            started_at: row.get(12)?,
            finished_at: row.get(13)?,
            steps: row.get(14)?,
            tool_calls: row.get(15)?,
            last_tool: row.get(16)?,
            input_tokens: row.get(17)?,
            output_tokens: row.get(18)?,
            cached_tokens: row.get(19)?,
            result: row.get(20)?,
            error: row.get(21)?,
            session_path: row.get(22)?,
            pid: row.get(23)?,
            process_started_at: row.get(24)?,
        })
    }

    /// The stored status, unless it is `queued` or `running` and the owning
    /// process is no longer verifiably the one that wrote the row.
    fn displayed_status(&self) -> String {
        if matches!(self.status.as_str(), "queued" | "running")
            && interrupted(self.pid, &self.process_started_at)
        {
            "interrupted".to_string()
        } else {
            self.status.clone()
        }
    }

    fn into_task_row(self) -> TaskRow {
        let status = self.displayed_status();
        TaskRow {
            parent_session: self.parent_session,
            task_id: self.task_id,
            workspace: self.workspace,
            parent_session_path: self.parent_session_path,
            name: self.name,
            agent: self.agent,
            description: self.description,
            model: self.model,
            context: self.context,
            prompt: self.prompt,
            status,
            created_at: self.created_at,
            started_at: self.started_at,
            finished_at: self.finished_at,
            steps: self.steps,
            tool_calls: self.tool_calls,
            last_tool: self.last_tool,
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cached_tokens: self.cached_tokens,
            result: self.result,
            error: self.error,
            session_path: self.session_path,
        }
    }
}

/// Whether a stored `queued`/`running` row no longer has a verifiably live
/// owner: the pid is not running, or is running as a different process
/// (started at a different time than the one recorded).
fn interrupted(pid: i64, process_started_at: &str) -> bool {
    let Ok(pid) = u32::try_from(pid) else {
        return true;
    };
    let Ok(recorded) = DateTime::parse_from_rfc3339(process_started_at) else {
        return true;
    };
    match os_process_start_time(pid) {
        Some(actual) => actual != recorded.with_timezone(&Utc),
        None => true,
    }
}

/// The current process's pid and start time, for [`TaskContext::pid`] and
/// [`TaskContext::process_started_at`].
pub fn current_process() -> (i64, String) {
    let pid = std::process::id();
    // ponytail: when the platform cannot report a start time (see
    // `os_process_start_time`), fall back to now so the row is still
    // written; every later liveness check for this pid will also fail to
    // confirm a match and read the row as `interrupted`, which is the honest
    // answer on a platform this build cannot introspect.
    let started = os_process_start_time(pid).unwrap_or_else(Utc::now);
    (
        i64::from(pid),
        started.to_rfc3339_opts(SecondsFormat::Nanos, true),
    )
}

#[cfg(target_os = "macos")]
fn os_process_start_time(pid: u32) -> Option<DateTime<Utc>> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let written = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            (&raw mut info).cast(),
            size,
        )
    };
    if written != size {
        return None;
    }
    DateTime::from_timestamp(
        info.pbi_start_tvsec as i64,
        (info.pbi_start_tvusec as u32).saturating_mul(1000),
    )
}

#[cfg(target_os = "linux")]
fn os_process_start_time(pid: u32) -> Option<DateTime<Utc>> {
    // /proc/{pid}/stat's comm field (2nd, parenthesised) may itself contain
    // spaces or parentheses, so the real fields start after the last ") ".
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.rsplit_once(") ")?.1;
    // state=0 ppid=1 pgrp=2 session=3 tty_nr=4 tpgid=5 flags=6 minflt=7
    // cminflt=8 majflt=9 cmajflt=10 utime=11 stime=12 cutime=13 cstime=14
    // priority=15 nice=16 num_threads=17 itrealvalue=18 starttime=19
    let starttime_ticks: u64 = after_comm.split_whitespace().nth(19)?.parse().ok()?;
    let clock_ticks_per_second = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if clock_ticks_per_second <= 0 {
        return None;
    }
    let boot = linux_boot_time()?;
    let offset_millis =
        (starttime_ticks as f64 / clock_ticks_per_second as f64 * 1000.0).round() as i64;
    boot.checked_add_signed(chrono::Duration::milliseconds(offset_millis))
}

#[cfg(target_os = "linux")]
fn linux_boot_time() -> Option<DateTime<Utc>> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    let btime = stat.lines().find_map(|line| line.strip_prefix("btime "))?;
    DateTime::from_timestamp(btime.trim().parse().ok()?, 0)
}

/// otto only ships a confined driver for macOS and runs unsandboxed on
/// Linux (see `AGENTS.md`); on any other platform a `queued`/`running` row
/// simply cannot be confirmed alive and always reads as `interrupted`.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn os_process_start_time(_pid: u32) -> Option<DateTime<Utc>> {
    None
}

/// Truncates `text` to at most 64 KiB, on a character boundary.
fn truncate(text: &str) -> String {
    if text.len() <= MAX_TEXT_BYTES {
        return text.to_string();
    }
    let mut end = MAX_TEXT_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

fn rfc3339_or_empty(value: Option<DateTime<Utc>>) -> String {
    value
        .map(|when| when.to_rfc3339_opts(SecondsFormat::Nanos, true))
        .unwrap_or_default()
}

/// `~/.otto/tasks.db`, shared by every otto process on the machine.
pub struct Store {
    connection: Mutex<Connection>,
    /// Set after the first write error; [`Recorder::upsert`] then no-ops
    /// instead of trying the connection again.
    disabled: AtomicBool,
}

impl Store {
    pub fn open(filename: &Path) -> Result<Self> {
        if let Some(parent) = filename.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|_| Error)?;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
                .map_err(|_| Error)?;
        }
        let connection = Connection::open_with_flags(
            filename,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|_| Error)?;
        std::fs::set_permissions(filename, std::fs::Permissions::from_mode(0o600))
            .map_err(|_| Error)?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(|_| Error)?;
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .map_err(|_| Error)?;
        Self::initialize(connection)
    }

    #[cfg(test)]
    pub(crate) fn open_in_memory() -> Result<Self> {
        Self::initialize(Connection::open_in_memory().map_err(|_| Error)?)
    }

    fn initialize(connection: Connection) -> Result<Self> {
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(|_| Error)?;
        match version {
            0 => {
                connection.execute_batch(SCHEMA).map_err(|_| Error)?;
                connection
                    .pragma_update(None, "user_version", SCHEMA_VERSION)
                    .map_err(|_| Error)?;
            }
            version if version == SCHEMA_VERSION => {}
            _ => return Err(Error),
        }
        Ok(Self {
            connection: Mutex::new(connection),
            disabled: AtomicBool::new(false),
        })
    }

    /// Whether a write error has disabled this store. Read-only queries
    /// ([`Store::list`], [`Store::get`]) are unaffected.
    pub fn is_disabled(&self) -> bool {
        self.disabled.load(Ordering::Relaxed)
    }

    /// Breaks the schema so the next write fails. Test-only, for exercising
    /// the disable-on-error path from other modules (`crate::subagent::tasks`).
    #[cfg(test)]
    pub(crate) fn break_schema_for_test(&self) {
        let _ = self
            .connection
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .execute_batch("DROP TABLE tasks");
    }

    fn upsert_row(&self, context: &TaskContext, task: &Task) -> Result<()> {
        self.connection
            .lock()
            .map_err(|_| Error)?
            .execute(
                &format!(
                    "INSERT INTO tasks ({COLUMNS}) VALUES (\
                        ?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,\
                        ?21,?22,?23,?24,?25\
                     )
                     ON CONFLICT(parent_session, task_id) DO UPDATE SET
                        workspace = excluded.workspace,
                        parent_session_path = excluded.parent_session_path,
                        name = excluded.name,
                        agent = excluded.agent,
                        description = excluded.description,
                        model = excluded.model,
                        context = excluded.context,
                        prompt = excluded.prompt,
                        status = excluded.status,
                        created_at = excluded.created_at,
                        started_at = excluded.started_at,
                        finished_at = excluded.finished_at,
                        steps = excluded.steps,
                        tool_calls = excluded.tool_calls,
                        last_tool = excluded.last_tool,
                        input_tokens = excluded.input_tokens,
                        output_tokens = excluded.output_tokens,
                        cached_tokens = excluded.cached_tokens,
                        result = excluded.result,
                        error = excluded.error,
                        session_path = excluded.session_path,
                        pid = excluded.pid,
                        process_started_at = excluded.process_started_at"
                ),
                params![
                    context.parent_session,
                    task.id,
                    context.workspace,
                    context.parent_session_path,
                    task.name,
                    task.agent,
                    task.description,
                    task.model,
                    task.context,
                    truncate(&task.prompt),
                    task.status.as_str(),
                    rfc3339_or_empty(task.created_at),
                    rfc3339_or_empty(task.started_at),
                    rfc3339_or_empty(task.finished_at),
                    task.steps,
                    task.tool_calls,
                    task.last_tool,
                    task.usage.input_tokens,
                    task.usage.output_tokens,
                    task.usage.cached_input_tokens,
                    truncate(&task.result),
                    truncate(&task.error),
                    task.session_path,
                    context.pid,
                    context.process_started_at,
                ],
            )
            .map_err(|_| Error)?;
        Ok(())
    }

    fn query_rows(
        connection: &Connection,
        sql: &str,
        params: impl rusqlite::Params,
    ) -> Result<Vec<Row>> {
        let mut statement = connection.prepare(sql).map_err(|_| Error)?;
        statement
            .query_map(params, Row::from_sql)
            .map_err(|_| Error)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|_| Error)
    }

    /// Rows newest first by `created_at`, with liveness applied.
    pub fn list(&self, query: &ListQuery) -> Result<ListResult> {
        let limit = i64::from(query.limit.unwrap_or(100).clamp(1, 500));
        let connection = self.connection.lock().map_err(|_| Error)?;

        let live_statuses = ["queued", "running", "interrupted"];
        let wants_live_only = query
            .status
            .as_deref()
            .is_some_and(|status| live_statuses.contains(&status));
        let wants_final_only = query
            .status
            .as_deref()
            .is_some_and(|status| matches!(status, "succeeded" | "failed" | "canceled"));

        let mut candidates = Vec::new();
        if !wants_live_only {
            let final_status = query
                .status
                .as_deref()
                .filter(|status| matches!(*status, "succeeded" | "failed" | "canceled"));
            // limit + 1: enough to tell whether a further, older final row
            // exists without fetching every final row ever recorded.
            candidates.extend(Self::query_rows(
                &connection,
                &format!(
                    "SELECT {COLUMNS} FROM tasks
                     WHERE status IN ('succeeded','failed','canceled')
                     AND (?1 IS NULL OR workspace = ?1)
                     AND (?2 IS NULL OR created_at < ?2)
                     AND (?3 IS NULL OR status = ?3)
                     ORDER BY created_at DESC LIMIT ?4"
                ),
                params![query.workspace, query.before, final_status, limit + 1],
            )?);
        }
        if !wants_final_only {
            // ponytail: every queued/running row is rescanned on each call,
            // because liveness is computed on read and tasks.db is never
            // pruned (see spec, "Out of scope: pruning"). Fine for local,
            // personal use; cache the interrupted verdict per (pid, started
            // at) if this ever shows up in a profile.
            let live_rows = Self::query_rows(
                &connection,
                &format!(
                    "SELECT {COLUMNS} FROM tasks
                     WHERE status IN ('queued','running')
                     AND (?1 IS NULL OR workspace = ?1)
                     AND (?2 IS NULL OR created_at < ?2)
                     ORDER BY created_at DESC"
                ),
                params![query.workspace, query.before],
            )?;
            for row in live_rows {
                let matches = match query.status.as_deref() {
                    None => true,
                    Some(wanted) => wanted == row.displayed_status(),
                };
                if matches {
                    candidates.push(row);
                }
            }
        }

        candidates.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        let limit = limit as usize;
        let next_before = if candidates.len() > limit {
            candidates[limit - 1].created_at.clone()
        } else {
            String::new()
        };
        candidates.truncate(limit);
        Ok(ListResult {
            tasks: candidates.into_iter().map(Row::into_task_row).collect(),
            next_before,
        })
    }

    /// One row by its primary key.
    pub fn get(&self, parent_session: &str, task_id: &str) -> Result<Option<TaskRow>> {
        let connection = self.connection.lock().map_err(|_| Error)?;
        let rows = Self::query_rows(
            &connection,
            &format!("SELECT {COLUMNS} FROM tasks WHERE parent_session = ?1 AND task_id = ?2"),
            params![parent_session, task_id],
        )?;
        Ok(rows.into_iter().next().map(Row::into_task_row))
    }
}

impl Recorder for Store {
    fn upsert(&self, context: &TaskContext, task: &Task) {
        if self.disabled.load(Ordering::Relaxed) {
            return;
        }
        if self.upsert_row(context, task).is_err() {
            self.disabled.store(true, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subagent::tasks::TaskStatus;

    fn context() -> TaskContext {
        TaskContext {
            parent_session: "s1".into(),
            parent_session_path: "/home/me/.otto/sessions/s1.jsonl".into(),
            workspace: "/work".into(),
            pid: 4_294_967_294, // a pid that cannot exist
            process_started_at: "2026-09-25T10:00:00Z".into(),
        }
    }

    fn task() -> Task {
        Task {
            id: "t1".into(),
            name: "reviewer".into(),
            agent: "code-reviewer".into(),
            description: "review the diff".into(),
            prompt: "review it".into(),
            context: "fresh".into(),
            model: "gpt-5.1".into(),
            status: TaskStatus::Running,
            created_at: DateTime::parse_from_rfc3339("2026-09-25T10:00:00Z")
                .ok()
                .map(|dt| dt.with_timezone(&Utc)),
            started_at: DateTime::parse_from_rfc3339("2026-09-25T10:00:01Z")
                .ok()
                .map(|dt| dt.with_timezone(&Utc)),
            steps: 3,
            tool_calls: 5,
            last_tool: "read".into(),
            session_path: "/home/me/.otto/sessions/s1/t1-child.jsonl".into(),
            ..Task::default()
        }
    }

    #[test]
    fn upsert_and_list_round_trip_a_task_row() {
        let store = Store::open_in_memory().expect("store");
        store.upsert(&context(), &task());

        let result = store.list(&ListQuery::default()).expect("list");
        assert_eq!(result.next_before, "");
        assert_eq!(result.tasks.len(), 1);
        let row = &result.tasks[0];
        assert_eq!(row.parent_session, "s1");
        assert_eq!(row.task_id, "t1");
        assert_eq!(row.workspace, "/work");
        assert_eq!(row.parent_session_path, "/home/me/.otto/sessions/s1.jsonl");
        assert_eq!(row.name, "reviewer");
        assert_eq!(row.agent, "code-reviewer");
        assert_eq!(row.description, "review the diff");
        assert_eq!(row.model, "gpt-5.1");
        assert_eq!(row.context, "fresh");
        assert_eq!(row.prompt, "review it");
        // A dead pid: displayed as interrupted even though stored as running.
        assert_eq!(row.status, "interrupted");
        assert_eq!(row.created_at, "2026-09-25T10:00:00.000000000Z");
        assert_eq!(row.finished_at, "");
        assert_eq!(row.steps, 3);
        assert_eq!(row.tool_calls, 5);
        assert_eq!(row.last_tool, "read");
        assert_eq!(
            row.session_path,
            "/home/me/.otto/sessions/s1/t1-child.jsonl"
        );

        let fetched = store.get("s1", "t1").expect("get").expect("row exists");
        assert_eq!(fetched, *row);
        assert_eq!(store.get("s1", "missing").expect("get"), None);
    }

    #[test]
    fn a_later_upsert_replaces_the_same_row() {
        let store = Store::open_in_memory().expect("store");
        store.upsert(&context(), &task());
        let finished = Task {
            status: TaskStatus::Succeeded,
            result: "done".into(),
            ..task()
        };
        store.upsert(&context(), &finished);

        let result = store.list(&ListQuery::default()).expect("list");
        assert_eq!(
            result.tasks.len(),
            1,
            "one row per (parent_session, task_id)"
        );
        assert_eq!(result.tasks[0].status, "succeeded");
        assert_eq!(result.tasks[0].result, "done");
    }

    #[test]
    fn prompt_result_and_error_are_truncated_at_64kib_on_a_char_boundary() {
        // 'é' is 2 bytes in UTF-8; MAX_TEXT_BYTES is even, so a run of them
        // lands a boundary exactly on the limit and truncation must not
        // split the last character.
        let long = "é".repeat(MAX_TEXT_BYTES / 2 + 10);
        let store = Store::open_in_memory().expect("store");
        let over_limit = Task {
            prompt: long.clone(),
            result: long.clone(),
            error: long.clone(),
            ..task()
        };
        store.upsert(&context(), &over_limit);

        let row = store.get("s1", "t1").expect("get").expect("row exists");
        for field in [&row.prompt, &row.result, &row.error] {
            assert!(field.len() <= MAX_TEXT_BYTES);
            assert!(long.starts_with(field.as_str()));
            assert!(field.is_char_boundary(field.len()));
        }
    }

    #[test]
    fn a_write_error_disables_the_recorder_after_one_failure() {
        let store = Store::open_in_memory().expect("store");
        store
            .connection
            .lock()
            .expect("connection")
            .execute_batch("DROP TABLE tasks")
            .expect("break the schema");

        assert!(!store.is_disabled());
        store.upsert(&context(), &task());
        assert!(store.is_disabled(), "a write error must disable the store");
        // A second call after disabling must not panic or try the broken
        // connection again.
        store.upsert(&context(), &task());
        assert!(store.is_disabled());
    }

    #[test]
    fn an_unknown_user_version_is_not_opened() {
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("tasks.db");
        {
            let connection = Connection::open(&path).expect("create");
            connection
                .pragma_update(None, "user_version", 999)
                .expect("set version");
        }
        assert!(Store::open(&path).is_err());
    }

    #[test]
    fn a_running_row_with_a_dead_pid_reads_as_interrupted() {
        let store = Store::open_in_memory().expect("store");
        store.upsert(&context(), &task());
        let row = store.get("s1", "t1").expect("get").expect("row exists");
        assert_eq!(row.status, "interrupted");
    }

    #[test]
    fn a_running_row_with_a_reused_pid_reads_as_interrupted() {
        let store = Store::open_in_memory().expect("store");
        let (pid, _) = current_process();
        let reused = TaskContext {
            pid,                                               // this test process, definitely alive
            process_started_at: "1999-01-01T00:00:00Z".into(), // definitely wrong
            ..context()
        };
        store.upsert(&reused, &task());
        let row = store.get("s1", "t1").expect("get").expect("row exists");
        assert_eq!(row.status, "interrupted");
    }

    #[test]
    fn a_finished_row_never_shows_interrupted() {
        let store = Store::open_in_memory().expect("store");
        let finished = Task {
            status: TaskStatus::Succeeded,
            ..task()
        };
        store.upsert(&context(), &finished); // context()'s pid cannot exist
        let row = store.get("s1", "t1").expect("get").expect("row exists");
        assert_eq!(row.status, "succeeded");
    }

    #[test]
    fn list_filters_by_status_workspace_and_paginates_with_before() {
        let store = Store::open_in_memory().expect("store");
        let (pid, started_at) = current_process();
        for (id, created_at, status, workspace) in [
            ("t1", "2026-09-25T10:00:00Z", TaskStatus::Succeeded, "/work"),
            ("t2", "2026-09-25T10:01:00Z", TaskStatus::Running, "/work"),
            ("t3", "2026-09-25T10:02:00Z", TaskStatus::Failed, "/other"),
            ("t4", "2026-09-25T10:03:00Z", TaskStatus::Succeeded, "/work"),
        ] {
            store.upsert(
                &TaskContext {
                    pid,
                    process_started_at: started_at.clone(),
                    workspace: workspace.into(),
                    ..context()
                },
                &Task {
                    id: id.into(),
                    status,
                    created_at: DateTime::parse_from_rfc3339(created_at)
                        .ok()
                        .map(|dt| dt.with_timezone(&Utc)),
                    ..Task::default()
                },
            );
        }

        let workspace_only = store
            .list(&ListQuery {
                workspace: Some("/work".into()),
                ..ListQuery::default()
            })
            .expect("list");
        assert_eq!(workspace_only.tasks.len(), 3);

        let running_only = store
            .list(&ListQuery {
                status: Some("running".into()),
                ..ListQuery::default()
            })
            .expect("list");
        assert_eq!(running_only.tasks.len(), 1);
        assert_eq!(running_only.tasks[0].task_id, "t2");

        let page = store
            .list(&ListQuery {
                limit: Some(2),
                ..ListQuery::default()
            })
            .expect("list");
        assert_eq!(page.tasks.len(), 2);
        assert_eq!(page.tasks[0].task_id, "t4");
        assert_eq!(page.tasks[1].task_id, "t3");
        assert_eq!(page.next_before, "2026-09-25T10:02:00.000000000Z");

        let next_page = store
            .list(&ListQuery {
                limit: Some(2),
                before: Some(page.next_before.clone()),
                ..ListQuery::default()
            })
            .expect("list");
        assert_eq!(next_page.tasks.len(), 2);
        assert_eq!(next_page.tasks[0].task_id, "t2");
        assert_eq!(next_page.tasks[1].task_id, "t1");
        assert_eq!(next_page.next_before, "");
    }
}
