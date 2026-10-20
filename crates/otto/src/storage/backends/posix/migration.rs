//! Local, stopped-process migration control. Shared/distributed ownership is
//! deliberately unsupported. Legacy supervisors must prevent old writer restart.
use std::{
    collections::BTreeMap,
    fs::File,
    io::{Read, Write},
    os::unix::fs::PermissionsExt,
    path::{Component, Path},
    sync::Arc,
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};

use crate::{
    fsops,
    storage::migration::{
        Cutover, Error, FORMAT_VERSION, JournalEntry, Manifest, MigrationJournal, Result,
        StorageRoute,
    },
};

const AUTHORITY: &str = "authority.json";
const ADMISSION: &str = "writer-admission.lock";
const MAX_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone)]
pub(super) struct Files(pub(super) Arc<fsops::Dir>);

impl Files {
    pub(super) fn open(root: &Path) -> Result<Self> {
        let files = Self::pin_read_only(root)?;
        let root = fsops::open_at_no_follow(&files.0, ".", libc::O_RDONLY | libc::O_DIRECTORY)
            .map_err(unavailable)?;
        if root.metadata().map_err(unavailable)?.permissions().mode() & 0o777 != 0o700 {
            return Err(invalid("migration control directory must have mode 0700"));
        }
        Ok(files)
    }

    /// Source export can read historical directory permissions. This performs
    /// identical pinned/no-follow traversal and never changes permissions.
    pub(super) fn pin_read_only(root: &Path) -> Result<Self> {
        if !root.is_absolute() {
            return Err(invalid("migration root must be absolute"));
        }
        let mut dir = fsops::open_dir_no_follow(Path::new("/")).map_err(unavailable)?;
        for component in root.components() {
            match component {
                Component::RootDir => (),
                Component::Normal(name) => {
                    let name = name
                        .to_str()
                        .ok_or_else(|| invalid("non-Unicode migration root"))?;
                    dir = fsops::open_dir_at_no_follow(&dir, name).map_err(unavailable)?;
                }
                _ => return Err(invalid("migration root contains traversal")),
            }
        }
        Ok(Self(Arc::new(dir)))
    }

    pub(super) fn locked<T>(&self, operation: impl FnOnce(&fsops::Dir) -> Result<T>) -> Result<T> {
        // A dup shares flock's open-file description. Open '.' anew so two
        // concurrent calls on the SAME adapter still serialize correctly.
        let dir = fsops::open_dir_at_no_follow(&self.0, ".").map_err(unavailable)?;
        let _lock = fsops::lock_directory(&dir).map_err(unavailable)?;
        operation(&dir)
    }

    pub(super) async fn run<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&fsops::Dir) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let files = self.clone();
        tokio::task::spawn_blocking(move || files.locked(operation))
            .await
            .map_err(unavailable)?
    }
}

pub(super) fn invalid(message: impl Into<String>) -> Error {
    Error::Invalid(message.into())
}
pub(super) fn unavailable(error: impl std::fmt::Display) -> Error {
    Error::Unavailable(error.to_string())
}
pub(super) fn conflict(message: impl Into<String>) -> Error {
    Error::Conflict(message.into())
}

fn admission(dir: &fsops::Dir) -> Result<File> {
    let file = fsops::create_at_no_follow(dir, ADMISSION, libc::O_RDWR | libc::O_NONBLOCK, 0o600)
        .map_err(unavailable)?;
    if !file.metadata().map_err(unavailable)?.is_file() {
        return Err(invalid("invalid writer admission file"));
    }
    Ok(file)
}

pub(super) fn identity(value: &str) -> Result<()> {
    if value.is_empty() || value.len() > 1024 || value.chars().any(char::is_control) {
        return Err(invalid("invalid migration identity"));
    }
    Ok(())
}

fn journal_name(id: &str) -> Result<String> {
    identity(id)?;
    Ok(format!(
        "migration-{:x}.json",
        Sha256::digest(id.as_bytes())
    ))
}

