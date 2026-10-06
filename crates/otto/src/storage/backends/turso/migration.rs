//! Native isolated SQL staging. Domain codecs export logical records, never SQL dumps.
pub mod memory;
pub mod memory_advice;
mod named;
pub mod reflection;
pub mod schedule;
pub mod skill_checks;
pub mod tasks;
pub mod usage;
pub mod workflow;
use crate::storage::migration::{chunk_hash, contracts::*};
use crate::storage::{Connection, OptionalExtension, params};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

const SCAN_LIMIT: usize = 64 * 1024 * 1024;

pub use crate::storage::migration::contracts::FreezeGuard;

pub trait SqlDomainCodec: Send + Sync {
    fn domain(&self) -> &'static str;
    fn family(&self) -> &'static str;
    fn version(&self) -> u32;
    fn committed_cursor(&self, connection: &Connection) -> Result<String>;
    fn initialize(&self, connection: &mut Connection) -> Result<()>;
    /// Check encoded size before materializing a record. Cursor ordering is stable.
    fn next_record(
        &self,
        connection: &Connection,
        after: &str,
        max_bytes: usize,
    ) -> Result<Option<(String, Record)>>;
    fn import_record(&self, connection: &Connection, record: &Record) -> Result<()>;
    fn validate(&self, connection: &Connection) -> Result<()>;
    fn reopen(&self, connection: Connection) -> Result<()>;
}

pub struct Source {
    connection: Mutex<Option<Connection>>,
    codec: Arc<dyn SqlDomainCodec>,
    fence: Arc<dyn FreezeGuard>,
    snapshot: FrozenSnapshot,
}
impl Source {
    pub fn open(
        path: &Path,
        identity: String,
        generation: String,
        fence: Arc<dyn FreezeGuard>,
        codec: Arc<dyn SqlDomainCodec>,
    ) -> Result<Self> {
        if identity.is_empty() || generation.is_empty() || !path.is_file() {
            return Err(Error::Invalid(
                "missing source identity, generation or database".into(),
            ));
        }
        fence.validate(&generation)?;
        let connection = Connection::open_with_driver(
            &crate::storage::drivers::turso::TursoDriver,
            path,
            crate::storage::OpenOptions {
                policy: crate::storage::OpenPolicy::Existing,
                ..Default::default()
            },
            crate::storage::SqlRequirements::DURABLE,
        )
        .map_err(sql_error)?;
        connection
            .execute_batch("BEGIN IMMEDIATE")
            .map_err(sql_error)?;
        codec.validate(&connection)?;
        let (digest, _) = scan(&connection, codec.as_ref())?;
        let cursor = codec.committed_cursor(&connection)?;
        let snapshot = FrozenSnapshot {
            source_identity: identity,
            generation,
            domains: BTreeMap::from([(
                codec.domain().into(),
                DomainSnapshot {
                    schema_family: codec.family().into(),
                    schema_version: codec.version(),
                    committed_cursor: cursor,
                    digest,
                },
            )]),
        };
        Ok(Self {
            connection: Mutex::new(Some(connection)),
            codec,
            fence,
            snapshot,
        })
    }
}
#[async_trait]
impl SnapshotExport for Source {
    async fn descriptor(&self) -> Result<FrozenSnapshot> {
        self.fence.validate(&self.snapshot.generation)?;
        if self.connection.lock().map_err(lock_error)?.is_none() {
            return Err(Error::Conflict("source closed".into()));
        }
        Ok(self.snapshot.clone())
    }
    async fn validate_frozen(&self, expected: &FrozenSnapshot) -> Result<()> {
        self.fence.validate(&self.snapshot.generation)?;
        if expected != &self.snapshot {
            return Err(Error::Conflict("source generation changed".into()));
        }
        let connection = self.connection.lock().map_err(lock_error)?;
        if connection.is_none() {
            return Err(Error::Conflict("source closed".into()));
        }
        Ok(())
    }
    async fn read_chunk(&self, domain: &str, cursor: &str, max_bytes: usize) -> Result<Chunk> {
        self.validate_frozen(&self.snapshot).await?;
        if domain != self.codec.domain() {
            return Err(Error::Invalid("unsupported SQL domain".into()));
        }
        let connection = self.connection.lock().map_err(lock_error)?;
        let connection = connection
            .as_ref()
            .ok_or_else(|| Error::Conflict("source closed".into()))?;
        let mut chunk = Chunk {
            cursor: cursor.into(),
            next_cursor: None,
            records: vec![],
            blobs: vec![],
        };
        let mut after = cursor.to_owned();
        while let Some((next, record)) = self.codec.next_record(connection, &after, max_bytes)? {
            // ponytail: rehash each bounded chunk; incremental encoded sizing if
            // migrations with many small records make this measurable.
            chunk.records.push(record);
            chunk.next_cursor = Some(next.clone());
            if chunk_hash(&chunk, max_bytes).is_err() {
                chunk.records.pop();
                if chunk.records.is_empty() {
                    return Err(Error::Invalid("record exceeds chunk limit".into()));
                }
                chunk.next_cursor = Some(after);
                return Ok(chunk);
            }
            after = next;
        }
        chunk.next_cursor = None;
        chunk_hash(&chunk, max_bytes)?;
        Ok(chunk)
    }
    async fn close(&self) -> Result<()> {
        let fence = self.fence.validate(&self.snapshot.generation);
        if let Some(connection) = self.connection.lock().map_err(lock_error)?.take() {
            connection.execute_batch("ROLLBACK").map_err(sql_error)?;
            connection.close().map_err(sql_error)?;
        }
        fence
    }
}

