//! Offline transfer composition. Adapters own codecs, staging and reference validation.
use crate::{
    fsops,
    storage::{
        Connection,
        backends::{
            posix::{
                journal_migration::{Domain, JournalInput, JournalSource, JournalStage},
                migration::{
                    LocalCutover, LocalJournal, PreparedCutover, RouteGroup, StoppedOwners,
                },
            },
            turso::migration::{self as sql, SqlDomainCodec},
        },
        migration::{
            self, Coordinator, Cutover, FreezeGuard, MigrationJournal, SnapshotExport, StageImport,
            StorageRoute,
            group::{GroupSource, GroupStage},
        },
    },
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs::File,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    sync::Arc,
};

type Result<T> = migration::Result<T>;
const MAX_ENTRIES: usize = 10_000;
const MAX_CONFIG: u64 = 1024 * 1024;
const CHUNK_BYTES: usize = 16 * 1024 * 1024;
const OWNER: &str = ".transfer-owner.json";

struct Options {
    home: PathBuf,
    destination: PathBuf,
    id: String,
}
impl Options {
    fn parse(args: &[String]) -> Result<Self> {
        let [home, destination, id] = args else {
            return Err(invalid(
                "usage: otto storage transfer <source-home> <destination-root> <migration-id>",
            ));
        };
        validate_id(id)?;
        let home = absolute(Path::new(home))?;
        let destination = absolute(Path::new(destination))?;
        if destination.starts_with(&home) || home.starts_with(&destination) {
            return Err(invalid("source home and destination root must not overlap"));
        }
        Ok(Self {
            home,
            destination,
            id: id.clone(),
        })
    }
}

#[cfg(test)]
pub async fn run(
    args: &[String],
    stdout: &mut (dyn Write + Send),
    stderr: &mut (dyn Write + Send),
) -> i32 {
    run_with_backends(
        args,
        stdout,
        stderr,
        &Default::default(),
        &Default::default(),
    )
    .await
}

pub async fn run_with_backends(
    args: &[String],
    stdout: &mut (dyn Write + Send),
    stderr: &mut (dyn Write + Send),
    backends: &crate::runtime::storage::BackendRegistry,
    environment: &HashMap<String, String>,
) -> i32 {
    let result = async {
        let (args, destination_config) = match args {
            [rest @ .., flag, path] if flag == "--to-config" => {
                (rest, Some(read_configuration(&absolute(Path::new(path))?)?))
            }
            _ => (args, None),
        };
        transfer_registered(
            Options::parse(args)?,
            backends,
            environment,
            destination_config.as_ref(),
        )
        .await
    }
    .await;
    match result {
        Ok(manifest) => {
            let _ = writeln!(
                stdout,
                "transferred {} domains; route generation {}",
                manifest.source.domains.len(),
                manifest.published_generation.unwrap_or_default()
            );
            0
        }
        Err(error) => {
            // Extension adapters may wrap SDK failures; never print their payloads.
            if !backends.is_empty() {
                let message = match error {
                    migration::Error::Conflict(_) => {
                        "storage transfer conflicts with persisted state"
                    }
                    migration::Error::Invalid(_) => {
                        "storage transfer data, configuration or profile is incompatible"
                    }
                    migration::Error::Unavailable(_) => "storage transfer backend is unavailable",
                    migration::Error::CommitUnknown => {
                        "storage transfer commit outcome is unknown; reconcile its receipt"
                    }
                };
                let _ = writeln!(stderr, "{message}");
            } else {
                let _ = writeln!(stderr, "{error}");
            }
            1
        }
    }
}

struct EmptyInstallation(Vec<PathBuf>);
impl StoppedOwners for EmptyInstallation {
    fn verify_stopped(&self) -> Result<()> {
        for path in &self.0 {
            match std::fs::symlink_metadata(path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                _ => {
                    return Err(invalid(
                        "storage initialization refuses existing authority state; transfer it instead",
                    ));
                }
            }
        }
        Ok(())
    }
}

fn read_configuration(path: &Path) -> Result<otto_core::config::File> {
    let file = open_file(path, libc::O_RDONLY).map_err(unavailable)?;
    if file.metadata().map_err(unavailable)?.len() > MAX_CONFIG {
        return Err(invalid("storage configuration exceeds read limit"));
    }
    let mut text = String::new();
    file.take(MAX_CONFIG + 1)
        .read_to_string(&mut text)
        .map_err(unavailable)?;
    if text.len() as u64 > MAX_CONFIG {
        return Err(invalid("storage configuration exceeds read limit"));
    }
    otto_core::config::parse(&text).map_err(|_| invalid("storage configuration is invalid"))
}

