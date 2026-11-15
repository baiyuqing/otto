//! Step-state conversions and run progress/settlement transactions.

use super::*;

impl StepKind {
    pub(super) fn parse(value: &str) -> Result<Self, Error> {
        match value {
            "agent" => Ok(Self::Agent),
            "handoff" => Ok(Self::Handoff),
            "approval" => Ok(Self::Approval),
            _ => Err(corrupt()),
        }
    }
}

impl RunStatus {
    pub(super) fn parse(value: &str) -> Result<Self, Error> {
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
    pub(super) fn parse(value: &str) -> Result<Self, Error> {
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

pub(super) fn advance_ready(transaction: &Transaction<'_>, run_id: &str) -> Result<(), Error> {
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
        rows.collect::<otto_storage::Result<Vec<_>>>()
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

pub(super) fn copied_steps_at(
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
    rows.collect::<otto_storage::Result<HashMap<_, _>>>()
        .map_err(|_| op_error())
}

/// Derives the run status from its steps and writes it: the single authority
/// over run lifecycle. Every step transition settles, so a run can never be
/// left `running` without an owner, and a status that does not change writes no
/// second event.
pub(super) fn settle_run(transaction: &Transaction<'_>, run_id: &str) -> Result<(), Error> {
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