pub struct Stage {
    driver: Arc<dyn crate::storage::SqlDriver>,
    closed: std::sync::atomic::AtomicBool,
    path: PathBuf,
    identity: String,
    codec: Arc<dyn SqlDomainCodec>,
    connection: Mutex<Option<Connection>>,
}
impl Stage {
    /// Caller owns and fences this isolated namespace before supplying its path.
    pub fn new(path: PathBuf, identity: String, codec: Arc<dyn SqlDomainCodec>) -> Self {
        Self::with_driver(
            path,
            identity,
            codec,
            Arc::new(crate::storage::drivers::turso::TursoDriver),
        )
    }
    pub fn with_driver(
        path: PathBuf,
        identity: String,
        codec: Arc<dyn SqlDomainCodec>,
        driver: Arc<dyn crate::storage::SqlDriver>,
    ) -> Self {
        Self {
            driver,
            closed: std::sync::atomic::AtomicBool::new(false),
            path,
            identity,
            codec,
            connection: Mutex::new(None),
        }
    }
    fn open_connection(&self, read_only: bool) -> Result<Connection> {
        Connection::open_with_driver(
            self.driver.as_ref(),
            &self.path,
            crate::storage::OpenOptions {
                read_only,
                ..Default::default()
            },
            crate::storage::SqlRequirements::DURABLE,
        )
        .map_err(sql_error)
    }
    fn discard_unknown(&self, connection: &mut Option<Connection>) {
        if let Some(connection) = connection.take() {
            let _ = connection.close();
        }
    }
    fn compatibility(&self, manifest: &Manifest) -> Result<()> {
        self.check_open()?;
        if manifest.max_chunk_bytes == 0 || manifest.max_chunk_bytes > SCAN_LIMIT {
            return Err(Error::Invalid(
                "unsupported migration chunk capacity".into(),
            ));
        }
        if self.identity != manifest.destination_identity || manifest.source.domains.len() != 1 {
            return Err(Error::Invalid(
                "stage identity or unsupported domain group".into(),
            ));
        }
        let domain = manifest
            .source
            .domains
            .get(self.codec.domain())
            .ok_or_else(|| Error::Invalid("unsupported SQL domain".into()))?;
        if domain.schema_family != self.codec.family()
            || domain.schema_version != self.codec.version()
        {
            return Err(Error::Invalid("unsupported domain codec version".into()));
        }
        Ok(())
    }
    fn check_open(&self) -> Result<()> {
        if self.closed.load(std::sync::atomic::Ordering::Acquire) {
            return Err(Error::Conflict("stage closed".into()));
        }
        Ok(())
    }
    fn ensure_open(&self, connection: &mut Option<Connection>) -> Result<()> {
        self.check_open()?;
        if connection.is_none() {
            *connection = Some(self.open_connection(false)?);
        }
        Ok(())
    }
}
const STAGE_TABLE: &str = "CREATE TABLE otto_migration_stage (singleton INTEGER PRIMARY KEY CHECK(singleton=1), migration_id TEXT NOT NULL, identity TEXT NOT NULL, source TEXT NOT NULL, max_chunk_bytes INTEGER NOT NULL CHECK(max_chunk_bytes>0), sealed INTEGER NOT NULL CHECK(sealed IN (0,1))) STRICT";
const CHUNKS_TABLE: &str = "CREATE TABLE otto_migration_chunks (migration_id TEXT NOT NULL, domain TEXT NOT NULL, cursor TEXT NOT NULL, body TEXT NOT NULL, PRIMARY KEY(migration_id,domain,cursor)) STRICT";
const STAGE_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS otto_migration_stage (singleton INTEGER PRIMARY KEY CHECK(singleton=1), migration_id TEXT NOT NULL, identity TEXT NOT NULL, source TEXT NOT NULL, max_chunk_bytes INTEGER NOT NULL CHECK(max_chunk_bytes>0), sealed INTEGER NOT NULL CHECK(sealed IN (0,1))) STRICT;
CREATE TABLE IF NOT EXISTS otto_migration_chunks (migration_id TEXT NOT NULL, domain TEXT NOT NULL, cursor TEXT NOT NULL, body TEXT NOT NULL, PRIMARY KEY(migration_id,domain,cursor)) STRICT;";

/// Closed physical bookkeeping allowance for repositories with exact schemas.
/// Every object is checked, including the implicit composite-key index.
pub(super) fn verified_schema_objects(
    connection: &Connection,
    normalize: fn(&str) -> String,
) -> crate::storage::Result<Vec<String>> {
    let mut statement=connection.prepare("SELECT type,name,tbl_name,sql FROM sqlite_schema WHERE name IN ('otto_migration_stage','otto_migration_chunks','sqlite_autoindex_otto_migration_chunks_1')")?;
    let objects = statement
        .query_map((), |r| {
            Ok((
                r.get::<String>(0)?,
                r.get::<String>(1)?,
                r.get::<String>(2)?,
                r.get::<Option<String>>(3)?,
            ))
        })?
        .collect::<crate::storage::Result<Vec<_>>>()?;
    if objects.is_empty() {
        return Ok(vec![]);
    }
    if objects.len() != 3 {
        return Err(crate::storage::Error::Corrupt(
            "incomplete migration bookkeeping schema".into(),
        ));
    }
    for (kind, name, table, sql) in &objects {
        let valid = match name.as_str() {
            "otto_migration_stage" => {
                kind == "table"
                    && table == name
                    && sql
                        .as_ref()
                        .is_some_and(|sql| normalize(sql) == normalize(STAGE_TABLE))
            }
            "otto_migration_chunks" => {
                kind == "table"
                    && table == name
                    && sql
                        .as_ref()
                        .is_some_and(|sql| normalize(sql) == normalize(CHUNKS_TABLE))
            }
            "sqlite_autoindex_otto_migration_chunks_1" => {
                kind == "index" && table == "otto_migration_chunks" && sql.is_none()
            }
            _ => false,
        };
        if !valid {
            return Err(crate::storage::Error::Corrupt(
                "incompatible migration bookkeeping object".into(),
            ));
        }
    }
    Ok(objects.into_iter().map(|(_, name, _, _)| name).collect())
}

