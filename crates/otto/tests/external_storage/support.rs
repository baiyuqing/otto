//! An independent JSON repository used only for public extension acceptance.
//! It does not import or wrap any Otto SQL/file backend implementation.
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use async_trait::async_trait;
use otto::approval::migration::ApprovalRecord;
use otto::approval::repository::{ApprovalStore, Claim, ClaimChange};
use otto::approval::{self, Change, Request, Scope, Snapshot, State};
use otto::runtime::storage::backend::{MigrationContext, PreparedImport};
use otto::runtime::storage::{
    Endpoint, NamespaceIdentity, RepositoryFactory, RepositoryRequirements, StorageEndpoint,
};
use otto::storage::migration::{self, *};
use serde::{Deserialize, Serialize};

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StoredClaim {
    owner: String,
    pid: i64,
    started: String,
    native: bool,
}
impl From<&Claim> for StoredClaim {
    fn from(claim: &Claim) -> Self {
        Self {
            owner: claim.owner.clone(),
            pid: claim.pid,
            started: claim.started.clone(),
            native: claim.native,
        }
    }
}
impl From<StoredClaim> for Claim {
    fn from(claim: StoredClaim) -> Self {
        Self {
            owner: claim.owner,
            pid: claim.pid,
            started: claim.started,
            native: claim.native,
        }
    }
}
#[derive(Clone, Default, Serialize, Deserialize)]
struct Data {
    initialized: bool,
    sealed: bool,
    migration: Option<Manifest>,
    records: BTreeMap<String, Record>,
    receipts: BTreeMap<String, ChunkReceipt>,
    #[serde(default)]
    claims: BTreeMap<String, StoredClaim>,
}

pub struct JsonStore {
    path: PathBuf,
    data: Mutex<Data>,
}

impl JsonStore {
    fn new(path: PathBuf) -> Self {
        let data = if path.exists() {
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap()
        } else {
            Data::default()
        };
        Self {
            path,
            data: Mutex::new(data),
        }
    }
    fn save(&self, data: &Data) -> approval::Result<()> {
        let temporary = self.path.with_extension("pending");
        std::fs::write(
            &temporary,
            serde_json::to_vec(data).map_err(|_| approval::Error::Unavailable)?,
        )
        .map_err(|_| approval::Error::Unavailable)?;
        std::fs::File::open(&temporary)
            .and_then(|f| f.sync_all())
            .map_err(|_| approval::Error::Unavailable)?;
        std::fs::rename(temporary, &self.path).map_err(|_| approval::Error::Unavailable)?;
        std::fs::File::open(self.path.parent().unwrap())
            .and_then(|f| f.sync_all())
            .map_err(|_| approval::Error::CommitUnknown)
    }
    fn mutate<R>(
        &self,
        operation: impl FnOnce(&mut Data) -> approval::Result<R>,
    ) -> approval::Result<R> {
        let mut data = self.data.lock().map_err(|_| approval::Error::Unavailable)?;
        let mut next = data.clone();
        let result = operation(&mut next)?;
        self.save(&next)?;
        *data = next;
        Ok(result)
    }
    fn put(data: &mut Data, value: ApprovalRecord) -> approval::Result<()> {
        let (id, revision) = value.metadata().map_err(|_| approval::Error::Unavailable)?;
        let payload = serde_json::to_vec(&value).map_err(|_| approval::Error::Unavailable)?;
        data.records.insert(
            id.clone(),
            Record {
                id,
                revision,
                payload,
            },
        );
        Ok(())
    }
    fn values(data: &Data) -> Vec<ApprovalRecord> {
        data.records
            .values()
            .map(|r| serde_json::from_slice(&r.payload).unwrap())
            .collect()
    }
    fn requests(data: &Data) -> Vec<Request> {
        Self::values(data)
            .into_iter()
            .filter_map(|value| {
                if let ApprovalRecord::Request { request, .. } = value {
                    Some(approval::codec::decode_persisted(&request).unwrap())
                } else {
                    None
                }
            })
            .collect()
    }
    fn events_from(data: &Data) -> Vec<Change> {
        let mut events: Vec<_> = Self::values(data)
            .into_iter()
            .filter_map(|value| {
                if let ApprovalRecord::Event { sequence, request } = value {
                    Some(Change {
                        sequence,
                        request: approval::codec::decode_persisted(&request).unwrap(),
                    })
                } else {
                    None
                }
            })
            .collect();
        events.sort_by_key(|event| event.sequence);
        events
    }
    fn append_event(data: &mut Data, request: &Request) -> approval::Result<()> {
        let sequence = Self::events_from(data).last().map_or(1, |e| e.sequence + 1);
        Self::put(
            data,
            ApprovalRecord::Event {
                sequence,
                request: serde_json::to_string(request)
                    .map_err(|_| approval::Error::Unavailable)?,
            },
        )
    }
    fn claim_in(data: &Data, id: &str) -> Option<Claim> {
        data.claims.get(id).cloned().map(Claim::from).or_else(|| {
            Self::values(data)
                .into_iter()
                .find_map(|value| match value {
                    ApprovalRecord::InterruptedClaim {
                        approval_id,
                        recovery,
                    } if approval_id == id => Some(Claim {
                        owner: "interrupted".into(),
                        pid: 0,
                        started: String::new(),
                        native: recovery == "native",
                    }),
                    _ => None,
                })
        })
    }
    fn ready(&self) -> bool {
        let data = self.data.lock().unwrap();
        data.initialized || data.sealed
    }
}

