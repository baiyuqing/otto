//! Run fork, cancel, list and read queries for [`SqliteWorkflowStore`].

use super::*;

impl SqliteWorkflowStore {
    pub(super) fn fork_run(
        &self,
        id: &str,
        source_run_id: &str,
        after_step: &str,
    ) -> Result<Run, Error> {
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

    pub(super) fn cancel_run(&self, run_id: &str) -> Result<(), Error> {
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

    pub(super) fn list_runs(&self, workspace: &str, query: &ListQuery) -> Result<Vec<Run>, Error> {
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

    pub(super) fn requests(&self, run_id: &str) -> Result<Vec<ApprovalRequest>, Error> {
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

    pub(super) fn request(&self, request_id: &str) -> Result<Option<ApprovalRequest>, Error> {
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
    pub(super) fn run_age_ms(&self, run_id: &str) -> Option<u128> {
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

    pub(super) fn events(&self, run_id: &str, after: i64) -> Result<Vec<EventRecord>, Error> {
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