#[async_trait]
impl StageImport for Stage {
    async fn validate_plan(&self, manifest: &Manifest) -> Result<()> {
        self.compatibility(manifest)?;
        if self.identity.is_empty() || self.path.as_os_str().is_empty() {
            return Err(Error::Invalid("empty staging identity/path".into()));
        }
        if self.path.exists() {
            let connection = self.open_connection(true)?;
            let exists: i64 = connection
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE name='otto_migration_stage'",
                    (),
                    |r| r.get(0),
                )
                .map_err(sql_error)?;
            if exists == 0 {
                let tables: i64 = connection
                    .query_row(
                        "SELECT COUNT(*) FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'",
                        (),
                        |r| r.get(0),
                    )
                    .map_err(sql_error)?;
                if tables == 0 {
                    return Ok(());
                } // Recovery after opening an empty staging file.
                return Err(Error::Conflict(
                    "destination is not an isolated migration stage".into(),
                ));
            }
            let source = serde_json::to_string(&manifest.source).map_err(json_error)?;
            let matches: i64 = connection.query_row("SELECT COUNT(*) FROM otto_migration_stage WHERE migration_id=?1 AND identity=?2 AND source=?3 AND max_chunk_bytes=?4", params![manifest.migration_id,self.identity,source,manifest.max_chunk_bytes], |r| r.get(0)).map_err(sql_error)?;
            if matches != 1 {
                return Err(Error::Conflict(
                    "staging namespace belongs to another migration".into(),
                ));
            }
        }
        Ok(())
    }
    async fn begin(&self, manifest: &Manifest) -> Result<()> {
        self.validate_plan(manifest).await?;
        let mut connection = self.connection.lock().map_err(lock_error)?;
        self.ensure_open(&mut connection)?;
        let outcome = (|| {
            let connection = connection.as_mut().unwrap();
            let tx = connection.transaction().map_err(sql_error)?;
            tx.execute_batch(STAGE_SCHEMA).map_err(sql_error)?;
            let source = serde_json::to_string(&manifest.source).map_err(json_error)?;
            tx.execute(
                "INSERT OR IGNORE INTO otto_migration_stage VALUES (1,?1,?2,?3,?4,0)",
                params![
                    manifest.migration_id,
                    self.identity,
                    source,
                    manifest.max_chunk_bytes
                ],
            )
            .map_err(sql_error)?;
            let matches: i64 = tx.query_row("SELECT COUNT(*) FROM otto_migration_stage WHERE migration_id=?1 AND identity=?2 AND source=?3 AND max_chunk_bytes=?4", params![manifest.migration_id,self.identity,source,manifest.max_chunk_bytes], |r| r.get(0)).map_err(sql_error)?;
            if matches != 1 {
                return Err(Error::Conflict(
                    "stage already belongs to another migration".into(),
                ));
            }
            tx.commit().map_err(sql_error)?;
            self.codec.initialize(connection)
        })();
        if matches!(outcome, Err(Error::CommitUnknown)) {
            self.discard_unknown(&mut connection);
        }
        outcome
    }
    async fn apply_chunk(
        &self,
        migration_id: &str,
        domain: &str,
        chunk: &Chunk,
        sha256: &str,
    ) -> Result<ChunkReceipt> {
        if domain != self.codec.domain()
            || !chunk.blobs.is_empty()
            || chunk_hash(chunk, SCAN_LIMIT)? != sha256
        {
            return Err(Error::Invalid(
                "unsupported domain, blob or chunk hash".into(),
            ));
        }
        let mut connection = self.connection.lock().map_err(lock_error)?;
        self.ensure_open(&mut connection)?;
        let tx = connection
            .as_mut()
            .unwrap()
            .transaction()
            .map_err(sql_error)?;
        let max_bytes: usize = tx
            .query_row(
                "SELECT max_chunk_bytes FROM otto_migration_stage WHERE migration_id=?1",
                [migration_id],
                |r| r.get(0),
            )
            .map_err(sql_error)?;
        chunk_hash(chunk, max_bytes)?;
        let receipt = ChunkReceipt {
            cursor: chunk.cursor.clone(),
            sha256: sha256.into(),
            next_cursor: chunk.next_cursor.clone(),
        };
        if let Some(existing) = receipt_from(&tx, migration_id, domain, &chunk.cursor)? {
            if existing != receipt {
                return Err(Error::Conflict(
                    "chunk key reused with different content".into(),
                ));
            }
            return Ok(existing);
        }
        let valid: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM otto_migration_stage WHERE migration_id=?1 AND sealed=0",
                [migration_id],
                |r| r.get(0),
            )
            .map_err(sql_error)?;
        if valid != 1 {
            return Err(Error::Conflict(
                "stage missing, sealed or migration identity differs".into(),
            ));
        }
        for record in &chunk.records {
            self.codec.import_record(&tx, record)?;
        }
        tx.execute(
            "INSERT INTO otto_migration_chunks VALUES (?1,?2,?3,?4)",
            params![
                migration_id,
                domain,
                chunk.cursor,
                serde_json::to_string(&receipt).map_err(json_error)?
            ],
        )
        .map_err(sql_error)?;
        match tx.commit().map_err(sql_error) {
            Ok(()) => Ok(receipt),
            Err(Error::CommitUnknown) => {
                // Never query the handle that lost its commit acknowledgement.
                self.discard_unknown(&mut connection);
                let fresh = match self.open_connection(false) {
                    Ok(fresh) => fresh,
                    Err(_) => return Err(Error::CommitUnknown),
                };
                match receipt_from(&fresh, migration_id, domain, &chunk.cursor) {
                    Ok(actual) => {
                        *connection = Some(fresh);
                        match actual {
                            Some(actual) if actual == receipt => Ok(actual),
                            _ => Err(Error::CommitUnknown),
                        }
                    }
                    Err(_) => {
                        let _ = fresh.close();
                        Err(Error::CommitUnknown)
                    }
                }
            }
            Err(error) => Err(error),
        }
    }
    async fn chunk_receipt(
        &self,
        migration_id: &str,
        domain: &str,
        cursor: &str,
    ) -> Result<Option<ChunkReceipt>> {
        self.check_open()?;
        if !self.path.exists() {
            return Ok(None);
        }
        let mut connection = self.connection.lock().map_err(lock_error)?;
        self.ensure_open(&mut connection)?;
        receipt_from(connection.as_ref().unwrap(), migration_id, domain, cursor)
    }
    async fn verify(&self, manifest: &Manifest) -> Result<BTreeMap<String, DomainDigest>> {
        self.compatibility(manifest)?;
        let mut connection = self.connection.lock().map_err(lock_error)?;
        self.ensure_open(&mut connection)?;
        let connection = connection.as_ref().unwrap();
        self.codec.validate(connection)?;
        let (digest, _) = scan(connection, self.codec.as_ref())?;
        let progress = manifest
            .progress
            .get(self.codec.domain())
            .ok_or_else(|| Error::Invalid("missing domain progress".into()))?;
        for receipt in &progress.chunks {
            if receipt_from(
                connection,
                &manifest.migration_id,
                self.codec.domain(),
                &receipt.cursor,
            )?
            .as_ref()
                != Some(receipt)
            {
                return Err(Error::Invalid("missing or divergent import receipt".into()));
            }
        }
        Ok(BTreeMap::from([(self.codec.domain().into(), digest)]))
    }
    async fn seal(&self, manifest: &Manifest) -> Result<()> {
        let actual = self.verify(manifest).await?;
        if actual[self.codec.domain()] != manifest.source.domains[self.codec.domain()].digest {
            return Err(Error::Invalid("stage digest mismatch".into()));
        }
        let mut connection = self.connection.lock().map_err(lock_error)?;
        self.ensure_open(&mut connection)?;
        let tx = connection
            .as_mut()
            .unwrap()
            .transaction()
            .map_err(sql_error)?;
        tx.execute(
            "UPDATE otto_migration_stage SET sealed=1 WHERE migration_id=?1",
            [manifest.migration_id.as_str()],
        )
        .map_err(sql_error)?;
        if let Err(error) = tx.commit().map_err(sql_error) {
            if matches!(error, Error::CommitUnknown) {
                self.discard_unknown(&mut connection);
            }
            return Err(error);
        }
        connection.take().unwrap().close().map_err(sql_error)?;
        self.codec.reopen(self.open_connection(false)?)
    }
    async fn close(&self) -> Result<()> {
        self.closed
            .store(true, std::sync::atomic::Ordering::Release);
        if let Some(connection) = self.connection.lock().map_err(lock_error)?.take() {
            connection.close().map_err(sql_error)?;
        }
        Ok(())
    }
}
fn receipt_from(
    connection: &Connection,
    id: &str,
    domain: &str,
    cursor: &str,
) -> Result<Option<ChunkReceipt>> {
    connection.query_row("SELECT body FROM otto_migration_chunks WHERE migration_id=?1 AND domain=?2 AND cursor=?3", params![id,domain,cursor], |r| r.get::<String>(0)).optional().map_err(sql_error)?.map(|body| serde_json::from_str(&body).map_err(json_error)).transpose()
}
fn scan(connection: &Connection, codec: &dyn SqlDomainCodec) -> Result<(DomainDigest, String)> {
    let mut hash = Sha256::new();
    let mut cursor = String::new();
    let mut records = 0;
    while let Some((next, record)) = codec.next_record(connection, &cursor, SCAN_LIMIT)? {
        if next == cursor {
            return Err(Error::Invalid("non-progressing codec cursor".into()));
        }
        let bytes = serde_json::to_vec(&record).map_err(json_error)?;
        hash.update((bytes.len() as u64).to_be_bytes());
        hash.update(bytes);
        records += 1;
        cursor = next;
    }
    Ok((
        DomainDigest {
            records,
            blob_bytes: 0,
            sha256: format!("{:x}", hash.finalize()),
        },
        cursor,
    ))
}
fn sql_error(error: crate::storage::Error) -> Error {
    match error {
        crate::storage::Error::CommitUnknown => Error::CommitUnknown,
        other => Error::Unavailable(other.to_string()),
    }
}
fn json_error(error: serde_json::Error) -> Error {
    Error::Invalid(error.to_string())
}
fn lock_error<T>(_: std::sync::PoisonError<T>) -> Error {
    Error::Unavailable("migration lock poisoned".into())
}

