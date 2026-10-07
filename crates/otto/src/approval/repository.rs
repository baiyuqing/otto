//! Approval persistence: atomic request/event/claim commits against a persisted snapshot.
use super::{ApprovalMode, Change, Request, Result, Scope, Snapshot};

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

pub trait ApprovalStore: Send + Sync {
    fn mode(&self, scope: &Scope) -> Result<Option<ApprovalMode>>;
    fn set_mode(&self, scope: &Scope, mode: ApprovalMode) -> Result<()>;
    fn create(&self, subject_key: &str, request: &Request) -> Result<(Request, bool)>;
    fn load(&self, id: &str) -> Result<Request>;
    fn visible(&self, id: &str, workspace: &str, session: Option<&str>) -> Result<bool>;
    fn pending(&self) -> Result<Vec<Request>>;
    fn applications(&self, workspace: &str) -> Result<Vec<Request>>;
    fn snapshot(&self, workspace: &str, session: Option<&str>) -> Result<Snapshot>;
    fn events(&self, workspace: &str, session: Option<&str>, after: i64) -> Result<Vec<Change>>;
    fn claim(&self, id: &str) -> Result<Option<Claim>>;
    fn take_claim(&self, id: &str, expected: &Claim, next: &Claim) -> Result<bool>;
    /// Recheck the complete persisted snapshot, the persisted session mode and
    /// claim ownership; publish the request and its event together or publish
    /// neither. An automatic decision additionally rechecks
    /// [`ApprovalMode::decides`] against the persisted mode, inside the
    /// committing transaction.
    fn commit(
        &self,
        expected: &Request,
        next: &Request,
        claim: ClaimChange,
        automatic: bool,
    ) -> Result<Request>;
}

/// A commit may publish an automatic decision only for a request the persisted
/// session mode decides. Backends recheck this inside the committing
/// transaction, so a decision computed before the request or the mode changed
/// cannot land.
pub fn automatic_decision_allowed(mode: ApprovalMode, request: &Request) -> bool {
    mode.decides(request)
}
