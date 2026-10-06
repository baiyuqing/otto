//! Exact-byte logical migration for Pi journals and domain-owned opaque artifacts.
//! No leases, workspace identity, executable trust or credentials are exported.
use std::{
    collections::BTreeMap,
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::migration::{Files, conflict, identity, invalid, publish, read, unavailable};
use crate::{
    fsops,
    storage::{
        drivers::posix,
        migration::{chunk_hash, contracts::*},
    },
};

const MAX_FILES: usize = 10_000;
const MAX_CHUNK: usize = 16 * 1024 * 1024;
const MAX_ARTIFACT: u64 = 4 * 1024 * 1024 * 1024;
const IO_BUFFER: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Domain {
    Sessions,
    Artifacts,
}

impl Domain {
    pub fn name(self) -> &'static str {
        match self {
            Self::Sessions => "sessions",
            Self::Artifacts => "artifacts",
        }
    }
    fn family(self) -> &'static str {
        match self {
            Self::Sessions => "otto.pi-journal",
            Self::Artifacts => "otto.opaque-artifact",
        }
    }
    fn max_file(self) -> u64 {
        match self {
            Self::Sessions => otto_core::session::MAX_SESSION_FILE_BYTES as u64,
            Self::Artifacts => MAX_ARTIFACT,
        }
    }
    fn stage_name(self) -> String {
        format!(".migration-{}.json", self.name())
    }
    fn dataset_name(self) -> String {
        format!(".dataset-{}.json", self.name())
    }
}

pub struct JournalInput {
    pub relative_path: String,
    /// Retain the original path spelling referenced by durable domain records.
    pub legacy_path: Option<String>,
    /// Required only for opaque artifacts; session IDs come from Pi headers.
    pub artifact_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileRecord {
    pub domain: Domain,
    pub relative_path: String,
    pub id: String,
    pub aliases: Vec<String>,
    pub parent_session: Option<String>,
    pub leaf_id: Option<String>,
    pub len: u64,
    pub sha256: String,
}

/// Metadata becomes visible only after every referenced file is complete and
/// validated. Runtime aliases resolve old paths without rewriting Pi bytes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Dataset {
    pub version: u32,
    pub migration_id: String,
    pub snapshot: FrozenSnapshot,
    pub files: BTreeMap<String, FileRecord>,
    pub aliases: BTreeMap<String, String>,
    pub session_ids: BTreeMap<String, String>,
}

/// Compatibility lookup for an active destination. Journals can append after
/// cutover, so this checks immutable identity mappings rather than old heads.
pub fn read_dataset_alias(
    root: &Path,
    legacy_path: &str,
) -> std::result::Result<Option<PathBuf>, String> {
    resolve_alias(root, legacy_path)
        .map(|path| path.map(PathBuf::from))
        .map_err(|error| error.to_string())
}

fn resolve_alias(root: &Path, legacy_path: &str) -> Result<Option<String>> {
    if root
        .components()
        .any(|component| matches!(component, Component::ParentDir | Component::Prefix(_)))
    {
        return Err(invalid("session alias root contains traversal"));
    }
    // Legacy session roots predate the migration control directory and may
    // have their historical permissions. Probe the exact sealed dataset
    // name through a pinned, no-follow root first; only a present dataset
    // opts this lookup into the stricter migration-control permissions.
    let probe = match fsops::open_dir_no_follow(root) {
        Ok(root) => root,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(unavailable(error)),
    };
    let dataset_name = Domain::Sessions.dataset_name();
    let dataset_file =
        match fsops::open_at_no_follow(&probe, &dataset_name, libc::O_RDONLY | libc::O_NONBLOCK) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(unavailable(error)),
        };
    if !dataset_file.metadata().map_err(unavailable)?.is_file() {
        return Err(invalid("invalid session alias dataset"));
    }
    drop(dataset_file);
    drop(probe);

    let root = Files::open(root)?;
    let dataset = read::<Dataset>(&root.0, &dataset_name)?
        .ok_or_else(|| invalid("session alias dataset disappeared during lookup"))?;
    validate_alias_metadata(&dataset)?;
    let Some(relative) = dataset.aliases.get(legacy_path) else {
        return Ok(None);
    };
    let record = dataset
        .files
        .get(relative)
        .ok_or_else(|| invalid("session alias references missing record"))?;
    validate_record(record, Domain::Sessions)?;
    if relative != &record.relative_path
        || legacy_path != relative && !record.aliases.iter().any(|alias| alias == legacy_path)
        || dataset.session_ids.get(&record.id) != Some(relative)
    {
        return Err(invalid("session alias identity differs"));
    }
    let mut file = open_file(&root.0, relative)?;
    if file.metadata().map_err(unavailable)?.len() > Domain::Sessions.max_file()
        || session_metadata(&mut file)?.0 != record.id
    {
        return Err(invalid("current session alias target identity differs"));
    }
    Ok(Some(relative.clone()))
}

