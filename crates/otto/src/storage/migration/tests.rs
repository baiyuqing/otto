//! Fault injection checks sequencing only, not native durability/isolation.
use std::{collections::BTreeMap, sync::Mutex};

use async_trait::async_trait;

use super::*;

#[derive(Default)]
struct State {
    journal: Option<JournalEntry>,
    receipts: BTreeMap<String, ChunkReceipt>,
    generation: String,
    published: Option<String>,
    writes: usize,
    publications: usize,
    read_limits: Vec<usize>,
    rollbacks: usize,
    fail_checkpoint: bool,
    lose_chunk_ack: bool,
    lose_publish_ack: bool,
    corrupt_verify: bool,
    fail_publish_before_commit: bool,
    fail_published_checkpoint: bool,
    unknown_publication_receipt: bool,
    lose_journal_ack: bool,
    read_outage_after_unknown: bool,
    reconcile_outage: bool,
}

struct Ports(Mutex<State>);

impl Ports {
    fn new() -> Self {
        Self(Mutex::new(State {
            generation: "frozen-1".into(),
            ..State::default()
        }))
    }
    fn coordinator(&self) -> Coordinator<'_> {
        Coordinator {
            source: self,
            destination: self,
            journal: self,
            cutover: self,
            max_chunk_bytes: 1024,
        }
    }
    fn digest() -> DomainDigest {
        DomainDigest {
            records: 2,
            blob_bytes: 0,
            sha256: "canonical-domain-digest".into(),
        }
    }
    async fn plan(&self) {
        self.coordinator()
            .plan("move-1".into(), "destination".into(), "route-1".into())
            .await
            .unwrap();
    }
}

#[async_trait]
impl SnapshotExport for Ports {
    async fn descriptor(&self) -> Result<FrozenSnapshot> {
        Ok(FrozenSnapshot {
            source_identity: "source".into(),
            generation: self.0.lock().unwrap().generation.clone(),
            domains: BTreeMap::from([(
                "history".into(),
                DomainSnapshot {
                    schema_family: "history".into(),
                    schema_version: 1,
                    committed_cursor: "2".into(),
                    digest: Self::digest(),
                },
            )]),
        })
    }
    async fn validate_frozen(&self, expected: &FrozenSnapshot) -> Result<()> {
        if self.descriptor().await? != *expected {
            return Err(Error::Conflict("freeze changed".into()));
        }
        Ok(())
    }
    async fn read_chunk(&self, _: &str, cursor: &str, max_bytes: usize) -> Result<Chunk> {
        self.0.lock().unwrap().read_limits.push(max_bytes);
        Ok(Chunk {
            cursor: cursor.into(),
            next_cursor: if cursor.is_empty() {
                Some("1".into())
            } else {
                None
            },
            records: vec![Record {
                id: format!("record-{cursor}"),
                revision: "1".into(),
                payload: vec![1],
            }],
            blobs: vec![],
        })
    }
    async fn close(&self) -> Result<()> {
        Ok(())
    }
}

#[async_trait]
impl MigrationJournal for Ports {
    async fn load(&self, _: &str) -> Result<Option<JournalEntry>> {
        let state = self.0.lock().unwrap();
        if state.reconcile_outage {
            return Err(Error::Unavailable("receipt read outage".into()));
        }
        Ok(state.journal.clone())
    }
    async fn save(&self, expected: Option<u64>, manifest: &Manifest) -> Result<u64> {
        let mut state = self.0.lock().unwrap();
        if manifest.rolled_back && state.published.is_some() {
            return Err(Error::Conflict("publication already committed".into()));
        }
        if state.journal.as_ref().map(|entry| entry.revision) != expected {
            return Err(Error::Conflict("manifest CAS".into()));
        }
        if state.fail_checkpoint && !manifest.progress["history"].chunks.is_empty() {
            state.fail_checkpoint = false;
            return Err(Error::Unavailable(
                "checkpoint failure after chunk commit".into(),
            ));
        }
        if state.fail_published_checkpoint && manifest.published_generation.is_some() {
            state.fail_published_checkpoint = false;
            return Err(Error::Unavailable(
                "checkpoint failure after publication".into(),
            ));
        }
        let revision = expected.unwrap_or(0) + 1;
        state.journal = Some(JournalEntry {
            revision,
            manifest: manifest.clone(),
        });
        if state.lose_journal_ack {
            state.lose_journal_ack = false;
            state.reconcile_outage = state.read_outage_after_unknown;
            return Err(Error::CommitUnknown);
        }
        Ok(revision)
    }
}

