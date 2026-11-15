//! Approval persistence: atomic request/event/claim commits against a persisted snapshot.
use super::{Error, Result};
use otto_core::approval::{ApprovalMode, ApprovalSettings, Change, Request, Scope, Snapshot};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Claim {
    pub owner: String,
    pub pid: i64,
    pub started: String,
    pub native: bool,
}
pub enum ClaimChange {
    None,
    Start(Claim),
    Finish(String),
}

/// Reviewing has no operation effect and has its own expiring ownership.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewLease {
    pub owner: String,
    pub pid: i64,
    pub started: String,
    pub expires_at: i64,
}

pub trait ApprovalStore: Send + Sync {
    fn mode(&self, scope: &Scope) -> Result<Option<ApprovalMode>>;
    fn set_mode(&self, scope: &Scope, mode: ApprovalMode) -> Result<()>;
    fn settings(&self, scope: &Scope) -> Result<Option<ApprovalSettings>> {
        Ok(self.mode(scope)?.map(ApprovalSettings::from_mode))
    }
    fn set_settings(&self, _: &Scope, _: u64, _: ApprovalSettings) -> Result<ApprovalSettings> {
        Err(Error::Unavailable)
    }
    fn review_context(&self, _: &Scope, _: &str, _: &str) -> Result<()> {
        Err(Error::Unavailable)
    }
    /// Persisted authority basis is distinct from live review availability.
    fn review_snapshot(&self, _: &Scope) -> Result<Option<(String, String)>> {
        Ok(None)
    }
    fn policy_config(&self, _: &str, _: ApprovalSettings, _: &str) -> Result<()> {
        Err(Error::Unavailable)
    }
    fn review_denials(&self, _: &Scope, _: &str) -> Result<u64> {
        Ok(0)
    }
    fn authorization_current(&self, _: &Request) -> Result<bool> {
        Ok(false)
    }
    fn managed_policy(&self, _: &str, _: &otto_core::config::ApprovalConfig) -> Result<()> {
        Err(Error::Unavailable)
    }
    fn take_review_lease(&self, _: &Request, _: &ReviewLease) -> Result<bool> {
        Ok(false)
    }
    fn release_review_lease(&self, _: &str, _: &str) -> Result<()> {
        Ok(())
    }
    fn commit_review(&self, _: &Request, _: &Request, _: &str) -> Result<Request> {
        Err(Error::Unavailable)
    }
    fn create(&self, subject_key: &str, request: &Request) -> Result<(Request, bool)>;
    fn load(&self, id: &str) -> Result<Request>;
    fn visible(&self, id: &str, workspace: &str, session: Option<&str>) -> Result<bool>;
    fn pending(&self) -> Result<Vec<Request>>;
    fn applications(&self, workspace: &str) -> Result<Vec<Request>>;
    fn snapshot(&self, workspace: &str, session: Option<&str>) -> Result<Snapshot>;
    fn events(&self, workspace: &str, session: Option<&str>, after: i64) -> Result<Vec<Change>>;
    fn claim(&self, id: &str) -> Result<Option<Claim>>;
    fn take_claim(&self, id: &str, expected: &Claim, next: &Claim) -> Result<bool>;
    /// Recheck the persisted request, canonical settings, policy/evidence
    /// snapshots and claim ownership in the transaction. Publish the request
    /// and its event together; an automatic effect claim uses the same fence.
    fn commit(
        &self,
        expected: &Request,
        next: &Request,
        claim: ClaimChange,
        automatic: bool,
    ) -> Result<Request>;
}

/// Nullable legacy metadata and review-status-only updates do not create a
/// synthetic operation revision. A changed authority basis does.
pub(crate) fn pending_basis_changed(expected: &Request, next: &Request) -> bool {
    expected.state == otto_core::approval::State::Pending
        && next.state == otto_core::approval::State::Pending
        && expected
            .review
            .as_ref()
            .zip(next.review.as_ref())
            .is_some_and(|(old, new)| {
                !old.matches(
                    new.settings_revision,
                    &new.evidence_digest,
                    &new.policy_digest,
                )
            })
}