impl ApprovalStore for JsonStore {
    fn mode(&self, scope: &Scope) -> approval::Result<Option<bool>> {
        Ok(Self::values(&self.data.lock().unwrap())
            .into_iter()
            .find_map(|value| match value {
                ApprovalRecord::Mode {
                    workspace,
                    session_id,
                    automatic,
                } if workspace == scope.workspace && session_id == scope.session_id => {
                    Some(automatic)
                }
                _ => None,
            }))
    }
    fn set_mode(&self, scope: &Scope, automatic: bool) -> approval::Result<()> {
        self.mutate(|data| {
            Self::put(
                data,
                ApprovalRecord::Mode {
                    workspace: scope.workspace.clone(),
                    session_id: scope.session_id.clone(),
                    automatic,
                },
            )
        })
    }
    fn create(&self, key: &str, request: &Request) -> approval::Result<(Request, bool)> {
        self.mutate(|data| {
            for value in Self::values(data) {
                if let ApprovalRecord::Request {
                    subject_key,
                    request,
                } = value
                    && subject_key == key
                {
                    return Ok((approval::codec::decode_persisted(&request)?, false));
                }
            }
            if data
                .records
                .contains_key(&format!("request:{}", request.id))
            {
                return Err(approval::Error::Conflict);
            }
            Self::put(
                data,
                ApprovalRecord::Request {
                    subject_key: key.into(),
                    request: serde_json::to_string(request).unwrap(),
                },
            )?;
            Self::put(
                data,
                ApprovalRecord::Visibility {
                    approval_id: request.id.clone(),
                    workspace: request.scope.workspace.clone(),
                    session_id: request.scope.session_id.clone(),
                },
            )?;
            Self::append_event(data, request)?;
            Ok((request.clone(), true))
        })
    }
    fn load(&self, id: &str) -> approval::Result<Request> {
        Self::requests(&self.data.lock().unwrap())
            .into_iter()
            .find(|r| r.id == id)
            .ok_or(approval::Error::NotFound)
    }
    fn visible(&self, id: &str, workspace: &str, session: Option<&str>) -> approval::Result<bool> {
        let request = self.load(id)?;
        Ok(request.scope.workspace == workspace
            && session.is_none_or(|session| request.scope.session_id == session))
    }
    fn pending(&self) -> approval::Result<Vec<Request>> {
        Ok(Self::requests(&self.data.lock().unwrap())
            .into_iter()
            .filter(|r| r.state == State::Pending)
            .collect())
    }
    fn applications(&self, workspace: &str) -> approval::Result<Vec<Request>> {
        Ok(Self::requests(&self.data.lock().unwrap())
            .into_iter()
            .filter(|r| r.scope.workspace == workspace)
            .collect())
    }
    fn snapshot(&self, workspace: &str, session: Option<&str>) -> approval::Result<Snapshot> {
        let data = self.data.lock().unwrap();
        Ok(Snapshot {
            requests: Self::requests(&data)
                .into_iter()
                .filter(|r| {
                    r.scope.workspace == workspace
                        && session.is_none_or(|session| r.scope.session_id == session)
                })
                .collect(),
            cursor: Self::events_from(&data).last().map_or(0, |r| r.sequence),
        })
    }
    fn events(
        &self,
        workspace: &str,
        session: Option<&str>,
        after: i64,
    ) -> approval::Result<Vec<Change>> {
        Ok(Self::events_from(&self.data.lock().unwrap())
            .into_iter()
            .filter(|r| {
                r.sequence > after
                    && r.request.scope.workspace == workspace
                    && session.is_none_or(|session| r.request.scope.session_id == session)
            })
            .collect())
    }
    fn claim(&self, id: &str) -> approval::Result<Option<Claim>> {
        Ok(Self::claim_in(&self.data.lock().unwrap(), id))
    }
    fn take_claim(&self, id: &str, expected: &Claim, next: &Claim) -> approval::Result<bool> {
        self.mutate(|data| {
            if Self::claim_in(data, id).as_ref() != Some(expected) {
                return Ok(false);
            }
            data.records.remove(&format!("claim:{id}"));
            data.claims.insert(id.into(), StoredClaim::from(next));
            Ok(true)
        })
    }
    fn commit(
        &self,
        expected: &Request,
        next: &Request,
        claim: ClaimChange,
        automatic: bool,
    ) -> approval::Result<Request> {
        if next.id != expected.id
            || next.scope != expected.scope
            || next.revision != expected.revision
            || next.schema_version != expected.schema_version
            || next.subject != expected.subject
        {
            return Err(approval::Error::Conflict);
        }
        self.mutate(|data| {
            let current = Self::requests(data).into_iter().find(|r| r.id == expected.id).ok_or(approval::Error::NotFound)?;
            if current != *expected { return Err(approval::Error::Conflict) }
            if automatic && Self::values(data).into_iter().any(|value| matches!(value, ApprovalRecord::Mode { workspace, session_id, automatic: false } if workspace == expected.scope.workspace && session_id == expected.scope.session_id)) { return Err(approval::Error::Conflict) }
            match claim {
                ClaimChange::None => (),
                ClaimChange::Start(claim) => {
                    if Self::claim_in(data, &expected.id).is_some() { return Err(approval::Error::Conflict) }
                    data.claims.insert(expected.id.clone(), StoredClaim::from(&claim));
                }
                ClaimChange::Finish(owner) => {
                    if Self::claim_in(data, &expected.id).is_none_or(|claim| claim.owner != owner) { return Err(approval::Error::Conflict) }
                }
            }
            let subject_key = Self::values(data).into_iter().find_map(|value| match value { ApprovalRecord::Request { subject_key, request } if approval::codec::decode_persisted(&request).unwrap().id == expected.id => Some(subject_key), _ => None }).unwrap();
            let record = &data.records[&format!("request:{}", expected.id)];
            let original = match serde_json::from_slice::<ApprovalRecord>(&record.payload).unwrap() { ApprovalRecord::Request { request, .. } => request, _ => unreachable!() };
            let body = approval::codec::merge_persisted(&original, next)?;
            Self::put(data, ApprovalRecord::Request { subject_key, request: body.clone() })?;
            let sequence = Self::events_from(data).last().map_or(1, |event| event.sequence + 1);
            Self::put(data, ApprovalRecord::Event { sequence, request: body })?;
            Ok(next.clone())
        })
    }
}