fn validate_alias_metadata(dataset: &Dataset) -> Result<()> {
    identity(&dataset.migration_id)?;
    identity(&dataset.snapshot.source_identity)?;
    identity(&dataset.snapshot.generation)?;
    if dataset.version != 1
        || dataset.files.len() > MAX_FILES
        || dataset.snapshot.domains.len() != 1
        || dataset
            .snapshot
            .domains
            .get(Domain::Sessions.name())
            .is_none_or(|schema| {
                schema.schema_family != Domain::Sessions.family() || schema.schema_version != 1
            })
    {
        return Err(invalid("incompatible session alias dataset"));
    }
    for (path, record) in &dataset.files {
        validate_record(record, Domain::Sessions)?;
        if path != &record.relative_path {
            return Err(invalid("session alias file key differs"));
        }
    }
    let (aliases, ids) = mappings(&dataset.files, Domain::Sessions)?;
    if aliases != dataset.aliases
        || ids != dataset.session_ids
        || digest(&dataset.files)? != dataset.snapshot.domains[Domain::Sessions.name()].digest
    {
        return Err(invalid("session alias metadata digest/closure differs"));
    }
    Ok(())
}

fn components(relative: &str) -> Result<Vec<&str>> {
    if relative.is_empty()
        || relative.len() > 4096
        || relative.contains('\\')
        || relative.chars().any(char::is_control)
    {
        return Err(invalid("invalid journal/artifact reference"));
    }
    let path = Path::new(relative);
    if path
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(invalid("journal/artifact reference escapes root"));
    }
    let names: Vec<_> = relative.split('/').collect();
    if names.iter().any(|name| {
        name.is_empty()
            || *name == "."
            || *name == ".."
            || name.starts_with(".migration-")
            || name.starts_with(".dataset-")
            || *name == ".otto"
    }) {
        return Err(invalid("reserved or escaping journal/artifact reference"));
    }
    Ok(names)
}

fn parent(root: &fsops::Dir, relative: &str, create: bool) -> Result<(fsops::Dir, String)> {
    let names = components(relative)?;
    let mut dir = fsops::open_dir_at_no_follow(root, ".").map_err(unavailable)?;
    for name in &names[..names.len() - 1] {
        if create {
            match fsops::mkdir_at(&dir, name, 0o700) {
                Ok(()) => sync_dir(&dir)?,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
                Err(error) => return Err(unavailable(error)),
            }
        }
        dir = fsops::open_dir_at_no_follow(&dir, name).map_err(unavailable)?;
    }
    Ok((dir, names.last().unwrap().to_string()))
}

fn open_file(root: &fsops::Dir, relative: &str) -> Result<File> {
    let (dir, name) = parent(root, relative, false)?;
    let file = fsops::open_at_no_follow(&dir, &name, libc::O_RDONLY | libc::O_NONBLOCK)
        .map_err(unavailable)?;
    if !file.metadata().map_err(unavailable)?.is_file() {
        return Err(invalid("migration input is not a regular file"));
    }
    Ok(file)
}

fn sync_dir(dir: &fsops::Dir) -> Result<()> {
    fsops::open_at_no_follow(dir, ".", libc::O_RDONLY | libc::O_DIRECTORY)
        .and_then(|file| file.sync_all())
        .map_err(unavailable)
}

