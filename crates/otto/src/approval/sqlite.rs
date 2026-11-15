use crate::approval::repository::*;
use crate::approval::{Error, Result};
use crate::storage::drivers::sqlite::SqliteDriver;
use crate::storage::{Connection, OptionalExtension, Transaction, params};
use crate::storage::{OpenOptions, SqlDriver};
use otto_core::approval::*;
use serde::{Deserialize, Serialize};
use std::{path::Path, sync::Mutex, time::Duration};

mod checks;
mod commit;
mod schema;

use checks::*;
use schema::*;

pub struct Store {
    connection: Mutex<Connection>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with_driver(&SqliteDriver, path)
    }

    pub fn open_with_driver(driver: &dyn SqlDriver, path: &Path) -> Result<Self> {
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
        let mut connection = Connection::open_with_driver(driver, path, OpenOptions::default())
            .map_err(storage_error)?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(storage_error)?;
        let tx = connection.transaction().map_err(storage_error)?;
        let version: i64 = tx
            .query_row("PRAGMA user_version", (), |r| r.get(0))
            .map_err(storage_error)?;
        if !(0..=4).contains(&version) {
            return Err(Error::Invalid("unsupported approval database version"));
        }
        if (1..=3).contains(&version) {
            backup_before_v4(driver, path, &tx, version)?;
        }
        initialize_tx(&tx)?;
        tx.commit().map_err(storage_error)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }
    pub fn in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory().map_err(storage_error)?)
    }
    pub(crate) fn from_connection(mut connection: Connection) -> Result<Self> {
        initialize(&mut connection)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }
}

