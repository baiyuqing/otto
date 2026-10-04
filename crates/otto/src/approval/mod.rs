//! Durable approval authority. Decisions and event cursors commit together.
//! Domain mutations are separate and reconciled through domain receipts; an
//! uncertain external action is never automatically replayed.
use crate::memory::guard::{ContentGuard, DefaultGuard, ExactGuard};
use crate::storage::{Connection, OptionalExtension, Transaction, params};
use chrono::Utc;
pub use otto_core::approval::*;
use sha2::{Digest, Sha256};
pub mod config;
pub mod memory;
pub mod reflection;
pub mod workflow;
use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;
use std::sync::{Mutex, RwLock};
use std::time::Duration;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("approval storage is unavailable")]
    Unavailable,
    #[error("approval not found")]
    NotFound,
    #[error("approval scope is unauthorized")]
    Unauthorized,
    #[error("approval changed or was already decided")]
    Conflict,
    #[error("invalid approval input: {0}")]
    Invalid(&'static str),
}
pub type Result<T> = std::result::Result<T, Error>;

/// Constructed by admitted native/HTTP/ACP/chat handlers, never model arguments.
#[derive(Debug, Clone)]
pub struct Actor {
    pub identity: String,
    pub transport: String,
}
impl Actor {
    pub fn local(transport: &str) -> Self {
        Self {
            identity: "local-user".into(),
            transport: transport.into(),
        }
    }
}

pub struct Proposal {
    pub kind: Kind,
    pub scope: Scope,
    pub subject: Subject,
    pub title: String,
    pub reason: String,
    pub persistent: bool,
    pub blocking: bool,
    pub expires_at: Option<i64>,
}

pub trait Domain: Send + Sync {
    fn details(&self, request: &Request) -> Result<String>;
    /// Must be idempotent using a receipt committed with the domain mutation.
    fn apply(&self, request: &Request) -> Result<()>;
}

