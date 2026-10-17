//! The file-backed memory store.
//!
//! One directory holds the whole store: `records/<id>.md` (Markdown with
//! frontmatter), `tombstones/<id>.json`, `candidates/<id>.json`,
//! `identity.json` and the `.lock` file. A person can read and edit the record
//! files directly; the next operation sees the edit.
//!
//! Writes take an exclusive `flock` on `.lock`, re-read the directory under
//! it and publish each file with an atomic rename. Reads take no lock and
//! tolerate a file that disappears between the directory listing and the read.
//!
//! ponytail: every operation loads the full directory (tens of records). Add
//! an index only if a store holds thousands of records.

mod candidates;
mod cursor;
mod format;
pub(crate) mod import;
mod records;
mod retrieve;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::fs::{DirBuilder, File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg};

use self::format::{
    IdentityWire, MAX_FILE_BYTES, encode_candidate_file, encode_record_file, encode_tombstone_file,
    parse_candidate, parse_record, parse_tombstone, valid_file_id,
};
use crate::fsops::{self, PosixPublish, Publish, PublishMode};
use crate::memory::guard::{ContentGuard, guard_candidate, guard_record};
use crate::memory::repository::MemoryOptions;
use crate::memory::{
    Candidate, Error, ErrorKind, GuardField, GuardInput, MAX_DUPLICATE_ID_RETRIES, MAX_ID_BYTES,
    Record, Result, Scope, StoreIdentity, Tombstone,
};

#[cfg(test)]
pub(crate) use self::retrieve::record_cost;

const SCHEMA_VERSION: i64 = 1;
const LOCK_RETRY: Duration = Duration::from_millis(10);

/// A file the store did not read: unparseable, failing validation or the
/// content guard, or a record that lost a duplicate-key conflict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidFile {
    pub path: PathBuf,
    pub reason: String,
}

/// Everything a logical import writes into an empty store.
pub(crate) struct ImportSet {
    pub database_id: String,
    pub user_scope_id: String,
    pub records: Vec<Record>,
    pub tombstones: Vec<Tombstone>,
    pub candidates: Vec<Candidate>,
}

fn unavailable<E>(_: E) -> Error {
    Error::new(ErrorKind::Unavailable)
}

pub fn has_identity(root: &Path) -> bool {
    root.join("identity.json").exists()
}

fn ensure_directory(path: &Path) -> Result<()> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(unavailable)
}

fn ensure_layout(root: &Path) -> Result<()> {
    ensure_directory(root)?;
    std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700)).map_err(unavailable)?;
    for name in ["records", "tombstones", "candidates"] {
        ensure_directory(&root.join(name))?;
    }
    Ok(())
}

/// Takes the exclusive store lock, retrying every 10 ms until `timeout`
/// elapses. A zero timeout makes one attempt.
fn acquire(root: &Path, timeout: Duration) -> Result<Flock<File>> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(root.join(".lock"))
        .map_err(unavailable)?;
    let deadline = Instant::now() + timeout;
    loop {
        match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(lock) => return Ok(lock),
            Err((returned, Errno::EWOULDBLOCK)) if Instant::now() < deadline => {
                file = returned;
                std::thread::sleep(LOCK_RETRY);
            }
            Err((_, Errno::EWOULDBLOCK)) => return Err(Error::new(ErrorKind::Busy)),
            Err(_) => return Err(Error::new(ErrorKind::Unavailable)),
        }
    }
}

/// Publishes `content` at `path` through a temporary file in the same
/// directory and an atomic rename.
fn publish_path(path: &Path, content: &str) -> Result<()> {
    let (parent, name) = fsops::open_parent(path).map_err(unavailable)?;
    let temp = format!(
        ".{name}.{}.tmp",
        crate::auth::random_hex::<8>().map_err(unavailable)?
    );
    fsops::publish_file(
        &parent,
        &Publish {
            name,
            temp: &temp,
            permissions: 0o600,
            mode: PublishMode::Replace,
        },
        content.as_bytes(),
        &PosixPublish,
    )
    .map_err(|error| {
        Error::new(if error.committed {
            ErrorKind::CommitUnknown
        } else {
            ErrorKind::Unavailable
        })
    })
}

