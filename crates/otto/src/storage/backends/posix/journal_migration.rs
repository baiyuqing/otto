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
use crate::hex::to_hex;
use crate::{
    fsops,
    storage::{
        drivers::posix,
        migration::{chunk_hash, contracts::*},
    },
};

mod source;
pub use source::*;
mod stage;
pub use stage::*;

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
    Ok(to_hex(&hash.finalize()))
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
        sha256: to_hex(&hash.finalize()),
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

#[cfg(test)]
mod tests;