pub struct JsonBackend {
    pub store: Arc<JsonStore>,
    pub identity: String,
    pub lose_import_ack: Arc<AtomicBool>,
}
impl JsonBackend {
    pub fn new(path: PathBuf, identity: &str) -> Self {
        Self {
            store: Arc::new(JsonStore::new(path)),
            identity: identity.into(),
            lose_import_ack: Arc::new(AtomicBool::new(false)),
        }
    }
    pub async fn check_import_receipts(&self) -> migration::Result<()> {
        let data = self.store.data.lock().unwrap().clone();
        let manifest = data
            .migration
            .as_ref()
            .ok_or_else(|| Error::Invalid("test import missing".into()))?;
        let receipt = data
            .receipts
            .get(&serde_json::to_string(&("approvals", "")).unwrap())
            .ok_or_else(|| Error::Invalid("test receipt missing".into()))?;
        let records = record::canonical_records("approvals", data.records.values().cloned())?;
        let command = Chunk {
            cursor: receipt.cursor.clone(),
            next_cursor: receipt.next_cursor.clone(),
            records,
            blobs: Vec::new(),
        };
        let stage = JsonStage {
            store: self.store.clone(),
            closed: AtomicBool::new(false),
            lose_ack: Arc::new(AtomicBool::new(false)),
        };
        otto::storage::conformance::import_receipts(
            &stage,
            &manifest.migration_id,
            "approvals",
            &command,
            manifest.max_chunk_bytes,
        )
        .await
    }
    fn check(
        endpoint: &StorageEndpoint,
        requirement: &RepositoryRequirements,
        options: &BTreeMap<String, String>,
    ) -> otto::storage::Result<()> {
        if !matches!(endpoint, StorageEndpoint::Service(_))
            || requirement.domain != "approvals"
            || requirement.profile_version != 1
            || !options.is_empty()
        {
            return Err(otto::storage::Error::UnsupportedCapability(
                "independent approval profile",
            ));
        }
        Ok(())
    }
}