#[async_trait]
impl StageImport for Ports {
    async fn validate_plan(&self, _: &Manifest) -> Result<()> {
        Ok(())
    }
    async fn begin(&self, _: &Manifest) -> Result<()> {
        Ok(())
    }
    async fn apply_chunk(
        &self,
        _: &str,
        _: &str,
        chunk: &Chunk,
        hash: &str,
    ) -> Result<ChunkReceipt> {
        let mut state = self.0.lock().unwrap();
        let receipt = ChunkReceipt {
            cursor: chunk.cursor.clone(),
            sha256: hash.into(),
            next_cursor: chunk.next_cursor.clone(),
        };
        if let Some(previous) = state.receipts.get(&chunk.cursor) {
            return if previous == &receipt {
                Ok(previous.clone())
            } else {
                Err(Error::Conflict("hash changed".into()))
            };
        }
        state.writes += 1;
        state.receipts.insert(chunk.cursor.clone(), receipt.clone());
        if state.lose_chunk_ack {
            state.lose_chunk_ack = false;
            state.reconcile_outage = state.read_outage_after_unknown;
            return Err(Error::CommitUnknown);
        }
        Ok(receipt)
    }
    async fn chunk_receipt(&self, _: &str, _: &str, cursor: &str) -> Result<Option<ChunkReceipt>> {
        let state = self.0.lock().unwrap();
        if state.reconcile_outage {
            return Err(Error::Unavailable("receipt read outage".into()));
        }
        Ok(state.receipts.get(cursor).cloned())
    }
    async fn verify(&self, _: &Manifest) -> Result<BTreeMap<String, DomainDigest>> {
        let mut digest = Self::digest();
        if self.0.lock().unwrap().corrupt_verify {
            digest.records = 99;
        }
        Ok(BTreeMap::from([("history".into(), digest)]))
    }
    async fn seal(&self, _: &Manifest) -> Result<()> {
        Ok(())
    }
    async fn close(&self) -> Result<()> {
        Ok(())
    }
}

#[async_trait]
impl Cutover for Ports {
    async fn validate_plan(&self, _: &Manifest) -> Result<()> {
        Ok(())
    }
    async fn publish(&self, manifest: &Manifest) -> Result<String> {
        let mut state = self.0.lock().unwrap();
        assert!(manifest.sealed && manifest.progress.values().all(|progress| progress.complete));
        if state.fail_publish_before_commit {
            state.fail_publish_before_commit = false;
            return Err(Error::Unavailable("publication not submitted".into()));
        }
        if let Some(generation) = &state.published {
            return Ok(generation.clone());
        }
        state.publications += 1;
        state.published = Some("route-2".into());
        if state.lose_publish_ack {
            state.lose_publish_ack = false;
            state.reconcile_outage = state.read_outage_after_unknown;
            return Err(Error::CommitUnknown);
        }
        Ok("route-2".into())
    }
    async fn receipt(&self, _: &str) -> Result<Option<String>> {
        let state = self.0.lock().unwrap();
        if state.reconcile_outage {
            return Err(Error::Unavailable("receipt read outage".into()));
        }
        if state.unknown_publication_receipt {
            return Err(Error::CommitUnknown);
        }
        Ok(state.published.clone())
    }
    async fn rollback(&self, _: &Manifest) -> Result<()> {
        let mut state = self.0.lock().unwrap();
        if state.published.is_some() {
            return Err(Error::Conflict("destination active".into()));
        }
        state.rollbacks += 1;
        Ok(())
    }
}

#[tokio::test]
async fn reconciliation_read_outage_preserves_unknown_for_every_durable_boundary() {
    for boundary in 0..3 {
        let ports = Ports::new();
        ports.plan().await;
        {
            let mut state = ports.0.lock().unwrap();
            state.read_outage_after_unknown = true;
            match boundary {
                0 => state.lose_journal_ack = true,
                1 => state.lose_chunk_ack = true,
                _ => state.lose_publish_ack = true,
            }
        }
        assert!(matches!(
            ports.coordinator().run("move-1").await,
            Err(Error::CommitUnknown)
        ));
        ports.0.lock().unwrap().reconcile_outage = false;
        ports.coordinator().run("move-1").await.unwrap();
        let state = ports.0.lock().unwrap();
        assert_eq!(state.writes, 2);
        assert_eq!(state.publications, 1);
    }
}

#[tokio::test]
async fn sealed_resume_rechecks_data_but_published_resume_reconciles_receipt() {
    let ports = Ports::new();
    ports.plan().await;
    ports.0.lock().unwrap().fail_publish_before_commit = true;
    assert!(ports.coordinator().run("move-1").await.is_err());
    assert!(
        ports
            .0
            .lock()
            .unwrap()
            .journal
            .as_ref()
            .unwrap()
            .manifest
            .sealed
    );
    ports.0.lock().unwrap().corrupt_verify = true;
    assert!(matches!(
        ports.coordinator().run("move-1").await,
        Err(Error::Invalid(_))
    ));
    assert_eq!(ports.0.lock().unwrap().publications, 0);

    let ports = Ports::new();
    ports.plan().await;
    ports.0.lock().unwrap().fail_published_checkpoint = true;
    assert!(ports.coordinator().run("move-1").await.is_err());
    ports.0.lock().unwrap().corrupt_verify = true; // Destination owners changed state after cutover.
    let recovered = ports.coordinator().run("move-1").await.unwrap();
    assert_eq!(recovered.published_generation.as_deref(), Some("route-2"));
    assert_eq!(ports.0.lock().unwrap().publications, 1);
}

