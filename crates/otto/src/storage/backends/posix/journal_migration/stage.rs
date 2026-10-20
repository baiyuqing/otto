//! Journal staging import: envelopes, reconstruction and `StageImport`.

use super::*;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct StageHeader {
    pub(super) migration_id: String,
    pub(super) destination_identity: String,
    pub(super) max_chunk_bytes: usize,
    pub(super) source: FrozenSnapshot,
}

impl StageHeader {
    fn new(manifest: &Manifest) -> Self {
        Self {
            migration_id: manifest.migration_id.clone(),
            destination_identity: manifest.destination_identity.clone(),
            max_chunk_bytes: manifest.max_chunk_bytes,
            source: manifest.source.clone(),
        }
    }
}

#[derive(Serialize, Deserialize)]
pub(super) struct Envelope {
    pub(super) migration_id: String,
    pub(super) domain: String,
    pub(super) receipt: ChunkReceipt,
    pub(super) chunk: Chunk,
}

pub(super) fn chunk_name(domain: Domain, position: &str) -> String {
    format!(
        ".chunk-{}-{:x}.json",
        domain.name(),
        Sha256::digest(position.as_bytes())
    )
}

pub(super) fn envelope(
    dir: &fsops::Dir,
    domain: Domain,
    position: &str,
    id: &str,
    limit: usize,
) -> Result<Option<Envelope>> {
    let envelope: Option<Envelope> = read(dir, &chunk_name(domain, position))?;
    if let Some(envelope) = &envelope {
        let receipt = &envelope.receipt;
        if envelope.migration_id != id
            || envelope.domain != domain.name()
            || receipt.cursor != position
            || envelope.chunk.cursor != position
            || receipt.next_cursor != envelope.chunk.next_cursor
            || chunk_hash(&envelope.chunk, limit)? != receipt.sha256
        {
            return Err(invalid("corrupt atomic chunk receipt"));
        }
    }
    Ok(envelope)
}

pub struct JournalStage {
    pub(super) files: Files,
    pub(super) root_path: PathBuf,
    pub(super) domain: Domain,
    pub(super) identity: String,
    pub(super) closed: Arc<AtomicBool>,
    pub(super) owned: Arc<Mutex<()>>,
}

impl JournalStage {
    pub fn new(root: &Path, domain: Domain, destination_identity: String) -> Result<Self> {
        identity(&destination_identity)?;
        Ok(Self {
            files: Files::open(root)?,
            root_path: root.to_path_buf(),
            domain,
            identity: destination_identity,
            closed: Arc::new(AtomicBool::new(false)),
            owned: Arc::new(Mutex::new(())),
        })
    }
    fn compatibility(&self, manifest: &Manifest) -> Result<()> {
        let snapshot = manifest
            .source
            .domains
            .get(self.domain.name())
            .ok_or_else(|| invalid("journal domain missing"))?;
        if manifest.source.domains.len() != 1
            || manifest.progress.len() != 1
            || snapshot.schema_family != self.domain.family()
            || snapshot.schema_version != 1
            || manifest.max_chunk_bytes == 0
            || manifest.max_chunk_bytes > MAX_CHUNK
            || manifest.destination_identity != self.identity
        {
            return Err(invalid(
                "incompatible journal migration format/domain/identity",
            ));
        }
        Ok(())
    }
    async fn run<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&fsops::Dir) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let closed = self.closed.clone();
        let owned = self.owned.clone();
        let files = self.files.clone();
        if closed.load(Ordering::Acquire) {
            return Err(conflict("journal stage closed"));
        }
        tokio::task::spawn_blocking(move || {
            let _owned = owned.lock().map_err(unavailable)?;
            if closed.load(Ordering::Acquire) {
                return Err(conflict("journal stage closed"));
            }
            files.locked(operation)
        })
        .await
        .map_err(unavailable)?
    }
    pub async fn dataset(&self) -> Result<Dataset> {
        let name = self.domain.dataset_name();
        self.run(move |dir| read(dir, &name)?.ok_or_else(|| invalid("dataset is not sealed")))
            .await
    }

    /// Read aliases before sealing, after ordinary `verify` has materialized
    /// and validated files. Accepts the full cutover manifest and scopes it.
    pub async fn reference_dataset(&self, manifest: &Manifest) -> Result<Dataset> {
        let mut manifest = manifest.clone();
        let domain = self.domain;
        manifest
            .source
            .domains
            .retain(|name, _| name == domain.name());
        manifest.progress.retain(|name, _| name == domain.name());
        self.compatibility(&manifest)?;
        self.run(move |dir| {
            checked_header(dir, domain, &manifest)?;
            let dataset = match read::<Dataset>(dir, &domain.dataset_name())? {
                Some(dataset) => dataset,
                None => {
                    let files = reference_records(dir, domain, &manifest)?;
                    let (aliases, session_ids) = mappings(&files, domain)?;
                    Dataset {
                        version: 1,
                        migration_id: manifest.migration_id.clone(),
                        snapshot: manifest.source.clone(),
                        files,
                        aliases,
                        session_ids,
                    }
                }
            };
            if verify_dataset(dir, domain, &manifest, &dataset)?
                != manifest.source.domains[domain.name()].digest
            {
                return Err(invalid("reference metadata digest differs"));
            }
            Ok(dataset)
        })
        .await
    }
}

