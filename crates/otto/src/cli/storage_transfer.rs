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

mod filesystem;
mod sources;
mod transfer;
use filesystem::*;
use sources::*;
use transfer::*;

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

#[cfg(test)]
mod tests;