fn file_hash(file: &mut File) -> Result<String> {
    file.seek(SeekFrom::Start(0)).map_err(unavailable)?;
    let mut hash = Sha256::new();
    let mut buffer = [0; IO_BUFFER];
    loop {
        let read = file.read(&mut buffer).map_err(unavailable)?;
        if read == 0 {
            break;
        }
        hash.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn validate_record(record: &FileRecord, domain: Domain) -> Result<()> {
    components(&record.relative_path)?;
    identity(&record.id)?;
    if record.domain != domain
        || record.len > domain.max_file()
        || record.sha256.len() != 64
        || !record.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
        || record.aliases.len() > 4
    {
        return Err(invalid("invalid journal/artifact metadata"));
    }
    for alias in &record.aliases {
        if alias.is_empty() || alias.len() > 4096 || alias.chars().any(char::is_control) {
            return Err(invalid("invalid journal alias"));
        }
    }
    match domain {
        Domain::Sessions if !record.relative_path.ends_with(".jsonl") => {
            return Err(invalid("session input must be a Pi journal"));
        }
        Domain::Artifacts
            if !record.relative_path.starts_with("artifacts/")
                || record.parent_session.is_some()
                || record.leaf_id.is_some() =>
        {
            return Err(invalid(
                "artifact input must belong to the artifacts namespace",
            ));
        }
        _ => (),
    }
    Ok(())
}

fn session_metadata(file: &mut File) -> Result<(String, Option<String>, Option<String>)> {
    // The ordinary read-only recovery codec preserves a recognized incomplete
    // final tail; migration copies those bytes, and never invokes write repair.
    let decoded = super::session::store::decode_pi_file_read_only(file)
        .map_err(|_| invalid("invalid Pi journal"))?;
    let state = super::session::store::resolve_pi_store_state(&decoded)
        .map_err(|_| invalid("invalid session replay/operation ledger"))?;
    Ok((
        decoded.header.id,
        decoded.header.parent_session,
        state.leaf_id,
    ))
}

fn digest(records: &BTreeMap<String, FileRecord>) -> Result<DomainDigest> {
    let mut hash = Sha256::new();
    let mut bytes = 0u64;
    for record in records.values() {
        let encoded = serde_json::to_vec(record).map_err(|_| invalid("file metadata encoding"))?;
        hash.update((encoded.len() as u64).to_be_bytes());
        hash.update(encoded);
        bytes = bytes
            .checked_add(record.len)
            .ok_or_else(|| invalid("blob byte count overflow"))?;
    }
    Ok(DomainDigest {
        records: records.len() as u64,
        blob_bytes: bytes,
        sha256: format!("{:x}", hash.finalize()),
    })
}

fn mappings(
    records: &BTreeMap<String, FileRecord>,
    domain: Domain,
) -> Result<(BTreeMap<String, String>, BTreeMap<String, String>)> {
    let mut aliases = BTreeMap::new();
    let mut ids = BTreeMap::new();
    for record in records.values() {
        if ids
            .insert(record.id.clone(), record.relative_path.clone())
            .is_some()
        {
            return Err(invalid("duplicate logical file identity"));
        }
        for alias in &record.aliases {
            if aliases
                .insert(alias.clone(), record.relative_path.clone())
                .is_some()
            {
                return Err(invalid("ambiguous legacy file alias"));
            }
        }
        if aliases
            .insert(record.relative_path.clone(), record.relative_path.clone())
            .is_some_and(|previous| previous != record.relative_path)
        {
            return Err(invalid("legacy alias collides with logical file reference"));
        }
    }
    if domain == Domain::Sessions {
        for record in records.values() {
            if record
                .parent_session
                .as_ref()
                .is_some_and(|parent| !aliases.contains_key(parent))
            {
                return Err(invalid(
                    "parent session outside migration reference closure",
                ));
            }
        }
    } else {
        ids.clear();
    }
    Ok((aliases, ids))
}

struct LockedFile {
    record: FileRecord,
    file: File,
}

pub struct JournalSource {
    files: Arc<Mutex<Option<Vec<LockedFile>>>>,
    fence: Arc<dyn FreezeGuard>,
    snapshot: FrozenSnapshot,
    domain: Domain,
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

fn cursor(cursor: &str) -> Result<(usize, Option<u64>)> {
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct StageHeader {
    migration_id: String,
    destination_identity: String,
    max_chunk_bytes: usize,
    source: FrozenSnapshot,
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
struct Envelope {
    migration_id: String,
    domain: String,
    receipt: ChunkReceipt,
    chunk: Chunk,
}

fn chunk_name(domain: Domain, position: &str) -> String {
    format!(
        ".chunk-{}-{:x}.json",
        domain.name(),
        Sha256::digest(position.as_bytes())
    )
}

fn envelope(
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
    files: Files,
    root_path: PathBuf,
    domain: Domain,
    identity: String,
    closed: Arc<AtomicBool>,
    owned: Arc<Mutex<()>>,
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

fn checked_header(dir: &fsops::Dir, domain: Domain, manifest: &Manifest) -> Result<()> {
    let header: StageHeader =
        read(dir, &domain.stage_name())?.ok_or_else(|| invalid("journal stage missing"))?;
    if header != StageHeader::new(manifest) {
        return Err(conflict("journal stage source identity differs"));
    }
    Ok(())
}

fn fresh_namespace(dir: &fsops::Dir) -> Result<()> {
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

fn reference_records(
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

fn checked_record(record: &Record, domain: Domain) -> Result<FileRecord> {
    let metadata: FileRecord = serde_json::from_slice(&record.payload)
        .map_err(|_| invalid("invalid logical file record"))?;
    validate_record(&metadata, domain)?;
    if record.id != metadata.relative_path || record.revision != metadata.sha256 {
        return Err(invalid("logical file identity/hash mismatch"));
    }
    Ok(metadata)
}

fn verify_dataset(
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

struct PendingFile {
    record: FileRecord,
    dir: fsops::Dir,
    temporary: String,
    file: File,
    offset: u64,
    hash: Sha256,
}

impl Drop for PendingFile {
    fn drop(&mut self) {
        let _ = fsops::unlink_at(&self.dir, &self.temporary);
    }
}

fn finish(pending: Option<PendingFile>) -> Result<()> {
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

fn reconstruct(dir: &fsops::Dir, domain: Domain, manifest: &Manifest) -> Result<Dataset> {
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
                    super::session::Store::read_entries(root.join(path)).map_err(|_| {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::migration::group::{GroupSource, GroupStage, ReferenceClosure};

    struct Fence;
    impl FreezeGuard for Fence {
        fn validate(&self, generation: &str) -> Result<()> {
            if generation == "freeze-1" {
                Ok(())
            } else {
                Err(conflict("different freeze"))
            }
        }
    }
    struct Closure;
    #[async_trait]
    impl ReferenceClosure for Closure {
        async fn validate(&self, _: &Manifest) -> Result<()> {
            Ok(())
        }
    }
    struct MissingReference;
    #[async_trait]
    impl ReferenceClosure for MissingReference {
        async fn validate(&self, _: &Manifest) -> Result<()> {
            Err(invalid("missing cross-domain reference"))
        }
    }

    fn tempdir() -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt;
        tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir_in(std::env::temp_dir().canonicalize().unwrap())
            .unwrap()
    }
    fn journal(id: &str, parent: Option<&str>) -> Vec<u8> {
        let mut header = serde_json::json!({ "type":"session", "version":3, "id":id, "timestamp":"2026-10-06T00:00:00Z", "cwd":"/workspace", "futureHeader":{"keep":true} });
        if let Some(parent) = parent {
            header["parentSession"] = serde_json::json!(parent);
        }
        let entry = serde_json::json!({ "type":"future_entry", "id":"00000001", "parentId":null, "timestamp":"2026-10-06T00:00:01Z", "unknown":{"verbatim":"yes"} });
        format!("{header}\n{entry}\n").into_bytes()
    }
    fn source(root: &Path, domain: Domain, inputs: Vec<JournalInput>) -> JournalSource {
        JournalSource::open(
            root,
            domain,
            inputs,
            "source".into(),
            "freeze-1".into(),
            Arc::new(Fence),
        )
        .unwrap()
    }
    fn input(path: &str) -> JournalInput {
        JournalInput {
            relative_path: path.into(),
            legacy_path: None,
            artifact_id: None,
        }
    }
    async fn manifest(source: &dyn SnapshotExport) -> Manifest {
        let snapshot = source.descriptor().await.unwrap();
        Manifest {
            format_version: 1,
            max_chunk_bytes: 8192,
            migration_id: "move-1".into(),
            destination_identity: "destination".into(),
            expected_route_generation: "route-1".into(),
            progress: snapshot
                .domains
                .keys()
                .map(|domain| {
                    (
                        domain.clone(),
                        Progress {
                            cursor: String::new(),
                            complete: false,
                            chunks: vec![],
                        },
                    )
                })
                .collect(),
            source: snapshot,
            destination_write_started: true,
            sealed: false,
            published_generation: None,
            rolled_back: false,
        }
    }
    async fn copy(source: &dyn SnapshotExport, stage: &dyn StageImport, manifest: &mut Manifest) {
        stage.begin(manifest).await.unwrap();
        for domain in manifest.source.domains.keys().cloned().collect::<Vec<_>>() {
            let mut position = String::new();
            loop {
                let chunk = source
                    .read_chunk(&domain, &position, manifest.max_chunk_bytes)
                    .await
                    .unwrap();
                let hash = chunk_hash(&chunk, manifest.max_chunk_bytes).unwrap();
                let receipt = stage
                    .apply_chunk(&manifest.migration_id, &domain, &chunk, &hash)
                    .await
                    .unwrap();
                // Stable idempotency reuses the same atomic payload+receipt file.
                assert_eq!(
                    stage
                        .apply_chunk(&manifest.migration_id, &domain, &chunk, &hash)
                        .await
                        .unwrap(),
                    receipt
                );
                assert_eq!(
                    stage
                        .chunk_receipt(&manifest.migration_id, &domain, &position)
                        .await
                        .unwrap(),
                    Some(receipt.clone())
                );
                let progress = manifest.progress.get_mut(&domain).unwrap();
                progress.chunks.push(receipt);
                if let Some(next) = chunk.next_cursor {
                    position = next;
                    progress.cursor = position.clone();
                } else {
                    progress.complete = true;
                    break;
                }
            }
        }
    }

    #[tokio::test]
    async fn legacy_public_session_root_private_workspace_transfers_without_permission_changes() {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let from = tempdir();
        let to = tempdir();
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(from.path().join("workspace"))
            .unwrap();
        let bytes = journal("session-id", None);
        std::fs::write(from.path().join("workspace/session.jsonl"), &bytes).unwrap();
        std::fs::set_permissions(from.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        super::super::session::Store::read_entries(from.path().join("workspace/session.jsonl"))
            .unwrap();
        let source = source(
            from.path(),
            Domain::Sessions,
            vec![input("workspace/session.jsonl")],
        );
        let mut manifest = manifest(&source).await;
        let stage = JournalStage::new(to.path(), Domain::Sessions, "destination".into()).unwrap();
        copy(&source, &stage, &mut manifest).await;
        stage.seal(&manifest).await.unwrap();
        assert_eq!(
            std::fs::read(to.path().join("workspace/session.jsonl")).unwrap(),
            bytes
        );
        assert_eq!(
            std::fs::metadata(from.path()).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(
            std::fs::metadata(from.path().join("workspace"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert!(JournalStage::new(from.path(), Domain::Sessions, "destination".into()).is_err());
        assert!(super::super::migration::LocalJournal::open(from.path()).is_err());
        source.close().await.unwrap();
    }

    #[tokio::test]
    async fn legacy_root_without_metadata_keeps_permissions_but_present_dataset_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempdir();
        std::fs::write(
            root.path().join("session.jsonl"),
            journal("session-id", None),
        )
        .unwrap();
        let source = source(root.path(), Domain::Sessions, vec![input("session.jsonl")]);
        let snapshot = source.descriptor().await.unwrap();
        let record = source.files.lock().unwrap().as_ref().unwrap()[0]
            .record
            .clone();
        let files = BTreeMap::from([(record.relative_path.clone(), record)]);
        let (aliases, session_ids) = mappings(&files, Domain::Sessions).unwrap();
        let dataset = Dataset {
            version: 1,
            migration_id: "move-1".into(),
            snapshot,
            files,
            aliases,
            session_ids,
        };
        source.close().await.unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            read_dataset_alias(root.path(), "/legacy/session.jsonl").unwrap(),
            None
        );
        assert_eq!(
            super::super::session::Store::read_entries(root.path().join("session.jsonl"))
                .unwrap()
                .0
                .len(),
            1
        );
        assert_eq!(
            std::fs::metadata(root.path()).unwrap().permissions().mode() & 0o777,
            0o755
        );
        std::fs::write(
            root.path().join(Domain::Sessions.dataset_name()),
            serde_json::to_vec(&dataset).unwrap(),
        )
        .unwrap();
        assert!(
            read_dataset_alias(root.path(), "/legacy/session.jsonl")
                .unwrap_err()
                .contains("mode 0700")
        );
        assert!(read_dataset_alias(&root.path().join(".."), "/legacy/session.jsonl").is_err());
    }

    #[tokio::test]
    async fn exact_unknown_pi_bytes_parent_aliases_and_incomplete_tail_survive_reopen() {
        let from = tempdir();
        let to = tempdir();
        let parent_path = from
            .path()
            .join("parent.jsonl")
            .to_string_lossy()
            .into_owned();
        let parent_bytes = journal("parent-id", None);
        let mut child_bytes = journal("child-id", Some(&parent_path));
        child_bytes.extend_from_slice(b"{\"type\":\"message\"");
        std::fs::write(from.path().join("parent.jsonl"), &parent_bytes).unwrap();
        std::fs::write(from.path().join("child.jsonl"), &child_bytes).unwrap();
        let source = source(
            from.path(),
            Domain::Sessions,
            vec![input("parent.jsonl"), input("child.jsonl")],
        );
        let mut manifest = manifest(&source).await;
        let stage = JournalStage::new(to.path(), Domain::Sessions, "destination".into()).unwrap();
        copy(&source, &stage, &mut manifest).await;
        stage.verify(&manifest).await.unwrap();
        assert!(stage.dataset().await.is_err());
        let references = stage.reference_dataset(&manifest).await.unwrap();
        assert_eq!(references.aliases[&parent_path], "parent.jsonl");
        assert_eq!(references.session_ids["child-id"], "child.jsonl");
        stage.seal(&manifest).await.unwrap();
        assert_eq!(
            std::fs::read(to.path().join("parent.jsonl")).unwrap(),
            parent_bytes
        );
        assert_eq!(
            std::fs::read(to.path().join("child.jsonl")).unwrap(),
            child_bytes
        );
        assert_eq!(
            read_dataset_alias(to.path(), &parent_path).unwrap(),
            Some(PathBuf::from("parent.jsonl"))
        );
        stage.close().await.unwrap();
        assert!(stage.verify(&manifest).await.is_err());
        let reopened =
            JournalStage::new(to.path(), Domain::Sessions, "destination".into()).unwrap();
        reopened.verify(&manifest).await.unwrap();
        let (entries, _) =
            super::super::session::Store::read_entries(to.path().join("child.jsonl")).unwrap();
        assert_eq!(entries[0].type_name, "future_entry");
        source.close().await.unwrap();
        assert!(source.descriptor().await.is_err());
        let mut append = std::fs::OpenOptions::new()
            .append(true)
            .open(to.path().join("parent.jsonl"))
            .unwrap();
        append.write_all(b"{\"type\":\"future_entry\",\"id\":\"00000002\",\"parentId\":\"00000001\",\"timestamp\":\"2026-10-06T00:00:02Z\"}\n").unwrap();
        assert_eq!(
            read_dataset_alias(to.path(), &parent_path).unwrap(),
            Some(PathBuf::from("parent.jsonl"))
        );
        std::fs::write(
            to.path().join("parent.jsonl"),
            journal("different-session", None),
        )
        .unwrap();
        assert!(read_dataset_alias(to.path(), &parent_path).is_err());
    }

    #[tokio::test]
    async fn atomic_chunk_survives_new_stage_instance_before_checkpoint_and_blob_stream_is_bounded()
    {
        let from = tempdir();
        let to = tempdir();
        std::fs::create_dir(from.path().join("artifacts")).unwrap();
        let bytes: Vec<_> = (0..100_000).map(|index| (index % 256) as u8).collect();
        std::fs::write(from.path().join("artifacts/output.bin"), &bytes).unwrap();
        let source = source(
            from.path(),
            Domain::Artifacts,
            vec![JournalInput {
                relative_path: "artifacts/output.bin".into(),
                legacy_path: None,
                artifact_id: Some("artifact-1".into()),
            }],
        );
        let mut manifest = manifest(&source).await;
        let first = JournalStage::new(to.path(), Domain::Artifacts, "destination".into()).unwrap();
        first.begin(&manifest).await.unwrap();
        let chunk = source.read_chunk("artifacts", "", 8192).await.unwrap();
        let hash = chunk_hash(&chunk, 8192).unwrap();
        first
            .apply_chunk("move-1", "artifacts", &chunk, &hash)
            .await
            .unwrap();
        first.close().await.unwrap();
        let reopened =
            JournalStage::new(to.path(), Domain::Artifacts, "destination".into()).unwrap();
        assert!(
            reopened
                .chunk_receipt("move-1", "artifacts", "")
                .await
                .unwrap()
                .is_some()
        );
        copy(&source, &reopened, &mut manifest).await;
        assert!(manifest.progress["artifacts"].chunks.len() > 2);
        reopened.verify(&manifest).await.unwrap();
        reopened.seal(&manifest).await.unwrap();
        assert_eq!(
            std::fs::read(to.path().join("artifacts/output.bin")).unwrap(),
            bytes
        );
        let last = source.read_chunk("artifacts", "0:0", 8192).await.unwrap();
        assert!(serde_json::to_vec(&last).unwrap().len() <= 8192);
        assert!(
            source
                .read_chunk("artifacts", &format!("{}:0", usize::MAX), 8192)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn owned_file_lock_symlink_escape_and_missing_parent_fail_closed() {
        use std::os::unix::fs::symlink;
        let root = tempdir();
        let outside = tempdir();
        std::fs::write(root.path().join("parent.jsonl"), journal("parent-id", None)).unwrap();
        let writer =
            fsops::open_no_follow(&root.path().join("parent.jsonl"), libc::O_RDWR).unwrap();
        posix::try_lock_exclusive(&writer).unwrap();
        assert!(
            JournalSource::open(
                root.path(),
                Domain::Sessions,
                vec![input("parent.jsonl")],
                "source".into(),
                "freeze-1".into(),
                Arc::new(Fence)
            )
            .is_err()
        );
        drop(writer);
        std::fs::write(
            outside.path().join("other.jsonl"),
            journal("other-id", None),
        )
        .unwrap();
        symlink(outside.path(), root.path().join("escape")).unwrap();
        assert!(
            JournalSource::open(
                root.path(),
                Domain::Sessions,
                vec![input("escape/other.jsonl")],
                "source".into(),
                "freeze-1".into(),
                Arc::new(Fence)
            )
            .is_err()
        );
        assert!(
            JournalSource::open(
                root.path(),
                Domain::Sessions,
                vec![input("../other.jsonl")],
                "source".into(),
                "freeze-1".into(),
                Arc::new(Fence)
            )
            .is_err()
        );
        std::fs::write(
            root.path().join("child.jsonl"),
            journal("child-id", Some("/missing/parent.jsonl")),
        )
        .unwrap();
        assert!(
            JournalSource::open(
                root.path(),
                Domain::Sessions,
                vec![input("child.jsonl")],
                "source".into(),
                "freeze-1".into(),
                Arc::new(Fence)
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn corrupted_receipts_or_blob_ranges_never_verify_or_seal() {
        let from = tempdir();
        let to = tempdir();
        std::fs::write(
            from.path().join("session.jsonl"),
            journal("session-id", None),
        )
        .unwrap();
        let source = source(from.path(), Domain::Sessions, vec![input("session.jsonl")]);
        let mut manifest = manifest(&source).await;
        let stage = JournalStage::new(to.path(), Domain::Sessions, "destination".into()).unwrap();
        copy(&source, &stage, &mut manifest).await;
        std::fs::write(to.path().join(chunk_name(Domain::Sessions, "")), b"corrupt").unwrap();
        assert!(stage.chunk_receipt("move-1", "sessions", "").await.is_err());
        assert!(stage.verify(&manifest).await.is_err());
        assert!(stage.seal(&manifest).await.is_err());
        assert!(stage.dataset().await.is_err());
    }

    #[tokio::test]
    async fn explicit_abort_after_failed_closure_restores_unchanged_source_without_activating_stage()
     {
        use super::super::migration::{
            LocalCutover, LocalJournal, PreparedCutover, RouteGroup, StoppedOwners,
        };
        struct Stopped;
        impl StoppedOwners for Stopped {
            fn verify_stopped(&self) -> Result<()> {
                Ok(())
            }
        }
        let from = tempdir();
        let to = tempdir();
        let controls = tempdir();
        let bytes = journal("session-id", None);
        std::fs::write(from.path().join("session.jsonl"), &bytes).unwrap();
        let routes = |root: &Path| {
            BTreeMap::from([(
                "sessions".into(),
                StorageRoute {
                    backend: "posix".into(),
                    endpoint: root.to_string_lossy().into_owned(),
                },
            )])
        };
        let source_routes = routes(from.path());
        let destination_routes = routes(to.path());
        let control = LocalCutover::open(
            controls.path(),
            RouteGroup {
                identity: "source".into(),
                generation: "route-1".into(),
                writer_epoch: "epoch-1".into(),
                domains: source_routes.clone(),
            },
        )
        .unwrap();
        control
            .freeze(
                PreparedCutover {
                    migration_id: "move-1".into(),
                    source_identity: "source".into(),
                    destination_identity: "destination".into(),
                    source_generation: "freeze-1".into(),
                    expected_route_generation: "route-1".into(),
                    source_routes,
                    destination_routes,
                },
                Arc::new(Stopped),
            )
            .await
            .unwrap();
        let source = JournalSource::open(
            from.path(),
            Domain::Sessions,
            vec![input("session.jsonl")],
            "source".into(),
            "freeze-1".into(),
            Arc::new(control.clone()),
        )
        .unwrap();
        let mut manifest = manifest(&source).await;
        let journal = LocalJournal::open(controls.path()).unwrap();
        let revision = journal.save(None, &manifest).await.unwrap();
        let stage: Arc<dyn StageImport> =
            Arc::new(JournalStage::new(to.path(), Domain::Sessions, "destination".into()).unwrap());
        let group = GroupStage::new(
            BTreeMap::from([("sessions".into(), stage)]),
            Arc::new(MissingReference),
        )
        .unwrap();
        copy(&source, &group, &mut manifest).await;
        assert!(group.verify(&manifest).await.is_err());
        journal.save(Some(revision), &manifest).await.unwrap();
        source.close().await.unwrap();
        group.close().await.unwrap();
        control.abort_planned("move-1").await.unwrap();
        control.abort_planned("move-1").await.unwrap();
        control.ensure_writable("source", "route-1").await.unwrap();
        assert_eq!(
            std::fs::read(from.path().join("session.jsonl")).unwrap(),
            bytes
        );
        assert!(control.receipt("move-1").await.unwrap().is_none());
        assert!(!to.path().join(Domain::Sessions.dataset_name()).exists());
        let aborted = journal.load("move-1").await.unwrap().unwrap();
        assert!(aborted.manifest.rolled_back && aborted.manifest.destination_write_started);
        assert!(
            journal
                .save(Some(aborted.revision), &manifest)
                .await
                .is_err()
        );
        assert!(control.publish(&manifest).await.is_err());
    }

    #[tokio::test]
    async fn group_composes_native_domains_and_rejects_unverified_cross_store_closure() {
        let session_root = tempdir();
        let artifact_root = tempdir();
        let session_to = tempdir();
        let artifact_to = tempdir();
        std::fs::write(
            session_root.path().join("session.jsonl"),
            journal("session-id", None),
        )
        .unwrap();
        std::fs::create_dir(artifact_root.path().join("artifacts")).unwrap();
        std::fs::write(artifact_root.path().join("artifacts/value"), b"artifact").unwrap();
        let sessions: Arc<dyn SnapshotExport> = Arc::new(source(
            session_root.path(),
            Domain::Sessions,
            vec![input("session.jsonl")],
        ));
        let artifacts: Arc<dyn SnapshotExport> = Arc::new(source(
            artifact_root.path(),
            Domain::Artifacts,
            vec![JournalInput {
                relative_path: "artifacts/value".into(),
                legacy_path: None,
                artifact_id: Some("artifact-id".into()),
            }],
        ));
        let source = GroupSource::new(vec![sessions, artifacts]).await.unwrap();
        let mut manifest = manifest(&source).await;
        let session_stage: Arc<dyn StageImport> = Arc::new(
            JournalStage::new(session_to.path(), Domain::Sessions, "destination".into()).unwrap(),
        );
        let artifact_stage: Arc<dyn StageImport> = Arc::new(
            JournalStage::new(artifact_to.path(), Domain::Artifacts, "destination".into()).unwrap(),
        );
        let destinations = BTreeMap::from([
            ("sessions".into(), session_stage.clone()),
            ("artifacts".into(), artifact_stage.clone()),
        ]);
        let missing = GroupStage::new(
            BTreeMap::from([("sessions".into(), session_stage.clone())]),
            Arc::new(Closure),
        )
        .unwrap();
        assert!(missing.validate_plan(&manifest).await.is_err());
        let mut wrong_schema = manifest.clone();
        wrong_schema
            .source
            .domains
            .get_mut("sessions")
            .unwrap()
            .schema_family = "unsupported".into();
        let rejected = GroupStage::new(destinations.clone(), Arc::new(MissingReference)).unwrap();
        assert!(rejected.validate_plan(&wrong_schema).await.is_err());
        copy(&source, &rejected, &mut manifest).await;
        assert!(rejected.verify(&manifest).await.is_err());
        assert!(rejected.seal(&manifest).await.is_err());
        assert!(
            !session_to
                .path()
                .join(Domain::Sessions.dataset_name())
                .exists()
        );
        let accepted = GroupStage::new(destinations, Arc::new(Closure)).unwrap();
        assert_eq!(accepted.verify(&manifest).await.unwrap().len(), 2);
        accepted.seal(&manifest).await.unwrap();
        accepted.close().await.unwrap();
        let mut scoped = manifest.clone();
        scoped
            .source
            .domains
            .retain(|domain, _| domain == "sessions");
        scoped.progress.retain(|domain, _| domain == "sessions");
        assert!(session_stage.verify(&scoped).await.is_err());
        source.close().await.unwrap();
        assert!(source.descriptor().await.is_err());
    }

    #[tokio::test]
    async fn group_refuses_duplicate_source_owners_and_different_identity() {
        let root = tempdir();
        std::fs::write(
            root.path().join("session.jsonl"),
            journal("session-id", None),
        )
        .unwrap();
        let duplicate: Arc<dyn SnapshotExport> = Arc::new(source(
            root.path(),
            Domain::Sessions,
            vec![input("session.jsonl")],
        ));
        assert!(
            GroupSource::new(vec![duplicate.clone(), duplicate.clone()])
                .await
                .is_err()
        );
        assert!(duplicate.descriptor().await.is_err()); // Failed construction drains owners.
        let first: Arc<dyn SnapshotExport> =
            Arc::new(source(root.path(), Domain::Sessions, vec![]));
        let other: Arc<dyn SnapshotExport> = Arc::new(
            JournalSource::open(
                root.path(),
                Domain::Sessions,
                vec![],
                "different-source".into(),
                "freeze-1".into(),
                Arc::new(Fence),
            )
            .unwrap(),
        );
        assert!(GroupSource::new(vec![first, other]).await.is_err());
    }
}
