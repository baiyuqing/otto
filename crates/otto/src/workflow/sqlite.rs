//! SQLite repository for durable workflow runs.
use crate::storage::drivers::sqlite::SqliteDriver;
use crate::storage::{Connection, OptionalExtension, Transaction, params};
use crate::storage::{OpenOptions, SqlDriver, SqlRequirements};
use crate::workflow::contracts::*;
use crate::workflow::error::Error;
use crate::workflow::repository::WorkflowStore;
use chrono::{SecondsFormat, Utc};
use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

mod approvals;
mod attempts;
mod progress;
mod runs;
mod schema;

use progress::*;
use schema::*;

/// Run header columns, in the order every read selects them.
const RUN_COLUMNS: usize = 13;
struct RawRun {
    id: String,
    workflow: String,
    workspace: String,
    profile: String,
    provider: String,
    model: String,
    input: String,
    forked_from_run_id: Option<String>,
    forked_from_event_seq: Option<i64>,
    forked_from_step_id: Option<String>,
    status: String,
    definition_json: String,
    created_at: String,
}

impl RawRun {
    fn decode(self, steps: Vec<StepRecord>) -> Result<Run, Error> {
        Ok(Run {
            id: self.id,
            workflow: self.workflow,
            workspace: self.workspace,
            profile: self.profile,
            provider: self.provider,
            model: self.model,
            input: self.input,
            forked_from_run_id: self.forked_from_run_id,
            forked_from_event_seq: self.forked_from_event_seq,
            forked_from_step_id: self.forked_from_step_id,
            status: RunStatus::parse(&self.status)?,
            steps,
            definition: serde_json::from_str(&self.definition_json).map_err(|_| corrupt())?,
            created_at: self.created_at,
        })
    }
}

struct RawStep {
    id: String,
    kind: String,
    agent: String,
    prompt: String,
    needs: String,
    status: String,
    attempt: i64,
    result: String,
    error: String,
    transcript_path: String,
    source_run_id: Option<String>,
    source_step_id: Option<String>,
    source_attempt: Option<i64>,
}

impl RawStep {
    fn decode(self) -> Result<StepRecord, Error> {
        Ok(StepRecord {
            id: self.id,
            kind: StepKind::parse(&self.kind)?,
            agent: self.agent,
            prompt: self.prompt,
            needs: serde_json::from_str(&self.needs).map_err(|_| corrupt())?,
            status: StepStatus::parse(&self.status)?,
            attempt: u32::try_from(self.attempt).map_err(|_| corrupt())?,
            result: self.result,
            error: self.error,
            transcript_path: self.transcript_path,
            source_run_id: self.source_run_id,
            source_step_id: self.source_step_id,
            source_attempt: self
                .source_attempt
                .map(u32::try_from)
                .transpose()
                .map_err(|_| corrupt())?,
        })
    }
}

fn raw_run(row: &crate::storage::Row, offset: usize) -> crate::storage::Result<RawRun> {
    Ok(RawRun {
        id: row.get(offset)?,
        workflow: row.get(offset + 1)?,
        workspace: row.get(offset + 2)?,
        profile: row.get(offset + 3)?,
        provider: row.get(offset + 4)?,
        model: row.get(offset + 5)?,
        input: row.get(offset + 6)?,
        forked_from_run_id: row.get(offset + 7)?,
        forked_from_event_seq: row.get(offset + 8)?,
        forked_from_step_id: row.get(offset + 9)?,
        status: row.get(offset + 10)?,
        definition_json: row.get(offset + 11)?,
        created_at: row.get(offset + 12)?,
    })
}

