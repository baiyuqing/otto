use crate::approval::repository::*;
use crate::approval::{Error, Result};
use crate::storage::{Connection, OptionalExtension, Transaction, params};
use otto_core::approval::*;
use std::{path::Path, sync::Mutex, time::Duration};

pub struct Store {
    connection: Mutex<Connection>,
}
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS approval_modes (
 workspace TEXT NOT NULL, session_id TEXT NOT NULL, automatic INTEGER NOT NULL CHECK(automatic IN (0,1)),
 PRIMARY KEY(workspace,session_id)
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
        Self::from_connection(Connection::open(path).map_err(storage_error)?)
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
    let version: i64 = tx
        .query_row("PRAGMA user_version", (), |row| row.get(0))
        .map_err(storage_error)?;
    if !(0..=2).contains(&version) {
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
    tx.execute_batch(SCHEMA).map_err(storage_error)?;
    tx.execute_batch("PRAGMA user_version=2")
        .map_err(storage_error)?;
    tx.commit().map_err(storage_error)?;
    Ok(())
}
impl ApprovalStore for Store {
    fn mode(&self, scope: &Scope) -> Result<Option<bool>> {
        self.connection
            .lock()
            .map_err(|_| Error::Unavailable)?
            .query_row(
                "SELECT automatic FROM approval_modes WHERE workspace=?1 AND session_id=?2",
                params![scope.workspace, scope.session_id],
                |row| row.get::<i64>(0),
            )
            .optional()
            .map(|value| value.map(|v| v != 0))
            .map_err(storage_error)
    }
    fn set_mode(&self, scope: &Scope, automatic: bool) -> Result<()> {
        self.connection.lock().map_err(|_| Error::Unavailable)?.execute(
            "INSERT INTO approval_modes VALUES (?1,?2,?3) ON CONFLICT(workspace,session_id) DO UPDATE SET automatic=excluded.automatic",
            params![scope.workspace, scope.session_id, i64::from(automatic)],
        ).map(|_| ()).map_err(storage_error)
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
        let mut statement = tx.prepare("SELECT DISTINCT a.body FROM approvals a JOIN approval_visibility v ON v.approval_id=a.id WHERE v.workspace=?1 AND (?2='' OR v.session_id=?2 OR v.session_id='') AND a.state='pending' ORDER BY a.id")
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
        if next.id != expected.id
            || next.scope != expected.scope
            || next.revision != expected.revision
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
        if automatic {
            let enabled = tx
                .query_row(
                    "SELECT automatic FROM approval_modes WHERE workspace=?1 AND session_id=?2",
                    params![expected.scope.workspace, expected.scope.session_id],
                    |row| row.get::<i64>(0),
                )
                .optional()
                .map_err(storage_error)?
                .unwrap_or(1)
                != 0;
            if !enabled {
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
        tx.commit().map_err(storage_error)?;
        Ok(next.clone())
    }
}
fn encode(request: &Request) -> Result<String> {
    serde_json::to_string(request).map_err(|_| Error::Unavailable)
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
    use std::sync::Arc;

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
        service.set_auto_mode(&scope, false).unwrap();
        let pending = service
            .create(Proposal {
                kind: Kind::Confirmation,
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
}
