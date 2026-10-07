//! Turso repository for durable workflow runs.
use crate::storage::drivers::turso::TursoDriver;
use crate::storage::{Connection, OptionalExtension, Transaction, params};
use crate::storage::{OpenOptions, SqlDriver, SqlRequirements};
use crate::workflow::contracts::*;
use crate::workflow::repository::WorkflowStore;
use chrono::{SecondsFormat, Utc};
use std::collections::{HashMap, HashSet};
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
CREATE TABLE IF NOT EXISTS workflow_events (
    seq INTEGER PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES workflow_runs(id),
    occurred_at TEXT NOT NULL,
    kind TEXT NOT NULL,
    step_id TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL DEFAULT ''
) STRICT;
CREATE INDEX IF NOT EXISTS workflow_events_run ON workflow_events(run_id, seq);
"#;

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
    ) -> Result<Run, String> {
        if input.len() > MAX_TEXT_BYTES {
            return Err("workflow input exceeds 65536 UTF-8 bytes".to_string());
        }
        let definition_json = serde_json::to_string(definition).map_err(|_| unavailable())?;
        let now = timestamp();
        let mut connection = self.connection.lock().map_err(|_| unavailable())?;
        let transaction = connection.transaction().map_err(|_| unavailable())?;
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
            .map_err(|_| unavailable())?;
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
                        serde_json::to_string(&step.needs).map_err(|_| unavailable())?,
                        status.as_str(),
                    ],
                )
                .map_err(|_| unavailable())?;
            if status == StepStatus::Ready {
                event(&transaction, id, "step_ready", &step.id, status.as_str())?;
            }
        }
        transaction.commit().map_err(|_| unavailable())?;
        drop(connection);
        self.get_run(id)?.ok_or_else(unavailable)
    }

    fn get_run(&self, id: &str) -> Result<Option<Run>, String> {
        let connection = self.connection.lock().map_err(|_| unavailable())?;
        let header = connection
            .query_row(
                "SELECT workflow, workspace, profile, provider, model, input,
                        forked_from_run_id, forked_from_event_seq, forked_from_step_id,
                        status, definition_json
                 FROM workflow_runs WHERE id = ?1",
                [id],
                |row| {
                    Ok((
                        row.get::<String>(0)?,
                        row.get::<String>(1)?,
                        row.get::<String>(2)?,
                        row.get::<String>(3)?,
                        row.get::<String>(4)?,
                        row.get::<String>(5)?,
                        row.get::<Option<String>>(6)?,
                        row.get::<Option<i64>>(7)?,
                        row.get::<Option<String>>(8)?,
                        row.get::<String>(9)?,
                        row.get::<String>(10)?,
                    ))
                },
            )
            .optional()
            .map_err(|_| unavailable())?;
        let Some((
            workflow,
            workspace,
            profile,
            provider,
            model,
            input,
            forked_from_run_id,
            forked_from_event_seq,
            forked_from_step_id,
            status,
            definition_json,
        )) = header
        else {
            return Ok(None);
        };
        let mut statement = connection
            .prepare(
                "SELECT id, kind, agent, prompt, needs_json, status, attempt,
                        result, error, transcript_path, source_run_id, source_step_id,
                        source_attempt
                 FROM workflow_steps WHERE run_id = ?1 ORDER BY position",
            )
            .map_err(|_| unavailable())?;
        let rows = statement
            .query_map([id], |row| {
                Ok((
                    row.get::<String>(0)?,
                    row.get::<String>(1)?,
                    row.get::<String>(2)?,
                    row.get::<String>(3)?,
                    row.get::<String>(4)?,
                    row.get::<String>(5)?,
                    row.get::<i64>(6)?,
                    row.get::<String>(7)?,
                    row.get::<String>(8)?,
                    row.get::<String>(9)?,
                    row.get::<Option<String>>(10)?,
                    row.get::<Option<String>>(11)?,
                    row.get::<Option<i64>>(12)?,
                ))
            })
            .map_err(|_| unavailable())?;
        let mut steps = Vec::new();
        for row in rows {
            let (
                step_id,
                kind,
                agent,
                prompt,
                needs,
                status,
                attempt,
                result,
                error,
                path,
                source_run_id,
                source_step_id,
                source_attempt,
            ) = row.map_err(|_| unavailable())?;
            steps.push(StepRecord {
                id: step_id,
                kind: StepKind::parse(&kind)?,
                agent,
                prompt,
                needs: serde_json::from_str(&needs).map_err(|_| unavailable())?,
                status: StepStatus::parse(&status)?,
                attempt: u32::try_from(attempt).map_err(|_| unavailable())?,
                result,
                error,
                transcript_path: path,
                source_run_id,
                source_step_id,
                source_attempt: source_attempt
                    .map(u32::try_from)
                    .transpose()
                    .map_err(|_| unavailable())?,
            });
        }
        Ok(Some(Run {
            id: id.to_string(),
            workflow,
            workspace,
            profile,
            provider,
            model,
            input,
            forked_from_run_id,
            forked_from_event_seq,
            forked_from_step_id,
            status: RunStatus::parse(&status)?,
            steps,
            definition: serde_json::from_str(&definition_json).map_err(|_| unavailable())?,
        }))
    }

    fn fork_run(&self, id: &str, source_run_id: &str, after_step: &str) -> Result<Run, String> {
        let mut connection = self.connection.lock().map_err(|_| unavailable())?;
        let transaction = connection.transaction().map_err(|_| unavailable())?;
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
            .map_err(|_| unavailable())?
            .ok_or_else(|| "workflow run not found".to_string())?;
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
            .map_err(|_| unavailable())?
            .ok_or_else(|| "workflow fork boundary is not a committed success".to_string())?;
        let definition: Definition = serde_json::from_str(&source.1).map_err(|_| unavailable())?;
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
            .map_err(|_| unavailable())?;
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
                        serde_json::to_string(&step.needs).map_err(|_| unavailable())?,
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
                .map_err(|_| unavailable())?;
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
        transaction.commit().map_err(|_| unavailable())?;
        drop(connection);
        self.get_run(id)?.ok_or_else(unavailable)
    }

    fn claim_ready(
        &self,
        run_id: &str,
        step_id: &str,
        transcript_path: &str,
    ) -> Result<u32, String> {
        let mut connection = self.connection.lock().map_err(|_| unavailable())?;
        let transaction = connection.transaction().map_err(|_| unavailable())?;
        let current = transaction
            .query_row(
                "SELECT status, attempt FROM workflow_steps WHERE run_id = ?1 AND id = ?2",
                params![run_id, step_id],
                |row| Ok((row.get::<String>(0)?, row.get::<i64>(1)?)),
            )
            .optional()
            .map_err(|_| unavailable())?
            .ok_or_else(|| "workflow step not found".to_string())?;
        if current.0 != "ready" {
            return Err("workflow step is not ready".to_string());
        }
        let attempt = current.1.checked_add(1).ok_or_else(unavailable)?;
        let now = timestamp();
        transaction
            .execute(
                "UPDATE workflow_steps SET status = 'running', attempt = ?3,
                    transcript_path = ?4, result = '', error = ''
                 WHERE run_id = ?1 AND id = ?2",
                params![run_id, step_id, attempt, transcript_path],
            )
            .map_err(|_| unavailable())?;
        transaction
            .execute(
                "INSERT INTO workflow_attempts (
                    run_id, step_id, attempt, status, transcript_path, started_at
                 ) VALUES (?1, ?2, ?3, 'running', ?4, ?5)",
                params![run_id, step_id, attempt, transcript_path, now],
            )
            .map_err(|_| unavailable())?;
        event(&transaction, run_id, "step_started", step_id, "running")?;
        transaction.commit().map_err(|_| unavailable())?;
        u32::try_from(attempt).map_err(|_| unavailable())
    }

    fn finish_attempt(
        &self,
        run_id: &str,
        step_id: &str,
        attempt: u32,
        result: Result<&str, &str>,
    ) -> Result<(), String> {
        let mut connection = self.connection.lock().map_err(|_| unavailable())?;
        let transaction = connection.transaction().map_err(|_| unavailable())?;
        let matches: i64 = transaction
            .query_row(
                "SELECT COUNT(*) FROM workflow_steps
                 WHERE run_id = ?1 AND id = ?2 AND status = 'running' AND attempt = ?3",
                params![run_id, step_id, attempt],
                |row| row.get(0),
            )
            .map_err(|_| unavailable())?;
        if matches != 1 {
            return Err("workflow attempt is not running".to_string());
        }
        let now = timestamp();
        match result {
            Ok(result) => {
                transaction
                    .execute(
                        "UPDATE workflow_attempts SET status = 'succeeded', result = ?4,
                            finished_at = ?5
                         WHERE run_id = ?1 AND step_id = ?2 AND attempt = ?3",
                        params![run_id, step_id, attempt, result, now],
                    )
                    .map_err(|_| unavailable())?;
                transaction
                    .execute(
                        "UPDATE workflow_steps SET status = 'succeeded', result = ?3
                         WHERE run_id = ?1 AND id = ?2",
                        params![run_id, step_id, result],
                    )
                    .map_err(|_| unavailable())?;
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
                    .map_err(|_| unavailable())?;
                transaction
                    .execute(
                        "UPDATE workflow_steps SET status = 'failed', error = ?3
                         WHERE run_id = ?1 AND id = ?2",
                        params![run_id, step_id, error],
                    )
                    .map_err(|_| unavailable())?;
                transaction
                    .execute(
                        "UPDATE workflow_steps SET status = 'canceled'
                         WHERE run_id = ?1 AND status IN ('pending','ready','waiting')",
                        [run_id],
                    )
                    .map_err(|_| unavailable())?;
                event(&transaction, run_id, "step_failed", step_id, "failed")?;
            }
        }
        transaction.commit().map_err(|_| unavailable())
    }

    fn interrupt_attempt(
        &self,
        run_id: &str,
        step_id: &str,
        attempt: u32,
        error: &str,
    ) -> Result<(), String> {
        let mut connection = self.connection.lock().map_err(|_| unavailable())?;
        let transaction = connection.transaction().map_err(|_| unavailable())?;
        let now = timestamp();
        let changed = transaction
            .execute(
                "UPDATE workflow_attempts SET status = 'interrupted', error = ?4,
                    finished_at = ?5
                 WHERE run_id = ?1 AND step_id = ?2 AND attempt = ?3 AND status = 'running'",
                params![run_id, step_id, attempt, error, now],
            )
            .map_err(|_| unavailable())?;
        if changed != 1 {
            return Err("workflow attempt is not running".to_string());
        }
        transaction
            .execute(
                "UPDATE workflow_steps SET status = 'interrupted', error = ?4
                 WHERE run_id = ?1 AND id = ?2 AND attempt = ?3 AND status = 'running'",
                params![run_id, step_id, attempt, error],
            )
            .map_err(|_| unavailable())?;
        event(
            &transaction,
            run_id,
            "step_interrupted",
            step_id,
            "interrupted",
        )?;
        // Keep the run non-retryable until every parallel owner has settled.
        settle_run(&transaction, run_id)?;
        transaction.commit().map_err(|_| unavailable())
    }

    fn recover(&self, workspace: &str) -> Result<usize, String> {
        let mut connection = self.connection.lock().map_err(|_| unavailable())?;
        let transaction = connection.transaction().map_err(|_| unavailable())?;
        let running = {
            let mut statement = transaction
                .prepare(
                    "SELECT s.run_id, s.id, s.attempt
                     FROM workflow_steps s
                     JOIN workflow_runs r ON r.id = s.run_id
                     WHERE s.status = 'running' AND r.workspace = ?1",
                )
                .map_err(|_| unavailable())?;
            let rows = statement
                .query_map([workspace], |row| {
                    Ok((
                        row.get::<String>(0)?,
                        row.get::<String>(1)?,
                        row.get::<i64>(2)?,
                    ))
                })
                .map_err(|_| unavailable())?;
            rows.collect::<crate::storage::Result<Vec<_>>>()
                .map_err(|_| unavailable())?
        };
        let mut runs = HashSet::new();
        for (run_id, step_id, attempt) in &running {
            transaction
                .execute(
                    "UPDATE workflow_steps SET status = 'interrupted'
                     WHERE run_id = ?1 AND id = ?2",
                    params![run_id, step_id],
                )
                .map_err(|_| unavailable())?;
            transaction
                .execute(
                    "UPDATE workflow_attempts SET status = 'interrupted', finished_at = ?4
                     WHERE run_id = ?1 AND step_id = ?2 AND attempt = ?3",
                    params![run_id, step_id, attempt, timestamp()],
                )
                .map_err(|_| unavailable())?;
            event(
                &transaction,
                run_id,
                "step_interrupted",
                step_id,
                "interrupted",
            )?;
            runs.insert(run_id.clone());
        }
        for run_id in &runs {
            transaction
                .execute(
                    "UPDATE workflow_runs SET status = 'paused', updated_at = ?2 WHERE id = ?1",
                    params![run_id, timestamp()],
                )
                .map_err(|_| unavailable())?;
            event(&transaction, run_id, "run_paused", "", "paused")?;
        }
        transaction.commit().map_err(|_| unavailable())?;
        Ok(running.len())
    }

    fn pause_run(&self, run_id: &str) -> Result<(), String> {
        let mut connection = self.connection.lock().map_err(|_| unavailable())?;
        let transaction = connection.transaction().map_err(|_| unavailable())?;
        let status: String = transaction
            .query_row(
                "SELECT status FROM workflow_runs WHERE id = ?1",
                [run_id],
                |row| row.get(0),
            )
            .map_err(|_| unavailable())?;
        if !matches!(status.as_str(), "running" | "waiting") {
            return Ok(());
        }
        let attempts = {
            let mut statement = transaction
                .prepare(
                    "SELECT id, attempt FROM workflow_steps
                     WHERE run_id = ?1 AND attempt > 0 AND status IN ('running','canceled')",
                )
                .map_err(|_| unavailable())?;
            let rows = statement
                .query_map([run_id], |row| {
                    Ok((row.get::<String>(0)?, row.get::<i64>(1)?))
                })
                .map_err(|_| unavailable())?;
            rows.collect::<crate::storage::Result<Vec<_>>>()
                .map_err(|_| unavailable())?
        };
        for (step_id, attempt) in attempts {
            transaction
                .execute(
                    "UPDATE workflow_steps SET status = 'interrupted'
                     WHERE run_id = ?1 AND id = ?2 AND status IN ('running','canceled')",
                    params![run_id, step_id],
                )
                .map_err(|_| unavailable())?;
            transaction
                .execute(
                    "UPDATE workflow_attempts SET status = 'interrupted', finished_at = ?4
                     WHERE run_id = ?1 AND step_id = ?2 AND attempt = ?3
                       AND status IN ('running','canceled')",
                    params![run_id, step_id, attempt, timestamp()],
                )
                .map_err(|_| unavailable())?;
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
            .map_err(|_| unavailable())?;
        event(&transaction, run_id, "run_paused", "", "paused")?;
        transaction.commit().map_err(|_| unavailable())
    }

    fn finalize_interrupted_batch(&self, run_id: &str) -> Result<(), String> {
        let mut connection = self.connection.lock().map_err(|_| unavailable())?;
        let transaction = connection.transaction().map_err(|_| unavailable())?;
        let running: i64 = transaction
            .query_row(
                "SELECT COUNT(*) FROM workflow_steps
                 WHERE run_id = ?1 AND status = 'running'",
                [run_id],
                |row| row.get(0),
            )
            .map_err(|_| unavailable())?;
        if running != 0 {
            return Err("workflow interruption is not ready to finalize".to_string());
        }
        let changed = transaction
            .execute(
                "UPDATE workflow_runs SET status = 'paused', updated_at = ?2
                 WHERE id = ?1 AND status = 'running'
                   AND EXISTS (
                       SELECT 1 FROM workflow_steps
                       WHERE run_id = ?1 AND status = 'interrupted'
                   )",
                params![run_id, timestamp()],
            )
            .map_err(|_| unavailable())?;
        if changed == 1 {
            event(&transaction, run_id, "run_paused", "", "paused")?;
        }
        transaction.commit().map_err(|_| unavailable())
    }

    fn finalize_failed(&self, run_id: &str) -> Result<(), String> {
        let mut connection = self.connection.lock().map_err(|_| unavailable())?;
        let transaction = connection.transaction().map_err(|_| unavailable())?;
        let (running, failed): (i64, i64) = transaction
            .query_row(
                "SELECT
                    SUM(CASE WHEN status = 'running' THEN 1 ELSE 0 END),
                    SUM(CASE WHEN status = 'failed' THEN 1 ELSE 0 END)
                 FROM workflow_steps WHERE run_id = ?1",
                [run_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|_| unavailable())?;
        if running != 0 || failed == 0 {
            return Err("workflow failure is not ready to finalize".to_string());
        }
        transaction
            .execute(
                "UPDATE workflow_runs SET status = 'failed', updated_at = ?2 WHERE id = ?1",
                params![run_id, timestamp()],
            )
            .map_err(|_| unavailable())?;
        event(&transaction, run_id, "run_failed", "", "failed")?;
        transaction.commit().map_err(|_| unavailable())
    }

    fn request_approval(
        &self,
        request_id: &str,
        run_id: &str,
        step_id: &str,
    ) -> Result<ApprovalRequest, String> {
        let mut connection = self.connection.lock().map_err(|_| unavailable())?;
        let transaction = connection.transaction().map_err(|_| unavailable())?;
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
            .map_err(|_| unavailable())?
            .ok_or_else(|| "workflow step not found".to_string())?;
        if row.0 != "approval" || row.1 != "ready" {
            return Err("workflow approval step is not ready".to_string());
        }
        let now = timestamp();
        transaction
            .execute(
                "UPDATE workflow_steps SET status = 'waiting' WHERE run_id = ?1 AND id = ?2",
                params![run_id, step_id],
            )
            .map_err(|_| unavailable())?;
        transaction
            .execute(
                "INSERT INTO workflow_requests (
                    id, run_id, step_id, prompt, status, created_at
                 ) VALUES (?1, ?2, ?3, ?4, 'pending', ?5)",
                params![request_id, run_id, step_id, row.2, now],
            )
            .map_err(|_| unavailable())?;
        event(
            &transaction,
            run_id,
            "approval_requested",
            step_id,
            "waiting",
        )?;
        settle_run(&transaction, run_id)?;
        transaction.commit().map_err(|_| unavailable())?;
        Ok(ApprovalRequest {
            id: request_id.to_string(),
            run_id: run_id.to_string(),
            step_id: step_id.to_string(),
            prompt: row.2,
            status: "pending".to_string(),
        })
    }

    fn respond(&self, request_id: &str, approved: bool) -> Result<String, String> {
        let mut connection = self.connection.lock().map_err(|_| unavailable())?;
        let transaction = connection.transaction().map_err(|_| unavailable())?;
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
            .map_err(|_| unavailable())?
            .ok_or_else(|| "approval request not found".to_string())?;
        let wanted = if approved { "approved" } else { "rejected" };
        if row.2 == wanted {
            return Ok(row.0);
        }
        if row.2 != "pending" {
            return Err("approval request already answered differently".to_string());
        }
        let now = timestamp();
        transaction
            .execute(
                "UPDATE workflow_requests SET status = ?2, responded_at = ?3 WHERE id = ?1",
                params![request_id, wanted, now],
            )
            .map_err(|_| unavailable())?;
        if approved {
            transaction
                .execute(
                    "UPDATE workflow_steps SET status = 'succeeded', result = 'approved'
                     WHERE run_id = ?1 AND id = ?2 AND status = 'waiting'",
                    params![row.0, row.1],
                )
                .map_err(|_| unavailable())?;
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
                .map_err(|_| unavailable())?;
            transaction
                .execute(
                    "UPDATE workflow_runs SET status = 'canceled', updated_at = ?2 WHERE id = ?1",
                    params![row.0, now],
                )
                .map_err(|_| unavailable())?;
            event(
                &transaction,
                &row.0,
                "approval_rejected",
                &row.1,
                "canceled",
            )?;
            event(&transaction, &row.0, "run_canceled", "", "canceled")?;
        }
        transaction.commit().map_err(|_| unavailable())?;
        Ok(row.0)
    }

    fn retry(&self, run_id: &str, step_id: &str) -> Result<(), String> {
        let mut connection = self.connection.lock().map_err(|_| unavailable())?;
        let transaction = connection.transaction().map_err(|_| unavailable())?;
        let run_status: String = transaction
            .query_row(
                "SELECT status FROM workflow_runs WHERE id = ?1",
                [run_id],
                |row| row.get(0),
            )
            .map_err(|_| unavailable())?;
        if run_status != "paused" {
            return Err("workflow run is not paused".to_string());
        }
        let changed = transaction
            .execute(
                "UPDATE workflow_steps SET status = 'ready', result = '', error = ''
                 WHERE run_id = ?1 AND id = ?2 AND status = 'interrupted'",
                params![run_id, step_id],
            )
            .map_err(|_| unavailable())?;
        if changed != 1 {
            return Err("workflow step is not interrupted".to_string());
        }
        transaction
            .execute(
                "UPDATE workflow_runs SET status = 'running', updated_at = ?2
                 WHERE id = ?1 AND status = 'paused'",
                params![run_id, timestamp()],
            )
            .map_err(|_| unavailable())?;
        event(&transaction, run_id, "step_ready", step_id, "ready")?;
        event(&transaction, run_id, "run_resumed", "", "running")?;
        transaction.commit().map_err(|_| unavailable())
    }

    fn cancel_attempt(&self, run_id: &str, step_id: &str, attempt: u32) -> Result<(), String> {
        let mut connection = self.connection.lock().map_err(|_| unavailable())?;
        let transaction = connection.transaction().map_err(|_| unavailable())?;
        let now = timestamp();
        transaction
            .execute(
                "UPDATE workflow_attempts SET status = 'canceled', finished_at = ?4
                 WHERE run_id = ?1 AND step_id = ?2 AND attempt = ?3 AND status = 'running'",
                params![run_id, step_id, attempt, now],
            )
            .map_err(|_| unavailable())?;
        transaction
            .execute(
                "UPDATE workflow_steps SET status = 'canceled'
                 WHERE run_id = ?1 AND id = ?2 AND status = 'running' AND attempt = ?3",
                params![run_id, step_id, attempt],
            )
            .map_err(|_| unavailable())?;
        event(&transaction, run_id, "step_canceled", step_id, "canceled")?;
        transaction.commit().map_err(|_| unavailable())
    }

    fn cancel_run(&self, run_id: &str) -> Result<(), String> {
        let mut connection = self.connection.lock().map_err(|_| unavailable())?;
        let transaction = connection.transaction().map_err(|_| unavailable())?;
        let status: Option<String> = transaction
            .query_row(
                "SELECT status FROM workflow_runs WHERE id = ?1",
                [run_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|_| unavailable())?;
        let Some(status) = status else {
            return Err("workflow run not found".to_string());
        };
        if matches!(status.as_str(), "succeeded" | "failed" | "canceled") {
            return Err("workflow run already finished".to_string());
        }
        transaction
            .execute(
                "UPDATE workflow_steps SET status = 'canceled'
                 WHERE run_id = ?1 AND status NOT IN ('succeeded','failed','canceled')",
                [run_id],
            )
            .map_err(|_| unavailable())?;
        transaction
            .execute(
                "UPDATE workflow_requests SET status = 'canceled', responded_at = ?2
                 WHERE run_id = ?1 AND status = 'pending'",
                params![run_id, timestamp()],
            )
            .map_err(|_| unavailable())?;
        transaction
            .execute(
                "UPDATE workflow_runs SET status = 'canceled', updated_at = ?2 WHERE id = ?1",
                params![run_id, timestamp()],
            )
            .map_err(|_| unavailable())?;
        event(&transaction, run_id, "run_canceled", "", "canceled")?;
        transaction.commit().map_err(|_| unavailable())
    }

    fn list_runs(&self, workspace: &str) -> Result<Vec<Run>, String> {
        let ids = {
            let connection = self.connection.lock().map_err(|_| unavailable())?;
            let mut statement = connection
                .prepare(
                    "SELECT id FROM workflow_runs WHERE workspace = ?1 ORDER BY created_at DESC",
                )
                .map_err(|_| unavailable())?;
            let rows = statement
                .query_map([workspace], |row| row.get::<String>(0))
                .map_err(|_| unavailable())?;
            rows.collect::<crate::storage::Result<Vec<_>>>()
                .map_err(|_| unavailable())?
        };
        ids.into_iter()
            .map(|id| self.get_run(&id)?.ok_or_else(unavailable))
            .collect()
    }

    fn requests(&self, run_id: &str) -> Result<Vec<ApprovalRequest>, String> {
        let connection = self.connection.lock().map_err(|_| unavailable())?;
        let mut statement = connection
            .prepare(
                "SELECT id, step_id, prompt, status FROM workflow_requests
                 WHERE run_id = ?1 ORDER BY created_at",
            )
            .map_err(|_| unavailable())?;
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
            .map_err(|_| unavailable())?;
        rows.collect::<crate::storage::Result<Vec<_>>>()
            .map_err(|_| unavailable())
    }

    fn request(&self, request_id: &str) -> Result<Option<ApprovalRequest>, String> {
        self.connection
            .lock()
            .map_err(|_| unavailable())?
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
            .map_err(|_| unavailable())
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

    fn events(&self, run_id: &str, after: i64) -> Result<Vec<EventRecord>, String> {
        let connection = self.connection.lock().map_err(|_| unavailable())?;
        let mut statement = connection
            .prepare(
                "SELECT seq, occurred_at, kind, step_id, status FROM workflow_events
                 WHERE run_id = ?1 AND seq > ?2 ORDER BY seq",
            )
            .map_err(|_| unavailable())?;
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
            .map_err(|_| unavailable())?;
        rows.collect::<crate::storage::Result<Vec<_>>>()
            .map_err(|_| unavailable())
    }
}

pub(super) fn migrate_schema(connection: &Connection) -> Result<(), String> {
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
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "agent" => Ok(Self::Agent),
            "handoff" => Ok(Self::Handoff),
            "approval" => Ok(Self::Approval),
            _ => Err(unavailable()),
        }
    }
}