pub struct ApprovalCodec;
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum ApprovalRecord {
    Mode {
        workspace: String,
        session_id: String,
        automatic: bool,
    },
    Request {
        subject_key: String,
        request: String,
    },
    Visibility {
        approval_id: String,
        workspace: String,
        session_id: String,
    },
    InterruptedClaim {
        approval_id: String,
        recovery: String,
    },
    Event {
        sequence: i64,
        request: String,
    },
}
impl ApprovalRecord {
    fn metadata(&self) -> Result<(String, String)> {
        Ok(match self {
            ApprovalRecord::Mode {
                workspace,
                session_id,
                ..
            } => (
                format!(
                    "mode:{}",
                    serde_json::to_string(&(workspace, session_id)).map_err(json_error)?
                ),
                "1".into(),
            ),
            ApprovalRecord::Request { request, .. } => {
                let typed = approval_request(request)?;
                (format!("request:{}", typed.id), typed.revision.to_string())
            }
            ApprovalRecord::Visibility {
                approval_id,
                workspace,
                session_id,
            } => (
                format!(
                    "visibility:{}",
                    serde_json::to_string(&(approval_id, workspace, session_id))
                        .map_err(json_error)?
                ),
                "1".into(),
            ),
            ApprovalRecord::InterruptedClaim { approval_id, .. } => {
                (format!("claim:{approval_id}"), "1".into())
            }
            ApprovalRecord::Event { sequence, request } => (
                format!("event:{sequence}"),
                approval_request(request)?.revision.to_string(),
            ),
        })
    }
}
impl SqlDomainCodec for ApprovalCodec {
    fn domain(&self) -> &'static str {
        "approvals"
    }
    fn family(&self) -> &'static str {
        "otto.approvals"
    }
    fn version(&self) -> u32 {
        1
    }
    fn committed_cursor(&self, connection: &Connection) -> Result<String> {
        connection
            .query_row(
                "SELECT COALESCE(MAX(sequence),0) FROM approval_events",
                (),
                |r| r.get::<i64>(0),
            )
            .map(|v| v.to_string())
            .map_err(sql_error)
    }
    fn initialize(&self, connection: &mut Connection) -> Result<()> {
        super::approval::initialize(connection).map_err(approval_error)
    }
    fn next_record(
        &self,
        connection: &Connection,
        after: &str,
        max_bytes: usize,
    ) -> Result<Option<(String, Record)>> {
        let (mut section, mut offset): (usize, usize) = if after.is_empty() {
            (0, 0)
        } else {
            let (section, offset) = after
                .split_once(':')
                .ok_or_else(|| Error::Invalid("invalid approval cursor".into()))?;
            (
                section
                    .parse()
                    .map_err(|_| Error::Invalid("invalid section".into()))?,
                offset
                    .parse()
                    .map_err(|_| Error::Invalid("invalid offset".into()))?,
            )
        };
        // ponytail: OFFSET scans are quadratic for large snapshots; switch these
        // five known record kinds to keyset cursors when migration volume needs it.
        while section < 5 {
            let (fields, lengths, table, order) = match section {
                0 => (
                    "workspace,session_id,automatic",
                    "length(CAST(workspace AS BLOB))+length(CAST(session_id AS BLOB))+8",
                    "approval_modes",
                    "workspace,session_id",
                ),
                1 => (
                    "subject_key,body",
                    "length(CAST(subject_key AS BLOB))+length(CAST(body AS BLOB))",
                    "approvals",
                    "id",
                ),
                2 => (
                    "approval_id,workspace,session_id",
                    "length(CAST(approval_id AS BLOB))+length(CAST(workspace AS BLOB))+length(CAST(session_id AS BLOB))",
                    "approval_visibility",
                    "approval_id,workspace,session_id",
                ),
                3 => (
                    "approval_id,recovery",
                    "length(CAST(approval_id AS BLOB))+8",
                    "approval_claims",
                    "approval_id",
                ),
                4 => (
                    "sequence,body",
                    "length(CAST(body AS BLOB))+20",
                    "approval_events",
                    "sequence",
                ),
                _ => unreachable!(),
            };
            let length: Option<i64> = connection
                .query_row(
                    &format!("SELECT {lengths} FROM {table} ORDER BY {order} LIMIT 1 OFFSET ?1"),
                    [offset],
                    |r| r.get(0),
                )
                .optional()
                .map_err(sql_error)?;
            let Some(length) = length else {
                section += 1;
                offset = 0;
                continue;
            };
            // JSON can expand every byte to six bytes, then the neutral payload
            // Vec<u8> encoding expands each byte to four. Bound before loading.
            if length < 0
                || (length as usize)
                    .checked_mul(24)
                    .and_then(|n| n.checked_add(1024))
                    .is_none_or(|n| n > max_bytes)
            {
                return Err(Error::Invalid(
                    "approval record exceeds migration chunk limit".into(),
                ));
            }
            let record = connection
                .query_row(
                    &format!("SELECT {fields} FROM {table} ORDER BY {order} LIMIT 1 OFFSET ?1"),
                    [offset],
                    |r| {
                        let value = match section {
                            0 => ApprovalRecord::Mode {
                                workspace: r.get(0)?,
                                session_id: r.get(1)?,
                                automatic: r.get(2)?,
                            },
                            1 => ApprovalRecord::Request {
                                subject_key: r.get(0)?,
                                request: r.get(1)?,
                            },
                            2 => ApprovalRecord::Visibility {
                                approval_id: r.get(0)?,
                                workspace: r.get(1)?,
                                session_id: r.get(2)?,
                            },
                            3 => ApprovalRecord::InterruptedClaim {
                                approval_id: r.get(0)?,
                                recovery: r.get(1)?,
                            },
                            4 => ApprovalRecord::Event {
                                sequence: r.get(0)?,
                                request: r.get(1)?,
                            },
                            _ => unreachable!(),
                        };
                        Ok(value)
                    },
                )
                .map_err(sql_error)?;
            let (id, revision) = record.metadata()?;
            let payload = serde_json::to_vec(&record).map_err(json_error)?;
            return Ok(Some((
                format!("{section}:{}", offset + 1),
                Record {
                    id,
                    revision,
                    payload,
                },
            )));
        }
        if section != 5 {
            return Err(Error::Invalid("unsupported approval cursor section".into()));
        }
        Ok(None)
    }
    fn import_record(&self, connection: &Connection, record: &Record) -> Result<()> {
        let value: ApprovalRecord = serde_json::from_slice(&record.payload).map_err(json_error)?;
        let expected = value.metadata()?;
        if record.id != expected.0 || record.revision != expected.1 {
            return Err(Error::Invalid(
                "logical approval identity/revision mismatch".into(),
            ));
        }
        match value {
            ApprovalRecord::Mode {
                workspace,
                session_id,
                automatic,
            } => {
                connection
                    .execute(
                        "INSERT INTO approval_modes VALUES (?1,?2,?3)",
                        params![workspace, session_id, automatic],
                    )
                    .map_err(sql_error)?;
            }
            ApprovalRecord::Request {
                subject_key,
                request,
            } => {
                let body = request;
                let request = approval_request(&body)?;
                if record.id != format!("request:{}", request.id)
                    || record.revision != request.revision.to_string()
                {
                    return Err(Error::Invalid("approval identity/revision mismatch".into()));
                }
                let state = serde_json::to_value(request.state).map_err(json_error)?;
                connection
                    .execute(
                        "INSERT INTO approvals VALUES (?1,?2,?3,?4,?5,?6)",
                        params![
                            request.id,
                            subject_key,
                            request.scope.workspace,
                            request.scope.session_id,
                            state.as_str().unwrap(),
                            body
                        ],
                    )
                    .map_err(sql_error)?;
            }
            ApprovalRecord::Visibility {
                approval_id,
                workspace,
                session_id,
            } => {
                connection
                    .execute(
                        "INSERT INTO approval_visibility VALUES (?1,?2,?3)",
                        params![approval_id, workspace, session_id],
                    )
                    .map_err(sql_error)?;
            }
            ApprovalRecord::InterruptedClaim {
                approval_id,
                recovery,
            } => {
                // Preserve intent and ensure recovery observes a dead owner.
                // The destination service claims ownership before reconciliation.
                connection.execute("INSERT INTO approval_claims VALUES (?1,'migration-interrupted',2147483647,'1970-01-01T00:00:00Z',?2)",params![approval_id,recovery]).map_err(sql_error)?;
            }
            ApprovalRecord::Event { sequence, request } => {
                let body = request;
                let request = approval_request(&body)?;
                if sequence <= 0
                    || record.id != format!("event:{sequence}")
                    || record.revision != request.revision.to_string()
                {
                    return Err(Error::Invalid(
                        "approval event identity/revision mismatch".into(),
                    ));
                }
                connection
                    .execute(
                        "INSERT INTO approval_events VALUES (?1,?2,?3,?4,?5)",
                        params![
                            sequence,
                            request.id,
                            request.scope.workspace,
                            request.scope.session_id,
                            body
                        ],
                    )
                    .map_err(sql_error)?;
            }
        }
        Ok(())
    }
    fn validate(&self, connection: &Connection) -> Result<()> {
        let version: i64 = connection
            .query_row("PRAGMA user_version", (), |r| r.get(0))
            .map_err(sql_error)?;
        if version != 2 {
            return Err(Error::Invalid(
                "unsupported approval database schema".into(),
            ));
        }
        let unknown: i64 = connection.query_row("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name NOT IN ('approval_modes','approvals','approval_events','approval_claims','approval_visibility','sqlite_sequence','__turso_internal_seq___turso_internal_autoincrement_approval_events','otto_migration_stage','otto_migration_chunks')", (), |r| r.get(0)).map_err(sql_error)?;
        if unknown != 0 {
            return Err(Error::Invalid(
                "unsupported approval state tables; refusing partial migration".into(),
            ));
        }
        // Current Turso implements this known AUTOINCREMENT using a physical
        // sequence table. Its contents are rebuilt by importing explicit event
        // IDs; it is not an independently owned domain record.
        let sequence_schema:Option<String>=connection.query_row("SELECT sql FROM sqlite_master WHERE type='table' AND name='__turso_internal_seq___turso_internal_autoincrement_approval_events' AND length(sql)<=4096",(),|row|row.get(0)).optional().map_err(sql_error)?;
        if let Some(actual) = sequence_schema {
            let expected = "CREATE TABLE \"__turso_internal_seq___turso_internal_autoincrement_approval_events\"(value INTEGER PRIMARY KEY,is_called INTEGER,start INTEGER,inc INTEGER,min INTEGER,max INTEGER,cycle INTEGER)";
            let normalize = super::memory::schema::normalize_schema_sql;
            if normalize(&actual) != normalize(expected) {
                return Err(Error::Invalid(
                    "unsupported approval event sequence schema".into(),
                ));
            }
            let invalid:i64=connection.query_row("SELECT COUNT(*) FROM __turso_internal_seq___turso_internal_autoincrement_approval_events WHERE value<1 OR is_called NOT IN (0,1) OR start<>1 OR inc<>1 OR min<>1 OR max<>9223372036854775807 OR cycle<>0 OR is_called IS NULL OR start IS NULL OR inc IS NULL OR min IS NULL OR max IS NULL OR cycle IS NULL",(),|row|row.get(0)).map_err(sql_error)?;
            if invalid != 0 {
                return Err(Error::Invalid(
                    "unsupported approval event sequence configuration".into(),
                ));
            }
        } else {
            let exists:i64=connection.query_row("SELECT COUNT(*) FROM sqlite_master WHERE name='__turso_internal_seq___turso_internal_autoincrement_approval_events'",(),|row|row.get(0)).map_err(sql_error)?;
            if exists != 0 {
                return Err(Error::Invalid(
                    "invalid approval event sequence schema".into(),
                ));
            }
        }
        let broken: i64 = connection.query_row("SELECT (SELECT COUNT(*) FROM approval_visibility v LEFT JOIN approvals a ON a.id=v.approval_id WHERE a.id IS NULL) + (SELECT COUNT(*) FROM approval_claims c LEFT JOIN approvals a ON a.id=c.approval_id WHERE a.id IS NULL) + (SELECT COUNT(*) FROM approval_events e LEFT JOIN approvals a ON a.id=e.approval_id WHERE a.id IS NULL)",(),|r|r.get(0)).map_err(sql_error)?;
        if broken != 0 {
            return Err(Error::Invalid("approval references are incomplete".into()));
        }
        let oversized: i64 = connection
            .query_row(
                "SELECT (SELECT COUNT(*) FROM approvals WHERE length(CAST(body AS BLOB))>?1)+(SELECT COUNT(*) FROM approval_events WHERE length(CAST(body AS BLOB))>?1)",
                [SCAN_LIMIT / 24],
                |r| r.get(0),
            )
            .map_err(sql_error)?;
        if oversized != 0 {
            return Err(Error::Invalid(
                "approval record exceeds validation bound".into(),
            ));
        }
        let mut statement = connection
            .prepare("SELECT id,workspace,session_id,state,body FROM approvals")
            .map_err(sql_error)?;
        for row in statement
            .query_map((), |r| {
                Ok((
                    r.get::<String>(0)?,
                    r.get::<String>(1)?,
                    r.get::<String>(2)?,
                    r.get::<String>(3)?,
                    r.get::<String>(4)?,
                ))
            })
            .map_err(sql_error)?
        {
            let (id, workspace, session, state, body) = row.map_err(sql_error)?;
            let request = approval_request(&body)?;
            if request.id != id
                || request.scope.workspace != workspace
                || request.scope.session_id != session
                || serde_json::to_value(request.state)
                    .map_err(json_error)?
                    .as_str()
                    != Some(&state)
            {
                return Err(Error::Invalid(
                    "approval projection differs from canonical record".into(),
                ));
            }
        }
        Ok(())
    }
    fn reopen(&self, connection: Connection) -> Result<()> {
        super::approval::Store::from_connection(connection)
            .map(|_| ())
            .map_err(approval_error)
    }
}
fn approval_error(error: crate::approval::Error) -> Error {
    match error {
        crate::approval::Error::CommitUnknown => Error::CommitUnknown,
        crate::approval::Error::Invalid(message) => Error::Invalid(message.into()),
        other => Error::Unavailable(other.to_string()),
    }
}