fn publish_at(root: &Path, dir: &str, id: &str, extension: &str, content: &str) -> Result<()> {
    publish_path(&root.join(dir).join(format!("{id}.{extension}")), content)
}

fn remove_if_present(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(Error::new(ErrorKind::Unavailable)),
    }
}

fn check_identity_ids(
    guard: &dyn ContentGuard,
    database_id: &str,
    user_scope_id: &str,
) -> Result<()> {
    for id in [database_id, user_scope_id] {
        if !valid_file_id(id) {
            return Err(crate::memory::invalid_request("database ID"));
        }
    }
    guard.check(&GuardInput {
        fields: vec![
            GuardField {
                name: "database ID".into(),
                value: database_id.to_string(),
                opaque: true,
            },
            GuardField {
                name: "user scope ID".into(),
                value: user_scope_id.to_string(),
                opaque: true,
            },
        ],
    })
}

/// Writes a logical import into `root`. Under the store lock, an existing
/// `identity.json` means the store is already initialized and nothing is
/// written (`Ok(false)`). Otherwise every file is written and
/// `identity.json` last, so an interrupted import leaves no identity and can
/// be repeated.
pub(crate) fn write_import(root: &Path, set: &ImportSet, busy_timeout: Duration) -> Result<bool> {
    ensure_layout(root)?;
    let _lock = acquire(root, busy_timeout)?;
    if has_identity(root) {
        return Ok(false);
    }
    for record in &set.records {
        publish_at(
            root,
            "records",
            &record.id,
            "md",
            &encode_record_file(record)?,
        )?;
    }
    for tombstone in &set.tombstones {
        publish_at(
            root,
            "tombstones",
            &tombstone.id,
            "json",
            &encode_tombstone_file(tombstone)?,
        )?;
    }
    for candidate in &set.candidates {
        publish_at(
            root,
            "candidates",
            &candidate.id,
            "json",
            &encode_candidate_file(candidate)?,
        )?;
    }
    let identity = IdentityWire {
        database_id: set.database_id.clone(),
        user_scope_id: set.user_scope_id.clone(),
    };
    publish_path(&root.join("identity.json"), &identity.encode())?;
    Ok(true)
}

/// One consistent reading of the directory.
pub(super) struct Snapshot {
    /// Active records after duplicate-key resolution, ordered by id.
    pub records: Vec<Record>,
    pub tombstones: Vec<Tombstone>,
    pub candidates: Vec<Candidate>,
    pub invalid: Vec<InvalidFile>,
}

impl Snapshot {
    pub fn record(&self, scope: &Scope, id: &str) -> Option<&Record> {
        self.records
            .iter()
            .find(|record| record.id == id && &record.scope == scope)
    }

    pub fn tombstone(&self, scope: &Scope, id: &str) -> Option<&Tombstone> {
        self.tombstones
            .iter()
            .find(|tombstone| tombstone.id == id && &tombstone.scope == scope)
    }

    pub fn candidate(&self, scope: &Scope, id: &str) -> Option<&Candidate> {
        self.candidates
            .iter()
            .find(|candidate| candidate.id == id && &candidate.proposed.scope == scope)
    }

    pub fn is_invalid(&self, path: &Path) -> bool {
        self.invalid.iter().any(|file| file.path == path)
    }
}

/// One open memory directory.
pub struct FileStore {
    root: PathBuf,
    guard: Box<dyn ContentGuard>,
    new_id: Box<dyn Fn() -> Result<String> + Send + Sync>,
    busy_timeout: Duration,
    database_id: String,
    user_scope: Scope,
    closed: AtomicBool,
}

