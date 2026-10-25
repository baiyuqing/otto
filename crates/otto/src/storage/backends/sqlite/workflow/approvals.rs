//! Pause, approval and retry transitions for [`SqliteWorkflowStore`].

use super::*;

impl SqliteWorkflowStore {
    pub(super) fn pause_run(&self, run_id: &str, error: &str) -> Result<(), Error> {
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

    pub(super) fn request_approval(
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

    pub(super) fn respond(&self, request_id: &str, approved: bool) -> Result<String, Error> {
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

    pub(super) fn settle(&self, run_id: &str) -> Result<(), Error> {
        let mut connection = self.connection.lock().map_err(|_| op_error())?;
        let transaction = connection.transaction().map_err(|_| op_error())?;
        settle_run(&transaction, run_id)?;
        transaction.commit().map_err(|_| op_error())
    }

    pub(super) fn retry(&self, run_id: &str, step_id: &str) -> Result<(), Error> {
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
}