pub struct Service {
    worker_started: std::sync::atomic::AtomicBool,
    owner: String,
    connection: Mutex<Connection>,
    changes: watch::Sender<u64>,
    secrets: RwLock<BTreeMap<String, ExactGuard>>,
    domains: RwLock<BTreeMap<(String, String), Arc<dyn Domain>>>,
}
enum Recovery {
    Active,
    Domain,
    Unknown,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS approvals (
 id TEXT PRIMARY KEY, subject_key TEXT NOT NULL UNIQUE,
 workspace TEXT NOT NULL, session_id TEXT NOT NULL,
 state TEXT NOT NULL, body TEXT NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS approval_events (
 sequence INTEGER PRIMARY KEY AUTOINCREMENT,
 approval_id TEXT NOT NULL, workspace TEXT NOT NULL,
 session_id TEXT NOT NULL, body TEXT NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS approval_claims (
 approval_id TEXT PRIMARY KEY REFERENCES approvals(id), owner TEXT NOT NULL,
 pid INTEGER NOT NULL, started TEXT NOT NULL, recovery TEXT NOT NULL DEFAULT 'domain'
) STRICT;
CREATE TABLE IF NOT EXISTS approval_visibility (
 approval_id TEXT NOT NULL REFERENCES approvals(id), workspace TEXT NOT NULL,
 session_id TEXT NOT NULL, PRIMARY KEY(approval_id,workspace,session_id)
) STRICT;
CREATE INDEX IF NOT EXISTS approvals_scope ON approvals(workspace, session_id, state);
";

impl Service {
    pub fn open(path: &Path) -> Result<Self> {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|_| Error::Unavailable)?;
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .map_err(|_| Error::Unavailable)?;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|_| Error::Unavailable)?;
        Self::from_connection(Connection::open(path).map_err(|_| Error::Unavailable)?)
    }
    pub fn in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory().map_err(|_| Error::Unavailable)?)
    }
    fn from_connection(mut connection: Connection) -> Result<Self> {
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(|_| Error::Unavailable)?;
        let tx = connection.transaction().map_err(|_| Error::Unavailable)?;
        let version: i64 = tx
            .query_row("PRAGMA user_version", (), |row| row.get(0))
            .map_err(|_| Error::Unavailable)?;
        if !(0..=2).contains(&version) {
            return Err(Error::Invalid("unsupported approval database version"));
        }
        if version == 1 {
            // Older claims lack application intent. Preserve them without
            // inventing permission to replay an interrupted native review.
            tx.execute_batch(
                "ALTER TABLE approval_claims ADD COLUMN recovery TEXT NOT NULL DEFAULT 'native'",
            )
            .map_err(|_| Error::Unavailable)?;
        }
        tx.execute_batch(SCHEMA).map_err(|_| Error::Unavailable)?;
        tx.execute_batch("PRAGMA user_version=2")
            .map_err(|_| Error::Unavailable)?;
        tx.commit().map_err(|_| Error::Unavailable)?;
        let mut random = [0; 16];
        std::fs::File::open("/dev/urandom")
            .and_then(|mut file| file.read_exact(&mut random))
            .map_err(|_| Error::Unavailable)?;
        Ok(Self {
            worker_started: std::sync::atomic::AtomicBool::new(false),
            owner: random.iter().map(|b| format!("{b:02x}")).collect(),
            connection: Mutex::new(connection),
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
        let requests = {
            let connection = self.connection.lock().map_err(|_| Error::Unavailable)?;
            let mut statement=connection.prepare("SELECT DISTINCT a.body FROM approvals a JOIN approval_visibility v ON v.approval_id=a.id WHERE v.workspace=?1 AND a.state IN ('approved','rejected','withdrawn','expired')").map_err(|_|Error::Unavailable)?;
            statement
                .query_map([workspace], |row| row.get::<String>(0))
                .map_err(|_| Error::Unavailable)?
                .map(|body| decode(&body.map_err(|_| Error::Unavailable)?))
                .collect::<Result<Vec<_>>>()?
        };
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
        let connection = self.connection.lock().map_err(|_| Error::Unavailable)?;
        let claim = connection
            .query_row(
                "SELECT owner,pid,started,recovery FROM approval_claims WHERE approval_id=?1",
                [id],
                |row| {
                    Ok((
                        row.get::<String>(0)?,
                        row.get::<i64>(1)?,
                        row.get::<String>(2)?,
                        row.get::<String>(3)?,
                    ))
                },
            )
            .optional()
            .map_err(|_| Error::Unavailable)?;
        let Some((owner, pid, started, recovery)) = claim else {
            return Ok(Recovery::Active);
        };
        if crate::subagent::record::process_matches(pid, &started) != Some(false) {
            return Ok(Recovery::Active);
        }
        let (current_pid, current_start) = crate::subagent::record::current_process();
        let changed=connection.execute("UPDATE approval_claims SET owner=?2,pid=?3,started=?4 WHERE approval_id=?1 AND owner=?5",params![id,self.owner,current_pid,current_start,owner]).map_err(|_|Error::Unavailable)?;
        Ok(if changed != 1 {
            Recovery::Active
        } else if recovery == "domain" {
            Recovery::Domain
        } else {
            Recovery::Unknown
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
                    &request.title,
                    &request.reason,
                    request.persistent,
                    request.blocking,
                ))
                .map_err(|_| Error::Unavailable)?,
            )
        };
        let mut connection = self.connection.lock().map_err(|_| Error::Unavailable)?;
        let tx = connection.transaction().map_err(|_| Error::Unavailable)?;
        let existing = tx
            .query_row(
                "SELECT body FROM approvals WHERE subject_key=?1",
                [key.as_str()],
                |row| row.get::<String>(0),
            )
            .optional()
            .map_err(|_| Error::Unavailable)?;
        if let Some(body) = existing {
            let existing = decode(&body)?;
            tx.execute(
                "INSERT OR IGNORE INTO approval_visibility VALUES (?1,?2,?3)",
                params![
                    existing.id,
                    request.scope.workspace,
                    request.scope.session_id
                ],
            )
            .map_err(|_| Error::Unavailable)?;
            tx.commit().map_err(|_| Error::Unavailable)?;
            return Ok(existing);
        }
        tx.execute(
            "INSERT INTO approvals VALUES (?1,?2,?3,?4,'pending',?5)",
            params![
                request.id,
                key,
                request.scope.workspace,
                request.scope.session_id,
                encode(&request)?
            ],
        )
        .map_err(|_| Error::Unavailable)?;
        tx.execute(
            "INSERT INTO approval_visibility VALUES (?1,?2,?3)",
            params![
                request.id,
                request.scope.workspace,
                request.scope.session_id
            ],
        )
        .map_err(|_| Error::Unavailable)?;
        append_event(&tx, &request)?;
        tx.commit().map_err(|_| Error::Unavailable)?;
        drop(connection);
        self.changed();
        Ok(request)
    }
    pub fn get(&self, id: &str, workspace: &str, session: Option<&str>) -> Result<Request> {
        let connection = self.connection.lock().map_err(|_| Error::Unavailable)?;
        let body = connection
            .query_row("SELECT body FROM approvals WHERE id=?1", [id], |row| {
                row.get::<String>(0)
            })
            .optional()
            .map_err(|_| Error::Unavailable)?
            .ok_or(Error::NotFound)?;
        let request = decode(&body)?;
        let visible=connection.query_row("SELECT 1 FROM approval_visibility WHERE approval_id=?1 AND workspace=?2 AND (?3='' OR session_id=?3 OR session_id='') LIMIT 1",params![id,workspace,session.unwrap_or("")],|row|row.get::<i64>(0)).optional().map_err(|_|Error::Unavailable)?.is_some();
        if !visible {
            return Err(Error::Unauthorized);
        }
        drop(connection);
        self.hydrate(request)
    }
    fn expire_pending(&self) -> Result<()> {
        let mut connection = self.connection.lock().map_err(|_| Error::Unavailable)?;
        let tx = connection.transaction().map_err(|_| Error::Unavailable)?;
        let bodies = {
            let mut statement = tx
                .prepare("SELECT body FROM approvals WHERE state='pending'")
                .map_err(|_| Error::Unavailable)?;
            statement
                .query_map((), |row| row.get::<String>(0))
                .map_err(|_| Error::Unavailable)?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|_| Error::Unavailable)?
        };
        let now = Utc::now().timestamp();
        let mut changed = false;
        for body in bodies {
            let mut request = decode(&body)?;
            if request.expires_at.is_some_and(|expiry| expiry <= now) {
                request.state = State::Expired;
                tx.execute(
                    "UPDATE approvals SET state='expired',body=?2 WHERE id=?1",
                    params![request.id, encode(&request)?],
                )
                .map_err(|_| Error::Unavailable)?;
                append_event(&tx, &request)?;
                changed = true;
            }
        }
        tx.commit().map_err(|_| Error::Unavailable)?;
        drop(connection);
        if changed {
            self.changed();
        }
        Ok(())
    }

    pub fn snapshot(&self, workspace: &str, session: Option<&str>) -> Result<Snapshot> {
        self.expire_pending()?;
        let mut connection = self.connection.lock().map_err(|_| Error::Unavailable)?;
        let tx = connection.transaction().map_err(|_| Error::Unavailable)?;
        let cursor = tx
            .query_row(
                "SELECT COALESCE(MAX(sequence),0) FROM approval_events",
                (),
                |row| row.get(0),
            )
            .map_err(|_| Error::Unavailable)?;
        let mut statement = tx.prepare("SELECT DISTINCT a.body FROM approvals a JOIN approval_visibility v ON v.approval_id=a.id WHERE v.workspace=?1 AND (?2='' OR v.session_id=?2 OR v.session_id='') AND a.state='pending' ORDER BY a.id")
            .map_err(|_| Error::Unavailable)?;
        let bodies = statement
            .query_map(params![workspace, session.unwrap_or("")], |row| {
                row.get::<String>(0)
            })
            .map_err(|_| Error::Unavailable)?;
        let requests = bodies
            .map(|row| decode(&row.map_err(|_| Error::Unavailable)?))
            .collect::<Result<Vec<_>>>()?;
        drop(statement);
        tx.commit().map_err(|_| Error::Unavailable)?;
        let requests = requests
            .into_iter()
            .map(|r| self.hydrate(r))
            .collect::<Result<Vec<_>>>()?;
        Ok(Snapshot { requests, cursor })
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
        let connection = self.connection.lock().map_err(|_| Error::Unavailable)?;
        let mut statement = connection.prepare("SELECT DISTINCT e.sequence,e.body FROM approval_events e JOIN approval_visibility v ON v.approval_id=e.approval_id WHERE v.workspace=?1 AND (?2='' OR v.session_id=?2 OR v.session_id='') AND e.sequence>?3 ORDER BY e.sequence LIMIT 100")
            .map_err(|_| Error::Unavailable)?;
        let rows = statement
            .query_map(params![workspace, session.unwrap_or(""), after], |row| {
                Ok((row.get::<i64>(0)?, row.get::<String>(1)?))
            })
            .map_err(|_| Error::Unavailable)?;
        rows.map(|row| {
            let (sequence, body) = row.map_err(|_| Error::Unavailable)?;
            Ok(Change {
                sequence,
                request: decode(&body)?,
            })
        })
        .collect()
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
        update: impl FnOnce(&mut Request) -> Result<bool>,
    ) -> Result<Request> {
        let mut connection = self.connection.lock().map_err(|_| Error::Unavailable)?;
        let tx = connection.transaction().map_err(|_| Error::Unavailable)?;
        let body = tx
            .query_row("SELECT body FROM approvals WHERE id=?1", [id], |row| {
                row.get::<String>(0)
            })
            .optional()
            .map_err(|_| Error::Unavailable)?
            .ok_or(Error::NotFound)?;
        let mut request = decode(&body)?;
        if &request.scope != scope {
            return Err(Error::Unauthorized);
        }
        let previous_application = request.application;
        let previous_state = request.state;
        if update(&mut request)? {
            if request.application != previous_application {
                if request.application == ApplicationState::Applying {
                    let (pid, started) = crate::subagent::record::current_process();
                    tx.execute(
                        "INSERT INTO approval_claims (approval_id,owner,pid,started,recovery) VALUES (?1,?2,?3,?4,?5)",
                        params![id, self.owner, pid, started, if previous_state == State::Pending { "native" } else { "domain" }],
                    )
                    .map_err(|_| Error::Conflict)?;
                } else if previous_application == ApplicationState::Applying {
                    let owner = tx
                        .query_row(
                            "SELECT owner FROM approval_claims WHERE approval_id=?1",
                            [id],
                            |row| row.get::<String>(0),
                        )
                        .map_err(|_| Error::Unavailable)?;
                    if owner != self.owner {
                        return Err(Error::Conflict);
                    }
                }
            }
            let state = serde_json::to_value(request.state).map_err(|_| Error::Unavailable)?;
            tx.execute(
                "UPDATE approvals SET state=?2,body=?3 WHERE id=?1",
                params![
                    id,
                    state.as_str().ok_or(Error::Unavailable)?,
                    encode(&request)?
                ],
            )
            .map_err(|_| Error::Unavailable)?;
            append_event(&tx, &request)?;
        }
        tx.commit().map_err(|_| Error::Unavailable)?;
        drop(connection);
        self.changed();
        Ok(request)
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
            let request = self.get(id, &scope.workspace, Some(&scope.session_id))?;
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
fn encode(request: &Request) -> Result<String> {
    serde_json::to_string(request).map_err(|_| Error::Unavailable)
}
fn decode(body: &str) -> Result<Request> {
    serde_json::from_str(body).map_err(|_| Error::Unavailable)
}
fn append_event(tx: &Transaction<'_>, request: &Request) -> Result<()> {
    tx.execute(
        "INSERT INTO approval_events (approval_id,workspace,session_id,body) VALUES (?1,?2,?3,?4)",
        params![
            request.id,
            request.scope.workspace,
            request.scope.session_id,
            encode(request)?
        ],
    )
    .map(|_| ())
    .map_err(|_| Error::Unavailable)
}
pub fn digest(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn proposal() -> Proposal {
        Proposal {
            kind: Kind::Confirmation,
            scope: Scope {
                workspace: "/w".into(),
                session_id: "s".into(),
                task_id: String::new(),
                run_id: String::new(),
            },
            subject: Subject {
                tool_call_id: String::new(),
                source: "conversation".into(),
                id: "operation-1".into(),
                version: "1".into(),
                details: "plan".into(),
            },
            title: "Review plan".into(),
            reason: "scope".into(),
            persistent: false,
            blocking: true,
            expires_at: None,
        }
    }
    #[test]
    fn future_database_version_is_rejected_without_modifying_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("approvals.db");
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch("PRAGMA user_version=3").unwrap();
        assert!(matches!(Service::open(&path), Err(Error::Invalid(_))));
        let version: i64 = connection
            .query_row("PRAGMA user_version", (), |row| row.get(0))
            .unwrap();
        assert_eq!(version, 3);
    }
    #[test]
    fn decisions_deduplicate_conflict_and_survive_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("approvals.db");
        let service = Service::open(&path).unwrap();
        let r = service.create(proposal()).unwrap();
        assert_eq!(service.create(proposal()).unwrap().id, r.id);
        let snapshot = service.snapshot("/w", Some("s")).unwrap();
        assert_eq!(snapshot.requests.len(), 1);
        let approved = service
            .decide(&r.id, &r.scope, 1, true, "go", &Actor::local("test"))
            .unwrap();
        assert_eq!(approved.state, State::Approved);
        assert_eq!(
            service
                .decide(&r.id, &r.scope, 1, true, "go", &Actor::local("test"))
                .unwrap(),
            approved
        );
        assert!(matches!(
            service.decide(&r.id, &r.scope, 1, false, "", &Actor::local("test")),
            Err(Error::Conflict)
        ));
        assert!(matches!(
            service.get(&r.id, "/foreign", None),
            Err(Error::Unauthorized)
        ));
        assert_eq!(
            service
                .events("/w", Some("s"), snapshot.cursor)
                .unwrap()
                .len(),
            1
        );
        drop(service);
        let reopened = Service::open(&path).unwrap();
        assert_eq!(reopened.get(&r.id, "/w", Some("s")).unwrap(), approved);
        assert!(
            reopened
                .snapshot("/w", Some("s"))
                .unwrap()
                .requests
                .is_empty()
        );
    }
    #[tokio::test]
    async fn cancelled_wait_withdraws_without_approval() {
        let service = Service::in_memory().unwrap();
        let r = service.create(proposal()).unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            service.wait(&r.id, &r.scope, &cancel).await.unwrap().state,
            State::Withdrawn
        );
        assert!(matches!(
            service.decide(&r.id, &r.scope, 1, true, "", &Actor::local("test")),
            Err(Error::Conflict)
        ));
    }
    #[test]
    fn legacy_claims_migrate_conservatively_without_replaying_a_native_review() {
        struct NoReplay;
        impl Domain for NoReplay {
            fn details(&self, request: &Request) -> Result<String> {
                Ok(request.subject.details.clone())
            }
            fn apply(&self, _: &Request) -> Result<()> {
                panic!("interrupted native reviews must not be replayed")
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("approvals.db");
        let service = Service::open(&path).unwrap();
        let mut proposal = proposal();
        proposal.kind = Kind::Authorization;
        proposal.subject.source = "native-review".into();
        let request = service.create(proposal).unwrap();
        service
            .decide_and_claim(
                &request.id,
                &request.scope,
                1,
                true,
                &Actor::local("review"),
            )
            .unwrap();
        drop(service);
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch("ALTER TABLE approval_claims RENAME TO newer_claims;
            CREATE TABLE approval_claims (approval_id TEXT PRIMARY KEY, owner TEXT NOT NULL, pid INTEGER NOT NULL, started TEXT NOT NULL) STRICT;
            INSERT INTO approval_claims SELECT approval_id,owner,2147483647,'2000-01-01T00:00:00Z' FROM newer_claims;
            DROP TABLE newer_claims; PRAGMA user_version=1;").unwrap();
        drop(connection);
        let service = Service::open(&path).unwrap();
        service
            .register_domain("/w", "native-review", Arc::new(NoReplay))
            .unwrap();
        service.apply_available("/w").unwrap();
        assert_eq!(
            service
                .get(&request.id, "/w", Some("s"))
                .unwrap()
                .application,
            ApplicationState::Unknown
        );
        let version: i64 = service
            .connection
            .lock()
            .unwrap()
            .query_row("PRAGMA user_version", (), |row| row.get(0))
            .unwrap();
        assert_eq!(version, 2);
    }
    #[test]
    fn native_review_claim_is_committed_with_the_decision_before_maintenance() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("approvals.db");
        let one = Service::open(&path).unwrap();
        let other = Service::open(&path).unwrap();
        let mut proposal = proposal();
        proposal.kind = Kind::Authorization;
        proposal.subject.source = "memory".into();
        let request = one.create(proposal).unwrap();
        let decided = one
            .decide_and_claim(
                &request.id,
                &request.scope,
                1,
                true,
                &Actor::local("review"),
            )
            .unwrap();
        assert_eq!(decided.application, ApplicationState::Applying);
        other.apply_available("/w").unwrap();
        assert_eq!(
            other.get(&request.id, "/w", Some("s")).unwrap().application,
            ApplicationState::Applying
        );
        assert!(matches!(
            other.application(
                &request.id,
                &request.scope,
                ApplicationState::NotStarted,
                ApplicationState::Applying,
                ""
            ),
            Err(Error::Conflict)
        ));
        one.application(
            &request.id,
            &request.scope,
            ApplicationState::Applying,
            ApplicationState::Applied,
            "",
        )
        .unwrap();
    }
    #[test]
    fn separate_connections_have_one_decision_winner() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("approvals.db");
        let one = Service::open(&path).unwrap();
        let two = Service::open(&path).unwrap();
        let r = one.create(proposal()).unwrap();
        one.decide(&r.id, &r.scope, 1, true, "", &Actor::local("one"))
            .unwrap();
        assert!(matches!(
            two.decide(&r.id, &r.scope, 1, false, "", &Actor::local("two")),
            Err(Error::Conflict)
        ));
    }
    #[test]
    fn recovery_does_not_take_over_a_live_claim_or_replay_a_lost_external_action() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("approvals.db");
        let one = Service::open(&path).unwrap();
        let two = Service::open(&path).unwrap();
        let mut p = proposal();
        p.kind = Kind::Authorization;
        p.subject.source = "sandbox".into();
        let request = one.create(p).unwrap();
        one.decide(
            &request.id,
            &request.scope,
            1,
            true,
            "",
            &Actor::local("test-human"),
        )
        .unwrap();
        one.application(
            &request.id,
            &request.scope,
            ApplicationState::NotStarted,
            ApplicationState::Applying,
            "",
        )
        .unwrap();
        two.apply_available("/w").unwrap();
        assert_eq!(
            two.get(&request.id, "/w", Some("s")).unwrap().application,
            ApplicationState::Applying
        );
        one.connection
            .lock()
            .unwrap()
            .execute(
                "UPDATE approval_claims SET pid=2147483000 WHERE approval_id=?1",
                [request.id.as_str()],
            )
            .unwrap();
        two.apply_available("/w").unwrap();
        assert_eq!(
            two.get(&request.id, "/w", Some("s")).unwrap().application,
            ApplicationState::Unknown
        );
        assert!(matches!(
            one.application(
                &request.id,
                &request.scope,
                ApplicationState::Applying,
                ApplicationState::Applied,
                ""
            ),
            Err(Error::Conflict)
        ));
    }
}
