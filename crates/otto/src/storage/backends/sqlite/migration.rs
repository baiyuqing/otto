//! Native isolated SQL staging. Domain codecs export logical records, never SQL dumps.
pub mod legacy;
pub mod memory_advice;
mod named;
pub mod schedule;
pub mod skill_checks;
pub mod tasks;
pub mod usage;
pub mod workflow;
use crate::hex::to_hex;
use crate::storage::migration::{chunk_hash, contracts::*};
use crate::storage::{Connection, OptionalExtension, params};
use async_trait::async_trait;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

mod approval_codec;
pub use approval_codec::*;

/// Normalizes one statement: trim, drop one trailing semicolon, remove
/// whitespace outside single-quoted values.
fn normalize_schema_sql(statement: &str) -> String {
    // Schemas written by the earlier Turso engine serialize parsed SQL: keyword
    // identifiers gain quotes, <> becomes !=, and punctuation spacing changes.
    // Compare schemas ignoring that; preserve every quoted value.
    statement
        .trim()
        .trim_end_matches(';')
        .split('\'')
        .enumerate()
        .map(|(index, part)| {
            if index % 2 == 1 {
                return part.to_string();
            }
            part.replace("\"key\"", "key")
                .replace("\"action\"", "action")
                .replace("<>", "!=")
                .chars()
                .filter(|character| !character.is_whitespace())
                .collect()
        })
        .collect::<Vec<_>>()
        .join("'")
}

const SCAN_LIMIT: usize = 64 * 1024 * 1024;

pub use crate::storage::migration::contracts::FreezeGuard;

pub trait SqlDomainCodec: Send + Sync {
    fn domain(&self) -> &'static str;
    fn family(&self) -> &'static str;
    fn version(&self) -> u32;
    /// Older portable descriptors are admitted only by their owning domain.
    fn supports_version(&self, version: u32) -> bool {
        version == self.version()
    }
    /// Reconstruct the source representation for end-to-end digest verification.
    /// Physical staging always uses the current schema.
    fn verification_record(&self, record: Record, version: u32) -> Result<Record> {
        if version != self.version() {
            return Err(Error::Invalid(
                "unsupported verification codec version".into(),
            ));
        }
        Ok(record)
    }
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
            &crate::storage::drivers::sqlite::SqliteDriver,
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
            Arc::new(crate::storage::drivers::sqlite::SqliteDriver),
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
            || !self.codec.supports_version(domain.schema_version)
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
const STAGE_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS otto_migration_stage (singleton INTEGER \
    PRIMARY KEY CHECK(singleton=1), migration_id TEXT NOT NULL, identity \
    TEXT NOT NULL, source TEXT NOT NULL, max_chunk_bytes INTEGER NOT NULL \
    CHECK(max_chunk_bytes>0), sealed INTEGER NOT NULL CHECK(sealed IN \
    (0,1))) STRICT;
CREATE TABLE IF NOT EXISTS otto_migration_chunks (migration_id TEXT NOT NULL, domain TEXT NOT NULL, cursor TEXT NOT NULL, body TEXT NOT NULL, PRIMARY KEY(migration_id,domain,cursor)) STRICT;";

#[async_trait]
impl StageImport for Stage {
    async fn verification_chunk(
        &self,
        domain: &str,
        cursor: &str,
        max_bytes: usize,
    ) -> Result<Chunk> {
        if domain != self.codec.domain() || max_bytes == 0 || max_bytes > SCAN_LIMIT {
            return Err(Error::Invalid(
                "invalid logical verification request".into(),
            ));
        }
        let mut connection = self.connection.lock().map_err(lock_error)?;
        self.ensure_open(&mut connection)?;
        let mut records = Vec::new();
        let mut after = cursor.to_owned();
        while let Some((next, record)) =
            self.codec
                .next_record(connection.as_ref().unwrap(), &after, max_bytes)?
        {
            records.push(record);
            let chunk = Chunk {
                cursor: cursor.into(),
                next_cursor: Some(next.clone()),
                records: records.clone(),
                blobs: Vec::new(),
            };
            if chunk_hash(&chunk, max_bytes).is_err() {
                records.pop();
                if records.is_empty() {
                    return Err(Error::Invalid("verification record exceeds bound".into()));
                }
                return Ok(Chunk {
                    cursor: cursor.into(),
                    next_cursor: Some(after),
                    records,
                    blobs: Vec::new(),
                });
            }
            after = next;
        }
        Ok(Chunk {
            cursor: cursor.into(),
            next_cursor: None,
            records,
            blobs: Vec::new(),
        })
    }
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
            let matches: i64 = connection
                .query_row(
                    "SELECT COUNT(*) FROM otto_migration_stage WHERE migration_id=?1 AND \
                identity=?2 AND source=?3 AND max_chunk_bytes=?4",
                    params![
                        manifest.migration_id,
                        self.identity,
                        source,
                        manifest.max_chunk_bytes
                    ],
                    |r| r.get(0),
                )
                .map_err(sql_error)?;
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
            let matches: i64 = tx
                .query_row(
                    "SELECT COUNT(*) FROM otto_migration_stage WHERE migration_id=?1 AND \
                identity=?2 AND source=?3 AND max_chunk_bytes=?4",
                    params![
                        manifest.migration_id,
                        self.identity,
                        source,
                        manifest.max_chunk_bytes
                    ],
                    |r| r.get(0),
                )
                .map_err(sql_error)?;
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
        let (digest, _) = scan_version(
            connection,
            self.codec.as_ref(),
            manifest.source.domains[self.codec.domain()].schema_version,
        )?;
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
    connection
        .query_row(
            "SELECT body FROM otto_migration_chunks WHERE migration_id=?1 AND \
        domain=?2 AND cursor=?3",
            params![id, domain, cursor],
            |r| r.get::<String>(0),
        )
        .optional()
        .map_err(sql_error)?
        .map(|body| serde_json::from_str(&body).map_err(json_error))
        .transpose()
}
fn scan(connection: &Connection, codec: &dyn SqlDomainCodec) -> Result<(DomainDigest, String)> {
    scan_version(connection, codec, codec.version())
}
fn scan_version(
    connection: &Connection,
    codec: &dyn SqlDomainCodec,
    version: u32,
) -> Result<(DomainDigest, String)> {
    let mut hash = Sha256::new();
    let mut cursor = String::new();
    let mut records = 0;
    while let Some((next, record)) = codec.next_record(connection, &cursor, SCAN_LIMIT)? {
        if next == cursor {
            return Err(Error::Invalid("non-progressing codec cursor".into()));
        }
        let record = codec.verification_record(record, version)?;
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
            sha256: to_hex(&hash.finalize()),
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

#[cfg(test)]
mod tests;

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
mod commit_ack_tests;
