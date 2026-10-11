use crate::approval::repository::*;
use crate::approval::{Error, Result};
use crate::storage::drivers::turso::TursoDriver;
use crate::storage::{Connection, OptionalExtension, Transaction, params};
use crate::storage::{OpenOptions, SqlDriver, SqlRequirements};
use otto_core::approval::*;
use std::{path::Path, sync::Mutex, time::Duration};

pub struct Store {
    connection: Mutex<Connection>,
}
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS approval_modes (
 workspace TEXT NOT NULL, session_id TEXT NOT NULL,
 mode TEXT NOT NULL CHECK(mode IN ('manual','auto','yolo')),
 PRIMARY KEY(workspace,session_id)
) STRICT;
CREATE TABLE IF NOT EXISTS approval_settings (
 workspace TEXT NOT NULL, session_id TEXT NOT NULL, policy TEXT NOT NULL,
 reviewer TEXT NOT NULL, revision INTEGER NOT NULL CHECK(revision>0),
 PRIMARY KEY(workspace,session_id)
) STRICT;
CREATE TABLE IF NOT EXISTS approval_review_contexts (
 workspace TEXT NOT NULL, session_id TEXT NOT NULL,
 evidence_digest TEXT NOT NULL, policy_digest TEXT NOT NULL,
 PRIMARY KEY(workspace,session_id)
) STRICT;
CREATE TABLE IF NOT EXISTS approval_managed_policy (
 workspace TEXT PRIMARY KEY, body TEXT NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS approval_review_leases (
 approval_id TEXT PRIMARY KEY REFERENCES approvals(id), owner TEXT NOT NULL,
 pid INTEGER NOT NULL, started TEXT NOT NULL, expires_at INTEGER NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS approval_review_denials (
 workspace TEXT NOT NULL, session_id TEXT NOT NULL, task_id TEXT NOT NULL,
 run_id TEXT NOT NULL, evidence_digest TEXT NOT NULL, denials INTEGER NOT NULL,
 PRIMARY KEY(workspace,session_id,task_id,run_id,evidence_digest)
) STRICT;
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

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with_driver(&TursoDriver, path)
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
        let mut connection = Connection::open_with_driver(
            driver,
            path,
            OpenOptions::default(),
            SqlRequirements::DURABLE,
        )
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

pub(super) fn initialize(connection: &mut Connection) -> Result<()> {
    connection
        .busy_timeout(Duration::from_secs(5))
        .map_err(storage_error)?;
    let tx = connection.transaction().map_err(storage_error)?;
    initialize_tx(&tx)?;
    tx.commit().map_err(storage_error)
}
fn initialize_tx(tx: &Transaction<'_>) -> Result<()> {
    let version: i64 = tx
        .query_row("PRAGMA user_version", (), |row| row.get(0))
        .map_err(storage_error)?;
    if !(0..=4).contains(&version) {
        return Err(Error::Invalid("unsupported approval database version"));
    }
    if version == 1 {
        // Older claims lack application intent. Preserve them without
        // inventing permission to replay an interrupted native review.
        tx.execute_batch(
            "ALTER TABLE approval_claims ADD COLUMN recovery TEXT NOT NULL DEFAULT 'native'",
        )
        .map_err(storage_error)?;
    }
    if has_legacy_mode(tx)? {
        // The mode used to be an `automatic` boolean; rewrite every row with
        // the meaning it was written with. A row cannot be lost here: the
        // copy, the drop and the new table share one transaction.
        for statement in [
            "ALTER TABLE approval_modes RENAME TO approval_modes_legacy",
            "CREATE TABLE approval_modes (workspace TEXT NOT NULL, session_id TEXT NOT NULL, \
             mode TEXT NOT NULL CHECK(mode IN ('manual','auto','yolo')), \
             PRIMARY KEY(workspace,session_id)) STRICT",
            "INSERT INTO approval_modes (workspace,session_id,mode) SELECT workspace,session_id, \
             CASE automatic WHEN 0 THEN 'manual' ELSE 'auto' END FROM approval_modes_legacy",
            "DROP TABLE approval_modes_legacy",
        ] {
            tx.execute(statement, ()).map_err(storage_error)?;
        }
    }
    tx.execute_batch(SCHEMA).map_err(storage_error)?;
    if version < 4 {
        tx.execute_batch("INSERT OR IGNORE INTO approval_settings SELECT workspace,session_id,'on_request',CASE mode WHEN 'manual' THEN 'user' WHEN 'yolo' THEN 'bypass' ELSE 'auto_review' END,1 FROM approval_modes").map_err(storage_error)?;
    }
    tx.execute_batch("PRAGMA user_version=4")
        .map_err(storage_error)?;
    Ok(())
}

/// Whether `approval_modes` still carries the pre-mode `automatic` column.
fn has_legacy_mode(tx: &Transaction<'_>) -> Result<bool> {
    let mut statement = tx
        .prepare("PRAGMA table_info(approval_modes)")
        .map_err(storage_error)?;
    let fields = statement
        .query_map((), |row| row.get::<String>(1))
        .map_err(storage_error)?;
    for name in fields {
        if name.map_err(storage_error)? == "automatic" {
            return Ok(true);
        }
    }
    Ok(false)
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
        self.connection.lock().map_err(|_|Error::Unavailable)?.query_row("SELECT evidence_digest,policy_digest FROM approval_review_contexts WHERE workspace=?1 AND session_id=?2",params![scope.workspace,scope.session_id],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(storage_error)
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
        let old_digest: Option<String> = tx.query_row("SELECT policy_digest FROM approval_review_contexts WHERE workspace=?1 AND session_id=''",[workspace],|r|r.get(0)).optional().map_err(storage_error)?;
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
        tx.execute("INSERT INTO approval_review_contexts VALUES (?1,'','policy',?2) ON CONFLICT(workspace,session_id) DO UPDATE SET policy_digest=excluded.policy_digest",params![workspace,digest]).map_err(storage_error)?;
        if old_digest.as_deref() != Some(digest) {
            // Invalidate every old root snapshot in the same transaction as
            // the policy epoch. Live contexts are republished by the service.
            tx.execute("UPDATE approval_review_contexts SET policy_digest=?2 WHERE workspace=?1 AND session_id<>''",params![workspace,format!("policy-changed:{digest}")]).map_err(storage_error)?;
        }
        tx.commit().map_err(storage_error)
    }
    fn review_denials(&self, scope: &Scope, evidence: &str) -> Result<u64> {
        self.connection.lock().map_err(|_|Error::Unavailable)?.query_row("SELECT denials FROM approval_review_denials WHERE workspace=?1 AND session_id=?2 AND task_id=?3 AND run_id=?4 AND evidence_digest=?5",params![scope.workspace,scope.session_id,scope.task_id,scope.run_id,evidence],|r|r.get(0)).optional().map(|v|v.unwrap_or(0)).map_err(storage_error)
    }
    fn managed_policy(
        &self,
        workspace: &str,
        forced: &otto_core::config::ApprovalConfig,
    ) -> Result<()> {
        let mut connection = self.connection.lock().map_err(|_| Error::Unavailable)?;
        let tx = connection.transaction().map_err(storage_error)?;
        let forced = otto_core::config::ApprovalConfig {
            policy: forced.policy,
            reviewer: forced.reviewer,
            ..Default::default()
        };
        tx.execute("INSERT INTO approval_managed_policy VALUES (?1,?2) ON CONFLICT(workspace) DO UPDATE SET body=excluded.body",params![workspace,serde_json::to_string(&forced).map_err(|_|Error::Unavailable)?]).map_err(storage_error)?;
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
        let old = tx.query_row("SELECT owner,pid,started,expires_at FROM approval_review_leases WHERE approval_id=?1", [expected.id.as_str()], |r|Ok(ReviewLease {owner:r.get(0)?,pid:r.get(1)?,started:r.get(2)?,expires_at:r.get(3)?})).optional().map_err(storage_error)?;
        if old.as_ref().is_some_and(|old| {
            old.expires_at > chrono::Utc::now().timestamp()
                && crate::subagent::record::process_matches(old.pid, &old.started) != Some(false)
        }) {
            return Ok(false);
        }
        tx.execute("INSERT INTO approval_review_leases VALUES (?1,?2,?3,?4,?5) ON CONFLICT(approval_id) DO UPDATE SET owner=excluded.owner,pid=excluded.pid,started=excluded.started,expires_at=excluded.expires_at", params![expected.id,lease.owner,lease.pid,lease.started,lease.expires_at]).map_err(storage_error)?;
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
        connection.query_row("SELECT 1 FROM approval_visibility WHERE approval_id=?1 AND workspace=?2 AND (?3='' OR session_id=?3 OR session_id='') LIMIT 1",params![id,workspace,session.unwrap_or("")],|row|row.get::<i64>(0)).optional().map(|value| value.is_some()).map_err(|_|Error::Unavailable)
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
        let mut statement=connection.prepare("SELECT DISTINCT a.body FROM approvals a JOIN approval_visibility v ON v.approval_id=a.id WHERE v.workspace=?1 AND a.state IN ('approved','rejected','withdrawn','expired')").map_err(|_|Error::Unavailable)?;
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
        let mut statement = tx.prepare("SELECT DISTINCT a.body FROM approvals a JOIN approval_visibility v ON v.approval_id=a.id WHERE v.workspace=?1 AND (?2='' OR v.session_id=?2 OR v.session_id='') AND a.state='pending' ORDER BY a.rowid")
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
        let mut statement = connection.prepare("SELECT DISTINCT e.sequence,e.body FROM approval_events e JOIN approval_visibility v ON v.approval_id=e.approval_id WHERE v.workspace=?1 AND (?2='' OR v.session_id=?2 OR v.session_id='') AND e.sequence>?3 ORDER BY e.sequence LIMIT 100")
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
impl Store {
    fn commit_checked(
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
    let row: Option<(String,String,u64)> = connection.query_row("SELECT policy,reviewer,revision FROM approval_settings WHERE workspace=?1 AND (session_id=?2 OR session_id='') ORDER BY CASE WHEN session_id=?2 THEN 0 ELSE 1 END LIMIT 1", params![scope.workspace,scope.session_id], |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional().map_err(storage_error)?;
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
    connection.execute("INSERT INTO approval_settings VALUES (?1,?2,?3,?4,?5) ON CONFLICT(workspace,session_id) DO UPDATE SET policy=excluded.policy,reviewer=excluded.reviewer,revision=excluded.revision",params![scope.workspace,scope.session_id,policy.as_str().ok_or(Error::Unavailable)?,reviewer.as_str().ok_or(Error::Unavailable)?,settings.revision]).map_err(storage_error)?;
    connection.execute("INSERT INTO approval_modes VALUES (?1,?2,?3) ON CONFLICT(workspace,session_id) DO UPDATE SET mode=excluded.mode", params![scope.workspace,scope.session_id,settings.mode().as_str()]).map(|_|()).map_err(storage_error)
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
        serde_json::from_str(&body).map_err(|_| Error::Invalid("invalid managed approval policy"))
    })
    .transpose()
}
fn check_automatic(tx: &Transaction<'_>, request: &Request, owner: Option<&str>) -> Result<()> {
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
        let actual:Option<String>=tx.query_row("SELECT policy_digest FROM approval_review_contexts WHERE workspace=?1 AND session_id=''",[request.scope.workspace.as_str()],|r|r.get(0)).optional().map_err(storage_error)?;
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
        let digest:Option<String>=tx.query_row("SELECT policy_digest FROM approval_review_contexts WHERE workspace=?1 AND session_id=''",[request.scope.workspace.as_str()],|r|r.get(0)).optional().map_err(storage_error)?;
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
    let context: Option<(String,String)> = tx.query_row("SELECT evidence_digest,policy_digest FROM approval_review_contexts WHERE workspace=?1 AND session_id=?2",params![request.scope.workspace,request.scope.session_id],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(storage_error)?;
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
fn check_human_basis(tx: &Transaction<'_>, request: &Request, allow: bool) -> Result<()> {
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
    let context:Option<(String,String)>=tx.query_row("SELECT evidence_digest,policy_digest FROM approval_review_contexts WHERE workspace=?1 AND session_id=?2",params![request.scope.workspace,request.scope.session_id],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(storage_error)?;
    if let Some((evidence, policy)) = context {
        return if metadata.matches(settings.revision, &evidence, &policy) {
            Ok(())
        } else {
            Err(Error::Conflict)
        };
    }
    let policy:Option<String>=tx.query_row("SELECT policy_digest FROM approval_review_contexts WHERE workspace=?1 AND session_id=''",[request.scope.workspace.as_str()],|r|r.get(0)).optional().map_err(storage_error)?;
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

/// Copy a consistent pre-migration snapshot, including opaque envelopes and
/// the original schema. A transaction holds the source writer fence until the
/// destination is durable; a raw file copy would omit committed WAL pages.
fn backup_before_v4(
    driver: &dyn SqlDriver,
    path: &Path,
    tx: &Transaction<'_>,
    version: i64,
) -> Result<()> {
    let mut schemas = tx.prepare("SELECT name,sql FROM sqlite_master WHERE type='table' AND name LIKE 'approval%' ORDER BY name").map_err(storage_error)?;
    let tables = schemas
        .query_map((), |r| Ok((r.get::<String>(0)?, r.get::<String>(1)?)))
        .map_err(storage_error)?
        .collect::<crate::storage::Result<Vec<_>>>()
        .map_err(storage_error)?;
    drop(schemas);
    let filename = format!(
        "{}.pre-v4-{}.db",
        path.file_name()
            .and_then(|s| s.to_str())
            .ok_or(Error::Unavailable)?,
        chrono::Utc::now().format("%Y%m%dT%H%M%S%.9f")
    );
    let backup_path = path.with_file_name(filename);
    let partial_path = backup_path.with_extension("db.partial");
    {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&partial_path)
            .map_err(|_| Error::Unavailable)?;
    }
    let mut backup = Connection::open_with_driver(
        driver,
        &partial_path,
        OpenOptions::default(),
        SqlRequirements::DURABLE,
    )
    .map_err(storage_error)?;
    let destination = backup.transaction().map_err(storage_error)?;
    for (name, schema) in &tables {
        if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(Error::Invalid("invalid approval backup table"));
        }
        destination.execute_batch(schema).map_err(storage_error)?;
    }
    let mut tables = tables;
    tables.sort_by_key(|(name, _)| usize::from(name != "approvals"));
    for (name, _) in tables {
        let mut statement = tx
            .prepare(&format!("SELECT * FROM {name}"))
            .map_err(storage_error)?;
        let mut rows = statement.query(()).map_err(storage_error)?;
        while let Some(row) = rows.next().map_err(storage_error)? {
            let placeholders = (1..=row.column_count())
                .map(|index| format!("?{index}"))
                .collect::<Vec<_>>()
                .join(",");
            destination
                .execute(
                    &format!("INSERT INTO {name} VALUES ({placeholders})"),
                    crate::storage::Params(row.0.clone()),
                )
                .map_err(storage_error)?;
        }
    }
    destination
        .execute_batch(&format!("PRAGMA user_version={version}"))
        .map_err(storage_error)?;
    destination.commit().map_err(storage_error)?;
    backup.checkpoint().map_err(storage_error)?;
    backup.close().map_err(storage_error)?;
    // Publish only a complete backup, atomically refusing an existing target.
    // Failures retain a clearly named partial artifact and never alter source.
    std::fs::hard_link(&partial_path, &backup_path).map_err(|_| Error::Unavailable)?;
    std::fs::remove_file(&partial_path).map_err(|_| Error::Unavailable)?;
    if let Some(parent) = backup_path.parent() {
        std::fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|_| Error::Unavailable)?;
    }
    Ok(())
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
mod tests {
    use super::*;
    use crate::approval::{Actor, Proposal, Service};
    use otto_core::approval::ApprovalMode;
    use std::sync::Arc;

    fn fixture_proposal() -> Proposal {
        Proposal {
            kind: Kind::Authorization,
            authority: Authority::Boundary,
            scope: Scope {
                workspace: "/backup".into(),
                session_id: "root".into(),
                task_id: String::new(),
                run_id: String::new(),
            },
            subject: Subject {
                source: "sandbox".into(),
                id: "operation".into(),
                version: "1".into(),
                tool_call_id: "call".into(),
                details: "exact command".into(),
            },
            title: "Review fixture".into(),
            reason: "scope".into(),
            persistent: false,
            blocking: true,
            expires_at: None,
        }
    }
    #[test]
    fn backup_and_migration_share_the_same_source_writer_fence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("approvals.db");
        let service = Service::open(&path).unwrap();
        let scope = fixture_proposal().scope;
        service.set_mode(&scope, ApprovalMode::Manual).unwrap();
        let request = service.create(fixture_proposal()).unwrap();
        drop(service);
        let mut source = Connection::open(&path).unwrap();
        source.execute_batch("PRAGMA user_version=3").unwrap();
        let writer = Connection::open(&path).unwrap();
        writer.busy_timeout(Duration::ZERO).unwrap();
        let tx = source.transaction().unwrap();
        backup_before_v4(&TursoDriver, &path, &tx, 3).unwrap();
        assert!(matches!(
            writer.execute(
                "INSERT INTO approval_modes VALUES ('/backup','late','manual')",
                ()
            ),
            Err(crate::storage::Error::Busy(_))
        ));
        initialize_tx(&tx).unwrap();
        assert!(matches!(
            writer.execute(
                "INSERT INTO approval_modes VALUES ('/backup','late','manual')",
                ()
            ),
            Err(crate::storage::Error::Busy(_))
        ));
        tx.commit().unwrap();
        let backup = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("approvals.db.pre-v4-")
                    && p.extension().is_some_and(|e| e == "db")
            })
            .unwrap();
        let snapshot = Connection::open_read_only(&backup).unwrap();
        assert_eq!(
            snapshot
                .query_row("PRAGMA user_version", (), |r| r.get::<i64>(0))
                .unwrap(),
            3
        );
        assert_eq!(
            snapshot
                .query_row("SELECT id FROM approvals", (), |r| r.get::<String>(0))
                .unwrap(),
            request.id
        );
        assert_eq!(
            source
                .query_row("PRAGMA user_version", (), |r| r.get::<i64>(0))
                .unwrap(),
            4
        );
    }
    #[test]
    fn expired_review_tokens_cannot_commit_or_release_a_replacement_lease() {
        let store = Arc::new(Store::in_memory().unwrap());
        let service = Service::from_store(store.clone()).unwrap();
        let scope = fixture_proposal().scope;
        service.set_mode(&scope, ApprovalMode::Manual).unwrap();
        let expected = service.create(fixture_proposal()).unwrap();
        let (pid, started) = crate::subagent::record::current_process();
        let old = ReviewLease {
            owner: "process:old-review".into(),
            pid,
            started: started.clone(),
            expires_at: chrono::Utc::now().timestamp() - 1,
        };
        let replacement = ReviewLease {
            owner: "process:new-review".into(),
            pid,
            started,
            expires_at: chrono::Utc::now().timestamp() + 30,
        };
        assert!(store.take_review_lease(&expected, &old).unwrap());
        assert!(store.take_review_lease(&expected, &replacement).unwrap());
        store
            .release_review_lease(&expected.id, &old.owner)
            .unwrap();
        let actual: String = store
            .connection
            .lock()
            .unwrap()
            .query_row(
                "SELECT owner FROM approval_review_leases WHERE approval_id=?1",
                [expected.id.as_str()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(actual, replacement.owner);
        service.set_mode(&scope, ApprovalMode::Yolo).unwrap();
        let current = store.load(&expected.id).unwrap();
        // Even an otherwise unchanged reviewer result cannot use an old token.
        assert!(matches!(
            store.commit_review(&current, &current, &old.owner),
            Err(Error::Conflict)
        ));
        store
            .release_review_lease(&expected.id, &replacement.owner)
            .unwrap();
    }
    #[test]
    fn v3_backup_contains_complete_opaque_rows_and_application_claims() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("approvals.db");
        let service = Service::open(&path).unwrap();
        let scope = fixture_proposal().scope;
        service.set_mode(&scope, ApprovalMode::Manual).unwrap();
        let request = service.create(fixture_proposal()).unwrap();
        service
            .decide_and_claim(
                &request.id,
                &scope,
                request.revision,
                true,
                &Actor::local("fixture"),
            )
            .unwrap();
        drop(service);
        let connection = Connection::open(&path).unwrap();
        let body: String = connection
            .query_row(
                "SELECT body FROM approvals WHERE id=?1",
                [request.id.as_str()],
                |r| r.get(0),
            )
            .unwrap();
        let opaque = body.strip_suffix('}').unwrap().to_owned()
            + ",\"future_review_extension\":{\"n\":9007199254740993}}";
        connection
            .execute(
                "UPDATE approvals SET body=?2 WHERE id=?1",
                params![request.id, opaque],
            )
            .unwrap();
        connection.execute_batch("PRAGMA user_version=3").unwrap();
        connection.close().unwrap();
        let upgraded = Service::open(&path).unwrap();
        assert_eq!(
            upgraded
                .get(&request.id, &scope.workspace, Some(&scope.session_id))
                .unwrap()
                .application,
            ApplicationState::Applying
        );
        let backup = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("approvals.db.pre-v4-")
                    && p.extension().is_some_and(|v| v == "db")
            })
            .unwrap();
        assert_eq!(
            std::fs::metadata(&backup).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let snapshot = Connection::open_read_only(&backup).unwrap();
        assert_eq!(
            snapshot
                .query_row("PRAGMA user_version", (), |r| r.get::<i64>(0))
                .unwrap(),
            3
        );
        assert_eq!(
            snapshot
                .query_row(
                    "SELECT body FROM approvals WHERE id=?1",
                    [request.id.as_str()],
                    |r| r.get::<String>(0)
                )
                .unwrap(),
            opaque
        );
        assert_eq!(
            snapshot
                .query_row("SELECT COUNT(*) FROM approval_claims", (), |r| r
                    .get::<i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            snapshot
                .query_row("SELECT COUNT(*) FROM approval_events", (), |r| r
                    .get::<i64>(0))
                .unwrap(),
            2
        );
        assert_eq!(
            snapshot
                .query_row("SELECT COUNT(*) FROM approval_visibility", (), |r| r
                    .get::<i64>(0))
                .unwrap(),
            1
        );
    }

    #[test]
    fn stale_snapshot_or_auto_mode_never_publishes_a_partial_decision() {
        let store = Arc::new(Store::in_memory().unwrap());
        let service = Service::from_store(store.clone()).unwrap();
        let scope = Scope {
            workspace: "/w".into(),
            session_id: "s".into(),
            task_id: String::new(),
            run_id: String::new(),
        };
        service.set_mode(&scope, ApprovalMode::Manual).unwrap();
        let pending = service
            .create(Proposal {
                kind: Kind::Authorization,
                authority: Authority::Bounded,
                scope: scope.clone(),
                subject: Subject {
                    tool_call_id: String::new(),
                    source: "plugin".into(),
                    id: "plugin".into(),
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
        let mut next = pending.clone();
        next.state = State::Approved;
        assert!(matches!(
            store.commit(&pending, &next, ClaimChange::None, true),
            Err(Error::Conflict)
        ));
        assert_eq!(store.load(&pending.id).unwrap(), pending);
        assert_eq!(store.events("/w", Some("s"), 0).unwrap().len(), 1);
        service
            .decide(&pending.id, &scope, 1, false, "", &Actor::local("test"))
            .unwrap();
        assert!(matches!(
            store.commit(&pending, &next, ClaimChange::None, false),
            Err(Error::Conflict)
        ));
        assert_eq!(store.load(&pending.id).unwrap().state, State::Rejected);
        assert_eq!(store.events("/w", Some("s"), 0).unwrap().len(), 2);
    }

    #[test]
    fn the_commit_fence_rechecks_the_persisted_mode_for_yolo_and_manual() {
        let store = Arc::new(Store::in_memory().unwrap());
        let service = Service::from_store(store.clone()).unwrap();
        let scope = Scope {
            workspace: "/w".into(),
            session_id: "s".into(),
            task_id: String::new(),
            run_id: String::new(),
        };
        // A consent request: the class AUTO may never decide.
        let review = Proposal {
            kind: Kind::Confirmation,
            authority: Authority::Consent,
            scope: scope.clone(),
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
        };
        service.set_mode(&scope, ApprovalMode::Manual).unwrap();
        let pending = service.create(review).unwrap();
        assert_eq!(pending.state, State::Pending);
        let mut next = pending.clone();
        next.state = State::Approved;
        // MANUAL refuses it even though the service could compute the
        // decision, so the fence reads the persisted mode, not the caller.
        assert!(matches!(
            store.commit(&pending, &next, ClaimChange::None, true),
            Err(Error::Conflict)
        ));
        assert_eq!(store.load(&pending.id).unwrap(), pending);
        // The same commit lands once the store itself is in YOLO. The store
        // port is used so the pending request is not swept by `set_mode`:
        // the fence, not the sweep, is what publishes this decision.
        store.set_mode(&scope, ApprovalMode::Yolo).unwrap();
        next.review.as_mut().unwrap().settings_revision =
            store.settings(&scope).unwrap().unwrap().revision;
        next.review.as_mut().unwrap().status = ReviewStatus::Completed;
        next.review.as_mut().unwrap().policy_digest = crate::approval::digest(
            &serde_json::to_string(&otto_core::config::ApprovalConfig::default()).unwrap(),
        );
        next.decision = Some(Decision {
            option_id: "approve".into(),
            feedback: "Explicit bypass preset.".into(),
            actor: "approval-policy".into(),
            transport: "yolo-mode".into(),
            decided_at: chrono::Utc::now().timestamp(),
        });
        let approved = store
            .commit(&pending, &next, ClaimChange::None, true)
            .unwrap();
        assert_eq!(approved.state, State::Approved);
    }

    #[test]
    fn a_boolean_mode_column_migrates_to_the_mode_form_with_its_meaning() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("approvals.db");
        {
            let connection = Connection::open(&path).unwrap();
            connection
                .execute(
                    "CREATE TABLE approval_modes (workspace TEXT NOT NULL,\
                     session_id TEXT NOT NULL, automatic INTEGER NOT NULL CHECK(automatic IN (0,1)),\
                     PRIMARY KEY(workspace,session_id)) STRICT",
                    (),
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO approval_modes VALUES (?1,?2,?3)",
                    params!["/w", "off", 0],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO approval_modes VALUES (?1,?2,?3)",
                    params!["/w", "on", 1],
                )
                .unwrap();
            connection.execute_batch("PRAGMA user_version=2").unwrap();
        }
        let store = match Store::open(&path) {
            Ok(store) => store,
            Err(error) => panic!("open failed: {error}"),
        };
        fn mode(store: &Store, session: &str) -> Option<ApprovalMode> {
            store
                .mode(&Scope {
                    workspace: "/w".into(),
                    session_id: session.into(),
                    task_id: String::new(),
                    run_id: String::new(),
                })
                .unwrap()
        }
        // A stored `automatic = 0` was MANUAL and `1` was AUTO; neither is
        // re-read as the default, and YOLO is reachable afterwards.
        assert_eq!(mode(&store, "off"), Some(ApprovalMode::Manual));
        assert_eq!(mode(&store, "on"), Some(ApprovalMode::Auto));
        assert_eq!(mode(&store, "absent"), None);
        store
            .set_mode(
                &Scope {
                    workspace: "/w".into(),
                    session_id: "on".into(),
                    task_id: String::new(),
                    run_id: String::new(),
                },
                ApprovalMode::Yolo,
            )
            .unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        assert_eq!(mode(&store, "on"), Some(ApprovalMode::Yolo));
        assert_eq!(mode(&store, "off"), Some(ApprovalMode::Manual));
        let version: i64 = Connection::open(&path)
            .unwrap()
            .query_row("PRAGMA user_version", (), |row| row.get(0))
            .unwrap();
        assert_eq!(version, 4);
    }

    #[test]
    fn auto_mode_never_publishes_a_decision_for_a_request_it_cannot_decide() {
        let store = Arc::new(Store::in_memory().unwrap());
        let service = Service::from_store(store.clone()).unwrap();
        let scope = Scope {
            workspace: "/w".into(),
            session_id: "s".into(),
            task_id: String::new(),
            run_id: String::new(),
        };
        // AUTO is the default for a new session, so this fence is the only
        // thing standing between a plan review and an automatic decision.
        assert_eq!(service.mode(&scope).unwrap(), ApprovalMode::Auto);
        let pending = service
            .create(Proposal {
                kind: Kind::Confirmation,
                authority: Authority::Consent,
                scope: scope.clone(),
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
        assert_eq!(pending.state, State::Pending);
        let mut next = pending.clone();
        next.state = State::Approved;
        assert!(matches!(
            store.commit(&pending, &next, ClaimChange::None, true),
            Err(Error::Conflict)
        ));
        assert_eq!(store.load(&pending.id).unwrap(), pending);
        assert_eq!(store.events("/w", Some("s"), 0).unwrap().len(), 1);
    }
}
