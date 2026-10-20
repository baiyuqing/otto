//! Snapshot export and staged import for the JSON repository backend.

use super::*;

pub(super) fn chunk(
    records: &[Record],
    cursor: &str,
    max_bytes: usize,
) -> migration::Result<Chunk> {
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

pub(super) struct JsonSource {
    pub(super) snapshot: FrozenSnapshot,
    pub(super) records: Vec<Record>,
    pub(super) fence: Arc<dyn FreezeGuard>,
    pub(super) closed: AtomicBool,
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

pub(super) struct JsonStage {
    pub(super) store: Arc<JsonStore>,
    pub(super) closed: AtomicBool,
    pub(super) lose_ack: Arc<AtomicBool>,
}
pub(super) fn failure(_: approval::Error) -> Error {
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
