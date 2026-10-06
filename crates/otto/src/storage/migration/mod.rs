//! Offline logical migration. Concrete adapters own codecs, durable staging,
//! journal CAS and enforceable writer exclusion; this module owns sequencing.
pub mod contracts;
pub mod group;
pub mod record;
pub mod references;
pub use contracts::*;

use std::io::Write;

use sha2::{Digest, Sha256};

pub struct Coordinator<'a> {
    pub source: &'a dyn SnapshotExport,
    pub destination: &'a dyn StageImport,
    pub journal: &'a dyn MigrationJournal,
    pub cutover: &'a dyn Cutover,
    pub max_chunk_bytes: usize,
}

impl Coordinator<'_> {
    /// Source must already be frozen by the owning lifecycle. This only plans
    /// and records the migration; it does not acquire a fictitious local lock.
    pub async fn plan(
        &self,
        migration_id: String,
        destination_identity: String,
        expected_route_generation: String,
    ) -> Result<Manifest> {
        if migration_id.is_empty()
            || destination_identity.is_empty()
            || expected_route_generation.is_empty()
            || self.max_chunk_bytes == 0
        {
            return Err(Error::Invalid(
                "empty migration identity, route or chunk limit".into(),
            ));
        }
        let source = self.source.descriptor().await?;
        if source.source_identity.is_empty()
            || source.generation.is_empty()
            || source.domains.is_empty()
            || source.source_identity == destination_identity
        {
            return Err(Error::Invalid(
                "invalid source snapshot or identical destination".into(),
            ));
        }
        self.source.validate_frozen(&source).await?;
        let progress = source
            .domains
            .keys()
            .map(|domain| {
                (
                    domain.clone(),
                    Progress {
                        cursor: String::new(),
                        complete: false,
                        chunks: Vec::new(),
                    },
                )
            })
            .collect();
        let manifest = Manifest {
            format_version: FORMAT_VERSION,
            max_chunk_bytes: self.max_chunk_bytes,
            migration_id,
            destination_identity,
            expected_route_generation,
            source,
            progress,
            destination_write_started: false,
            sealed: false,
            published_generation: None,
            rolled_back: false,
        };
        self.destination.validate_plan(&manifest).await?;
        self.cutover.validate_plan(&manifest).await?;
        self.persist(None, &manifest).await?;
        Ok(manifest)
    }

    async fn persist(&self, expected: Option<u64>, manifest: &Manifest) -> Result<u64> {
        match self.journal.save(expected, manifest).await {
            Err(Error::CommitUnknown) => match self.journal.load(&manifest.migration_id).await {
                Ok(Some(entry)) if entry.manifest == *manifest => Ok(entry.revision),
                _ => Err(Error::CommitUnknown),
            },
            result => result,
        }
    }

    async fn load(&self, id: &str) -> Result<JournalEntry> {
        let entry = self
            .journal
            .load(id)
            .await?
            .ok_or_else(|| Error::Invalid("migration manifest missing".into()))?;
        let manifest = &entry.manifest;
        if manifest.migration_id != id
            || manifest.format_version != FORMAT_VERSION
            || manifest.max_chunk_bytes == 0
            || manifest.max_chunk_bytes > self.max_chunk_bytes
            || manifest.progress.keys().ne(manifest.source.domains.keys())
        {
            return Err(Error::Invalid("incompatible migration manifest".into()));
        }
        if self.source.descriptor().await? != manifest.source {
            return Err(Error::Conflict(
                "resume requires the same frozen source".into(),
            ));
        }
        self.source.validate_frozen(&manifest.source).await?;
        Ok(entry)
    }

    /// Resume by reading durable receipts; copying never executes pending tools.
    pub async fn run(&self, id: &str) -> Result<Manifest> {
        let JournalEntry {
            mut revision,
            mut manifest,
        } = self.load(id).await?;
        if manifest.rolled_back {
            return Err(Error::Conflict("migration rolled back".into()));
        }
        if manifest.published_generation.is_some() {
            return Ok(manifest);
        }
        // Reconcile publication before checking the old snapshot at destination:
        // fresh owners may have written there after a lost publication ack.
        if let Some(generation) = self.cutover.receipt(id).await? {
            if !manifest.sealed
                || generation.is_empty()
                || generation == manifest.expected_route_generation
            {
                return Err(Error::Invalid(
                    "publication receipt does not match sealed cutover".into(),
                ));
            }
            manifest.published_generation = Some(generation);
            self.persist(Some(revision), &manifest).await?;
            return Ok(manifest);
        }
        if !manifest.destination_write_started {
            self.destination.validate_plan(&manifest).await?;
            self.cutover.validate_plan(&manifest).await?;
            manifest.destination_write_started = true;
            revision = self.persist(Some(revision), &manifest).await?;
        }
        if !manifest.sealed {
            self.destination.begin(&manifest).await?;
            for domain in manifest.source.domains.keys().cloned().collect::<Vec<_>>() {
                while !manifest.progress[&domain].complete {
                    self.source.validate_frozen(&manifest.source).await?;
                    let progress = &manifest.progress[&domain];
                    let chunk = self
                        .source
                        .read_chunk(&domain, &progress.cursor, manifest.max_chunk_bytes)
                        .await?;
                    if chunk.cursor != progress.cursor
                        || chunk.next_cursor.as_ref().is_some_and(|next| {
                            next == &chunk.cursor
                                || progress
                                    .chunks
                                    .iter()
                                    .any(|receipt| &receipt.cursor == next)
                        })
                    {
                        return Err(Error::Invalid("non-progressing export cursor".into()));
                    }
                    let sha256 = chunk_hash(&chunk, manifest.max_chunk_bytes)?;
                    let expected = ChunkReceipt {
                        cursor: chunk.cursor.clone(),
                        sha256: sha256.clone(),
                        next_cursor: chunk.next_cursor.clone(),
                    };
                    let receipt = match self
                        .destination
                        .chunk_receipt(id, &domain, &chunk.cursor)
                        .await?
                    {
                        Some(receipt) => receipt,
                        None => match self
                            .destination
                            .apply_chunk(id, &domain, &chunk, &sha256)
                            .await
                        {
                            Err(Error::CommitUnknown) => self
                                .destination
                                .chunk_receipt(id, &domain, &chunk.cursor)
                                .await
                                .map_err(|_| Error::CommitUnknown)?
                                .ok_or(Error::CommitUnknown)?,
                            result => result?,
                        },
                    };
                    if receipt != expected {
                        return Err(Error::Conflict("chunk receipt/hash mismatch".into()));
                    }
                    let progress = manifest
                        .progress
                        .get_mut(&domain)
                        .expect("validated domain");
                    progress.complete = chunk.next_cursor.is_none();
                    if let Some(next) = chunk.next_cursor {
                        progress.cursor = next;
                    }
                    progress.chunks.push(receipt);
                    revision = self.persist(Some(revision), &manifest).await?;
                }
            }
            self.source.validate_frozen(&manifest.source).await?;
            let verified = self.destination.verify(&manifest).await?;
            let expected = manifest
                .source
                .domains
                .iter()
                .map(|(name, snapshot)| (name.clone(), snapshot.digest.clone()))
                .collect();
            if verified != expected {
                return Err(Error::Invalid(
                    "destination verification digest/count mismatch".into(),
                ));
            }
            self.destination.seal(&manifest).await?;
            manifest.sealed = true;
            revision = self.persist(Some(revision), &manifest).await?;
        } else {
            // A crash/restart can separate sealing from publication. Reopen and
            // verify actual staged data rather than trusting the checkpoint bit.
            self.destination.validate_plan(&manifest).await?;
            let verified = self.destination.verify(&manifest).await?;
            let expected = manifest
                .source
                .domains
                .iter()
                .map(|(name, snapshot)| (name.clone(), snapshot.digest.clone()))
                .collect();
            if verified != expected {
                return Err(Error::Invalid(
                    "sealed destination verification mismatch".into(),
                ));
            }
            self.destination.seal(&manifest).await?;
        }
        self.source.validate_frozen(&manifest.source).await?;
        let generation = match self.cutover.receipt(id).await? {
            Some(generation) => generation,
            None => match self.cutover.publish(&manifest).await {
                Err(Error::CommitUnknown) => self
                    .cutover
                    .receipt(id)
                    .await
                    .map_err(|_| Error::CommitUnknown)?
                    .ok_or(Error::CommitUnknown)?,
                result => result?,
            },
        };
        if generation.is_empty() || generation == manifest.expected_route_generation {
            return Err(Error::Invalid(
                "cutover did not issue a new route generation".into(),
            ));
        }
        manifest.published_generation = Some(generation);
        self.persist(Some(revision), &manifest).await?;
        Ok(manifest)
    }

    pub async fn rollback(&self, id: &str) -> Result<()> {
        let entry = self
            .journal
            .load(id)
            .await?
            .ok_or_else(|| Error::Invalid("migration manifest missing".into()))?;
        if entry.manifest.migration_id != id || entry.manifest.format_version != FORMAT_VERSION {
            return Err(Error::Invalid("incompatible rollback manifest".into()));
        }
        let JournalEntry {
            revision,
            mut manifest,
        } = entry;
        if manifest.published_generation.is_some() || self.cutover.receipt(id).await?.is_some() {
            return Err(Error::Conflict(
                "published destination requires a reverse verified migration".into(),
            ));
        }
        if manifest.rolled_back {
            return self.cutover.rollback(&manifest).await;
        }
        // The route owner must atomically exclude a concurrent run; journal CAS
        // alone is not writer authority. Mark rollback first to stop new resumes.
        manifest.rolled_back = true;
        self.persist(Some(revision), &manifest).await?;
        self.cutover.rollback(&manifest).await
    }

    /// Caller-owned lifecycle: always drain both ports, including on run errors.
    /// Fencing deliberately remains held until a route-owner decision.
    pub async fn close(&self) -> Result<()> {
        let source = self.source.close().await;
        let destination = self.destination.close().await;
        source.and(destination)
    }
}