fn raw_step(row: &crate::storage::Row, offset: usize) -> crate::storage::Result<RawStep> {
    Ok(RawStep {
        id: row.get(offset)?,
        kind: row.get(offset + 1)?,
        agent: row.get(offset + 2)?,
        prompt: row.get(offset + 3)?,
        needs: row.get(offset + 4)?,
        status: row.get(offset + 5)?,
        attempt: row.get(offset + 6)?,
        result: row.get(offset + 7)?,
        error: row.get(offset + 8)?,
        transcript_path: row.get(offset + 9)?,
        source_run_id: row.get(offset + 10)?,
        source_step_id: row.get(offset + 11)?,
        source_attempt: row.get(offset + 12)?,
    })
}

/// Caps a stored result so one pathological step cannot make every later read
/// and prompt unbounded.
fn bounded(result: &str) -> std::borrow::Cow<'_, str> {
    use std::borrow::Cow;
    if result.len() <= MAX_TEXT_BYTES {
        return Cow::Borrowed(result);
    }
    let mut end = MAX_TEXT_BYTES;
    while end > 0 && !result.is_char_boundary(end) {
        end -= 1;
    }
    Cow::Owned(format!(
        "{}\n\n[truncated at {MAX_TEXT_BYTES} bytes]",
        &result[..end]
    ))
}

struct CopiedStep {
    seq: i64,
    attempt: i64,
    result: String,
    transcript_path: String,
}
pub struct SqliteWorkflowStore {
    connection: Mutex<Connection>,
}
impl SqliteWorkflowStore {
    pub fn open(path: &Path) -> Result<Self, String> {
        Self::open_with_driver(&SqliteDriver, path)
    }

