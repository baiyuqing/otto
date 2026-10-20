//! Local writer admission and cutover authority.

use super::*;

#[derive(Clone)]
pub struct LocalCutover(Files);

/// Retain for the entire supported process lifecycle. A new owner and freeze
/// cannot pass their admission checks concurrently. Multiple ordinary owners
/// share this lock; freeze alone needs exclusion. Drop releases local flock.
pub struct WriterPermit {
    pub(super) _lock: File,
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

pub(super) fn check_writable(state: &Authority, identity: &str, generation: &str) -> Result<()> {
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