/// Hash deterministic neutral encoding without allocating another chunk-sized
/// buffer. The exporter must enforce the same bound BEFORE materializing data.
pub fn chunk_hash(chunk: &Chunk, max_bytes: usize) -> Result<String> {
    if max_bytes == 0 {
        return Err(Error::Invalid("zero chunk limit".into()));
    }
    for blob in &chunk.blobs {
        if blob.id.is_empty()
            || blob
                .offset
                .checked_add(blob.bytes.len() as u64)
                .is_none_or(|end| end > blob.total_len)
        {
            return Err(Error::Invalid("invalid blob range".into()));
        }
    }
    if chunk
        .records
        .iter()
        .any(|record| record.id.is_empty() || record.revision.is_empty())
    {
        return Err(Error::Invalid("empty record identity/revision".into()));
    }
    struct BoundedHash {
        hash: Sha256,
        remaining: usize,
    }
    impl Write for BoundedHash {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.remaining {
                return Err(std::io::Error::other("chunk size limit exceeded"));
            }
            self.remaining -= bytes.len();
            self.hash.update(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = BoundedHash {
        hash: Sha256::new(),
        remaining: max_bytes,
    };
    serde_json::to_writer(&mut writer, chunk).map_err(|error| Error::Invalid(error.to_string()))?;
    Ok(format!("{:x}", writer.hash.finalize()))
}

#[cfg(test)]
mod tests;