#[async_trait]
impl RepositoryFactory for JsonBackend {
    async fn validate(
        &self,
        endpoint: &StorageEndpoint,
        requirements: &RepositoryRequirements,
        options: &BTreeMap<String, String>,
    ) -> otto::storage::Result<NamespaceIdentity> {
        tokio::task::yield_now().await;
        Self::check(endpoint, requirements, options)?;
        if !self.store.ready() {
            return Err(otto::storage::Error::Unavailable);
        }
        Ok(NamespaceIdentity(self.identity.clone()))
    }
    async fn initialize(
        &self,
        endpoint: &StorageEndpoint,
        domains: &[RepositoryRequirements],
        options: &BTreeMap<String, String>,
    ) -> otto::storage::Result<NamespaceIdentity> {
        for requirement in domains {
            Self::check(endpoint, requirement, options)?;
        }
        self.store
            .mutate(|data| {
                if data.initialized || data.sealed || !data.records.is_empty() {
                    return Err(approval::Error::Conflict);
                }
                data.initialized = true;
                Ok(())
            })
            .map_err(|_| otto::storage::Error::Unavailable)?;
        Ok(NamespaceIdentity(self.identity.clone()))
    }
    fn approvals(&self, endpoint: Endpoint<'_>) -> approval::Result<Arc<dyn ApprovalStore>> {
        if !self.store.ready() || endpoint.address.is_empty() {
            return Err(approval::Error::Unavailable);
        }
        Ok(self.store.clone())
    }
    async fn prepare_import(
        &self,
        endpoint: &StorageEndpoint,
        requirement: &RepositoryRequirements,
        options: &BTreeMap<String, String>,
        context: &MigrationContext,
    ) -> migration::Result<PreparedImport> {
        Self::check(endpoint, requirement, options)
            .map_err(|_| Error::Invalid("unsupported import profile".into()))?;
        {
            let data = self.store.data.lock().unwrap();
            if data.initialized
                || data
                    .migration
                    .as_ref()
                    .is_some_and(|m| m.migration_id != context.migration_id)
                || data.migration.is_none() && !data.records.is_empty()
            {
                return Err(Error::Conflict("destination is occupied".into()));
            }
        }
        Ok(PreparedImport {
            namespace_identity: NamespaceIdentity(self.identity.clone()),
            stage: Arc::new(JsonStage {
                store: self.store.clone(),
                closed: AtomicBool::new(false),
                lose_ack: self.lose_import_ack.clone(),
            }),
        })
    }
    async fn export(
        &self,
        endpoint: &StorageEndpoint,
        requirement: &RepositoryRequirements,
        options: &BTreeMap<String, String>,
        context: &MigrationContext,
    ) -> migration::Result<Arc<dyn SnapshotExport>> {
        Self::check(endpoint, requirement, options)
            .map_err(|_| Error::Invalid("unsupported export profile".into()))?;
        context.fence.validate(&context.generation)?;
        let mut data = self.store.data.lock().unwrap().clone();
        for (id, claim) in data.claims.clone() {
            JsonStore::put(
                &mut data,
                ApprovalRecord::InterruptedClaim {
                    approval_id: id,
                    recovery: if claim.native { "native" } else { "domain" }.into(),
                },
            )
            .map_err(failure)?;
        }
        let records = record::canonical_records("approvals", data.records.values().cloned())?;
        let cursor = JsonStore::events_from(&self.store.data.lock().unwrap())
            .last()
            .map_or(0, |e| e.sequence)
            .to_string();
        let snapshot = FrozenSnapshot {
            source_identity: context.source_identity.clone(),
            generation: context.generation.clone(),
            domains: BTreeMap::from([(
                "approvals".into(),
                DomainSnapshot {
                    schema_family: "otto.approvals".into(),
                    schema_version: 1,
                    committed_cursor: cursor,
                    digest: record::digest("approvals", records.clone())?,
                },
            )]),
        };
        Ok(Arc::new(JsonSource {
            snapshot,
            records,
            fence: context.fence.clone(),
            closed: AtomicBool::new(false),
        }))
    }
}

