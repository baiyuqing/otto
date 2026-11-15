//! Durable approval authority. Decisions and event cursors commit together.
//! Domain mutations are separate and reconciled through domain receipts; an
//! uncertain external action is never automatically replayed.
use super::contracts::*;
use super::repository::{ApprovalStore, Claim, ClaimChange};
use chrono::Utc;
use otto_core::approval::{
    ApplicationState, ApprovalMode, ApprovalSettings, Change, Choice, Decision, Kind, Request,
    ReviewStatus, Scope, Snapshot, State,
};
use otto_platform::hex::to_hex;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Read;
use std::sync::Arc;
use std::sync::RwLock;
use std::time::Duration;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

pub struct Service {
    worker_started: std::sync::atomic::AtomicBool,
    pub(super) owner: String,
    pub(super) store: Arc<dyn ApprovalStore>,
    changes: watch::Sender<u64>,
    domains: RwLock<BTreeMap<(String, String), Arc<dyn Domain>>>,
    pub(super) review_state: RwLock<super::coordinator::ReviewState>,
}
enum Recovery {
    Active,
    Domain,
    Unknown,
}

impl Service {
    pub(crate) fn from_store(store: Arc<dyn ApprovalStore>) -> Result<Self> {
        let mut random = [0; 16];
        std::fs::File::open("/dev/urandom")
            .and_then(|mut file| file.read_exact(&mut random))
            .map_err(|_| Error::Unavailable)?;
        Ok(Self {
            worker_started: std::sync::atomic::AtomicBool::new(false),
            owner: random.iter().map(|b| format!("{b:02x}")).collect(),
            store,
            changes: watch::channel(0).0,
            domains: RwLock::new(BTreeMap::new()),
            review_state: RwLock::new(Default::default()),
        })
    }
    pub fn start(self: &Arc<Self>) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        if self
            .worker_started
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            return;
        }
        let weak = Arc::downgrade(self);
        runtime.spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(200));
            loop {
                tick.tick().await;
                let Some(service) = weak.upgrade() else {
                    return;
                };
                let workspaces: std::collections::BTreeSet<String> = match service.domains.read() {
                    Ok(domains) => domains
                        .keys()
                        .map(|(workspace, _)| workspace.clone())
                        .collect(),
                    Err(_) => return,
                };
                if service.expire_pending().is_err() {
                    eprintln!("approval maintenance is unavailable");
                }
                service.schedule_reviews();
                for workspace in workspaces {
                    if service.apply_available(&workspace).is_err() {
                        eprintln!("approval application maintenance is unavailable");
                    }
                }
            }
        });
    }

    pub fn register_domain(
        &self,
        workspace: &str,
        source: &str,
        domain: Arc<dyn Domain>,
    ) -> Result<()> {
        self.domains
            .write()
            .map_err(|_| Error::Unavailable)?
            .insert((workspace.into(), source.into()), domain);
        Ok(())
    }
    pub(super) fn hydrate(&self, mut request: Request) -> Result<Request> {
        let domain = self
            .domains
            .read()
            .map_err(|_| Error::Unavailable)?
            .get(&(
                request.scope.workspace.clone(),
                request.subject.source.clone(),
            ))
            .cloned();
        if let Some(domain) = domain {
            match domain.details(&request) {
                Ok(details) => request.subject.details = details,
                Err(_) if request.state != State::Pending => {}
                Err(Error::Conflict) => {
                    return self.mutate(&request.id, &request.scope, |current| {
                        if current.state != State::Pending {
                            return Ok(false);
                        }
                        current.state = State::Superseded;
                        if let Some(review) = current.review.as_mut() {
                            review.status = ReviewStatus::Completed;
                        }
                        Ok(true)
                    });
                }
                Err(error) => return Err(error),
            }
        } else if request.subject.source == "memory" {
            let domains = self.domains.read().map_err(|_| Error::Unavailable)?;
            for ((_, source), domain) in domains.iter() {
                if source == "memory"
                    && let Ok(details) = domain.details(&request)
                {
                    request.subject.details = details;
                    break;
                }
            }
        }
        Ok(request)
    }
    pub fn apply_available(&self, workspace: &str) -> Result<()> {
        let requests = self.store.applications(workspace)?;
        for mut request in requests {
            if !matches!(
                request.application,
                ApplicationState::NotStarted | ApplicationState::Applying
            ) {
                continue;
            }
            if request.state == State::Approved
                && request.application == ApplicationState::NotStarted
            {
                request = self.revalidate_authorization(&request)?;
                if request.state == State::Pending {
                    continue;
                }
            }
            if request.application == ApplicationState::Applying {
                match self.recover_claim(&request.id)? {
                    Recovery::Active => continue,
                    Recovery::Domain => {}
                    Recovery::Unknown => {
                        self.application(&request.id, &request.scope, ApplicationState::Applying,
                            ApplicationState::Unknown, "The native review owner exited; verify its canonical result before continuing.")?;
                        continue;
                    }
                }
            }
            let domain = self
                .domains
                .read()
                .map_err(|_| Error::Unavailable)?
                .get(&(
                    request.scope.workspace.clone(),
                    request.subject.source.clone(),
                ))
                .cloned();
            let domain = domain.or_else(|| {
                self.domains.read().ok().and_then(|domains| {
                    domains
                        .iter()
                        .find(|((_, source), d)| {
                            source == &request.subject.source && d.details(&request).is_ok()
                        })
                        .map(|(_, d)| d.clone())
                })
            });
            let Some(domain) = domain else {
                if request.application == ApplicationState::Applying {
                    self.application(
                        &request.id,
                        &request.scope,
                        ApplicationState::Applying,
                        ApplicationState::Unknown,
                        "The execution owner exited; external effects cannot be established.",
                    )?;
                }
                continue;
            };
            if request.state == State::Approved
                && request.application == ApplicationState::NotStarted
                && matches!(domain.details(&request), Err(Error::Conflict))
            {
                self.mutate(&request.id, &request.scope, |current| {
                    if current.state != State::Approved
                        || current.application != ApplicationState::NotStarted
                    {
                        return Ok(false);
                    }
                    current.state = State::Superseded;
                    Ok(true)
                })?;
                continue;
            }
            if request.application == ApplicationState::NotStarted
                && self
                    .application(
                        &request.id,
                        &request.scope,
                        ApplicationState::NotStarted,
                        ApplicationState::Applying,
                        "",
                    )
                    .is_err()
            {
                let _ = self.revalidate_authorization(&request);
                continue;
            }
            let outcome = domain.apply(&request);
            let next = if outcome.is_ok() {
                ApplicationState::Applied
            } else {
                ApplicationState::Failed
            };
            let _ = self.application(
                &request.id,
                &request.scope,
                ApplicationState::Applying,
                next,
                if outcome.is_err() {
                    "Domain application failed; review the current target."
                } else {
                    ""
                },
            );
        }
        Ok(())
    }

    pub async fn finish_decision(&self, request: Request) -> Result<Request> {
        self.apply_available(&request.scope.workspace)?;
        if request.blocking || request.application == ApplicationState::NotRequired {
            return self.get(
                &request.id,
                &request.scope.workspace,
                Some(&request.scope.session_id),
            );
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let current = self.get(
                &request.id,
                &request.scope.workspace,
                Some(&request.scope.session_id),
            )?;
            if !matches!(
                current.application,
                ApplicationState::NotStarted | ApplicationState::Applying
            ) {
                return Ok(current);
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(current);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn recover_claim(&self, id: &str) -> Result<Recovery> {
        let Some(claim) = self.store.claim(id)? else {
            return Ok(Recovery::Active);
        };
        if otto_platform::process_identity::process_matches(claim.pid, &claim.started)
            != Some(false)
        {
            return Ok(Recovery::Active);
        }
        let (pid, started) = otto_platform::process_identity::current_process();
        let next = Claim {
            owner: self.owner.clone(),
            pid,
            started,
            native: claim.native,
        };
        Ok(if !self.store.take_claim(id, &claim, &next)? {
            Recovery::Active
        } else if claim.native {
            Recovery::Unknown
        } else {
            Recovery::Domain
        })
    }

    /// Compatibility no-op: approval content is no longer secret-scanned.
    ///
    /// Content may be displayed and persisted; callers must not include real secrets.
    #[deprecated(note = "approval content is no longer secret-scanned; do not include secrets")]
    pub fn protect_secrets(&self, _secrets: &[String]) -> Result<()> {
        Ok(())
    }

    pub fn changes(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }
    pub(super) fn changed(&self) {
        self.changes.send_modify(|v| *v = v.wrapping_add(1));
    }

    /// The session policy for `scope`. A missing preference means AUTO for a
    /// session. Workspace-only requests report MANUAL; a child task inherits
    /// its root session's current settings.
    pub fn mode(&self, scope: &Scope) -> Result<ApprovalMode> {
        if scope.session_id.is_empty() {
            return Ok(ApprovalMode::Manual);
        }
        Ok(self.settings(scope)?.mode())
    }

    pub fn set_mode(&self, scope: &Scope, mode: ApprovalMode) -> Result<()> {
        if scope.workspace.is_empty() || scope.session_id.is_empty() || !scope.task_id.is_empty() {
            return Err(Error::Invalid("approval mode requires a session"));
        }
        let settings = ApprovalSettings::from_mode(mode);
        match self.set_settings(
            scope,
            self.settings(scope)?.revision,
            settings.policy,
            settings.reviewer,
        ) {
            Ok(_) => {}
            Err(Error::Unavailable) => {
                self.store.set_mode(scope, mode)?;
                self.changed();
            }
            Err(error) => return Err(error),
        }
        Ok(())
    }

    fn auto_approve(&self, id: &str, scope: &Scope) -> Result<Request> {
        self.route_request(id, scope)
    }

    pub fn create(&self, proposal: Proposal) -> Result<Request> {
        self.create_with_origin(proposal, "")
    }

    /// The request as stored, without the presentation changes of [`Service::get`].
    /// For tests of a domain adapter outside this module.
    #[cfg(test)]
    pub(crate) fn stored_for_test(&self, id: &str) -> Result<Request> {
        self.store.load(id)
    }

    /// Creates an approval with a trusted passive-presentation origin.
    /// The origin changes delivery only; scope remains the authority boundary.
    pub(crate) fn create_with_origin(&self, proposal: Proposal, origin: &str) -> Result<Request> {
        if !origin.is_empty() {
            otto_session::origin::Origin::parse(origin)
                .map_err(|_| Error::Invalid("invalid approval presentation origin"))?;
        }
        let now = Utc::now().timestamp();
        let mut bytes = [0; 16];
        std::fs::File::open("/dev/urandom")
            .and_then(|mut file| file.read_exact(&mut bytes))
            .map_err(|_| Error::Unavailable)?;
        let mut request = Request {
            options: vec![
                Choice {
                    id: "approve".into(),
                    label: if proposal.kind == Kind::Confirmation {
                        "Approve proposal"
                    } else if proposal.persistent {
                        "Approve persistent authorization"
                    } else {
                        "Approve operation"
                    }
                    .into(),
                    scope: if proposal.persistent {
                        "persistent"
                    } else {
                        "once"
                    }
                    .into(),
                },
                Choice {
                    id: "reject".into(),
                    label: "Reject".into(),
                    scope: "once".into(),
                },
            ],
            schema_version: 1,
            id: bytes.iter().map(|b| format!("{b:02x}")).collect(),
            revision: 1,
            kind: proposal.kind,
            authority: proposal.authority,
            scope: proposal.scope,
            presentation_origin: origin.to_owned(),
            subject: proposal.subject,
            title: proposal.title,
            reason: proposal.reason,
            persistent: proposal.persistent,
            blocking: proposal.blocking,
            created_at: now,
            expires_at: proposal.expires_at,
            state: State::Pending,
            decision: None,
            application: if proposal.kind == Kind::Confirmation {
                ApplicationState::NotRequired
            } else {
                ApplicationState::NotStarted
            },
            application_error: String::new(),
            review: None,
        };
        request.review = Some(self.creation_review(&request)?);
        request.validate().map_err(Error::Invalid)?;
        let key = if request.subject.source == "memory" {
            digest(&serde_json::to_string(&request.subject).map_err(|_| Error::Unavailable)?)
        } else {
            digest(
                &serde_json::to_string(&(
                    &request.scope,
                    &request.subject,
                    request.kind,
                    request.authority,
                    &request.title,
                    &request.reason,
                    request.persistent,
                    request.blocking,
                ))
                .map_err(|_| Error::Unavailable)?,
            )
        };
        let (request, created) = self.store.create(&key, &request)?;
        if !created {
            return Ok(request);
        }
        self.changed();
        self.auto_approve(&request.id, &request.scope)
    }
    pub fn get(&self, id: &str, workspace: &str, session: Option<&str>) -> Result<Request> {
        let request = self.store.load(id)?;
        if !self.store.visible(id, workspace, session)? {
            return Err(Error::Unauthorized);
        }
        self.hydrate(request)
    }
    fn expire_pending(&self) -> Result<()> {
        let now = Utc::now().timestamp();
        for request in self.store.pending()? {
            if request.expires_at.is_some_and(|expiry| expiry <= now) {
                match self.mutate(&request.id, &request.scope, |r| {
                    if r.state != State::Pending {
                        return Ok(false);
                    }
                    r.state = State::Expired;
                    Ok(true)
                }) {
                    Ok(_) | Err(Error::Conflict) => {}
                    Err(error) => return Err(error),
                }
            }
        }
        Ok(())
    }
    pub fn snapshot(&self, workspace: &str, session: Option<&str>) -> Result<Snapshot> {
        self.expire_pending()?;
        let mut snapshot = self.store.snapshot(workspace, session)?;
        snapshot.requests = snapshot
            .requests
            .into_iter()
            .filter(Request::is_human_pending)
            .map(|r| self.hydrate(r))
            .collect::<Result<_>>()?;
        snapshot.requests.retain(Request::is_human_pending);
        Ok(snapshot)
    }
    pub fn activity_snapshot(
        &self,
        workspace: &str,
        session: Option<&str>,
    ) -> Result<ApprovalActivity> {
        let snapshot = self.store.snapshot(workspace, session)?;
        let mut requests = Vec::new();
        let mut reviewing = Vec::new();
        for request in snapshot.requests {
            if request.is_human_pending() {
                let request = self.hydrate(request)?;
                if request.is_human_pending() {
                    requests.push(request);
                }
            } else if request
                .review
                .as_ref()
                .is_some_and(|r| matches!(r.status, ReviewStatus::Queued | ReviewStatus::Reviewing))
            {
                let request = self.hydrate(request)?;
                if request.state == State::Pending {
                    reviewing.push(request);
                }
            }
        }
        let mut decided = Vec::new();
        for request in self.store.applications(workspace)? {
            if self.store.visible(&request.id, workspace, session)? {
                decided.push(request);
            }
        }
        let application_failed = decided
            .iter()
            .filter(|r| {
                matches!(
                    r.application,
                    ApplicationState::Failed | ApplicationState::Unknown
                )
            })
            .count();
        decided.sort_by_key(|r| {
            std::cmp::Reverse(r.decision.as_ref().map_or(r.created_at, |d| d.decided_at))
        });
        let recent_decisions = decided
            .into_iter()
            .take(20)
            .map(|r| self.hydrate(r))
            .collect::<Result<Vec<_>>>()?;
        let counts = ApprovalCounts {
            human_pending: requests.len(),
            reviewing: reviewing.len(),
            application_failed,
        };
        Ok(ApprovalActivity {
            requests,
            cursor: snapshot.cursor,
            reviewing,
            recent_decisions,
            counts,
        })
    }
    pub fn events(
        &self,
        workspace: &str,
        session: Option<&str>,
        after: i64,
    ) -> Result<Vec<Change>> {
        if after < 0 {
            return Err(Error::Invalid("negative event cursor"));
        }
        self.store.events(workspace, session, after)
    }
    pub fn decide(
        &self,
        id: &str,
        scope: &Scope,
        revision: u64,
        allow: bool,
        feedback: &str,
        actor: &Actor,
    ) -> Result<Request> {
        if feedback.len() > 8192 || actor.identity.is_empty() || actor.transport.is_empty() {
            return Err(Error::Invalid("invalid decision provenance or feedback"));
        }
        self.mutate(id, scope, |request| {
            Self::record_decision(request, revision, allow, feedback, actor, false)
        })
    }
    /// Commit the human decision and this process's application claim together.
    /// Used by native review handlers that preserve an edited domain request.
    pub(crate) fn decide_and_claim(
        &self,
        id: &str,
        scope: &Scope,
        revision: u64,
        allow: bool,
        actor: &Actor,
    ) -> Result<Request> {
        if actor.identity.is_empty() || actor.transport.is_empty() {
            return Err(Error::Invalid("invalid decision provenance"));
        }
        self.mutate(id, scope, |request| {
            Self::record_decision(request, revision, allow, "", actor, true)
        })
    }
    pub(super) fn record_decision(
        request: &mut Request,
        revision: u64,
        allow: bool,
        feedback: &str,
        actor: &Actor,
        claim: bool,
    ) -> Result<bool> {
        if request.revision != revision {
            return Err(Error::Conflict);
        }
        let wanted = if allow {
            State::Approved
        } else {
            State::Rejected
        };
        if !claim
            && request.state == wanted
            && request
                .decision
                .as_ref()
                .is_some_and(|d| d.feedback == feedback)
        {
            return Ok(false);
        }
        if request.state != State::Pending {
            return Err(Error::Conflict);
        }
        if request
            .expires_at
            .is_some_and(|time| time <= Utc::now().timestamp())
        {
            if claim {
                return Err(Error::Conflict);
            }
            request.state = State::Expired;
            return Ok(true);
        }
        if claim {
            if request.kind != Kind::Authorization
                || request.application != ApplicationState::NotStarted
            {
                return Err(Error::Conflict);
            }
            request.application = ApplicationState::Applying;
        }
        request.state = wanted;
        request.decision = Some(Decision {
            option_id: if allow { "approve" } else { "reject" }.into(),
            feedback: feedback.into(),
            actor: actor.identity.clone(),
            transport: actor.transport.clone(),
            decided_at: Utc::now().timestamp(),
        });
        if let Some(review) = request.review.as_mut() {
            review.status = ReviewStatus::Completed;
        }
        Ok(true)
    }
}
pub fn digest(value: &str) -> String {
    to_hex(&Sha256::digest(value.as_bytes()))
}

mod transitions;

#[cfg(test)]
mod tests;
