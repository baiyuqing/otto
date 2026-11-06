//! Approval schema DDL, initialization and the pre-v4 backup.

use super::*;

pub(super) const SCHEMA: &str = "
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

pub(in crate::storage::backends::sqlite) fn initialize(connection: &mut Connection) -> Result<()> {
    connection
        .busy_timeout(Duration::from_secs(5))
        .map_err(storage_error)?;
    let tx = connection.transaction().map_err(storage_error)?;
    initialize_tx(&tx)?;
    tx.commit().map_err(storage_error)
}
pub(super) fn initialize_tx(tx: &Transaction<'_>) -> Result<()> {
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
        tx.execute_batch(
            "INSERT OR IGNORE INTO approval_settings SELECT \
            workspace,session_id,'on_request',CASE mode WHEN 'manual' THEN 'user' \
            WHEN 'yolo' THEN 'bypass' ELSE 'auto_review' END,1 FROM approval_modes",
        )
        .map_err(storage_error)?;
    }
    tx.execute_batch("PRAGMA user_version=4")
        .map_err(storage_error)?;
    Ok(())
}

/// Whether `approval_modes` still carries the pre-mode `automatic` column.
pub(super) fn has_legacy_mode(tx: &Transaction<'_>) -> Result<bool> {
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

/// Copy a consistent pre-migration snapshot, including opaque envelopes and
/// the original schema. A transaction holds the source writer fence until the
/// destination is durable; a raw file copy would omit committed WAL pages.
pub(super) fn backup_before_v4(
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
    // Match source table definitions as data, then reconstruct with trusted SQL.
    // The pre-v4 backup does not replay any source DDL or expressions.
    let canonical = Connection::open_in_memory().map_err(storage_error)?;
    canonical.execute_batch(SCHEMA).map_err(storage_error)?;
    let mut trusted_indexes = Vec::new();
    let mut source_objects = tx.prepare(
        "SELECT sql FROM sqlite_schema WHERE type<>'table' AND sql IS NOT NULL AND tbl_name LIKE 'approval%'",
    ).map_err(storage_error)?;
    for source_sql in source_objects
        .query_map((), |row| row.get::<String>(0))
        .map_err(storage_error)?
    {
        let source_sql = source_sql.map_err(storage_error)?;
        let expected: String = canonical
            .query_row(
                "SELECT sql FROM sqlite_schema WHERE type='index' AND name='approvals_scope'",
                (),
                |row| row.get(0),
            )
            .map_err(storage_error)?;
        super::super::migration::legacy::validate_objects(
            &[source_sql],
            std::slice::from_ref(&expected),
        )
        .map_err(|_| Error::Invalid("unsupported approval backup object"))?;
        trusted_indexes.push(expected);
    }
    drop(source_objects);
    let mut trusted_tables = Vec::new();
    for (name, source_sql) in &tables {
        let expected: String = canonical
            .query_row(
                "SELECT sql FROM sqlite_schema WHERE type='table' AND name=?",
                [name.as_str()],
                |row| row.get(0),
            )
            .map_err(storage_error)?;
        let variants = [
            expected.clone(),
            expected.replace(
                "mode TEXT NOT NULL CHECK(mode IN ('manual','auto','yolo'))",
                "automatic INTEGER NOT NULL CHECK(automatic IN (0,1))",
            ),
            expected.replace(", recovery TEXT NOT NULL DEFAULT 'domain'", ""),
            expected.replace("DEFAULT 'domain'", "DEFAULT 'native'"),
        ];
        let trusted = variants
            .into_iter()
            .find(|candidate| {
                super::super::migration::legacy::validate_objects(
                    std::slice::from_ref(source_sql),
                    std::slice::from_ref(candidate),
                )
                .is_ok()
            })
            .ok_or(Error::Invalid("unsupported approval backup schema"))?;
        trusted_tables.push(trusted);
    }
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
    for schema in trusted_tables.into_iter().chain(trusted_indexes) {
        destination.execute_batch(&schema).map_err(storage_error)?;
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