pub(super) fn checked_header(dir: &fsops::Dir, domain: Domain, manifest: &Manifest) -> Result<()> {
    let header: StageHeader =
        read(dir, &domain.stage_name())?.ok_or_else(|| invalid("journal stage missing"))?;
    if header != StageHeader::new(manifest) {
        return Err(conflict("journal stage source identity differs"));
    }
    Ok(())
}

pub(super) fn fresh_namespace(dir: &fsops::Dir) -> Result<()> {
    for name in posix::read_directory_names(dir, MAX_FILES).map_err(unavailable)? {
        // A crash before publishing the first identity can leave an unreachable
        // atomic-write temporary. It is not an active dataset or a receipt.
        if name.strip_prefix(".migration-").is_none_or(|nonce| {
            nonce.len() != 32 || !nonce.bytes().all(|byte| byte.is_ascii_hexdigit())
        }) {
            return Err(conflict("destination is not an isolated journal stage"));
        }
        let file = fsops::open_at_no_follow(dir, &name, libc::O_RDONLY | libc::O_NONBLOCK)
            .map_err(unavailable)?;
        if !file.metadata().map_err(unavailable)?.is_file() {
            return Err(invalid("invalid unpublished stage temporary"));
        }
    }
    Ok(())
}

pub(super) fn reference_records(
    dir: &fsops::Dir,
    domain: Domain,
    manifest: &Manifest,
) -> Result<BTreeMap<String, FileRecord>> {
    let progress = &manifest.progress[domain.name()];
    if !progress.complete {
        return Err(invalid("references requested before complete import"));
    }
    let mut files = BTreeMap::new();
    let mut next = Some(String::new());
    for receipt in &progress.chunks {
        if next.as_ref() != Some(&receipt.cursor) {
            return Err(invalid("reference chunk discontinuity"));
        }
        let stored = envelope(
            dir,
            domain,
            &receipt.cursor,
            &manifest.migration_id,
            manifest.max_chunk_bytes,
        )?
        .ok_or_else(|| invalid("reference chunk missing"))?;
        if stored.receipt != *receipt {
            return Err(invalid("reference receipt differs"));
        }
        next = receipt.next_cursor.clone();
        for record in stored.chunk.records {
            let metadata = checked_record(&record, domain)?;
            if files.len() >= MAX_FILES
                || files
                    .insert(metadata.relative_path.clone(), metadata)
                    .is_some()
            {
                return Err(invalid("duplicate/too many reference files"));
            }
        }
    }
    if next.is_some() {
        return Err(invalid("reference stream incomplete"));
    }
    Ok(files)
}