impl FileStore {
    /// Opens `root`, creating its layout and identity when absent, and removes
    /// record files whose id has a tombstone (left by an interrupted forget).
    pub fn open(root: &Path, options: MemoryOptions) -> Result<Self> {
        ensure_layout(root)?;
        let lock = acquire(root, options.busy_timeout)?;
        let identity_path = root.join("identity.json");
        let identity = match std::fs::read_to_string(&identity_path) {
            Ok(content) => serde_json::from_str::<IdentityWire>(&content)
                .map_err(|_| Error::new(ErrorKind::Corrupt))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let ids = crate::memory::generate_distinct_ids(2, options.new_id.as_ref())?;
                let identity = IdentityWire {
                    database_id: ids[0].clone(),
                    user_scope_id: ids[1].clone(),
                };
                check_identity_ids(
                    options.guard.as_ref(),
                    &identity.database_id,
                    &identity.user_scope_id,
                )?;
                publish_path(&root.join("identity.json"), &identity.encode())?;
                identity
            }
            Err(_) => return Err(Error::new(ErrorKind::Unavailable)),
        };
        check_identity_ids(
            options.guard.as_ref(),
            &identity.database_id,
            &identity.user_scope_id,
        )?;
        let store = Self {
            root: root.to_path_buf(),
            guard: options.guard,
            new_id: options.new_id,
            busy_timeout: options.busy_timeout,
            database_id: identity.database_id,
            user_scope: Scope::new(crate::memory::NAMESPACE_USER, identity.user_scope_id),
            closed: AtomicBool::new(false),
        };
        let snapshot = store.snapshot()?;
        for tombstone in &snapshot.tombstones {
            remove_if_present(&store.path("records", &tombstone.id, "md"))?;
        }
        drop(lock);
        Ok(store)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The files the store skipped, ordered by path.
    pub fn invalid_files(&self) -> Result<Vec<InvalidFile>> {
        Ok(self.snapshot()?.invalid)
    }

    pub fn identity(&self) -> Result<StoreIdentity> {
        self.ensure_open()?;
        Ok(StoreIdentity {
            database_id: self.database_id.clone(),
            user_scope: self.user_scope.clone(),
            schema_version: SCHEMA_VERSION,
        })
    }

    pub fn guard(&self) -> &dyn ContentGuard {
        self.guard.as_ref()
    }

    /// Mints an ID that is not already in `taken`.
    pub fn mint_id(&self, taken: &[String]) -> Result<String> {
        for _ in 0..=MAX_DUPLICATE_ID_RETRIES {
            let id = (self.new_id)()?;
            if !taken.contains(&id) {
                return Ok(id);
            }
        }
        Err(Error::new(ErrorKind::Unavailable))
    }

