//! An independent JSON repository used only for public extension acceptance.
//! It does not import or wrap any Otto SQL/file backend implementation.
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use async_trait::async_trait;
use otto::approval;
use otto::approval::migration::ApprovalRecord;
use otto::approval::repository::{ApprovalStore, Claim, ClaimChange, ReviewLease};
use otto::runtime::storage::backend::{MigrationContext, PreparedImport};
use otto::runtime::storage::{
    Endpoint, NamespaceIdentity, RepositoryFactory, RepositoryRequirements, StorageEndpoint,
};
use otto::storage::migration::{self, *};
use otto_core::approval::{
    ApplicationState, ApprovalMode, ApprovalPolicy, ApprovalReviewer, ApprovalSettings, Change,
    Request, Scope, Snapshot, State,
};
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
    #[serde(default)]
    review_leases: BTreeMap<String, (String, i64, String, i64)>,
}

pub struct JsonStore {
    path: PathBuf,
    data: Mutex<Data>,
}

#[path = "support/approval_store.rs"]
mod approval_store;
#[path = "support/json_store.rs"]
mod json_store;
#[path = "support/snapshot_import.rs"]
mod snapshot_import;
use snapshot_import::{JsonSource, JsonStage, failure};

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