pub(super) fn checked_record(record: &Record, domain: Domain) -> Result<FileRecord> {
    let metadata: FileRecord = serde_json::from_slice(&record.payload)
        .map_err(|_| invalid("invalid logical file record"))?;
    validate_record(&metadata, domain)?;
    if record.id != metadata.relative_path || record.revision != metadata.sha256 {
        return Err(invalid("logical file identity/hash mismatch"));
    }
    Ok(metadata)
}

pub(super) fn verify_dataset(
    dir: &fsops::Dir,
    domain: Domain,
    manifest: &Manifest,
    dataset: &Dataset,
) -> Result<DomainDigest> {
    if dataset.version != 1
        || dataset.migration_id != manifest.migration_id
        || dataset.snapshot != manifest.source
        || dataset.files.len() > MAX_FILES
    {
        return Err(conflict("dataset identity differs"));
    }
    for (path, record) in &dataset.files {
        validate_record(record, domain)?;
        if path != &record.relative_path {
            return Err(invalid("dataset file key mismatch"));
        }
        let mut file = open_file(dir, path)?;
        if file.metadata().map_err(unavailable)?.len() != record.len
            || file_hash(&mut file)? != record.sha256
        {
            return Err(invalid("sealed file digest/length mismatch"));
        }
        if domain == Domain::Sessions {
            let (id, parent, leaf) = session_metadata(&mut file)?;
            if id != record.id || parent != record.parent_session || leaf != record.leaf_id {
                return Err(invalid("session identity/replay metadata differs"));
            }
        }
    }
    let (aliases, session_ids) = mappings(&dataset.files, domain)?;
    if aliases != dataset.aliases || session_ids != dataset.session_ids {
        return Err(invalid("dataset reference aliases differ"));
    }
    digest(&dataset.files)
}

pub(super) struct PendingFile {
    pub(super) record: FileRecord,
    pub(super) dir: fsops::Dir,
    pub(super) temporary: String,
    pub(super) file: File,
    pub(super) offset: u64,
    pub(super) hash: Sha256,
}

impl Drop for PendingFile {
    fn drop(&mut self) {
        let _ = fsops::unlink_at(&self.dir, &self.temporary);
    }
}

pub(super) fn finish(pending: Option<PendingFile>) -> Result<()> {
    if let Some(pending) = pending {
        if pending.offset != pending.record.len
            || format!("{:x}", pending.hash.clone().finalize()) != pending.record.sha256
        {
            return Err(invalid("incomplete/divergent staged blob"));
        }
        pending.file.sync_all().map_err(unavailable)?;
        let name = components(&pending.record.relative_path)?
            .last()
            .unwrap()
            .to_string();
        fsops::rename_at(&pending.dir, &pending.temporary, &name).map_err(unavailable)?;
        sync_dir(&pending.dir).map_err(|_| Error::CommitUnknown)?;
    }
    Ok(())
}