    pub fn close(&self) -> Result<()> {
        self.closed.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn ensure_open(&self) -> Result<()> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(Error::new(ErrorKind::Closed));
        }
        Ok(())
    }

    fn path(&self, dir: &str, id: &str, extension: &str) -> PathBuf {
        self.root.join(dir).join(format!("{id}.{extension}"))
    }

    /// Runs `body` holding the exclusive store lock, against a snapshot read
    /// after the lock was taken.
    fn write<T>(&self, body: impl FnOnce(&Snapshot) -> Result<T>) -> Result<T> {
        self.ensure_open()?;
        let _lock = acquire(&self.root, self.busy_timeout)?;
        let snapshot = self.snapshot()?;
        body(&snapshot)
    }

    fn put_record(&self, record: &Record) -> Result<()> {
        publish_at(
            &self.root,
            "records",
            &record.id,
            "md",
            &encode_record_file(record)?,
        )
    }

    fn put_tombstone(&self, tombstone: &Tombstone) -> Result<()> {
        publish_at(
            &self.root,
            "tombstones",
            &tombstone.id,
            "json",
            &encode_tombstone_file(tombstone)?,
        )
    }

    fn put_candidate(&self, candidate: &Candidate) -> Result<()> {
        publish_at(
            &self.root,
            "candidates",
            &candidate.id,
            "json",
            &encode_candidate_file(candidate)?,
        )
    }

    fn snapshot(&self) -> Result<Snapshot> {
        self.ensure_open()?;
        let mut invalid = Vec::new();
        let tombstones = self.read_entries("tombstones", "json", &parse_tombstone, &mut invalid)?;
        let guard = self.guard.as_ref();
        let mut found = self.read_entries(
            "records",
            "md",
            &|id, content| {
                let record = parse_record(id, content)?;
                guard_record(guard, &record).map_err(|error| error.to_string())?;
                Ok(record)
            },
            &mut invalid,
        )?;
        let candidates = self.read_entries(
            "candidates",
            "json",
            &|id, content| {
                let candidate = parse_candidate(id, content)?;
                guard_candidate(guard, &candidate).map_err(|error| error.to_string())?;
                Ok(candidate)
            },
            &mut invalid,
        )?;

        found.retain(|record| !tombstones.iter().any(|tombstone| tombstone.id == record.id));
        // Duplicate active (scope, kind, non-empty key): the later updated_at
        // wins, then the larger id; the others are reported.
        let mut winners: BTreeMap<(Scope, String, String), usize> = BTreeMap::new();
        for (index, record) in found.iter().enumerate() {
            if record.key.is_empty() {
                continue;
            }
            let key = (
                record.scope.clone(),
                record.kind.clone(),
                record.key.clone(),
            );
            match winners.get(&key) {
                Some(&best)
                    if (found[best].updated_at, &found[best].id)
                        >= (record.updated_at, &record.id) => {}
                _ => {
                    winners.insert(key, index);
                }
            }
        }
        let mut records = Vec::with_capacity(found.len());
        for (index, record) in found.iter().enumerate() {
            let key = (
                record.scope.clone(),
                record.kind.clone(),
                record.key.clone(),
            );
            match winners.get(&key) {
                Some(&winner) if winner != index && !record.key.is_empty() => {
                    invalid.push(InvalidFile {
                        path: self.path("records", &record.id, "md"),
                        reason: format!("duplicate key, superseded by record {}", found[winner].id),
                    });
                }
                _ => records.push(record.clone()),
            }
        }
        invalid.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(Snapshot {
            records,
            tombstones,
            candidates,
            invalid,
        })
    }

    /// Reads and parses every `*.<extension>` file of `dir`, ordered by id.
    /// A file that cannot be read or parsed is recorded in `invalid`.
    fn read_entries<T>(
        &self,
        dir: &str,
        extension: &str,
        parse: &dyn Fn(&str, &str) -> std::result::Result<T, String>,
        invalid: &mut Vec<InvalidFile>,
    ) -> Result<Vec<T>> {
        let suffix = format!(".{extension}");
        let listing = std::fs::read_dir(self.root.join(dir)).map_err(unavailable)?;
        let mut out: Vec<(String, T)> = Vec::new();
        for entry in listing {
            let entry = entry.map_err(unavailable)?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let Some(stem) = name.strip_suffix(&suffix) else {
                continue;
            };
            let path = entry.path();
            let mut reject = |reason: &str| {
                invalid.push(InvalidFile {
                    path: path.clone(),
                    reason: reason.to_string(),
                });
            };
            if !valid_file_id(stem) {
                reject("file name is not a valid id");
                continue;
            }
            match entry.file_type() {
                Ok(kind) if kind.is_file() => {}
                Ok(_) => {
                    reject("not a regular file");
                    continue;
                }
                Err(_) => continue,
            }
            let mut bytes = Vec::new();
            let read = File::open(&path)
                .and_then(|file| file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes));
            match read {
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => {
                    reject("file cannot be read");
                    continue;
                }
            }
            if bytes.len() as u64 > MAX_FILE_BYTES {
                reject("file is larger than 128 KiB");
                continue;
            }
            let Ok(content) = String::from_utf8(bytes) else {
                reject("file is not UTF-8");
                continue;
            };
            match parse(stem, &content) {
                Ok(value) => out.push((stem.to_string(), value)),
                Err(reason) => reject(&reason),
            }
        }
        out.sort_by(|left, right| left.0.cmp(&right.0));
        Ok(out.into_iter().map(|(_, value)| value).collect())
    }

    /// Checks that `id` is usable as a file stem.
    fn file_id(id: &str, what: &str) -> Result<()> {
        if valid_file_id(id) && id.len() <= MAX_ID_BYTES {
            Ok(())
        } else {
            Err(crate::memory::invalid_request(what))
        }
    }
}

