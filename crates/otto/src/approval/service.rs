//! Durable approval authority. Decisions and event cursors commit together.
//! Domain mutations are separate and reconciled through domain receipts; an
//! uncertain external action is never automatically replayed.
use super::contracts::*;
use super::repository::{ApprovalStore, Claim, ClaimChange};
use crate::memory::guard::{ContentGuard, DefaultGuard, ExactGuard};
use chrono::Utc;
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
    owner: String,
    store: Arc<dyn ApprovalStore>,
    changes: watch::Sender<u64>,
    secrets: RwLock<BTreeMap<String, ExactGuard>>,
    domains: RwLock<BTreeMap<(String, String), Arc<dyn Domain>>>,
}
enum Recovery {
    Active,
    Domain,
    Unknown,
}

impl Service {
    pub fn from_store(store: Arc<dyn ApprovalStore>) -> Result<Self> {
        let mut random = [0; 16];
        std::fs::File::open("/dev/urandom")
            .and_then(|mut file| file.read_exact(&mut random))
            .map_err(|_| Error::Unavailable)?;
        Ok(Self {
            worker_started: std::sync::atomic::AtomicBool::new(false),
            owner: random.iter().map(|b| format!("{b:02x}")).collect(),
            store,
            changes: watch::channel(0).0,
            secrets: RwLock::new(BTreeMap::new()),
            domains: RwLock::new(BTreeMap::new()),
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
    fn hydrate(&self, mut request: Request) -> Result<Request> {
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
            request.subject.details = domain.details(&request)?;
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
        for request in requests {
            if !matches!(
                request.application,
                ApplicationState::NotStarted | ApplicationState::Applying
            ) {
                continue;
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
        if crate::subagent::record::process_matches(claim.pid, &claim.started) != Some(false) {
            return Ok(Recovery::Active);
        }
        let (pid, started) = crate::subagent::record::current_process();
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

    pub fn protect_secrets(&self, secrets: &[String]) -> Result<()> {
        let key = digest(&serde_json::to_string(secrets).map_err(|_| Error::Unavailable)?);
        let guard = ExactGuard::new(secrets).map_err(|_| Error::Unavailable)?;
        self.secrets
            .write()
            .map_err(|_| Error::Unavailable)?
            .insert(key, guard);
        Ok(())
    }
    fn check_content(&self, text: &str) -> Result<()> {
        let input = crate::memory::GuardInput {
            fields: vec![crate::memory::GuardField {
                name: "approval".into(),
                value: text.into(),
                opaque: false,
            }],
        };
        DefaultGuard
            .check(&input)
            .map_err(|_| Error::Invalid("sensitive approval content"))?;
        for guard in self
            .secrets
            .read()
            .map_err(|_| Error::Unavailable)?
            .values()
        {
            guard
                .check(&input)
                .map_err(|_| Error::Invalid("sensitive approval content"))?;
        }
        Ok(())
    }

    pub fn changes(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }
    fn changed(&self) {
        self.changes.send_modify(|v| *v = v.wrapping_add(1));
    }

    /// A missing preference means AUTO for a session; workspace-only requests,
    /// independent tasks and every request that is not a bounded authorization
    /// still require manual approval.
    pub fn auto_mode(&self, scope: &Scope) -> Result<bool> {
        if scope.session_id.is_empty() || !scope.task_id.is_empty() {
            return Ok(false);
        }
        Ok(self.store.mode(scope)?.unwrap_or(true))
    }

    pub fn set_auto_mode(&self, scope: &Scope, automatic: bool) -> Result<()> {
        if scope.workspace.is_empty() || scope.session_id.is_empty() || !scope.task_id.is_empty() {
            return Err(Error::Invalid("approval mode requires a session"));
        }
        self.store.set_mode(scope, automatic)?;
        self.changed();
        if automatic {
            for request in self
                .snapshot(&scope.workspace, Some(&scope.session_id))?
                .requests
            {
                // Visibility alone must not give this session authority over a
                // workspace request or a different session's shared candidate.
                if request.scope.workspace == scope.workspace
                    && request.scope.session_id == scope.session_id
                    && request.scope.task_id.is_empty()
                {
                    self.auto_approve(&request.id, &request.scope)?;
                }
            }
        }
        Ok(())
    }

    fn auto_approve(&self, id: &str, scope: &Scope) -> Result<Request> {
        self.mutate_checked(id, scope, true, |request| {
            if request.state != State::Pending
                || !request.auto_decidable()
                || !self.auto_mode(scope)?
            {
                return Ok(false);
            }
            Self::record_decision(
                request,
                request.revision,
                true,
                "Automatically approved in AUTO mode.",
                &Actor::local("auto-mode"),
                false,
            )
        })
    }

    pub fn create(&self, proposal: Proposal) -> Result<Request> {
        let now = Utc::now().timestamp();
        let mut bytes = [0; 16];
        std::fs::File::open("/dev/urandom")
            .and_then(|mut file| file.read_exact(&mut bytes))
            .map_err(|_| Error::Unavailable)?;
        let request = Request {
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
        };
        request.validate().map_err(Error::Invalid)?;
        self.check_content(&request.subject.details)?;
        self.check_content(&request.title)?;
        self.check_content(&request.reason)?;
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
            .map(|r| self.hydrate(r))
            .collect::<Result<_>>()?;
        Ok(snapshot)
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
        self.check_content(feedback)?;
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
    fn record_decision(
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
        Ok(true)
    }
    pub fn withdraw(&self, id: &str, scope: &Scope, revision: u64) -> Result<Request> {
        self.mutate(id, scope, |request| {
            if request.revision != revision {
                return Err(Error::Conflict);
            }
            if request.state == State::Withdrawn {
                return Ok(false);
            }
            if request.state != State::Pending {
                return Err(Error::Conflict);
            }
            request.state = State::Withdrawn;
            Ok(true)
        })
    }
    pub fn application(
        &self,
        id: &str,
        scope: &Scope,
        expected: ApplicationState,
        next: ApplicationState,
        error: &str,
    ) -> Result<Request> {
        self.mutate(id, scope, |request| {
            if !matches!(
                request.state,
                State::Approved | State::Rejected | State::Withdrawn | State::Expired
            ) || request.application != expected
            {
                return Err(Error::Conflict);
            }
            if next == ApplicationState::Applying
                && request.state == State::Approved
                && request
                    .expires_at
                    .is_some_and(|time| time <= Utc::now().timestamp())
            {
                return Err(Error::Conflict);
            }
            if !matches!(
                (expected, next),
                (ApplicationState::NotStarted, ApplicationState::Applying)
                    | (
                        ApplicationState::Applying,
                        ApplicationState::Applied
                            | ApplicationState::Failed
                            | ApplicationState::Unknown
                    )
            ) {
                return Err(Error::Invalid("invalid application transition"));
            }
            request.application = next;
            request.application_error = error.into();
            Ok(true)
        })
    }
    fn mutate(
        &self,
        id: &str,
        scope: &Scope,
        update: impl Fn(&mut Request) -> Result<bool>,
    ) -> Result<Request> {
        self.mutate_checked(id, scope, false, update)
    }
    fn mutate_checked(
        &self,
        id: &str,
        scope: &Scope,
        automatic: bool,
        update: impl Fn(&mut Request) -> Result<bool>,
    ) -> Result<Request> {
        loop {
            let expected = self.store.load(id)?;
            if &expected.scope != scope {
                return Err(Error::Unauthorized);
            }
            let mut request = expected.clone();
            if update(&mut request)? {
                let claim = if request.application != expected.application {
                    if request.application == ApplicationState::Applying {
                        let (pid, started) = crate::subagent::record::current_process();
                        ClaimChange::Start(Claim {
                            owner: self.owner.clone(),
                            pid,
                            started,
                            native: expected.state == State::Pending,
                        })
                    } else if expected.application == ApplicationState::Applying {
                        ClaimChange::Finish(self.owner.clone())
                    } else {
                        ClaimChange::None
                    }
                } else {
                    ClaimChange::None
                };
                request = match self.store.commit(&expected, &request, claim, automatic) {
                    Ok(request) => request,
                    Err(Error::Conflict)
                        if self.store.load(id)? != expected
                            || (automatic && !expected.auto_decidable())
                            || (automatic && !self.auto_mode(scope)?) =>
                    {
                        continue;
                    }
                    Err(error) => return Err(error),
                };
            }
            self.changed();
            return Ok(request);
        }
    }
    pub async fn wait(
        &self,
        id: &str,
        scope: &Scope,
        cancel: &CancellationToken,
    ) -> Result<Request> {
        let mut changes = self.changes();
        let mut poll = tokio::time::interval(Duration::from_millis(200));
        loop {
            if cancel.is_cancelled() {
                let request = self.get(id, &scope.workspace, Some(&scope.session_id))?;
                if request.state == State::Pending {
                    return self.withdraw(id, scope, request.revision);
                }
                return Ok(request);
            }
            let request = self.auto_approve(id, scope)?;
            if request.state != State::Pending {
                return Ok(request);
            }
            if request
                .expires_at
                .is_some_and(|time| time <= Utc::now().timestamp())
            {
                return self.mutate(id, scope, |r| {
                    if r.state == State::Pending {
                        r.state = State::Expired;
                        Ok(true)
                    } else {
                        Ok(false)
                    }
                });
            }
            tokio::select! {
                () = cancel.cancelled() => {
                    match self.withdraw(id,scope,request.revision) { Ok(r) => return Ok(r), Err(Error::Conflict) => {}, Err(e) => return Err(e) }
                    return self.get(id,&scope.workspace,Some(&scope.session_id));
                },
                _ = changes.changed() => {},
                _ = poll.tick() => {},
            }
        }
    }
}
pub fn digest(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct TestStore {
        request: Mutex<Option<Request>>,
        events: Mutex<Vec<Change>>,
    }
    impl ApprovalStore for TestStore {
        fn mode(&self, _: &Scope) -> Result<Option<bool>> {
            Ok(Some(false))
        }
        fn set_mode(&self, _: &Scope, _: bool) -> Result<()> {
            Ok(())
        }
        fn create(&self, _: &str, request: &Request) -> Result<(Request, bool)> {
            *self.request.lock().unwrap() = Some(request.clone());
            Ok((request.clone(), true))
        }
        fn load(&self, _: &str) -> Result<Request> {
            self.request.lock().unwrap().clone().ok_or(Error::NotFound)
        }
        fn visible(&self, _: &str, workspace: &str, _: Option<&str>) -> Result<bool> {
            Ok(workspace == "/w")
        }
        fn pending(&self) -> Result<Vec<Request>> {
            Ok(vec![])
        }
        fn applications(&self, _: &str) -> Result<Vec<Request>> {
            Ok(vec![])
        }
        fn snapshot(&self, _: &str, _: Option<&str>) -> Result<Snapshot> {
            Ok(Snapshot {
                requests: vec![],
                cursor: 0,
            })
        }
        fn events(&self, _: &str, _: Option<&str>, _: i64) -> Result<Vec<Change>> {
            Ok(self.events.lock().unwrap().clone())
        }
        fn claim(&self, _: &str) -> Result<Option<Claim>> {
            Ok(None)
        }
        fn take_claim(&self, _: &str, _: &Claim, _: &Claim) -> Result<bool> {
            Ok(false)
        }
        fn commit(
            &self,
            expected: &Request,
            next: &Request,
            _: ClaimChange,
            _: bool,
        ) -> Result<Request> {
            let mut current = self.request.lock().unwrap();
            if current.as_ref() != Some(expected) {
                return Err(Error::Conflict);
            }
            *current = Some(next.clone());
            self.events.lock().unwrap().push(Change {
                sequence: 1,
                request: next.clone(),
            });
            Ok(next.clone())
        }
    }

    #[test]
    fn injected_store_keeps_revision_and_human_authority_in_the_service() {
        let store = Arc::new(TestStore {
            request: Mutex::new(None),
            events: Mutex::new(vec![]),
        });
        let service = Service::from_store(store.clone()).unwrap();
        let request = service
            .create(Proposal {
                kind: Kind::Confirmation,
                authority: Authority::Consent,
                scope: Scope {
                    workspace: "/w".into(),
                    session_id: "s".into(),
                    task_id: String::new(),
                    run_id: String::new(),
                },
                subject: Subject {
                    tool_call_id: String::new(),
                    source: "conversation".into(),
                    id: "plan".into(),
                    version: "1".into(),
                    details: "review".into(),
                },
                title: "Review".into(),
                reason: "scope".into(),
                persistent: false,
                blocking: true,
                expires_at: None,
            })
            .unwrap();
        let actor = Actor::local("test");
        assert!(matches!(
            service.decide(&request.id, &request.scope, 2, true, "", &actor),
            Err(Error::Conflict)
        ));
        let mut foreign = request.scope.clone();
        foreign.workspace = "/foreign".into();
        assert!(matches!(
            service.decide(&request.id, &foreign, 1, true, "", &actor),
            Err(Error::Unauthorized)
        ));
        assert!(matches!(
            service.decide(
                &request.id,
                &request.scope,
                1,
                true,
                "",
                &Actor {
                    identity: String::new(),
                    transport: "model".into()
                }
            ),
            Err(Error::Invalid(_))
        ));
        assert!(store.events.lock().unwrap().is_empty());
        let approved = service
            .decide(&request.id, &request.scope, 1, true, "", &actor)
            .unwrap();
        assert_eq!(approved.state, State::Approved);
        assert_eq!(approved.decision.unwrap().actor, "local-user");
        assert_eq!(store.events.lock().unwrap().len(), 1);
    }
}
