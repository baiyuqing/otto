//! Destination root, SQL codecs, database discovery and journal inputs.

use super::*;
use otto::hex::to_hex;

pub(super) fn destination(options: &Options, source_identity: &str) -> Result<fsops::Dir> {
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
    let temporary = format!(".otto-transfer-{}", to_hex(&Sha256::digest(&bytes)));
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

pub(super) fn codec(domain: &str) -> Result<Arc<dyn SqlDomainCodec>> {
    Ok(match domain {
        "approvals" => Arc::new(sql::ApprovalCodec),
        "workflows" => Arc::new(sql::workflow::WorkflowCodec),
        "schedule" => Arc::new(sql::schedule::ScheduleCodec),
        "usage" => Arc::new(sql::usage::UsageCodec),
        "tasks" => Arc::new(sql::tasks::TaskCodec),
        "skill_checks" => Arc::new(sql::skill_checks::SkillChecksCodec),
        "memory_advice" => Arc::new(sql::memory_advice::MemoryAdviceCodec),
        _ => return Err(invalid("unsupported local destination domain")),
    })
}

pub(super) struct Database {
    pub(super) path: PathBuf,
    pub(super) codec: Arc<dyn SqlDomainCodec>,
}
pub(super) fn databases(
    options: &Options,
    routes: &BTreeMap<String, StorageRoute>,
) -> Result<Vec<Database>> {
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

pub(super) fn journal_inputs(root: &Path) -> Result<Vec<JournalInput>> {
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

pub(super) struct NativeStopped {
    pub(super) databases: Vec<PathBuf>,
    pub(super) journals: Vec<PathBuf>,
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
            otto::storage::drivers::posix::try_lock_exclusive(&file).map_err(|_| {
                migration::Error::Conflict("a source session writer is still active".into())
            })?;
            journals.push(file);
        }
        Ok(())
    }
}
pub(super) struct Fence(pub(super) LocalCutover);
impl FreezeGuard for Fence {
    fn validate(&self, generation: &str) -> Result<()> {
        self.0.validate_freeze_sync(generation)
    }
}

pub(super) struct ExternalDomain {
    pub(super) domain: String,
    pub(super) factory: Arc<dyn otto::runtime::storage::RepositoryFactory>,
    pub(super) binding: otto::runtime::storage::backend::ConfiguredEndpoint,
}