fn approval_request(value: &str) -> Result<otto_core::approval::Request> {
    super::approval::decode_persisted(value).map_err(approval_error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::{Actor, ApplicationState, Kind, Proposal, Scope, Service, Subject};
    use std::sync::atomic::{AtomicBool, Ordering};

    struct Fence(AtomicBool);
    impl FreezeGuard for Fence {
        fn validate(&self, generation: &str) -> Result<()> {
            if self.0.load(Ordering::SeqCst) && generation == "frozen-1" {
                Ok(())
            } else {
                Err(Error::Conflict("maintenance marker changed".into()))
            }
        }
    }
    #[test]
    fn approval_schema_accepts_its_native_sequence_and_rejects_unknown_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("approvals.db");
        Service::open(&path).unwrap();
        let connection = Connection::open(&path).unwrap();
        assert!(ApprovalCodec.validate(&connection).is_ok());
        connection
            .execute_batch("CREATE TABLE unowned_approval_state(id TEXT)")
            .unwrap();
        assert!(matches!(
            ApprovalCodec.validate(&connection),
            Err(Error::Invalid(_))
        ));
    }
    #[tokio::test]
    async fn approval_export_import_resumes_receipts_preserves_unknown_fields_and_reopens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.db");
        let stage_path = dir.path().join("destination.db");
        let service = Service::open(&path).unwrap();
        let scope = Scope {
            workspace: "/w".into(),
            session_id: "s".into(),
            task_id: String::new(),
            run_id: String::new(),
        };
        service.set_auto_mode(&scope, false).unwrap();
        let request = service
            .create(Proposal {
                kind: Kind::Authorization,
                scope: scope.clone(),
                subject: Subject {
                    tool_call_id: String::new(),
                    source: "native-review".into(),
                    id: "operation".into(),
                    version: "7".into(),
                    details: "prepared".into(),
                },
                title: "Review".into(),
                reason: "scope".into(),
                persistent: false,
                blocking: true,
                expires_at: None,
            })
            .unwrap();
        service
            .decide_and_claim(&request.id, &scope, 1, true, &Actor::local("test"))
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
        fn raw_extension(body: &str) -> String {
            let value: serde_json::Value = serde_json::from_str(body).unwrap();
            let pretty = serde_json::to_string_pretty(&value).unwrap();
            let prefix = pretty.strip_suffix('}').unwrap();
            format!(
                "\n\t{prefix},\n  \"future_extension\": {{\"receipt\": \"keep-me\", \"precise\": 123456789012345678901234567890.123456789012345678901234567890}}\n}} \r\n"
            )
        }
        let original_body = raw_extension(&body);
        connection
            .execute(
                "UPDATE approvals SET body=?2 WHERE id=?1",
                params![request.id, original_body],
            )
            .unwrap();
        let mut statement = connection
            .prepare("SELECT sequence,body FROM approval_events ORDER BY sequence")
            .unwrap();
        let event_bodies = statement
            .query_map((), |r| Ok((r.get::<i64>(0)?, r.get::<String>(1)?)))
            .unwrap()
            .collect::<crate::storage::Result<Vec<_>>>()
            .unwrap()
            .into_iter()
            .map(|(sequence, body)| (sequence, raw_extension(&body)))
            .collect::<Vec<_>>();
        drop(statement);
        for (sequence, body) in &event_bodies {
            connection
                .execute(
                    "UPDATE approval_events SET body=?2 WHERE sequence=?1",
                    params![sequence, body],
                )
                .unwrap();
        }
        connection.close().unwrap();
        let fence = Arc::new(Fence(AtomicBool::new(true)));
        let codec = Arc::new(ApprovalCodec);
        let source = Source::open(
            &path,
            "source".into(),
            "frozen-1".into(),
            fence.clone(),
            codec.clone(),
        )
        .unwrap();
        let snapshot = source.descriptor().await.unwrap();
        assert_eq!(snapshot.domains["approvals"].committed_cursor, "2");
        let mut manifest = Manifest {
            format_version: FORMAT_VERSION,
            migration_id: "copy".into(),
            destination_identity: "target".into(),
            expected_route_generation: "route-1".into(),
            max_chunk_bytes: 64 * 1024,
            source: snapshot.clone(),
            progress: BTreeMap::from([(
                "approvals".into(),
                Progress {
                    cursor: String::new(),
                    complete: false,
                    chunks: vec![],
                },
            )]),
            destination_write_started: true,
            sealed: false,
            published_generation: None,
            rolled_back: false,
        };
        let stage = Stage::new(stage_path.clone(), "target".into(), codec.clone());
        stage.begin(&manifest).await.unwrap();
        let chunk = source.read_chunk("approvals", "", 64 * 1024).await.unwrap();
        assert!(chunk.next_cursor.is_none());
        let hash = chunk_hash(&chunk, 64 * 1024).unwrap();
        let receipt = stage
            .apply_chunk("copy", "approvals", &chunk, &hash)
            .await
            .unwrap();
        stage.close().await.unwrap();
        assert!(matches!(
            stage.begin(&manifest).await,
            Err(Error::Conflict(_))
        ));
        // Simulate a lost coordinator checkpoint after the destination commit.
        let stage = Stage::new(stage_path.clone(), "target".into(), codec);
        stage.begin(&manifest).await.unwrap();
        assert_eq!(
            stage.chunk_receipt("copy", "approvals", "").await.unwrap(),
            Some(receipt.clone())
        );
        assert_eq!(
            stage
                .apply_chunk("copy", "approvals", &chunk, &hash)
                .await
                .unwrap(),
            receipt
        );
        manifest
            .progress
            .get_mut("approvals")
            .unwrap()
            .chunks
            .push(receipt);
        manifest.progress.get_mut("approvals").unwrap().complete = true;
        assert_eq!(
            stage.verify(&manifest).await.unwrap()["approvals"],
            snapshot.domains["approvals"].digest
        );
        stage.seal(&manifest).await.unwrap();
        stage.seal(&manifest).await.unwrap();
        stage.close().await.unwrap();
        let connection = Connection::open(&stage_path).unwrap();
        let migrated: String = connection
            .query_row(
                "SELECT body FROM approvals WHERE id=?1",
                [request.id.as_str()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(migrated, original_body);
        let mut statement = connection
            .prepare("SELECT sequence,body FROM approval_events ORDER BY sequence")
            .unwrap();
        let migrated_events = statement
            .query_map((), |r| Ok((r.get::<i64>(0)?, r.get::<String>(1)?)))
            .unwrap()
            .collect::<crate::storage::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(migrated_events, event_bodies);
        drop(statement);
        connection.close().unwrap();
        let service = Service::open(&stage_path).unwrap();
        assert!(!service.auto_mode(&scope).unwrap());
        assert_eq!(service.events("/w", Some("s"), 0).unwrap().len(), 2);
        service.apply_available("/w").unwrap();
        assert_eq!(
            service
                .get(&request.id, "/w", Some("s"))
                .unwrap()
                .application,
            ApplicationState::Unknown
        );
        let changed = Connection::open(&stage_path)
            .unwrap()
            .query_row(
                "SELECT body FROM approvals WHERE id=?1",
                [request.id.as_str()],
                |row| row.get::<String>(0),
            )
            .unwrap();
        let raw_fields = |body: &str| {
            serde_json::from_str::<BTreeMap<String, Box<serde_json::value::RawValue>>>(body)
                .unwrap()
        };
        assert_eq!(
            raw_fields(&changed)["future_extension"].get(),
            raw_fields(&original_body)["future_extension"].get()
        );
        let event = Connection::open(&stage_path)
            .unwrap()
            .query_row(
                "SELECT body FROM approval_events ORDER BY sequence DESC LIMIT 1",
                (),
                |row| row.get::<String>(0),
            )
            .unwrap();
        assert_eq!(
            raw_fields(&event)["future_extension"].get(),
            raw_fields(&original_body)["future_extension"].get()
        );
        let mut future_schema: serde_json::Value = serde_json::from_str(&original_body).unwrap();
        future_schema["schema_version"] = serde_json::json!(2);
        assert!(approval_request(&serde_json::to_string(&future_schema).unwrap()).is_err());
        future_schema["schema_version"] = serde_json::json!(1);
        future_schema["scope"]["unrecognized_authority"] = serde_json::json!(true);
        assert!(approval_request(&serde_json::to_string(&future_schema).unwrap()).is_err());
        // Wire/model decoding remains strict even for the allowed persisted
        // additive envelope metadata.
        assert!(serde_json::from_str::<otto_core::approval::Request>(&original_body).is_err());
        // Explicitly imported IDs must advance the ordinary allocator too.
        let fresh = service
            .create(Proposal {
                kind: Kind::Confirmation,
                scope: scope.clone(),
                subject: Subject {
                    tool_call_id: String::new(),
                    source: "conversation".into(),
                    id: "post-migration-plan".into(),
                    version: "1".into(),
                    details: "Review the new plan.".into(),
                },
                title: "Review new plan".into(),
                reason: "scope".into(),
                persistent: false,
                blocking: true,
                expires_at: None,
            })
            .unwrap();
        service
            .decide(
                &fresh.id,
                &scope,
                fresh.revision,
                true,
                "",
                &Actor::local("test"),
            )
            .unwrap();
        let source_cursor = snapshot.domains["approvals"]
            .committed_cursor
            .parse::<i64>()
            .unwrap();
        let fresh_events = service
            .events("/w", Some("s"), source_cursor)
            .unwrap()
            .into_iter()
            .filter(|event| event.request.id == fresh.id)
            .collect::<Vec<_>>();
        assert_eq!(fresh_events.len(), 2);
        assert!(fresh_events[0].sequence > source_cursor);
        assert!(fresh_events[1].sequence > fresh_events[0].sequence);
        fence.0.store(false, Ordering::SeqCst);
        assert!(source.validate_frozen(&snapshot).await.is_err());
        assert!(source.close().await.is_err());
    }

    #[tokio::test]
    async fn invalid_chunk_rolls_back_records_and_receipt_and_small_limit_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.db");
        let service = Service::open(&path).unwrap();
        let scope = Scope {
            workspace: "/w".into(),
            session_id: "s".into(),
            task_id: String::new(),
            run_id: String::new(),
        };
        service.set_auto_mode(&scope, false).unwrap();
        drop(service);
        let codec = Arc::new(ApprovalCodec);
        let source = Source::open(
            &path,
            "source".into(),
            "frozen-1".into(),
            Arc::new(Fence(AtomicBool::new(true))),
            codec.clone(),
        )
        .unwrap();
        assert!(source.read_chunk("approvals", "", 32).await.is_err());
        let snapshot = source.descriptor().await.unwrap();
        let manifest = Manifest {
            format_version: FORMAT_VERSION,
            migration_id: "bad".into(),
            destination_identity: "target".into(),
            expected_route_generation: "route-1".into(),
            max_chunk_bytes: 64 * 1024,
            source: snapshot,
            progress: BTreeMap::from([(
                "approvals".into(),
                Progress {
                    cursor: String::new(),
                    complete: false,
                    chunks: vec![],
                },
            )]),
            destination_write_started: true,
            sealed: false,
            published_generation: None,
            rolled_back: false,
        };
        let stage_path = dir.path().join("destination.db");
        let stage = Stage::new(stage_path.clone(), "target".into(), codec);
        stage.begin(&manifest).await.unwrap();
        let mut chunk = source.read_chunk("approvals", "", 64 * 1024).await.unwrap();
        chunk.records.push(Record {
            id: "invalid".into(),
            revision: "1".into(),
            payload: b"{}".to_vec(),
        });
        let hash = chunk_hash(&chunk, 64 * 1024).unwrap();
        assert!(
            stage
                .apply_chunk("bad", "approvals", &chunk, &hash)
                .await
                .is_err()
        );
        assert!(
            stage
                .chunk_receipt("bad", "approvals", "")
                .await
                .unwrap()
                .is_none()
        );
        let connection = Connection::open(&stage_path).unwrap();
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM approval_modes", (), |r| r
                    .get::<i64>(0))
                .unwrap(),
            0
        );
        source.close().await.unwrap();
    }
}