fn chunk(records: &[Record], cursor: &str, max_bytes: usize) -> migration::Result<Chunk> {
    let index: usize = if cursor.is_empty() {
        0
    } else {
        cursor
            .parse()
            .map_err(|_| Error::Invalid("invalid export cursor".into()))?
    };
    if index > records.len() {
        return Err(Error::Invalid("export cursor outside snapshot".into()));
    }
    let value = Chunk {
        cursor: cursor.into(),
        next_cursor: (index < records.len()).then(|| (index + 1).to_string()),
        records: records.get(index).cloned().into_iter().collect(),
        blobs: Vec::new(),
    };
    chunk_hash(&value, max_bytes)?;
    Ok(value)
}

struct JsonSource {
    snapshot: FrozenSnapshot,
    records: Vec<Record>,
    fence: Arc<dyn FreezeGuard>,
    closed: AtomicBool,
}
#[async_trait]
impl SnapshotExport for JsonSource {
    async fn descriptor(&self) -> migration::Result<FrozenSnapshot> {
        self.validate_frozen(&self.snapshot).await?;
        Ok(self.snapshot.clone())
    }
    async fn validate_frozen(&self, expected: &FrozenSnapshot) -> migration::Result<()> {
        self.fence.validate(&self.snapshot.generation)?;
        if self.closed.load(Ordering::SeqCst) || expected != &self.snapshot {
            return Err(Error::Conflict("snapshot closed or changed".into()));
        }
        Ok(())
    }
    async fn read_chunk(
        &self,
        domain: &str,
        cursor: &str,
        max_bytes: usize,
    ) -> migration::Result<Chunk> {
        self.validate_frozen(&self.snapshot).await?;
        if domain != "approvals" {
            return Err(Error::Invalid("unsupported export domain".into()));
        }
        chunk(&self.records, cursor, max_bytes)
    }
    async fn close(&self) -> migration::Result<()> {
        self.closed.store(true, Ordering::SeqCst);
        Ok(())
    }
}