pub(super) fn read<T: DeserializeOwned>(dir: &fsops::Dir, name: &str) -> Result<Option<T>> {
    let mut file = match fsops::open_at_no_follow(dir, name, libc::O_RDONLY | libc::O_NONBLOCK) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(unavailable(error)),
    };
    let metadata = file.metadata().map_err(unavailable)?;
    if !metadata.is_file() || metadata.len() > MAX_BYTES {
        return Err(invalid("invalid migration control file"));
    }
    let mut bytes = Vec::new();
    (&mut file)
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(unavailable)?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(invalid("migration control file exceeds limit"));
    }
    let value =
        serde_json::from_slice(&bytes).map_err(|_| invalid("corrupt migration control file"))?;
    // Reconciliation after a failed directory sync must establish durability,
    // not merely observe a rename still visible in this process's page cache.
    file.sync_all().map_err(unavailable)?;
    fsops::open_at_no_follow(dir, ".", libc::O_RDONLY | libc::O_DIRECTORY)
        .and_then(|file| file.sync_all())
        .map_err(unavailable)?;
    Ok(Some(value))
}

pub(super) fn publish<T: Serialize>(dir: &fsops::Dir, name: &str, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec(value).map_err(|_| invalid("migration encoding failed"))?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(invalid("migration control file exceeds limit"));
    }
    let temporary = format!(
        ".migration-{}",
        crate::auth::random_hex::<16>().map_err(unavailable)?
    );
    let mut file =
        fsops::create_at_no_follow(dir, &temporary, libc::O_WRONLY | libc::O_EXCL, 0o600)
            .map_err(unavailable)?;
    let written = file.write_all(&bytes).and_then(|()| file.sync_all());
    if let Err(error) = written {
        let _ = fsops::unlink_at(dir, &temporary);
        return Err(unavailable(error));
    }
    if let Err(error) = fsops::rename_at(dir, &temporary, name) {
        let _ = fsops::unlink_at(dir, &temporary);
        return Err(unavailable(error));
    }
    // Once renamed, a failed directory sync is an unknown commit, never rollback.
    fsops::open_at_no_follow(dir, ".", libc::O_RDONLY | libc::O_DIRECTORY)
        .and_then(|file| file.sync_all())
        .map_err(|_| Error::CommitUnknown)
}

#[derive(Clone)]
pub struct LocalJournal(Files);

impl LocalJournal {
    /// The dedicated control directory already exists. Every ancestor is pinned
    /// without symlink traversal; root must already have owner-only permissions.
    pub fn open(root: &Path) -> Result<Self> {
        Ok(Self(Files::open(root)?))
    }
}

fn read_journal(dir: &fsops::Dir, id: &str) -> Result<Option<JournalEntry>> {
    let entry: Option<JournalEntry> = read(dir, &journal_name(id)?)?;
    if let Some(entry) = &entry
        && (entry.revision == 0
            || entry.manifest.migration_id != id
            || entry.manifest.format_version != FORMAT_VERSION)
    {
        return Err(invalid("incompatible migration checkpoint"));
    }
    Ok(entry)
}