#[cfg(test)]
pub(super) async fn roundtrip_for_test(
    path: &Path,
    destination: &Path,
    codec: Arc<dyn SqlDomainCodec>,
) {
    struct Fence;
    impl FreezeGuard for Fence {
        fn validate(&self, _: &str) -> Result<()> {
            Ok(())
        }
    }
    let source = Source::open(
        path,
        "source".into(),
        "frozen".into(),
        Arc::new(Fence),
        codec.clone(),
    )
    .unwrap();
    let snapshot = source.descriptor().await.unwrap();
    let domain = codec.domain();
    let mut manifest = Manifest {
        format_version: FORMAT_VERSION,
        migration_id: "copy".into(),
        destination_identity: "target".into(),
        expected_route_generation: "route-1".into(),
        max_chunk_bytes: 1024 * 1024,
        source: snapshot,
        progress: BTreeMap::from([(
            domain.into(),
            Progress {
                cursor: String::new(),
                complete: false,
                chunks: vec![],
            },
        )]),
        destination_write_started: true,
        sealed: false,
        published_generation: None,
        rolled_back: false,
    };
    let stage = Stage::new(destination.to_owned(), "target".into(), codec);
    stage.begin(&manifest).await.unwrap();
    loop {
        let chunk = source
            .read_chunk(
                domain,
                &manifest.progress[domain].cursor,
                manifest.max_chunk_bytes,
            )
            .await
            .unwrap();
        let hash = chunk_hash(&chunk, manifest.max_chunk_bytes).unwrap();
        let receipt = stage
            .apply_chunk("copy", domain, &chunk, &hash)
            .await
            .unwrap();
        let progress = manifest.progress.get_mut(domain).unwrap();
        progress.chunks.push(receipt);
        progress.complete = chunk.next_cursor.is_none();
        if let Some(cursor) = chunk.next_cursor {
            progress.cursor = cursor;
        } else {
            break;
        }
    }
    assert_eq!(
        stage.verify(&manifest).await.unwrap()[domain],
        manifest.source.domains[domain].digest
    );
    stage.seal(&manifest).await.unwrap();
    stage.close().await.unwrap();
    source.close().await.unwrap();
}