    pub fn open_with_driver(driver: &dyn SqlDriver, path: &Path) -> Result<Self, String> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|_| unavailable())?;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
                .map_err(|_| unavailable())?;
        }
        let connection = Connection::open_with_driver(
            driver,
            path,
            OpenOptions::default(),
            SqlRequirements::DURABLE,
        )
        .map_err(|_| unavailable())?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|_| unavailable())?;
        Self::from_connection(connection)
    }
    pub(crate) fn from_connection(connection: Connection) -> Result<Self, String> {
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(|_| unavailable())?;
        connection
            .pragma_update("journal_mode", "WAL")
            .map_err(|_| unavailable())?;
        Self::initialize(connection)
    }
    #[cfg(any(test, feature = "test-support"))]
    pub fn open_in_memory() -> Self {
        Self::initialize(Connection::open_in_memory().expect("in-memory database")).expect("schema")
    }
    fn initialize(connection: Connection) -> Result<Self, String> {
        connection
            .execute_batch(SCHEMA)
            .map_err(|_| unavailable())?;
        migrate_schema(&connection)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }
}
impl WorkflowStore for SqliteWorkflowStore {
    #[allow(clippy::too_many_arguments)]
    fn create_run(
        &self,
        id: &str,
        definition: &Definition,
        workspace: &str,
        profile: &str,
        provider: &str,
        model: &str,
        input: &str,
    ) -> Result<Run, Error> {
        self.create_run_inner(
            id, definition, workspace, profile, provider, model, input, None,
        )
        .map(|(run, _)| run)
    }
    fn create_origin_run(
        &self,
        id: &str,
        definition: &Definition,
        workspace: &str,
        runtime: &RuntimeIdentity,
        input: &str,
        session: &str,
        operation: &str,
    ) -> Result<(Run, bool), Error> {
        if session.is_empty() || operation.is_empty() {
            return Err(Error::invalid(
                "trusted parent session and operation identity required",
            ));
        }
        self.create_run_inner(
            id,
            definition,
            workspace,
            &runtime.profile,
            &runtime.provider,
            &runtime.model,
            input,
            Some((session, operation)),
        )
    }
    fn origin_run(
        &self,
        workspace: &str,
        session: &str,
        operation: &str,
    ) -> Result<Option<Run>, Error> {
        let id: Option<String> = self
            .connection
            .lock()
            .map_err(|_| op_error())?
            .query_row(
                "SELECT run_id FROM workflow_origins WHERE workspace=?1 AND \
            session_id=?2 AND operation_id=?3",
                params![workspace, session, operation],
                |row| row.get(0),
            )
            .optional()
            .map_err(|_| op_error())?;
        match id {
            Some(id) => self.get_run(&id),
            None => Ok(None),
        }
    }
    fn pending_events(
        &self,
        workspace: &str,
        session: &str,
        limit: usize,
    ) -> Result<Vec<EventRecord>, Error> {
        let connection = self.connection.lock().map_err(|_| op_error())?;
        let mut statement = connection
            .prepare(
                "SELECT e.seq,e.run_id,e.occurred_at,e.kind,e.step_id,e.status FROM \
            workflow_pending p JOIN workflow_events e ON e.seq=p.seq WHERE \
            p.workspace=?1 AND p.session_id=?2 ORDER BY p.seq LIMIT ?3",
            )
            .map_err(|_| op_error())?;
        let rows = statement
            .query_map(
                params![workspace, session, limit.clamp(1, 64) as i64],
                |row| {
                    Ok(EventRecord {
                        seq: row.get(0)?,
                        run_id: row.get(1)?,
                        occurred_at: row.get(2)?,
                        kind: row.get(3)?,
                        step_id: row.get(4)?,
                        status: row.get(5)?,
                    })
                },
            )
            .map_err(|_| op_error())?;
        rows.collect::<crate::storage::Result<Vec<_>>>()
            .map_err(|_| op_error())
    }
    fn acknowledge_event(&self, workspace: &str, session: &str, seq: i64) -> Result<(), Error> {
        self.connection
            .lock()
            .map_err(|_| op_error())?
            .execute(
                "DELETE FROM workflow_pending WHERE workspace=?1 AND session_id=?2 AND seq=?3",
                params![workspace, session, seq],
            )
            .map_err(|_| op_error())?;
        Ok(())
    }
    fn origin_runs(&self, workspace: &str, session: &str) -> Result<Vec<String>, Error> {
        let connection = self.connection.lock().map_err(|_| op_error())?;
        let mut statement = connection.prepare("SELECT run_id FROM workflow_origins WHERE workspace=?1 AND session_id=?2 ORDER BY rowid").map_err(|_| op_error())?;
        statement
            .query_map(params![workspace, session], |row| row.get(0))
            .map_err(|_| op_error())?
            .collect::<Result<Vec<String>, _>>()
            .map_err(|_| op_error())
    }
    fn get_run(&self, id: &str) -> Result<Option<Run>, Error> {
        let connection = self.connection.lock().map_err(|_| op_error())?;
        let header = connection
            .query_row(
                "SELECT id, workflow, workspace, profile, provider, model, input,
                        forked_from_run_id, forked_from_event_seq, forked_from_step_id,
                        status, definition_json, created_at
                 FROM workflow_runs WHERE id = ?1",
                [id],
                |row| raw_run(row, 0),
            )
            .optional()
            .map_err(|_| op_error())?;
        let Some(header) = header else {
            return Ok(None);
        };
        let mut statement = connection
            .prepare(
                "SELECT id, kind, agent, prompt, needs_json, status, attempt,
                        result, error, transcript_path, source_run_id, source_step_id,
                        source_attempt
                 FROM workflow_steps WHERE run_id = ?1 ORDER BY position",
            )
            .map_err(|_| op_error())?;
        let rows = statement
            .query_map([id], |row| raw_step(row, 0))
            .map_err(|_| op_error())?;
        let mut steps = Vec::new();
        for row in rows {
            steps.push(row.map_err(|_| op_error())?.decode()?);
        }
        Ok(Some(header.decode(steps)?))
    }

    fn fork_run(&self, id: &str, source_run_id: &str, after_step: &str) -> Result<Run, Error> {
        SqliteWorkflowStore::fork_run(self, id, source_run_id, after_step)
    }

