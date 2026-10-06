//! Compose independent domain ports without giving the coordinator backend knowledge.
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use async_trait::async_trait;

use super::contracts::*;

pub struct GroupSource {
    sources: Vec<Arc<dyn SnapshotExport>>,
    owners: BTreeMap<String, usize>,
    snapshot: FrozenSnapshot,
}

impl GroupSource {
    pub async fn new(sources: Vec<Arc<dyn SnapshotExport>>) -> Result<Self> {
        let result = Self::describe(&sources).await;
        match result {
            Ok((snapshot, owners)) => Ok(Self {
                sources,
                owners,
                snapshot,
            }),
            Err(error) => {
                for source in &sources {
                    let _ = source.close().await;
                }
                Err(error)
            }
        }
    }

    async fn describe(
        sources: &[Arc<dyn SnapshotExport>],
    ) -> Result<(FrozenSnapshot, BTreeMap<String, usize>)> {
        let mut snapshot: Option<FrozenSnapshot> = None;
        let mut owners = BTreeMap::new();
        for (index, source) in sources.iter().enumerate() {
            let descriptor = source.descriptor().await?;
            source.validate_frozen(&descriptor).await?;
            if descriptor.source_identity.is_empty()
                || descriptor.generation.is_empty()
                || descriptor.domains.is_empty()
            {
                return Err(Error::Invalid("empty source group identity/domain".into()));
            }
            if let Some(group) = &snapshot {
                if group.source_identity != descriptor.source_identity
                    || group.generation != descriptor.generation
                {
                    return Err(Error::Conflict(
                        "source group freeze identity differs".into(),
                    ));
                }
            } else {
                snapshot = Some(FrozenSnapshot {
                    source_identity: descriptor.source_identity.clone(),
                    generation: descriptor.generation.clone(),
                    domains: BTreeMap::new(),
                });
            }
            let group = snapshot.as_mut().unwrap();
            for (domain, schema) in descriptor.domains {
                if owners.insert(domain.clone(), index).is_some() {
                    return Err(Error::Invalid("duplicate source domain".into()));
                }
                group.domains.insert(domain, schema);
            }
        }
        Ok((
            snapshot.ok_or_else(|| Error::Invalid("empty source group".into()))?,
            owners,
        ))
    }
}

#[async_trait]
impl SnapshotExport for GroupSource {
    async fn descriptor(&self) -> Result<FrozenSnapshot> {
        self.validate_frozen(&self.snapshot).await?;
        Ok(self.snapshot.clone())
    }
    async fn validate_frozen(&self, expected: &FrozenSnapshot) -> Result<()> {
        if expected != &self.snapshot {
            return Err(Error::Conflict("source group snapshot changed".into()));
        }
        for (index, source) in self.sources.iter().enumerate() {
            let mut scoped = expected.clone();
            scoped
                .domains
                .retain(|domain, _| self.owners[domain] == index);
            source.validate_frozen(&scoped).await?;
        }
        Ok(())
    }
    async fn read_chunk(&self, domain: &str, cursor: &str, max_bytes: usize) -> Result<Chunk> {
        self.validate_frozen(&self.snapshot).await?;
        let owner = self
            .owners
            .get(domain)
            .ok_or_else(|| Error::Invalid("domain outside source group".into()))?;
        self.sources[*owner]
            .read_chunk(domain, cursor, max_bytes)
            .await
    }
    async fn close(&self) -> Result<()> {
        let mut result = Ok(());
        for source in &self.sources {
            if let Err(error) = source.close().await
                && result.is_ok()
            {
                result = Err(error);
            }
        }
        result
    }
}

/// Domain-owned validators inspect staged ports and check references across
/// stores. Per-domain hashes alone cannot establish this closure.
#[async_trait]
pub trait ReferenceClosure: Send + Sync {
    async fn validate(&self, manifest: &Manifest) -> Result<()>;
}

struct StageGroup {
    stage: Arc<dyn StageImport>,
    domains: BTreeSet<String>,
}

pub struct GroupStage {
    stages: Vec<StageGroup>,
    owners: BTreeMap<String, usize>,
    closure: Arc<dyn ReferenceClosure>,
}

