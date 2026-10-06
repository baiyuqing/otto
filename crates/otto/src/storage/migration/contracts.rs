//! Logical migration ports. Payload codecs and invariant checks belong to domains.
use std::collections::BTreeMap;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub const FORMAT_VERSION: u32 = 1;
pub type Result<T> = std::result::Result<T, Error>;

/// Routing metadata only. Credentials are supplied separately from environment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageRoute {
    pub backend: String,
    pub endpoint: String,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("migration conflict: {0}")]
    Conflict(String),
    #[error("invalid migration data: {0}")]
    Invalid(String),
    #[error("migration persistence unavailable: {0}")]
    Unavailable(String),
    #[error("migration commit outcome unknown")]
    CommitUnknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DomainDigest {
    pub records: u64,
    pub blob_bytes: u64,
    /// Domain-defined canonical digest, including references and receipts.
    pub sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DomainSnapshot {
    /// Domain-owned codec identity; independent of physical engine schema.
    pub schema_family: String,
    pub schema_version: u32,
    pub committed_cursor: String,
    pub digest: DomainDigest,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrozenSnapshot {
    pub source_identity: String,
    /// Changes whenever source state or its authoritative freeze changes.
    pub generation: String,
    pub domains: BTreeMap<String, DomainSnapshot>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub id: String,
    pub revision: String,
    /// Versioned domain record, not a physical row/table representation.
    pub payload: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Blob {
    pub id: String,
    pub offset: u64,
    pub total_len: u64,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chunk {
    pub cursor: String,
    pub next_cursor: Option<String>,
    pub records: Vec<Record>,
    pub blobs: Vec<Blob>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkReceipt {
    pub cursor: String,
    pub sha256: String,
    pub next_cursor: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    pub cursor: String,
    pub complete: bool,
    pub chunks: Vec<ChunkReceipt>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub format_version: u32,
    /// Pins deterministic chunk boundaries across retries and CLI default changes.
    pub max_chunk_bytes: usize,
    pub migration_id: String,
    pub destination_identity: String,
    pub expected_route_generation: String,
    pub source: FrozenSnapshot,
    pub progress: BTreeMap<String, Progress>,
    /// Set durably before submitting the first isolated staging mutation.
    /// Never rewound, including after abort. Live activation is established
    /// separately by the authoritative cutover receipt.
    pub destination_write_started: bool,
    pub sealed: bool,
    pub published_generation: Option<String>,
    pub rolled_back: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalEntry {
    pub revision: u64,
    pub manifest: Manifest,
}

/// Durable source exclusion belongs to the lifecycle/route owner. Concrete
/// exporters also hold their engine/file snapshot locks while reading.
pub trait FreezeGuard: Send + Sync {
    fn validate(&self, generation: &str) -> Result<()>;
}

/// Owned frozen source: admissions are stopped and all submitted writes resolved.
/// Legacy writers must be stopped externally if they cannot be fenced.
/// Drop must retain fencing; only explicit rollback may release ownership.
#[async_trait]
pub trait SnapshotExport: Send + Sync {
    async fn descriptor(&self) -> Result<FrozenSnapshot>;
    async fn validate_frozen(&self, expected: &FrozenSnapshot) -> Result<()>;
    /// Return deterministic chunks at the same frozen revision. Bound encoded
    /// bytes before allocating, including record IDs, metadata and blob bytes.
    async fn read_chunk(&self, domain: &str, cursor: &str, max_bytes: usize) -> Result<Chunk>;
    /// Drain owned I/O and reject further reads; never release source fencing.
    async fn close(&self) -> Result<()>;
}

/// Owns an isolated namespace; ordinary readers cannot see it before cutover.
#[async_trait]
pub trait StageImport: Send + Sync {
    /// Read-only compatibility/quota/profile check before copying or writes.
    async fn validate_plan(&self, manifest: &Manifest) -> Result<()>;
    /// Idempotently create/validate staging identity and supported domain codecs.
    async fn begin(&self, manifest: &Manifest) -> Result<()>;
    /// Chunk receipt and all records/blob segments commit as one atomic unit.
    /// The stable key is (migration_id, domain, cursor). A different hash conflicts.
    async fn apply_chunk(
        &self,
        migration_id: &str,
        domain: &str,
        chunk: &Chunk,
        sha256: &str,
    ) -> Result<ChunkReceipt>;
    /// Authoritative read: None certifies this chunk was not committed.
    async fn chunk_receipt(
        &self,
        migration_id: &str,
        domain: &str,
        cursor: &str,
    ) -> Result<Option<ChunkReceipt>>;
    /// Validate reference closure across ALL manifest domains, ordering, receipts,
    /// replay/operation ledgers and rebuilt query semantics, then return digests.
    async fn verify(&self, manifest: &Manifest) -> Result<BTreeMap<String, DomainDigest>>;
    /// Idempotently seal and reopen through ordinary domain ports. No new import
    /// mutations are accepted after sealing; no live ownership claims are copied.
    async fn seal(&self, manifest: &Manifest) -> Result<()>;
    async fn close(&self) -> Result<()>;
}

#[async_trait]
pub trait MigrationJournal: Send + Sync {
    async fn load(&self, migration_id: &str) -> Result<Option<JournalEntry>>;
    /// Durable CAS. None means exclusive create; return the new revision.
    /// Initial creation must honor freeze/abort authority. Abort-intent saves
    /// serialize with publication and refuse when activation already committed.
    async fn save(&self, expected_revision: Option<u64>, manifest: &Manifest) -> Result<u64>;
}

/// Enforceable route/writer ownership authority, not a config-file rewrite.
/// Publication atomically switches the entire cutover group, retaining source
/// fencing and issuing fresh destination epochs. No tool execution is involved.
#[async_trait]
pub trait Cutover: Send + Sync {
    /// Check route generation, whole-group ownership and enforceable fencing.
    async fn validate_plan(&self, manifest: &Manifest) -> Result<()>;
    async fn publish(&self, manifest: &Manifest) -> Result<String>;
    /// Authoritative command receipt; None certifies publication did not commit.
    async fn receipt(&self, migration_id: &str) -> Result<Option<String>>;
    /// Restore source admission under exclusive ownership, idempotently.
    /// Atomically refuse after publication/live destination activation. Isolated
    /// staging writes do not change source authority and may be explicitly aborted.
    async fn rollback(&self, manifest: &Manifest) -> Result<()>;
}
