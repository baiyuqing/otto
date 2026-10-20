//! Turso repository for durable workflow runs.
use crate::storage::drivers::turso::TursoDriver;
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
pub(super) const SCHEMA: &str = r#"
PRAGMA foreign_keys = ON;
CREATE TABLE IF NOT EXISTS workflow_runs (
    id TEXT PRIMARY KEY,
    workflow TEXT NOT NULL,
    definition_json TEXT NOT NULL,
    definition_hash TEXT NOT NULL,
    workspace TEXT NOT NULL,
    profile TEXT NOT NULL,
    provider TEXT NOT NULL,
    model TEXT NOT NULL,
    input TEXT NOT NULL,
    forked_from_run_id TEXT,
    forked_from_event_seq INTEGER,
    forked_from_step_id TEXT,
    status TEXT NOT NULL CHECK (status IN (
        'running','waiting','paused','succeeded','failed','canceled'
    )),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS workflow_steps (
    run_id TEXT NOT NULL REFERENCES workflow_runs(id),
    id TEXT NOT NULL,
    position INTEGER NOT NULL CHECK (position >= 0),
    kind TEXT NOT NULL CHECK (kind IN ('agent','approval','handoff')),
    agent TEXT NOT NULL,
    prompt TEXT NOT NULL,
    needs_json TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN (
        'pending','ready','running','waiting','succeeded','failed','canceled','interrupted'
    )),
    attempt INTEGER NOT NULL DEFAULT 0 CHECK (attempt >= 0),
    result TEXT NOT NULL DEFAULT '',
    error TEXT NOT NULL DEFAULT '',
    transcript_path TEXT NOT NULL DEFAULT '',
    source_run_id TEXT,
    source_step_id TEXT,
    source_attempt INTEGER,
    PRIMARY KEY (run_id, id)
) STRICT;
CREATE TABLE IF NOT EXISTS workflow_attempts (
    run_id TEXT NOT NULL,
    step_id TEXT NOT NULL,
    attempt INTEGER NOT NULL CHECK (attempt > 0),
    status TEXT NOT NULL CHECK (status IN (
        'running','succeeded','failed','canceled','interrupted'
    )),
    transcript_path TEXT NOT NULL,
    result TEXT NOT NULL DEFAULT '',
    error TEXT NOT NULL DEFAULT '',
    started_at TEXT NOT NULL,
    finished_at TEXT,
    PRIMARY KEY (run_id, step_id, attempt),
    FOREIGN KEY (run_id, step_id) REFERENCES workflow_steps(run_id, id)
) STRICT;
CREATE TABLE IF NOT EXISTS workflow_requests (
    id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL,
    step_id TEXT NOT NULL,
    prompt TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('pending','approved','rejected','canceled')),
    created_at TEXT NOT NULL,
    responded_at TEXT,
    UNIQUE (run_id, step_id),
    FOREIGN KEY (run_id, step_id) REFERENCES workflow_steps(run_id, id)
) STRICT;
CREATE TABLE IF NOT EXISTS workflow_origins (
    workspace TEXT NOT NULL,
    session_id TEXT NOT NULL,
    operation_id TEXT NOT NULL,
    run_id TEXT NOT NULL REFERENCES workflow_runs(id),
    definition_hash TEXT NOT NULL,
    input TEXT NOT NULL,
    PRIMARY KEY(workspace, session_id, operation_id)
) STRICT;
CREATE TABLE IF NOT EXISTS workflow_events (
    seq INTEGER PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES workflow_runs(id),
    occurred_at TEXT NOT NULL,
    kind TEXT NOT NULL,
    step_id TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL DEFAULT ''
) STRICT;
CREATE INDEX IF NOT EXISTS workflow_events_run ON workflow_events(run_id, seq);
CREATE INDEX IF NOT EXISTS workflow_origins_run ON workflow_origins(run_id);
CREATE TABLE IF NOT EXISTS workflow_pending (
    workspace TEXT NOT NULL,
    session_id TEXT NOT NULL,
    seq INTEGER NOT NULL REFERENCES workflow_events(seq),
    PRIMARY KEY(workspace, session_id, seq)
) STRICT;
CREATE TABLE IF NOT EXISTS workflow_delivery_schema (version INTEGER PRIMARY KEY) STRICT;
"#;

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
pub struct TursoWorkflowStore {
    connection: Mutex<Connection>,
}
impl TursoWorkflowStore {
    pub fn open(path: &Path) -> Result<Self, String> {
        Self::open_with_driver(&TursoDriver, path)
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
    #[cfg(test)]
    pub(crate) fn open_in_memory() -> Self {
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
impl WorkflowStore for TursoWorkflowStore {
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
        let id: Option<String> = self.connection.lock().map_err(|_|op_error())?.query_row("SELECT run_id FROM workflow_origins WHERE workspace=?1 AND session_id=?2 AND operation_id=?3",params![workspace,session,operation],|row|row.get(0)).optional().map_err(|_|op_error())?;
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
        let mut statement = connection.prepare("SELECT e.seq,e.run_id,e.occurred_at,e.kind,e.step_id,e.status FROM workflow_pending p JOIN workflow_events e ON e.seq=p.seq WHERE p.workspace=?1 AND p.session_id=?2 ORDER BY p.seq LIMIT ?3").map_err(|_| op_error())?;
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
        let mut connection = self.connection.lock().map_err(|_| op_error())?;
        let transaction = connection.transaction().map_err(|_| op_error())?;
        let source = transaction
            .query_row(
                "SELECT workflow, definition_json, definition_hash, workspace, profile,
                        provider, model, input
                 FROM workflow_runs WHERE id = ?1",
                [source_run_id],
                |row| {
                    Ok((
                        row.get::<String>(0)?,
                        row.get::<String>(1)?,
                        row.get::<String>(2)?,
                        row.get::<String>(3)?,
                        row.get::<String>(4)?,
                        row.get::<String>(5)?,
                        row.get::<String>(6)?,
                        row.get::<String>(7)?,
                    ))
                },
            )
            .optional()
            .map_err(|_| op_error())?
            .ok_or_else(|| Error::not_found("workflow run not found"))?;
        let boundary_seq: i64 = transaction
            .query_row(
                "SELECT seq FROM workflow_events
                 WHERE run_id = ?1 AND step_id = ?2
                   AND kind IN ('step_succeeded','approval_approved')
                 ORDER BY seq DESC LIMIT 1",
                params![source_run_id, after_step],
                |row| row.get(0),
            )
            .optional()
            .map_err(|_| op_error())?
            .ok_or_else(|| Error::conflict("workflow fork boundary is not a committed success"))?;
        let definition: Definition = serde_json::from_str(&source.1).map_err(|_| op_error())?;
        let copied = copied_steps_at(&transaction, source_run_id, boundary_seq)?;
        let now = timestamp();
        transaction
            .execute(
                "INSERT INTO workflow_runs (
                    id, workflow, definition_json, definition_hash, workspace,
                    profile, provider, model, input, forked_from_run_id,
                    forked_from_event_seq, forked_from_step_id, status, created_at, updated_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                    'running', ?13, ?13)",
                params![
                    id,
                    source.0,
                    source.1,
                    source.2,
                    source.3,
                    source.4,
                    source.5,
                    source.6,
                    source.7,
                    source_run_id,
                    boundary_seq,
                    after_step,
                    now,
                ],
            )
            .map_err(|_| op_error())?;
        event(&transaction, id, "run_forked", after_step, "running")?;
        for (position, step) in definition.steps.iter().enumerate() {
            let copied_step = copied.get(step.id.as_str());
            let status = if copied_step.is_some() {
                StepStatus::Succeeded
            } else if step
                .needs
                .iter()
                .all(|dependency| copied.contains_key(dependency.as_str()))
            {
                StepStatus::Ready
            } else {
                StepStatus::Pending
            };
            transaction
                .execute(
                    "INSERT INTO workflow_steps (
                        run_id, id, position, kind, agent, prompt, needs_json, status,
                        attempt, result, transcript_path, source_run_id, source_step_id,
                        source_attempt
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11,
                        ?12, ?13, ?14)",
                    params![
                        id,
                        step.id,
                        position as i64,
                        step.kind.as_str(),
                        step.agent,
                        step.prompt,
                        serde_json::to_string(&step.needs).map_err(|_| op_error())?,
                        status.as_str(),
                        copied_step.map(|step| step.attempt).unwrap_or(0),
                        copied_step.map(|step| step.result.as_str()).unwrap_or(""),
                        copied_step
                            .map(|step| step.transcript_path.as_str())
                            .unwrap_or(""),
                        copied_step.map(|_| source_run_id),
                        copied_step.map(|_| step.id.as_str()),
                        copied_step.map(|step| step.attempt),
                    ],
                )
                .map_err(|_| op_error())?;
            if let Some(copied_step) = copied_step {
                let kind = match step.kind {
                    StepKind::Approval => "approval_approved",
                    StepKind::Agent | StepKind::Handoff => "step_succeeded",
                };
                event(&transaction, id, kind, &step.id, "succeeded")?;
                if copied_step.seq == boundary_seq {
                    event(&transaction, id, "fork_boundary", &step.id, "succeeded")?;
                }
            } else if status == StepStatus::Ready {
                event(&transaction, id, "step_ready", &step.id, status.as_str())?;
            }
        }
        settle_run(&transaction, id)?;
        transaction.commit().map_err(|_| op_error())?;
        drop(connection);
        self.get_run(id)?.ok_or_else(op_error)
    }