impl RunStatus {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "running" => Ok(Self::Running),
            "waiting" => Ok(Self::Waiting),
            "paused" => Ok(Self::Paused),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            "canceled" => Ok(Self::Canceled),
            _ => Err(unavailable()),
        }
    }
}

impl StepStatus {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "pending" => Ok(Self::Pending),
            "ready" => Ok(Self::Ready),
            "running" => Ok(Self::Running),
            "waiting" => Ok(Self::Waiting),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            "canceled" => Ok(Self::Canceled),
            "interrupted" => Ok(Self::Interrupted),
            _ => Err(unavailable()),
        }
    }
}

fn advance_ready(transaction: &Transaction<'_>, run_id: &str) -> Result<(), String> {
    let run_status: String = transaction
        .query_row(
            "SELECT status FROM workflow_runs WHERE id = ?1",
            [run_id],
            |row| row.get(0),
        )
        .map_err(|_| unavailable())?;
    if !matches!(run_status.as_str(), "running" | "waiting") {
        return Ok(());
    }
    let pending = {
        let mut statement = transaction
            .prepare(
                "SELECT id, needs_json FROM workflow_steps
                 WHERE run_id = ?1 AND status = 'pending' ORDER BY position",
            )
            .map_err(|_| unavailable())?;
        let rows = statement
            .query_map([run_id], |row| {
                Ok((row.get::<String>(0)?, row.get::<String>(1)?))
            })
            .map_err(|_| unavailable())?;
        rows.collect::<crate::storage::Result<Vec<_>>>()
            .map_err(|_| unavailable())?
    };
    for (step_id, encoded) in pending {
        let needs: Vec<String> = serde_json::from_str(&encoded).map_err(|_| unavailable())?;
        let mut ready = true;
        for dependency in needs {
            let status: String = transaction
                .query_row(
                    "SELECT status FROM workflow_steps WHERE run_id = ?1 AND id = ?2",
                    params![run_id, dependency],
                    |row| row.get(0),
                )
                .map_err(|_| unavailable())?;
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
                .map_err(|_| unavailable())?;
            event(transaction, run_id, "step_ready", &step_id, "ready")?;
        }
    }
    Ok(())
}

