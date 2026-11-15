//! Attempt claiming, completion and recovery for [`SqliteWorkflowStore`].

use super::*;

impl SqliteWorkflowStore {
    pub(super) fn claim_ready(
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

    pub(super) fn finish_attempt(
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

    pub(super) fn interrupt_attempt(
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

    pub(super) fn recover(&self, workspace: &str) -> Result<usize, Error> {
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

    pub(super) fn cancel_attempt(
        &self,
        run_id: &str,
        step_id: &str,
        attempt: u32,
    ) -> Result<(), Error> {
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
}