pub async fn initialize(
    args: &[String],
    stdout: &mut (dyn Write + Send),
    stderr: &mut (dyn Write + Send),
    registry: &crate::runtime::storage::BackendRegistry,
    environment: &HashMap<String, String>,
) -> i32 {
    let result = async {
        let [home, config] = args else {
            return Err(invalid("usage: otto storage init <home> <config-path>"));
        };
        let home = absolute(Path::new(home))?;
        pin_directory(&home).map_err(unavailable)?;
        let config = read_configuration(&absolute(Path::new(config))?)?;
        let bindings =
            crate::runtime::storage::backend::resolve(&config.storage, environment, &home)
                .map_err(|_| invalid("storage endpoint configuration is invalid"))?;
        let mut domains = crate::runtime::storage::backend::configured_routes(&config.storage);
        if domains.is_empty() {
            return Err(invalid(
                "storage initialization requires explicit domain routes",
            ));
        }
        let empty = Arc::new(EmptyInstallation(vec![
            home.join(".otto/approvals.db"),
            home.join(".otto/workflows.db"),
            home.join(".otto/schedule.db"),
            home.join(".otto/usage.db"),
            home.join(".otto/tasks.db"),
            home.join(".otto/reflection.db"),
            home.join(".otto/skill-checks.db"),
            home.join(".otto/memory-advice.db"),
            home.join(".otto/sessions"),
        ]));
        empty.verify_stopped()?;
        // This creates only the normal empty local route authority, never a domain store.
        let storage = crate::runtime::storage::Storage::open_home(&home)
            .await
            .map_err(|_| invalid("storage bootstrap authority is unavailable"))?;
        drop(storage);
        let cutover = LocalCutover::open_existing(&home.join(".otto/storage"))?;
        if !cutover.routes().await?.domains.is_empty() {
            return Err(invalid("storage bootstrap requires a fresh installation"));
        }
        let mut by_instance: BTreeMap<
            String,
            Vec<crate::runtime::storage::RepositoryRequirements>,
        > = BTreeMap::new();
        for (domain, instance) in &config.storage.routes {
            by_instance.entry(instance.clone()).or_default().push(
                crate::runtime::storage::RepositoryRequirements::for_domain(domain)
                    .map_err(|_| invalid("unsupported domain"))?,
            );
        }
        for (instance, requirements) in by_instance {
            let binding = &bindings[&instance];
            let factory = registry
                .configured_factory(&binding.factory, &binding.endpoint, &binding.options)
                .await
                .map_err(|_| invalid("storage backend is unavailable"))?;
            let identity = factory
                .initialize(&binding.endpoint, &requirements, &binding.options)
                .await
                .map_err(|_| invalid("backend namespace initialization failed"))?;
            identity
                .validate()
                .map_err(|_| invalid("backend namespace identity is invalid"))?;
            for requirement in requirements {
                let verified = factory
                    .validate(&binding.endpoint, &requirement, &binding.options)
                    .await
                    .map_err(|_| invalid("initialized namespace profile is incompatible"))?;
                if verified != identity {
                    return Err(invalid("initialized namespace identity differs"));
                }
                domains
                    .get_mut(&requirement.domain)
                    .unwrap()
                    .namespace_identity = Some(identity.0.clone());
            }
        }
        cutover.initialize_routes(domains, empty).await
    }
    .await;
    match result {
        Ok(()) => {
            let _ = writeln!(
                stdout,
                "initialized storage routes; existing-state replacement requires transfer"
            );
            0
        }
        Err(error) => {
            let _ = writeln!(stderr, "{error}");
            1
        }
    }
}

/// Explicit recovery of an unpublished isolated copy; all staged bytes remain.
pub async fn abort(
    args: &[String],
    stdout: &mut (dyn Write + Send),
    stderr: &mut (dyn Write + Send),
) -> i32 {
    let result = async {
        let [home, id] = args else {
            return Err(invalid(
                "usage: otto storage abort <source-home> <migration-id>",
            ));
        };
        validate_id(id)?;
        let home = absolute(Path::new(home))?;
        pin_directory(&home).map_err(unavailable)?;
        let control = home.join(".otto/storage");
        // Recovery must never initialize an unrelated, empty authority.
        let cutover = LocalCutover::open_existing(&control)?;
        let journal = LocalJournal::open(&control)?;
        if journal.load(id).await?.is_some() {
            cutover.abort_planned(id).await
        } else {
            cutover.abort_unplanned(id).await
        }
    }
    .await;
    match result {
        Ok(()) => {
            let _ = writeln!(
                stdout,
                "source admission restored; isolated staging retained"
            );
            0
        }
        Err(error) => {
            let _ = writeln!(stderr, "{error}");
            1
        }
    }
}

fn validate_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(invalid(
            "migration ID must contain 1-128 letters, numbers, '-' or '_'",
        ));
    }
    Ok(())
}