#[cfg(test)]
mod commit_ack_tests {
    use super::*;
    use crate::storage::{
        OpenOptions, Params, SqlCapabilities, SqlConnection, SqlDriver, SqlStatement,
    };
    use std::{
        sync::atomic::{AtomicBool, AtomicUsize, Ordering},
        time::Duration,
    };
    #[derive(Default)]
    struct Faults {
        lose_ack: AtomicBool,
        fail_open: AtomicBool,
        poisoned_calls: AtomicUsize,
        opens: AtomicUsize,
    }
    struct Driver(Arc<Faults>);
    impl SqlDriver for Driver {
        fn capabilities(&self, path: &Path) -> SqlCapabilities {
            crate::storage::drivers::turso::TursoDriver.capabilities(path)
        }
        fn open(
            &self,
            path: &Path,
            options: OpenOptions,
        ) -> crate::storage::Result<Box<dyn SqlConnection>> {
            self.0.opens.fetch_add(1, Ordering::SeqCst);
            if self.0.fail_open.load(Ordering::SeqCst) {
                return Err(crate::storage::Error::Unavailable);
            }
            Ok(Box::new(LostAck {
                inner: crate::storage::drivers::turso::TursoDriver.open(path, options)?,
                faults: self.0.clone(),
                poisoned: AtomicBool::new(false),
            }))
        }
    }
    struct LostAck {
        inner: Box<dyn SqlConnection>,
        faults: Arc<Faults>,
        poisoned: AtomicBool,
    }
    impl LostAck {
        fn check(&self) -> crate::storage::Result<()> {
            if self.poisoned.load(Ordering::SeqCst) {
                self.faults.poisoned_calls.fetch_add(1, Ordering::SeqCst);
                return Err(crate::storage::Error::Closed);
            }
            Ok(())
        }
    }
    impl SqlConnection for LostAck {
        fn capabilities(&self) -> SqlCapabilities {
            self.inner.capabilities()
        }
        fn busy_timeout(&self, value: Duration) -> crate::storage::Result<()> {
            self.check()?;
            self.inner.busy_timeout(value)
        }
        fn pragma_update(&self, name: &str, value: &str) -> crate::storage::Result<()> {
            self.check()?;
            self.inner.pragma_update(name, value)
        }
        fn execute_batch(&self, sql: &str) -> crate::storage::Result<()> {
            self.check()?;
            self.inner.execute_batch(sql)
        }
        fn execute(&self, sql: &str, params: Params) -> crate::storage::Result<usize> {
            self.check()?;
            self.inner.execute(sql, params)
        }
        fn prepare(&self, sql: &str) -> crate::storage::Result<Box<dyn SqlStatement>> {
            self.check()?;
            self.inner.prepare(sql)
        }
        fn begin(&self) -> crate::storage::Result<()> {
            self.check()?;
            self.inner.begin()
        }
        fn commit(&self) -> crate::storage::Result<()> {
            self.check()?;
            self.inner.commit()?;
            if self.faults.lose_ack.swap(false, Ordering::SeqCst) {
                self.poisoned.store(true, Ordering::SeqCst);
                return Err(crate::storage::Error::CommitUnknown);
            }
            Ok(())
        }
        fn rollback(&self) -> crate::storage::Result<()> {
            self.check()?;
            self.inner.rollback()
        }
        fn checkpoint(&self) -> crate::storage::Result<()> {
            self.check()?;
            self.inner.checkpoint()
        }
        fn close(self: Box<Self>) -> crate::storage::Result<()> {
            self.inner.close()
        }
    }
    struct Fence;
    impl FreezeGuard for Fence {
        fn validate(&self, _: &str) -> Result<()> {
            Ok(())
        }
    }
    #[tokio::test]
    async fn committed_chunk_lost_ack_uses_fresh_authoritative_receipt_or_remains_unknown() {
        for fail_reopen in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let source_path = dir.path().join("source.db");
            let target = dir.path().join("target.db");
            let service = crate::approval::Service::open(&source_path).unwrap();
            service
                .set_auto_mode(
                    &otto_core::approval::Scope {
                        workspace: "/w".into(),
                        session_id: "s".into(),
                        task_id: String::new(),
                        run_id: String::new(),
                    },
                    false,
                )
                .unwrap();
            drop(service);
            let codec = Arc::new(ApprovalCodec);
            let source = Source::open(
                &source_path,
                "source".into(),
                "frozen".into(),
                Arc::new(Fence),
                codec.clone(),
            )
            .unwrap();
            let manifest = Manifest {
                format_version: FORMAT_VERSION,
                migration_id: "copy".into(),
                destination_identity: "target".into(),
                expected_route_generation: "route".into(),
                max_chunk_bytes: 64 * 1024,
                source: source.descriptor().await.unwrap(),
                progress: BTreeMap::from([(
                    "approvals".into(),
                    Progress {
                        cursor: String::new(),
                        complete: false,
                        chunks: vec![],
                    },
                )]),
                destination_write_started: true,
                sealed: false,
                published_generation: None,
                rolled_back: false,
            };
            let faults = Arc::new(Faults::default());
            let stage = Stage::with_driver(
                target.clone(),
                "target".into(),
                codec,
                Arc::new(Driver(faults.clone())),
            );
            stage.begin(&manifest).await.unwrap();
            let chunk = source
                .read_chunk("approvals", "", manifest.max_chunk_bytes)
                .await
                .unwrap();
            let hash = chunk_hash(&chunk, manifest.max_chunk_bytes).unwrap();
            let opens = faults.opens.load(Ordering::SeqCst);
            faults.lose_ack.store(true, Ordering::SeqCst);
            faults.fail_open.store(fail_reopen, Ordering::SeqCst);
            let outcome = stage.apply_chunk("copy", "approvals", &chunk, &hash).await;
            if fail_reopen {
                assert!(matches!(outcome, Err(Error::CommitUnknown)));
                assert!(stage.chunk_receipt("copy", "approvals", "").await.is_err());
                faults.fail_open.store(false, Ordering::SeqCst);
            } else {
                assert!(outcome.is_ok());
            }
            assert!(faults.opens.load(Ordering::SeqCst) > opens);
            assert_eq!(faults.poisoned_calls.load(Ordering::SeqCst), 0);
            let receipt = stage
                .chunk_receipt("copy", "approvals", "")
                .await
                .unwrap()
                .unwrap();
            assert_eq!(receipt.sha256, hash);
            assert_eq!(
                stage
                    .apply_chunk("copy", "approvals", &chunk, &hash)
                    .await
                    .unwrap(),
                receipt
            );
            let connection = Connection::open(&target).unwrap();
            assert_eq!(
                connection
                    .query_row("SELECT COUNT(*) FROM approval_modes", (), |r| r
                        .get::<i64>(0))
                    .unwrap(),
                1
            );
            assert_eq!(
                connection
                    .query_row("SELECT COUNT(*) FROM otto_migration_chunks", (), |r| r
                        .get::<i64>(0))
                    .unwrap(),
                1
            );
            stage.close().await.unwrap();
            source.close().await.unwrap();
        }
    }
}
