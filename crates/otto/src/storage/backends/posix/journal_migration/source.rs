//! Journal snapshot export: locked source files and cursors.

use super::*;

pub(super) struct LockedFile {
    pub(super) record: FileRecord,
    pub(super) file: File,
}

pub struct JournalSource {
    pub(super) files: Arc<Mutex<Option<Vec<LockedFile>>>>,
    pub(super) fence: Arc<dyn FreezeGuard>,
    pub(super) snapshot: FrozenSnapshot,
    pub(super) domain: Domain,
}

impl JournalSource {
    pub fn open(
        root: &Path,
        domain: Domain,
        inputs: Vec<JournalInput>,
        source_identity: String,
        generation: String,
        fence: Arc<dyn FreezeGuard>,
    ) -> Result<Self> {
        identity(&source_identity)?;
        identity(&generation)?;
        fence.validate(&generation)?;
        if inputs.len() > MAX_FILES {
            return Err(invalid("migration input file count exceeds limit"));
        }
        let root_path = root.to_path_buf();
        let root = Files::pin_read_only(root)?;
        let mut files = BTreeMap::new();
        for input in inputs {
            let mut file = open_file(&root.0, &input.relative_path)?;
            if file.metadata().map_err(unavailable)?.len() > domain.max_file() {
                return Err(invalid("migration input exceeds domain file limit"));
            }
            posix::try_lock_exclusive(&file)
                .map_err(|_| conflict("journal/artifact has an active writer"))?;
            let (id, parent_session, leaf_id) = match domain {
                Domain::Sessions => {
                    if input.artifact_id.is_some() {
                        return Err(invalid("artifact identity on session input"));
                    }
                    session_metadata(&mut file)?
                }
                Domain::Artifacts => (
                    input
                        .artifact_id
                        .ok_or_else(|| invalid("artifact identity required"))?,
                    None,
                    None,
                ),
            };
            let mut aliases = vec![
                root_path
                    .join(&input.relative_path)
                    .to_str()
                    .ok_or_else(|| invalid("non-Unicode source reference"))?
                    .to_owned(),
            ];
            if let Some(alias) = input.legacy_path
                && !aliases.contains(&alias)
            {
                aliases.push(alias);
            }
            let record = FileRecord {
                domain,
                relative_path: input.relative_path.clone(),
                id,
                aliases,
                parent_session,
                leaf_id,
                len: file.metadata().map_err(unavailable)?.len(),
                sha256: file_hash(&mut file)?,
            };
            validate_record(&record, domain)?;
            if files
                .insert(input.relative_path, LockedFile { record, file })
                .is_some()
            {
                return Err(invalid("duplicate file reference"));
            }
        }
        let records = files
            .iter()
            .map(|(path, file)| (path.clone(), file.record.clone()))
            .collect();
        mappings(&records, domain)?;
        let digest = digest(&records)?;
        let snapshot = FrozenSnapshot {
            source_identity,
            generation,
            domains: BTreeMap::from([(
                domain.name().into(),
                DomainSnapshot {
                    schema_family: domain.family().into(),
                    schema_version: 1,
                    committed_cursor: digest.sha256.clone(),
                    digest,
                },
            )]),
        };
        Ok(Self {
            files: Arc::new(Mutex::new(Some(files.into_values().collect()))),
            fence,
            snapshot,
            domain,
        })
    }
}

pub(super) fn cursor(cursor: &str) -> Result<(usize, Option<u64>)> {
    if cursor.len() > 64 {
        return Err(invalid("blob cursor exceeds limit"));
    }
    if cursor.is_empty() {
        return Ok((0, None));
    }
    let (index, offset) = cursor
        .split_once(':')
        .ok_or_else(|| invalid("invalid blob cursor"))?;
    let index = index
        .parse()
        .map_err(|_| invalid("invalid blob file cursor"))?;
    let offset = if offset == "m" {
        None
    } else {
        Some(
            offset
                .parse()
                .map_err(|_| invalid("invalid blob offset cursor"))?,
        )
    };
    Ok((index, offset))
}