#[async_trait]
impl MigrationJournal for LocalJournal {
    async fn load(&self, id: &str) -> Result<Option<JournalEntry>> {
        let id = id.to_owned();
        self.0.run(move |dir| read_journal(dir, &id)).await
    }
    async fn save(&self, expected: Option<u64>, manifest: &Manifest) -> Result<u64> {
        let manifest = manifest.clone();
        self.0
            .run(move |dir| {
                if manifest.format_version != FORMAT_VERSION {
                    return Err(invalid("unsupported migration version"));
                }
                let previous = read_journal(dir, &manifest.migration_id)?;
                if read::<Authority>(dir, AUTHORITY)?.is_some() {
                    let state = read_authority(dir)?;
                    if manifest
                        .published_generation
                        .as_ref()
                        .is_some_and(|generation| {
                            state
                                .publications
                                .get(&manifest.migration_id)
                                .is_none_or(|receipt| &receipt.generation != generation)
                        })
                    {
                        return Err(conflict(
                            "published checkpoint lacks matching authority receipt",
                        ));
                    }
                    if manifest.rolled_back
                        && state.publications.contains_key(&manifest.migration_id)
                    {
                        return Err(conflict("published migration cannot be aborted"));
                    }
                    let prepared = if previous.is_none() {
                        if state.publications.contains_key(&manifest.migration_id)
                            || state.rollbacks.contains_key(&manifest.migration_id)
                        {
                            return Err(conflict(
                                "migration no longer accepts an initial checkpoint",
                            ));
                        }
                        state.freeze.as_ref().ok_or_else(|| {
                            conflict("source is not frozen for initial checkpoint")
                        })?
                    } else if let Some(publication) = state.publications.get(&manifest.migration_id)
                    {
                        &publication.prepared
                    } else if let Some(rollback) = state.rollbacks.get(&manifest.migration_id) {
                        rollback
                    } else {
                        state
                            .freeze
                            .as_ref()
                            .ok_or_else(|| conflict("migration authority fence absent"))?
                    };
                    match_manifest(prepared, &manifest)?;
                }
                if let Some(previous) = &previous {
                    if previous.manifest == manifest {
                        return Ok(previous.revision);
                    }
                    let prior = &previous.manifest;
                    if prior.source != manifest.source
                        || prior.destination_identity != manifest.destination_identity
                        || prior.expected_route_generation != manifest.expected_route_generation
                        || prior.max_chunk_bytes != manifest.max_chunk_bytes
                        || prior.destination_write_started && !manifest.destination_write_started
                        || prior.sealed && !manifest.sealed
                        || prior.rolled_back
                        || prior.published_generation.is_some()
                            && prior.published_generation != manifest.published_generation
                        || prior.progress.iter().any(|(domain, progress)| {
                            manifest.progress.get(domain).is_none_or(|next| {
                                !next.chunks.starts_with(&progress.chunks)
                                    || progress.complete && !next.complete
                            })
                        })
                    {
                        return Err(conflict(
                            "migration checkpoint cannot rewind or change source",
                        ));
                    }
                }
                if previous.as_ref().map(|entry| entry.revision) != expected {
                    return Err(conflict("migration checkpoint revision"));
                }
                let revision = expected
                    .unwrap_or(0)
                    .checked_add(1)
                    .ok_or_else(|| invalid("migration revision exhausted"))?;
                publish(
                    dir,
                    &journal_name(&manifest.migration_id)?,
                    &JournalEntry { revision, manifest },
                )?;
                Ok(revision)
            })
            .await
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteGroup {
    pub identity: String,
    pub generation: String,
    pub writer_epoch: String,
    pub domains: BTreeMap<String, StorageRoute>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreparedCutover {
    pub migration_id: String,
    pub source_identity: String,
    pub destination_identity: String,
    pub source_generation: String,
    pub expected_route_generation: String,
    pub source_routes: BTreeMap<String, StorageRoute>,
    pub destination_routes: BTreeMap<String, StorageRoute>,
}

/// Verify actual backend/file ownership locks, never just a PID registry.
/// External supervision must additionally stop legacy writers and prevent
/// restart: this interface cannot establish absence of non-cooperating code.
pub trait StoppedOwners: Send + Sync {
    fn verify_stopped(&self) -> Result<()>;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Publication {
    prepared: PreparedCutover,
    generation: String,
    manifest_digest: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Authority {
    version: u32,
    active: RouteGroup,
    freeze: Option<PreparedCutover>,
    publications: BTreeMap<String, Publication>,
    rollbacks: BTreeMap<String, PreparedCutover>,
}

fn validate_routes(routes: &BTreeMap<String, StorageRoute>) -> Result<()> {
    if routes.is_empty() {
        return Err(invalid("empty route group"));
    }
    for (domain, route) in routes {
        identity(domain)?;
        identity(&route.backend)?;
        identity(&route.endpoint)?;
        if let Ok(url) = url::Url::parse(&route.endpoint)
            && (!url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some())
        {
            return Err(invalid(
                "route endpoint cannot carry credentials, queries or fragments",
            ));
        }
    }
    Ok(())
}

fn validate_group(group: &RouteGroup) -> Result<()> {
    identity(&group.identity)?;
    identity(&group.generation)?;
    identity(&group.writer_epoch)?;
    if group.domains.is_empty() {
        Ok(())
    } else {
        validate_routes(&group.domains)
    }
}

fn validate_prepared(prepared: &PreparedCutover) -> Result<()> {
    identity(&prepared.migration_id)?;
    identity(&prepared.source_identity)?;
    identity(&prepared.destination_identity)?;
    identity(&prepared.source_generation)?;
    identity(&prepared.expected_route_generation)?;
    if prepared.source_identity == prepared.destination_identity {
        return Err(invalid("source and destination identities coincide"));
    }
    validate_routes(&prepared.source_routes)?;
    validate_routes(&prepared.destination_routes)?;
    if prepared
        .source_routes
        .keys()
        .ne(prepared.destination_routes.keys())
    {
        return Err(invalid("source and destination route closure differ"));
    }
    Ok(())
}

fn read_authority(dir: &fsops::Dir) -> Result<Authority> {
    let authority: Authority =
        read(dir, AUTHORITY)?.ok_or_else(|| invalid("route authority missing"))?;
    if authority.version != FORMAT_VERSION {
        return Err(invalid("unsupported route authority version"));
    }
    validate_group(&authority.active)?;
    if let Some(freeze) = &authority.freeze {
        validate_prepared(freeze)?;
    }
    for (id, publication) in &authority.publications {
        validate_prepared(&publication.prepared)?;
        if id != &publication.prepared.migration_id
            || publication.generation != route_generation(&publication.prepared)?
            || publication.manifest_digest.len() != 64
            || !publication
                .manifest_digest
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(invalid("corrupt publication receipt"));
        }
        if authority.active.generation == publication.generation
            && (authority.active.identity != publication.prepared.destination_identity
                || authority.active.domains != publication.prepared.destination_routes)
        {
            return Err(invalid("published routes differ from receipt"));
        }
    }
    for (id, prepared) in &authority.rollbacks {
        validate_prepared(prepared)?;
        if id != &prepared.migration_id {
            return Err(invalid("corrupt rollback receipt"));
        }
    }
    Ok(authority)
}

fn route_generation(prepared: &PreparedCutover) -> Result<String> {
    Ok(format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(prepared).map_err(|_| invalid("route encoding failed"))?)
    ))
}

fn match_manifest(prepared: &PreparedCutover, manifest: &Manifest) -> Result<()> {
    if prepared.migration_id != manifest.migration_id
        || prepared.source_identity != manifest.source.source_identity
        || prepared.source_generation != manifest.source.generation
        || prepared.destination_identity != manifest.destination_identity
        || prepared.expected_route_generation != manifest.expected_route_generation
        || prepared
            .destination_routes
            .keys()
            .ne(manifest.source.domains.keys())
    {
        return Err(conflict("manifest differs from prepared cutover group"));
    }
    Ok(())
}

fn manifest_digest(manifest: &Manifest) -> Result<String> {
    let mut input = manifest.clone();
    input.published_generation = None;
    let bytes = serde_json::to_vec(&input).map_err(|_| invalid("manifest encoding failed"))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

#[derive(Clone)]
pub struct LocalCutover(Files);

/// Retain for the entire supported process lifecycle. A new owner and freeze
/// cannot pass their admission checks concurrently. Multiple ordinary owners
/// share this lock; freeze alone needs exclusion. Drop releases local flock.
pub struct WriterPermit {
    _lock: File,
}

impl crate::storage::migration::contracts::FreezeGuard for LocalCutover {
    fn validate(&self, generation: &str) -> Result<()> {
        self.validate_freeze_sync(generation)
    }
}

impl LocalCutover {
    /// Explicit fresh bootstrap, serialized with supported startup and migration.
    pub async fn initialize_routes(
        &self,
        domains: BTreeMap<String, StorageRoute>,
        stopped: Arc<dyn StoppedOwners>,
    ) -> Result<()> {
        validate_routes(&domains)?;
        self.0
            .run(move |dir| {
                let permit = admission(dir)?;
                crate::storage::drivers::posix::try_lock_exclusive(&permit)
                    .map_err(|_| conflict("supported writer process is still active"))?;
                let mut state = read_authority(dir)?;
                if !state.active.domains.is_empty()
                    || state.active.generation != "initial"
                    || state.freeze.is_some()
                    || !state.publications.is_empty()
                    || !state.rollbacks.is_empty()
                {
                    return Err(conflict("storage bootstrap requires a fresh installation"));
                }
                stopped.verify_stopped()?;
                state.active.domains = domains;
                state.active.generation = "initialized".into();
                publish(dir, AUTHORITY, &state)
            })
            .await
    }
    /// Recovery must never bootstrap a missing or corrupt routing authority.
    pub fn open_existing(root: &Path) -> Result<Self> {
        let files = Files::open(root)?;
        files.locked(|dir| read_authority(dir).map(|_| ()))?;
        Ok(Self(files))
    }

    pub fn open(root: &Path, initial: RouteGroup) -> Result<Self> {
        validate_group(&initial)?;
        let files = Files::open(root)?;
        files.locked(|dir| {
            if read::<Authority>(dir, AUTHORITY)?.is_some() {
                read_authority(dir)?;
            } else {
                publish(
                    dir,
                    AUTHORITY,
                    &Authority {
                        version: FORMAT_VERSION,
                        active: initial,
                        freeze: None,
                        publications: BTreeMap::new(),
                        rollbacks: BTreeMap::new(),
                    },
                )?;
            }
            Ok(())
        })?;
        Ok(Self(files))
    }

    pub async fn routes(&self) -> Result<RouteGroup> {
        // Immutable, complete authority publication makes read-only checks safe
        // without acquiring a running owner's lifetime lock.
        let files = self.0.clone();
        tokio::task::spawn_blocking(move || read_authority(&files.0).map(|state| state.active))
            .await
            .map_err(unavailable)?
    }

    pub async fn published_routes(
        &self,
        migration_id: &str,
    ) -> Result<Option<BTreeMap<String, StorageRoute>>> {
        let files = self.0.clone();
        let id = migration_id.to_owned();
        tokio::task::spawn_blocking(move || {
            Ok(read_authority(&files.0)?
                .publications
                .get(&id)
                .map(|publication| publication.prepared.destination_routes.clone()))
        })
        .await
        .map_err(unavailable)?
    }

    pub async fn freeze(
        &self,
        prepared: PreparedCutover,
        stopped: Arc<dyn StoppedOwners>,
    ) -> Result<()> {
        validate_prepared(&prepared)?;
        self.0
            .run(move |dir| {
                let permit = admission(dir)?;
                crate::storage::drivers::posix::try_lock_exclusive(&permit)
                    .map_err(|_| conflict("supported writer process is still active"))?;
                let mut state = read_authority(dir)?;
                if state.publications.contains_key(&prepared.migration_id)
                    || state.rollbacks.contains_key(&prepared.migration_id)
                {
                    return Err(conflict("migration identity already completed"));
                }
                if state.freeze.as_ref() == Some(&prepared) {
                    return Ok(());
                }
                if state
                    .freeze
                    .as_ref()
                    .is_some_and(|old| !state.publications.contains_key(&old.migration_id))
                    || state.active.identity != prepared.source_identity
                    || state.active.generation != prepared.expected_route_generation
                    || (!state.active.domains.is_empty()
                        && state.active.domains != prepared.source_routes)
                {
                    return Err(conflict("source route is not available for freeze"));
                }
                stopped.verify_stopped()?;
                state.active.domains = prepared.source_routes.clone();
                state.freeze = Some(prepared);
                publish(dir, AUTHORITY, &state)
            })
            .await
    }

    /// Abort a freeze whose plan never committed. Journal creation shares this
    /// directory CAS lock, so a concurrent late initial save cannot escape.
    /// Destination mutations must only be submitted after the durable manifest.
    pub async fn abort_unplanned(&self, migration_id: &str) -> Result<()> {
        let id = migration_id.to_owned();
        self.0
            .run(move |dir| {
                let mut state = read_authority(dir)?;
                if read_journal(dir, &id)?.is_some() || state.publications.contains_key(&id) {
                    return Err(conflict(
                        "a planned/published migration requires ordinary recovery",
                    ));
                }
                if state.rollbacks.contains_key(&id) {
                    return Ok(());
                }
                let prepared = state
                    .freeze
                    .as_ref()
                    .ok_or_else(|| conflict("migration not frozen"))?
                    .clone();
                if prepared.migration_id != id {
                    return Err(conflict("another migration owns the freeze"));
                }
                state.freeze = None;
                state.rollbacks.insert(id, prepared);
                publish(dir, AUTHORITY, &state)
            })
            .await
    }

    /// Explicitly abort an isolated stage before activation, without reopening
    /// exporters. Caller first drains/closes owned source and staging work.
    /// Syncing the intent before restoring admission is a recoverable sequence,
    /// rather than an assumed atomic multi-file transaction.
    pub async fn abort_planned(&self, migration_id: &str) -> Result<()> {
        let id = migration_id.to_owned();
        self.0
            .run(move |dir| {
                let mut state = read_authority(dir)?;
                let mut entry =
                    read_journal(dir, &id)?.ok_or_else(|| invalid("planned checkpoint missing"))?;
                if state.publications.contains_key(&id)
                    || entry.manifest.published_generation.is_some()
                {
                    return Err(conflict("published destination requires reverse migration"));
                }
                if let Some(prepared) = state.rollbacks.get(&id) {
                    return match_manifest(prepared, &entry.manifest);
                }
                let prepared = state
                    .freeze
                    .as_ref()
                    .ok_or_else(|| conflict("source not frozen"))?
                    .clone();
                match_manifest(&prepared, &entry.manifest)?;
                if state.active.identity != prepared.source_identity
                    || state.active.generation != prepared.expected_route_generation
                    || state.active.domains != prepared.source_routes
                {
                    return Err(conflict("source routing changed before abort"));
                }
                if !entry.manifest.rolled_back {
                    entry.manifest.rolled_back = true;
                    entry.revision = entry
                        .revision
                        .checked_add(1)
                        .ok_or_else(|| invalid("checkpoint revision exhausted"))?;
                    publish(dir, &journal_name(&id)?, &entry)?;
                }
                state.freeze = None;
                state.rollbacks.insert(id, prepared);
                publish(dir, AUTHORITY, &state)
            })
            .await
    }

    pub async fn validate_freeze(&self, source_generation: &str) -> Result<()> {
        let expected = source_generation.to_owned();
        let control = self.clone();
        tokio::task::spawn_blocking(move || control.validate_freeze_sync(&expected))
            .await
            .map_err(unavailable)?
    }

    /// Native frozen exporters already run on their owned blocking boundary.
    pub fn validate_freeze_sync(&self, expected: &str) -> Result<()> {
        let state = read_authority(&self.0.0)?;
        if state
            .freeze
            .as_ref()
            .is_some_and(|freeze| freeze.source_generation == expected)
            || state
                .publications
                .values()
                .any(|publication| publication.prepared.source_generation == expected)
        {
            Ok(())
        } else {
            Err(conflict("source maintenance fence absent or changed"))
        }
    }

    pub async fn ensure_writable(&self, identity: &str, generation: &str) -> Result<()> {
        let identity = identity.to_owned();
        let generation = generation.to_owned();
        let files = self.0.clone();
        tokio::task::spawn_blocking(move || {
            check_writable(&read_authority(&files.0)?, &identity, &generation)
        })
        .await
        .map_err(unavailable)?
    }

    pub async fn acquire_writer(&self, identity: &str, generation: &str) -> Result<WriterPermit> {
        let identity = identity.to_owned();
        let generation = generation.to_owned();
        let files = self.0.clone();
        tokio::task::spawn_blocking(move || {
            let lock = admission(&files.0)?;
            crate::storage::drivers::posix::try_lock_shared(&lock)
                .map_err(|_| conflict("migration has exclusive writer admission"))?;
            check_writable(&read_authority(&files.0)?, &identity, &generation)?;
            Ok(WriterPermit { _lock: lock })
        })
        .await
        .map_err(unavailable)?
    }
}

fn check_writable(state: &Authority, identity: &str, generation: &str) -> Result<()> {
    if state.active.identity != identity || state.active.generation != generation {
        return Err(conflict("stale route or writer identity"));
    }
    if state
        .freeze
        .as_ref()
        .is_some_and(|freeze| freeze.source_identity == identity)
    {
        return Err(conflict("source is frozen for migration"));
    }
    Ok(())
}

#[async_trait]
impl Cutover for LocalCutover {
    async fn validate_plan(&self, manifest: &Manifest) -> Result<()> {
        let manifest = manifest.clone();
        self.0
            .run(move |dir| {
                let state = read_authority(dir)?;
                let prepared = state
                    .freeze
                    .as_ref()
                    .ok_or_else(|| conflict("source not frozen"))?;
                match_manifest(prepared, &manifest)?;
                if state.active.identity != prepared.source_identity
                    || state.active.generation != prepared.expected_route_generation
                {
                    return Err(conflict("route changed before migration"));
                }
                Ok(())
            })
            .await
    }
    async fn publish(&self, manifest: &Manifest) -> Result<String> {
        let manifest = manifest.clone();
        self.0
            .run(move |dir| {
                let mut state = read_authority(dir)?;
                if let Some(publication) = state.publications.get(&manifest.migration_id) {
                    match_manifest(&publication.prepared, &manifest)?;
                    if publication.manifest_digest != manifest_digest(&manifest)?
                        || manifest
                            .published_generation
                            .as_ref()
                            .is_some_and(|generation| generation != &publication.generation)
                    {
                        return Err(conflict(
                            "publication command reused with different manifest",
                        ));
                    }
                    return Ok(publication.generation.clone());
                }
                let prepared = state
                    .freeze
                    .as_ref()
                    .ok_or_else(|| conflict("source not frozen"))?
                    .clone();
                match_manifest(&prepared, &manifest)?;
                let checkpoint = read_journal(dir, &manifest.migration_id)?
                    .ok_or_else(|| invalid("sealed checkpoint missing"))?;
                if checkpoint.manifest != manifest
                    || !manifest.sealed
                    || manifest.rolled_back
                    || !manifest.destination_write_started
                    || !manifest.progress.values().all(|progress| progress.complete)
                    || state.active.generation != prepared.expected_route_generation
                    || state.active.identity != prepared.source_identity
                {
                    return Err(conflict(
                        "cutover requires current verified sealed checkpoint",
                    ));
                }
                let generation = route_generation(&prepared)?;
                if generation == state.active.generation {
                    return Err(conflict("route generation did not advance"));
                }
                state.active = RouteGroup {
                    identity: prepared.destination_identity.clone(),
                    generation: generation.clone(),
                    writer_epoch: format!("migration:{generation}"),
                    domains: prepared.destination_routes.clone(),
                };
                state.publications.insert(
                    manifest.migration_id.clone(),
                    Publication {
                        prepared,
                        generation: generation.clone(),
                        manifest_digest: manifest_digest(&manifest)?,
                    },
                );
                publish(dir, AUTHORITY, &state)?;
                Ok(generation)
            })
            .await
    }
    async fn receipt(&self, id: &str) -> Result<Option<String>> {
        let id = id.to_owned();
        self.0
            .run(move |dir| {
                Ok(read_authority(dir)?
                    .publications
                    .get(&id)
                    .map(|publication| publication.generation.clone()))
            })
            .await
    }
    async fn rollback(&self, manifest: &Manifest) -> Result<()> {
        let manifest = manifest.clone();
        self.0
            .run(move |dir| {
                let mut state = read_authority(dir)?;
                if state.publications.contains_key(&manifest.migration_id)
                    || manifest.published_generation.is_some()
                {
                    return Err(conflict("published destination requires reverse migration"));
                }
                let checkpoint = read_journal(dir, &manifest.migration_id)?
                    .ok_or_else(|| invalid("rollback checkpoint missing"))?;
                if checkpoint.manifest != manifest || !manifest.rolled_back {
                    return Err(conflict(
                        "rollback requires current abort checkpoint before publication",
                    ));
                }
                if let Some(prepared) = state.rollbacks.get(&manifest.migration_id) {
                    return match_manifest(prepared, &manifest);
                }
                let prepared = state
                    .freeze
                    .as_ref()
                    .ok_or_else(|| conflict("source not frozen"))?
                    .clone();
                match_manifest(&prepared, &manifest)?;
                if state.active.identity != prepared.source_identity
                    || state.active.generation != prepared.expected_route_generation
                    || state.active.domains != prepared.source_routes
                {
                    return Err(conflict("source route changed"));
                }
                state.freeze = None;
                state
                    .rollbacks
                    .insert(manifest.migration_id.clone(), prepared);
                publish(dir, AUTHORITY, &state)
            })
            .await
    }
}

#[cfg(test)]
mod tests;
