//! Durable delivery receipt and task state.

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiptState {
    Prepared,
    Applying,
    Started,
    Unknown,
}

impl ReceiptState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::Applying => "applying",
            Self::Started => "started",
            Self::Unknown => "unknown",
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "prepared" => Ok(Self::Prepared),
            "applying" => Ok(Self::Applying),
            "started" => Ok(Self::Started),
            "unknown" => Ok(Self::Unknown),
            _ => Err("delivery receipt state is invalid".into()),
        }
    }
}

/// Immutable facts committed before a delivery may mutate Git metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiptBinding {
    pub approval_id: String,
    pub source_workspace: String,
    pub target_repository: String,
    pub target_common_dir: String,
    pub worktree: String,
    pub branch: String,
    pub base: String,
    pub handoff_digest: String,
}

impl ReceiptBinding {
    fn valid(&self) -> bool {
        !self.approval_id.is_empty()
            && !self.source_workspace.is_empty()
            && Path::new(&self.target_repository).is_absolute()
            && Path::new(&self.target_common_dir).is_absolute()
            && Path::new(&self.worktree).is_absolute()
            && validate_branch(&self.branch).is_ok()
            && valid_object_id(&self.base)
            && self.handoff_digest.len() == 64
            && self
                .handoff_digest
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredReceipt {
    pub binding: ReceiptBinding,
    pub state: ReceiptState,
}

/// Terminal and in-progress state for the target-only delivery runner. This
/// deliberately lives outside the ordinary sub-agent task schema: a delivery
/// may retain a worktree for inspection after failure and cannot be replayed
/// from an interrupted effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    Running,
    Succeeded,
    Failed,
    Canceled,
    Unknown,
}

impl TaskState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
            Self::Unknown => "unknown",
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "running" => Ok(Self::Running),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            "canceled" => Ok(Self::Canceled),
            "unknown" => Ok(Self::Unknown),
            _ => Err("delivery task state is invalid".into()),
        }
    }

    pub fn terminal(self) -> bool {
        !matches!(self, Self::Running)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredTask {
    pub approval_id: String,
    pub source_session: String,
    pub target_session: String,
    pub worktree: String,
    pub state: TaskState,
    pub result: String,
    pub error: String,
    pub source_notified: bool,
}

/// Separate durable receipt store. It intentionally does not share the
/// ordinary sub-agent task table: delivery has a different recovery contract.
pub struct ReceiptStore {
    connection: Mutex<Connection>,
}

impl ReceiptStore {
    const SCHEMA: &'static str = "
CREATE TABLE IF NOT EXISTS delivery_receipts (
 approval_id TEXT PRIMARY KEY,
 source_workspace TEXT NOT NULL,
 target_repository TEXT NOT NULL,
 target_common_dir TEXT NOT NULL,
 worktree TEXT NOT NULL,
 branch TEXT NOT NULL,
 base TEXT NOT NULL,
 handoff_digest TEXT NOT NULL,
 state TEXT NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS delivery_sessions (
 approval_id TEXT PRIMARY KEY REFERENCES delivery_receipts(approval_id),
 session_id TEXT NOT NULL,
 worktree TEXT NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS delivery_tasks (
 approval_id TEXT PRIMARY KEY REFERENCES delivery_receipts(approval_id),
 source_session TEXT NOT NULL,
 target_session TEXT NOT NULL,
 worktree TEXT NOT NULL,
 state TEXT NOT NULL,
 result TEXT NOT NULL,
 error TEXT NOT NULL,
 source_notified INTEGER NOT NULL
) STRICT;";

    pub fn open(path: &Path) -> Result<Self, String> {
        use std::os::unix::fs::PermissionsExt;

        if let Some(parent) = path.parent().filter(|path| !path.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .map_err(|_| "delivery receipt storage is unavailable")?;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
                .map_err(|_| "delivery receipt storage is unavailable")?;
        }
        let connection =
            Connection::open(path).map_err(|_| "delivery receipt storage is unavailable")?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|_| "delivery receipt storage is unavailable")?;
        Self::from_connection(connection)
    }

    pub fn from_connection(connection: Connection) -> Result<Self, String> {
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(|_| "delivery receipt storage is unavailable")?;
        Self::initialize_schema(&connection)
            .map_err(|_| "delivery receipt storage is unavailable")?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    /// Creates the tables and adds the owner columns to a database written
    /// before they existed. `BEGIN IMMEDIATE` serializes two processes that
    /// open the same file at once, so only one runs each `ALTER TABLE`.
    fn initialize_schema(connection: &Connection) -> Result<(), crate::storage::Error> {
        connection.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| {
            connection.execute_batch(Self::SCHEMA)?;
            for table in ["delivery_receipts", "delivery_tasks"] {
                let mut columns = std::collections::HashSet::new();
                for name in connection
                    .prepare(&format!("PRAGMA table_info({table})"))?
                    .query_map((), |row| row.get::<String>(1))?
                {
                    columns.insert(name?);
                }
                for (column, declaration) in [
                    ("owner_pid", "INTEGER NOT NULL DEFAULT 0"),
                    ("owner_started", "TEXT NOT NULL DEFAULT ''"),
                ] {
                    if !columns.contains(column) {
                        connection.execute_batch(&format!(
                            "ALTER TABLE {table} ADD COLUMN {column} {declaration}"
                        ))?;
                    }
                }
            }
            Ok(())
        })();
        match result {
            Ok(()) => connection.execute_batch("COMMIT"),
            Err(error) => {
                let _ = connection.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }

    /// Overwrites the recorded owner of a receipt and its task.
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_owner_for_test(&self, approval_id: &str, pid: i64, started: &str) {
        let connection = self.connection.lock().unwrap();
        for table in ["delivery_receipts", "delivery_tasks"] {
            connection
                .execute(
                    &format!(
                        "UPDATE {table} SET owner_pid=?2, owner_started=?3 WHERE approval_id=?1"
                    ),
                    params![approval_id, pid, started],
                )
                .unwrap();
        }
    }

    #[cfg(test)]
    pub(super) fn in_memory() -> Self {
        Self::from_connection(Connection::open_in_memory().unwrap()).unwrap()
    }

    /// Commits the binding once. A retry for exactly the same approved facts
    /// returns the existing receipt; mismatched facts never overwrite it.
    pub fn prepare(&self, binding: ReceiptBinding) -> Result<StoredReceipt, String> {
        if !binding.valid() {
            return Err("delivery receipt binding is invalid".into());
        }
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| "delivery receipt storage is unavailable")?;
        let transaction = connection
            .transaction()
            .map_err(|_| "delivery receipt storage is unavailable")?;
        let existing = Self::get_from(&transaction, &binding.approval_id)?;
        if let Some(existing) = existing {
            if existing.binding != binding {
                return Err("delivery approval receipt conflicts with existing binding".into());
            }
            transaction
                .commit()
                .map_err(|_| "delivery receipt storage is unavailable")?;
            return Ok(existing);
        }
        transaction
            .execute(
                "INSERT INTO delivery_receipts (approval_id,source_workspace,target_repository,\
                 target_common_dir,worktree,branch,base,handoff_digest,state) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,'prepared')",
                params![
                    binding.approval_id,
                    binding.source_workspace,
                    binding.target_repository,
                    binding.target_common_dir,
                    binding.worktree,
                    binding.branch,
                    binding.base,
                    binding.handoff_digest,
                ],
            )
            .map_err(|_| "delivery receipt storage is unavailable")?;
        transaction
            .commit()
            .map_err(|_| "delivery receipt storage is unavailable")?;
        Ok(StoredReceipt {
            binding,
            state: ReceiptState::Prepared,
        })
    }

    pub fn get(&self, approval_id: &str) -> Result<Option<StoredReceipt>, String> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| "delivery receipt storage is unavailable")?;
        Self::get_from(&connection, approval_id)
    }

    /// Persists the one target-worktree session bound to an approved receipt.
    /// A retry may observe the same binding but can never replace it.
    pub fn bind_session(
        &self,
        approval_id: &str,
        session_id: &str,
        worktree: &Path,
    ) -> Result<(), String> {
        if session_id.is_empty() || !worktree.is_absolute() {
            return Err("delivery session binding is invalid".into());
        }
        let connection = self
            .connection
            .lock()
            .map_err(|_| "delivery receipt storage is unavailable")?;
        let existing: Option<(String, String)> = connection
            .query_row(
                "SELECT session_id,worktree FROM delivery_sessions WHERE approval_id=?1",
                [approval_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|_| "delivery receipt storage is unavailable")?;
        let worktree = worktree.to_string_lossy().into_owned();
        match existing {
            Some((id, path)) if id == session_id && path == worktree => Ok(()),
            Some(_) => Err("delivery session binding conflicts with existing receipt".into()),
            None => connection
                .execute(
                    "INSERT INTO delivery_sessions VALUES (?1,?2,?3)",
                    params![approval_id, session_id, worktree],
                )
                .map(|_| ())
                .map_err(|_| "delivery receipt storage is unavailable".into()),
        }
    }

    pub fn session(&self, approval_id: &str) -> Result<Option<(String, String)>, String> {
        self.connection
            .lock()
            .map_err(|_| "delivery receipt storage is unavailable")?
            .query_row(
                "SELECT session_id,worktree FROM delivery_sessions WHERE approval_id=?1",
                [approval_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|_| "delivery receipt storage is unavailable".into())
    }

    /// Creates the one target-runner task for a receipt. Existing identical
    /// bindings are idempotent; no caller may replace its target session.
    pub fn begin_task(
        &self,
        approval_id: &str,
        source_session: &str,
        target_session: &str,
        worktree: &Path,
    ) -> Result<StoredTask, String> {
        if source_session.is_empty() || target_session.is_empty() || !worktree.is_absolute() {
            return Err("delivery task binding is invalid".into());
        }
        let worktree = worktree.to_string_lossy().into_owned();
        let (owner_pid, owner_started) = crate::subagent::record::current_process();
        let connection = self
            .connection
            .lock()
            .map_err(|_| "delivery receipt storage is unavailable")?;
        if let Some(existing) = Self::task_from(&connection, approval_id)? {
            if existing.source_session == source_session
                && existing.target_session == target_session
                && existing.worktree == worktree
            {
                return Ok(existing);
            }
            return Err("delivery task binding conflicts with existing receipt".into());
        }
        connection
            .execute(
                "INSERT INTO delivery_tasks (approval_id,source_session,target_session,worktree,\
                 state,result,error,source_notified,owner_pid,owner_started) \
                 VALUES (?1,?2,?3,?4,'running','','',0,?5,?6)",
                params![
                    approval_id,
                    source_session,
                    target_session,
                    worktree,
                    owner_pid,
                    owner_started
                ],
            )
            .map_err(|_| "delivery receipt storage is unavailable")?;
        Self::task_from(&connection, approval_id)?
            .ok_or("delivery task storage is unavailable".into())
    }

    pub fn task(&self, approval_id: &str) -> Result<Option<StoredTask>, String> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| "delivery receipt storage is unavailable")?;
        Self::task_from(&connection, approval_id)
    }

    pub fn finish_task(
        &self,
        approval_id: &str,
        state: TaskState,
        result: &str,
        error: &str,
    ) -> Result<StoredTask, String> {
        if !state.terminal() {
            return Err("delivery task must finish in a terminal state".into());
        }
        let connection = self
            .connection
            .lock()
            .map_err(|_| "delivery receipt storage is unavailable")?;
        let task =
            Self::task_from(&connection, approval_id)?.ok_or("delivery task is unavailable")?;
        if task.state.terminal() {
            if task.state == state && task.result == result && task.error == error {
                return Ok(task);
            }
            return Err("delivery task is already terminal".into());
        }
        connection.execute(
            "UPDATE delivery_tasks SET state=?2,result=?3,error=?4 WHERE approval_id=?1 AND state='running'",
            params![approval_id, state.as_str(), result, error],
        ).map_err(|_| "delivery receipt storage is unavailable")?;
        Self::task_from(&connection, approval_id)?
            .ok_or("delivery task storage is unavailable".into())
    }

    /// Claims the source notification once after a terminal task. The caller
    /// must append only a text summary, never a target capability or path.
    pub fn claim_source_notification(
        &self,
        approval_id: &str,
    ) -> Result<Option<StoredTask>, String> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| "delivery receipt storage is unavailable")?;
        let task =
            Self::task_from(&connection, approval_id)?.ok_or("delivery task is unavailable")?;
        if !task.state.terminal() || task.source_notified {
            return Ok(None);
        }
        connection.execute(
            "UPDATE delivery_tasks SET source_notified=1 WHERE approval_id=?1 AND source_notified=0",
            [approval_id],
        ).map_err(|_| "delivery receipt storage is unavailable")?;
        Ok(Some(
            Self::task_from(&connection, approval_id)?
                .ok_or("delivery task storage is unavailable")?,
        ))
    }

    /// Marks a crash-interrupted target run unknown; it is never resumed
    /// automatically because its provider/tool effects may be ambiguous.
    /// Only tasks whose recorded owner process is gone are marked; a task
    /// owned by another live process (or by this one) is left running.
    pub fn mark_running_tasks_unknown(&self) -> Result<usize, String> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| "delivery receipt storage is unavailable")?;
        let dead = Self::dead_owner_ids(
            &connection,
            "SELECT approval_id,owner_pid,owner_started FROM delivery_tasks WHERE state='running'",
        )?;
        let mut marked = 0;
        for approval_id in dead {
            marked += connection
                .execute(
                    "UPDATE delivery_tasks SET state='unknown',error='delivery runner interrupted' \
                     WHERE approval_id=?1 AND state='running'",
                    [approval_id],
                )
                .map_err(|_| "delivery receipt storage is unavailable")?;
        }
        Ok(marked)
    }

    /// Approval ids of the rows `query` returns whose owner process is
    /// proven gone. A row without a recorded owner counts as gone; a row
    /// whose liveness cannot be determined counts as alive.
    fn dead_owner_ids(connection: &Connection, query: &str) -> Result<Vec<String>, String> {
        let rows = connection
            .prepare(query)
            .and_then(|mut statement| {
                statement
                    .query_map((), |row| {
                        Ok((
                            row.get::<String>(0)?,
                            row.get::<i64>(1)?,
                            row.get::<String>(2)?,
                        ))
                    })?
                    .collect::<Result<Vec<_>, _>>()
            })
            .map_err(|_| "delivery receipt storage is unavailable")?;
        Ok(rows
            .into_iter()
            .filter(|(_, pid, started)| {
                *pid == 0
                    || started.is_empty()
                    || crate::subagent::record::process_matches(*pid, started) == Some(false)
            })
            .map(|(approval_id, _, _)| approval_id)
            .collect())
    }

    /// Marks the exact receipt as executing. Retrying a started receipt is
    /// idempotent; an unknown receipt is never replayed.
    pub fn begin(&self, approval_id: &str) -> Result<StoredReceipt, String> {
        self.transition(approval_id, ReceiptState::Prepared, ReceiptState::Applying)
    }

    pub fn started(&self, approval_id: &str) -> Result<StoredReceipt, String> {
        self.transition(approval_id, ReceiptState::Applying, ReceiptState::Started)
    }

    pub fn unknown(&self, approval_id: &str) -> Result<StoredReceipt, String> {
        self.transition(approval_id, ReceiptState::Applying, ReceiptState::Unknown)
    }

    /// Crash recovery is deliberately conservative: no external Git effect is
    /// replayed. The caller must inspect the recorded worktree before a future
    /// explicit resume feature may continue it.
    /// Only receipts whose recorded owner process is gone are marked; one
    /// applying in another live process is left alone.
    pub fn mark_interrupted_unknown(&self) -> Result<usize, String> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| "delivery receipt storage is unavailable")?;
        let dead = Self::dead_owner_ids(
            &connection,
            "SELECT approval_id,owner_pid,owner_started FROM delivery_receipts WHERE state='applying'",
        )?;
        let mut marked = 0;
        for approval_id in dead {
            marked += connection
                .execute(
                    "UPDATE delivery_receipts SET state='unknown' \
                     WHERE approval_id=?1 AND state='applying'",
                    [approval_id],
                )
                .map_err(|_| "delivery receipt storage is unavailable")?;
        }
        Ok(marked)
    }

    fn transition(
        &self,
        approval_id: &str,
        expected: ReceiptState,
        next: ReceiptState,
    ) -> Result<StoredReceipt, String> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| "delivery receipt storage is unavailable")?;
        let transaction = connection
            .transaction()
            .map_err(|_| "delivery receipt storage is unavailable")?;
        let receipt =
            Self::get_from(&transaction, approval_id)?.ok_or("delivery receipt is unavailable")?;
        let receipt = match receipt.state {
            state if state == next => receipt,
            state if state == expected => {
                // Entering `applying` records this process as the owner.
                let (owner_pid, owner_started) = if next == ReceiptState::Applying {
                    crate::subagent::record::current_process()
                } else {
                    (0, String::new())
                };
                transaction
                    .execute(
                        "UPDATE delivery_receipts SET state=?2, \
                         owner_pid=iif(?2='applying',?4,owner_pid), \
                         owner_started=iif(?2='applying',?5,owner_started) \
                         WHERE approval_id=?1 AND state=?3",
                        params![
                            approval_id,
                            next.as_str(),
                            expected.as_str(),
                            owner_pid,
                            owner_started
                        ],
                    )
                    .map_err(|_| "delivery receipt storage is unavailable")?;
                StoredReceipt {
                    state: next,
                    ..receipt
                }
            }
            ReceiptState::Unknown => {
                return Err(
                    "delivery receipt outcome is unknown; inspect the worktree before retrying"
                        .into(),
                );
            }
            _ => return Err("delivery receipt is in an invalid state".into()),
        };
        transaction
            .commit()
            .map_err(|_| "delivery receipt storage is unavailable")?;
        Ok(receipt)
    }

    fn task_from(connection: &Connection, approval_id: &str) -> Result<Option<StoredTask>, String> {
        connection
            .query_row(
                "SELECT approval_id,source_session,target_session,worktree,state,result,error,source_notified FROM delivery_tasks WHERE approval_id=?1",
                [approval_id],
                |row| {
                    Ok(StoredTask {
                        approval_id: row.get(0)?,
                        source_session: row.get(1)?,
                        target_session: row.get(2)?,
                        worktree: row.get(3)?,
                        state: TaskState::parse(&row.get::<String>(4)?).map_err(|_| {
                            crate::storage::Error::Corrupt("delivery task state".into())
                        })?,
                        result: row.get(5)?,
                        error: row.get(6)?,
                        source_notified: row.get::<i64>(7)? != 0,
                    })
                },
            )
            .optional()
            .map_err(|_| "delivery receipt storage is unavailable".into())
    }

    fn get_from(
        connection: &Connection,
        approval_id: &str,
    ) -> Result<Option<StoredReceipt>, String> {
        connection
            .query_row(
                "SELECT approval_id,source_workspace,target_repository,target_common_dir,worktree,branch,base,handoff_digest,state FROM delivery_receipts WHERE approval_id=?1",
                [approval_id],
                |row| {
                    Ok(StoredReceipt {
                        binding: ReceiptBinding {
                            approval_id: row.get(0)?,
                            source_workspace: row.get(1)?,
                            target_repository: row.get(2)?,
                            target_common_dir: row.get(3)?,
                            worktree: row.get(4)?,
                            branch: row.get(5)?,
                            base: row.get(6)?,
                            handoff_digest: row.get(7)?,
                        },
                        state: ReceiptState::parse(&row.get::<String>(8)?).map_err(|_| {
                            crate::storage::Error::Corrupt("delivery receipt state".into())
                        })?,
                    })
                },
            )
            .optional()
            .map_err(|_| "delivery receipt storage is unavailable".into())
    }
}
