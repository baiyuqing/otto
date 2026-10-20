//! Automatic and human approval checks run inside commit transactions.

use super::*;

pub(super) fn check_automatic(
    tx: &Transaction<'_>,
    request: &Request,
    owner: Option<&str>,
) -> Result<()> {
    if request.scope.session_id.is_empty() {
        return Err(Error::Conflict);
    }
    let settings = read_settings(tx, &request.scope)?.unwrap_or_default();
    if request.decision.as_ref().is_some_and(|d| {
        matches!(
            d.transport.as_str(),
            "bounded-authority" | "approval-rule" | "never-policy" | "yolo-mode"
        )
    }) {
        let metadata = request.review.as_ref().ok_or(Error::Conflict)?;
        let actual: Option<String> = tx
            .query_row(
                "SELECT policy_digest FROM approval_review_contexts WHERE workspace=?1 \
            AND session_id=''",
                [request.scope.workspace.as_str()],
                |r| r.get(0),
            )
            .optional()
            .map_err(storage_error)?;
        let actual = actual.unwrap_or_else(|| {
            crate::approval::digest(
                &serde_json::to_string(&otto_core::config::ApprovalConfig::default())
                    .expect("approval config encodes"),
            )
        });
        if metadata.settings_revision != settings.revision || metadata.policy_digest != actual {
            return Err(Error::Conflict);
        }
    }
    if request
        .decision
        .as_ref()
        .is_some_and(|d| d.transport == "bounded-authority")
    {
        return if request.kind == Kind::Authorization
            && request.authority == Authority::Bounded
            && !request.persistent
            && request
                .review
                .as_ref()
                .is_some_and(|r| r.settings_revision == settings.revision)
            && (settings.policy == ApprovalPolicy::Never
                || settings.reviewer != ApprovalReviewer::User)
        {
            Ok(())
        } else {
            Err(Error::Conflict)
        };
    }
    if request
        .decision
        .as_ref()
        .is_some_and(|d| d.transport == "approval-rule")
    {
        let metadata = request.review.as_ref().ok_or(Error::Conflict)?;
        let digest: Option<String> = tx
            .query_row(
                "SELECT policy_digest FROM approval_review_contexts WHERE workspace=?1 \
            AND session_id=''",
                [request.scope.workspace.as_str()],
                |r| r.get(0),
            )
            .optional()
            .map_err(storage_error)?;
        return if metadata.settings_revision == settings.revision
            && digest.as_deref() == Some(&metadata.policy_digest)
        {
            Ok(())
        } else {
            Err(Error::Conflict)
        };
    }
    if settings.policy == ApprovalPolicy::Never {
        return if request.state == State::Rejected
            && request
                .review
                .as_ref()
                .is_some_and(|r| r.settings_revision == settings.revision)
        {
            Ok(())
        } else {
            Err(Error::Conflict)
        };
    }
    if settings.reviewer == ApprovalReviewer::Bypass {
        return if request
            .review
            .as_ref()
            .is_none_or(|r| r.settings_revision == settings.revision)
        {
            Ok(())
        } else {
            Err(Error::Conflict)
        };
    }
    if settings.reviewer != ApprovalReviewer::AutoReview {
        return Err(Error::Conflict);
    }
    let metadata = request.review.as_ref().ok_or(Error::Conflict)?;
    if metadata.settings_revision != settings.revision {
        return Err(Error::Conflict);
    }
    let context: Option<(String, String)> = tx
        .query_row(
            "SELECT evidence_digest,policy_digest FROM approval_review_contexts \
        WHERE workspace=?1 AND session_id=?2",
            params![request.scope.workspace, request.scope.session_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(storage_error)?;
    if !context.is_some_and(|(e, p)| metadata.matches(settings.revision, &e, &p)) {
        return Err(Error::Conflict);
    }
    if owner.is_none()
        && request
            .decision
            .as_ref()
            .is_none_or(|d| d.transport != "approval-rule")
    {
        return Err(Error::Conflict);
    }
    if request.state == State::Approved
        && metadata
            .assessment
            .as_ref()
            .is_none_or(|a| a.outcome != ReviewOutcome::Allow)
    {
        return Err(Error::Conflict);
    }
    if request.state == State::Rejected
        && metadata
            .assessment
            .as_ref()
            .is_none_or(|a| a.outcome != ReviewOutcome::Deny)
    {
        return Err(Error::Conflict);
    }
    Ok(())
}
pub(super) fn check_human_basis(
    tx: &Transaction<'_>,
    request: &Request,
    allow: bool,
) -> Result<()> {
    if request.scope.session_id.is_empty() {
        return Ok(());
    }
    let settings = read_settings(tx, &request.scope)?.unwrap_or_default();
    if allow && settings.policy == ApprovalPolicy::Never {
        return Err(Error::Conflict);
    }
    let Some(metadata) = &request.review else {
        return Ok(());
    };
    if metadata.settings_revision != settings.revision {
        return Err(Error::Conflict);
    }
    let context: Option<(String, String)> = tx
        .query_row(
            "SELECT evidence_digest,policy_digest FROM approval_review_contexts \
        WHERE workspace=?1 AND session_id=?2",
            params![request.scope.workspace, request.scope.session_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(storage_error)?;
    if let Some((evidence, policy)) = context {
        return if metadata.matches(settings.revision, &evidence, &policy) {
            Ok(())
        } else {
            Err(Error::Conflict)
        };
    }
    let policy: Option<String> = tx
        .query_row(
            "SELECT policy_digest FROM approval_review_contexts WHERE workspace=?1 \
        AND session_id=''",
            [request.scope.workspace.as_str()],
            |r| r.get(0),
        )
        .optional()
        .map_err(storage_error)?;
    let policy = policy.unwrap_or_else(|| {
        crate::approval::digest(
            &serde_json::to_string(&otto_core::config::ApprovalConfig::default())
                .expect("approval config encodes"),
        )
    });
    if metadata.evidence_digest == "missing" && metadata.policy_digest == policy {
        Ok(())
    } else {
        Err(Error::Conflict)
    }
}