    fn claim_ready(
        &self,
        run_id: &str,
        step_id: &str,
        transcript_path: &str,
    ) -> Result<u32, Error> {
        let mut connection = self.connection.lock().map_err(|_| op_error())?;
        let transaction = connection.transaction().map_err(|_| op_error())?;
        let current = transaction
            .query_row(
                "SELECT status, attempt FROM workflow_steps WHERE run_id = ?1 AND id = ?2",
                params![run_id, step_id],
                |row| Ok((row.get::<String>(0)?, row.get::<i64>(1)?)),
            )
            .optional()
            .map_err(|_| op_error())?
            .ok_or_else(|| Error::not_found("workflow step not found"))?;
        if current.0 != "ready" {
            return Err(Error::conflict("workflow step is not ready"));
        }
        let attempt = current.1.checked_add(1).ok_or_else(op_error)?;
        let now = timestamp();
        transaction
            .execute(
                "UPDATE workflow_steps SET status = 'running', attempt = ?3,
                    transcript_path = ?4, result = '', error = ''
                 WHERE run_id = ?1 AND id = ?2",
                params![run_id, step_id, attempt, transcript_path],
            )
            .map_err(|_| op_error())?;
        transaction
            .execute(
                "INSERT INTO workflow_attempts (
                    run_id, step_id, attempt, status, transcript_path, started_at
                 ) VALUES (?1, ?2, ?3, 'running', ?4, ?5)",
                params![run_id, step_id, attempt, transcript_path, now],
            )
            .map_err(|_| op_error())?;
        event(&transaction, run_id, "step_started", step_id, "running")?;
        transaction.commit().map_err(|_| op_error())?;
        u32::try_from(attempt).map_err(|_| op_error())
    }

    fn finish_attempt(
        &self,
        run_id: &str,
        step_id: &str,
        attempt: u32,
        result: Result<&str, &str>,
    ) -> Result<(), Error> {
        let mut connection = self.connection.lock().map_err(|_| op_error())?;
        let transaction = connection.transaction().map_err(|_| op_error())?;
        let matches: i64 = transaction
            .query_row(
                "SELECT COUNT(*) FROM workflow_steps
                 WHERE run_id = ?1 AND id = ?2 AND status = 'running' AND attempt = ?3",
                params![run_id, step_id, attempt],
                |row| row.get(0),
            )
            .map_err(|_| op_error())?;
        if matches != 1 {
            return Err(Error::conflict("workflow attempt is not running"));
        }
        let now = timestamp();
        match result {
            Ok(result) => {
                let result = bounded(result);
                transaction
                    .execute(
                        "UPDATE workflow_attempts SET status = 'succeeded', result = ?4,
                            finished_at = ?5
                         WHERE run_id = ?1 AND step_id = ?2 AND attempt = ?3",
                        params![run_id, step_id, attempt, result.as_ref(), now],
                    )
                    .map_err(|_| op_error())?;
                transaction
                    .execute(
                        "UPDATE workflow_steps SET status = 'succeeded', result = ?3
                         WHERE run_id = ?1 AND id = ?2",
                        params![run_id, step_id, result.as_ref()],
                    )
                    .map_err(|_| op_error())?;
                event(&transaction, run_id, "step_succeeded", step_id, "succeeded")?;
                advance_ready(&transaction, run_id)?;
                settle_run(&transaction, run_id)?;
            }
            Err(error) => {
                transaction
                    .execute(
                        "UPDATE workflow_attempts SET status = 'failed', error = ?4,
                            finished_at = ?5
                         WHERE run_id = ?1 AND step_id = ?2 AND attempt = ?3",
                        params![run_id, step_id, attempt, error, now],
                    )
                    .map_err(|_| op_error())?;
                transaction
                    .execute(
                        "UPDATE workflow_steps SET status = 'failed', error = ?3
                         WHERE run_id = ?1 AND id = ?2",
                        params![run_id, step_id, error],
                    )
                    .map_err(|_| op_error())?;
                transaction
                    .execute(
                        "UPDATE workflow_steps SET status = 'canceled'
                         WHERE run_id = ?1 AND status IN ('pending','ready','waiting')",
                        [run_id],
                    )
                    .map_err(|_| op_error())?;
                event(&transaction, run_id, "step_failed", step_id, "failed")?;
                // A failure is durable the moment it commits: settle the run here
                // so a crash before the driver's next observation cannot leave it
                // `running` with no owner.
                settle_run(&transaction, run_id)?;
            }
        }
        transaction.commit().map_err(|_| op_error())
    }

    fn interrupt_attempt(
        &self,
        run_id: &str,
        step_id: &str,
        attempt: u32,
        error: &str,
    ) -> Result<(), Error> {
        let mut connection = self.connection.lock().map_err(|_| op_error())?;
        let transaction = connection.transaction().map_err(|_| op_error())?;
        let now = timestamp();
        let changed = transaction
            .execute(
                "UPDATE workflow_attempts SET status = 'interrupted', error = ?4,
                    finished_at = ?5
                 WHERE run_id = ?1 AND step_id = ?2 AND attempt = ?3 AND status = 'running'",
                params![run_id, step_id, attempt, error, now],
            )
            .map_err(|_| op_error())?;
        if changed != 1 {
            return Err(Error::conflict("workflow attempt is not running"));
        }
        transaction
            .execute(
                "UPDATE workflow_steps SET status = 'interrupted', error = ?4
                 WHERE run_id = ?1 AND id = ?2 AND attempt = ?3 AND status = 'running'",
                params![run_id, step_id, attempt, error],
            )
            .map_err(|_| op_error())?;
        event(
            &transaction,
            run_id,
            "step_interrupted",
            step_id,
            "interrupted",
        )?;
        // Keep the run non-retryable until every parallel owner has settled.
        settle_run(&transaction, run_id)?;
        transaction.commit().map_err(|_| op_error())
    }

    fn recover(&self, workspace: &str) -> Result<usize, Error> {
        let mut connection = self.connection.lock().map_err(|_| op_error())?;
        let transaction = connection.transaction().map_err(|_| op_error())?;
        let running = {
            let mut statement = transaction
                .prepare(
                    "SELECT s.run_id, s.id, s.attempt
                     FROM workflow_steps s
                     JOIN workflow_runs r ON r.id = s.run_id
                     WHERE s.status = 'running' AND r.workspace = ?1",
                )
                .map_err(|_| op_error())?;
            let rows = statement
                .query_map([workspace], |row| {
                    Ok((
                        row.get::<String>(0)?,
                        row.get::<String>(1)?,
                        row.get::<i64>(2)?,
                    ))
                })
                .map_err(|_| op_error())?;
            rows.collect::<crate::storage::Result<Vec<_>>>()
                .map_err(|_| op_error())?
        };
        for (run_id, step_id, attempt) in &running {
            transaction
                .execute(
                    "UPDATE workflow_steps SET status = 'interrupted'
                     WHERE run_id = ?1 AND id = ?2",
                    params![run_id, step_id],
                )
                .map_err(|_| op_error())?;
            transaction
                .execute(
                    "UPDATE workflow_attempts SET status = 'interrupted', finished_at = ?4
                     WHERE run_id = ?1 AND step_id = ?2 AND attempt = ?3",
                    params![run_id, step_id, attempt, timestamp()],
                )
                .map_err(|_| op_error())?;
            event(
                &transaction,
                run_id,
                "step_interrupted",
                step_id,
                "interrupted",
            )?;
        }
        // Settle every unfinished run in this workspace. A run whose steps all
        // settled before the process died must reach its durable outcome instead
        // of staying `running` with no owner.
        let unfinished = {
            let mut statement = transaction
                .prepare(
                    "SELECT id FROM workflow_runs
                     WHERE workspace = ?1
                       AND status NOT IN ('succeeded','failed','canceled')",
                )
                .map_err(|_| op_error())?;
            let rows = statement
                .query_map([workspace], |row| row.get::<String>(0))
                .map_err(|_| op_error())?;
            rows.collect::<crate::storage::Result<Vec<_>>>()
                .map_err(|_| op_error())?
        };
        for run_id in &unfinished {
            settle_run(&transaction, run_id)?;
        }
        transaction.commit().map_err(|_| op_error())?;
        Ok(running.len())
    }

    fn pause_run(&self, run_id: &str, error: &str) -> Result<(), Error> {
        let mut connection = self.connection.lock().map_err(|_| op_error())?;
        let transaction = connection.transaction().map_err(|_| op_error())?;
        let status: String = transaction
            .query_row(
                "SELECT status FROM workflow_runs WHERE id = ?1",
                [run_id],
                |row| row.get(0),
            )
            .map_err(|_| op_error())?;
        if !matches!(status.as_str(), "running" | "waiting") {
            return Ok(());
        }
        let attempts = {
            let mut statement = transaction
                .prepare(
                    "SELECT id, attempt FROM workflow_steps
                     WHERE run_id = ?1 AND attempt > 0 AND status IN ('running','canceled')",
                )
                .map_err(|_| op_error())?;
            let rows = statement
                .query_map([run_id], |row| {
                    Ok((row.get::<String>(0)?, row.get::<i64>(1)?))
                })
                .map_err(|_| op_error())?;
            rows.collect::<crate::storage::Result<Vec<_>>>()
                .map_err(|_| op_error())?
        };
        for (step_id, attempt) in attempts {
            transaction
                .execute(
                    "UPDATE workflow_steps SET status = 'interrupted', error = ?3
                     WHERE run_id = ?1 AND id = ?2 AND status IN ('running','canceled')",
                    params![run_id, step_id, error],
                )
                .map_err(|_| op_error())?;
            transaction
                .execute(
                    "UPDATE workflow_attempts SET status = 'interrupted', finished_at = ?4
                     WHERE run_id = ?1 AND step_id = ?2 AND attempt = ?3
                       AND status IN ('running','canceled')",
                    params![run_id, step_id, attempt, timestamp()],
                )
                .map_err(|_| op_error())?;
            event(
                &transaction,
                run_id,
                "step_interrupted",
                &step_id,
                "interrupted",
            )?;
        }
        transaction
            .execute(
                "UPDATE workflow_runs SET status = 'paused', updated_at = ?2
                 WHERE id = ?1 AND status IN ('running','waiting')",
                params![run_id, timestamp()],
            )
            .map_err(|_| op_error())?;
        event(&transaction, run_id, "run_paused", "", "paused")?;
        transaction.commit().map_err(|_| op_error())
    }

    fn request_approval(
        &self,
        request_id: &str,
        run_id: &str,
        step_id: &str,
    ) -> Result<ApprovalRequest, Error> {
        let mut connection = self.connection.lock().map_err(|_| op_error())?;
        let transaction = connection.transaction().map_err(|_| op_error())?;
        let row = transaction
            .query_row(
                "SELECT kind, status, prompt FROM workflow_steps WHERE run_id = ?1 AND id = ?2",
                params![run_id, step_id],
                |row| {
                    Ok((
                        row.get::<String>(0)?,
                        row.get::<String>(1)?,
                        row.get::<String>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(|_| op_error())?
            .ok_or_else(|| Error::not_found("workflow step not found"))?;
        if row.0 != "approval" || row.1 != "ready" {
            return Err(Error::conflict("workflow approval step is not ready"));
        }
        let now = timestamp();
        transaction
            .execute(
                "UPDATE workflow_steps SET status = 'waiting' WHERE run_id = ?1 AND id = ?2",
                params![run_id, step_id],
            )
            .map_err(|_| op_error())?;
        // `workflow_requests` is unique per (run, step): re-asking a gate keeps a
        // still-pending request id, and a re-asked answered gate takes a fresh id
        // so the approval service presents it again.
        let existing = transaction
            .query_row(
                "SELECT id, status FROM workflow_requests
                 WHERE run_id = ?1 AND step_id = ?2",
                params![run_id, step_id],
                |row| Ok((row.get::<String>(0)?, row.get::<String>(1)?)),
            )
            .optional()
            .map_err(|_| op_error())?;
        let request_id = match existing {
            Some((existing_id, status)) if status == "pending" => existing_id,
            Some((existing_id, _)) => {
                transaction
                    .execute(
                        "UPDATE workflow_requests SET id = ?2, prompt = ?3,
                            status = 'pending', responded_at = NULL
                         WHERE id = ?1",
                        params![existing_id, request_id, row.2],
                    )
                    .map_err(|_| op_error())?;
                request_id.to_string()
            }
            None => {
                transaction
                    .execute(
                        "INSERT INTO workflow_requests (
                            id, run_id, step_id, prompt, status, created_at
                         ) VALUES (?1, ?2, ?3, ?4, 'pending', ?5)",
                        params![request_id, run_id, step_id, row.2, now],
                    )
                    .map_err(|_| op_error())?;
                request_id.to_string()
            }
        };
        event(
            &transaction,
            run_id,
            "approval_requested",
            step_id,
            "waiting",
        )?;
        settle_run(&transaction, run_id)?;
        transaction.commit().map_err(|_| op_error())?;
        Ok(ApprovalRequest {
            id: request_id,
            run_id: run_id.to_string(),
            step_id: step_id.to_string(),
            prompt: row.2,
            status: "pending".to_string(),
        })
    }

    fn respond(&self, request_id: &str, approved: bool) -> Result<String, Error> {
        let mut connection = self.connection.lock().map_err(|_| op_error())?;
        let transaction = connection.transaction().map_err(|_| op_error())?;
        let row = transaction
            .query_row(
                "SELECT run_id, step_id, status FROM workflow_requests WHERE id = ?1",
                [request_id],
                |row| {
                    Ok((
                        row.get::<String>(0)?,
                        row.get::<String>(1)?,
                        row.get::<String>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(|_| op_error())?
            .ok_or_else(|| Error::not_found("approval request not found"))?;
        let wanted = if approved { "approved" } else { "rejected" };
        if row.2 == wanted {
            return Ok(row.0);
        }
        if row.2 != "pending" {
            return Err(Error::conflict(
                "approval request already answered differently",
            ));
        }
        let now = timestamp();
        transaction
            .execute(
                "UPDATE workflow_requests SET status = ?2, responded_at = ?3 WHERE id = ?1",
                params![request_id, wanted, now],
            )
            .map_err(|_| op_error())?;
        if approved {
            transaction
                .execute(
                    "UPDATE workflow_steps SET status = 'succeeded', result = 'approved'
                     WHERE run_id = ?1 AND id = ?2 AND status = 'waiting'",
                    params![row.0, row.1],
                )
                .map_err(|_| op_error())?;
            event(
                &transaction,
                &row.0,
                "approval_approved",
                &row.1,
                "succeeded",
            )?;
            advance_ready(&transaction, &row.0)?;
            settle_run(&transaction, &row.0)?;
        } else {
            transaction
                .execute(
                    "UPDATE workflow_steps SET status = 'canceled'
                     WHERE run_id = ?1 AND status NOT IN ('succeeded','failed','canceled')",
                    [row.0.as_str()],
                )
                .map_err(|_| op_error())?;
            transaction
                .execute(
                    "UPDATE workflow_attempts SET status = 'canceled', finished_at = ?2
                     WHERE run_id = ?1 AND status = 'running'",
                    params![row.0, now],
                )
                .map_err(|_| op_error())?;
            transaction
                .execute(
                    "UPDATE workflow_requests SET status = 'canceled', responded_at = ?2
                     WHERE run_id = ?1 AND status = 'pending'",
                    params![row.0, now],
                )
                .map_err(|_| op_error())?;
            transaction
                .execute(
                    "UPDATE workflow_runs SET status = 'canceled', updated_at = ?2 WHERE id = ?1",
                    params![row.0, now],
                )
                .map_err(|_| op_error())?;
            event(
                &transaction,
                &row.0,
                "approval_rejected",
                &row.1,
                "canceled",
            )?;
            event(&transaction, &row.0, "run_canceled", "", "canceled")?;
        }
        transaction.commit().map_err(|_| op_error())?;
        Ok(row.0)
    }

    fn settle(&self, run_id: &str) -> Result<(), Error> {
        let mut connection = self.connection.lock().map_err(|_| op_error())?;
        let transaction = connection.transaction().map_err(|_| op_error())?;
        settle_run(&transaction, run_id)?;
        transaction.commit().map_err(|_| op_error())
    }

    fn retry(&self, run_id: &str, step_id: &str) -> Result<(), Error> {
        let mut connection = self.connection.lock().map_err(|_| op_error())?;
        let transaction = connection.transaction().map_err(|_| op_error())?;
        let run_status: String = transaction
            .query_row(
                "SELECT status FROM workflow_runs WHERE id = ?1",
                [run_id],
                |row| row.get(0),
            )
            .map_err(|_| op_error())?;
        if !matches!(run_status.as_str(), "paused" | "failed") {
            return Err(Error::conflict("workflow run is not paused or failed"));
        }
        let changed = transaction
            .execute(
                "UPDATE workflow_steps SET status = 'ready', result = '', error = ''
                 WHERE run_id = ?1 AND id = ?2 AND status IN ('failed','interrupted')",
                params![run_id, step_id],
            )
            .map_err(|_| op_error())?;
        if changed != 1 {
            return Err(Error::conflict("workflow step is not retryable"));
        }
        // Siblings canceled by the failure never ran to completion, so a retry
        // re-admits them as pending; `advance_ready` schedules them again once
        // their dependencies succeed.
        let revived = {
            let mut statement = transaction
                .prepare(
                    "SELECT id FROM workflow_steps
                     WHERE run_id = ?1 AND status = 'canceled' ORDER BY position",
                )
                .map_err(|_| op_error())?;
            let rows = statement
                .query_map([run_id], |row| row.get::<String>(0))
                .map_err(|_| op_error())?;
            rows.collect::<crate::storage::Result<Vec<_>>>()
                .map_err(|_| op_error())?
        };
        if !revived.is_empty() {
            transaction
                .execute(
                    "UPDATE workflow_steps SET status = 'pending', result = '', error = ''
                     WHERE run_id = ?1 AND status = 'canceled'",
                    [run_id],
                )
                .map_err(|_| op_error())?;
        }
        transaction
            .execute(
                "UPDATE workflow_runs SET status = 'running', updated_at = ?2
                 WHERE id = ?1 AND status IN ('paused','failed')",
                params![run_id, timestamp()],
            )
            .map_err(|_| op_error())?;
        for revived_id in &revived {
            event(&transaction, run_id, "step_pending", revived_id, "pending")?;
        }
        event(&transaction, run_id, "step_ready", step_id, "ready")?;
        event(&transaction, run_id, "run_resumed", "", "running")?;
        advance_ready(&transaction, run_id)?;
        settle_run(&transaction, run_id)?;
        transaction.commit().map_err(|_| op_error())
    }

    fn cancel_attempt(&self, run_id: &str, step_id: &str, attempt: u32) -> Result<(), Error> {
        let mut connection = self.connection.lock().map_err(|_| op_error())?;
        let transaction = connection.transaction().map_err(|_| op_error())?;
        let now = timestamp();
        transaction
            .execute(
                "UPDATE workflow_attempts SET status = 'canceled', finished_at = ?4
                 WHERE run_id = ?1 AND step_id = ?2 AND attempt = ?3 AND status = 'running'",
                params![run_id, step_id, attempt, now],
            )
            .map_err(|_| op_error())?;
        transaction
            .execute(
                "UPDATE workflow_steps SET status = 'canceled'
                 WHERE run_id = ?1 AND id = ?2 AND status = 'running' AND attempt = ?3",
                params![run_id, step_id, attempt],
            )
            .map_err(|_| op_error())?;
        event(&transaction, run_id, "step_canceled", step_id, "canceled")?;
        // The caller that canceled the run records the run status; settling here
        // would race it into `canceled` before `cancel_run` can commit.
        transaction.commit().map_err(|_| op_error())
    }

    fn cancel_run(&self, run_id: &str) -> Result<(), Error> {
        let mut connection = self.connection.lock().map_err(|_| op_error())?;
        let transaction = connection.transaction().map_err(|_| op_error())?;
        let status: Option<String> = transaction
            .query_row(
                "SELECT status FROM workflow_runs WHERE id = ?1",
                [run_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|_| op_error())?;
        let Some(status) = status else {
            return Err(Error::not_found("workflow run not found"));
        };
        if matches!(status.as_str(), "succeeded" | "failed" | "canceled") {
            return Err(Error::conflict("workflow run already finished"));
        }
        transaction
            .execute(
                "UPDATE workflow_steps SET status = 'canceled'
                 WHERE run_id = ?1 AND status NOT IN ('succeeded','failed','canceled')",
                [run_id],
            )
            .map_err(|_| op_error())?;
        transaction
            .execute(
                "UPDATE workflow_requests SET status = 'canceled', responded_at = ?2
                 WHERE run_id = ?1 AND status = 'pending'",
                params![run_id, timestamp()],
            )
            .map_err(|_| op_error())?;
        transaction
            .execute(
                "UPDATE workflow_attempts SET status = 'canceled', finished_at = ?2
                 WHERE run_id = ?1 AND status = 'running'",
                params![run_id, timestamp()],
            )
            .map_err(|_| op_error())?;
        transaction
            .execute(
                "UPDATE workflow_runs SET status = 'canceled', updated_at = ?2 WHERE id = ?1",
                params![run_id, timestamp()],
            )
            .map_err(|_| op_error())?;
        event(&transaction, run_id, "run_canceled", "", "canceled")?;
        transaction.commit().map_err(|_| op_error())
    }

    fn list_runs(&self, workspace: &str, query: &ListQuery) -> Result<Vec<Run>, Error> {
        let connection = self.connection.lock().map_err(|_| op_error())?;
        // One query per page: run headers and their steps are joined so a page
        // does not cost a read per run. `LIMIT -1` means "no bound".
        let mut statement = connection
            .prepare(
                "SELECT r.id, r.workflow, r.workspace, r.profile, r.provider, r.model,
                        r.input, r.forked_from_run_id, r.forked_from_event_seq,
                        r.forked_from_step_id, r.status, r.definition_json, r.created_at,
                        s.id, s.kind, s.agent, s.prompt, s.needs_json, s.status, s.attempt,
                        s.result, s.error, s.transcript_path, s.source_run_id,
                        s.source_step_id, s.source_attempt
                 FROM (
                     SELECT id, workflow, workspace, profile, provider, model, input,
                            forked_from_run_id, forked_from_event_seq,
                            forked_from_step_id, status, definition_json, created_at
                     FROM workflow_runs
                     WHERE workspace = ?1 AND (?2 IS NULL OR created_at < ?2)
                     ORDER BY created_at DESC, id DESC
                     LIMIT ?3
                 ) r
                 JOIN workflow_steps s ON s.run_id = r.id
                 ORDER BY r.created_at DESC, r.id DESC, s.position",
            )
            .map_err(|_| op_error())?;
        let limit = i64::try_from(query.limit.unwrap_or(0))
            .map(|limit| if limit == 0 { -1 } else { limit })
            .map_err(|_| op_error())?;
        let rows = statement
            .query_map(params![workspace, query.before, limit], |row| {
                Ok((raw_run(row, 0)?, raw_step(row, RUN_COLUMNS)?))
            })
            .map_err(|_| op_error())?;
        let mut pages: Vec<(RawRun, Vec<StepRecord>)> = Vec::new();
        for row in rows {
            let (run, step) = row.map_err(|_| op_error())?;
            let step = step.decode()?;
            match pages.last_mut() {
                Some((last, steps)) if last.id == run.id => steps.push(step),
                _ => pages.push((run, vec![step])),
            }
        }
        let mut runs = Vec::with_capacity(pages.len());
        for (run, steps) in pages {
            runs.push(run.decode(steps)?);
        }
        Ok(runs)
    }

    fn requests(&self, run_id: &str) -> Result<Vec<ApprovalRequest>, Error> {
        let connection = self.connection.lock().map_err(|_| op_error())?;
        let mut statement = connection
            .prepare(
                "SELECT id, step_id, prompt, status FROM workflow_requests
                 WHERE run_id = ?1 ORDER BY created_at",
            )
            .map_err(|_| op_error())?;
        let rows = statement
            .query_map([run_id], |row| {
                Ok(ApprovalRequest {
                    id: row.get(0)?,
                    run_id: run_id.to_string(),
                    step_id: row.get(1)?,
                    prompt: row.get(2)?,
                    status: row.get(3)?,
                })
            })
            .map_err(|_| op_error())?;
        rows.collect::<crate::storage::Result<Vec<_>>>()
            .map_err(|_| op_error())
    }

    fn request(&self, request_id: &str) -> Result<Option<ApprovalRequest>, Error> {
        self.connection
            .lock()
            .map_err(|_| op_error())?
            .query_row(
                "SELECT run_id, step_id, prompt, status FROM workflow_requests WHERE id = ?1",
                [request_id],
                |row| {
                    Ok(ApprovalRequest {
                        id: request_id.to_string(),
                        run_id: row.get(0)?,
                        step_id: row.get(1)?,
                        prompt: row.get(2)?,
                        status: row.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(|_| op_error())
    }

    /// Milliseconds from the run's `created_at` to now, for the run-finished
    /// event. `None` when the row or its timestamp is unavailable.
    fn run_age_ms(&self, run_id: &str) -> Option<u128> {
        let connection = self.connection.lock().ok()?;
        let created: String = connection
            .query_row(
                "SELECT created_at FROM workflow_runs WHERE id = ?1",
                [run_id],
                |row| row.get(0),
            )
            .ok()?;
        let created = chrono::DateTime::parse_from_rfc3339(&created).ok()?;
        u128::try_from((Utc::now() - created.with_timezone(&Utc)).num_milliseconds()).ok()
    }

    fn events(&self, run_id: &str, after: i64) -> Result<Vec<EventRecord>, Error> {
        let connection = self.connection.lock().map_err(|_| op_error())?;
        let mut statement = connection
            .prepare(
                "SELECT seq, occurred_at, kind, step_id, status FROM workflow_events
                 WHERE run_id = ?1 AND seq > ?2 ORDER BY seq",
            )
            .map_err(|_| op_error())?;
        let rows = statement
            .query_map(params![run_id, after], |row| {
                Ok(EventRecord {
                    seq: row.get(0)?,
                    run_id: run_id.to_string(),
                    occurred_at: row.get(1)?,
                    kind: row.get(2)?,
                    step_id: row.get(3)?,
                    status: row.get(4)?,
                })
            })
            .map_err(|_| op_error())?;
        rows.collect::<crate::storage::Result<Vec<_>>>()
            .map_err(|_| op_error())
    }
}

pub(super) fn migrate_schema(connection: &Connection) -> Result<(), String> {
    // One-time legacy backfill, never repeated after acknowledgement or on reopen.
    // Import initializes an empty target first; logical imports do not execute event writers.
    connection.execute_batch("BEGIN IMMEDIATE;
        INSERT OR IGNORE INTO workflow_pending(workspace,session_id,seq)
        SELECT o.workspace,o.session_id,e.seq FROM workflow_origins o JOIN workflow_events e ON e.run_id=o.run_id
        WHERE NOT EXISTS(SELECT 1 FROM workflow_delivery_schema WHERE version=1);
        INSERT OR IGNORE INTO workflow_delivery_schema(version) VALUES(1);
        COMMIT;").map_err(|_| unavailable())?;
    for (table, column, definition) in [
        ("workflow_runs", "forked_from_run_id", "TEXT"),
        ("workflow_runs", "forked_from_event_seq", "INTEGER"),
        ("workflow_runs", "forked_from_step_id", "TEXT"),
        ("workflow_steps", "source_run_id", "TEXT"),
        ("workflow_steps", "source_step_id", "TEXT"),
        ("workflow_steps", "source_attempt", "INTEGER"),
    ] {
        if !column_exists(connection, table, column)? {
            connection
                .execute(
                    &format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"),
                    (),
                )
                .map_err(|_| unavailable())?;
        }
    }
    Ok(())
}

fn column_exists(connection: &Connection, table: &str, column: &str) -> Result<bool, String> {
    let mut statement = connection
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|_| unavailable())?;
    let rows = statement
        .query_map((), |row| row.get::<String>(1))
        .map_err(|_| unavailable())?;
    for name in rows {
        if name.map_err(|_| unavailable())? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

impl StepKind {
    fn parse(value: &str) -> Result<Self, Error> {
        match value {
            "agent" => Ok(Self::Agent),
            "handoff" => Ok(Self::Handoff),
            "approval" => Ok(Self::Approval),
            _ => Err(corrupt()),
        }
    }
}

impl RunStatus {
    fn parse(value: &str) -> Result<Self, Error> {
        match value {
            "running" => Ok(Self::Running),
            "waiting" => Ok(Self::Waiting),
            "paused" => Ok(Self::Paused),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            "canceled" => Ok(Self::Canceled),
            _ => Err(corrupt()),
        }
    }
}

impl StepStatus {
    fn parse(value: &str) -> Result<Self, Error> {
        match value {
            "pending" => Ok(Self::Pending),
            "ready" => Ok(Self::Ready),
            "running" => Ok(Self::Running),
            "waiting" => Ok(Self::Waiting),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            "canceled" => Ok(Self::Canceled),
            "interrupted" => Ok(Self::Interrupted),
            _ => Err(corrupt()),
        }
    }
}

fn advance_ready(transaction: &Transaction<'_>, run_id: &str) -> Result<(), Error> {
    let run_status: String = transaction
        .query_row(
            "SELECT status FROM workflow_runs WHERE id = ?1",
            [run_id],
            |row| row.get(0),
        )
        .map_err(|_| op_error())?;
    if !matches!(run_status.as_str(), "running" | "waiting") {
        return Ok(());
    }
    let pending = {
        let mut statement = transaction
            .prepare(
                "SELECT id, needs_json FROM workflow_steps
                 WHERE run_id = ?1 AND status = 'pending' ORDER BY position",
            )
            .map_err(|_| op_error())?;
        let rows = statement
            .query_map([run_id], |row| {
                Ok((row.get::<String>(0)?, row.get::<String>(1)?))
            })
            .map_err(|_| op_error())?;
        rows.collect::<crate::storage::Result<Vec<_>>>()
            .map_err(|_| op_error())?
    };
    for (step_id, encoded) in pending {
        let needs: Vec<String> = serde_json::from_str(&encoded).map_err(|_| op_error())?;
        let mut ready = true;
        for dependency in needs {
            let status: String = transaction
                .query_row(
                    "SELECT status FROM workflow_steps WHERE run_id = ?1 AND id = ?2",
                    params![run_id, dependency],
                    |row| row.get(0),
                )
                .map_err(|_| op_error())?;
            if status != "succeeded" {
                ready = false;
                break;
            }
        }
        if ready {
            transaction
                .execute(
                    "UPDATE workflow_steps SET status = 'ready' WHERE run_id = ?1 AND id = ?2",
                    params![run_id, step_id],
                )
                .map_err(|_| op_error())?;
            event(transaction, run_id, "step_ready", &step_id, "ready")?;
        }
    }
    Ok(())
}

fn copied_steps_at(
    transaction: &Transaction<'_>,
    run_id: &str,
    boundary_seq: i64,
) -> Result<HashMap<String, CopiedStep>, Error> {
    let mut statement = transaction
        .prepare(
            "SELECT e.step_id, e.seq, s.attempt, s.result, s.transcript_path
             FROM workflow_events e
             JOIN workflow_steps s ON s.run_id = e.run_id AND s.id = e.step_id
             WHERE e.run_id = ?1 AND e.seq <= ?2
               AND e.kind IN ('step_succeeded','approval_approved')
             ORDER BY e.seq",
        )
        .map_err(|_| op_error())?;
    let rows = statement
        .query_map(params![run_id, boundary_seq], |row| {
            Ok((
                row.get::<String>(0)?,
                CopiedStep {
                    seq: row.get(1)?,
                    attempt: row.get(2)?,
                    result: row.get(3)?,
                    transcript_path: row.get(4)?,
                },
            ))
        })
        .map_err(|_| op_error())?;
    rows.collect::<crate::storage::Result<HashMap<_, _>>>()
        .map_err(|_| op_error())
}

/// Derives the run status from its steps and writes it: the single authority
/// over run lifecycle. Every step transition settles, so a run can never be
/// left `running` without an owner, and a status that does not change writes no
/// second event.
fn settle_run(transaction: &Transaction<'_>, run_id: &str) -> Result<(), Error> {
    let status: String = transaction
        .query_row(
            "SELECT status FROM workflow_runs WHERE id = ?1",
            [run_id],
            |row| row.get(0),
        )
        .map_err(|_| op_error())?;
    // A canceled run is owned by the caller that canceled it; terminal runs are
    // final.
    if matches!(status.as_str(), "succeeded" | "failed" | "canceled") {
        return Ok(());
    }
    let (unfinished, active, waiting, interrupted, failed): (i64, i64, i64, i64, i64) = transaction
        .query_row(
            "SELECT
                    SUM(CASE WHEN status != 'succeeded' THEN 1 ELSE 0 END),
                    SUM(CASE WHEN status IN ('ready','running') THEN 1 ELSE 0 END),
                    SUM(CASE WHEN status = 'waiting' THEN 1 ELSE 0 END),
                    SUM(CASE WHEN status = 'interrupted' THEN 1 ELSE 0 END),
                    SUM(CASE WHEN status = 'failed' THEN 1 ELSE 0 END)
                 FROM workflow_steps WHERE run_id = ?1",
            [run_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .map_err(|_| op_error())?;
    let (next, kind, event_status) = if unfinished == 0 {
        ("succeeded", "run_succeeded", "succeeded")
    } else if active > 0 {
        ("running", "run_resumed", "running")
    } else if waiting > 0 {
        ("waiting", "run_waiting", "waiting")
    } else if interrupted > 0 {
        ("paused", "run_paused", "paused")
    } else if failed > 0 {
        ("failed", "run_failed", "failed")
    } else {
        // Only canceled steps remain: nothing failed and nothing can advance
        // them, so the run reaches its durable cancellation instead of staying
        // `running` with no owner.
        ("canceled", "run_canceled", "canceled")
    };
    if next == status {
        return Ok(());
    }
    transaction
        .execute(
            "UPDATE workflow_runs SET status = ?2, updated_at = ?3 WHERE id = ?1",
            params![run_id, next, timestamp()],
        )
        .map_err(|_| op_error())?;
    event(transaction, run_id, kind, "", event_status)?;
    Ok(())
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
    transaction.execute("INSERT OR IGNORE INTO workflow_pending(workspace,session_id,seq) SELECT o.workspace,o.session_id,e.seq FROM workflow_origins o JOIN workflow_events e ON e.run_id=o.run_id WHERE e.seq=(SELECT MAX(seq) FROM workflow_events) AND e.run_id=?1", params![run_id]).map_err(|_|op_error())?;
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
mod repository_tests {
    use super::*;
    use crate::workflow::repository::WorkflowStore;
    #[test]
    fn legacy_delivery_backfill_is_once_and_idle_plan_uses_pending_index() {
        let store = TursoWorkflowStore::open_in_memory();
        let connection = store.connection.lock().unwrap();
        // Simulate old schema populated before conversational delivery ledger existed.
        connection.execute("INSERT INTO workflow_runs(id,workflow,definition_json,definition_hash,workspace,profile,provider,model,input,status,created_at,updated_at) VALUES ('legacy','flow','{}','hash','/w','','','','','succeeded','now','now')",()).unwrap();
        connection.execute("INSERT INTO workflow_origins(workspace,session_id,operation_id,run_id,definition_hash,input) VALUES('/w','source','op','legacy','hash','')",()).unwrap();
        connection.execute("INSERT INTO workflow_events(seq,run_id,occurred_at,kind) VALUES(1,'legacy','now','run_succeeded')",()).unwrap();
        connection
            .execute("DELETE FROM workflow_delivery_schema", ())
            .unwrap();
        migrate_schema(&connection).unwrap();
        drop(connection);
        assert_eq!(store.pending_events("/w", "source", 64).unwrap()[0].seq, 1);
        store.acknowledge_event("/w", "source", 1).unwrap();
        let connection = store.connection.lock().unwrap();
        migrate_schema(&connection).unwrap();
        let mut query=connection.prepare("EXPLAIN QUERY PLAN SELECT e.seq FROM workflow_pending p JOIN workflow_events e ON e.seq=p.seq WHERE p.workspace='/w' AND p.session_id='source' ORDER BY p.seq LIMIT 64").unwrap();
        let plan = query
            .query_map((), |row| row.get::<String>(3))
            .unwrap()
            .collect::<crate::storage::Result<Vec<_>>>()
            .unwrap()
            .join(";");
        assert!(plan.contains("SEARCH p USING"), "{plan}");
        assert!(
            !plan.contains("SCAN e") && !plan.contains("SCAN o"),
            "idle query must not scan retained origins/events: {plan}"
        );
        drop(query);
        drop(connection);
        assert!(store.pending_events("/w", "source", 64).unwrap().is_empty());
    }

    #[test]
    fn attempt_journal_keeps_each_transcript_path() {
        let store = TursoWorkflowStore::open_in_memory();
        let definition = Definition {
            source: String::new(),
            name: "flow".into(),
            description: String::new(),
            steps: vec![Step {
                id: "work".into(),
                kind: StepKind::Agent,
                agent: "worker".into(),
                prompt: "work".into(),
                needs: vec![],
            }],
            agents: vec![],
            hash: "hash".into(),
        };
        store
            .create_run("run", &definition, "/w", "p", "provider", "m", "")
            .unwrap();
        store
            .claim_ready("run", "work", "/tmp/attempt-1.jsonl")
            .unwrap();
        store.recover("/w").unwrap();
        store.retry("run", "work").unwrap();
        store
            .claim_ready("run", "work", "/tmp/attempt-2.jsonl")
            .unwrap();
        let connection = store.connection.lock().unwrap();
        let mut statement = connection.prepare("SELECT transcript_path FROM workflow_attempts WHERE run_id = 'run' AND step_id = 'work' ORDER BY attempt").unwrap();
        let paths: Vec<String> = statement
            .query_map((), |row| row.get(0))
            .unwrap()
            .collect::<crate::storage::Result<_>>()
            .unwrap();
        assert_eq!(paths, ["/tmp/attempt-1.jsonl", "/tmp/attempt-2.jsonl"]);
    }
}

impl TursoWorkflowStore {
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
            transaction.execute("INSERT INTO workflow_origins(workspace,session_id,operation_id,run_id,definition_hash,input) VALUES (?1,?2,?3,?4,?5,?6)", params![workspace,session,operation,id,definition.hash,input]).map_err(|_| op_error())?;
            transaction.execute("INSERT OR IGNORE INTO workflow_pending(workspace,session_id,seq) SELECT ?1,?2,seq FROM workflow_events WHERE run_id=?3",params![workspace,session,id]).map_err(|_|op_error())?;
        }
        transaction.commit().map_err(|_| op_error())?;
        drop(connection);
        self.get_run(id)?
            .map(|run| (run, true))
            .ok_or_else(op_error)
    }
}