#[tokio::test]
async fn resume_after_chunk_commit_before_checkpoint_does_not_reapply() {
    let ports = Ports::new();
    ports.plan().await;
    ports.0.lock().unwrap().fail_checkpoint = true;
    assert!(matches!(
        ports.coordinator().run("move-1").await,
        Err(Error::Unavailable(_))
    ));
    assert_eq!(ports.0.lock().unwrap().writes, 1);
    let mut resumed = ports.coordinator();
    resumed.max_chunk_bytes = 4096;
    let manifest = resumed.run("move-1").await.unwrap();
    assert_eq!(manifest.progress["history"].chunks.len(), 2);
    assert_eq!(ports.0.lock().unwrap().writes, 2);
    assert!(
        ports
            .0
            .lock()
            .unwrap()
            .read_limits
            .iter()
            .all(|limit| *limit == 1024)
    );
    ports.coordinator().run("move-1").await.unwrap();
    assert_eq!(ports.0.lock().unwrap().publications, 1);
}

#[tokio::test]
async fn lost_chunk_and_cutover_ack_are_reconciled_by_receipt() {
    let ports = Ports::new();
    ports.plan().await;
    {
        let mut state = ports.0.lock().unwrap();
        state.lose_chunk_ack = true;
        state.lose_publish_ack = true;
    }
    let manifest = ports.coordinator().run("move-1").await.unwrap();
    assert_eq!(manifest.published_generation.as_deref(), Some("route-2"));
    let state = ports.0.lock().unwrap();
    assert_eq!(state.writes, 2);
    assert_eq!(state.publications, 1);
}

#[tokio::test]
async fn wrong_source_and_bad_verification_never_publish() {
    let ports = Ports::new();
    ports.plan().await;
    ports.0.lock().unwrap().generation = "frozen-2".into();
    assert!(matches!(
        ports.coordinator().run("move-1").await,
        Err(Error::Conflict(_))
    ));
    assert_eq!(ports.0.lock().unwrap().writes, 0);
    {
        let mut state = ports.0.lock().unwrap();
        state.generation = "frozen-1".into();
        state.corrupt_verify = true;
    }
    assert!(matches!(
        ports.coordinator().run("move-1").await,
        Err(Error::Invalid(_))
    ));
    assert_eq!(ports.0.lock().unwrap().publications, 0);
}

#[tokio::test]
async fn rollback_allows_isolated_stage_abort_but_not_publication_or_unknown_outcome() {
    let ports = Ports::new();
    ports.plan().await;
    ports.coordinator().rollback("move-1").await.unwrap();
    assert!(matches!(
        ports.coordinator().run("move-1").await,
        Err(Error::Conflict(_))
    ));
    assert_eq!(ports.0.lock().unwrap().rollbacks, 1);

    let ports = Ports::new();
    ports.plan().await;
    ports.0.lock().unwrap().fail_checkpoint = true;
    ports.coordinator().run("move-1").await.unwrap_err();
    ports.coordinator().rollback("move-1").await.unwrap();
    assert_eq!(ports.0.lock().unwrap().rollbacks, 1);
    assert!(
        ports
            .0
            .lock()
            .unwrap()
            .journal
            .as_ref()
            .unwrap()
            .manifest
            .destination_write_started
    );

    let ports = Ports::new();
    ports.plan().await;
    ports.coordinator().run("move-1").await.unwrap();
    assert!(matches!(
        ports.coordinator().rollback("move-1").await,
        Err(Error::Conflict(_))
    ));
    assert_eq!(ports.0.lock().unwrap().rollbacks, 0);

    let ports = Ports::new();
    ports.plan().await;
    ports.0.lock().unwrap().unknown_publication_receipt = true;
    assert!(matches!(
        ports.coordinator().rollback("move-1").await,
        Err(Error::CommitUnknown)
    ));
    assert!(
        !ports
            .0
            .lock()
            .unwrap()
            .journal
            .as_ref()
            .unwrap()
            .manifest
            .rolled_back
    );
}

#[tokio::test]
async fn changed_chunk_receipt_conflicts_and_bounds_are_checked() {
    let ports = Ports::new();
    ports.plan().await;
    ports.0.lock().unwrap().receipts.insert(
        String::new(),
        ChunkReceipt {
            cursor: String::new(),
            sha256: "wrong".into(),
            next_cursor: Some("1".into()),
        },
    );
    assert!(matches!(
        ports.coordinator().run("move-1").await,
        Err(Error::Conflict(_))
    ));
    assert_eq!(ports.0.lock().unwrap().writes, 0);
    let chunk = ports.read_chunk("history", "", 1024).await.unwrap();
    assert!(chunk_hash(&chunk, 1).is_err());
    assert_eq!(
        chunk_hash(&chunk, 1024).unwrap(),
        chunk_hash(&chunk, 1024).unwrap()
    );
}