impl GroupStage {
    pub fn new(
        domains: BTreeMap<String, Arc<dyn StageImport>>,
        closure: Arc<dyn ReferenceClosure>,
    ) -> Result<Self> {
        if domains.is_empty() || domains.keys().any(String::is_empty) {
            return Err(Error::Invalid("empty destination group".into()));
        }
        let mut stages: Vec<StageGroup> = Vec::new();
        let mut owners = BTreeMap::new();
        for (domain, stage) in domains {
            let owner = match stages
                .iter()
                .position(|group| Arc::ptr_eq(&group.stage, &stage))
            {
                Some(owner) => owner,
                None => {
                    stages.push(StageGroup {
                        stage,
                        domains: BTreeSet::new(),
                    });
                    stages.len() - 1
                }
            };
            stages[owner].domains.insert(domain.clone());
            owners.insert(domain, owner);
        }
        Ok(Self {
            stages,
            owners,
            closure,
        })
    }
    fn scoped(&self, manifest: &Manifest, group: &StageGroup) -> Result<Manifest> {
        if self.owners.keys().ne(manifest.source.domains.keys())
            || manifest.progress.keys().ne(manifest.source.domains.keys())
        {
            return Err(Error::Invalid(
                "destination group domain closure differs".into(),
            ));
        }
        let mut scoped = manifest.clone();
        scoped
            .source
            .domains
            .retain(|domain, _| group.domains.contains(domain));
        scoped
            .progress
            .retain(|domain, _| group.domains.contains(domain));
        Ok(scoped)
    }
    fn owner(&self, domain: &str) -> Result<&dyn StageImport> {
        let index = self
            .owners
            .get(domain)
            .ok_or_else(|| Error::Invalid("domain outside destination group".into()))?;
        Ok(self.stages[*index].stage.as_ref())
    }
}

#[async_trait]
impl StageImport for GroupStage {
    async fn validate_plan(&self, manifest: &Manifest) -> Result<()> {
        for group in &self.stages {
            group
                .stage
                .validate_plan(&self.scoped(manifest, group)?)
                .await?;
        }
        Ok(())
    }
    async fn begin(&self, manifest: &Manifest) -> Result<()> {
        for group in &self.stages {
            group.stage.begin(&self.scoped(manifest, group)?).await?;
        }
        Ok(())
    }
    async fn apply_chunk(
        &self,
        id: &str,
        domain: &str,
        chunk: &Chunk,
        sha256: &str,
    ) -> Result<ChunkReceipt> {
        self.owner(domain)?
            .apply_chunk(id, domain, chunk, sha256)
            .await
    }
    async fn chunk_receipt(
        &self,
        id: &str,
        domain: &str,
        cursor: &str,
    ) -> Result<Option<ChunkReceipt>> {
        self.owner(domain)?.chunk_receipt(id, domain, cursor).await
    }
    async fn verify(&self, manifest: &Manifest) -> Result<BTreeMap<String, DomainDigest>> {
        let mut digests = BTreeMap::new();
        for group in &self.stages {
            let scoped = self.scoped(manifest, group)?;
            let verified = group.stage.verify(&scoped).await?;
            if verified.keys().ne(scoped.source.domains.keys()) {
                return Err(Error::Invalid(
                    "stage verification returned wrong domains".into(),
                ));
            }
            for (domain, digest) in verified {
                if digest != manifest.source.domains[&domain].digest {
                    return Err(Error::Invalid("group domain digest mismatch".into()));
                }
                digests.insert(domain, digest);
            }
        }
        self.closure.validate(manifest).await?;
        Ok(digests)
    }
    async fn seal(&self, manifest: &Manifest) -> Result<()> {
        self.verify(manifest).await?;
        for group in &self.stages {
            group.stage.seal(&self.scoped(manifest, group)?).await?;
        }
        Ok(())
    }
    async fn close(&self) -> Result<()> {
        let mut result = Ok(());
        for group in &self.stages {
            if let Err(error) = group.stage.close().await
                && result.is_ok()
            {
                result = Err(error);
            }
        }
        result
    }
}