fn copied_steps_at(
    transaction: &Transaction<'_>,
    run_id: &str,
    boundary_seq: i64,
) -> Result<HashMap<String, CopiedStep>, String> {
    let mut statement = transaction
        .prepare(
            "SELECT e.step_id, e.seq, s.attempt, s.result, s.transcript_path
             FROM workflow_events e
             JOIN workflow_steps s ON s.run_id = e.run_id AND s.id = e.step_id
             WHERE e.run_id = ?1 AND e.seq <= ?2
               AND e.kind IN ('step_succeeded','approval_approved')
             ORDER BY e.seq",
        )
        .map_err(|_| unavailable())?;
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
        .map_err(|_| unavailable())?;
    rows.collect::<crate::storage::Result<HashMap<_, _>>>()
        .map_err(|_| unavailable())
}

fn settle_run(transaction: &Transaction<'_>, run_id: &str) -> Result<(), String> {
    let (unfinished, active, waiting, interrupted): (i64, i64, i64, i64) = transaction
        .query_row(
            "SELECT
                SUM(CASE WHEN status != 'succeeded' THEN 1 ELSE 0 END),
                SUM(CASE WHEN status IN ('ready','running') THEN 1 ELSE 0 END),
                SUM(CASE WHEN status = 'waiting' THEN 1 ELSE 0 END),
                SUM(CASE WHEN status = 'interrupted' THEN 1 ELSE 0 END)
             FROM workflow_steps WHERE run_id = ?1",
            [run_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .map_err(|_| unavailable())?;
    if unfinished == 0 {
        transaction
            .execute(
                "UPDATE workflow_runs SET status = 'succeeded', updated_at = ?2 WHERE id = ?1",
                params![run_id, timestamp()],
            )
            .map_err(|_| unavailable())?;
        event(transaction, run_id, "run_succeeded", "", "succeeded")?;
        return Ok(());
    }
    let status: String = transaction
        .query_row(
            "SELECT status FROM workflow_runs WHERE id = ?1",
            [run_id],
            |row| row.get(0),
        )
        .map_err(|_| unavailable())?;
    if active == 0 && interrupted > 0 && status == "running" {
        transaction
            .execute(
                "UPDATE workflow_runs SET status = 'paused', updated_at = ?2 WHERE id = ?1",
                params![run_id, timestamp()],
            )
            .map_err(|_| unavailable())?;
        event(transaction, run_id, "run_paused", "", "paused")?;
    } else if active == 0 && waiting > 0 && status == "running" {
        transaction
            .execute(
                "UPDATE workflow_runs SET status = 'waiting', updated_at = ?2 WHERE id = ?1",
                params![run_id, timestamp()],
            )
            .map_err(|_| unavailable())?;
        event(transaction, run_id, "run_waiting", "", "waiting")?;
    } else if active > 0 && status == "waiting" {
        transaction
            .execute(
                "UPDATE workflow_runs SET status = 'running', updated_at = ?2 WHERE id = ?1",
                params![run_id, timestamp()],
            )
            .map_err(|_| unavailable())?;
        event(transaction, run_id, "run_resumed", "", "running")?;
    }
    Ok(())
}

fn event(
    transaction: &Transaction<'_>,
    run_id: &str,
    kind: &str,
    step_id: &str,
    status: &str,
) -> Result<(), String> {
    transaction
        .execute(
            "INSERT INTO workflow_events (run_id, occurred_at, kind, step_id, status)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![run_id, timestamp(), kind, step_id, status],
        )
        .map_err(|_| unavailable())?;
    Ok(())
}

fn timestamp() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn unavailable() -> String {
    "workflow store unavailable".to_string()
}

#[cfg(test)]
mod repository_tests {
    use super::*;
    use crate::workflow::repository::WorkflowStore;
    #[test]
    fn attempt_journal_keeps_each_transcript_path() {
        let store = TursoWorkflowStore::open_in_memory();
        let definition = Definition {
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