#[async_trait]
impl SnapshotExport for JournalSource {
    async fn descriptor(&self) -> Result<FrozenSnapshot> {
        self.validate_frozen(&self.snapshot).await?;
        Ok(self.snapshot.clone())
    }
    async fn validate_frozen(&self, expected: &FrozenSnapshot) -> Result<()> {
        if expected != &self.snapshot {
            return Err(conflict("journal source snapshot changed"));
        }
        let files = self.files.clone();
        let fence = self.fence.clone();
        let generation = self.snapshot.generation.clone();
        tokio::task::spawn_blocking(move || {
            fence.validate(&generation)?;
            if files.lock().map_err(unavailable)?.is_none() {
                return Err(conflict("journal source closed"));
            }
            Ok(())
        })
        .await
        .map_err(unavailable)?
    }
    async fn read_chunk(&self, domain: &str, position: &str, max_bytes: usize) -> Result<Chunk> {
        self.validate_frozen(&self.snapshot).await?;
        if domain != self.domain.name() || max_bytes == 0 || max_bytes > MAX_CHUNK {
            return Err(invalid("unsupported journal domain/chunk limit"));
        }
        cursor(position)?;
        let files = self.files.clone();
        let position = position.to_owned();
        tokio::task::spawn_blocking(move || {
            let mut owned = files.lock().map_err(unavailable)?;
            let files = owned
                .as_mut()
                .ok_or_else(|| conflict("journal source closed"))?;
            let (index, offset) = cursor(&position)?;
            let mut chunk = Chunk {
                cursor: position,
                next_cursor: None,
                records: vec![],
                blobs: vec![],
            };
            if files.is_empty() && index == 0 && offset.is_none() {
                chunk_hash(&chunk, max_bytes)?;
                return Ok(chunk);
            }
            if index >= files.len() {
                return Err(invalid("blob cursor outside snapshot"));
            }
            let next_file = (index + 1 < files.len()).then(|| format!("{}:m", index + 1));
            let file = files
                .get_mut(index)
                .ok_or_else(|| invalid("blob cursor outside snapshot"))?;
            if file.file.metadata().map_err(unavailable)?.len() != file.record.len {
                return Err(conflict("frozen journal length changed"));
            }
            if let Some(offset) = offset {
                if offset >= file.record.len {
                    return Err(invalid("blob cursor past end"));
                }
                chunk.next_cursor = Some(format!("{index}:{}", file.record.len));
                chunk.blobs.push(Blob {
                    id: file.record.relative_path.clone(),
                    offset,
                    total_len: file.record.len,
                    bytes: vec![],
                });
                let overhead = serde_json::to_vec(&chunk)
                    .map_err(|_| invalid("blob encoding failed"))?
                    .len();
                let capacity = max_bytes.saturating_sub(overhead + 128) / 4;
                let count = capacity
                    .min(IO_BUFFER)
                    .min((file.record.len - offset) as usize);
                if count == 0 {
                    return Err(invalid("chunk limit too small for blob envelope"));
                }
                let mut bytes = vec![0; count];
                file.file
                    .seek(SeekFrom::Start(offset))
                    .and_then(|_| file.file.read_exact(&mut bytes))
                    .map_err(unavailable)?;
                chunk.blobs[0].bytes = bytes;
                chunk.next_cursor = if offset + count as u64 == file.record.len {
                    next_file
                } else {
                    Some(format!("{index}:{}", offset + count as u64))
                };
            } else {
                chunk.records.push(Record {
                    id: file.record.relative_path.clone(),
                    revision: file.record.sha256.clone(),
                    payload: serde_json::to_vec(&file.record)
                        .map_err(|_| invalid("file record encoding failed"))?,
                });
                chunk.next_cursor = if file.record.len == 0 {
                    next_file
                } else {
                    Some(format!("{index}:0"))
                };
            }
            chunk_hash(&chunk, max_bytes)?;
            Ok(chunk)
        })
        .await
        .map_err(unavailable)?
    }
    async fn close(&self) -> Result<()> {
        let files = self.files.clone();
        tokio::task::spawn_blocking(move || {
            files.lock().map_err(unavailable)?.take();
            Ok(())
        })
        .await
        .map_err(unavailable)?
    }
}
