//! SQLite repository for persisted sub-agent task records.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::storage::drivers::sqlite::SqliteDriver;
use crate::storage::{Connection, params};
use crate::storage::{OpenOptions, SqlDriver, SqlRequirements};
use crate::subagent::record::{
    Error, ListQuery, ListResult, MAX_TEXT_BYTES, Recorder, Result, TaskContext, TaskReader,
    TaskRepository, TaskRow, interrupted,
};
use crate::subagent::tasks::{ModelSource, Task};
use chrono::{DateTime, SecondsFormat, Utc};

/// The schema version this build writes and expects to read.
pub(super) const SCHEMA_VERSION: i64 = 2;

pub(super) const SCHEMA: &str = r#"
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
    model_source TEXT NOT NULL DEFAULT 'unknown',
    PRIMARY KEY (parent_session, task_id)
) STRICT;
CREATE INDEX IF NOT EXISTS tasks_created_at ON tasks(created_at);
CREATE INDEX IF NOT EXISTS tasks_workspace ON tasks(workspace, created_at);
"#;

const COLUMNS: &str = "parent_session, task_id, workspace, parent_session_path, name, agent, \
description, model, context, prompt, status, created_at, started_at, finished_at, steps, \
tool_calls, last_tool, input_tokens, output_tokens, cached_tokens, result, error, session_path, \
pid, process_started_at, model_source";

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
    model_source: ModelSource,
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
    fn from_sql(row: &crate::storage::Row) -> crate::storage::Result<Self> {
        Ok(Self {
            parent_session: row.get(0)?,
            task_id: row.get(1)?,
            workspace: row.get(2)?,
            parent_session_path: row.get(3)?,
            name: row.get(4)?,
            agent: row.get(5)?,
            description: row.get(6)?,
            model: row.get(7)?,
            model_source: match row.get::<String>(25)?.as_str() {
                "explicit" => ModelSource::Explicit,
                "definition" => ModelSource::Definition,
                "configured_first" => ModelSource::ConfiguredFirst,
                "parent" => ModelSource::Parent,
                _ => ModelSource::Unknown,
            },
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
            model_source: self.model_source,
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
        Self::open_with_driver(&SqliteDriver, filename)
    }

    pub fn open_with_driver(driver: &dyn SqlDriver, filename: &Path) -> Result<Self> {
        if let Some(parent) = filename.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|_| Error)?;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
                .map_err(|_| Error)?;
        }
        let connection = Connection::open_with_driver(
            driver,
            filename,
            OpenOptions::default(),
            SqlRequirements::DURABLE,
        )
        .map_err(|_| Error)?;
        std::fs::set_permissions(filename, std::fs::Permissions::from_mode(0o600))
            .map_err(|_| Error)?;
        Self::from_connection(connection)
    }

    pub fn from_connection(connection: Connection) -> Result<Self> {
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(|_| Error)?;
        connection
            .pragma_update("journal_mode", "WAL")
            .map_err(|_| Error)?;
        Self::initialize(connection)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn open_in_memory() -> Result<Self> {
        Self::initialize(Connection::open_in_memory().map_err(|_| Error)?)
    }

    fn initialize(connection: Connection) -> Result<Self> {
        let version: i64 = connection
            .query_row("PRAGMA user_version", (), |row| row.get(0))
            .map_err(|_| Error)?;
        match version {
            0 => {
                connection.execute_batch(SCHEMA).map_err(|_| Error)?;
                connection
                    .pragma_update("user_version", SCHEMA_VERSION)
                    .map_err(|_| Error)?;
            }
            1 => {
                connection.execute_batch(
                    "BEGIN IMMEDIATE; ALTER TABLE tasks ADD COLUMN model_source TEXT NOT NULL DEFAULT 'unknown'; PRAGMA user_version = 2; COMMIT;"
                ).map_err(|_| Error)?;
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
                        ?21,?22,?23,?24,?25,?26\
                     )
                     ON CONFLICT(parent_session, task_id) DO UPDATE SET
                        workspace = excluded.workspace,
                        parent_session_path = excluded.parent_session_path,
                        name = excluded.name,
                        agent = excluded.agent,
                        description = excluded.description,
                        model = excluded.model,
                        model_source = excluded.model_source,
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
                    task.model_source.as_str(),
                ],
            )
            .map_err(|_| Error)?;
        Ok(())
    }

    fn query_rows(
        connection: &Connection,
        sql: &str,
        params: impl crate::storage::IntoParams,
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

        let wanted_status = query.status.as_deref();
        let wants_dynamic = wanted_status.is_none()
            || matches!(wanted_status, Some("queued" | "running" | "interrupted"));
        let wants_final = wanted_status.is_none()
            || matches!(
                wanted_status,
                Some("succeeded" | "failed" | "canceled" | "interrupted")
            );

        let mut candidates = Vec::new();
        if wants_final {
            let final_status = wanted_status.filter(|status| {
                matches!(*status, "succeeded" | "failed" | "canceled" | "interrupted")
            });
            // limit + 1: enough to tell whether a further, older final row
            // exists without fetching every final row ever recorded.
            candidates.extend(Self::query_rows(
                &connection,
                &format!(
                    "SELECT {COLUMNS} FROM tasks
                     WHERE status IN ('succeeded','failed','canceled','interrupted')
                     AND (?1 IS NULL OR workspace = ?1)
                     AND (?2 IS NULL OR created_at < ?2)
                     AND (?3 IS NULL OR status = ?3)
                     ORDER BY created_at DESC LIMIT ?4"
                ),
                params![query.workspace, query.before, final_status, limit + 1],
            )?);
        }
        if wants_dynamic {
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
                let matches = match wanted_status {
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

impl TaskReader for Store {
    fn list(&self, query: &ListQuery) -> Result<ListResult> {
        Store::list(self, query)
    }

    fn get(&self, parent_session: &str, task_id: &str) -> Result<Option<TaskRow>> {
        Store::get(self, parent_session, task_id)
    }
}

impl TaskRepository for Store {}

#[cfg(test)]
mod model_source_tests {
    use super::*;

    #[test]
    fn model_source_survives_reopen_for_all_sources() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tasks.db");
        let context = TaskContext {
            parent_session: "parent".into(),
            ..Default::default()
        };
        let sources = [
            ModelSource::Explicit,
            ModelSource::Definition,
            ModelSource::ConfiguredFirst,
            ModelSource::Parent,
            ModelSource::Unknown,
        ];
        {
            let store = Store::open(&path).unwrap();
            for (index, source) in sources.iter().enumerate() {
                store.upsert(
                    &context,
                    &Task {
                        id: format!("t{index}"),
                        model: "same-model".into(),
                        model_source: *source,
                        ..Default::default()
                    },
                );
            }
            assert!(!store.is_disabled());
        }
        let store = Store::open(&path).unwrap();
        for (index, source) in sources.iter().enumerate() {
            let row = store.get("parent", &format!("t{index}")).unwrap().unwrap();
            assert_eq!(row.model_source, *source);
            assert_eq!(
                serde_json::to_value(row).unwrap()["model_source"],
                source.as_str()
            );
        }
    }

    #[test]
    fn version_one_database_migrates_old_rows_to_unknown() {
        let connection = Connection::open_in_memory().unwrap();
        let old_schema = SCHEMA.replace("    model_source TEXT NOT NULL DEFAULT 'unknown',\n", "");
        connection.execute_batch(&old_schema).unwrap();
        connection.pragma_update("user_version", 1).unwrap();
        let old_columns = COLUMNS.strip_suffix(", model_source").unwrap();
        let values = old_columns
            .split(',')
            .map(|column| match column.trim() {
                "pid" | "steps" | "tool_calls" | "input_tokens" | "output_tokens"
                | "cached_tokens" => "0",
                _ => "'legacy'",
            })
            .collect::<Vec<_>>()
            .join(",");
        connection
            .execute_batch(&format!(
                "INSERT INTO tasks ({old_columns}) VALUES ({values})"
            ))
            .unwrap();
        let store = Store::from_connection(connection).unwrap();
        let row = store.get("legacy", "legacy").unwrap().unwrap();
        assert_eq!(row.model, "legacy");
        assert_eq!(row.model_source, ModelSource::Unknown);
        let version: i64 = store
            .connection
            .lock()
            .unwrap()
            .query_row("PRAGMA user_version", (), |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }
}