    fn claim_ready(
        &self,
        run_id: &str,
        step_id: &str,
        transcript_path: &str,
    ) -> Result<u32, Error> {
        SqliteWorkflowStore::claim_ready(self, run_id, step_id, transcript_path)
    }

    fn finish_attempt(
        &self,
        run_id: &str,
        step_id: &str,
        attempt: u32,
        result: Result<&str, &str>,
    ) -> Result<(), Error> {
        SqliteWorkflowStore::finish_attempt(self, run_id, step_id, attempt, result)
    }

    fn interrupt_attempt(
        &self,
        run_id: &str,
        step_id: &str,
        attempt: u32,
        error: &str,
    ) -> Result<(), Error> {
        SqliteWorkflowStore::interrupt_attempt(self, run_id, step_id, attempt, error)
    }

    fn recover(&self, workspace: &str) -> Result<usize, Error> {
        SqliteWorkflowStore::recover(self, workspace)
    }

    fn pause_run(&self, run_id: &str, error: &str) -> Result<(), Error> {
        SqliteWorkflowStore::pause_run(self, run_id, error)
    }

    fn request_approval(
        &self,
        request_id: &str,
        run_id: &str,
        step_id: &str,
    ) -> Result<ApprovalRequest, Error> {
        SqliteWorkflowStore::request_approval(self, request_id, run_id, step_id)
    }

    fn respond(&self, request_id: &str, approved: bool) -> Result<String, Error> {
        SqliteWorkflowStore::respond(self, request_id, approved)
    }

    fn settle(&self, run_id: &str) -> Result<(), Error> {
        SqliteWorkflowStore::settle(self, run_id)
    }

    fn retry(&self, run_id: &str, step_id: &str) -> Result<(), Error> {
        SqliteWorkflowStore::retry(self, run_id, step_id)
    }

    fn cancel_attempt(&self, run_id: &str, step_id: &str, attempt: u32) -> Result<(), Error> {
        SqliteWorkflowStore::cancel_attempt(self, run_id, step_id, attempt)
    }

    fn cancel_run(&self, run_id: &str) -> Result<(), Error> {
        SqliteWorkflowStore::cancel_run(self, run_id)
    }

    fn list_runs(&self, workspace: &str, query: &ListQuery) -> Result<Vec<Run>, Error> {
        SqliteWorkflowStore::list_runs(self, workspace, query)
    }

    fn requests(&self, run_id: &str) -> Result<Vec<ApprovalRequest>, Error> {
        SqliteWorkflowStore::requests(self, run_id)
    }

    fn request(&self, request_id: &str) -> Result<Option<ApprovalRequest>, Error> {
        SqliteWorkflowStore::request(self, request_id)
    }

    /// Milliseconds from the run's `created_at` to now, for the run-finished
    /// event. `None` when the row or its timestamp is unavailable.
    fn run_age_ms(&self, run_id: &str) -> Option<u128> {
        SqliteWorkflowStore::run_age_ms(self, run_id)
    }

    fn events(&self, run_id: &str, after: i64) -> Result<Vec<EventRecord>, Error> {
        SqliteWorkflowStore::events(self, run_id, after)
    }
}

fn event(
    transaction: &Transaction<'_>,
    run_id: &str,
    kind: &str,
    step_id: &str,
    status: &str,
) -> Result<(), Error> {
    transaction
        .execute(
            "INSERT INTO workflow_events (run_id, occurred_at, kind, step_id, status)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![run_id, timestamp(), kind, step_id, status],
        )
        .map_err(|_| op_error())?;
    transaction
        .execute(
            "INSERT OR IGNORE INTO workflow_pending(workspace,session_id,seq) SELECT \
        o.workspace,o.session_id,e.seq FROM workflow_origins o JOIN \
        workflow_events e ON e.run_id=o.run_id WHERE e.seq=(SELECT MAX(seq) \
        FROM workflow_events) AND e.run_id=?1",
            params![run_id],
        )
        .map_err(|_| op_error())?;
    Ok(())
}