struct JsonStage {
    store: Arc<JsonStore>,
    closed: AtomicBool,
    lose_ack: Arc<AtomicBool>,
}
fn failure(_: approval::Error) -> Error {
    Error::Unavailable("independent repository unavailable".into())
}
#[async_trait]
impl StageImport for JsonStage {
    async fn validate_plan(&self, manifest: &Manifest) -> migration::Result<()> {
        if self.closed.load(Ordering::SeqCst)
            || manifest.source.domains.len() != 1
            || manifest
                .source
                .domains
                .get("approvals")
                .is_none_or(|s| s.schema_family != "otto.approvals" || s.schema_version != 1)
        {
            return Err(Error::Invalid("invalid approval import profile".into()));
        }
        Ok(())
    }
    async fn begin(&self, manifest: &Manifest) -> migration::Result<()> {
        self.validate_plan(manifest).await?;
        self.store
            .mutate(|data| {
                if let Some(previous) = &data.migration {
                    if previous.migration_id != manifest.migration_id
                        || previous.source != manifest.source
                    {
                        return Err(approval::Error::Conflict);
                    }
                } else {
                    data.migration = Some(manifest.clone());
                }
                Ok(())
            })
            .map_err(failure)
    }
    async fn apply_chunk(
        &self,
        id: &str,
        domain: &str,
        chunk: &Chunk,
        hash: &str,
    ) -> migration::Result<ChunkReceipt> {
        let receipt = ChunkReceipt {
            cursor: chunk.cursor.clone(),
            next_cursor: chunk.next_cursor.clone(),
            sha256: hash.into(),
        };
        let key = serde_json::to_string(&(domain, &chunk.cursor)).unwrap();
        let data = self.store.data.lock().unwrap();
        let limit = data
            .migration
            .as_ref()
            .filter(|m| m.migration_id == id)
            .ok_or_else(|| Error::Conflict("migration differs".into()))?
            .max_chunk_bytes;
        if self.closed.load(Ordering::SeqCst)
            || domain != "approvals"
            || !chunk.blobs.is_empty()
            || chunk_hash(chunk, limit)? != hash
        {
            return Err(Error::Invalid("invalid import command".into()));
        }
        drop(data);
        let result = self
            .store
            .mutate(|data| {
                if let Some(prior) = data.receipts.get(&key) {
                    return if prior == &receipt {
                        Ok(prior.clone())
                    } else {
                        Err(approval::Error::Conflict)
                    };
                }
                if data.sealed {
                    return Err(approval::Error::Conflict);
                }
                for record in &chunk.records {
                    record::order(domain, record)
                        .map_err(|_| approval::Error::Invalid("invalid approval record"))?;
                    if data
                        .records
                        .insert(record.id.clone(), record.clone())
                        .is_some()
                    {
                        return Err(approval::Error::Conflict);
                    }
                }
                data.receipts.insert(key, receipt.clone());
                Ok(receipt)
            })
            .map_err(|error| match error {
                approval::Error::Conflict => Error::Conflict("import command differs".into()),
                other => failure(other),
            })?;
        if self.lose_ack.swap(false, Ordering::SeqCst) {
            return Err(Error::CommitUnknown);
        }
        Ok(result)
    }
    async fn chunk_receipt(
        &self,
        id: &str,
        domain: &str,
        cursor: &str,
    ) -> migration::Result<Option<ChunkReceipt>> {
        let data = self.store.data.lock().unwrap();
        if data
            .migration
            .as_ref()
            .is_some_and(|m| m.migration_id != id)
        {
            return Err(Error::Conflict("migration differs".into()));
        }
        Ok(data
            .receipts
            .get(&serde_json::to_string(&(domain, cursor)).unwrap())
            .cloned())
    }
    async fn verification_chunk(
        &self,
        domain: &str,
        cursor: &str,
        max_bytes: usize,
    ) -> migration::Result<Chunk> {
        if domain != "approvals" {
            return Err(Error::Invalid("unsupported verification domain".into()));
        }
        let records = record::canonical_records(
            domain,
            self.store.data.lock().unwrap().records.values().cloned(),
        )?;
        chunk(&records, cursor, max_bytes)
    }
    async fn verify(
        &self,
        manifest: &Manifest,
    ) -> migration::Result<BTreeMap<String, DomainDigest>> {
        self.validate_plan(manifest).await?;
        let data = self.store.data.lock().unwrap();
        for receipt in &manifest.progress["approvals"].chunks {
            if data
                .receipts
                .get(&serde_json::to_string(&("approvals", &receipt.cursor)).unwrap())
                != Some(receipt)
            {
                return Err(Error::Invalid("import receipt missing".into()));
            }
        }
        Ok(BTreeMap::from([(
            "approvals".into(),
            record::digest("approvals", data.records.values().cloned())?,
        )]))
    }
    async fn seal(&self, manifest: &Manifest) -> migration::Result<()> {
        if self.verify(manifest).await?["approvals"] != manifest.source.domains["approvals"].digest
        {
            return Err(Error::Invalid("import digest differs".into()));
        }
        self.store
            .mutate(|data| {
                data.sealed = true;
                Ok(())
            })
            .map_err(failure)
    }
    async fn close(&self) -> migration::Result<()> {
        self.closed.store(true, Ordering::SeqCst);
        Ok(())
    }
}