impl ApprovalStore for Store {
    fn mode(&self, scope: &Scope) -> Result<Option<ApprovalMode>> {
        Ok(self.settings(scope)?.map(|settings| settings.mode()))
    }
    fn set_mode(&self, scope: &Scope, mode: ApprovalMode) -> Result<()> {
        let mut connection = self.connection.lock().map_err(|_| Error::Unavailable)?;
        let tx = connection.transaction().map_err(storage_error)?;
        let current = read_settings(&tx, scope)?.unwrap_or_default();
        let mut next = ApprovalSettings::from_mode(mode);
        next.revision = current.revision + 1;
        write_settings(&tx, scope, next)?;
        tx.commit().map_err(storage_error)
    }
    fn settings(&self, scope: &Scope) -> Result<Option<ApprovalSettings>> {
        let connection = self.connection.lock().map_err(|_| Error::Unavailable)?;
        read_settings(&connection, scope)
    }
    fn set_settings(
        &self,
        scope: &Scope,
        revision: u64,
        next: ApprovalSettings,
    ) -> Result<ApprovalSettings> {
        next.validate().map_err(Error::Invalid)?;
        let mut connection = self.connection.lock().map_err(|_| Error::Unavailable)?;
        let tx = connection.transaction().map_err(storage_error)?;
        if read_settings(&tx, scope)?.unwrap_or_default().revision != revision
            || next.revision != revision.checked_add(1).ok_or(Error::Conflict)?
        {
            return Err(Error::Conflict);
        }
        write_settings(&tx, scope, next)?;
        tx.commit().map_err(storage_error)?;
        Ok(next)
    }
    fn review_context(&self, scope: &Scope, evidence: &str, policy: &str) -> Result<()> {
        self.connection.lock().map_err(|_| Error::Unavailable)?.execute(
            "INSERT INTO approval_review_contexts VALUES (?1,?2,?3,?4) ON CONFLICT(workspace,session_id) DO UPDATE SET evidence_digest=excluded.evidence_digest,policy_digest=excluded.policy_digest",
            params![scope.workspace, scope.session_id, evidence, policy],
        ).map(|_|()).map_err(storage_error)
    }
    fn review_snapshot(&self, scope: &Scope) -> Result<Option<(String, String)>> {
        self.connection
            .lock()
            .map_err(|_| Error::Unavailable)?
            .query_row(
                "SELECT evidence_digest,policy_digest FROM approval_review_contexts \
            WHERE workspace=?1 AND session_id=?2",
                params![scope.workspace, scope.session_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(storage_error)
    }
    fn policy_config(
        &self,
        workspace: &str,
        defaults: ApprovalSettings,
        digest: &str,
    ) -> Result<()> {
        let scope = Scope {
            workspace: workspace.into(),
            session_id: String::new(),
            task_id: String::new(),
            run_id: String::new(),
        };
        let mut connection = self.connection.lock().map_err(|_| Error::Unavailable)?;
        let tx = connection.transaction().map_err(storage_error)?;
        let old = read_settings(&tx, &scope)?;
        let old_digest: Option<String> = tx
            .query_row(
                "SELECT policy_digest FROM approval_review_contexts WHERE workspace=?1 \
            AND session_id=''",
                [workspace],
                |r| r.get(0),
            )
            .optional()
            .map_err(storage_error)?;
        let mut defaults = defaults;
        if let Some(forced) = read_managed(&tx, workspace)? {
            defaults.policy = forced.policy.unwrap_or(defaults.policy);
            defaults.reviewer = forced.reviewer.unwrap_or(defaults.reviewer);
        }
        defaults.revision = match old {
            Some(old) if old_digest.as_deref() == Some(digest) => old.revision,
            Some(old) => old.revision.checked_add(1).ok_or(Error::Conflict)?,
            None => 1,
        };
        write_settings(&tx, &scope, defaults)?;
        tx.execute(
            "INSERT INTO approval_review_contexts VALUES (?1,'','policy',?2) ON \
            CONFLICT(workspace,session_id) DO UPDATE SET \
            policy_digest=excluded.policy_digest",
            params![workspace, digest],
        )
        .map_err(storage_error)?;
        if old_digest.as_deref() != Some(digest) {
            // Invalidate every old root snapshot in the same transaction as
            // the policy epoch. Live contexts are republished by the service.
            tx.execute("UPDATE approval_review_contexts SET policy_digest=?2 WHERE workspace=?1 AND session_id<>''",params![workspace,format!("policy-changed:{digest}")]).map_err(storage_error)?;
        }
        tx.commit().map_err(storage_error)
    }
    fn review_denials(&self, scope: &Scope, evidence: &str) -> Result<u64> {
        self.connection
            .lock()
            .map_err(|_| Error::Unavailable)?
            .query_row(
                "SELECT denials FROM approval_review_denials WHERE workspace=?1 AND \
            session_id=?2 AND task_id=?3 AND run_id=?4 AND evidence_digest=?5",
                params![
                    scope.workspace,
                    scope.session_id,
                    scope.task_id,
                    scope.run_id,
                    evidence
                ],
                |r| r.get(0),
            )
            .optional()
            .map(|v| v.unwrap_or(0))
            .map_err(storage_error)
    }
    fn managed_policy(
        &self,
        workspace: &str,
        forced: &otto_core::config::ApprovalConfig,
    ) -> Result<()> {
        let mut connection = self.connection.lock().map_err(|_| Error::Unavailable)?;
        let tx = connection.transaction().map_err(storage_error)?;
        let row = ManagedRow {
            policy: forced.policy,
            reviewer: forced.reviewer,
        };
        tx.execute(
            "INSERT INTO approval_managed_policy VALUES (?1,?2) ON \
            CONFLICT(workspace) DO UPDATE SET body=excluded.body",
            params![
                workspace,
                serde_json::to_string(&row).map_err(|_| Error::Unavailable)?
            ],
        )
        .map_err(storage_error)?;
        let mut statement = tx
            .prepare("SELECT session_id FROM approval_settings WHERE workspace=?1")
            .map_err(storage_error)?;
        let mut sessions = statement
            .query_map([workspace], |r| r.get::<String>(0))
            .map_err(storage_error)?
            .collect::<crate::storage::Result<Vec<_>>>()
            .map_err(storage_error)?;
        drop(statement);
        if !sessions.iter().any(|s| s.is_empty()) {
            sessions.push(String::new());
        }
        for session_id in sessions {
            let scope = Scope {
                workspace: workspace.into(),
                session_id,
                task_id: String::new(),
                run_id: String::new(),
            };
            let current = read_settings(&tx, &scope)?;
            let mut next = current.unwrap_or_default();
            next.policy = forced.policy.unwrap_or(next.policy);
            next.reviewer = forced.reviewer.unwrap_or(next.reviewer);
            if current.is_some_and(|current| {
                current.policy != next.policy || current.reviewer != next.reviewer
            }) {
                next.revision = next.revision.checked_add(1).ok_or(Error::Conflict)?;
            }
            write_settings(&tx, &scope, next)?;
        }
        tx.commit().map_err(storage_error)
    }
    fn authorization_current(&self, request: &Request) -> Result<bool> {
        let mut connection = self.connection.lock().map_err(|_| Error::Unavailable)?;
        let tx = connection.transaction().map_err(storage_error)?;
        let body: String = tx
            .query_row(
                "SELECT body FROM approvals WHERE id=?1",
                [request.id.as_str()],
                |r| r.get(0),
            )
            .map_err(storage_error)?;
        if decode(&body)? != *request {
            return Err(Error::Conflict);
        }
        match check_automatic(&tx, request, Some("application")) {
            Ok(()) => Ok(true),
            Err(Error::Conflict) => Ok(false),
            Err(e) => Err(e),
        }
    }
    fn take_review_lease(&self, expected: &Request, lease: &ReviewLease) -> Result<bool> {
        let mut connection = self.connection.lock().map_err(|_| Error::Unavailable)?;
        let tx = connection.transaction().map_err(storage_error)?;
        let body: String = tx
            .query_row(
                "SELECT body FROM approvals WHERE id=?1",
                [expected.id.as_str()],
                |r| r.get(0),
            )
            .map_err(storage_error)?;
        if decode(&body)? != *expected || expected.state != State::Pending {
            return Ok(false);
        }
        if expected
            .expires_at
            .is_some_and(|time| time <= chrono::Utc::now().timestamp())
        {
            return Ok(false);
        }
        let old = tx
            .query_row(
                "SELECT owner,pid,started,expires_at FROM approval_review_leases WHERE \
            approval_id=?1",
                [expected.id.as_str()],
                |r| {
                    Ok(ReviewLease {
                        owner: r.get(0)?,
                        pid: r.get(1)?,
                        started: r.get(2)?,
                        expires_at: r.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(storage_error)?;
        if old.as_ref().is_some_and(|old| {
            old.expires_at > chrono::Utc::now().timestamp()
                && otto_platform::process_identity::process_matches(old.pid, &old.started)
                    != Some(false)
        }) {
            return Ok(false);
        }
        tx.execute("INSERT INTO approval_review_leases VALUES (?1,?2,?3,?4,?5) ON \
            CONFLICT(approval_id) DO UPDATE SET \
            owner=excluded.owner,pid=excluded.pid,started=excluded.started,expires_at=excluded.expires_at",
            params![expected.id, lease.owner, lease.pid, lease.started, lease.expires_at],
        ).map_err(storage_error)?;
        tx.commit().map_err(storage_error)?;
        Ok(true)
    }
    fn release_review_lease(&self, id: &str, owner: &str) -> Result<()> {
        self.connection
            .lock()
            .map_err(|_| Error::Unavailable)?
            .execute(
                "DELETE FROM approval_review_leases WHERE approval_id=?1 AND owner=?2",
                params![id, owner],
            )
            .map(|_| ())
            .map_err(storage_error)
    }
    fn commit_review(&self, expected: &Request, next: &Request, owner: &str) -> Result<Request> {
        self.commit_checked(expected, next, ClaimChange::None, true, Some(owner))
    }
    fn create(&self, key: &str, request: &Request) -> Result<(Request, bool)> {
        let mut connection = self.connection.lock().map_err(|_| Error::Unavailable)?;
        let tx = connection.transaction().map_err(storage_error)?;
        let existing = tx
            .query_row(
                "SELECT body FROM approvals WHERE subject_key=?1",
                [key],
                |row| row.get::<String>(0),
            )
            .optional()
            .map_err(storage_error)?;
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
            .map_err(storage_error)?;
            tx.commit().map_err(storage_error)?;
            return Ok((existing, false));
        }
        tx.execute(
            "INSERT INTO approvals VALUES (?1,?2,?3,?4,'pending',?5)",
            params![
                request.id,
                key,
                request.scope.workspace,
                request.scope.session_id,
                encode(request)?
            ],
        )
        .map_err(storage_error)?;
        tx.execute(
            "INSERT INTO approval_visibility VALUES (?1,?2,?3)",
            params![
                request.id,
                request.scope.workspace,
                request.scope.session_id
            ],
        )
        .map_err(storage_error)?;
        append_event(&tx, request)?;
        tx.commit().map_err(storage_error)?;
        Ok((request.clone(), true))
    }
    fn load(&self, id: &str) -> Result<Request> {
        let connection = self.connection.lock().map_err(|_| Error::Unavailable)?;
        let body = connection
            .query_row("SELECT body FROM approvals WHERE id=?1", [id], |row| {
                row.get::<String>(0)
            })
            .optional()
            .map_err(storage_error)?
            .ok_or(Error::NotFound)?;
        decode(&body)
    }
    fn visible(&self, id: &str, workspace: &str, session: Option<&str>) -> Result<bool> {
        let connection = self.connection.lock().map_err(|_| Error::Unavailable)?;
        connection
            .query_row(
                "SELECT 1 FROM approval_visibility WHERE approval_id=?1 AND workspace=?2 \
            AND (?3='' OR session_id=?3 OR session_id='') LIMIT 1",
                params![id, workspace, session.unwrap_or("")],
                |row| row.get::<i64>(0),
            )
            .optional()
            .map(|value| value.is_some())
            .map_err(|_| Error::Unavailable)
    }
    fn pending(&self) -> Result<Vec<Request>> {
        let connection = self.connection.lock().map_err(|_| Error::Unavailable)?;
        let mut statement = connection
            .prepare("SELECT body FROM approvals WHERE state='pending'")
            .map_err(storage_error)?;
        statement
            .query_map((), |row| row.get::<String>(0))
            .map_err(storage_error)?
            .map(|body| decode(&body.map_err(storage_error)?))
            .collect()
    }
    fn applications(&self, workspace: &str) -> Result<Vec<Request>> {
        let connection = self.connection.lock().map_err(|_| Error::Unavailable)?;
        let mut statement = connection
            .prepare(
                "SELECT DISTINCT a.body FROM approvals a JOIN approval_visibility v ON \
            v.approval_id=a.id WHERE v.workspace=?1 AND a.state IN \
            ('approved','rejected','withdrawn','expired')",
            )
            .map_err(|_| Error::Unavailable)?;
        statement
            .query_map([workspace], |row| row.get::<String>(0))
            .map_err(storage_error)?
            .map(|body| decode(&body.map_err(storage_error)?))
            .collect::<Result<Vec<_>>>()
    }
    fn snapshot(&self, workspace: &str, session: Option<&str>) -> Result<Snapshot> {
        let mut connection = self.connection.lock().map_err(|_| Error::Unavailable)?;
        let tx = connection.transaction().map_err(storage_error)?;
        let cursor = tx
            .query_row(
                "SELECT COALESCE(MAX(sequence),0) FROM approval_events",
                (),
                |row| row.get(0),
            )
            .map_err(storage_error)?;
        let mut statement = tx
            .prepare(
                "SELECT DISTINCT a.body FROM approvals a JOIN approval_visibility v ON \
            v.approval_id=a.id WHERE v.workspace=?1 AND (?2='' OR v.session_id=?2 \
            OR v.session_id='') AND a.state='pending' ORDER BY a.rowid",
            )
            .map_err(storage_error)?;
        let bodies = statement
            .query_map(params![workspace, session.unwrap_or("")], |row| {
                row.get::<String>(0)
            })
            .map_err(storage_error)?;
        let requests = bodies
            .map(|row| decode(&row.map_err(storage_error)?))
            .collect::<Result<Vec<_>>>()?;
        drop(statement);
        tx.commit().map_err(storage_error)?;
        Ok(Snapshot { requests, cursor })
    }
    fn events(&self, workspace: &str, session: Option<&str>, after: i64) -> Result<Vec<Change>> {
        if after < 0 {
            return Err(Error::Invalid("negative event cursor"));
        }
        let connection = self.connection.lock().map_err(|_| Error::Unavailable)?;
        let mut statement = connection
            .prepare(
                "SELECT DISTINCT e.sequence,e.body FROM approval_events e JOIN \
            approval_visibility v ON v.approval_id=e.approval_id WHERE \
            v.workspace=?1 AND (?2='' OR v.session_id=?2 OR v.session_id='') AND \
            e.sequence>?3 ORDER BY e.sequence LIMIT 100",
            )
            .map_err(storage_error)?;
        let rows = statement
            .query_map(params![workspace, session.unwrap_or(""), after], |row| {
                Ok((row.get::<i64>(0)?, row.get::<String>(1)?))
            })
            .map_err(storage_error)?;
        rows.map(|row| {
            let (sequence, body) = row.map_err(storage_error)?;
            Ok(Change {
                sequence,
                request: decode(&body)?,
            })
        })
        .collect()
    }
    fn claim(&self, id: &str) -> Result<Option<Claim>> {
        self.connection
            .lock()
            .map_err(|_| Error::Unavailable)?
            .query_row(
                "SELECT owner,pid,started,recovery FROM approval_claims WHERE approval_id=?1",
                [id],
                |row| {
                    Ok(Claim {
                        owner: row.get(0)?,
                        pid: row.get(1)?,
                        started: row.get(2)?,
                        native: row.get::<String>(3)? != "domain",
                    })
                },
            )
            .optional()
            .map_err(storage_error)
    }
    fn take_claim(&self, id: &str, expected: &Claim, next: &Claim) -> Result<bool> {
        self.connection.lock().map_err(|_| Error::Unavailable)?.execute(
            "UPDATE approval_claims SET owner=?2,pid=?3,started=?4 WHERE approval_id=?1 AND owner=?5 AND pid=?6 AND started=?7",
            params![id,next.owner,next.pid,next.started,expected.owner,expected.pid,expected.started],
        ).map(|count| count == 1).map_err(storage_error)
    }
    fn commit(
        &self,
        expected: &Request,
        next: &Request,
        claim: ClaimChange,
        automatic: bool,
    ) -> Result<Request> {
        self.commit_checked(expected, next, claim, automatic, None)
    }
}
fn encode(request: &Request) -> Result<String> {
    serde_json::to_string(request).map_err(|_| Error::Unavailable)
}
fn is_reauthorization(expected: &Request, next: &Request) -> bool {
    expected.state == State::Approved
        && expected.application == ApplicationState::NotStarted
        && expected.kind == Kind::Authorization
        && expected.decision.as_ref().is_some_and(|d| {
            matches!(
                d.transport.as_str(),
                "auto-review" | "approval-rule" | "yolo-mode" | "bounded-authority"
            )
        })
        && next.revision == expected.revision.checked_add(1).unwrap_or(0)
        && next.application == ApplicationState::NotStarted
        && matches!(next.state, State::Pending | State::Rejected)
        && next.review.as_ref().is_some_and(|r| {
            matches!(
                r.status,
                ReviewStatus::HumanRequired | ReviewStatus::Completed
            )
        })
}
fn read_settings(connection: &Connection, scope: &Scope) -> Result<Option<ApprovalSettings>> {
    let row: Option<(String, String, u64)> = connection
        .query_row(
            "SELECT policy,reviewer,revision FROM approval_settings WHERE \
        workspace=?1 AND (session_id=?2 OR session_id='') ORDER BY CASE WHEN \
        session_id=?2 THEN 0 ELSE 1 END LIMIT 1",
            params![scope.workspace, scope.session_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .map_err(storage_error)?;
    row.map(|(policy, reviewer, revision)| {
        let policy = serde_json::from_value(serde_json::Value::String(policy))
            .map_err(|_| Error::Invalid("unknown approval policy"))?;
        let reviewer = serde_json::from_value(serde_json::Value::String(reviewer))
            .map_err(|_| Error::Invalid("unknown approval reviewer"))?;
        let settings = ApprovalSettings {
            policy,
            reviewer,
            revision,
        };
        settings.validate().map_err(Error::Invalid)?;
        Ok(settings)
    })
    .transpose()
}
fn write_settings(
    connection: &Connection,
    scope: &Scope,
    settings: ApprovalSettings,
) -> Result<()> {
    if read_managed(connection, &scope.workspace)?.is_some_and(|forced| {
        forced.policy.is_some_and(|p| p != settings.policy)
            || forced.reviewer.is_some_and(|r| r != settings.reviewer)
    }) {
        return Err(Error::Invalid(
            "approval settings conflict with managed policy",
        ));
    }
    let policy = serde_json::to_value(settings.policy).map_err(|_| Error::Unavailable)?;
    let reviewer = serde_json::to_value(settings.reviewer).map_err(|_| Error::Unavailable)?;
    connection
        .execute(
            "INSERT INTO approval_settings VALUES (?1,?2,?3,?4,?5) ON \
        CONFLICT(workspace,session_id) DO UPDATE SET \
        policy=excluded.policy,reviewer=excluded.reviewer,revision=excluded.revision",
            params![
                scope.workspace,
                scope.session_id,
                policy.as_str().ok_or(Error::Unavailable)?,
                reviewer.as_str().ok_or(Error::Unavailable)?,
                settings.revision
            ],
        )
        .map_err(storage_error)?;
    connection
        .execute(
            "INSERT INTO approval_modes VALUES (?1,?2,?3) ON \
        CONFLICT(workspace,session_id) DO UPDATE SET mode=excluded.mode",
            params![scope.workspace, scope.session_id, settings.mode().as_str()],
        )
        .map(|_| ())
        .map_err(storage_error)
}
/// The stored managed approval policy: the two fields that bind approval
/// settings. It is a record of its own rather than `ApprovalConfig`, so a
/// change to the configuration schema does not make stored rows unreadable.
/// Keys it does not know are ignored; the row is rewritten from the managed
/// configuration each time a runner starts.
#[derive(Serialize, Deserialize)]
struct ManagedRow {
    policy: Option<ApprovalPolicy>,
    reviewer: Option<ApprovalReviewer>,
}

fn read_managed(
    connection: &Connection,
    workspace: &str,
) -> Result<Option<otto_core::config::ApprovalConfig>> {
    let body: Option<String> = connection
        .query_row(
            "SELECT body FROM approval_managed_policy WHERE workspace=?1",
            [workspace],
            |r| r.get(0),
        )
        .optional()
        .map_err(storage_error)?;
    body.map(|body| {
        let row: ManagedRow = serde_json::from_str(&body)
            .map_err(|_| Error::Invalid("invalid managed approval policy"))?;
        Ok(otto_core::config::ApprovalConfig {
            policy: row.policy,
            reviewer: row.reviewer,
            ..Default::default()
        })
    })
    .transpose()
}
fn decode(body: &str) -> Result<Request> {
    decode_persisted(body)
}
fn append_event(tx: &Transaction<'_>, request: &Request) -> Result<()> {
    append_event_body(tx, request, &encode(request)?)
}
fn append_event_body(tx: &Transaction<'_>, request: &Request, body: &str) -> Result<()> {
    tx.execute(
        "INSERT INTO approval_events (approval_id,workspace,session_id,body) VALUES (?1,?2,?3,?4)",
        params![
            request.id,
            request.scope.workspace,
            request.scope.session_id,
            body
        ],
    )
    .map(|_| ())
    .map_err(storage_error)
}

fn storage_error(error: crate::storage::Error) -> Error {
    match error {
        crate::storage::Error::CommitUnknown => Error::CommitUnknown,
        _ => Error::Unavailable,
    }
}

pub(crate) use crate::approval::codec::decode_persisted;
use crate::approval::codec::merge_persisted;

#[cfg(test)]
mod tests;
