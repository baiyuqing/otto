//! Transactional request commit for [`Store`].

use super::*;

impl Store {
    pub(super) fn commit_checked(
        &self,
        expected: &Request,
        next: &Request,
        claim: ClaimChange,
        automatic: bool,
        review_owner: Option<&str>,
    ) -> Result<Request> {
        if automatic
            && matches!(next.state, State::Approved | State::Rejected)
            && next.decision.is_none()
        {
            return Err(Error::Conflict);
        }
        if next.id != expected.id
            || next.scope != expected.scope
            || (if pending_basis_changed(expected, next) {
                next.revision != expected.revision.checked_add(1).unwrap_or(0)
            } else {
                next.revision != expected.revision && !is_reauthorization(expected, next)
            })
            || next.schema_version != expected.schema_version
            || next.subject != expected.subject
        {
            return Err(Error::Conflict);
        }
        let mut connection = self.connection.lock().map_err(|_| Error::Unavailable)?;
        let tx = connection.transaction().map_err(storage_error)?;
        let body = tx
            .query_row(
                "SELECT body FROM approvals WHERE id=?1",
                [expected.id.as_str()],
                |row| row.get::<String>(0),
            )
            .optional()
            .map_err(storage_error)?
            .ok_or(Error::NotFound)?;
        if decode(&body)? != *expected {
            return Err(Error::Conflict);
        }
        if !automatic
            && expected.state == State::Pending
            && next.decision.is_some()
            && matches!(next.state, State::Approved | State::Rejected)
        {
            check_human_basis(&tx, expected, next.state == State::Approved)?;
        }
        if is_reauthorization(expected, next)
            && check_automatic(&tx, expected, Some("application")).is_ok()
        {
            return Err(Error::Conflict);
        }
        if automatic
            || (expected.state == State::Approved
                && expected.application == ApplicationState::NotStarted
                && next.application == ApplicationState::Applying
                && expected.decision.as_ref().is_some_and(|d| {
                    matches!(
                        d.transport.as_str(),
                        "auto-review" | "approval-rule" | "yolo-mode" | "bounded-authority"
                    )
                }))
        {
            check_automatic(
                &tx,
                next,
                if automatic {
                    review_owner
                } else {
                    Some("application")
                },
            )?;
        }
        if let Some(owner) = review_owner {
            let lease: Option<(String, i64)> = tx
                .query_row(
                    "SELECT owner,expires_at FROM approval_review_leases WHERE approval_id=?1",
                    [expected.id.as_str()],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                .map_err(storage_error)?;
            if lease.is_none_or(|(actual, expires)| {
                actual != owner || expires <= chrono::Utc::now().timestamp()
            }) {
                return Err(Error::Conflict);
            }
        }
        match claim {
            ClaimChange::None => {}
            ClaimChange::Start(claim) => {
                tx.execute("INSERT INTO approval_claims (approval_id,owner,pid,started,recovery) VALUES (?1,?2,?3,?4,?5)", params![expected.id,claim.owner,claim.pid,claim.started,if claim.native { "native" } else { "domain" }]).map_err(|error| match error { crate::storage::Error::Constraint(_) => Error::Conflict, other => storage_error(other) })?;
            }
            ClaimChange::Finish(owner) => {
                let actual = tx
                    .query_row(
                        "SELECT owner FROM approval_claims WHERE approval_id=?1",
                        [expected.id.as_str()],
                        |row| row.get::<String>(0),
                    )
                    .map_err(storage_error)?;
                if actual != owner {
                    return Err(Error::Conflict);
                }
            }
        }
        let state = serde_json::to_value(next.state).map_err(|_| Error::Unavailable)?;
        let merged_body = merge_persisted(&body, next)?;
        tx.execute(
            "UPDATE approvals SET state=?2,body=?3 WHERE id=?1",
            params![
                next.id,
                state.as_str().ok_or(Error::Unavailable)?,
                merged_body
            ],
        )
        .map_err(storage_error)?;
        append_event_body(&tx, next, &merged_body)?;
        if expected.state == State::Pending
            && next.state != State::Pending
            && next
                .decision
                .as_ref()
                .is_some_and(|d| d.transport == "auto-review")
            && let Some(metadata) = &next.review
        {
            tx.execute("INSERT INTO approval_review_denials VALUES (?1,?2,?3,?4,?5,?6) ON CONFLICT(workspace,session_id,task_id,run_id,evidence_digest) DO UPDATE SET denials=CASE WHEN excluded.denials=0 THEN 0 ELSE approval_review_denials.denials+1 END",params![next.scope.workspace,next.scope.session_id,next.scope.task_id,next.scope.run_id,metadata.evidence_digest,if next.state == State::Rejected {1} else {0}]).map_err(storage_error)?;
        }
        tx.commit().map_err(storage_error)?;
        Ok(next.clone())
    }
}