fn timestamp() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn unavailable() -> String {
    "workflow store unavailable".to_string()
}

fn op_error() -> Error {
    Error::unavailable(unavailable())
}

fn corrupt() -> Error {
    Error::internal("workflow record is corrupt")
}

#[cfg(test)]
mod repository_tests;

impl SqliteWorkflowStore {
    #[allow(clippy::too_many_arguments)]
    fn create_run_inner(
        &self,
        id: &str,
        definition: &Definition,
        workspace: &str,
        profile: &str,
        provider: &str,
        model: &str,
        input: &str,
        origin: Option<(&str, &str)>,
    ) -> Result<(Run, bool), Error> {
        if input.len() > MAX_TEXT_BYTES {
            return Err(Error::invalid("workflow input exceeds 65536 UTF-8 bytes"));
        }
        let definition_json = serde_json::to_string(definition).map_err(|_| op_error())?;
        let now = timestamp();
        let mut connection = self.connection.lock().map_err(|_| op_error())?;
        let transaction = connection.transaction().map_err(|_| op_error())?;
        if let Some((session, operation)) = origin {
            let receipt: Option<(String, String, String)> = transaction.query_row(
                "SELECT run_id, definition_hash, input FROM workflow_origins WHERE workspace=?1 AND session_id=?2 AND operation_id=?3",
                params![workspace, session, operation], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            ).optional().map_err(|_| op_error())?;
            if let Some((run_id, hash, previous_input)) = receipt {
                if hash != definition.hash || previous_input != input {
                    return Err(Error::invalid(
                        "operation receipt conflicts with definition/input; inspect the original run, do not replay",
                    ));
                }
                drop(transaction);
                drop(connection);
                return self
                    .get_run(&run_id)?
                    .map(|run| (run, false))
                    .ok_or_else(op_error);
            }
        }
        transaction
            .execute(
                "INSERT INTO workflow_runs (
                    id, workflow, definition_json, definition_hash, workspace,
                    profile, provider, model, input, status, created_at, updated_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'running', ?10, ?10)",
                params![
                    id,
                    definition.name,
                    definition_json,
                    definition.hash,
                    workspace,
                    profile,
                    provider,
                    model,
                    input,
                    now,
                ],
            )
            .map_err(|_| op_error())?;
        event(&transaction, id, "run_started", "", "running")?;
        for (position, step) in definition.steps.iter().enumerate() {
            let status = if step.needs.is_empty() {
                StepStatus::Ready
            } else {
                StepStatus::Pending
            };
            transaction
                .execute(
                    "INSERT INTO workflow_steps (
                        run_id, id, position, kind, agent, prompt, needs_json, status
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        id,
                        step.id,
                        position as i64,
                        step.kind.as_str(),
                        step.agent,
                        step.prompt,
                        serde_json::to_string(&step.needs).map_err(|_| op_error())?,
                        status.as_str(),
                    ],
                )
                .map_err(|_| op_error())?;
            if status == StepStatus::Ready {
                event(&transaction, id, "step_ready", &step.id, status.as_str())?;
            }
        }
        if let Some((session, operation)) = origin {
            transaction.execute("INSERT INTO \
                workflow_origins(workspace,session_id,operation_id,run_id,definition_hash,input) VALUES \
                (?1,?2,?3,?4,?5,?6)", params![workspace,session,operation,id,definition.hash,input]).map_err(|_| op_error())?;
            transaction
                .execute(
                    "INSERT OR IGNORE INTO workflow_pending(workspace,session_id,seq) SELECT \
                ?1,?2,seq FROM workflow_events WHERE run_id=?3",
                    params![workspace, session, id],
                )
                .map_err(|_| op_error())?;
        }
        transaction.commit().map_err(|_| op_error())?;
        drop(connection);
        self.get_run(id)?
            .map(|run| (run, true))
            .ok_or_else(op_error)
    }
}