impl crate::memory::MemoryStore for FileStore {
    fn identity(&self) -> Result<StoreIdentity> {
        FileStore::identity(self)
    }

    fn close(&self) -> Result<()> {
        FileStore::close(self)
    }

    fn get(&self, reference: &crate::memory::RecordRef) -> Result<Record> {
        FileStore::get(self, reference)
    }

    fn get_by_key(&self, key: &crate::memory::RecordKey) -> Result<Record> {
        FileStore::get_by_key(self, key)
    }

    fn list(&self, request: &crate::memory::ListRequest) -> Result<crate::memory::RecordPage> {
        FileStore::list(self, request)
    }

    fn get_tombstone(&self, reference: &crate::memory::RecordRef) -> Result<Tombstone> {
        FileStore::get_tombstone(self, reference)
    }

    fn get_candidate(&self, reference: &crate::memory::CandidateRef) -> Result<Candidate> {
        FileStore::get_candidate(self, reference)
    }

    fn list_candidates(
        &self,
        request: &crate::memory::CandidateListRequest,
    ) -> Result<crate::memory::CandidatePage> {
        FileStore::list_candidates(self, request)
    }

    fn retrieve(
        &self,
        request: &crate::memory::RetrievalRequest,
    ) -> Result<crate::memory::RetrievalResult> {
        FileStore::retrieve(self, request)
    }

    fn upsert_within_standing_budget(
        &self,
        request: &crate::memory::UpsertRequest,
    ) -> Result<Record> {
        FileStore::upsert_within_standing_budget(self, request)
    }

    fn standing_usage(
        &self,
        user_scope: &Scope,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<crate::memory::StandingUsage> {
        FileStore::standing_usage(self, user_scope, now)
    }

    fn standing_records(
        &self,
        user_scope: &Scope,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<Vec<Record>> {
        FileStore::standing_records(self, user_scope, now)
    }

    fn forget(&self, request: &crate::memory::StoreForgetRequest) -> Result<Tombstone> {
        FileStore::forget(self, request)
    }

    fn review_within_standing_budget(
        &self,
        request: &crate::memory::StoreReviewRequest,
    ) -> Result<crate::memory::ReviewResult> {
        FileStore::review_within_standing_budget(self, request)
    }

    fn propose(&self, candidates: &[Candidate]) -> Result<Vec<Candidate>> {
        FileStore::propose(self, candidates)
    }

    fn check_proposals(&self, candidates: &[Candidate]) -> Result<()> {
        FileStore::check_proposals(self, candidates)
    }
}

#[cfg(test)]
pub(crate) mod testsupport {
    use chrono::{DateTime, TimeZone, Utc};

    use super::*;

    /// Opens a store in a fresh temporary directory, returning both so the
    /// directory outlives the store.
    pub fn open_temp() -> (tempfile::TempDir, FileStore) {
        let directory = tempfile::tempdir().expect("temp dir");
        let store = FileStore::open(&directory.path().join("memory"), MemoryOptions::default())
            .expect("open store");
        (directory, store)
    }

    /// A fixed instant `second` seconds after 2026-03-01T12:00:00Z.
    pub fn at(second: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 3, 1, 12, 0, 0).unwrap()
            + chrono::Duration::seconds(second.into())
    }

    /// A valid new record (revision 0), as a caller would submit it.
    pub fn sample_record(id: &str, scope: &Scope, key: &str, text: &str) -> Record {
        Record {
            id: id.to_string(),
            scope: scope.clone(),
            kind: "preference".to_string(),
            key: key.to_string(),
            text: text.to_string(),
            labels: vec!["style".to_string()],
            source: crate::memory::Provenance {
                origin: Some(crate::memory::Origin::Human),
                ..crate::memory::Provenance::default()
            },
            confidence: 0.5,
            revision: 0,
            created_at: at(0),
            updated_at: at(0),
            ..Record::default()
        }
    }
}