pub(super) fn reconstruct(
    dir: &fsops::Dir,
    domain: Domain,
    manifest: &Manifest,
) -> Result<Dataset> {
    let progress = manifest
        .progress
        .get(domain.name())
        .ok_or_else(|| invalid("journal progress missing"))?;
    if !progress.complete {
        return Err(invalid("journal chunks are incomplete"));
    }
    let mut files = BTreeMap::new();
    let mut next = Some(String::new());
    let mut pending: Option<PendingFile> = None;
    for receipt in &progress.chunks {
        if next.as_ref() != Some(&receipt.cursor) {
            return Err(invalid("chunk stream cursor discontinuity"));
        }
        let stored = envelope(
            dir,
            domain,
            &receipt.cursor,
            &manifest.migration_id,
            manifest.max_chunk_bytes,
        )?
        .ok_or_else(|| invalid("atomic chunk missing"))?;
        if stored.receipt != *receipt {
            return Err(invalid("checkpoint differs from chunk receipt"));
        }
        next = receipt.next_cursor.clone();
        let chunk = stored.chunk;
        if chunk.records.len() == 1 && chunk.blobs.is_empty() {
            finish(pending.take())?;
            let record = checked_record(&chunk.records[0], domain)?;
            if files.len() >= MAX_FILES
                || files
                    .insert(record.relative_path.clone(), record.clone())
                    .is_some()
            {
                return Err(invalid("duplicate/too many logical files"));
            }
            let (parent, _) = parent(dir, &record.relative_path, true)?;
            let temporary = format!(
                ".migration-file-{}",
                crate::auth::random_hex::<16>().map_err(unavailable)?
            );
            let file = fsops::create_at_no_follow(
                &parent,
                &temporary,
                libc::O_WRONLY | libc::O_EXCL,
                0o600,
            )
            .map_err(unavailable)?;
            pending = Some(PendingFile {
                record,
                dir: parent,
                temporary,
                file,
                offset: 0,
                hash: Sha256::new(),
            });
        } else if chunk.records.is_empty() && chunk.blobs.len() == 1 {
            let blob = &chunk.blobs[0];
            let file = pending
                .as_mut()
                .ok_or_else(|| invalid("blob has no logical file record"))?;
            if blob.id != file.record.relative_path
                || blob.total_len != file.record.len
                || blob.offset != file.offset
                || blob.bytes.is_empty()
            {
                return Err(invalid("blob reference/order/range mismatch"));
            }
            file.file.write_all(&blob.bytes).map_err(unavailable)?;
            file.hash.update(&blob.bytes);
            file.offset += blob.bytes.len() as u64;
        } else if !chunk.records.is_empty() || !chunk.blobs.is_empty() || progress.chunks.len() != 1
        {
            return Err(invalid("unsupported journal chunk shape"));
        }
    }
    if next.is_some() {
        return Err(invalid("journal stream lacks terminal cursor"));
    }
    finish(pending)?;
    let (aliases, session_ids) = mappings(&files, domain)?;
    let dataset = Dataset {
        version: 1,
        migration_id: manifest.migration_id.clone(),
        snapshot: manifest.source.clone(),
        files,
        aliases,
        session_ids,
    };
    verify_dataset(dir, domain, manifest, &dataset)?;
    Ok(dataset)
}