fn invalid(message: &str) -> migration::Error {
    migration::Error::Invalid(message.into())
}
fn unavailable(error: impl std::fmt::Display) -> migration::Error {
    migration::Error::Unavailable(error.to_string())
}
fn absolute(path: &Path) -> Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir().map_err(unavailable)?.join(path)
    };
    if path
        .components()
        .any(|part| !matches!(part, Component::RootDir | Component::Normal(_)))
    {
        return Err(invalid(
            "storage paths must not contain '.' or '..' traversal",
        ));
    }
    if path.to_str().is_none() {
        return Err(invalid("storage path is not UTF-8"));
    }
    Ok(path)
}
fn pin_directory(path: &Path) -> std::io::Result<fsops::Dir> {
    let mut root = fsops::open_dir_no_follow(Path::new("/"))?;
    for part in path.components() {
        match part {
            Component::RootDir => (),
            Component::Normal(name) => {
                root = fsops::open_dir_at_no_follow(
                    &root,
                    name.to_str().ok_or_else(|| {
                        std::io::Error::new(std::io::ErrorKind::InvalidInput, "non-UTF8 directory")
                    })?,
                )?
            }
            _ => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "path traversal",
                ));
            }
        }
    }
    Ok(root)
}
fn open_file(path: &Path, flags: libc::c_int) -> std::io::Result<File> {
    let parent = pin_directory(path.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "file has no parent")
    })?)?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "file has no name"))?;
    let file = fsops::open_at_no_follow(&parent, name, flags | libc::O_NONBLOCK)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "not a regular state file",
        ));
    }
    Ok(file)
}
fn present_file(path: &Path) -> Result<bool> {
    match open_file(path, libc::O_RDONLY) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(unavailable(error)),
    }
}
fn identity(path: &Path) -> String {
    format!(
        "local:{:x}",
        Sha256::digest(path.as_os_str().as_encoded_bytes())
    )
}
fn endpoint(path: &Path) -> Result<String> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| invalid("non-UTF8 endpoint"))
}
fn mkdir(root: &fsops::Dir, name: &str) -> Result<fsops::Dir> {
    match fsops::mkdir_at(root, name, 0o700) {
        Ok(()) => (),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(error) => return Err(unavailable(error)),
    }
    fsops::open_dir_at_no_follow(root, name).map_err(unavailable)
}
fn sync_directory(root: &fsops::Dir) -> Result<()> {
    let file = fsops::open_at_no_follow(root, ".", libc::O_RDONLY | libc::O_DIRECTORY)
        .map_err(unavailable)?;
    file.sync_all().map_err(unavailable)
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Owner {
    version: u32,
    migration_id: String,
    source_identity: String,
    destination_identity: String,
}
fn private_root(root: &fsops::Dir) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let file = fsops::open_at_no_follow(root, ".", libc::O_RDONLY | libc::O_DIRECTORY)
        .map_err(unavailable)?;
    if file.metadata().map_err(unavailable)?.permissions().mode() & 0o777 != 0o700 {
        return Err(invalid("migration staging directory must have mode 0700"));
    }
    Ok(())
}
fn read_owner(root: &fsops::Dir) -> Result<Owner> {
    let mut file = fsops::open_at_no_follow(root, OWNER, libc::O_RDONLY)
        .map_err(|_| invalid("existing destination is not this migration's isolated stage"))?;
    if !file.metadata().map_err(unavailable)?.is_file()
        || file.metadata().map_err(unavailable)?.len() > MAX_CONFIG
    {
        return Err(invalid("invalid destination ownership marker"));
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(MAX_CONFIG + 1)
        .read_to_end(&mut bytes)
        .map_err(unavailable)?;
    if bytes.len() as u64 > MAX_CONFIG {
        return Err(invalid("destination ownership marker exceeds limit"));
    }
    serde_json::from_slice(&bytes).map_err(|_| invalid("invalid destination ownership marker"))
}
fn destination(options: &Options, source_identity: &str) -> Result<fsops::Dir> {
    let parent = pin_directory(
        options
            .destination
            .parent()
            .ok_or_else(|| invalid("destination has no parent"))?,
    )
    .map_err(unavailable)?;
    let name = options
        .destination
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| invalid("invalid destination root"))?;
    let expected = Owner {
        version: 1,
        migration_id: options.id.clone(),
        source_identity: source_identity.into(),
        destination_identity: identity(&options.destination),
    };
    match fsops::open_dir_at_no_follow(&parent, name) {
        Ok(root) => {
            private_root(&root)?;
            if read_owner(&root)? != expected {
                return Err(invalid(
                    "destination belongs to another source or migration",
                ));
            }
            return Ok(root);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
        Err(error) => return Err(unavailable(error)),
    }
    let bytes = serde_json::to_vec(&expected).map_err(unavailable)?;
    let temporary = format!(".otto-transfer-{:x}", Sha256::digest(&bytes));
    let root = mkdir(&parent, &temporary)?;
    private_root(&root)?;
    let names = fsops::read_directory_names(&root, 1).map_err(unavailable)?;
    if names.iter().any(|name| name != OWNER) {
        return Err(invalid(
            "destination bootstrap directory contains unrelated data",
        ));
    }
    if !names.is_empty() {
        let marker = fsops::open_at_no_follow(&root, OWNER, libc::O_RDONLY | libc::O_NONBLOCK)
            .map_err(unavailable)?;
        let metadata = marker.metadata().map_err(unavailable)?;
        if !metadata.is_file() || metadata.len() > MAX_CONFIG {
            return Err(invalid("invalid destination bootstrap marker"));
        }
        // Only this deterministic unpublished namespace can have an interrupted
        // ownership-marker write. No domain writes happen before publication.
        if let Ok(owner) = read_owner(&root)
            && owner != expected
        {
            return Err(invalid(
                "destination bootstrap belongs to another migration",
            ));
        }
        fsops::unlink_at(&root, OWNER).map_err(unavailable)?;
    }
    let mut file = fsops::create_at_no_follow(&root, OWNER, libc::O_WRONLY | libc::O_EXCL, 0o600)
        .map_err(unavailable)?;
    file.write_all(&bytes).map_err(unavailable)?;
    file.sync_all().map_err(unavailable)?;
    sync_directory(&root)?;
    fsops::publish_directory(&parent, &temporary, name, false).map_err(unavailable)?;
    sync_directory(&parent)?;
    Ok(root)
}

fn codec(domain: &str) -> Result<Arc<dyn SqlDomainCodec>> {
    Ok(match domain {
        "approvals" => Arc::new(sql::ApprovalCodec),
        "workflows" => Arc::new(sql::workflow::WorkflowCodec),
        "schedule" => Arc::new(sql::schedule::ScheduleCodec),
        "usage" => Arc::new(sql::usage::UsageCodec),
        "tasks" => Arc::new(sql::tasks::TaskCodec),
        "reflection" => Arc::new(sql::reflection::ReflectionCodec),
        "skill_checks" => Arc::new(sql::skill_checks::SkillChecksCodec),
        "memory_advice" => Arc::new(sql::memory_advice::MemoryAdviceCodec),
        _ => return Err(invalid("unsupported local destination domain")),
    })
}

struct Database {
    path: PathBuf,
    codec: Arc<dyn SqlDomainCodec>,
}
fn databases(options: &Options, routes: &BTreeMap<String, StorageRoute>) -> Result<Vec<Database>> {
    let defaults: Vec<(PathBuf, Arc<dyn SqlDomainCodec>)> = vec![
        (
            options.home.join(".otto/approvals.db"),
            Arc::new(sql::ApprovalCodec),
        ),
        (
            options.home.join(".otto/workflows.db"),
            Arc::new(sql::workflow::WorkflowCodec),
        ),
        (
            options.home.join(".otto/schedule.db"),
            Arc::new(sql::schedule::ScheduleCodec),
        ),
        (
            options.home.join(".otto/usage.db"),
            Arc::new(sql::usage::UsageCodec),
        ),
        (
            options.home.join(".otto/tasks.db"),
            Arc::new(sql::tasks::TaskCodec),
        ),
        (
            options.home.join(".otto/reflection.db"),
            Arc::new(sql::reflection::ReflectionCodec),
        ),
        (
            options.home.join(".otto/skill-checks.db"),
            Arc::new(sql::skill_checks::SkillChecksCodec),
        ),
        (
            options.home.join(".otto/memory-advice.db"),
            Arc::new(sql::memory_advice::MemoryAdviceCodec),
        ),
    ];
    let mut databases = Vec::new();
    let mut seen = BTreeSet::new();
    for (default, codec) in defaults {
        let path = if let Some(route) = routes.get(codec.domain()) {
            if route.backend != "sqlite" || route.endpoint.starts_with("instance:") {
                continue;
            }
            absolute(Path::new(&route.endpoint))?
        } else {
            default
        };
        if present_file(&path)? {
            if !seen.insert(path.clone()) {
                return Err(invalid(
                    "one database is assigned to multiple unsupported schema families",
                ));
            }
            let connection = Connection::open_read_only(&path).map_err(unavailable)?;
            codec.validate(&connection)?;
            databases.push(Database { path, codec });
        } else if routes.contains_key(codec.domain()) {
            return Err(invalid("configured source state route is missing"));
        }
    }
    Ok(databases)
}

fn journal_inputs(root: &Path) -> Result<Vec<JournalInput>> {
    let directory = pin_directory(root).map_err(unavailable)?;
    let mut pending = vec![(directory, String::new())];
    let mut inputs = Vec::new();
    let mut entries = 0;
    while let Some((directory, prefix)) = pending.pop() {
        let mut names =
            fsops::read_directory_names(&directory, MAX_ENTRIES.saturating_sub(entries))
                .map_err(unavailable)?;
        entries += names.len();
        names.sort();
        for name in names {
            let relative = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            if relative.len() > 4096 || relative.split('/').count() > 64 {
                return Err(invalid("session tree exceeds migration traversal bound"));
            }
            match fsops::open_dir_at_no_follow(&directory, &name) {
                Ok(child) => {
                    // Live ownership claims are recreated by new owners, not migrated.
                    if !name.ends_with(".lease") {
                        pending.push((child, relative));
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotADirectory => {
                    let file = fsops::open_at_no_follow(
                        &directory,
                        &name,
                        libc::O_RDONLY | libc::O_NONBLOCK,
                    )
                    .map_err(unavailable)?;
                    if !file.metadata().map_err(unavailable)?.is_file() {
                        return Err(invalid("session tree contains a nonregular file"));
                    }
                    if name.ends_with(".jsonl") {
                        inputs.push(JournalInput {
                            legacy_path: Some(endpoint(&root.join(&relative))?),
                            relative_path: relative,
                            artifact_id: None,
                        });
                    } else if !(name.starts_with(".migration-")
                        || name.starts_with(".dataset-")
                        || name.starts_with(".chunk-"))
                    {
                        return Err(invalid(
                            "session tree contains an unsupported non-journal artifact",
                        ));
                    }
                }
                Err(error) => return Err(unavailable(error)),
            }
        }
    }
    inputs.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    Ok(inputs)
}

struct NativeStopped {
    databases: Vec<PathBuf>,
    journals: Vec<PathBuf>,
}
impl StoppedOwners for NativeStopped {
    fn verify_stopped(&self) -> Result<()> {
        struct HeldSql(Connection);
        impl Drop for HeldSql {
            fn drop(&mut self) {
                let _ = self.0.execute_batch("ROLLBACK");
            }
        }
        let mut databases = Vec::new();
        let mut journals = Vec::new();
        for path in &self.databases {
            let connection = Connection::open(path).map_err(unavailable)?;
            connection.execute_batch("BEGIN IMMEDIATE").map_err(|_| {
                migration::Error::Conflict("a source SQL writer is still active".into())
            })?;
            databases.push(HeldSql(connection));
        }
        for path in &self.journals {
            let file = open_file(path, libc::O_RDWR).map_err(unavailable)?;
            crate::storage::drivers::posix::try_lock_exclusive(&file).map_err(|_| {
                migration::Error::Conflict("a source session writer is still active".into())
            })?;
            journals.push(file);
        }
        Ok(())
    }
}
struct Fence(LocalCutover);
impl FreezeGuard for Fence {
    fn validate(&self, generation: &str) -> Result<()> {
        self.0.validate_freeze_sync(generation)
    }
}

struct ExternalDomain {
    domain: String,
    factory: Arc<dyn crate::runtime::storage::RepositoryFactory>,
    binding: crate::runtime::storage::backend::ConfiguredEndpoint,
}

async fn transfer_registered(
    options: Options,
    registry: &crate::runtime::storage::BackendRegistry,
    environment: &HashMap<String, String>,
    destination_config: Option<&otto_core::config::File>,
) -> Result<migration::Manifest> {
    let home = pin_directory(&options.home).map_err(unavailable)?;
    let otto = mkdir(&home, ".otto")?;
    let _control = mkdir(&otto, "storage")?;
    sync_directory(&otto)?;
    sync_directory(&home)?;
    let control_path = options.home.join(".otto/storage");
    let source_identity = identity(&options.home);
    let cutover = LocalCutover::open(
        &control_path,
        RouteGroup {
            identity: source_identity,
            generation: "initial".into(),
            writer_epoch: "initial".into(),
            domains: BTreeMap::new(),
        },
    )?;
    let journal = LocalJournal::open(&control_path)?;
    let mut previous = journal.load(&options.id).await?;
    if let Some(entry) = &mut previous
        && entry.manifest.published_generation.is_none()
        && let Some(generation) = cutover.receipt(&options.id).await?
    {
        if !entry.manifest.sealed
            || entry.manifest.rolled_back
            || generation == entry.manifest.expected_route_generation
        {
            return Err(invalid(
                "publication receipt differs from the sealed manifest",
            ));
        }
        entry.manifest.published_generation = Some(generation);
        entry.revision = journal.save(Some(entry.revision), &entry.manifest).await?;
    }
    if let Some(entry) = &previous {
        if entry.manifest.destination_identity != identity(&options.destination) {
            return Err(invalid(
                "resume destination differs from the durable migration manifest",
            ));
        }
        if entry.manifest.published_generation.is_some() {
            if let Some(config) = destination_config {
                let published = cutover
                    .published_routes(&options.id)
                    .await?
                    .ok_or_else(|| invalid("published routing receipt is missing"))?;
                let bindings = crate::runtime::storage::backend::resolve(
                    &config.storage,
                    environment,
                    &options.home,
                )
                .map_err(|_| invalid("destination backend configuration is invalid"))?;
                for (domain, requested) in
                    crate::runtime::storage::backend::configured_routes(&config.storage)
                {
                    let route = published
                        .get(&domain)
                        .ok_or_else(|| invalid("resume domain differs from published routes"))?;
                    if route.backend != requested.backend || route.endpoint != requested.endpoint {
                        return Err(invalid("resume backend differs from the published route"));
                    }
                    let instance = requested
                        .endpoint
                        .strip_prefix("instance:")
                        .ok_or_else(|| invalid("invalid destination instance"))?;
                    let binding = &bindings[instance];
                    let factory = registry
                        .configured_factory(&binding.factory, &binding.endpoint, &binding.options)
                        .await
                        .map_err(|_| invalid("destination backend is unavailable"))?;
                    let requirement =
                        crate::runtime::storage::RepositoryRequirements::for_domain(&domain)
                            .map_err(|_| invalid("unsupported destination domain"))?;
                    let actual = factory
                        .validate(&binding.endpoint, &requirement, &binding.options)
                        .await
                        .map_err(|_| {
                            invalid("published destination is unavailable or incompatible")
                        })?;
                    if route.namespace_identity.as_deref() != Some(actual.0.as_str()) {
                        return Err(invalid(
                            "resume namespace differs from the publication receipt",
                        ));
                    }
                }
            }
            return Ok(entry.manifest.clone());
        }
        if entry.manifest.rolled_back {
            return Err(invalid("migration was rolled back; use a new migration ID"));
        }
    }
    let routes = cutover.routes().await?;
    let supported = [
        "memory",
        "approvals",
        "workflows",
        "schedule",
        "usage",
        "tasks",
        "reflection",
        "skill_checks",
        "memory_advice",
        "sessions",
    ];
    if routes
        .domains
        .keys()
        .any(|domain| !supported.contains(&domain.as_str()))
    {
        return Err(invalid(
            "source routing contains a state domain unsupported by offline transfer",
        ));
    }
    let source_config_path = options.home.join(".config/otto/config.toml");
    let source_config = if present_file(&source_config_path)? {
        read_configuration(&source_config_path)?
    } else {
        Default::default()
    };
    let source_bindings = crate::runtime::storage::backend::resolve(
        &source_config.storage,
        environment,
        &options.home,
    )
    .map_err(|_| invalid("source backend configuration is invalid"))?;
    let mut external_sources = Vec::new();
    for (domain, route) in &routes.domains {
        // A published `memory` route is only the file store's import source.
        if domain == "memory"
            || route.backend == "posix"
            || route.backend == "sqlite" && !route.endpoint.starts_with("instance:")
        {
            continue;
        }
        let instance = route
            .endpoint
            .strip_prefix("instance:")
            .ok_or_else(|| invalid("external source requires a configured backend instance"))?;
        let binding = source_bindings
            .get(instance)
            .ok_or_else(|| invalid("source backend instance is not configured"))?
            .clone();
        if binding.factory != route.backend {
            return Err(invalid(
                "source backend instance differs from the published route",
            ));
        }
        let factory = registry
            .configured_factory(&binding.factory, &binding.endpoint, &binding.options)
            .await
            .map_err(|_| invalid("source backend is unavailable"))?;
        let requirements = crate::runtime::storage::RepositoryRequirements::for_domain(domain)
            .map_err(|_| invalid("unsupported source domain"))?;
        let actual = factory
            .validate(&binding.endpoint, &requirements, &binding.options)
            .await
            .map_err(|_| invalid("source backend profile is unavailable or incompatible"))?;
        if route.namespace_identity.as_deref() != Some(actual.0.as_str()) {
            return Err(invalid(
                "source backend namespace differs from the published route",
            ));
        }
        external_sources.push(ExternalDomain {
            domain: domain.clone(),
            factory,
            binding,
        });
    }
    let databases = databases(&options, &routes.domains)?;
    let session_root = match routes.domains.get("sessions") {
        Some(route) if route.backend == "posix" => absolute(Path::new(&route.endpoint))?,
        Some(_) => {
            return Err(invalid(
                "source sessions backend is unsupported by offline transfer",
            ));
        }
        None => options.home.join(".otto/sessions"),
    };
    if options.destination.starts_with(&session_root)
        || session_root.starts_with(&options.destination)
    {
        return Err(invalid(
            "source session namespace and destination root must not overlap",
        ));
    }
    let session_inputs = match pin_directory(&session_root) {
        Ok(_) => Some(journal_inputs(&session_root)?),
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                && !routes.domains.contains_key("sessions") =>
        {
            None
        }
        Err(error) => return Err(unavailable(error)),
    };
    if databases.is_empty() && external_sources.is_empty() && session_inputs.is_none() {
        return Err(invalid("source contains no supported persistent state"));
    }
    let staged_root = destination(&options, &routes.identity)?;
    let staged_sessions = options.destination.join("sessions");
    if session_inputs.is_some() {
        mkdir(&staged_root, "sessions")?;
        sync_directory(&staged_root)?;
    }
    let mut source_routes = BTreeMap::new();
    let mut destination_routes = BTreeMap::new();
    let mut staged_databases = BTreeMap::new();
    for database in &databases {
        let domain = database.codec.domain().to_owned();
        let target = options.destination.join(format!("{domain}.db"));
        source_routes.insert(
            domain.clone(),
            StorageRoute {
                backend: "sqlite".into(),
                endpoint: endpoint(&database.path)?,
                namespace_identity: None,
            },
        );
        destination_routes.insert(
            domain.clone(),
            StorageRoute {
                backend: "sqlite".into(),
                endpoint: endpoint(&target)?,
                namespace_identity: None,
            },
        );
        staged_databases.insert(domain, target);
    }
    if session_inputs.is_some() {
        source_routes.insert(
            "sessions".into(),
            StorageRoute {
                backend: "posix".into(),
                endpoint: endpoint(&session_root)?,
                namespace_identity: None,
            },
        );
        destination_routes.insert(
            "sessions".into(),
            StorageRoute {
                backend: "posix".into(),
                endpoint: endpoint(&staged_sessions)?,
                namespace_identity: None,
            },
        );
    }
    for source in &external_sources {
        source_routes.insert(
            source.domain.clone(),
            routes.domains[&source.domain].clone(),
        );
        let target = options.destination.join(format!("{}.db", source.domain));
        destination_routes.insert(
            source.domain.clone(),
            StorageRoute {
                backend: "sqlite".into(),
                endpoint: endpoint(&target)?,
                namespace_identity: None,
            },
        );
        staged_databases.insert(source.domain.clone(), target);
    }
    let generation = format!("frozen:{}", options.id);
    let fence: Arc<dyn FreezeGuard> = Arc::new(Fence(cutover.clone()));
    let context = crate::runtime::storage::backend::MigrationContext {
        migration_id: options.id.clone(),
        source_identity: routes.identity.clone(),
        destination_identity: identity(&options.destination),
        generation: generation.clone(),
        fence: fence.clone(),
    };
    let mut imported_stages = BTreeMap::new();
    if let Some(config) = destination_config {
        let bindings =
            crate::runtime::storage::backend::resolve(&config.storage, environment, &options.home)
                .map_err(|_| invalid("destination backend configuration is invalid"))?;
        for (domain, instance) in &config.storage.routes {
            if !source_routes.contains_key(domain) {
                return Err(invalid(
                    "destination route names a domain absent from the source snapshot",
                ));
            }
            let binding = &bindings[instance];
            let factory = registry
                .configured_factory(&binding.factory, &binding.endpoint, &binding.options)
                .await
                .map_err(|_| invalid("destination backend is unavailable"))?;
            let requirement = crate::runtime::storage::RepositoryRequirements::for_domain(domain)
                .map_err(|_| invalid("unsupported destination domain"))?;
            let prepared = factory
                .prepare_import(&binding.endpoint, &requirement, &binding.options, &context)
                .await
                .map_err(|_| invalid("destination logical import preparation failed"))?;
            prepared
                .namespace_identity
                .validate()
                .map_err(|_| invalid("destination namespace identity is invalid"))?;
            imported_stages.insert(domain.clone(), prepared.stage);
            staged_databases.remove(domain);
            destination_routes.insert(
                domain.clone(),
                StorageRoute {
                    backend: binding.factory.clone(),
                    endpoint: format!("instance:{instance}"),
                    namespace_identity: Some(prepared.namespace_identity.0),
                },
            );
        }
    }
    let prepared = PreparedCutover {
        migration_id: options.id.clone(),
        source_identity: routes.identity.clone(),
        destination_identity: identity(&options.destination),
        source_generation: generation.clone(),
        expected_route_generation: routes.generation.clone(),
        source_routes,
        destination_routes,
    };
    let stopped = Arc::new(NativeStopped {
        databases: databases
            .iter()
            .map(|database| database.path.clone())
            .collect(),
        journals: session_inputs
            .as_ref()
            .map(|inputs| {
                inputs
                    .iter()
                    .map(|input| session_root.join(&input.relative_path))
                    .collect()
            })
            .unwrap_or_default(),
    });
    cutover.freeze(prepared.clone(), stopped).await?;
    let assembled = assemble(
        &databases,
        session_inputs,
        &session_root,
        &staged_sessions,
        &staged_databases,
        &prepared,
        fence,
        &external_sources,
        imported_stages,
    )
    .await;
    let (source, stage) = match assembled {
        Ok(value) => value,
        Err(error) => {
            if previous.is_none() {
                cutover.abort_unplanned(&options.id).await?;
            }
            return Err(error);
        }
    };
    let coordinator = Coordinator {
        source: &source,
        destination: &stage,
        journal: &journal,
        cutover: &cutover,
        max_chunk_bytes: CHUNK_BYTES,
    };
    let result = async {
        if previous.is_none() {
            coordinator
                .plan(
                    options.id.clone(),
                    prepared.destination_identity,
                    prepared.expected_route_generation,
                )
                .await?;
        }
        coordinator.run(&options.id).await
    }
    .await;
    let close = coordinator.close().await;
    if result.is_err() && journal.load(&options.id).await?.is_none() {
        cutover.abort_unplanned(&options.id).await?;
    }
    close?;
    result
}

#[allow(clippy::too_many_arguments)]
async fn assemble(
    databases: &[Database],
    session_inputs: Option<Vec<JournalInput>>,
    session_root: &Path,
    staged_sessions: &Path,
    staged_databases: &BTreeMap<String, PathBuf>,
    prepared: &PreparedCutover,
    fence: Arc<dyn FreezeGuard>,
    external_sources: &[ExternalDomain],
    mut imported_stages: BTreeMap<String, Arc<dyn StageImport>>,
) -> Result<(GroupSource, GroupStage)> {
    let portable = !external_sources.is_empty() || !imported_stages.is_empty();
    let mut sources: Vec<Arc<dyn SnapshotExport>> = Vec::new();
    let mut stages: BTreeMap<String, Arc<dyn StageImport>> = BTreeMap::new();
    for database in databases {
        let source = sql::Source::open(
            &database.path,
            prepared.source_identity.clone(),
            prepared.source_generation.clone(),
            fence.clone(),
            database.codec.clone(),
        );
        let source = match source {
            Ok(source) => source,
            Err(error) => {
                for source in &sources {
                    let _ = source.close().await;
                }
                return Err(error);
            }
        };
        sources.push(Arc::new(source));
        let stage = imported_stages
            .remove(database.codec.domain())
            .unwrap_or_else(|| {
                Arc::new(sql::Stage::new(
                    staged_databases[database.codec.domain()].clone(),
                    prepared.destination_identity.clone(),
                    database.codec.clone(),
                ))
            });
        stages.insert(database.codec.domain().into(), stage);
    }
    let context = crate::runtime::storage::backend::MigrationContext {
        migration_id: prepared.migration_id.clone(),
        source_identity: prepared.source_identity.clone(),
        destination_identity: prepared.destination_identity.clone(),
        generation: prepared.source_generation.clone(),
        fence: fence.clone(),
    };
    for source in external_sources {
        let requirements =
            crate::runtime::storage::RepositoryRequirements::for_domain(&source.domain)
                .map_err(|_| invalid("unsupported source domain"))?;
        let exported = match source
            .factory
            .export(
                &source.binding.endpoint,
                &requirements,
                &source.binding.options,
                &context,
            )
            .await
        {
            Ok(exported) => exported,
            Err(_) => {
                for source in &sources {
                    let _ = source.close().await;
                }
                return Err(invalid("source logical export is unavailable"));
            }
        };
        sources.push(exported);
        let stage = if let Some(stage) = imported_stages.remove(&source.domain) {
            stage
        } else {
            Arc::new(sql::Stage::new(
                staged_databases[&source.domain].clone(),
                prepared.destination_identity.clone(),
                codec(&source.domain)?,
            ))
        };
        stages.insert(source.domain.clone(), stage);
    }
    let journals = if let Some(inputs) = session_inputs {
        let source = JournalSource::open(
            session_root,
            Domain::Sessions,
            inputs,
            prepared.source_identity.clone(),
            prepared.source_generation.clone(),
            fence,
        );
        let source = match source {
            Ok(source) => source,
            Err(error) => {
                for source in &sources {
                    let _ = source.close().await;
                }
                return Err(error);
            }
        };
        sources.push(Arc::new(source));
        let stage = match JournalStage::new(
            staged_sessions,
            Domain::Sessions,
            prepared.destination_identity.clone(),
        ) {
            Ok(stage) => Arc::new(stage),
            Err(error) => {
                for source in &sources {
                    let _ = source.close().await;
                }
                return Err(error);
            }
        };
        stages.insert("sessions".into(), stage.clone());
        Some(stage)
    } else {
        None
    };
    let source = GroupSource::new(sources).await?;
    let closure: Arc<dyn crate::storage::migration::group::ReferenceClosure> = if portable {
        Arc::new(
            crate::storage::migration::references::PortableReferenceClosure {
                stages: stages.clone(),
                max_bytes: 64 * 1024 * 1024,
            },
        )
    } else {
        // Retain the existing streaming native verifier and its established limits.
        Arc::new(
            crate::storage::backends::migration_references::NativeReferenceClosure::new(
                staged_databases.clone(),
                journals,
            ),
        )
    };
    match GroupStage::new(stages, closure) {
        Ok(stage) => Ok((source, stage)),
        Err(error) => {
            let _ = source.close().await;
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otto_core::approval::ApprovalMode;
    use std::os::unix::fs::DirBuilderExt;

    fn fixture() -> (tempfile::TempDir, Options) {
        let directory = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let home = directory.path().join("home");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&home)
            .unwrap();
        (
            directory,
            Options {
                home,
                destination: PathBuf::new(),
                id: "move-1".into(),
            },
        )
    }
    fn approvals(home: &Path) -> otto_core::approval::Scope {
        let service = crate::approval::Service::open(&home.join(".otto/approvals.db")).unwrap();
        let scope = otto_core::approval::Scope {
            workspace: "/workspace".into(),
            session_id: "parent".into(),
            task_id: String::new(),
            run_id: String::new(),
        };
        service.set_mode(&scope, ApprovalMode::Manual).unwrap();
        scope
    }
    fn journal(root: &Path) -> Vec<u8> {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(root.join("workspace"))
            .unwrap();
        let header = serde_json::json!({"type":"session","version":3,"id":"parent","timestamp":"2026-10-06T00:00:00Z","cwd":"/workspace","futureHeader":{"keep":true}});
        let runtime = serde_json::json!({"type":"custom","id":"00000001","parentId":null,"timestamp":"2026-10-06T00:00:00Z","customType":"otto.runtime","data":{"profile":"test","provider":"openai-compatible","model":"test"}});
        let entry = serde_json::json!({"type":"future_entry","id":"00000002","parentId":"00000001","timestamp":"2026-10-06T00:00:01Z","unknown":{"verbatim":"yes"}});
        let bytes = format!("{header}\n{runtime}\n{entry}\n").into_bytes();
        let path = root.join("workspace/parent.jsonl");
        std::fs::write(&path, &bytes).unwrap();
        crate::session::Store::read_entries(&path).expect("ordinary native journal replay");
        bytes
    }
    fn args(options: &Options) -> Vec<String> {
        vec![
            endpoint(&options.home).unwrap(),
            endpoint(&options.destination).unwrap(),
            options.id.clone(),
        ]
    }

    #[tokio::test]
    async fn command_transfers_all_existing_families_and_exact_pi_bytes() {
        let (directory, mut options) = fixture();
        options.destination = directory.path().join("stage");
        let scope = approvals(&options.home);
        let backend = &options.home.join(".otto");
        let memory = backend.join("memory");
        std::fs::create_dir_all(&memory).unwrap();
        std::fs::write(memory.join("identity.json"), b"{}").unwrap();
        drop(
            crate::storage::backends::turso::workflow::TursoWorkflowStore::open(
                &backend.join("workflows.db"),
            )
            .unwrap(),
        );
        drop(
            crate::storage::backends::turso::schedule::TursoScheduleStore::open(
                &backend.join("schedule.db"),
            )
            .unwrap(),
        );
        drop(
            crate::storage::backends::turso::usage::Store::open(&backend.join("usage.db")).unwrap(),
        );
        drop(
            crate::storage::backends::turso::tasks::Store::open(&backend.join("tasks.db")).unwrap(),
        );
        drop(
            crate::storage::backends::turso::reflection::Store::open(
                &backend.join("reflection.db"),
            )
            .unwrap(),
        );
        drop(
            crate::storage::backends::turso::skill_checks::Store::open(
                &backend.join("skill-checks.db"),
            )
            .unwrap(),
        );
        drop(
            crate::storage::backends::turso::memory_advice::Store::open(
                &backend.join("memory-advice.db"),
            )
            .unwrap(),
        );
        let bytes = journal(&backend.join("sessions"));
        let argv = args(&options);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        assert_eq!(
            run(&argv, &mut stdout, &mut stderr).await,
            0,
            "{}",
            String::from_utf8_lossy(&stderr)
        );
        assert!(String::from_utf8_lossy(&stdout).contains("transferred 9 domains"));
        assert_eq!(
            std::fs::read(options.destination.join("sessions/workspace/parent.jsonl")).unwrap(),
            bytes
        );
        assert_eq!(
            std::fs::read(backend.join("sessions/workspace/parent.jsonl")).unwrap(),
            bytes
        );
        assert!(!options.destination.join("memory.db").exists());
        assert_eq!(std::fs::read(memory.join("identity.json")).unwrap(), b"{}");
        let runtime = crate::runtime::storage::Storage::open_home(&options.home)
            .await
            .unwrap();
        let service = crate::approval::Service::from_store(
            runtime.approvals(&backend.join("approvals.db")).unwrap(),
        )
        .unwrap();
        assert_eq!(service.mode(&scope).unwrap(), ApprovalMode::Manual);
        drop(service);
        drop(runtime);
        assert_eq!(
            run(&argv, &mut Vec::new(), &mut stderr).await,
            0,
            "{}",
            String::from_utf8_lossy(&stderr)
        );
        assert_eq!(
            abort(
                &[endpoint(&options.home).unwrap(), options.id.clone()],
                &mut Vec::new(),
                &mut stderr
            )
            .await,
            1
        );
        assert!(String::from_utf8_lossy(&stderr).contains("published destination"));
    }

    #[tokio::test]
    async fn live_native_writer_and_unrelated_destination_are_refused() {
        let (directory, mut options) = fixture();
        options.destination = directory.path().join("stage");
        approvals(&options.home);
        let connection = Connection::open(options.home.join(".otto/approvals.db")).unwrap();
        let transaction = connection.unchecked_transaction().unwrap();
        let mut error = Vec::new();
        assert_eq!(run(&args(&options), &mut Vec::new(), &mut error).await, 1);
        assert!(String::from_utf8_lossy(&error).contains("writer"));
        drop(transaction);
        drop(connection);
        options.destination = directory.path().join("unrelated");
        std::fs::create_dir(&options.destination).unwrap();
        let unrelated = options.destination.join("important");
        std::fs::write(&unrelated, b"retain").unwrap();
        assert_eq!(
            run(&args(&options), &mut Vec::new(), &mut Vec::new()).await,
            1
        );
        assert_eq!(std::fs::read(&unrelated).unwrap(), b"retain");
    }

    #[tokio::test]
    async fn bootstrap_resumes_partial_marker_and_symlink_routes_fail_closed() {
        let (directory, mut options) = fixture();
        options.destination = directory.path().join("stage");
        approvals(&options.home);
        let owner = Owner {
            version: 1,
            migration_id: options.id.clone(),
            source_identity: identity(&options.home),
            destination_identity: identity(&options.destination),
        };
        let bootstrap = directory.path().join(format!(
            ".otto-transfer-{:x}",
            Sha256::digest(serde_json::to_vec(&owner).unwrap())
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&bootstrap)
            .unwrap();
        std::fs::write(bootstrap.join(OWNER), b"{partial").unwrap();
        let mut stderr = Vec::new();
        assert_eq!(
            run(&args(&options), &mut Vec::new(), &mut stderr).await,
            0,
            "{}",
            String::from_utf8_lossy(&stderr)
        );
        assert!(!bootstrap.exists());
        let other_home = directory.path().join("other-home");
        std::fs::create_dir_all(other_home.join(".otto")).unwrap();
        std::os::unix::fs::symlink(
            options.destination.join("approvals.db"),
            other_home.join(".otto/approvals.db"),
        )
        .unwrap();
        options.home = other_home;
        options.destination = directory.path().join("refused");
        assert_eq!(
            run(&args(&options), &mut Vec::new(), &mut Vec::new()).await,
            1
        );
        assert!(!options.destination.exists());
    }
    #[tokio::test]
    async fn command_abort_restores_source_after_failed_closure_without_deleting_staging() {
        let (directory, mut options) = fixture();
        options.destination = directory.path().join("failed-stage");
        let scope = approvals(&options.home);
        journal(&options.home.join(".otto/sessions"));
        let service =
            crate::approval::Service::open(&options.home.join(".otto/approvals.db")).unwrap();
        let request = service
            .create(crate::approval::Proposal {
                kind: otto_core::approval::Kind::Authorization,
                authority: otto_core::approval::Authority::Durable,
                scope: scope.clone(),
                subject: otto_core::approval::Subject {
                    tool_call_id: String::new(),
                    source: "unsupported-live-source".into(),
                    id: "op".into(),
                    version: "1".into(),
                    details: "pending".into(),
                },
                title: "Review".into(),
                reason: "test".into(),
                persistent: false,
                blocking: true,
                expires_at: None,
            })
            .unwrap();
        drop(service);
        let mut stderr = Vec::new();
        assert_eq!(run(&args(&options), &mut Vec::new(), &mut stderr).await, 1);
        assert!(String::from_utf8_lossy(&stderr).contains("unsupported live approval subject"));
        let control = LocalCutover::open_existing(&options.home.join(".otto/storage")).unwrap();
        assert!(
            control
                .ensure_writable(&identity(&options.home), "initial")
                .await
                .is_err()
        );
        let staged_before = std::fs::read(options.destination.join("approvals.db")).unwrap();
        let abort_args = vec![endpoint(&options.home).unwrap(), options.id.clone()];
        assert_eq!(
            abort(&abort_args, &mut Vec::new(), &mut stderr).await,
            0,
            "{}",
            String::from_utf8_lossy(&stderr)
        );
        control
            .ensure_writable(&identity(&options.home), "initial")
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(options.destination.join("approvals.db")).unwrap(),
            staged_before
        );
        let original =
            crate::approval::Service::open(&options.home.join(".otto/approvals.db")).unwrap();
        assert_eq!(
            original
                .get(&request.id, &scope.workspace, Some(&scope.session_id))
                .unwrap()
                .state,
            otto_core::approval::State::Pending
        );
        drop(original);
        assert_eq!(abort(&abort_args, &mut Vec::new(), &mut stderr).await, 0);

        let unrelated = directory.path().join("untouched-home");
        std::fs::create_dir(&unrelated).unwrap();
        assert_eq!(
            abort(
                &[endpoint(&unrelated).unwrap(), "missing".into()],
                &mut Vec::new(),
                &mut Vec::new()
            )
            .await,
            1
        );
        assert!(!unrelated.join(".otto").exists());
    }
}