#[async_trait]
impl StageImport for JournalStage {
    async fn journal_references(
        &self,
        manifest: &Manifest,
    ) -> Result<crate::storage::migration::JournalReferences> {
        let dataset = self.reference_dataset(manifest).await?;
        Ok(crate::storage::migration::JournalReferences {
            migration_id: dataset.migration_id,
            aliases: dataset.aliases,
            session_ids: dataset.session_ids,
        })
    }
    async fn validate_plan(&self, manifest: &Manifest) -> Result<()> {
        self.compatibility(manifest)?;
        let domain = self.domain;
        let header = StageHeader::new(manifest);
        self.run(move |dir| {
            if let Some(existing) = read::<StageHeader>(dir, &domain.stage_name())? {
                if existing != header {
                    return Err(conflict("journal stage belongs to another migration"));
                }
            } else {
                fresh_namespace(dir)?;
            }
            Ok(())
        })
        .await
    }
    async fn begin(&self, manifest: &Manifest) -> Result<()> {
        self.validate_plan(manifest).await?;
        let domain = self.domain;
        let header = StageHeader::new(manifest);
        self.run(
            move |dir| match read::<StageHeader>(dir, &domain.stage_name())? {
                Some(existing) if existing == header => Ok(()),
                Some(_) => Err(conflict("journal stage identity changed")),
                None => {
                    fresh_namespace(dir)?;
                    publish(dir, &domain.stage_name(), &header)
                }
            },
        )
        .await
    }
    async fn apply_chunk(
        &self,
        id: &str,
        domain_name: &str,
        chunk: &Chunk,
        sha256: &str,
    ) -> Result<ChunkReceipt> {
        if domain_name != self.domain.name() {
            return Err(invalid("unsupported journal domain"));
        }
        if chunk_hash(chunk, MAX_CHUNK)? != sha256 {
            return Err(invalid("journal chunk hash differs"));
        }
        let domain = self.domain;
        let id = id.to_owned();
        let chunk = chunk.clone();
        let sha256 = sha256.to_owned();
        self.run(move |dir| {
            let header: StageHeader =
                read(dir, &domain.stage_name())?.ok_or_else(|| invalid("journal stage missing"))?;
            if header.migration_id != id || chunk_hash(&chunk, header.max_chunk_bytes)? != sha256 {
                return Err(conflict("journal chunk identity/hash differs"));
            }
            let receipt = ChunkReceipt {
                cursor: chunk.cursor.clone(),
                sha256,
                next_cursor: chunk.next_cursor.clone(),
            };
            if let Some(previous) =
                envelope(dir, domain, &chunk.cursor, &id, header.max_chunk_bytes)?
            {
                return if previous.receipt == receipt {
                    Ok(receipt)
                } else {
                    Err(conflict("chunk cursor reused with different content"))
                };
            }
            if read::<Dataset>(dir, &domain.dataset_name())?.is_some() {
                return Err(conflict("journal dataset already sealed"));
            }
            for record in &chunk.records {
                checked_record(record, domain)?;
            }
            for blob in &chunk.blobs {
                components(&blob.id)?;
            }
            publish(
                dir,
                &chunk_name(domain, &chunk.cursor),
                &Envelope {
                    migration_id: id,
                    domain: domain.name().into(),
                    receipt: receipt.clone(),
                    chunk,
                },
            )?;
            Ok(receipt)
        })
        .await
    }
    async fn chunk_receipt(
        &self,
        id: &str,
        domain_name: &str,
        position: &str,
    ) -> Result<Option<ChunkReceipt>> {
        if domain_name != self.domain.name() {
            return Err(invalid("unsupported journal domain"));
        }
        let domain = self.domain;
        let id = id.to_owned();
        let position = position.to_owned();
        self.run(move |dir| {
            let header: Option<StageHeader> = read(dir, &domain.stage_name())?;
            let Some(header) = header else {
                return Ok(None);
            };
            if header.migration_id != id {
                return Err(conflict("journal receipt belongs to another migration"));
            }
            Ok(
                envelope(dir, domain, &position, &id, header.max_chunk_bytes)?
                    .map(|envelope| envelope.receipt),
            )
        })
        .await
    }
    async fn verify(&self, manifest: &Manifest) -> Result<BTreeMap<String, DomainDigest>> {
        self.compatibility(manifest)?;
        let domain = self.domain;
        let manifest = manifest.clone();
        self.run(move |dir| {
            checked_header(dir, domain, &manifest)?;
            let dataset = match read::<Dataset>(dir, &domain.dataset_name())? {
                Some(dataset) => dataset,
                None => reconstruct(dir, domain, &manifest)?,
            };
            let verified = verify_dataset(dir, domain, &manifest, &dataset)?;
            if verified != manifest.source.domains[domain.name()].digest {
                return Err(invalid("journal domain canonical digest/count differs"));
            }
            Ok(BTreeMap::from([(domain.name().into(), verified)]))
        })
        .await
    }
    async fn seal(&self, manifest: &Manifest) -> Result<()> {
        self.compatibility(manifest)?;
        let domain = self.domain;
        let manifest = manifest.clone();
        let root = self.root_path.clone();
        self.run(move |dir| {
            checked_header(dir, domain, &manifest)?;
            let dataset = match read::<Dataset>(dir, &domain.dataset_name())? {
                Some(dataset) => dataset,
                None => reconstruct(dir, domain, &manifest)?,
            };
            if verify_dataset(dir, domain, &manifest, &dataset)?
                != manifest.source.domains[domain.name()].digest
            {
                return Err(invalid("journal seal canonical digest differs"));
            }
            if domain == Domain::Sessions {
                for path in dataset.files.keys() {
                    super::super::session::Store::read_entries(root.join(path)).map_err(|_| {
                        invalid("ordinary session port could not reopen migrated journal")
                    })?;
                }
            }
            publish(dir, &domain.dataset_name(), &dataset)
        })
        .await
    }
    async fn close(&self) -> Result<()> {
        self.closed.store(true, Ordering::Release);
        let owned = self.owned.clone();
        tokio::task::spawn_blocking(move || {
            let _owned = owned.lock().map_err(unavailable)?;
            Ok(())
        })
        .await
        .map_err(unavailable)?
    }
}
